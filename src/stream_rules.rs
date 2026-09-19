//! Time-Traveling Stream Rules (TTSR) engine and Grievances Ledger (bd-cv653.3.4).
//!
//! TTSR allows rules to sit dormant until a model goes off-script mid-stream.
//! A regex match on the in-flight streaming response aborts the provider stream,
//! injects the rule as a system reminder, and retries the turn from the exact
//! same conversation state. Injections are recorded in session history and survive
//! compaction so course-corrections remain durable without taxing every turn's prompt context.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::memory::screen_secrets;

/// Default maximum number of rule injections allowed in a single turn before
/// halting to avoid infinite abort loops.
pub const DEFAULT_MAX_INJECTIONS_PER_TURN: usize = 3;

/// Default per-channel rolling lookback in bytes (4KB). One preceding
/// character is also retained as regex boundary context, never as match input.
pub const DEFAULT_ROLLING_LOOKBACK_BYTES: usize = 4096;

const MAX_MATCHED_EXCERPT_BYTES: usize = 4096;

/// Custom session entry type name for TTSR stream rule injections.
pub const TTSR_CUSTOM_ENTRY_TYPE: &str = "stream_rule_injection";

/// A configured time-traveling stream rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamRule {
    /// Unique identifier for the rule (e.g. `no-box-leak`).
    pub id: String,
    /// Human-readable title or label for UX display.
    pub name: String,
    /// Regex pattern to match against the streaming text.
    pub pattern: String,
    /// Reminder directive body injected upon match.
    pub body: String,
    /// Whether the rule is currently active.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional origin note or grievance id from which this rule was generated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_from: Option<String>,
    /// Optional cooldown in turns before this rule can be re-injected in the same session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_turns: Option<usize>,
}

const fn default_true() -> bool {
    true
}

/// Recorded match event when a stream rule triggers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamRuleMatch {
    pub rule_id: String,
    pub rule_name: String,
    pub rule_body: String,
    pub matched_excerpt: String,
}

/// Stream channels through which LLM content deltas arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamChannel {
    /// Visible text content produced by the assistant.
    AssistantText,
    /// Extended thinking / reasoning trace.
    Thinking,
    /// Tool call arguments JSON payload (excluded from TTSR matching).
    ToolCallArgument,
}

/// Evaluation result from the TTSR coordinator for an incoming chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtsrAction {
    /// No rule matched or channel was skipped; stream proceeds normally.
    Continue,
    /// Rule matched within injection budget; stream must abort and retry with injection.
    AbortAndInject {
        rule: StreamRule,
        matched_excerpt: String,
        reminder_message: String,
    },
    /// Rule matched but per-turn injection cap was reached; stops stream to notify user.
    CapExceeded {
        rule: StreamRule,
        matched_excerpt: String,
        total_injections: usize,
    },
}

/// One channel's retained history. `search_start` skips the preceding character
/// when a window rolls, while letting regex anchors and word boundaries inspect
/// its real context. Slicing it away would make a rolling tail look like a new
/// stream (`^`, `\A`) or invent a word boundary in the middle of a word.
#[derive(Debug, Default)]
struct ChannelWindow {
    text: String,
    search_start: usize,
}

impl ChannelWindow {
    fn reset(&mut self) {
        self.text.clear();
        self.search_start = 0;
    }

    fn retain_tail(&mut self, text: &str, limit: usize) {
        let mut start = text.len().saturating_sub(limit);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let context_start = text[..start]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index);
        self.search_start = start - context_start;
        // Only the bounded tail ever enters this allocation. A huge coalesced
        // delta must not leave a huge-capacity String resident between feeds.
        self.text.clear();
        self.text.push_str(&text[context_start..]);
    }
}

