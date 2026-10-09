//! Structured cross-session and cross-agent handoff generator.
//!
//! Provides generation of continuation briefs for oncoming human engineers
//! or autonomous agents when switching contexts, delegating tasks, or
//! handing off work between sessions (`bd-cv653.3.17`).

use crate::error::{Error, Result};
use crate::memory::screen_secrets;
use crate::model::{AssistantMessage, ContentBlock, UserContent};
use crate::session::{
    CompactionEntry, EntryBase, MessageEntry, Session, SessionEntry, SessionMessage,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Canonical schema identifier for the handoff JSON payload.
pub const HANDOFF_SCHEMA_V1: &str = "pi.handoff.v1";

type FileObservations = BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)>;

#[derive(Clone, Copy)]
enum ToolOutcome {
    Succeeded,
    Failed,
    Unconfirmed,
}

/// A result is evidence only for one unambiguous preceding call. Reused IDs,
/// duplicate outputs, and contradictory names must not select an arbitrary
/// successful record as proof that a file was changed.
#[derive(Default)]
struct ToolEvidence<'a> {
    call: Option<(usize, &'a str)>,
    result: Option<(usize, &'a str, bool)>,
    ambiguous: bool,
}

impl ToolEvidence<'_> {
    fn outcome(&self) -> ToolOutcome {
        let (Some((call_index, name)), Some((result_index, result_name, is_error))) =
            (self.call, self.result)
        else {
            return ToolOutcome::Unconfirmed;
        };
        if self.ambiguous
            || call_index >= result_index
            || name.trim().is_empty()
            || (!result_name.is_empty() && result_name != name)
        {
            return ToolOutcome::Unconfirmed;
        }
        if is_error {
            ToolOutcome::Failed
        } else {
            ToolOutcome::Succeeded
        }
    }
}

/// Target recipient or storage destination for a generated handoff brief.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffTarget {
    /// Human-facing report printed to stdout or written to disk.
    Human,
    /// Attached as a comment / note on a Beads issue ID via `br`.
    Bead(String),
    /// Formatted for an Agent Mail thread / agent-to-agent inbox message.
    Agent(String),
}

impl HandoffTarget {
    /// Parse a target specifier string (e.g. `human`, `bead:bd-123`, `agent:my-thread`).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        let trimmed = s.trim();
        if let Some(rest) = trimmed
            .strip_prefix("bead:")
            .or_else(|| trimmed.strip_prefix("bead="))
        {
            return Self::Bead(rest.trim().to_string());
        }
        if trimmed.eq_ignore_ascii_case("bead") {
            return Self::Bead(String::new());
        }
        if let Some(rest) = trimmed
            .strip_prefix("agent:")
            .or_else(|| trimmed.strip_prefix("agent="))
        {
            return Self::Agent(rest.trim().to_string());
        }
        if trimmed.eq_ignore_ascii_case("agent") {
            return Self::Agent(String::new());
        }
        Self::Human
    }
}

/// A key decision made during the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    /// Summary of the decision.
    pub decision: String,
    /// Rationale or motivation for the decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    /// Reference to session message, turn, or file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_point: Option<String>,
}

/// A failed approach or attempt, capturing the reason and context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedApproach {
    /// What was attempted.
    pub attempt: String,
    /// Why the attempt failed (compiler error, test failure, logic issue, etc.).
    pub reason: String,
    /// Reference to tool call, exit code, or entry ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ref_point: Option<String>,
}

/// File mentioned by a tool call or compaction report.
/// Recorded tool outcomes qualify the role; this is not a filesystem audit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTouched {
    /// File path.
    pub path: String,
    /// Reported effects and failed or unconfirmed attempts, in stable order.
    pub role: String,
    /// Line or range references if applicable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub line_refs: Vec<String>,
}

/// Complete handoff document containing structured session analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffDocument {
    /// Schema version (`pi.handoff.v1`).
    pub schema: String,
    /// Session identifier.
    pub session_id: String,
    /// ISO-8601 UTC timestamp of handoff generation.
    pub timestamp: String,
    /// Goal / primary objective of the session.
    pub goal: String,
    /// Current state at handoff time.
    pub current_state: String,
    /// Architectural and design decisions made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<Decision>,
    /// First-class failure memory: what didn't work and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_approaches: Vec<FailedApproach>,
    /// Files accessed or modified during the session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_touched: Vec<FileTouched>,
    /// Unresolved blockers or obstacles.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    /// Open questions or threads requiring follow-up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_threads: Vec<String>,
    /// Recommended next steps for the successor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_steps: Vec<String>,
    /// Procedural lessons learned (suitable for `cm playbook add`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lessons: Vec<String>,
    /// Number of compaction summaries incorporated.
    #[serde(default)]
    pub compaction_summaries_count: usize,
}

impl HandoffDocument {
    /// Format the handoff as a structured Markdown document.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn to_markdown(&self) -> String {
        use std::fmt::Write as _;
        let mut md = String::new();
        md.push_str("# Session Handoff Brief\n\n");
        let _ = writeln!(md, "- **Session ID:** `{}`", self.session_id);
        let _ = writeln!(md, "- **Timestamp:** `{}`", self.timestamp);
        let _ = writeln!(md, "- **Schema:** `{}`", self.schema);
        if self.compaction_summaries_count > 0 {
            let _ = writeln!(
                md,
                "- **Compaction Summaries Integrated:** {}",
                self.compaction_summaries_count
            );
        }
        md.push('\n');

        md.push_str("## 1. Goal & Objective\n\n");
        md.push_str(self.goal.trim());
        md.push_str("\n\n");

        md.push_str("## 2. Current State\n\n");
        md.push_str(self.current_state.trim());
        md.push_str("\n\n");

        md.push_str("## 3. Key Decisions\n\n");
        if self.decisions.is_empty() {
            md.push_str("_No explicit architectural decisions recorded._\n\n");
        } else {
            for (idx, d) in self.decisions.iter().enumerate() {
                let _ = write!(md, "{}. **{}**", idx + 1, d.decision.trim());
                if let Some(rationale) = &d.rationale {
                    let _ = write!(md, " — {}", rationale.trim());
                }
                if let Some(ref_point) = &d.ref_point {
                    let _ = write!(md, " *(Ref: `{}`)*", ref_point.trim());
                }
                md.push('\n');
            }
            md.push('\n');
        }

        md.push_str("## 4. Failed Approaches & Failure Memory\n\n");
        if self.failed_approaches.is_empty() {
            md.push_str("_No failed approaches or errors encountered._\n\n");
        } else {
            for (idx, f) in self.failed_approaches.iter().enumerate() {
                let _ = writeln!(
                    md,
                    "{}. **Attempt:** {}\n   - **Why it failed:** {}",
                    idx + 1,
                    f.attempt.trim(),
                    f.reason.trim()
                );
                if let Some(ref_point) = &f.ref_point {
                    let _ = writeln!(md, "   - **Ref:** `{}`", ref_point.trim());
                }
            }
            md.push('\n');
        }

