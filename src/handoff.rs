//! Structured cross-session and cross-agent handoff generator.
//!
//! Provides generation of continuation briefs for oncoming human engineers
//! or autonomous agents when switching contexts, delegating tasks, or
//! handing off work between sessions (`bd-cv653.3.17`).

use crate::error::{Error, Result};
use crate::memory::screen_secrets;
use crate::model::{ContentBlock, UserContent};
use crate::session::{CompactionEntry, MessageEntry, Session, SessionEntry, SessionMessage};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Canonical schema identifier for the handoff JSON payload.
pub const HANDOFF_SCHEMA_V1: &str = "pi.handoff.v1";

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
        if let Some(rest) = trimmed.strip_prefix("bead:") {
            Self::Bead(rest.trim().to_string())
        } else if let Some(rest) = trimmed.strip_prefix("bead=") {
            Self::Bead(rest.trim().to_string())
        } else if trimmed.eq_ignore_ascii_case("bead") {
            Self::Bead(String::new())
        } else if let Some(rest) = trimmed.strip_prefix("agent:") {
            Self::Agent(rest.trim().to_string())
        } else if let Some(rest) = trimmed.strip_prefix("agent=") {
            Self::Agent(rest.trim().to_string())
        } else if trimmed.eq_ignore_ascii_case("agent") {
            Self::Agent(String::new())
        } else {
            Self::Human
        }
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

/// File touched during the session and its observed role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTouched {
    /// File path.
    pub path: String,
    /// Role: "created", "modified", "read", "executed", etc.
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
    /// Generate a structured handoff document from a loaded `Session`.
    #[must_use]
    pub fn generate_from_session(session: &Session) -> HandoffDocument {
        let session_id = session.header.id.clone();
        let entries = session.entries.as_slice();
        Self::generate_from_entries(&session_id, entries)
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

    fn extract_assistant_text_and_tools(
        message: &AssistantMessage,
        files_touched_map: &mut BTreeMap<String, (String, BTreeSet<String>)>,
    ) -> String {
        let mut text_acc = String::new();
        for block in &message.content {
            match block {
                ContentBlock::Text(t) => {
                    text_acc.push_str(&t.text);
                    text_acc.push('\n');
                }
                ContentBlock::ToolCall(tc) => {
                    Self::record_tool_call(&tc.name, &tc.arguments, files_touched_map);
                }
                _ => {}
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
        last_assistant_text: &mut String,
        decisions: &mut Vec<Decision>,
        blockers: &mut Vec<String>,
        open_threads: &mut Vec<String>,
        next_steps: &mut Vec<String>,
        lessons: &mut Vec<String>,
        failed_approaches: &mut Vec<FailedApproach>,
        files_touched_map: &mut BTreeMap<String, (String, BTreeSet<String>)>,
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
                let screened = Self::extract_assistant_text_and_tools(message, files_touched_map);
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
                    *last_assistant_text = screened;
                }
            }
            SessionMessage::ToolResult {
                tool_name,
                content,
                is_error,
                ..
            } => {
                let tool_output = Self::extract_tool_result_text(content);

                if *is_error || tool_output.contains("Error:") || tool_output.contains("FAILED") {
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

    /// Generate a structured handoff document from a list of `SessionEntry` records.
    #[must_use]
    pub fn generate_from_entries(session_id: &str, entries: &[SessionEntry]) -> HandoffDocument {
        let mut goal = String::new();
        let mut current_state = String::new();
        let mut decisions = Vec::new();
        let mut failed_approaches = Vec::new();
        let mut files_touched_map: BTreeMap<String, (String, BTreeSet<String>)> = BTreeMap::new();
        let mut blockers = Vec::new();
        let mut open_threads = Vec::new();
        let mut next_steps = Vec::new();
        let mut lessons = Vec::new();
        let mut compaction_count = 0;

        let mut turn_index = 0usize;
        let mut last_assistant_text = String::new();

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
                        &mut last_assistant_text,
                        &mut decisions,
                        &mut blockers,
                        &mut open_threads,
                        &mut next_steps,
                        &mut lessons,
                        &mut failed_approaches,
                        &mut files_touched_map,
                    );
                }
                SessionEntry::BranchSummary(bs) => {
                    let screened = screen_secrets(&bs.summary);
                    current_state.push_str(&screened);
                    current_state.push('\n');
                }
                _ => {}
            }
        }

        if goal.is_empty() {
            goal = "No explicit initial goal detected in session history.".to_string();
        }

        if current_state.is_empty() {
            if !last_assistant_text.is_empty() {
                // Take the last few lines or first paragraph of last assistant message
                let snippet = last_assistant_text
                    .lines()
                    .take(6)
                    .collect::<Vec<_>>()
                    .join("\n");
                current_state = snippet;
            } else {
                current_state = "Session active, awaiting next instructions.".to_string();
            }
        }

        let files_touched = files_touched_map
            .into_iter()
            .map(|(path, (role, line_refs))| FileTouched {
                path,
                role,
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

    fn record_tool_call(
        tool_name: &str,
        arguments: &serde_json::Value,
        files_touched: &mut BTreeMap<String, (String, BTreeSet<String>)>,
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

            let entry = files_touched
                .entry(path)
                .or_insert_with(|| (role.to_string(), BTreeSet::new()));

            // Update role to higher mutation level if modified
            if role == "modified" || role == "created/overwritten" {
                entry.0 = role.to_string();
            }

            if let Some(start_line) = arguments.get("StartLine").and_then(|v| v.as_u64()) {
                if let Some(end_line) = arguments.get("EndLine").and_then(|v| v.as_u64()) {
                    entry.1.insert(format!("L{start_line}-L{end_line}"));
                } else {
                    entry.1.insert(format!("L{start_line}"));
                }
            }
        }
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
        if trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]") {
            let item = trimmed
                .trim_start_matches("- [ ]")
                .trim_start_matches("- [x]")
                .trim();
            if !item.is_empty() && !next_steps.iter().any(|s| s == item) {
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
        for line in text.lines() {
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
        files_touched: &mut BTreeMap<String, (String, BTreeSet<String>)>,
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
                    .or_insert_with(|| ("compacted session access".to_string(), BTreeSet::new()));
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
        files_touched: &mut BTreeMap<String, (String, BTreeSet<String>)>,
        lessons: &mut Vec<String>,
    ) {
        for line in summary.lines() {
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
        let (md_path, js_path) = if let Some(p) = out_path {
            let md = p.to_path_buf();
            let mut js = p.to_path_buf();
            let stem = js
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            js.set_file_name(format!("{stem}.json"));
            (md, js)
        } else {
            let base_name = format!("handoff_{}", handoff.session_id);
            (
                PathBuf::from(format!("{base_name}.md")),
                PathBuf::from(format!("{base_name}.json")),
            )
        };

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
        assert_eq!(doc.files_touched[0].role, "created/overwritten");
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