fn screened_excerpt(text: &str) -> String {
    // Screen before truncation: cutting through a secret first could make its
    // remaining prefix evade the detector. Only the diagnostic is shortened;
    // matching always considers the entire supplied delta.
    let mut text = screen_secrets(text);
    if text.len() > MAX_MATCHED_EXCERPT_BYTES {
        let mut end = MAX_MATCHED_EXCERPT_BYTES - '…'.len_utf8();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

/// Evaluate the entire incoming delta plus bounded same-channel history.
/// Lookback limits cross-delta history, not the portion of a delta inspected.
/// Matches that need more historical bytes than the lookback are not promised;
/// regex end anchors still observe the current, not a future, stream prefix.
#[derive(Debug)]
pub struct RollingStreamMatcher {
    lookback_limit: usize,
    assistant: ChannelWindow,
    thinking: ChannelWindow,
    compiled_rules: Vec<(StreamRule, Regex)>,
}

impl RollingStreamMatcher {
    /// Create a new matcher with the given lookback window and active rules.
    #[must_use]
    pub fn new(rules: &[StreamRule], lookback_limit: usize) -> Self {
        let compiled = rules
            .iter()
            .filter(|r| r.enabled)
            .filter_map(|r| Regex::new(&r.pattern).ok().map(|re| (r.clone(), re)))
            .collect();

        Self {
            lookback_limit: if lookback_limit == 0 {
                DEFAULT_ROLLING_LOOKBACK_BYTES
            } else {
                lookback_limit
            },
            assistant: ChannelWindow::default(),
            thinking: ChannelWindow::default(),
            compiled_rules: compiled,
        }
    }

    /// Reset both channels at the start of a turn or retry attempt.
    pub fn reset(&mut self) {
        self.assistant.reset();
        self.thinking.reset();
    }

    /// Feed a streaming delta and check for matches.
    /// Returns `Some(StreamRuleMatch)` on the first rule that triggers.
    pub fn feed(&mut self, chunk: &str, channel: StreamChannel) -> Option<StreamRuleMatch> {
        self.feed_filtered(chunk, channel, |_| true)
    }

    /// Like [`Self::feed`], but only rules admitted by `admit` participate.
    /// The coordinator passes cooldown-suppressed rule ids here: a suppressed
    /// rule's match stays in the rolling buffer, so without the filter it
    /// would shadow every other rule for the rest of the turn.
    pub fn feed_filtered(
        &mut self,
        chunk: &str,
        channel: StreamChannel,
        admit: impl Fn(&str) -> bool,
    ) -> Option<StreamRuleMatch> {
        // Guard: Tool call argument JSON streams are strictly excluded from TTSR matching
        // to avoid corrupting tool payloads.
        if channel == StreamChannel::ToolCallArgument || chunk.is_empty() {
            return None;
        }

        let window = match channel {
            StreamChannel::AssistantText => &mut self.assistant,
            StreamChannel::Thinking => &mut self.thinking,
            StreamChannel::ToolCallArgument => return None,
        };
        // Inspect before trimming: providers can coalesce many tokens into one
        // delta, including a complete answer larger than the lookback budget.
        // Thinking and visible prose must never synthesize each other's text.
        let text = if window.text.is_empty() {
            Cow::Borrowed(chunk)
        } else {
            let mut text = window.text.clone();
            text.push_str(chunk);
            Cow::Owned(text)
        };
        let matched = self.compiled_rules.iter().find_map(|(rule, regex)| {
            if !admit(&rule.id) {
                return None;
            }
            regex
                .find_at(&text, window.search_start)
                .map(|matched| StreamRuleMatch {
                    rule_id: rule.id.clone(),
                    rule_name: rule.name.clone(),
                    rule_body: rule.body.clone(),
                    matched_excerpt: screened_excerpt(matched.as_str()),
                })
        });
        window.retain_tail(&text, self.lookback_limit);
        matched
    }
}

/// TTSR coordinator tracking turn injection budgets, cooldowns, and retry reminders.
#[derive(Debug)]
pub struct TtsrCoordinator {
    matcher: RollingStreamMatcher,
    rules_by_id: HashMap<String, StreamRule>,
    max_injections_per_turn: usize,
    turn_injections: usize,
    injected_in_current_turn: HashSet<String>,
    cooldown_history: HashMap<String, usize>,
    current_turn: usize,
}

impl TtsrCoordinator {
    /// Create a coordinator with given rules and configuration.
    #[must_use]
    pub fn new(rules: &[StreamRule], max_injections: usize, lookback_bytes: usize) -> Self {
        let rules_map: HashMap<String, StreamRule> =
            rules.iter().cloned().map(|r| (r.id.clone(), r)).collect();

        Self {
            matcher: RollingStreamMatcher::new(rules, lookback_bytes),
            rules_by_id: rules_map,
            max_injections_per_turn: if max_injections == 0 {
                DEFAULT_MAX_INJECTIONS_PER_TURN
            } else {
                max_injections
            },
            turn_injections: 0,
            injected_in_current_turn: HashSet::new(),
            cooldown_history: HashMap::new(),
            current_turn: 0,
        }
    }

    /// Advance the current turn counter and reset per-turn injection counters.
    pub fn advance_turn(&mut self, turn_number: usize) {
        self.current_turn = turn_number;
        self.turn_injections = 0;
        self.injected_in_current_turn.clear();
        self.matcher.reset();
    }

    /// Reset stream buffer for a retry attempt within the same turn.
    pub fn reset_attempt(&mut self) {
        self.matcher.reset();
    }

    /// Format a system reminder message for a matched stream rule.
    #[must_use]
    pub fn format_reminder(rule: &StreamRule, matched_excerpt: &str) -> String {
        let screened_excerpt = screen_secrets(matched_excerpt);
        format!(
            "[SYSTEM REMINDER: Violation of stream rule '{name}']\n\n\
             Rule Directive:\n{body}\n\n\
             Offending Excerpt Matched:\n\"{screened_excerpt}\"\n\n\
             Please adjust your response immediately to strictly comply with this rule.",
            name = rule.name,
            body = rule.body.trim(),
        )
    }

    /// Process a stream chunk and evaluate if a TTSR action is required.
    pub fn process_chunk(&mut self, chunk: &str, channel: StreamChannel) -> TtsrAction {
        // Rules on cooldown are filtered at match time, not after: their
        // matched text stays in the rolling buffer, and post-hoc dropping
        // would let one cooling rule shadow every later rule this turn.
        let cooldowns: Vec<String> = self
            .rules_by_id
            .values()
            .filter(|rule| {
                rule.cooldown_turns.is_some_and(|cooldown| {
                    self.cooldown_history
                        .get(&rule.id)
                        .is_some_and(|last| self.current_turn.saturating_sub(*last) <= cooldown)
                })
            })
            .map(|rule| rule.id.clone())
            .collect();
        let Some(rule_match) = self
            .matcher
            .feed_filtered(chunk, channel, |id| !cooldowns.iter().any(|c| c == id))
        else {
            return TtsrAction::Continue;
        };

        let Some(rule) = self.rules_by_id.get(&rule_match.rule_id).cloned() else {
            return TtsrAction::Continue;
        };

        // Check if rule is on cooldown from a previous turn
        if let Some(cooldown) = rule.cooldown_turns
            && let Some(last_injected) = self.cooldown_history.get(&rule.id)
            && self.current_turn.saturating_sub(*last_injected) <= cooldown
        {
            return TtsrAction::Continue;
        }

        // Check turn injection cap
        if self.turn_injections >= self.max_injections_per_turn {
            return TtsrAction::CapExceeded {
                rule,
                matched_excerpt: rule_match.matched_excerpt,
                total_injections: self.turn_injections,
            };
        }

        self.turn_injections += 1;
        self.injected_in_current_turn.insert(rule.id.clone());
        self.cooldown_history
            .insert(rule.id.clone(), self.current_turn);

        let reminder = Self::format_reminder(&rule, &rule_match.matched_excerpt);

        TtsrAction::AbortAndInject {
            rule,
            matched_excerpt: rule_match.matched_excerpt,
            reminder_message: reminder,
        }
    }

    /// Check the count of injections performed during this turn.
    #[must_use]
    pub const fn current_turn_injections(&self) -> usize {
        self.turn_injections
    }
}

/// Rule file format for serialization in `.pi/stream-rules.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StreamRulesConfigFile {
    pub version: u32,
    pub rules: Vec<StreamRule>,
}

/// Project-scoped and global storage manager for stream rules.
#[derive(Debug, Default, Clone)]
pub struct StreamRuleStore {
    project_rules: Vec<StreamRule>,
    global_rules: Vec<StreamRule>,
    project_file_path: Option<PathBuf>,
    global_file_path: Option<PathBuf>,
}

impl StreamRuleStore {
    /// Load stream rules for the given project directory and user home directory.
    pub fn load_for_project(project_root: &Path) -> Self {
        let project_path = project_root.join(".pi").join("stream-rules.json");
        let global_path =
            dirs::home_dir().map(|h| h.join(".pi").join("agent").join("stream-rules.json"));

        let mut store = Self {
            project_rules: Vec::new(),
            global_rules: Vec::new(),
            project_file_path: Some(project_path.clone()),
            global_file_path: global_path.clone(),
        };

        if project_path.exists()
            && let Ok(content) = fs::read_to_string(&project_path)
            && let Ok(cfg) = serde_json::from_str::<StreamRulesConfigFile>(&content)
        {
            store.project_rules = cfg.rules;
        }

        if let Some(ref gp) = global_path
            && gp.exists()
            && let Ok(content) = fs::read_to_string(gp)
            && let Ok(cfg) = serde_json::from_str::<StreamRulesConfigFile>(&content)
        {
            store.global_rules = cfg.rules;
        }

        store
    }

    /// List all effective rules (project rules take precedence over global rules with the same ID).
    #[must_use]
    pub fn list_all_rules(&self) -> Vec<StreamRule> {
        let mut map: HashMap<&str, &StreamRule> = HashMap::new();
        for r in &self.global_rules {
            map.insert(r.id.as_str(), r);
        }
        for r in &self.project_rules {
            map.insert(r.id.as_str(), r);
        }
        let mut list: Vec<StreamRule> = map.into_values().cloned().collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// List only project-scoped rules.
    #[must_use]
    pub fn list_project_rules(&self) -> &[StreamRule] {
        &self.project_rules
    }

    /// List only global-scoped rules.
    #[must_use]
    pub fn list_global_rules(&self) -> &[StreamRule] {
        &self.global_rules
    }

    /// Add or update a stream rule.
    pub fn add_rule(&mut self, rule: StreamRule, is_global: bool) -> Result<()> {
        // Validate regex compilation
        Regex::new(&rule.pattern)
            .map_err(|e| Error::Validation(format!("Invalid regex pattern: {e}")))?;

        if is_global {
            if let Some(target) = self.global_rules.iter_mut().find(|r| r.id == rule.id) {
                *target = rule;
            } else {
                self.global_rules.push(rule);
            }
            self.save_global()?;
        } else {
            if let Some(target) = self.project_rules.iter_mut().find(|r| r.id == rule.id) {
                *target = rule;
            } else {
                self.project_rules.push(rule);
            }
            self.save_project()?;
        }

        Ok(())
    }

    /// Remove a stream rule by ID from project or global scope.
    pub fn remove_rule(&mut self, rule_id: &str) -> Result<bool> {
        let initial_proj_len = self.project_rules.len();
        self.project_rules.retain(|r| r.id != rule_id);
        let removed_proj = self.project_rules.len() < initial_proj_len;
        if removed_proj {
            self.save_project()?;
        }

        let initial_glob_len = self.global_rules.len();
        self.global_rules.retain(|r| r.id != rule_id);
        let removed_glob = self.global_rules.len() < initial_glob_len;
        if removed_glob {
            self.save_global()?;
        }

        Ok(removed_proj || removed_glob)
    }

    /// Toggle rule enabled status.
    pub fn toggle_rule(&mut self, rule_id: &str, enabled: bool) -> Result<bool> {
        let mut updated = false;
        for r in &mut self.project_rules {
            if r.id == rule_id {
                r.enabled = enabled;
                updated = true;
            }
        }
        if updated {
            self.save_project()?;
            return Ok(true);
        }

        for r in &mut self.global_rules {
            if r.id == rule_id {
                r.enabled = enabled;
                updated = true;
            }
        }
        if updated {
            self.save_global()?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Test a regex pattern or existing rule ID against sample text.
    pub fn test_pattern(&self, pattern_or_id: &str, sample_text: &str) -> Result<Option<String>> {
        let pattern = if let Some(rule) = self
            .list_all_rules()
            .into_iter()
            .find(|r| r.id == pattern_or_id)
        {
            rule.pattern
        } else {
            pattern_or_id.to_string()
        };

        let regex = Regex::new(&pattern)
            .map_err(|e| Error::Validation(format!("Invalid regex pattern: {e}")))?;

        Ok(regex.find(sample_text).map(|mat| mat.as_str().to_string()))
    }

    /// Export effective rules as pretty-printed JSON.
    pub fn export_json(&self) -> Result<String> {
        let all_rules = self.list_all_rules();
        let cfg = StreamRulesConfigFile {
            version: 1,
            rules: all_rules,
        };
        serde_json::to_string_pretty(&cfg)
            .map_err(|e| Error::Validation(format!("Serialization failure: {e}")))
    }

    /// Import rules from a JSON string.
    pub fn import_json(&mut self, json_str: &str, is_global: bool) -> Result<usize> {
        let cfg: StreamRulesConfigFile = serde_json::from_str(json_str)
            .map_err(|e| Error::Validation(format!("Invalid JSON format: {e}")))?;

        let count = cfg.rules.len();
        for rule in cfg.rules {
            self.add_rule(rule, is_global)?;
        }
        Ok(count)
    }

    fn save_project(&self) -> Result<()> {
        if let Some(ref path) = self.project_file_path {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    Error::Io(Box::new(std::io::Error::other(format!(
                        "Failed to create directory {}: {e}",
                        parent.display()
                    ))))
                })?;
            }
            let cfg = StreamRulesConfigFile {
                version: 1,
                rules: self.project_rules.clone(),
            };
            let json = serde_json::to_string_pretty(&cfg)
                .map_err(|e| Error::Validation(format!("Serialization failure: {e}")))?;
            fs::write(path, json).map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to write {}: {e}",
                    path.display()
                ))))
            })?;
        }
        Ok(())
    }

    fn save_global(&self) -> Result<()> {
        if let Some(ref path) = self.global_file_path {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    Error::Io(Box::new(std::io::Error::other(format!(
                        "Failed to create directory {}: {e}",
                        parent.display()
                    ))))
                })?;
            }
            let cfg = StreamRulesConfigFile {
                version: 1,
                rules: self.global_rules.clone(),
            };
            let json = serde_json::to_string_pretty(&cfg)
                .map_err(|e| Error::Validation(format!("Serialization failure: {e}")))?;
            fs::write(path, json).map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to write {}: {e}",
                    path.display()
                ))))
            })?;
        }
        Ok(())
    }
}