        md.push_str("## 5. Files Touched\n\n");
        if self.files_touched.is_empty() {
            md.push_str("_No files modified or accessed during this session._\n\n");
        } else {
            md.push_str("| File Path | Role | Line References |\n");
            md.push_str("| :--- | :--- | :--- |\n");
            for f in &self.files_touched {
                if f.line_refs.is_empty() {
                    let _ = writeln!(md, "| `{}` | {} | - |", f.path, f.role);
                } else {
                    let _ = write!(md, "| `{}` | {} | ", f.path, f.role);
                    for (i, r) in f.line_refs.iter().enumerate() {
                        if i > 0 {
                            md.push_str(", ");
                        }
                        md.push_str(r);
                    }
                    md.push_str(" |\n");
                }
            }
            md.push('\n');
        }

        md.push_str("## 6. Blockers & Open Threads\n\n");
        if self.blockers.is_empty() && self.open_threads.is_empty() {
            md.push_str("_None reported._\n\n");
        } else {
            if !self.blockers.is_empty() {
                md.push_str("### Blockers\n");
                for b in &self.blockers {
                    let _ = writeln!(md, "- ⛔ {}", b.trim());
                }
                md.push('\n');
            }
            if !self.open_threads.is_empty() {
                md.push_str("### Open Threads & Questions\n");
                for t in &self.open_threads {
                    let _ = writeln!(md, "- ❓ {}", t.trim());
                }
                md.push('\n');
            }
        }

        md.push_str("## 7. Recommended Next Steps\n\n");
        if self.next_steps.is_empty() {
            md.push_str("- [ ] Continue review and verification.\n\n");
        } else {
            for step in &self.next_steps {
                let _ = writeln!(md, "- [ ] {}", step.trim());
            }
            md.push('\n');
        }

        md.push_str("## 8. Lessons Learned (`cm playbook add` Candidates)\n\n");
        if self.lessons.is_empty() {
            md.push_str("_No procedural lessons recorded._\n");
        } else {
            for l in &self.lessons {
                let _ = writeln!(md, "- 💡 {}", l.trim());
            }
        }

        md
    }

    /// Serialize to JSON string matching `pi.handoff.v1`.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::session(format!("Failed to serialize handoff to JSON: {e}")))
    }
}

/// Delivery report returned when a handoff is emitted or posted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffDeliveryReport {
    /// Target destination.
    pub target: HandoffTarget,
    /// Path to written markdown brief, if any.
    pub markdown_path: Option<PathBuf>,
    /// Path to written JSON sidecar, if any.
    pub json_path: Option<PathBuf>,
    /// Success or status message.
    pub status: String,
    /// Note whether external tool delivery succeeded.
    pub external_delivery_success: bool,
}

/// Handoff generator and session analyzer.
pub struct HandoffGenerator;

impl HandoffGenerator {
    /// Generate a handoff for the selected branch, not the physical append log.
    ///
    /// Abandoned retry attempts and sibling branches stay available in the
    /// session tree, but must not become the successor's decisions, failures,
    /// or next steps. Borrow the selected entries so large tool payloads are
    /// not cloned just to choose the branch.
    #[must_use]
    pub fn generate_from_session(session: &Session) -> HandoffDocument {
        Self::generate_from_entry_refs(&session.header.id, session.entries_for_current_path())
    }

    fn extract_user_text(content: &UserContent) -> String {
        match content {
            UserContent::Text(t) => screen_secrets(t),
            UserContent::Blocks(blocks) => {
                let mut s = String::new();
                for b in blocks {
                    if let ContentBlock::Text(t) = b {
                        s.push_str(&t.text);
                        s.push('\n');
                    }
                }
                screen_secrets(&s)
            }
        }
    }

    fn extract_assistant_text(message: &AssistantMessage) -> String {
        let mut text_acc = String::new();
        for block in &message.content {
            if let ContentBlock::Text(t) = block {
                text_acc.push_str(&t.text);
                text_acc.push('\n');
            }
        }
        screen_secrets(&text_acc)
    }

    fn extract_tool_result_text(content: &[ContentBlock]) -> String {
        let mut tool_output = String::new();
        for block in content {
            if let ContentBlock::Text(t) = block {
                tool_output.push_str(&t.text);
            }
        }
        tool_output
    }