/// A recorded user complaint or grievance in the project ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grievance {
    pub id: String,
    pub timestamp: String,
    pub complaint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_rule_id: Option<String>,
    #[serde(default)]
    pub resolved: bool,
}

/// Per-project grievances ledger manager (`.pi/grievances.jsonl`).
#[derive(Debug)]
pub struct GrievancesLedger;

impl GrievancesLedger {
    /// File path for the grievances ledger.
    #[must_use]
    pub fn ledger_path(project_root: &Path) -> PathBuf {
        project_root.join(".pi").join("grievances.jsonl")
    }

    /// Record a user complaint in the project grievances ledger.
    pub fn record_complaint(
        project_root: &Path,
        complaint: &str,
        suggested_rule_id: Option<&str>,
    ) -> Result<Grievance> {
        let path = Self::ledger_path(project_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to create {}: {e}",
                    parent.display()
                ))))
            })?;
        }

        let screened_complaint = screen_secrets(complaint.trim());
        let id = format!("grv-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let timestamp = chrono::Utc::now().to_rfc3339();

        let grievance = Grievance {
            id,
            timestamp,
            complaint: screened_complaint,
            suggested_rule_id: suggested_rule_id.map(ToString::to_string),
            resolved: false,
        };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to open grievances ledger: {e}"
                ))))
            })?;

        let line = serde_json::to_string(&grievance)
            .map_err(|e| Error::Validation(format!("Serialization error: {e}")))?;
        writeln!(file, "{line}").map_err(|e| {
            Error::Io(Box::new(std::io::Error::other(format!(
                "Failed to append to grievances ledger: {e}"
            ))))
        })?;

        Ok(grievance)
    }

    /// List all grievances recorded in the project ledger.
    pub fn list_grievances(project_root: &Path) -> Result<Vec<Grievance>> {
        let path = Self::ledger_path(project_root);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&path).map_err(|e| {
            Error::Io(Box::new(std::io::Error::other(format!(
                "Failed to read grievances ledger: {e}"
            ))))
        })?;

        let mut grievances = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty()
                && let Ok(g) = serde_json::from_str::<Grievance>(trimmed)
            {
                grievances.push(g);
            }
        }
        Ok(grievances)
    }

    /// Forge a candidate stream rule from a grievance description.
    #[must_use]
    pub fn forge_candidate_rule(grievance: &Grievance) -> StreamRule {
        let safe_name = grievance
            .complaint
            .chars()
            .take(30)
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect::<String>()
            .to_ascii_lowercase();

        let rule_id = format!("rule-{}", grievance.id);
        let rule_name = if safe_name.is_empty() {
            format!("Rule from {}", grievance.id)
        } else {
            safe_name
        };

        let pattern = format!(
            r"(?i)\b({})\b",
            regex::escape(&grievance.complaint.chars().take(20).collect::<String>())
        );

        let body = format!(
            "Avoid recurring issue recorded in grievance {}: {}",
            grievance.id, grievance.complaint
        );

        StreamRule {
            id: rule_id,
            name: rule_name,
            pattern,
            body,
            enabled: true,
            created_from: Some(grievance.id.clone()),
            cooldown_turns: Some(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn stream_rule(id: &str, pattern: &str) -> StreamRule {
        StreamRule {
            id: id.to_string(),
            name: id.to_string(),
            pattern: pattern.to_string(),
            body: "Keep the user-defined constraint.".to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: None,
        }
    }

    #[test]
    fn complete_large_deltas_are_checked_before_retaining_the_tail() {
        for offset in [0, 4096, 8192] {
            for channel in [StreamChannel::AssistantText, StreamChannel::Thinking] {
                let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", "DANGER")], 16);
                let text = format!("{}DANGER{}", "x".repeat(offset), "y".repeat(8192 - offset));
                let found = matcher.feed(&text, channel).expect("whole delta checked");
                assert_eq!(found.matched_excerpt, "DANGER");
                assert_eq!(found.rule_id, "bad");
            }
        }
    }

    #[test]
    fn a_large_following_delta_cannot_erase_a_cross_chunk_match() {
        let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", "DANGER")], 16);
        assert!(
            matcher
                .feed("prefix DAN", StreamChannel::AssistantText)
                .is_none()
        );
        let next = format!("GER{}", "x".repeat(8192));
        let found = matcher.feed(&next, StreamChannel::AssistantText).unwrap();
        assert_eq!(found.matched_excerpt, "DANGER");
    }

    #[test]
    fn full_delta_matches_may_be_longer_than_the_history_budget() {
        let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", "BEGINx+END")], 4);
        let found = matcher
            .feed("BEGINxxxxxxxxEND", StreamChannel::AssistantText)
            .unwrap();
        assert_eq!(found.matched_excerpt, "BEGINxxxxxxxxEND");
    }

    #[test]
    fn thinking_and_prose_do_not_synthesize_a_violation_together() {
        let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", "DANGER")], 16);
        assert!(matcher.feed("DAN", StreamChannel::AssistantText).is_none());
        assert!(matcher.feed("GER", StreamChannel::Thinking).is_none());
        let found = matcher.feed("GER", StreamChannel::AssistantText).unwrap();
        assert_eq!(found.matched_excerpt, "DANGER");
    }

    #[test]
    fn interleaved_channels_keep_their_own_cross_chunk_history() {
        for (first, other) in [
            (StreamChannel::AssistantText, StreamChannel::Thinking),
            (StreamChannel::Thinking, StreamChannel::AssistantText),
        ] {
            let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", "DANGER")], 16);
            assert!(matcher.feed("DAN", first).is_none());
            assert!(matcher.feed(&"safe".repeat(100), other).is_none());
            assert_eq!(
                matcher.feed("GER", first).unwrap().matched_excerpt,
                "DANGER"
            );
        }
    }

    #[test]
    fn excluded_and_empty_deltas_neither_match_nor_change_history() {
        let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", "DANGER")], 16);
        assert!(matcher.feed("DAN", StreamChannel::AssistantText).is_none());
        assert!(matcher.feed("DAN", StreamChannel::Thinking).is_none());
        assert!(
            matcher
                .feed("DANGER", StreamChannel::ToolCallArgument)
                .is_none()
        );
        assert!(matcher.feed("", StreamChannel::AssistantText).is_none());
        assert_eq!(matcher.assistant.text, "DAN");
        assert_eq!(matcher.thinking.text, "DAN");
        assert_eq!(
            matcher
                .feed("GER", StreamChannel::Thinking)
                .unwrap()
                .matched_excerpt,
            "DANGER"
        );
    }

    #[test]
    fn rolling_history_does_not_invent_stream_start_anchors() {
        for pattern in [r"^danger!", r"\Adanger!"] {
            let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", pattern)], 6);
            assert!(
                matcher
                    .feed("xxdanger", StreamChannel::AssistantText)
                    .is_none()
            );
            assert!(matcher.feed("!", StreamChannel::AssistantText).is_none());
            matcher.reset();
            assert!(
                matcher
                    .feed("danger!", StreamChannel::AssistantText)
                    .is_some()
            );
        }
    }

    #[test]
    fn rolling_history_preserves_the_real_word_boundary_context() {
        for prefix in ["x", "é", "_", "9"] {
            let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", r"\bdanger!")], 6);
            assert!(
                matcher
                    .feed(&format!("{prefix}danger"), StreamChannel::AssistantText)
                    .is_none()
            );
            assert!(
                matcher.feed("!", StreamChannel::AssistantText).is_none(),
                "{prefix}"
            );
        }
        let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", r"\bdanger!")], 6);
        assert!(
            matcher
                .feed(" danger", StreamChannel::AssistantText)
                .is_none()
        );
        assert!(matcher.feed("!", StreamChannel::AssistantText).is_some());
    }

    #[test]
    fn retained_newlines_still_authorize_real_multiline_start_anchors() {
        let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", r"(?m)^danger!")], 6);
        assert!(
            matcher
                .feed("safe\ndanger", StreamChannel::AssistantText)
                .is_none()
        );
        assert_eq!(
            matcher
                .feed("!", StreamChannel::AssistantText)
                .unwrap()
                .matched_excerpt,
            "danger!"
        );
    }

    #[test]
    fn unicode_literals_match_across_every_valid_transport_split() {
        let literal = "🦀café危险";
        let text = format!("prefix {literal} suffix");
        for split in text
            .char_indices()
            .map(|(index, _)| index)
            .chain([text.len()])
        {
            let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", literal)], 32);
            let first = matcher.feed(&text[..split], StreamChannel::AssistantText);
            let second = matcher.feed(&text[split..], StreamChannel::AssistantText);
            let found = first.or(second).expect("literal survives any valid split");
            assert_eq!(found.matched_excerpt, literal, "split {split}");
        }
    }

    #[test]
    fn retained_memory_is_bounded_even_after_large_unicode_chunks() {
        let large = "🦀éx".repeat(16_384);
        for limit in [1, 2, 3, 4, 17, 4096] {
            let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", "absent")], limit);
            for channel in [StreamChannel::AssistantText, StreamChannel::Thinking] {
                for chunk in [large.as_str(), "🦀", "é", "x"] {
                    assert!(matcher.feed(chunk, channel).is_none());
                }
            }
            for window in [&matcher.assistant, &matcher.thinking] {
                assert!(window.text.len() <= limit + 4);
                assert!(window.text.is_char_boundary(window.search_start));
                assert!(window.text.len() - window.search_start <= limit);
                assert!(window.text.capacity() < large.len() / 2);
            }
        }
    }

    #[test]
    fn reset_clears_both_histories_and_anchor_context() {
        let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", "DANGER")], 3);
        assert!(matcher.feed("xxDAN", StreamChannel::AssistantText).is_none());
        assert!(matcher.feed("xxDAN", StreamChannel::Thinking).is_none());
        matcher.reset();
        for channel in [StreamChannel::AssistantText, StreamChannel::Thinking] {
            assert!(matcher.feed("GER", channel).is_none());
        }
        assert_eq!(matcher.assistant.search_start, 0);
        assert_eq!(matcher.thinking.search_start, 0);
    }

    #[test]
    fn admitted_rule_order_remains_deterministic_for_a_coalesced_delta() {
        let rules = [stream_rule("first", "LATE"), stream_rule("second", "EARLY")];
        let text = format!("EARLY{}LATE{}", "x".repeat(4096), "x".repeat(4096));
        let mut matcher = RollingStreamMatcher::new(&rules, 16);
        assert_eq!(
            matcher
                .feed(&text, StreamChannel::AssistantText)
                .unwrap()
                .rule_id,
            "first"
        );
        matcher.reset();
        assert_eq!(
            matcher
                .feed_filtered(&text, StreamChannel::AssistantText, |id| id != "first")
                .unwrap()
                .rule_id,
            "second"
        );
    }

    #[test]
    fn coordinator_enforces_large_deltas_through_retry_cap_and_cooldowns() {
        let rules = [stream_rule("bad", "DANGER")];
        let mut coordinator = TtsrCoordinator::new(&rules, 1, 16);
        let text = format!("DANGER{}", "x".repeat(8192));
        coordinator.advance_turn(1);
        assert!(matches!(
            coordinator.process_chunk(&text, StreamChannel::Thinking),
            TtsrAction::AbortAndInject { .. }
        ));
        coordinator.reset_attempt();
        assert!(matches!(
            coordinator.process_chunk(&text, StreamChannel::AssistantText),
            TtsrAction::CapExceeded {
                total_injections: 1,
                ..
            }
        ));
        coordinator.advance_turn(2);
        assert!(matches!(
            coordinator.process_chunk(&text, StreamChannel::AssistantText),
            TtsrAction::AbortAndInject { .. }
        ));

        let mut cooling = stream_rule("cooling", "COOL");
        cooling.cooldown_turns = Some(2);
        let mut coordinator = TtsrCoordinator::new(&[cooling, stream_rule("bad", "DANGER")], 3, 16);
        coordinator.advance_turn(1);
        assert!(matches!(
            coordinator.process_chunk("COOL", StreamChannel::AssistantText),
            TtsrAction::AbortAndInject { .. }
        ));
        coordinator.advance_turn(2);
        let text = format!("COOL {text}");
        let TtsrAction::AbortAndInject { rule, .. } =
            coordinator.process_chunk(&text, StreamChannel::AssistantText)
        else {
            panic!("a cooling rule must not hide an admitted rule");
        };
        assert_eq!(rule.id, "bad");
    }

    #[test]
    fn large_match_diagnostics_are_screened_then_bounded_on_utf8_boundaries() {
        let mut matcher = RollingStreamMatcher::new(&[stream_rule("bad", r"(?s).+")], 16);
        let text = "é🦀".repeat(4096);
        let found = matcher.feed(&text, StreamChannel::AssistantText).unwrap();
        assert!(found.matched_excerpt.len() <= MAX_MATCHED_EXCERPT_BYTES);
        assert!(found.matched_excerpt.ends_with('…'));
        let short = "password=secret-canary";
        assert_eq!(screened_excerpt(short), screen_secrets(short));
    }

    #[test]
    fn test_rolling_stream_matcher_chunk_boundary_split() {
        let rules = vec![StreamRule {
            id: "no-box-leak".to_string(),
            name: "No Box::leak".to_string(),
            pattern: r"Box::leak".to_string(),
            body: "Never use Box::leak; use structured concurrency and scoped references."
                .to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: None,
        }];

        let mut matcher = RollingStreamMatcher::new(&rules, 4096);

        // Feed first chunk
        let match1 = matcher.feed("Let us allocate with Box::", StreamChannel::AssistantText);
        assert!(match1.is_none());

        // Feed second chunk which completes the pattern across boundaries
        let match2 = matcher.feed("leak(boxed_val);", StreamChannel::AssistantText);
        let Some(m) = match2 else {
            assert!(false, "Pattern across chunk boundaries should match");
            return;
        };

        assert_eq!(m.rule_id, "no-box-leak");
        assert_eq!(m.matched_excerpt, "Box::leak");
    }

    #[test]
    fn test_tool_call_arguments_ignored() {
        let rules = vec![StreamRule {
            id: "no-panic".to_string(),
            name: "No panics".to_string(),
            pattern: r"panic!".to_string(),
            body: "Do not write panics.".to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: None,
        }];

        let mut matcher = RollingStreamMatcher::new(&rules, 4096);

        // Tool call arguments JSON stream containing "panic!" must NOT trigger TTSR match
        let match1 = matcher.feed(
            r#"{"command": "grep -rn 'panic!' src/"}"#,
            StreamChannel::ToolCallArgument,
        );
        assert!(match1.is_none());
    }

    #[test]
    fn test_ttsr_coordinator_turn_cap() {
        let rules = vec![StreamRule {
            id: "no-unwrap".to_string(),
            name: "No unwrap".to_string(),
            pattern: r"\.unwrap\(\)".to_string(),
            body: "Replace .unwrap() with error handling.".to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: None,
        }];

        let mut coord = TtsrCoordinator::new(&rules, 2, 4096);
        coord.advance_turn(1);

        // Injection 1
        let act1 = coord.process_chunk("let x = foo.unwrap();", StreamChannel::AssistantText);
        assert!(matches!(act1, TtsrAction::AbortAndInject { .. }));
        coord.reset_attempt();

        // Injection 2
        let act2 = coord.process_chunk("let y = bar.unwrap();", StreamChannel::AssistantText);
        assert!(matches!(act2, TtsrAction::AbortAndInject { .. }));
        coord.reset_attempt();

        // Injection 3 exceeds cap of 2
        let act3 = coord.process_chunk("let z = baz.unwrap();", StreamChannel::AssistantText);
        assert!(matches!(act3, TtsrAction::CapExceeded { .. }));
    }

    #[test]
    fn test_ttsr_coordinator_cooldown() {
        let rules = vec![StreamRule {
            id: "strict-format".to_string(),
            name: "Strict Format".to_string(),
            pattern: r"BAD_PATTERN".to_string(),
            body: "Do not output BAD_PATTERN.".to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: Some(2),
        }];

        let mut coord = TtsrCoordinator::new(&rules, 3, 4096);
        coord.advance_turn(1);

        // Turn 1 matches
        let act1 = coord.process_chunk("BAD_PATTERN", StreamChannel::AssistantText);
        assert!(matches!(act1, TtsrAction::AbortAndInject { .. }));

        // Turn 2 is within 2-turn cooldown -> skipped
        coord.advance_turn(2);
        let act2 = coord.process_chunk("BAD_PATTERN", StreamChannel::AssistantText);
        assert_eq!(act2, TtsrAction::Continue);

        // Turn 4 is past cooldown -> matches again
        coord.advance_turn(4);
        let act3 = coord.process_chunk("BAD_PATTERN", StreamChannel::AssistantText);
        assert!(matches!(act3, TtsrAction::AbortAndInject { .. }));
    }

    #[test]
    fn test_rules_store_and_grievances_ledger_persistence() {
        let Ok(tmp) = tempdir() else {
            return;
        };
        let project_dir = tmp.path();

        let mut store = StreamRuleStore::load_for_project(project_dir);
        let rule = StreamRule {
            id: "my-rule".to_string(),
            name: "My Rule".to_string(),
            pattern: r"TODO_FIXME".to_string(),
            body: "Avoid FIXME in committed code.".to_string(),
            enabled: true,
            created_from: None,
            cooldown_turns: None,
        };

        let Ok(()) = store.add_rule(rule, false) else {
            assert!(false, "add_rule failed");
            return;
        };

        let list = store.list_all_rules();
        assert_eq!(list.len(), 1);
        let Some(first_rule) = list.first() else {
            return;
        };
        assert_eq!(first_rule.id, "my-rule");

        // Test matching
        let test_match = store.test_pattern("my-rule", "Here is a TODO_FIXME comment");
        assert!(matches!(test_match, Ok(Some(mat)) if mat == "TODO_FIXME"));

        // Record grievance
        let Ok(grievance) = GrievancesLedger::record_complaint(
            project_dir,
            "Model keeps emitting raw unwrap without context",
            Some("my-rule"),
        ) else {
            assert!(false, "record_complaint failed");
            return;
        };

        assert_eq!(grievance.suggested_rule_id, Some("my-rule".to_string()));

        let Ok(grievances) = GrievancesLedger::list_grievances(project_dir) else {
            assert!(false, "list_grievances failed");
            return;
        };
        assert_eq!(grievances.len(), 1);

        let Some(first_grievance) = grievances.first() else {
            return;
        };
        let candidate = GrievancesLedger::forge_candidate_rule(first_grievance);
        assert!(candidate.pattern.contains("Model"));
    }
}