    #[allow(clippy::too_many_arguments)]
    fn process_message_entry(
        message: &SessionMessage,
        base: &EntryBase,
        turn_index: usize,
        goal: &mut String,
        latest_state_text: &mut String,
        decisions: &mut Vec<Decision>,
        blockers: &mut Vec<String>,
        open_threads: &mut Vec<String>,
        next_steps: &mut Vec<String>,
        lessons: &mut Vec<String>,
        failed_approaches: &mut Vec<FailedApproach>,
    ) {
        let ref_str = base
            .id
            .as_deref()
            .map_or_else(|| format!("turn#{turn_index}"), ToString::to_string);

        match message {
            SessionMessage::User { content, .. } => {
                let screened = Self::extract_user_text(content);
                if goal.is_empty() && !screened.trim().is_empty() {
                    *goal = screened.trim().to_string();
                }
            }
            SessionMessage::Assistant { message } => {
                let screened = Self::extract_assistant_text(message);
                if !screened.trim().is_empty() {
                    Self::extract_structured_notes(
                        &screened,
                        &ref_str,
                        decisions,
                        blockers,
                        open_threads,
                        next_steps,
                        lessons,
                    );
                    *latest_state_text = screened;
                }
            }
            SessionMessage::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } => {
                let tool_output = Self::extract_tool_result_text(content);

                // Reading source or logs containing "Error:"/"FAILED" does
                // not itself mean the read failed. Trust the recorded outcome.
                if *is_error {
                    let screened = screen_secrets(&tool_output);
                    let first_err = screened
                        .lines()
                        .find(|l| {
                            l.contains("error")
                                || l.contains("Error")
                                || l.contains("failed")
                                || l.contains("FAILED")
                        })
                        .unwrap_or_else(|| {
                            screened.lines().next().unwrap_or("Tool execution failure")
                        });

                    failed_approaches.push(FailedApproach {
                        attempt: format!("Executed tool `{tool_name}`"),
                        reason: first_err.trim().to_string(),
                        ref_point: Some(ref_str),
                    });
                }
            }
            SessionMessage::BashExecution {
                command,
                output,
                exit_code,
                ..
            } => {
                let screened_cmd = screen_secrets(command);
                let screened_out = screen_secrets(output);

                if *exit_code != 0 {
                    let first_err = screened_out
                        .lines()
                        .find(|l| {
                            l.contains("error")
                                || l.contains("Error")
                                || l.contains("failed")
                                || l.contains("FAILED")
                        })
                        .unwrap_or_else(|| {
                            screened_out
                                .lines()
                                .next()
                                .unwrap_or("Non-zero command exit")
                        });

                    failed_approaches.push(FailedApproach {
                        attempt: format!("Run shell command `{screened_cmd}`"),
                        reason: format!("Exit code {exit_code}: {}", first_err.trim()),
                        ref_point: Some(ref_str),
                    });
                }
            }
            _ => {}
        }
    }

    /// Generate a handoff from an explicitly selected chronological record list.
    /// Use [`Self::generate_from_session`] when the session's active leaf should
    /// select the branch; this lower-level entrypoint analyzes every supplied row.
    #[must_use]
    pub fn generate_from_entries(session_id: &str, entries: &[SessionEntry]) -> HandoffDocument {
        Self::generate_from_entry_refs(session_id, entries)
    }

    #[allow(clippy::too_many_lines)]
    fn generate_from_entry_refs<'a>(
        session_id: &str,
        entries: impl IntoIterator<Item = &'a SessionEntry>,
    ) -> HandoffDocument {
        let entries = entries.into_iter().collect::<Vec<_>>();
        let mut goal = String::new();
        let mut decisions = Vec::new();
        let mut failed_approaches = Vec::new();
        let mut files_touched_map = Self::collect_file_observations(&entries);
        let mut blockers = Vec::new();
        let mut open_threads = Vec::new();
        let mut next_steps = Vec::new();
        let mut lessons = Vec::new();
        let mut compaction_count = 0;

        let mut turn_index = 0usize;
        let mut latest_state_text = String::new();

        for entry in entries {
            match entry {
                SessionEntry::Compaction(CompactionEntry { summary, .. }) => {
                    compaction_count += 1;
                    Self::extract_from_compaction_summary(
                        summary,
                        &mut decisions,
                        &mut failed_approaches,
                        &mut files_touched_map,
                        &mut lessons,
                    );
                }
                SessionEntry::Message(MessageEntry { message, base, .. }) => {
                    turn_index += 1;
                    Self::process_message_entry(
                        message,
                        base,
                        turn_index,
                        &mut goal,
                        &mut latest_state_text,
                        &mut decisions,
                        &mut blockers,
                        &mut open_threads,
                        &mut next_steps,
                        &mut lessons,
                        &mut failed_approaches,
                    );
                }
                SessionEntry::BranchSummary(bs) => {
                    let screened = screen_secrets(&bs.summary);
                    if !screened.trim().is_empty() {
                        latest_state_text = screened;
                    }
                }
                _ => {}
            }
        }

        if goal.is_empty() {
            goal = "No explicit initial goal detected in session history.".to_string();
        }

        // A branch summary is state at its position, not an override for all
        // later assistant replies. Empty replies leave the last useful state.
        let current_state = if latest_state_text.is_empty() {
            "Session active, awaiting next instructions.".to_string()
        } else {
            latest_state_text
                .lines()
                .take(6)
                .collect::<Vec<_>>()
                .join("\n")
        };

        let files_touched = files_touched_map
            .into_iter()
            .map(|(path, (roles, line_refs))| FileTouched {
                path: screen_secrets(&path),
                role: screen_secrets(&roles.into_iter().collect::<Vec<_>>().join("; ")),
                line_refs: line_refs.into_iter().collect(),
            })
            .collect();

        // Screen all fields one final time for complete safety
        HandoffDocument {
            schema: HANDOFF_SCHEMA_V1.to_string(),
            session_id: session_id.to_string(),
            timestamp: Utc::now().to_rfc3339(),
            goal: screen_secrets(&goal),
            current_state: screen_secrets(&current_state),
            decisions: decisions
                .into_iter()
                .map(|d| Decision {
                    decision: screen_secrets(&d.decision),
                    rationale: d.rationale.as_ref().map(|r| screen_secrets(r)),
                    ref_point: d.ref_point,
                })
                .collect(),
            failed_approaches: failed_approaches
                .into_iter()
                .map(|f| FailedApproach {
                    attempt: screen_secrets(&f.attempt),
                    reason: screen_secrets(&f.reason),
                    ref_point: f.ref_point,
                })
                .collect(),
            files_touched,
            blockers: blockers.into_iter().map(|b| screen_secrets(&b)).collect(),
            open_threads: open_threads
                .into_iter()
                .map(|t| screen_secrets(&t))
                .collect(),
            next_steps: next_steps.into_iter().map(|s| screen_secrets(&s)).collect(),
            lessons: lessons.into_iter().map(|l| screen_secrets(&l)).collect(),
            compaction_summaries_count: compaction_count,
        }
    }

    fn collect_file_observations(entries: &[&SessionEntry]) -> FileObservations {
        let mut evidence: HashMap<&str, ToolEvidence<'_>> = HashMap::new();
        for (index, entry) in entries.iter().copied().enumerate() {
            let SessionEntry::Message(entry) = entry else {
                continue;
            };
            match &entry.message {
                SessionMessage::Assistant { message } => {
                    for block in &message.content {
                        if let ContentBlock::ToolCall(call) = block
                            && !call.id.trim().is_empty()
                        {
                            let observed = evidence.entry(&call.id).or_default();
                            observed.ambiguous |= observed
                                .call
                                .replace((index, call.name.as_str()))
                                .is_some();
                        }
                    }
                }
                SessionMessage::ToolResult {
                    tool_call_id,
                    tool_name,
                    is_error,
                    ..
                } if !tool_call_id.trim().is_empty() => {
                    let observed = evidence.entry(tool_call_id).or_default();
                    observed.ambiguous |= observed
                        .result
                        .replace((index, tool_name.as_str(), *is_error))
                        .is_some();
                }
                _ => {}
            }
        }

        let mut files = FileObservations::new();
        for entry in entries.iter().copied() {
            let SessionEntry::Message(MessageEntry {
                message: SessionMessage::Assistant { message },
                ..
            }) = entry
            else {
                continue;
            };
            for block in &message.content {
                if let ContentBlock::ToolCall(call) = block {
                    let outcome = evidence
                        .get(call.id.as_str())
                        .map_or(ToolOutcome::Unconfirmed, ToolEvidence::outcome);
                    Self::record_tool_call(&call.name, &call.arguments, outcome, &mut files);
                }
            }
        }
        files
    }

    fn record_tool_call(
        tool_name: &str,
        arguments: &serde_json::Value,
        outcome: ToolOutcome,
        files_touched: &mut FileObservations,
    ) {
        let path = arguments
            .get("path")
            .or_else(|| arguments.get("file_path"))
            .or_else(|| arguments.get("TargetFile"))
            .or_else(|| arguments.get("AbsolutePath"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        if let Some(path) = path {
            let role = match tool_name {
                "write" | "write_to_file" => "created/overwritten",
                "edit" | "replace_file_content" | "hashline_edit" => "modified",
                "read" | "view_file" => "read",
                _ => "accessed",
            };

            let observation = match outcome {
                ToolOutcome::Succeeded => role.to_string(),
                ToolOutcome::Failed => {
                    format!("{tool_name} attempted (tool reported failure; effects may be partial)")
                }
                ToolOutcome::Unconfirmed => {
                    format!("{tool_name} attempted (outcome unconfirmed)")
                }
            };
            // Retain both successful effects and subsequent failed attempts:
            // neither can erase the other from the successor's evidence.
            let entry = files_touched.entry(path).or_default();
            entry.0.insert(observation);

            if let Some(start_line) = arguments
                .get("StartLine")
                .and_then(serde_json::Value::as_u64)
            {
                if let Some(end_line) = arguments.get("EndLine").and_then(serde_json::Value::as_u64)
                {
                    entry.1.insert(format!("L{start_line}-L{end_line}"));
                } else {
                    entry.1.insert(format!("L{start_line}"));
                }
            }
        }
    }

    /// Recognize a Markdown task marker without stripping markers from the
    /// task's own text. Completion retires an earlier pending item; a later
    /// unchecked occurrence explicitly reopens it.
    fn task_status(trimmed: &str) -> Option<(bool, &str)> {
        let mut chars = trimmed.chars();
        if !matches!(chars.next()?, '-' | '*' | '+') {
            return None;
        }
        let rest = chars.as_str();
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let rest = rest.trim_start();
        let (complete, item) = if let Some(item) = rest.strip_prefix("[ ]") {
            (false, item)
        } else if let Some(item) = rest.strip_prefix("[x]").or_else(|| rest.strip_prefix("[X]")) {
            (true, item)
        } else {
            return None;
        };
        if !item.is_empty() && !item.starts_with(char::is_whitespace) {
            return None;
        }
        Some((complete, item.trim()))
    }

    /// Examples inside fenced code are not handoff directives. Match both the
    /// delimiter and its width so an inner triple fence cannot close a longer
    /// enclosing fence, and keep unterminated code excluded through EOF.
    fn unfenced_lines(text: &str) -> impl Iterator<Item = &str> {
        let mut fence: Option<(char, usize)> = None;
        text.lines().filter(move |line| {
            let trimmed = line.trim();
            if let Some(marker @ ('`' | '~')) = trimmed.chars().next() {
                let width = trimmed.chars().take_while(|ch| *ch == marker).count();
                if let Some((opening, minimum)) = fence {
                    if marker == opening
                        && width >= minimum
                        && trimmed[width..].trim().is_empty()
                    {
                        fence = None;
                    }
                    return false;
                }
                if width >= 3 {
                    fence = Some((marker, width));
                    return false;
                }
            }
            fence.is_none()
        })
    }

    fn parse_structured_line(
        trimmed: &str,
        ref_str: &str,
        decisions: &mut Vec<Decision>,
        blockers: &mut Vec<String>,
        open_threads: &mut Vec<String>,
        next_steps: &mut Vec<String>,
        lessons: &mut Vec<String>,
    ) {
        if let Some((complete, item)) = Self::task_status(trimmed) {
            if complete {
                next_steps.retain(|pending| pending != item);
            } else if !item.is_empty() && !next_steps.iter().any(|s| s == item) {
                next_steps.push(item.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("Decision:") {
            let dec = rest.trim();
            if !dec.is_empty() {
                decisions.push(Decision {
                    decision: dec.to_string(),
                    rationale: None,
                    ref_point: Some(ref_str.to_string()),
                });
            }
        } else if let Some(rest) = trimmed.strip_prefix("Blocker:") {
            let b = rest.trim();
            if !b.is_empty() && !blockers.iter().any(|s| s == b) {
                blockers.push(b.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("Lesson:") {
            let l = rest.trim();
            if !l.is_empty() && !lessons.iter().any(|s| s == l) {
                lessons.push(l.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("Question:") {
            let q = rest.trim();
            if !q.is_empty() && !open_threads.iter().any(|s| s == q) {
                open_threads.push(q.to_string());
            }
        }
    }

    fn extract_structured_notes(
        text: &str,
        ref_str: &str,
        decisions: &mut Vec<Decision>,
        blockers: &mut Vec<String>,
        open_threads: &mut Vec<String>,
        next_steps: &mut Vec<String>,
        lessons: &mut Vec<String>,
    ) {
        for line in Self::unfenced_lines(text) {
            Self::parse_structured_line(
                line.trim(),
                ref_str,
                decisions,
                blockers,
                open_threads,
                next_steps,
                lessons,
            );
        }
    }

    fn parse_compaction_line(
        trimmed: &str,
        decisions: &mut Vec<Decision>,
        failed_approaches: &mut Vec<FailedApproach>,
        files_touched: &mut FileObservations,
        lessons: &mut Vec<String>,
    ) {
        if let Some(rest) = trimmed.strip_prefix("Decision:") {
            decisions.push(Decision {
                decision: rest.trim().to_string(),
                rationale: Some("Retained from compaction summary".to_string()),
                ref_point: Some("compaction".to_string()),
            });
        } else if let Some(rest) = trimmed.strip_prefix("Failed approach:") {
            failed_approaches.push(FailedApproach {
                attempt: "Historical attempt (compacted)".to_string(),
                reason: rest.trim().to_string(),
                ref_point: Some("compaction".to_string()),
            });
        } else if let Some(rest) = trimmed.strip_prefix("File touched:") {
            let path = rest.trim();
            if !path.is_empty() {
                files_touched
                    .entry(path.to_string())
                    .or_default()
                    .0
                    .insert("compacted session access".to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("Lesson:") {
            let lesson_text = rest.trim();
            if !lesson_text.is_empty() && !lessons.iter().any(|s| s == lesson_text) {
                lessons.push(lesson_text.to_string());
            }
        }
    }

    fn extract_from_compaction_summary(
        summary: &str,
        decisions: &mut Vec<Decision>,
        failed_approaches: &mut Vec<FailedApproach>,
        files_touched: &mut FileObservations,
        lessons: &mut Vec<String>,
    ) {
        for line in Self::unfenced_lines(summary) {
            Self::parse_compaction_line(
                line.trim(),
                decisions,
                failed_approaches,
                files_touched,
                lessons,
            );
        }
    }

    /// Deliver a generated handoff to the requested target (human/disk, bead comment, or agent thread).
    pub fn deliver(
        handoff: &HandoffDocument,
        target: &HandoffTarget,
        out_path: Option<&Path>,
    ) -> Result<HandoffDeliveryReport> {
        let markdown = handoff.to_markdown();
        let json_str = handoff.to_json()?;

        let mut report = HandoffDeliveryReport {
            target: target.clone(),
            markdown_path: None,
            json_path: None,
            status: "Generated successfully".to_string(),
            external_delivery_success: true,
        };

        // Determine destination paths
        let (md_path, js_path) = out_path.map_or_else(
            || {
                let base_name = format!("handoff_{}", handoff.session_id);
                (
                    PathBuf::from(format!("{base_name}.md")),
                    PathBuf::from(format!("{base_name}.json")),
                )
            },
            |p| {
                let md = p.to_path_buf();
                let mut js = p.to_path_buf();
                let stem = js
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                js.set_file_name(format!("{stem}.json"));
                (md, js)
            },
        );

        // Always write to disk files
        if let Err(e) = fs::write(&md_path, &markdown) {
            return Err(Error::session(format!(
                "Failed to write handoff markdown to {}: {e}",
                md_path.display()
            )));
        }
        report.markdown_path = Some(md_path.clone());

        if let Err(e) = fs::write(&js_path, &json_str) {
            return Err(Error::session(format!(
                "Failed to write handoff JSON to {}: {e}",
                js_path.display()
            )));
        }
        report.json_path = Some(js_path);

        // Perform external tool integration if requested
        match target {
            HandoffTarget::Human => {
                report.status = format!(
                    "Handoff saved to markdown ({}) and sidecar JSON",
                    md_path.display()
                );
            }
            HandoffTarget::Bead(bead_id) => {
                if bead_id.is_empty() {
                    report.status =
                        "Bead target specified without issue ID; brief saved to disk".to_string();
                    report.external_delivery_success = false;
                } else {
                    let comment_text = format!(
                        "### 📋 Handoff Brief\n\n**Goal:** {}\n**Current State:** {}\n\nFull details written to `{}`.",
                        handoff.goal,
                        handoff.current_state,
                        md_path.display()
                    );
                    let res = Command::new("br")
                        .args(["comments", "add", bead_id, &comment_text])
                        .output();

                    match res {
                        Ok(output) if output.status.success() => {
                            report.status = format!(
                                "Handoff recorded as comment on bead `{bead_id}` and saved to {}",
                                md_path.display()
                            );
                        }
                        Ok(output) => {
                            let err_msg = String::from_utf8_lossy(&output.stderr);
                            report.status = format!(
                                "Saved to {}, but `br comments add` returned code {:?}: {}",
                                md_path.display(),
                                output.status.code(),
                                err_msg.trim()
                            );
                            report.external_delivery_success = false;
                        }
                        Err(e) => {
                            report.status = format!(
                                "Saved to {}, but `br` could not be executed: {e}",
                                md_path.display()
                            );
                            report.external_delivery_success = false;
                        }
                    }
                }
            }
            HandoffTarget::Agent(thread_id) => {
                report.status = format!(
                    "Handoff formatted for Agent Mail thread `{thread_id}` and saved to {}",
                    md_path.display()
                );
            }
        }

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, ContentBlock, TextContent, ToolCall, UserContent};
    use crate::session::{CompactionEntry, EntryBase, MessageEntry, SessionEntry, SessionMessage};

    fn handoff_tool_call(entry: &str, id: &str, name: &str, path: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(None, entry.to_string()),
            message: SessionMessage::Assistant {
                message: AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: id.to_string(),
                        name: name.to_string(),
                        arguments: serde_json::json!({"path": path}),
                        thought_signature: None,
                    })],
                    ..AssistantMessage::default()
                },
            },
        })
    }

    fn handoff_tool_result(
        entry: &str,
        id: &str,
        name: &str,
        is_error: bool,
        text: &str,
    ) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(None, entry.to_string()),
            message: SessionMessage::ToolResult {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                content: vec![ContentBlock::Text(TextContent::new(text))],
                details: None,
                is_error,
                timestamp: Some(0),
            },
        })
    }

    #[test]
    fn file_roles_require_a_matching_recorded_outcome() {
        for (result, expected) in [
            (None, "write attempted (outcome unconfirmed)"),
            (Some(false), "created/overwritten"),
            (
                Some(true),
                "write attempted (tool reported failure; effects may be partial)",
            ),
        ] {
            let mut entries = vec![handoff_tool_call("call", "write-id", "write", "target.rs")];
            if let Some(is_error) = result {
                entries.push(handoff_tool_result(
                    "result",
                    "write-id",
                    "write",
                    is_error,
                    "recorded outcome",
                ));
            }
            let original = serde_json::to_value(&entries).unwrap();
            let document = HandoffGenerator::generate_from_entries("effects", &entries);
            assert_eq!(document.files_touched.len(), 1);
            assert_eq!(document.files_touched[0].role, expected);
            assert_eq!(
                document.failed_approaches.len(),
                usize::from(result == Some(true))
            );
            assert_eq!(serde_json::to_value(&entries).unwrap(), original);
        }
    }

    #[test]
    fn ambiguous_reordered_and_conflicting_outputs_cannot_prove_a_mutation() {
        let call = || handoff_tool_call("call", "id", "write", "target.rs");
        let success = || handoff_tool_result("result", "id", "write", false, "ok");
        for entries in [
            vec![success(), call()],
            vec![
                call(),
                success(),
                handoff_tool_result("duplicate", "id", "write", true, "failed"),
            ],
            vec![
                call(),
                handoff_tool_call("duplicate", "id", "write", "other.rs"),
                success(),
            ],
            vec![
                call(),
                handoff_tool_result("result", "id", "read", false, "ok"),
            ],
            vec![
                call(),
                handoff_tool_result("result", "other-id", "write", false, "ok"),
            ],
            vec![
                handoff_tool_call("call", "", "write", "target.rs"),
                handoff_tool_result("result", "", "write", false, "ok"),
            ],
            vec![
                handoff_tool_call("call", "id", " ", "target.rs"),
                handoff_tool_result("result", "id", " ", false, "ok"),
            ],
        ] {
            let document = HandoffGenerator::generate_from_entries("ambiguous", &entries);
            assert!(!document.files_touched.is_empty());
            for file in &document.files_touched {
                assert!(
                    file.role.contains("outcome unconfirmed"),
                    "{}: {}",
                    file.path,
                    file.role
                );
                assert!(!file.role.contains("created/overwritten"));
            }
        }
    }

    #[test]
    fn parallel_results_match_ids_instead_of_arrival_order() {
        let entries = [
            handoff_tool_call("write", "write-id", "write", "a.rs"),
            handoff_tool_call("edit", "edit-id", "edit", "b.rs"),
            handoff_tool_call("read", "read-id", "read", "c.rs"),
            handoff_tool_result("read-result", "read-id", "read", false, "contents"),
            handoff_tool_result("edit-result", "edit-id", "edit", true, "edit failed"),
            // An omitted foreign name can be filled from a unique matching ID.
            handoff_tool_result("write-result", "write-id", "", false, "ok"),
        ];
        let document = HandoffGenerator::generate_from_entries("parallel", &entries);
        let files = &document.files_touched;
        assert_eq!(files.len(), 3);
        assert_eq!(
            (files[0].path.as_str(), files[0].role.as_str()),
            ("a.rs", "created/overwritten")
        );
        assert_eq!(files[1].path, "b.rs");
        assert!(files[1].role.contains("tool reported failure"));
        assert_eq!(
            (files[2].path.as_str(), files[2].role.as_str()),
            ("c.rs", "read")
        );
        assert_eq!(document.failed_approaches.len(), 1);
    }

    #[test]
    fn failed_and_unfinished_attempts_do_not_erase_completed_file_effects() {
        let entries = [
            handoff_tool_call("created", "one", "write", "target.rs"),
            handoff_tool_result("created-result", "one", "write", false, "ok"),
            handoff_tool_call("failed", "two", "edit", "target.rs"),
            handoff_tool_result("failed-result", "two", "edit", true, "edit failed"),
            handoff_tool_call("unfinished", "three", "write", "target.rs"),
        ];
        let document = HandoffGenerator::generate_from_entries("mixed", &entries);
        assert_eq!(document.files_touched.len(), 1);
        assert_eq!(
            document.files_touched[0].role,
            "created/overwritten; edit attempted (tool reported failure; effects may be partial); write attempted (outcome unconfirmed)"
        );
    }

    #[test]
    fn reading_error_examples_is_not_itself_a_failed_approach() {
        let entries = [
            handoff_tool_call("read", "id", "read", "error_examples.rs"),
            handoff_tool_result(
                "result",
                "id",
                "read",
                false,
                "Error: documented example\nFAILED is a fixture literal",
            ),
        ];
        let document = HandoffGenerator::generate_from_entries("source", &entries);
        assert!(document.failed_approaches.is_empty());
        assert_eq!(document.files_touched[0].role, "read");
    }

    #[test]
    fn exported_file_observations_screen_credential_paths_and_tool_names() {
        let secret = "ghp_12345678901234567890";
        let path = format!("output/{secret}/file.rs");
        let entries = [
            handoff_tool_call("call", "id", secret, &path),
            handoff_tool_result("result", "id", secret, true, "failed"),
        ];
        let document = HandoffGenerator::generate_from_entries("private", &entries);
        for rendered in [document.to_markdown(), document.to_json().unwrap()] {
            assert!(!rendered.contains(secret));
            assert!(rendered.contains("[REDACTED_GITHUB_PAT]"));
        }
    }

    fn handoff_note_entry(id: &str, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: EntryBase::new(None, id.to_string()),
            message: SessionMessage::Assistant {
                message: AssistantMessage {
                    content: vec![ContentBlock::Text(TextContent::new(text))],
                    ..AssistantMessage::default()
                },
            },
        })
    }

    fn handoff_notes(notes: &[&str]) -> HandoffDocument {
        let entries = notes
            .iter()
            .enumerate()
            .map(|(index, text)| handoff_note_entry(&format!("note-{index}"), text))
            .collect::<Vec<_>>();
        HandoffGenerator::generate_from_entries("task-state", &entries)
    }

    #[test]
    fn completed_work_is_not_reissued_as_a_next_step() {
        let document = handoff_notes(&[
            "- [ ] Apply migration\n- [ ] Validate service\n- [ ] Apply migration",
            "- [x] Apply migration\n* [X] Validate service\n+ [x] Already completed",
        ]);
        assert!(document.next_steps.is_empty());
        assert!(!document.to_markdown().contains("- [ ] Apply migration"));
    }

    #[test]
    fn reopened_tasks_preserve_order_and_do_not_close_similarly_named_work() {
        let document = handoff_notes(&[
            "- [ ] Test\n- [ ] Test deployment\n- [x] Test",
            "+ [ ] Test\n* [ ] Test deployment\n- [ ] - [x] literal task text",
        ]);
        assert_eq!(
            document.next_steps,
            ["Test deployment", "Test", "- [x] literal task text"]
        );
    }

    #[test]
    fn task_markers_require_a_markdown_boundary_and_keep_unicode_text() {
        let document = handoff_notes(&[
            "-[ ] Not a list\n- [ ]not a task\n- [x]not a task\n- [ ]\n- [ ] 验证 🦀\n*\t[ ]\tDeploy safely",
            "- [X] 验证 🦀",
        ]);
        assert_eq!(document.next_steps, ["Deploy safely"]);
    }

    #[test]
    fn fenced_examples_cannot_add_or_complete_handoff_work() {
        let document = handoff_notes(&[
            "- [ ] Keep real work\n````markdown\n- [x] Keep real work\n```\n- [ ] Still code\n~~~~\nDecision: still code\n````\n~~~text\nBlocker: sample blocker\nQuestion: sample question\nLesson: sample lesson\n~~~\nDecision: Real decision\n- [ ] Real next step\n```unclosed\n- [ ] Incomplete example",
        ]);
        assert_eq!(document.next_steps, ["Keep real work", "Real next step"]);
        assert_eq!(document.decisions.len(), 1);
        assert_eq!(document.decisions[0].decision, "Real decision");
        assert!(document.blockers.is_empty());
        assert!(document.open_threads.is_empty());
        assert!(document.lessons.is_empty());
    }

    #[test]
    fn latest_nonempty_branch_or_assistant_state_wins_in_chronological_order() {
        let summary = |id: &str, text: &str| {
            SessionEntry::BranchSummary(crate::session::BranchSummaryEntry {
                base: EntryBase::new(None, id.to_string()),
                from_id: "other-branch".to_string(),
                summary: text.to_string(),
                details: None,
                from_hook: None,
            })
        };
        let mut entries = vec![
            summary("branch", "Old branch context"),
            handoff_note_entry("latest", "Implementation finished\nReady for verification"),
            handoff_note_entry("empty", "  \n"),
        ];
        let document = HandoffGenerator::generate_from_entries("state", &entries);
        assert_eq!(
            document.current_state,
            "Implementation finished\nReady for verification"
        );
        entries.push(summary("new-branch", "New branch context"));
        assert_eq!(
            HandoffGenerator::generate_from_entries("state", &entries).current_state,
            "New branch context"
        );
    }

    #[test]
    fn compaction_code_examples_do_not_become_historical_decisions_or_files() {
        let entries = [SessionEntry::Compaction(CompactionEntry {
            base: EntryBase::new(None, "summary".to_string()),
            summary: "```text\nDecision: sample decision\nFile touched: sample.rs\nFailed approach: sample failure\nLesson: sample lesson\n```\nDecision: actual decision\nFile touched: actual.rs".to_string(),
            first_kept_entry_id: "retained".to_string(),
            tokens_before: 100,
            details: None,
            from_hook: None,
        })];
        let document = HandoffGenerator::generate_from_entries("summary", &entries);
        assert_eq!(document.decisions.len(), 1);
        assert_eq!(document.decisions[0].decision, "actual decision");
        assert_eq!(document.files_touched.len(), 1);
        assert_eq!(document.files_touched[0].path, "actual.rs");
        assert!(document.failed_approaches.is_empty());
        assert!(document.lessons.is_empty());
    }

    fn append_handoff_branch(session: &mut Session, marker: &str) -> String {
        let user = session.append_message(SessionMessage::User {
            content: UserContent::Text(format!("Continue {marker}")),
            timestamp: Some(0),
        });
        session.append_message(SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![
                    ContentBlock::Text(TextContent::new(format!(
                        "Decision: {marker} design\nBlocker: {marker} blocker\nQuestion: {marker} question\nLesson: {marker} lesson\n- [ ] {marker} next step"
                    ))),
                    ContentBlock::ToolCall(ToolCall {
                        id: format!("call-{marker}"),
                        name: "read".to_string(),
                        arguments: serde_json::json!({"path": format!("{marker}.rs")}),
                        thought_signature: None,
                    }),
                ],
                ..AssistantMessage::default()
            },
        });
        session.append_message(SessionMessage::ToolResult {
            tool_call_id: format!("call-{marker}"),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(format!(
                "Error: {marker} read failed"
            )))],
            details: None,
            is_error: true,
            timestamp: Some(0),
        });
        session.append_compaction(
            format!("Decision: {marker} checkpoint"),
            user,
            100,
            None,
            None,
        )
    }

    #[test]
    fn session_handoff_excludes_sibling_notes_files_failures_and_checkpoints() {
        let mut session = Session::in_memory();
        let root = session.append_message(SessionMessage::User {
            content: UserContent::Text("Shared objective".to_string()),
            timestamp: Some(0),
        });
        let left = append_handoff_branch(&mut session, "left-branch");
        assert!(session.navigate_to(&root));
        let right = append_handoff_branch(&mut session, "right-branch");
        let original = serde_json::to_value(&session.entries).expect("original entries");

        for (leaf, included, excluded) in [
            (&left, "left-branch", "right-branch"),
            (&right, "right-branch", "left-branch"),
        ] {
            assert!(session.navigate_to(leaf));
            let document = HandoffGenerator::generate_from_session(&session);
            assert_eq!(document.goal, "Shared objective");
            assert_eq!(document.compaction_summaries_count, 1);
            assert_eq!(document.decisions.len(), 2);
            assert_eq!(document.failed_approaches.len(), 1);
            assert_eq!(document.files_touched.len(), 1);
            assert_eq!(document.files_touched[0].path, format!("{included}.rs"));
            assert!(document.current_state.contains(included));
            for rendered in [document.to_markdown(), document.to_json().expect("JSON")] {
                assert!(rendered.contains(included));
                assert!(
                    !rendered.contains(excluded),
                    "sibling history leaked: {rendered}"
                );
            }
            assert_eq!(serde_json::to_value(&session.entries).unwrap(), original);
        }

        // Explicit record-list callers retain their deliberately selected input.
        let all = HandoffGenerator::generate_from_entries(&session.header.id, &session.entries);
        assert_eq!(all.compaction_summaries_count, 2);
    }

    #[test]
    fn root_selection_does_not_handoff_abandoned_history() {
        let mut session = Session::in_memory();
        let abandoned = append_handoff_branch(&mut session, "abandoned-branch");
        session.reset_leaf();
        let document = HandoffGenerator::generate_from_session(&session);
        assert_eq!(document.compaction_summaries_count, 0);
        assert!(document.decisions.is_empty());
        assert!(document.failed_approaches.is_empty());
        assert!(document.files_touched.is_empty());
        assert!(document.next_steps.is_empty());
        assert!(!document.to_json().unwrap().contains("abandoned-branch"));
        assert!(
            session.get_entry(&abandoned).is_some(),
            "tree history is preserved"
        );
    }

    #[test]
    fn persisted_non_tip_selection_remains_the_handoff_branch_after_reopen() {
        let directory = tempfile::tempdir().expect("session directory");
        let path = directory.path().join("handoff.jsonl");
        let mut session = Session::create_with_dir(Some(directory.path().to_path_buf()));
        session.path = Some(path.clone());
        let root = session.append_message(SessionMessage::User {
            content: UserContent::Text("Persisted objective".to_string()),
            timestamp: Some(0),
        });
        let selected = append_handoff_branch(&mut session, "selected-branch");
        assert!(session.navigate_to(&root));
        let abandoned = append_handoff_branch(&mut session, "newer-abandoned-branch");
        assert!(session.navigate_to(&selected));

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let reopened = runtime.block_on(async {
            session.save().await.expect("save selected branch");
            Session::open(path.to_str().expect("UTF-8 fixture path"))
                .await
                .expect("reopen selected branch")
        });
        assert_eq!(reopened.leaf_id(), Some(selected.as_str()));
        assert!(reopened.get_entry(&abandoned).is_some());
        let before = fs::read(&path).expect("saved session bytes");
        let document = HandoffGenerator::generate_from_session(&reopened);
        assert_eq!(document.goal, "Persisted objective");
        let rendered = document.to_json().expect("handoff JSON");
        assert!(rendered.contains("selected-branch"));
        assert!(!rendered.contains("newer-abandoned-branch"));
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "handoff must be read-only"
        );
    }

    #[test]
    fn test_handoff_target_parsing() {
        assert_eq!(HandoffTarget::parse("human"), HandoffTarget::Human);
        assert_eq!(
            HandoffTarget::parse("bead:bd-1234"),
            HandoffTarget::Bead("bd-1234".to_string())
        );
        assert_eq!(
            HandoffTarget::parse("bead=bd-5678"),
            HandoffTarget::Bead("bd-5678".to_string())
        );
        assert_eq!(
            HandoffTarget::parse("agent:thread-abc"),
            HandoffTarget::Agent("thread-abc".to_string())
        );
    }

    #[test]
    fn test_secret_redaction_in_handoff() {
        let entries = vec![
            SessionEntry::Message(MessageEntry {
                base: EntryBase::new(None, "e1".to_string()),
                message: SessionMessage::User {
                    content: UserContent::Text(
                        "Investigate leak of sk-ant-api03-secretkey1234567890 in config"
                            .to_string(),
                    ),
                    timestamp: None,
                },
            }),
            SessionEntry::Message(MessageEntry {
                base: EntryBase::new(Some("e1".to_string()), "e2".to_string()),
                message: SessionMessage::Assistant {
                    message: AssistantMessage {
                        content: vec![ContentBlock::Text(TextContent {
                            text: "Decision: Rotate the key ghp_12345678901234567890 immediately"
                                .to_string(),
                            text_signature: None,
                        })],
                        provider: "test".to_string(),
                        model: "test".to_string(),
                        ..Default::default()
                    },
                },
            }),
        ];

        let doc = HandoffGenerator::generate_from_entries("sess-test-secrets", &entries);
        assert!(!doc.goal.contains("sk-ant-api03"));
        assert!(doc.goal.contains("[REDACTED_ANTHROPIC_KEY]"));

        assert_eq!(doc.decisions.len(), 1);
        assert!(!doc.decisions[0].decision.contains("ghp_"));
        assert!(doc.decisions[0].decision.contains("[REDACTED_GITHUB_PAT]"));
    }

    #[test]
    fn test_failed_approaches_and_tool_results() {
        let entries = vec![
            SessionEntry::Message(MessageEntry {
                base: EntryBase::new(None, "e1".to_string()),
                message: SessionMessage::User {
                    content: UserContent::Text("Build and test the project".to_string()),
                    timestamp: None,
                },
            }),
            SessionEntry::Message(MessageEntry {
                base: EntryBase::new(Some("e1".to_string()), "e2".to_string()),
                message: SessionMessage::Assistant {
                    message: AssistantMessage {
                        content: vec![ContentBlock::ToolCall(ToolCall {
                            id: "tc1".to_string(),
                            name: "write".to_string(),
                            arguments: serde_json::json!({
                                "path": "src/main.rs",
                                "StartLine": 1,
                                "EndLine": 10
                            }),
                            thought_signature: None,
                        })],
                        provider: "test".to_string(),
                        model: "test".to_string(),
                        ..Default::default()
                    },
                },
            }),
            SessionEntry::Message(MessageEntry {
                base: EntryBase::new(Some("e2".to_string()), "e3".to_string()),
                message: SessionMessage::ToolResult {
                    tool_call_id: "tc1".to_string(),
                    tool_name: "write".to_string(),
                    content: vec![ContentBlock::Text(TextContent {
                        text: "Error: Permission denied writing to src/main.rs".to_string(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: true,
                    timestamp: None,
                },
            }),
            SessionEntry::Message(MessageEntry {
                base: EntryBase::new(Some("e3".to_string()), "e4".to_string()),
                message: SessionMessage::BashExecution {
                    command: "cargo check".to_string(),
                    output: "error[E0432]: unresolved import `foo`".to_string(),
                    exit_code: 101,
                    cancelled: None,
                    truncated: None,
                    full_output_path: None,
                    timestamp: None,
                    extra: std::collections::HashMap::new(),
                },
            }),
        ];

        let doc = HandoffGenerator::generate_from_entries("sess-failure-mem", &entries);

        // Check files touched
        assert_eq!(doc.files_touched.len(), 1);
        assert_eq!(doc.files_touched[0].path, "src/main.rs");
        assert_eq!(
            doc.files_touched[0].role,
            "write attempted (tool reported failure; effects may be partial)"
        );
        assert!(
            doc.files_touched[0]
                .line_refs
                .contains(&"L1-L10".to_string())
        );

        // Check failed approaches
        assert_eq!(doc.failed_approaches.len(), 2);
        assert!(doc.failed_approaches[0].attempt.contains("write"));
        assert!(
            doc.failed_approaches[0]
                .reason
                .contains("Permission denied")
        );

        assert!(doc.failed_approaches[1].attempt.contains("cargo check"));
        assert!(
            doc.failed_approaches[1]
                .reason
                .contains("unresolved import")
        );
    }

    #[test]
    fn test_compaction_summary_integration() {
        let entries = vec![
            SessionEntry::Compaction(CompactionEntry {
                base: EntryBase::new(None, "c1".to_string()),
                summary: "Compacted Session Summary:\nDecision: Adopted fsqlite for concurrency\nFailed approach: Tried pure mutex, ran into lock contention\nFile touched: src/db.rs\nLesson: Lock-free queues are preferred".to_string(),
                first_kept_entry_id: "e10".to_string(),
                tokens_before: 50000,
                details: None,
                from_hook: None,
            }),
            SessionEntry::Message(MessageEntry {
                base: EntryBase::new(Some("c1".to_string()), "e10".to_string()),
                message: SessionMessage::User {
                    content: UserContent::Text("Now implement the remaining cache layer".to_string()),
                    timestamp: None,
                },
            }),
        ];

        let doc = HandoffGenerator::generate_from_entries("sess-compacted", &entries);
        assert_eq!(doc.compaction_summaries_count, 1);
        assert!(doc.decisions.iter().any(|d| d.decision.contains("fsqlite")));
        assert!(
            doc.failed_approaches
                .iter()
                .any(|f| f.reason.contains("lock contention"))
        );
        assert!(doc.lessons.iter().any(|l| l.contains("Lock-free")));
        assert!(doc.files_touched.iter().any(|f| f.path == "src/db.rs"));
    }

    #[test]
    fn test_markdown_and_json_roundtrip() {
        let doc = HandoffDocument {
            schema: HANDOFF_SCHEMA_V1.to_string(),
            session_id: "sess-roundtrip-123".to_string(),
            timestamp: "2026-08-21T12:00:00Z".to_string(),
            goal: "Refactor provider streaming".to_string(),
            current_state: "Provider streaming refactored and passing tests".to_string(),
            decisions: vec![Decision {
                decision: "Use zero-copy buffer slices".to_string(),
                rationale: Some("Reduces memory allocations".to_string()),
                ref_point: Some("turn#4".to_string()),
            }],
            failed_approaches: vec![FailedApproach {
                attempt: "Direct string cloning".to_string(),
                reason: "High CPU overhead".to_string(),
                ref_point: Some("turn#2".to_string()),
            }],
            files_touched: vec![FileTouched {
                path: "src/provider.rs".to_string(),
                role: "modified".to_string(),
                line_refs: vec!["L40-L90".to_string()],
            }],
            blockers: vec![],
            open_threads: vec!["Evaluate on ARM64 CI lane".to_string()],
            next_steps: vec!["Merge PR to main".to_string()],
            lessons: vec!["Always profile buffer allocations".to_string()],
            compaction_summaries_count: 0,
        };

        let md = doc.to_markdown();
        assert!(md.contains("# Session Handoff Brief"));
        assert!(md.contains("sess-roundtrip-123"));
        assert!(md.contains("Refactor provider streaming"));
        assert!(md.contains("Direct string cloning"));
        assert!(md.contains("High CPU overhead"));
        assert!(md.contains("src/provider.rs"));

        let Ok(json_str) = doc.to_json() else {
            return;
        };
        let Ok(parsed) = serde_json::from_str::<HandoffDocument>(&json_str) else {
            return;
        };
        assert_eq!(doc, parsed);
    }
}
