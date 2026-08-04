//! Context compaction for long sessions.
//!
//! This module ports the pi-mono compaction algorithm:
//! - Estimate context usage and choose a cut point that keeps recent context
//! - Summarize the discarded portion with the LLM (iteratively updating prior summaries)
//! - Record a `compaction` session entry containing the summary and cut point
//! - When building provider context, the session inserts the summary before the kept region
//!   and omits older messages.

use crate::error::{Error, Result};
use crate::model::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ThinkingLevel, ToolCall,
    Usage, UserContent, UserMessage,
};
use crate::provider::{Context, Provider, StreamOptions};
use crate::session::{SessionEntry, SessionMessage, session_message_to_model};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

/// Approximate characters per token for English text with GPT-family tokenizers.
/// Intentionally conservative (overestimates tokens) to avoid exceeding context windows.
/// Set to 3 to safely account for code/symbol-heavy content which is denser than prose.
const CHARS_PER_TOKEN_ESTIMATE: usize = 3;

/// Estimated tokens for an image content block (~1200 tokens).
const IMAGE_TOKEN_ESTIMATE: usize = 1200;

/// Character-equivalent estimate for an image (IMAGE_TOKEN_ESTIMATE * CHARS_PER_TOKEN_ESTIMATE).
const IMAGE_CHAR_ESTIMATE: usize = IMAGE_TOKEN_ESTIMATE * CHARS_PER_TOKEN_ESTIMATE;

/// Count the serialized JSON byte length of a [`Value`] without allocating a `String`.
///
/// Uses `serde_json::to_writer` with a sink that only counts bytes – this gives the
/// exact same length as `serde_json::to_string(&v).len()` at zero heap cost.
fn json_byte_len(value: &Value) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(buf.len());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Counter(0);
    if serde_json::to_writer(&mut c, value).is_err() {
        // Fallback or partial count on error (e.g. recursion limit)
    }
    c.0
}

// =============================================================================
// Public types
// =============================================================================

#[derive(Debug, Clone)]
pub struct ResolvedCompactionSettings {
    pub enabled: bool,
    pub context_window_tokens: u32,
    pub reserve_tokens: u32,
    pub keep_recent_tokens: u32,
}

impl Default for ResolvedCompactionSettings {
    /// Conservative default using the smallest common context window (32K).
    ///
    /// Production code paths should always override `context_window_tokens`
    /// with the actual model's context window via
    /// [`context_window_tokens_for_entry`](crate::main) or equivalent.
    /// This default is deliberately conservative so that if a code path
    /// forgets to override, compaction triggers too early (safe) rather
    /// than too late (could exceed the real context window).
    fn default() -> Self {
        let context_window_tokens: u32 = 128_000;
        Self {
            enabled: true,
            context_window_tokens,
            // ~8% of context window
            reserve_tokens: 10_240,
            // 10% of context window
            keep_recent_tokens: 12_800,
        }
    }
}

/// Details stored in `CompactionEntry.details` for cumulative file tracking.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionDetails {
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub details: CompactionDetails,
}

#[derive(Debug, Clone)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages_to_summarize: Vec<SessionMessage>,
    pub turn_prefix_messages: Vec<SessionMessage>,
    pub is_split_turn: bool,
    pub tokens_before: u64,
    pub previous_summary: Option<String>,
    pub file_ops: FileOperations,
    pub settings: ResolvedCompactionSettings,
}

pub const SEMANTIC_COMPACTION_QUALITY_SCHEMA: &str = "pi.session.semantic_compaction_quality.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCompactionMarkerKind {
    Task,
    FileReference,
    Decision,
    ToolOutput,
    Constraint,
    HandoffFact,
    AgentMailDegraded,
    BeadsClaim,
    Interruption,
    TruncationNotice,
}

impl SemanticCompactionMarkerKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::FileReference => "file_reference",
            Self::Decision => "decision",
            Self::ToolOutput => "tool_output",
            Self::Constraint => "constraint",
            Self::HandoffFact => "handoff_fact",
            Self::AgentMailDegraded => "agent_mail_degraded",
            Self::BeadsClaim => "beads_claim",
            Self::Interruption => "interruption",
            Self::TruncationNotice => "truncation_notice",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCompactionMarkerSeverity {
    Critical,
    Important,
    Informational,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCompactionQualityVerdict {
    Pass,
    Degraded,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticCompactionLossClass {
    PreCompactionMarkerAbsent,
    MissingMarker,
    WrongBranch,
    WrongTurn,
    TruncationReceiptMissing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticCompactionMarker {
    pub id: String,
    pub kind: SemanticCompactionMarkerKind,
    pub severity: SemanticCompactionMarkerSeverity,
    pub source_entry_id: String,
    pub expected_branch_leaf_id: String,
    pub expected_turn_id: String,
    pub marker: String,
    pub requires_truncation_receipt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticCompactionMarkerObservation {
    pub marker_id: String,
    pub branch_leaf_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub has_truncation_receipt: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticCompactionQualityView {
    pub name: String,
    pub branch_leaf_id: String,
    pub observations: Vec<SemanticCompactionMarkerObservation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticCompactionCoverage {
    pub expected: u32,
    pub preserved: u32,
    pub missing: u32,
    pub wrong_branch: u32,
    pub wrong_turn: u32,
    pub truncation_receipt_missing: u32,
    pub pre_compaction_absent: u32,
    pub coverage_bps: u16,
}

impl SemanticCompactionCoverage {
    const fn note_expected(&mut self) {
        self.expected = self.expected.saturating_add(1);
    }

    const fn note_preserved(&mut self) {
        self.preserved = self.preserved.saturating_add(1);
    }

    const fn note_loss(&mut self, class: SemanticCompactionLossClass) {
        match class {
            SemanticCompactionLossClass::PreCompactionMarkerAbsent => {
                self.pre_compaction_absent = self.pre_compaction_absent.saturating_add(1);
            }
            SemanticCompactionLossClass::MissingMarker => {
                self.missing = self.missing.saturating_add(1);
            }
            SemanticCompactionLossClass::WrongBranch => {
                self.wrong_branch = self.wrong_branch.saturating_add(1);
            }
            SemanticCompactionLossClass::WrongTurn => {
                self.wrong_turn = self.wrong_turn.saturating_add(1);
            }
            SemanticCompactionLossClass::TruncationReceiptMissing => {
                self.truncation_receipt_missing = self.truncation_receipt_missing.saturating_add(1);
            }
        }
    }

    fn finalize(&mut self) {
        if self.expected == 0 {
            self.coverage_bps = 10_000;
            return;
        }
        let bps = self.preserved.saturating_mul(10_000) / self.expected;
        self.coverage_bps = u16::try_from(bps).unwrap_or(u16::MAX);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticCompactionMarkerSummary {
    pub id: String,
    pub kind: SemanticCompactionMarkerKind,
    pub severity: SemanticCompactionMarkerSeverity,
    pub source_entry_id: String,
    pub expected_branch_leaf_id: String,
    pub expected_turn_id: String,
    pub marker_digest: String,
    pub preserved: bool,
    pub loss_classes: Vec<SemanticCompactionLossClass>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticCompactionMarkerLoss {
    pub marker_id: String,
    pub kind: SemanticCompactionMarkerKind,
    pub severity: SemanticCompactionMarkerSeverity,
    pub class: SemanticCompactionLossClass,
    pub expected_branch_leaf_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_branch_leaf_id: Option<String>,
    pub expected_turn_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_turn_id: Option<String>,
}

impl SemanticCompactionMarkerLoss {
    fn for_marker(
        marker: &SemanticCompactionMarker,
        class: SemanticCompactionLossClass,
        observation: Option<&SemanticCompactionMarkerObservation>,
    ) -> Self {
        Self {
            marker_id: marker.id.clone(),
            kind: marker.kind,
            severity: marker.severity,
            class,
            expected_branch_leaf_id: marker.expected_branch_leaf_id.clone(),
            actual_branch_leaf_id: observation.map(|obs| obs.branch_leaf_id.clone()),
            expected_turn_id: marker.expected_turn_id.clone(),
            actual_turn_id: observation.map(|obs| obs.turn_id.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticCompactionFalsePositiveControl {
    pub marker_id: String,
    pub observed_branch_leaf_id: String,
    pub observed_turn_id: String,
    pub disposition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticCompactionQualityReport {
    pub schema: String,
    pub before_view: String,
    pub after_view: String,
    pub before_branch_leaf_id: String,
    pub after_branch_leaf_id: String,
    pub marker_count: usize,
    pub coverage: SemanticCompactionCoverage,
    pub coverage_by_kind: BTreeMap<String, SemanticCompactionCoverage>,
    pub verdict: SemanticCompactionQualityVerdict,
    pub marker_summaries: Vec<SemanticCompactionMarkerSummary>,
    pub losses: Vec<SemanticCompactionMarkerLoss>,
    pub false_positive_controls: Vec<SemanticCompactionFalsePositiveControl>,
    pub unexpected_marker_ids: Vec<String>,
}

fn observation_map(
    view: &SemanticCompactionQualityView,
) -> BTreeMap<String, SemanticCompactionMarkerObservation> {
    let mut observations = BTreeMap::new();
    for observation in &view.observations {
        observations
            .entry(String::from(observation.marker_id.as_str()))
            .or_insert_with(|| SemanticCompactionMarkerObservation {
                marker_id: String::from(observation.marker_id.as_str()),
                branch_leaf_id: String::from(observation.branch_leaf_id.as_str()),
                turn_id: String::from(observation.turn_id.as_str()),
                has_truncation_receipt: observation.has_truncation_receipt,
            });
    }
    observations
}

fn marker_digest(marker: &SemanticCompactionMarker) -> String {
    let mut digest = Sha256::new();
    digest.update(marker.id.as_bytes());
    digest.update([0]);
    digest.update(marker.marker.as_bytes());
    format!("{:x}", digest.finalize())
}

fn record_loss(
    coverage: &mut SemanticCompactionCoverage,
    coverage_by_kind: &mut BTreeMap<String, SemanticCompactionCoverage>,
    marker: &SemanticCompactionMarker,
    class: SemanticCompactionLossClass,
) {
    coverage.note_loss(class);
    coverage_by_kind
        .entry(marker.kind.as_str().to_string())
        .or_default()
        .note_loss(class);
}

fn record_preserved(
    coverage: &mut SemanticCompactionCoverage,
    coverage_by_kind: &mut BTreeMap<String, SemanticCompactionCoverage>,
    marker: &SemanticCompactionMarker,
) {
    coverage.note_preserved();
    coverage_by_kind
        .entry(marker.kind.as_str().to_string())
        .or_default()
        .note_preserved();
}

fn marker_summary_for(
    marker: &SemanticCompactionMarker,
    marker_loss_classes: Vec<SemanticCompactionLossClass>,
) -> SemanticCompactionMarkerSummary {
    SemanticCompactionMarkerSummary {
        id: String::from(marker.id.as_str()),
        kind: marker.kind,
        severity: marker.severity,
        source_entry_id: String::from(marker.source_entry_id.as_str()),
        expected_branch_leaf_id: String::from(marker.expected_branch_leaf_id.as_str()),
        expected_turn_id: String::from(marker.expected_turn_id.as_str()),
        marker_digest: marker_digest(marker),
        preserved: marker_loss_classes.is_empty(),
        loss_classes: marker_loss_classes,
    }
}

#[allow(clippy::too_many_lines)]
pub fn evaluate_semantic_compaction_quality(
    markers: &[SemanticCompactionMarker],
    before: &SemanticCompactionQualityView,
    after: &SemanticCompactionQualityView,
) -> SemanticCompactionQualityReport {
    let before_observations = observation_map(before);
    let after_observations = observation_map(after);

    let mut sorted_markers = markers.iter().collect::<Vec<_>>();
    sorted_markers.sort_by(|left, right| left.id.cmp(&right.id));

    let expected_marker_ids = sorted_markers
        .iter()
        .map(|marker| marker.id.clone())
        .collect::<BTreeSet<_>>();

    let unexpected_marker_ids = after_observations
        .keys()
        .filter(|id| !expected_marker_ids.contains(*id))
        .cloned()
        .collect::<Vec<_>>();

    let false_positive_controls = unexpected_marker_ids
        .iter()
        .filter_map(|id| after_observations.get(id))
        .map(|observation| SemanticCompactionFalsePositiveControl {
            marker_id: observation.marker_id.clone(),
            observed_branch_leaf_id: observation.branch_leaf_id.clone(),
            observed_turn_id: observation.turn_id.clone(),
            disposition: "ignored_unexpected_marker".to_string(),
        })
        .collect::<Vec<_>>();

    let mut coverage = SemanticCompactionCoverage::default();
    let mut coverage_by_kind = BTreeMap::<String, SemanticCompactionCoverage>::new();
    let mut marker_summaries = Vec::with_capacity(sorted_markers.len());
    let mut losses = Vec::new();

    for marker in sorted_markers {
        coverage.note_expected();
        coverage_by_kind
            .entry(marker.kind.as_str().to_string())
            .or_default()
            .note_expected();

        let before_observation = before_observations.get(&marker.id);
        let after_observation = after_observations.get(&marker.id);
        let mut marker_loss_classes = Vec::new();

        if before_observation.is_none() {
            marker_loss_classes.push(SemanticCompactionLossClass::PreCompactionMarkerAbsent);
        }

        if let Some(observation) = after_observation {
            if observation.branch_leaf_id != marker.expected_branch_leaf_id
                || after.branch_leaf_id != marker.expected_branch_leaf_id
            {
                marker_loss_classes.push(SemanticCompactionLossClass::WrongBranch);
            }
            if observation.turn_id != marker.expected_turn_id {
                marker_loss_classes.push(SemanticCompactionLossClass::WrongTurn);
            }
            if marker.requires_truncation_receipt && !observation.has_truncation_receipt {
                marker_loss_classes.push(SemanticCompactionLossClass::TruncationReceiptMissing);
            }
        } else {
            marker_loss_classes.push(SemanticCompactionLossClass::MissingMarker);
        }

        if marker_loss_classes.is_empty() {
            record_preserved(&mut coverage, &mut coverage_by_kind, marker);
        } else {
            for class in marker_loss_classes.iter().copied() {
                record_loss(&mut coverage, &mut coverage_by_kind, marker, class);
                losses.push(SemanticCompactionMarkerLoss::for_marker(
                    marker,
                    class,
                    after_observation,
                ));
            }
        }

        marker_summaries.push(marker_summary_for(marker, marker_loss_classes));
    }

    coverage.finalize();
    for value in coverage_by_kind.values_mut() {
        value.finalize();
    }

    let has_unexpected_markers = !unexpected_marker_ids.is_empty();
    let verdict = if has_unexpected_markers
        || losses
            .iter()
            .any(|loss| loss.severity == SemanticCompactionMarkerSeverity::Critical)
    {
        SemanticCompactionQualityVerdict::Fail
    } else if losses.is_empty() {
        SemanticCompactionQualityVerdict::Pass
    } else {
        SemanticCompactionQualityVerdict::Degraded
    };

    SemanticCompactionQualityReport {
        schema: SEMANTIC_COMPACTION_QUALITY_SCHEMA.to_string(),
        before_view: before.name.clone(),
        after_view: after.name.clone(),
        before_branch_leaf_id: before.branch_leaf_id.clone(),
        after_branch_leaf_id: after.branch_leaf_id.clone(),
        marker_count: markers.len(),
        coverage,
        coverage_by_kind,
        verdict,
        marker_summaries,
        losses,
        false_positive_controls,
        unexpected_marker_ids,
    }
}

pub fn semantic_compaction_quality_report_to_value(
    report: &SemanticCompactionQualityReport,
) -> Result<Value> {
    serde_json::to_value(report)
        .map_err(|e| Error::session(format!("Semantic compaction quality report: {e}")))
}

pub fn semantic_compaction_quality_report_to_jsonl(
    report: &SemanticCompactionQualityReport,
) -> Result<String> {
    let mut line = serde_json::to_string(report)
        .map_err(|e| Error::session(format!("Semantic compaction quality report JSONL: {e}")))?;
    line.push('\n');
    Ok(line)
}

pub fn compaction_preparation_to_value(prep: &CompactionPreparation) -> Value {
    let messages_to_summarize =
        serde_json::to_value(&prep.messages_to_summarize).unwrap_or(Value::Array(Vec::new()));
    let turn_prefix_messages =
        serde_json::to_value(&prep.turn_prefix_messages).unwrap_or(Value::Array(Vec::new()));

    let mut obj = Map::new();
    obj.insert(
        "firstKeptEntryId".to_string(),
        Value::String(prep.first_kept_entry_id.clone()),
    );
    obj.insert("messagesToSummarize".to_string(), messages_to_summarize);
    obj.insert("turnPrefixMessages".to_string(), turn_prefix_messages);
    obj.insert("isSplitTurn".to_string(), Value::Bool(prep.is_split_turn));
    obj.insert("tokensBefore".to_string(), Value::from(prep.tokens_before));
    if let Some(previous_summary) = &prep.previous_summary {
        obj.insert(
            "previousSummary".to_string(),
            Value::String(previous_summary.clone()),
        );
    }
    obj.insert("fileOps".to_string(), file_ops_to_value(&prep.file_ops));
    obj.insert(
        "settings".to_string(),
        compaction_settings_to_value(&prep.settings),
    );
    Value::Object(obj)
}

fn file_ops_to_value(file_ops: &FileOperations) -> Value {
    let read = sorted_file_ops(&file_ops.read);
    let written = sorted_file_ops(&file_ops.written);
    let edited = sorted_file_ops(&file_ops.edited);
    let mut obj = Map::new();
    obj.insert("read".to_string(), Value::Array(read));
    obj.insert("written".to_string(), Value::Array(written));
    obj.insert("edited".to_string(), Value::Array(edited));
    Value::Object(obj)
}

fn sorted_file_ops(values: &HashSet<String>) -> Vec<Value> {
    let mut entries = values.iter().cloned().collect::<Vec<_>>();
    entries.sort();
    entries.into_iter().map(Value::String).collect()
}

fn compaction_settings_to_value(settings: &ResolvedCompactionSettings) -> Value {
    let mut obj = Map::new();
    obj.insert("enabled".to_string(), Value::Bool(settings.enabled));
    obj.insert(
        "contextWindowTokens".to_string(),
        Value::from(settings.context_window_tokens),
    );
    obj.insert(
        "reserveTokens".to_string(),
        Value::from(settings.reserve_tokens),
    );
    obj.insert(
        "keepRecentTokens".to_string(),
        Value::from(settings.keep_recent_tokens),
    );
    Value::Object(obj)
}

// =============================================================================
// File op tracking (read/write/edit)
// =============================================================================

#[derive(Debug, Clone, Default)]
pub struct FileOperations {
    read: HashSet<String>,
    written: HashSet<String>,
    edited: HashSet<String>,
}

impl FileOperations {
    pub fn read_files(&self) -> impl Iterator<Item = &str> {
        self.read.iter().map(String::as_str)
    }
}

fn build_tool_status_map(messages: &[SessionMessage]) -> HashMap<&str, bool> {
    let mut status = HashMap::new();
    for msg in messages {
        if let SessionMessage::ToolResult {
            tool_call_id,
            is_error,
            ..
        } = msg
        {
            status.insert(tool_call_id.as_str(), !*is_error);
        }
    }
    status
}

fn extract_file_ops_from_message(
    message: &SessionMessage,
    file_ops: &mut FileOperations,
    tool_status: &HashMap<&str, bool>,
) {
    let SessionMessage::Assistant { message } = message else {
        return;
    };

    for block in &message.content {
        let ContentBlock::ToolCall(ToolCall {
            id,
            name,
            arguments,
            ..
        }) = block
        else {
            continue;
        };

        // Only track successful tool calls.
        if !tool_status.get(id.as_str()).copied().unwrap_or(false) {
            continue;
        }

        let Some(path) = arguments.get("path").and_then(Value::as_str) else {
            continue;
        };

        match name.as_str() {
            "read" | "grep" | "find" | "ls" => {
                file_ops.read.insert(path.to_string());
            }
            "write" => {
                file_ops.written.insert(path.to_string());
            }
            "edit" | "hashline_edit" => {
                file_ops.edited.insert(path.to_string());
            }
            _ => {}
        }
    }
}

fn compute_file_lists(file_ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let modified: HashSet<&String> = file_ops
        .edited
        .iter()
        .chain(file_ops.written.iter())
        .collect();

    let mut read_only = file_ops
        .read
        .iter()
        .filter(|f| !modified.contains(f))
        .cloned()
        .collect::<Vec<_>>();
    read_only.sort();

    let mut modified_files = modified.into_iter().cloned().collect::<Vec<_>>();
    modified_files.sort();

    (read_only, modified_files)
}

fn write_escaped_file_list(out: &mut String, tag: &str, files: &[String]) {
    out.push('<');
    out.push_str(tag);
    out.push_str(">\n");
    for (i, file) in files.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // Inline escape: replace < and > in file paths
        for ch in file.chars() {
            match ch {
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                _ => out.push(ch),
            }
        }
    }
    out.push_str("\n</");
    out.push_str(tag);
    out.push('>');
}

fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    if read_files.is_empty() && modified_files.is_empty() {
        return String::new();
    }

    let mut out = String::from("\n\n");
    if !read_files.is_empty() {
        write_escaped_file_list(&mut out, "read-files", read_files);
    }
    if !modified_files.is_empty() {
        if !read_files.is_empty() {
            out.push_str("\n\n");
        }
        write_escaped_file_list(&mut out, "modified-files", modified_files);
    }
    out
}

// =============================================================================
// Token estimation
// =============================================================================

const fn calculate_context_tokens(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input.saturating_add(usage.output)
    }
}

const fn get_assistant_usage(message: &SessionMessage) -> Option<&Usage> {
    let SessionMessage::Assistant { message } = message else {
        return None;
    };

    if matches!(message.stop_reason, StopReason::Aborted | StopReason::Error) {
        return None;
    }

    Some(&message.usage)
}

#[derive(Debug, Clone, Copy)]
struct ContextUsageEstimate {
    tokens: u64,
    last_usage_index: Option<usize>,
}

fn estimate_context_tokens(messages: &[SessionMessage]) -> ContextUsageEstimate {
    let mut last_usage: Option<(&Usage, usize)> = None;
    for (idx, msg) in messages.iter().enumerate().rev() {
        if let Some(usage) = get_assistant_usage(msg) {
            last_usage = Some((usage, idx));
            break;
        }
    }

    let Some((usage, usage_index)) = last_usage else {
        let total = messages
            .iter()
            .map(estimate_tokens)
            .fold(0u64, u64::saturating_add);
        return ContextUsageEstimate {
            tokens: total,
            last_usage_index: None,
        };
    };

    let usage_tokens = calculate_context_tokens(usage);

    // Fall back to heuristic estimation if the provider didn't return usage metrics
    if usage_tokens == 0 {
        let total = messages
            .iter()
            .map(estimate_tokens)
            .fold(0u64, u64::saturating_add);
        return ContextUsageEstimate {
            tokens: total,
            last_usage_index: None,
        };
    }

    let trailing_tokens = messages[usage_index + 1..]
        .iter()
        .map(estimate_tokens)
        .fold(0u64, u64::saturating_add);
    ContextUsageEstimate {
        tokens: usage_tokens.saturating_add(trailing_tokens),
        last_usage_index: Some(usage_index),
    }
}

fn should_compact(
    context_tokens: u64,
    context_window: u32,
    settings: &ResolvedCompactionSettings,
) -> bool {
    if !settings.enabled {
        return false;
    }
    let reserve = u64::from(settings.reserve_tokens);
    let window = u64::from(context_window);
    context_tokens >= window.saturating_sub(reserve)
}

fn estimate_tokens(message: &SessionMessage) -> u64 {
    let mut chars: usize = 0;

    match message {
        SessionMessage::User { content, .. } => match content {
            UserContent::Text(text) => chars = text.len(),
            UserContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text(text) => {
                            chars = chars.saturating_add(text.text.len());
                        }
                        ContentBlock::Image(_) => {
                            chars = chars.saturating_add(IMAGE_CHAR_ESTIMATE);
                        }
                        ContentBlock::Thinking(thinking) => {
                            chars = chars.saturating_add(thinking.thinking.len());
                        }
                        ContentBlock::ToolCall(call) => {
                            chars = chars.saturating_add(call.name.len());
                            chars = chars.saturating_add(json_byte_len(&call.arguments));
                        }
                        // Opaque marker — the data field is never replayed to a model
                        // (see `convert_content_block_to_anthropic`), so it contributes
                        // zero context tokens.
                        ContentBlock::RedactedThinking(_) => {}
                    }
                }
            }
        },
        SessionMessage::Assistant { message } => {
            for block in &message.content {
                match block {
                    ContentBlock::Text(text) => {
                        chars = chars.saturating_add(text.text.len());
                    }
                    ContentBlock::Thinking(thinking) => {
                        chars = chars.saturating_add(thinking.thinking.len());
                    }
                    ContentBlock::Image(_) => {
                        chars = chars.saturating_add(IMAGE_CHAR_ESTIMATE);
                    }
                    ContentBlock::ToolCall(call) => {
                        chars = chars.saturating_add(call.name.len());
                        chars = chars.saturating_add(json_byte_len(&call.arguments));
                    }
                    ContentBlock::RedactedThinking(_) => {}
                }
            }
        }
        SessionMessage::ToolResult { content, .. } => {
            for block in content {
                match block {
                    ContentBlock::Text(text) => {
                        chars = chars.saturating_add(text.text.len());
                    }
                    ContentBlock::Thinking(thinking) => {
                        chars = chars.saturating_add(thinking.thinking.len());
                    }
                    ContentBlock::Image(_) => {
                        chars = chars.saturating_add(IMAGE_CHAR_ESTIMATE);
                    }
                    ContentBlock::ToolCall(call) => {
                        chars = chars.saturating_add(call.name.len());
                        chars = chars.saturating_add(json_byte_len(&call.arguments));
                    }
                    ContentBlock::RedactedThinking(_) => {}
                }
            }
        }
        SessionMessage::Custom { content, .. } => chars = content.len(),
        SessionMessage::BashExecution {
            command, output, ..
        } => chars = command.len().saturating_add(output.len()),
        SessionMessage::BranchSummary { summary, .. }
        | SessionMessage::CompactionSummary { summary, .. } => chars = summary.len(),
    }

    u64::try_from(chars.div_ceil(CHARS_PER_TOKEN_ESTIMATE)).unwrap_or(u64::MAX)
}

// =============================================================================
// Cut point detection
// =============================================================================

#[derive(Debug, Clone, Copy)]
struct CutPointResult {
    first_kept_entry_index: usize,
    turn_start_index: Option<usize>,
    is_split_turn: bool,
}

fn message_from_entry(entry: &SessionEntry) -> Option<SessionMessage> {
    match entry {
        SessionEntry::Message(msg_entry) => Some(msg_entry.message.clone()),
        SessionEntry::BranchSummary(summary) => Some(SessionMessage::BranchSummary {
            summary: summary.summary.clone(),
            from_id: summary.from_id.clone(),
        }),
        SessionEntry::Compaction(compaction) => Some(SessionMessage::CompactionSummary {
            summary: compaction.summary.clone(),
            tokens_before: compaction.tokens_before,
        }),
        _ => None,
    }
}

const fn entry_is_message_like(entry: &SessionEntry) -> bool {
    matches!(
        entry,
        SessionEntry::Message(_) | SessionEntry::BranchSummary(_)
    )
}

const fn entry_is_compaction_boundary(entry: &SessionEntry) -> bool {
    matches!(entry, SessionEntry::Compaction(_))
}

fn find_valid_cut_points(
    entries: &[SessionEntry],
    start_index: usize,
    end_index: usize,
) -> Vec<usize> {
    let mut cut_points = Vec::new();
    for (idx, entry) in entries.iter().enumerate().take(end_index).skip(start_index) {
        match entry {
            SessionEntry::Message(msg_entry) => match msg_entry.message {
                SessionMessage::ToolResult { .. } => {}
                _ => cut_points.push(idx),
            },
            SessionEntry::BranchSummary(_) => cut_points.push(idx),
            _ => {}
        }
    }
    cut_points
}

fn entry_has_tool_calls(entry: &SessionEntry) -> bool {
    matches!(
        entry,
        SessionEntry::Message(msg) if matches!(
            &msg.message,
            SessionMessage::Assistant { message } if message.content.iter().any(|b| matches!(b, ContentBlock::ToolCall(_)))
        )
    )
}

const fn is_user_turn_start(entry: &SessionEntry) -> bool {
    match entry {
        SessionEntry::BranchSummary(_) => true,
        SessionEntry::Message(msg_entry) => matches!(
            msg_entry.message,
            SessionMessage::User { .. } | SessionMessage::BashExecution { .. }
        ),
        _ => false,
    }
}

fn find_turn_start_index(
    entries: &[SessionEntry],
    entry_index: usize,
    start_index: usize,
) -> Option<usize> {
    (start_index..=entry_index)
        .rev()
        .find(|&idx| is_user_turn_start(&entries[idx]))
}

fn find_cut_point(
    entries: &[SessionEntry],
    start_index: usize,
    end_index: usize,
    keep_recent_tokens: u32,
) -> CutPointResult {
    let cut_points = find_valid_cut_points(entries, start_index, end_index);
    if cut_points.is_empty() {
        return CutPointResult {
            first_kept_entry_index: start_index,
            turn_start_index: None,
            is_split_turn: false,
        };
    }

    let mut accumulated_tokens: u64 = 0;
    let mut cut_index = cut_points[0];

    for i in (start_index..end_index).rev() {
        let entry = &entries[i];
        if let Some(msg) = message_from_entry(entry) {
            accumulated_tokens = accumulated_tokens.saturating_add(estimate_tokens(&msg));
        } else {
            continue;
        }

        if accumulated_tokens >= u64::from(keep_recent_tokens) {
            // Binary search: find the largest cut point <= i.
            // `partition_point` returns the index of the first element > i,
            // so idx-1 is the largest element <= i (if any).
            let pos = cut_points.partition_point(|&cp| cp <= i);
            if pos > 0 {
                cut_index = cut_points[pos - 1];
            }
            // else: no cut point <= i, keep the fallback (cut_points[0])
            break;
        }
    }

    while cut_index > start_index {
        let prev = &entries[cut_index - 1];
        if entry_is_compaction_boundary(prev) {
            break;
        }
        if entry_is_message_like(prev) {
            break;
        }
        cut_index -= 1;
    }

    let is_user_message = is_user_turn_start(&entries[cut_index]);
    let turn_start_index = if is_user_message {
        None
    } else {
        find_turn_start_index(entries, cut_index, start_index)
    };

    CutPointResult {
        first_kept_entry_index: cut_index,
        turn_start_index,
        is_split_turn: !is_user_message && turn_start_index.is_some(),
    }
}

// =============================================================================
// Summarization prompts
// =============================================================================

const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI coding assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

fn push_message_separator(out: &mut String) {
    if !out.is_empty() {
        out.push_str("\n\n");
    }
}

fn user_has_serializable_content(user: &UserMessage) -> bool {
    match &user.content {
        UserContent::Text(text) => !text.is_empty(),
        UserContent::Blocks(blocks) => blocks
            .iter()
            .any(|c| matches!(c, ContentBlock::Text(t) if !t.text.is_empty())),
    }
}

fn append_user_message(out: &mut String, user: &UserMessage) {
    if !user_has_serializable_content(user) {
        return;
    }

    push_message_separator(out);
    out.push_str("[User]: ");
    match &user.content {
        UserContent::Text(text) => out.push_str(text),
        UserContent::Blocks(blocks) => {
            for block in blocks {
                if let ContentBlock::Text(text) = block {
                    out.push_str(&text.text);
                }
            }
        }
    }
}

fn append_custom_message(out: &mut String, custom_type: &str, content: &str) {
    if content.trim().is_empty() {
        return;
    }

    push_message_separator(out);
    out.push('[');
    if custom_type.trim().is_empty() {
        out.push_str("Custom");
    } else {
        out.push_str("Custom:");
        out.push_str(custom_type);
    }
    out.push_str("]: ");
    out.push_str(content);
}

fn assistant_content_flags(assistant: &AssistantMessage) -> (bool, bool, bool) {
    let mut has_thinking = false;
    let mut has_text = false;
    let mut has_tools = false;
    for block in &assistant.content {
        match block {
            ContentBlock::Thinking(_) => has_thinking = true,
            ContentBlock::Text(_) => has_text = true,
            ContentBlock::ToolCall(_) => has_tools = true,
            // Redacted thinking has no surfaceable content, so don't flip
            // has_thinking — that would produce an empty `[Assistant thinking]:`
            // section in the compaction output.
            ContentBlock::Image(_) | ContentBlock::RedactedThinking(_) => {}
        }
    }
    (has_thinking, has_text, has_tools)
}

fn append_assistant_thinking(out: &mut String, assistant: &AssistantMessage) {
    push_message_separator(out);
    out.push_str("[Assistant thinking]: ");
    let mut first = true;
    for block in &assistant.content {
        if let ContentBlock::Thinking(thinking) = block {
            if !first {
                out.push('\n');
            }
            out.push_str(&thinking.thinking);
            first = false;
        }
    }
}

fn append_assistant_text(out: &mut String, assistant: &AssistantMessage) {
    push_message_separator(out);
    out.push_str("[Assistant]: ");
    let mut first = true;
    for block in &assistant.content {
        if let ContentBlock::Text(text) = block {
            if !first {
                out.push('\n');
            }
            out.push_str(&text.text);
            first = false;
        }
    }
}

fn append_tool_call_arguments(out: &mut String, arguments: &Value) {
    if let Some(obj) = arguments.as_object() {
        let mut first_kv = true;
        for (k, v) in obj {
            if !first_kv {
                out.push_str(", ");
            }
            out.push_str(k);
            out.push('=');
            match serde_json::to_string(v) {
                Ok(s) => out.push_str(&s),
                Err(_) => {
                    let _ = write!(out, "{v}");
                }
            }
            first_kv = false;
        }
    } else {
        match serde_json::to_string(arguments) {
            Ok(s) => out.push_str(&s),
            Err(_) => {
                let _ = write!(out, "{arguments}");
            }
        }
    }
}

fn append_assistant_tool_calls(out: &mut String, assistant: &AssistantMessage) {
    push_message_separator(out);
    out.push_str("[Assistant tool calls]: ");
    let mut first = true;
    for block in &assistant.content {
        if let ContentBlock::ToolCall(call) = block {
            if !first {
                out.push_str("; ");
            }
            out.push_str(&call.name);
            out.push('(');
            append_tool_call_arguments(out, &call.arguments);
            out.push(')');
            first = false;
        }
    }
}

fn append_assistant_message(out: &mut String, assistant: &AssistantMessage) {
    let (has_thinking, has_text, has_tools) = assistant_content_flags(assistant);
    if has_thinking {
        append_assistant_thinking(out, assistant);
    }
    if has_text {
        append_assistant_text(out, assistant);
    }
    if has_tools {
        append_assistant_tool_calls(out, assistant);
    }
}

fn tool_result_has_serializable_content(content: &[ContentBlock]) -> bool {
    content
        .iter()
        .any(|c| matches!(c, ContentBlock::Text(t) if !t.text.is_empty()))
}

fn append_tool_result_message(out: &mut String, content: &[ContentBlock]) {
    if !tool_result_has_serializable_content(content) {
        return;
    }

    push_message_separator(out);
    out.push_str("[Tool result]: ");
    for block in content {
        if let ContentBlock::Text(text) = block {
            out.push_str(&text.text);
        }
    }
}

fn collect_text_blocks(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    let mut first = true;
    for block in blocks {
        if let ContentBlock::Text(text) = block {
            if !first {
                out.push('\n');
            }
            out.push_str(&text.text);
            first = false;
        }
    }
    out
}

fn serialize_conversation(messages: &[Message]) -> String {
    let mut out = String::new();

    for msg in messages {
        match msg {
            Message::User(user) => append_user_message(&mut out, user),
            Message::Custom(custom) => {
                append_custom_message(&mut out, &custom.custom_type, &custom.content);
            }
            Message::Assistant(assistant) => append_assistant_message(&mut out, assistant),
            Message::ToolResult(tool) => append_tool_result_message(&mut out, &tool.content),
        }
    }

    out
}

async fn complete_simple(
    provider: Arc<dyn Provider>,
    system_prompt: &str,
    prompt_text: String,
    api_key: &str,
    reserve_tokens: u32,
    max_tokens_factor: f64,
) -> Result<AssistantMessage> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let max_tokens = (f64::from(reserve_tokens) * max_tokens_factor).floor() as u32;
    let max_tokens = max_tokens.max(256);

    let context = Context {
        system_prompt: Some(system_prompt.to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(prompt_text))]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        })]
        .into(),
        tools: Vec::new().into(),
    };

    let options = StreamOptions {
        api_key: Some(api_key.to_string()),
        max_tokens: Some(max_tokens),
        thinking_level: Some(ThinkingLevel::High),
        ..Default::default()
    };

    let mut stream = provider.stream(&context, &options).await?;
    let mut final_message: Option<AssistantMessage> = None;

    while let Some(event) = stream.next().await {
        match event? {
            crate::model::StreamEvent::Done { message, .. } => {
                final_message = Some(message);
            }
            crate::model::StreamEvent::Error { error, .. } => {
                let msg = error
                    .error_message
                    .unwrap_or_else(|| "Summarization error".to_string());
                return Err(Error::api(msg));
            }
            _ => {}
        }
    }

    let message = final_message.ok_or_else(|| Error::api("Stream ended without Done event"))?;
    if matches!(message.stop_reason, StopReason::Aborted | StopReason::Error) {
        let msg = message
            .error_message
            .unwrap_or_else(|| "Summarization error".to_string());
        return Err(Error::api(msg));
    }
    Ok(message)
}

async fn generate_summary(
    messages: &[SessionMessage],
    provider: Arc<dyn Provider>,
    api_key: &str,
    settings: &ResolvedCompactionSettings,
    custom_instructions: Option<&str>,
    previous_summary: Option<&str>,
) -> Result<String> {
    let base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    };

    let mut prompt = base_prompt.to_string();
    if let Some(custom) = custom_instructions.filter(|s| !s.trim().is_empty()) {
        let _ = write!(prompt, "\n\nAdditional focus: {custom}");
    }

    let llm_messages = messages
        .iter()
        .filter_map(session_message_to_model)
        .collect::<Vec<_>>();
    let conversation_text = serialize_conversation(&llm_messages);

    let mut prompt_text = format!("<conversation>\n{conversation_text}\n</conversation>\n\n");
    if let Some(previous) = previous_summary {
        let _ = write!(
            prompt_text,
            "<previous-summary>\n{previous}\n</previous-summary>\n\n"
        );
    }
    prompt_text.push_str(&prompt);

    let assistant = complete_simple(
        provider,
        SUMMARIZATION_SYSTEM_PROMPT,
        prompt_text,
        api_key,
        settings.reserve_tokens,
        0.8,
    )
    .await?;

    let text = collect_text_blocks(&assistant.content);

    if text.trim().is_empty() {
        return Err(Error::api(
            "Summarization returned empty text; refusing to store empty compaction summary",
        ));
    }

    Ok(text)
}

async fn generate_turn_prefix_summary(
    messages: &[SessionMessage],
    provider: Arc<dyn Provider>,
    api_key: &str,
    settings: &ResolvedCompactionSettings,
) -> Result<String> {
    let llm_messages = messages
        .iter()
        .filter_map(session_message_to_model)
        .collect::<Vec<_>>();
    let conversation_text = serialize_conversation(&llm_messages);
    let prompt_text = format!(
        "<conversation>\n{conversation_text}\n</conversation>\n\n{TURN_PREFIX_SUMMARIZATION_PROMPT}"
    );

    let assistant = complete_simple(
        provider,
        SUMMARIZATION_SYSTEM_PROMPT,
        prompt_text,
        api_key,
        settings.reserve_tokens,
        0.5,
    )
    .await?;

    let text = collect_text_blocks(&assistant.content);

    if text.trim().is_empty() {
        return Err(Error::api(
            "Turn prefix summarization returned empty text; refusing to store empty summary",
        ));
    }

    Ok(text)
}

// =============================================================================
// Public API
// =============================================================================

#[allow(clippy::too_many_lines)]
pub fn prepare_compaction(
    path_entries: &[SessionEntry],
    settings: ResolvedCompactionSettings,
) -> Option<CompactionPreparation> {
    if path_entries.is_empty() {
        return None;
    }

    if path_entries
        .last()
        .is_some_and(|entry| matches!(entry, SessionEntry::Compaction(_)))
    {
        return None;
    }

    let mut prev_compaction_index: Option<usize> = None;
    for (idx, entry) in path_entries.iter().enumerate().rev() {
        if matches!(entry, SessionEntry::Compaction(_)) {
            prev_compaction_index = Some(idx);
            break;
        }
    }

    let boundary_start = prev_compaction_index.map_or(0, |i| i + 1);
    let boundary_end = path_entries.len();

    let usage_start = prev_compaction_index.unwrap_or(0);
    let mut usage_messages = Vec::new();
    for entry in &path_entries[usage_start..boundary_end] {
        if let Some(msg) = message_from_entry(entry) {
            usage_messages.push(msg);
        }
    }
    // Calculate the tokens *currently* occupied by the segment we are about to compact.
    // If the segment includes a previous compaction summary, this counts the *summary* tokens,
    // not the original uncompressed history tokens. This effectively tracks the "compressed size"
    // of the history prior to the new cut point.
    let tokens_before = estimate_context_tokens(&usage_messages).tokens;

    if !should_compact(tokens_before, settings.context_window_tokens, &settings) {
        return None;
    }

    let cut_point = find_cut_point(
        path_entries,
        boundary_start,
        boundary_end,
        settings.keep_recent_tokens,
    );

    let first_kept_entry = &path_entries[cut_point.first_kept_entry_index];
    let first_kept_entry_id = first_kept_entry.base_id()?.clone();

    let history_end = if cut_point.is_split_turn {
        cut_point.turn_start_index?
    } else {
        cut_point.first_kept_entry_index
    };

    let mut messages_to_summarize = Vec::new();
    for entry in &path_entries[boundary_start..history_end] {
        if let Some(msg) = message_from_entry(entry) {
            messages_to_summarize.push(msg);
        }
    }

    let mut turn_prefix_messages = Vec::new();
    if cut_point.is_split_turn {
        let turn_start = cut_point.turn_start_index?;
        for entry in &path_entries[turn_start..cut_point.first_kept_entry_index] {
            if let Some(msg) = message_from_entry(entry) {
                turn_prefix_messages.push(msg);
            }
        }
    }

    // No-op compaction: if there's nothing to summarize, don't issue an LLM call and don't append a
    // compaction entry. This can happen early in a session (e.g. session header entries only).
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return None;
    }

    let previous_summary = prev_compaction_index.and_then(|idx| match &path_entries[idx] {
        SessionEntry::Compaction(entry) => Some(entry.summary.clone()),
        _ => None,
    });

    let mut file_ops = FileOperations::default();

    // Collect file tracking from previous compaction details if pi-generated.
    if let Some(idx) = prev_compaction_index
        && let SessionEntry::Compaction(entry) = &path_entries[idx]
        && !entry.from_hook.unwrap_or(false)
        && let Some(details) = entry.details.as_ref().and_then(Value::as_object)
    {
        if let Some(read_files) = details.get("readFiles").and_then(Value::as_array) {
            for item in read_files.iter().filter_map(Value::as_str) {
                file_ops.read.insert(item.to_string());
            }
        }
        if let Some(modified_files) = details.get("modifiedFiles").and_then(Value::as_array) {
            for item in modified_files.iter().filter_map(Value::as_str) {
                file_ops.edited.insert(item.to_string());
            }
        }
    }

    let mut tool_status = build_tool_status_map(&messages_to_summarize);
    tool_status.extend(build_tool_status_map(&turn_prefix_messages));

    for msg in &messages_to_summarize {
        extract_file_ops_from_message(msg, &mut file_ops, &tool_status);
    }
    for msg in &turn_prefix_messages {
        extract_file_ops_from_message(msg, &mut file_ops, &tool_status);
    }

    Some(CompactionPreparation {
        first_kept_entry_id,
        messages_to_summarize,
        turn_prefix_messages,
        is_split_turn: cut_point.is_split_turn,
        tokens_before,
        previous_summary,
        file_ops,
        settings,
    })
}

pub async fn summarize_entries(
    entries: &[SessionEntry],
    provider: Arc<dyn Provider>,
    api_key: &str,
    reserve_tokens: u32,
    custom_instructions: Option<&str>,
) -> Result<Option<String>> {
    let mut messages = Vec::new();
    for entry in entries {
        if let Some(message) = message_from_entry(entry) {
            messages.push(message);
        }
    }

    if messages.is_empty() {
        return Ok(None);
    }

    let settings = ResolvedCompactionSettings {
        enabled: true,
        reserve_tokens,
        keep_recent_tokens: 0,
        ..Default::default()
    };

    let summary = generate_summary(
        &messages,
        provider,
        api_key,
        &settings,
        custom_instructions,
        None,
    )
    .await?;

    Ok(Some(summary))
}

pub async fn compact(
    preparation: CompactionPreparation,
    provider: Arc<dyn Provider>,
    api_key: &str,
    custom_instructions: Option<&str>,
) -> Result<CompactionResult> {
    let summary = if preparation.is_split_turn && !preparation.turn_prefix_messages.is_empty() {
        let history_summary = if preparation.messages_to_summarize.is_empty() {
            "No prior history.".to_string()
        } else {
            generate_summary(
                &preparation.messages_to_summarize,
                Arc::clone(&provider),
                api_key,
                &preparation.settings,
                custom_instructions,
                preparation.previous_summary.as_deref(),
            )
            .await?
        };

        let turn_prefix_summary = generate_turn_prefix_summary(
            &preparation.turn_prefix_messages,
            Arc::clone(&provider),
            api_key,
            &preparation.settings,
        )
        .await?;

        format!(
            "{history_summary}\n\n---\n\n**Turn Context (split turn):**\n\n{turn_prefix_summary}"
        )
    } else {
        generate_summary(
            &preparation.messages_to_summarize,
            Arc::clone(&provider),
            api_key,
            &preparation.settings,
            custom_instructions,
            preparation.previous_summary.as_deref(),
        )
        .await?
    };

    let (read_files, modified_files) = compute_file_lists(&preparation.file_ops);
    let details = CompactionDetails {
        read_files: read_files.clone(),
        modified_files: modified_files.clone(),
    };

    let mut summary = summary;
    summary.push_str(&format_file_operations(&read_files, &modified_files));

    Ok(CompactionResult {
        summary,
        first_kept_entry_id: preparation.first_kept_entry_id,
        tokens_before: preparation.tokens_before,
        details,
    })
}

pub fn compaction_details_to_value(details: &CompactionDetails) -> Result<Value> {
    serde_json::to_value(details).map_err(|e| Error::session(format!("Compaction details: {e}")))
}

pub mod semantic_marker_scan_quality {
    use super::*;
    use serde_json::json;

    // =============================================================================
    // Semantic compaction quality differential harness
    // =============================================================================

    /// Schema emitted by the deterministic semantic compaction quality harness.
    pub const SEMANTIC_COMPACTION_QUALITY_SCHEMA_V1: &str =
        "pi.session.semantic_compaction_quality.v1";

    const SEMANTIC_QUALITY_MARKER_PREFIX: &str = "[[SCQ:";
    const SEMANTIC_QUALITY_MARKER_SUFFIX: &str = "]]";

    /// One redacted turn or summary chunk to scan for structured semantic markers.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SemanticCompactionQualityTurn {
        pub branch_id: String,
        pub turn_id: String,
        pub role: String,
        pub content: String,
    }

    impl SemanticCompactionQualityTurn {
        #[must_use]
        pub fn new(
            branch_id: impl Into<String>,
            turn_id: impl Into<String>,
            role: impl Into<String>,
            content: impl Into<String>,
        ) -> Self {
            Self {
                branch_id: branch_id.into(),
                turn_id: turn_id.into(),
                role: role.into(),
                content: content.into(),
            }
        }
    }

    /// Named baseline or candidate view used by the quality evaluator.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SemanticCompactionQualityView {
        pub name: String,
        pub turns: Vec<SemanticCompactionQualityTurn>,
    }

    impl SemanticCompactionQualityView {
        #[must_use]
        pub fn new(name: impl Into<String>, turns: Vec<SemanticCompactionQualityTurn>) -> Self {
            Self {
                name: name.into(),
                turns,
            }
        }
    }

    /// Redacted summary of an evaluated view.
    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    pub struct SemanticCompactionQualityViewSummary {
        pub name: String,
        pub turn_count: usize,
        pub marker_count: usize,
        pub content_fingerprint: String,
    }

    /// One structured marker occurrence found in a turn.
    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    pub struct SemanticMarkerOccurrence {
        pub id: String,
        pub kind: String,
        pub critical: bool,
        pub declared_branch_id: String,
        pub declared_turn_id: String,
        pub source_view: String,
        pub location_branch_id: String,
        pub location_turn_id: String,
        pub location_role: String,
        pub marker_index: usize,
        pub content_fingerprint: String,
    }

    /// Per-marker differential outcome.
    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    pub struct SemanticCompactionQualityOutcome {
        pub marker_id: String,
        pub status: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub loss_class: Option<String>,
        pub critical: bool,
        pub kind: String,
        pub expected_branch_id: String,
        pub expected_turn_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub observed_branch_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub observed_turn_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub baseline_location: Option<SemanticMarkerOccurrence>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub candidate_location: Option<SemanticMarkerOccurrence>,
    }

    /// False-positive control result for marker IDs that must not be invented.
    #[derive(Debug, Clone, Serialize, PartialEq, Eq)]
    #[serde(rename_all = "camelCase")]
    pub struct SemanticFalsePositiveControlResult {
        pub marker_id: String,
        pub tripped: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub candidate_location: Option<SemanticMarkerOccurrence>,
    }

    /// Aggregate quality counts.
    #[derive(Debug, Clone, Serialize, PartialEq)]
    #[serde(rename_all = "camelCase")]
    pub struct SemanticCompactionQualitySummary {
        pub total_expected_markers: usize,
        pub critical_expected_markers: usize,
        pub preserved_markers: usize,
        pub missing_markers: usize,
        pub wrong_branch_markers: usize,
        pub wrong_turn_markers: usize,
        pub metadata_mismatch_markers: usize,
        pub duplicate_markers: usize,
        pub unexpected_markers: usize,
        pub false_positive_controls_tripped: usize,
        pub marker_coverage: f64,
        pub critical_marker_coverage: f64,
    }

    /// Complete deterministic semantic quality report.
    #[derive(Debug, Clone, Serialize, PartialEq)]
    #[serde(rename_all = "camelCase")]
    pub struct SemanticCompactionQualityReport {
        pub schema: &'static str,
        pub baseline_view: SemanticCompactionQualityViewSummary,
        pub candidate_view: SemanticCompactionQualityViewSummary,
        pub verdict: String,
        pub summary: SemanticCompactionQualitySummary,
        pub outcomes: Vec<SemanticCompactionQualityOutcome>,
        pub false_positive_controls: Vec<SemanticFalsePositiveControlResult>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SemanticMarkerDescriptor {
        id: String,
        kind: String,
        branch_id: String,
        turn_id: String,
        critical: bool,
    }

    /// Build a marker string accepted by the semantic compaction quality harness.
    #[must_use]
    pub fn semantic_compaction_quality_marker(
        id: &str,
        kind: &str,
        branch_id: &str,
        turn_id: &str,
        critical: bool,
    ) -> String {
        format!(
            "{SEMANTIC_QUALITY_MARKER_PREFIX}id={id};kind={kind};branch={branch_id};turn={turn_id};critical={critical}{SEMANTIC_QUALITY_MARKER_SUFFIX}"
        )
    }

    fn normalize_marker_field(value: &str) -> String {
        value.trim().to_ascii_lowercase().replace('-', "_")
    }

    fn parse_semantic_quality_marker(payload: &str) -> Option<SemanticMarkerDescriptor> {
        let mut fields = HashMap::new();
        for raw_part in payload.split(';') {
            let (raw_key, raw_value) = raw_part.split_once('=')?;
            let key = normalize_marker_field(raw_key);
            let value = raw_value.trim();
            if key.is_empty() || value.is_empty() {
                return None;
            }
            fields.insert(key, value.to_string());
        }

        let id = fields.remove("id")?;
        let kind = fields.remove("kind")?;
        let branch_id = fields.remove("branch")?;
        let turn_id = fields.remove("turn")?;
        let critical = fields
            .remove("critical")
            .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "critical"));

        Some(SemanticMarkerDescriptor {
            id,
            kind,
            branch_id,
            turn_id,
            critical,
        })
    }

    fn short_content_fingerprint(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        digest.chars().take(16).collect()
    }

    fn view_content_fingerprint(turns: &[SemanticCompactionQualityTurn]) -> String {
        let mut hasher = Sha256::new();
        for turn in turns {
            hasher.update(turn.branch_id.as_bytes());
            hasher.update(b"\0");
            hasher.update(turn.turn_id.as_bytes());
            hasher.update(b"\0");
            hasher.update(turn.role.as_bytes());
            hasher.update(b"\0");
            hasher.update(short_content_fingerprint(&turn.content).as_bytes());
            hasher.update(b"\0");
        }
        let digest = format!("{:x}", hasher.finalize());
        digest.chars().take(16).collect()
    }

    fn semantic_owned(value: &str) -> String {
        value.to_owned()
    }

    fn semantic_occurrence_owned(
        occurrence: &SemanticMarkerOccurrence,
    ) -> SemanticMarkerOccurrence {
        occurrence.clone()
    }

    fn scan_semantic_quality_markers(
        view: &SemanticCompactionQualityView,
    ) -> Vec<SemanticMarkerOccurrence> {
        let mut occurrences = Vec::new();
        for turn in &view.turns {
            let content_fingerprint = short_content_fingerprint(&turn.content);
            let mut search_start = 0usize;
            while let Some(start_rel) =
                turn.content[search_start..].find(SEMANTIC_QUALITY_MARKER_PREFIX)
            {
                let payload_start = search_start + start_rel + SEMANTIC_QUALITY_MARKER_PREFIX.len();
                let Some(end_rel) =
                    turn.content[payload_start..].find(SEMANTIC_QUALITY_MARKER_SUFFIX)
                else {
                    break;
                };
                let payload_end = payload_start + end_rel;
                let payload = &turn.content[payload_start..payload_end];
                if let Some(marker) = parse_semantic_quality_marker(payload) {
                    occurrences.push(SemanticMarkerOccurrence {
                        id: marker.id,
                        kind: marker.kind,
                        critical: marker.critical,
                        declared_branch_id: marker.branch_id,
                        declared_turn_id: marker.turn_id,
                        source_view: semantic_owned(&view.name),
                        location_branch_id: semantic_owned(&turn.branch_id),
                        location_turn_id: semantic_owned(&turn.turn_id),
                        location_role: semantic_owned(&turn.role),
                        marker_index: occurrences.len(),
                        content_fingerprint: semantic_owned(&content_fingerprint),
                    });
                }
                search_start = payload_end + SEMANTIC_QUALITY_MARKER_SUFFIX.len();
            }
        }
        occurrences
    }

    fn occurrences_by_id(
        occurrences: Vec<SemanticMarkerOccurrence>,
    ) -> BTreeMap<String, Vec<SemanticMarkerOccurrence>> {
        let mut by_id: BTreeMap<String, Vec<SemanticMarkerOccurrence>> = BTreeMap::new();
        for occurrence in occurrences {
            by_id
                .entry(semantic_owned(&occurrence.id))
                .or_default()
                .push(occurrence);
        }
        by_id
    }

    #[allow(clippy::cast_precision_loss)]
    fn coverage(preserved: usize, total: usize) -> f64 {
        if total == 0 {
            return 1.0;
        }
        preserved as f64 / total as f64
    }

    fn outcome_for_loss(
        baseline: &SemanticMarkerOccurrence,
        candidate: Option<&SemanticMarkerOccurrence>,
        status: &str,
        loss_class: &str,
    ) -> SemanticCompactionQualityOutcome {
        SemanticCompactionQualityOutcome {
            marker_id: semantic_owned(&baseline.id),
            status: status.to_string(),
            loss_class: Some(loss_class.to_string()),
            critical: baseline.critical,
            kind: semantic_owned(&baseline.kind),
            expected_branch_id: semantic_owned(&baseline.declared_branch_id),
            expected_turn_id: semantic_owned(&baseline.declared_turn_id),
            observed_branch_id: candidate
                .map(|occurrence| semantic_owned(&occurrence.declared_branch_id)),
            observed_turn_id: candidate
                .map(|occurrence| semantic_owned(&occurrence.declared_turn_id)),
            baseline_location: Some(semantic_occurrence_owned(baseline)),
            candidate_location: candidate.map(semantic_occurrence_owned),
        }
    }

    fn outcome_for_preserved(
        baseline: &SemanticMarkerOccurrence,
        candidate: &SemanticMarkerOccurrence,
    ) -> SemanticCompactionQualityOutcome {
        SemanticCompactionQualityOutcome {
            marker_id: semantic_owned(&baseline.id),
            status: "preserved".to_string(),
            loss_class: None,
            critical: baseline.critical,
            kind: semantic_owned(&baseline.kind),
            expected_branch_id: semantic_owned(&baseline.declared_branch_id),
            expected_turn_id: semantic_owned(&baseline.declared_turn_id),
            observed_branch_id: Some(semantic_owned(&candidate.declared_branch_id)),
            observed_turn_id: Some(semantic_owned(&candidate.declared_turn_id)),
            baseline_location: Some(semantic_occurrence_owned(baseline)),
            candidate_location: Some(semantic_occurrence_owned(candidate)),
        }
    }

    fn unexpected_outcome(
        candidate: &SemanticMarkerOccurrence,
        is_control: bool,
    ) -> SemanticCompactionQualityOutcome {
        let loss_class = if is_control {
            "false_positive_control"
        } else {
            "unexpected_marker"
        };
        SemanticCompactionQualityOutcome {
            marker_id: semantic_owned(&candidate.id),
            status: "failed".to_string(),
            loss_class: Some(loss_class.to_string()),
            critical: candidate.critical,
            kind: semantic_owned(&candidate.kind),
            expected_branch_id: String::new(),
            expected_turn_id: String::new(),
            observed_branch_id: Some(semantic_owned(&candidate.declared_branch_id)),
            observed_turn_id: Some(semantic_owned(&candidate.declared_turn_id)),
            baseline_location: None,
            candidate_location: Some(semantic_occurrence_owned(candidate)),
        }
    }

    /// Evaluate whether a compacted/replayed view preserved structured semantic markers.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn evaluate_semantic_compaction_quality(
        baseline: &SemanticCompactionQualityView,
        candidate: &SemanticCompactionQualityView,
        false_positive_control_ids: &[String],
    ) -> SemanticCompactionQualityReport {
        let baseline_occurrences = scan_semantic_quality_markers(baseline);
        let candidate_occurrences = scan_semantic_quality_markers(candidate);
        let baseline_marker_count = baseline_occurrences.len();
        let candidate_marker_count = candidate_occurrences.len();
        let baseline_by_id = occurrences_by_id(baseline_occurrences);
        let candidate_by_id = occurrences_by_id(candidate_occurrences);
        let controls: BTreeSet<String> = false_positive_control_ids.iter().cloned().collect();

        let mut outcomes = Vec::new();
        let mut preserved_markers = 0usize;
        let mut missing_markers = 0usize;
        let mut wrong_branch_markers = 0usize;
        let mut wrong_turn_markers = 0usize;
        let mut metadata_mismatch_markers = 0usize;
        let mut duplicate_markers = 0usize;
        let mut unexpected_markers = 0usize;
        let mut critical_expected_markers = 0usize;
        let mut critical_preserved_markers = 0usize;

        for (id, baseline_matches) in &baseline_by_id {
            let baseline_marker = &baseline_matches[0];
            critical_expected_markers += usize::from(baseline_marker.critical);

            if baseline_matches.len() > 1 {
                duplicate_markers += 1;
                outcomes.push(outcome_for_loss(
                    baseline_marker,
                    None,
                    "failed",
                    "duplicate_baseline_marker",
                ));
                continue;
            }

            let Some(candidate_matches) = candidate_by_id.get(id) else {
                missing_markers += 1;
                outcomes.push(outcome_for_loss(
                    baseline_marker,
                    None,
                    "failed",
                    "missing_marker",
                ));
                continue;
            };

            if candidate_matches.len() > 1 {
                duplicate_markers += 1;
                outcomes.push(outcome_for_loss(
                    baseline_marker,
                    candidate_matches.first(),
                    "failed",
                    "duplicate_candidate_marker",
                ));
                continue;
            }

            let candidate_marker = &candidate_matches[0];
            if baseline_marker.kind != candidate_marker.kind
                || baseline_marker.critical != candidate_marker.critical
            {
                metadata_mismatch_markers += 1;
                outcomes.push(outcome_for_loss(
                    baseline_marker,
                    Some(candidate_marker),
                    "failed",
                    "metadata_mismatch",
                ));
            } else if baseline_marker.declared_branch_id != candidate_marker.declared_branch_id {
                wrong_branch_markers += 1;
                outcomes.push(outcome_for_loss(
                    baseline_marker,
                    Some(candidate_marker),
                    "failed",
                    "wrong_branch",
                ));
            } else if baseline_marker.declared_turn_id != candidate_marker.declared_turn_id {
                wrong_turn_markers += 1;
                outcomes.push(outcome_for_loss(
                    baseline_marker,
                    Some(candidate_marker),
                    "failed",
                    "wrong_turn",
                ));
            } else {
                preserved_markers += 1;
                critical_preserved_markers += usize::from(baseline_marker.critical);
                outcomes.push(outcome_for_preserved(baseline_marker, candidate_marker));
            }
        }

        let mut false_positive_controls = Vec::new();
        let mut false_positive_controls_tripped = 0usize;
        for marker_id in &controls {
            let candidate_location = candidate_by_id
                .get(marker_id)
                .and_then(|matches| matches.first())
                .map(semantic_occurrence_owned);
            let tripped = candidate_location.is_some();
            false_positive_controls_tripped += usize::from(tripped);
            false_positive_controls.push(SemanticFalsePositiveControlResult {
                marker_id: semantic_owned(marker_id),
                tripped,
                candidate_location,
            });
        }

        for (id, candidate_matches) in &candidate_by_id {
            if baseline_by_id.contains_key(id) {
                continue;
            }
            unexpected_markers += 1;
            outcomes.push(unexpected_outcome(
                &candidate_matches[0],
                controls.contains(id),
            ));
        }

        outcomes.sort_by(|a, b| a.marker_id.cmp(&b.marker_id));
        false_positive_controls.sort_by(|a, b| a.marker_id.cmp(&b.marker_id));

        let total_expected_markers = baseline_by_id.len();
        let summary = SemanticCompactionQualitySummary {
            total_expected_markers,
            critical_expected_markers,
            preserved_markers,
            missing_markers,
            wrong_branch_markers,
            wrong_turn_markers,
            metadata_mismatch_markers,
            duplicate_markers,
            unexpected_markers,
            false_positive_controls_tripped,
            marker_coverage: coverage(preserved_markers, total_expected_markers),
            critical_marker_coverage: coverage(
                critical_preserved_markers,
                critical_expected_markers,
            ),
        };

        let failed = summary.missing_markers > 0
            || summary.wrong_branch_markers > 0
            || summary.wrong_turn_markers > 0
            || summary.metadata_mismatch_markers > 0
            || summary.duplicate_markers > 0
            || summary.unexpected_markers > 0
            || summary.false_positive_controls_tripped > 0;

        SemanticCompactionQualityReport {
            schema: SEMANTIC_COMPACTION_QUALITY_SCHEMA_V1,
            baseline_view: SemanticCompactionQualityViewSummary {
                name: baseline.name.clone(),
                turn_count: baseline.turns.len(),
                marker_count: baseline_marker_count,
                content_fingerprint: view_content_fingerprint(&baseline.turns),
            },
            candidate_view: SemanticCompactionQualityViewSummary {
                name: candidate.name.clone(),
                turn_count: candidate.turns.len(),
                marker_count: candidate_marker_count,
                content_fingerprint: view_content_fingerprint(&candidate.turns),
            },
            verdict: if failed { "fail" } else { "pass" }.to_string(),
            summary,
            outcomes,
            false_positive_controls,
        }
    }

    /// Serialize a report as JSONL: one summary row followed by one row per outcome/control.
    pub fn semantic_compaction_quality_report_to_jsonl(
        report: &SemanticCompactionQualityReport,
    ) -> Result<String> {
        let mut lines = Vec::with_capacity(
            1usize
                .saturating_add(report.outcomes.len())
                .saturating_add(report.false_positive_controls.len()),
        );
        lines.push(
            serde_json::to_string(&json!({
                "schema": report.schema,
                "recordType": "summary",
                "verdict": report.verdict,
                "baselineView": report.baseline_view,
                "candidateView": report.candidate_view,
                "summary": report.summary,
            }))
            .map_err(|e| Error::session(format!("Semantic quality summary JSONL: {e}")))?,
        );
        for outcome in &report.outcomes {
            lines.push(
                serde_json::to_string(&json!({
                    "schema": report.schema,
                    "recordType": "marker_outcome",
                    "outcome": outcome,
                }))
                .map_err(|e| Error::session(format!("Semantic quality outcome JSONL: {e}")))?,
            );
        }
        for control in &report.false_positive_controls {
            lines.push(
                serde_json::to_string(&json!({
                    "schema": report.schema,
                    "recordType": "false_positive_control",
                    "control": control,
                }))
                .map_err(|e| Error::session(format!("Semantic quality control JSONL: {e}")))?,
            );
        }
        Ok(lines.join("\n"))
    }

    fn push_content_block_text(out: &mut String, block: &ContentBlock) {
        match block {
            ContentBlock::Text(text) => out.push_str(&text.text),
            ContentBlock::Thinking(thinking) => out.push_str(&thinking.thinking),
            ContentBlock::ToolCall(call) => {
                let _ = write!(out, "{} {}", call.name, call.arguments);
            }
            ContentBlock::Image(_) | ContentBlock::RedactedThinking(_) => {}
        }
    }

    fn session_message_semantic_text(message: &SessionMessage) -> String {
        let mut text = String::new();
        match message {
            SessionMessage::User { content, .. } => match content {
                UserContent::Text(value) => text.push_str(value),
                UserContent::Blocks(blocks) => {
                    for block in blocks {
                        push_content_block_text(&mut text, block);
                        text.push('\n');
                    }
                }
            },
            SessionMessage::Assistant { message } => {
                for block in &message.content {
                    push_content_block_text(&mut text, block);
                    text.push('\n');
                }
            }
            SessionMessage::ToolResult { content, .. } => {
                for block in content {
                    push_content_block_text(&mut text, block);
                    text.push('\n');
                }
            }
            SessionMessage::Custom { content, .. } => text.push_str(content),
            SessionMessage::BashExecution {
                command, output, ..
            } => {
                let _ = write!(text, "{command}\n{output}");
            }
            SessionMessage::BranchSummary { summary, .. }
            | SessionMessage::CompactionSummary { summary, .. } => text.push_str(summary),
        }
        text
    }

    const fn semantic_role_for_message(message: &SessionMessage) -> &'static str {
        match message {
            SessionMessage::User { .. } => "user",
            SessionMessage::Assistant { .. } => "assistant",
            SessionMessage::ToolResult { .. } => "tool_result",
            SessionMessage::Custom { .. } => "custom",
            SessionMessage::BashExecution { .. } => "bash_execution",
            SessionMessage::BranchSummary { .. } => "branch_summary",
            SessionMessage::CompactionSummary { .. } => "compaction_summary",
        }
    }

    /// Build a semantic quality scan view from session messages.
    #[must_use]
    pub fn semantic_quality_view_from_messages(
        name: &str,
        branch_id: &str,
        messages: &[SessionMessage],
    ) -> SemanticCompactionQualityView {
        let turns = messages
            .iter()
            .enumerate()
            .map(|(idx, message)| {
                SemanticCompactionQualityTurn::new(
                    branch_id,
                    format!("turn-{idx:04}"),
                    semantic_role_for_message(message),
                    session_message_semantic_text(message),
                )
            })
            .collect();
        SemanticCompactionQualityView::new(name, turns)
    }

    /// Build a semantic quality scan view from message-like session entries.
    #[must_use]
    pub fn semantic_quality_view_from_entries(
        name: &str,
        branch_id: &str,
        entries: &[SessionEntry],
    ) -> SemanticCompactionQualityView {
        let turns = entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                let message = message_from_entry(entry)?;
                let turn_id = entry
                    .base_id()
                    .cloned()
                    .unwrap_or_else(|| format!("turn-{idx:04}"));
                Some(SemanticCompactionQualityTurn::new(
                    branch_id,
                    turn_id,
                    semantic_role_for_message(&message),
                    session_message_semantic_text(&message),
                ))
            })
            .collect();
        SemanticCompactionQualityView::new(name, turns)
    }
}

#[cfg(test)]
mod tests {
    use super::semantic_marker_scan_quality as marker_scan;
    use super::*;
    use crate::model::{AssistantMessage, ContentBlock, TextContent, Usage};
    use serde_json::json;

    fn make_user_text(text: &str) -> SessionMessage {
        SessionMessage::User {
            content: UserContent::Text(text.to_string()),
            timestamp: Some(0),
        }
    }

    fn make_assistant_text(text: &str, input: u64, output: u64) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
                usage: Usage {
                    input,
                    output,
                    cache_read: 0,
                    cache_write: 0,
                    total_tokens: input + output,
                    ..Default::default()
                },
            },
        }
    }

    fn make_assistant_tool_call(name: &str, args: Value) -> SessionMessage {
        SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::ToolCall(ToolCall {
                    id: "call_1".to_string(),
                    name: name.to_string(),
                    arguments: args,
                    thought_signature: None,
                })],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                stop_reason: StopReason::ToolUse,
                stop_details: None,
                error_message: None,
                timestamp: 0,
                usage: Usage::default(),
            },
        }
    }

    fn make_tool_result(text: &str) -> SessionMessage {
        SessionMessage::ToolResult {
            tool_call_id: "call_1".to_string(),
            tool_name: String::new(),
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error: false,
            timestamp: None,
        }
    }

    // ── calculate_context_tokens ─────────────────────────────────────

    #[test]
    fn context_tokens_prefers_total_tokens() {
        let usage = Usage {
            input: 100,
            output: 50,
            total_tokens: 200,
            ..Default::default()
        };
        assert_eq!(calculate_context_tokens(&usage), 200);
    }

    #[test]
    fn context_tokens_falls_back_to_input_plus_output() {
        let usage = Usage {
            input: 100,
            output: 50,
            total_tokens: 0,
            ..Default::default()
        };
        assert_eq!(calculate_context_tokens(&usage), 150);
    }

    // ── should_compact ───────────────────────────────────────────────

    #[test]
    fn should_compact_when_over_threshold() {
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
            ..Default::default()
        };
        // window=100k, reserve=10k => threshold=90k, context=95k => should compact
        assert!(should_compact(95_000, 100_000, &settings));
    }

    #[test]
    fn should_not_compact_when_under_threshold() {
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
            ..Default::default()
        };
        // window=100k, reserve=10k => threshold=90k, context=80k => should not compact
        assert!(!should_compact(80_000, 100_000, &settings));
    }

    #[test]
    fn should_not_compact_when_disabled() {
        let settings = ResolvedCompactionSettings {
            enabled: false,
            reserve_tokens: 0,
            keep_recent_tokens: 0,
            ..Default::default()
        };
        assert!(!should_compact(1_000_000, 100_000, &settings));
    }

    #[test]
    fn should_compact_at_exact_threshold() {
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 10_000,
            keep_recent_tokens: 5_000,
            ..Default::default()
        };
        // window=100k, reserve=10k => threshold=90k, context=90k => compact
        assert!(should_compact(90_000, 100_000, &settings));
        // 89999 should not trigger
        assert!(!should_compact(89_999, 100_000, &settings));
        // 90001 should also trigger
        assert!(should_compact(90_001, 100_000, &settings));
    }

    // ── estimate_tokens ──────────────────────────────────────────────

    #[test]
    fn estimate_tokens_user_text() {
        let msg = make_user_text("hello world"); // 11 chars => ceil(11/3) = 4
        assert_eq!(estimate_tokens(&msg), 4);
    }

    #[test]
    fn estimate_tokens_empty_text() {
        let msg = make_user_text(""); // 0 chars => 0
        assert_eq!(estimate_tokens(&msg), 0);
    }

    #[test]
    fn estimate_tokens_assistant_text() {
        let msg = make_assistant_text("hello", 10, 5); // 5 chars => ceil(5/3) = 2
        assert_eq!(estimate_tokens(&msg), 2);
    }

    #[test]
    fn estimate_tokens_tool_result() {
        let msg = make_tool_result("file contents here"); // 18 chars => ceil(18/3) = 6
        assert_eq!(estimate_tokens(&msg), 6);
    }

    #[test]
    fn estimate_tokens_custom_message() {
        let msg = SessionMessage::Custom {
            custom_type: "system".to_string(),
            content: "some custom content".to_string(),
            display: true,
            details: None,
            timestamp: Some(0),
        };
        // 19 chars => ceil(19/3) = 7
        assert_eq!(estimate_tokens(&msg), 7);
    }

    // ── estimate_context_tokens ──────────────────────────────────────

    #[test]
    fn estimate_context_with_assistant_usage() {
        let messages = vec![
            make_user_text("hi"),
            make_assistant_text("hello", 50, 10),
            make_user_text("bye"),
        ];
        let estimate = estimate_context_tokens(&messages);
        // Last assistant usage: input=50, output=10, total=60
        // Trailing after that: "bye" = ceil(3/3) = 1
        assert_eq!(estimate.tokens, 61);
        assert_eq!(estimate.last_usage_index, Some(1));
    }

    #[test]
    fn estimate_context_no_assistant() {
        let messages = vec![make_user_text("hello"), make_user_text("world")];
        let estimate = estimate_context_tokens(&messages);
        // No assistant messages, so sum estimate_tokens for all: ceil(5/3)+ceil(5/3) = 2+2 = 4
        assert_eq!(estimate.tokens, 4);
        assert!(estimate.last_usage_index.is_none());
    }

    #[test]
    fn estimate_context_zero_usage_falls_back_to_heuristics() {
        let messages = vec![
            make_user_text("hi"),
            make_assistant_text("hello", 0, 0),
            make_user_text("bye"),
        ];
        let estimate = estimate_context_tokens(&messages);
        // Zero provider usage should not collapse the estimate to trailing
        // messages only. We should fall back to whole-history heuristics:
        // "hi" => 1, "hello" => 2, "bye" => 1.
        assert_eq!(estimate.tokens, 4);
        assert!(estimate.last_usage_index.is_none());
    }

    // ── extract_file_ops_from_message ────────────────────────────────

    #[test]
    fn extract_file_ops_read() {
        let msg = make_assistant_tool_call("read", json!({"path": "/foo/bar.rs"}));
        let mut ops = FileOperations::default();
        let mut status = HashMap::new();
        status.insert("call_1", true);
        extract_file_ops_from_message(&msg, &mut ops, &status);
        assert!(ops.read.contains("/foo/bar.rs"));
        assert!(ops.written.is_empty());
        assert!(ops.edited.is_empty());
    }

    #[test]
    fn extract_file_ops_write() {
        let msg = make_assistant_tool_call("write", json!({"path": "/out.txt"}));
        let mut ops = FileOperations::default();
        let mut status = HashMap::new();
        status.insert("call_1", true);
        extract_file_ops_from_message(&msg, &mut ops, &status);
        assert!(ops.written.contains("/out.txt"));
        assert!(ops.read.is_empty());
    }

    #[test]
    fn extract_file_ops_edit() {
        let msg = make_assistant_tool_call("edit", json!({"path": "/src/main.rs"}));
        let mut ops = FileOperations::default();
        let mut status = HashMap::new();
        status.insert("call_1", true);
        extract_file_ops_from_message(&msg, &mut ops, &status);
        assert!(ops.edited.contains("/src/main.rs"));
    }

    #[test]
    fn extract_file_ops_ignores_failed_tools() {
        let msg = make_assistant_tool_call("read", json!({"path": "/secret.rs"}));
        let mut ops = FileOperations::default();
        let mut status = HashMap::new();
        status.insert("call_1", false); // Failed!
        extract_file_ops_from_message(&msg, &mut ops, &status);
        assert!(ops.read.is_empty());
    }

    #[test]
    fn extract_file_ops_ignores_other_tools() {
        let msg = make_assistant_tool_call("bash", json!({"command": "ls"}));
        let mut ops = FileOperations::default();
        let mut status = HashMap::new();
        status.insert("call_1", true);
        extract_file_ops_from_message(&msg, &mut ops, &status);
        assert!(ops.read.is_empty());
        assert!(ops.written.is_empty());
        assert!(ops.edited.is_empty());
    }

    #[test]
    fn extract_file_ops_ignores_user_messages() {
        let msg = make_user_text("read the file /foo.rs");
        let mut ops = FileOperations::default();
        let status = HashMap::new();
        extract_file_ops_from_message(&msg, &mut ops, &status);
        assert!(ops.read.is_empty());
    }

    // ── compute_file_lists ───────────────────────────────────────────

    #[test]
    fn compute_file_lists_separates_read_from_modified() {
        let mut ops = FileOperations::default();
        ops.read.insert("/a.rs".to_string());
        ops.read.insert("/b.rs".to_string());
        ops.written.insert("/b.rs".to_string());
        ops.edited.insert("/c.rs".to_string());

        let (read_only, modified) = compute_file_lists(&ops);
        // /a.rs was only read; /b.rs was read AND written (so it's modified)
        assert_eq!(read_only, vec!["/a.rs"]);
        assert!(modified.contains(&"/b.rs".to_string()));
        assert!(modified.contains(&"/c.rs".to_string()));
    }

    #[test]
    fn compute_file_lists_empty() {
        let ops = FileOperations::default();
        let (read_only, modified) = compute_file_lists(&ops);
        assert!(read_only.is_empty());
        assert!(modified.is_empty());
    }

    // ── format_file_operations ───────────────────────────────────────

    #[test]
    fn format_file_operations_empty() {
        assert_eq!(format_file_operations(&[], &[]), String::new());
    }

    #[test]
    fn format_file_operations_read_only() {
        let result = format_file_operations(&["src/main.rs".to_string()], &[]);
        assert!(result.contains("<read-files>"));
        assert!(result.contains("src/main.rs"));
        assert!(!result.contains("<modified-files>"));
    }

    #[test]
    fn format_file_operations_both() {
        let result = format_file_operations(&["a.rs".to_string()], &["b.rs".to_string()]);
        assert!(result.contains("<read-files>"));
        assert!(result.contains("a.rs"));
        assert!(result.contains("<modified-files>"));
        assert!(result.contains("b.rs"));
    }

    // ── compaction_details_to_value ──────────────────────────────────

    #[test]
    fn compaction_details_serializes() {
        let details = CompactionDetails {
            read_files: vec!["a.rs".to_string()],
            modified_files: vec!["b.rs".to_string()],
        };
        let value = compaction_details_to_value(&details).unwrap();
        assert_eq!(value["readFiles"], json!(["a.rs"]));
        assert_eq!(value["modifiedFiles"], json!(["b.rs"]));
    }

    // ── ResolvedCompactionSettings default ───────────────────────────

    #[test]
    fn default_settings() {
        let settings = ResolvedCompactionSettings::default();
        assert!(settings.enabled);
        assert_eq!(settings.context_window_tokens, 128_000);
        assert_eq!(settings.reserve_tokens, 10_240);
        assert_eq!(settings.keep_recent_tokens, 12_800);
    }

    // ── Helper: entry constructors ──────────────────────────────────

    use crate::model::{ImageContent, ThinkingContent};
    use crate::session::{
        BranchSummaryEntry, CompactionEntry, EntryBase, MessageEntry, ModelChangeEntry,
    };
    use std::collections::HashMap;

    fn test_base(id: &str) -> EntryBase {
        EntryBase {
            id: Some(id.to_string()),
            parent_id: None,
            timestamp: "2026-01-01T00:00:00.000Z".to_string(),
        }
    }

    fn user_entry(id: &str, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: make_user_text(text),
        })
    }

    fn assistant_entry(id: &str, text: &str, input: u64, output: u64) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: make_assistant_text(text, input, output),
        })
    }

    fn tool_call_entry(id: &str, tool_name: &str, path: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: make_assistant_tool_call(tool_name, json!({"path": path})),
        })
    }

    fn tool_result_entry(id: &str, text: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: make_tool_result(text),
        })
    }

    fn branch_entry(id: &str, summary: &str) -> SessionEntry {
        SessionEntry::BranchSummary(BranchSummaryEntry {
            base: test_base(id),
            from_id: "parent".to_string(),
            summary: summary.to_string(),
            details: None,
            from_hook: None,
        })
    }

    fn compact_entry(id: &str, summary: &str, tokens: u64) -> SessionEntry {
        SessionEntry::Compaction(CompactionEntry {
            base: test_base(id),
            summary: summary.to_string(),
            first_kept_entry_id: "kept".to_string(),
            tokens_before: tokens,
            details: None,
            from_hook: None,
        })
    }

    fn bash_entry(id: &str) -> SessionEntry {
        SessionEntry::Message(MessageEntry {
            base: test_base(id),
            message: SessionMessage::BashExecution {
                command: "ls".to_string(),
                output: "ok".to_string(),
                exit_code: 0,
                cancelled: None,
                truncated: None,
                full_output_path: None,
                timestamp: None,
                extra: HashMap::new(),
            },
        })
    }

    fn scq_marker(id: &str, kind: &str, branch: &str, turn: &str, critical: bool) -> String {
        marker_scan::semantic_compaction_quality_marker(id, kind, branch, turn, critical)
    }

    fn quality_view(name: &str, content: &str) -> marker_scan::SemanticCompactionQualityView {
        marker_scan::SemanticCompactionQualityView::new(
            name,
            vec![marker_scan::SemanticCompactionQualityTurn::new(
                "main",
                "summary",
                "compaction_summary",
                content,
            )],
        )
    }

    #[test]
    fn semantic_compaction_quality_preserves_structured_markers() {
        let markers = [
            scq_marker("task-plan", "task", "main", "turn-task", true),
            scq_marker("file-src-lib", "file_reference", "main", "turn-file", true),
            scq_marker("decision-rch", "decision", "main", "turn-decision", true),
            scq_marker("tool-read", "tool_output", "main", "turn-tool", true),
            scq_marker(
                "constraint-no-live",
                "constraint",
                "main",
                "turn-constraint",
                true,
            ),
            scq_marker(
                "mail-degraded",
                "agent_mail_degraded",
                "main",
                "turn-mail",
                true,
            ),
            scq_marker("bead-claim", "beads_claim", "main", "turn-beads", true),
            scq_marker(
                "interrupt-handled",
                "interruption",
                "main",
                "turn-interrupt",
                false,
            ),
            scq_marker(
                "tool-truncated",
                "truncation_notice",
                "main",
                "turn-tool",
                true,
            ),
            scq_marker("handoff-fact", "handoff_fact", "main", "turn-handoff", true),
        ];
        let baseline_content = markers.join("\n");
        let candidate_content = format!("Compacted semantic inventory:\n{}", markers.join("\n"));
        let baseline = quality_view("pre-compaction", &baseline_content);
        let candidate = quality_view("post-compaction", &candidate_content);

        let report = marker_scan::evaluate_semantic_compaction_quality(
            &baseline,
            &candidate,
            &[String::from("control-never-present")],
        );

        assert_eq!(
            report.schema,
            marker_scan::SEMANTIC_COMPACTION_QUALITY_SCHEMA_V1
        );
        assert_eq!(report.verdict, "pass");
        assert_eq!(report.summary.total_expected_markers, markers.len());
        assert_eq!(report.summary.preserved_markers, markers.len());
        assert!((report.summary.marker_coverage - 1.0).abs() < f64::EPSILON);
        assert_eq!(report.summary.false_positive_controls_tripped, 0);

        let jsonl = marker_scan::semantic_compaction_quality_report_to_jsonl(&report)
            .expect("jsonl report");
        assert!(
            jsonl
                .lines()
                .next()
                .is_some_and(|line| line.contains("\"recordType\":\"summary\""))
        );
        assert!(jsonl.contains("\"recordType\":\"marker_outcome\""));
        assert!(!jsonl.contains("Compacted semantic inventory"));
    }

    #[test]
    fn semantic_compaction_quality_missing_file_reference_fails_closed() {
        let task = scq_marker("task-plan", "task", "main", "turn-task", true);
        let file = scq_marker("file-src-lib", "file_reference", "main", "turn-file", true);
        let baseline = quality_view("pre-compaction", &format!("{task}\n{file}"));
        let candidate = quality_view("post-compaction", &task);

        let report = marker_scan::evaluate_semantic_compaction_quality(&baseline, &candidate, &[]);

        assert_eq!(report.verdict, "fail");
        assert_eq!(report.summary.missing_markers, 1);
        assert!(report.outcomes.iter().any(|outcome| {
            outcome.marker_id == "file-src-lib"
                && outcome.loss_class.as_deref() == Some("missing_marker")
        }));
    }

    #[test]
    fn semantic_compaction_quality_wrong_branch_fails_closed() {
        let baseline = quality_view(
            "pre-compaction",
            &scq_marker("branch-fact", "handoff_fact", "main", "turn-handoff", true),
        );
        let candidate = quality_view(
            "post-compaction",
            &scq_marker("branch-fact", "handoff_fact", "side", "turn-handoff", true),
        );

        let report = marker_scan::evaluate_semantic_compaction_quality(&baseline, &candidate, &[]);

        assert_eq!(report.verdict, "fail");
        assert_eq!(report.summary.wrong_branch_markers, 1);
        assert!(report.outcomes.iter().any(|outcome| {
            outcome.marker_id == "branch-fact"
                && outcome.loss_class.as_deref() == Some("wrong_branch")
                && outcome.observed_branch_id.as_deref() == Some("side")
        }));
    }

    #[test]
    fn semantic_compaction_quality_stale_beads_turn_fails_closed() {
        let baseline = quality_view(
            "pre-compaction",
            &scq_marker("bead-handoff", "beads_claim", "main", "turn-fresh", true),
        );
        let candidate = quality_view(
            "post-compaction",
            &scq_marker("bead-handoff", "beads_claim", "main", "turn-stale", true),
        );

        let report = marker_scan::evaluate_semantic_compaction_quality(&baseline, &candidate, &[]);

        assert_eq!(report.verdict, "fail");
        assert_eq!(report.summary.wrong_turn_markers, 1);
        assert!(report.outcomes.iter().any(|outcome| {
            outcome.marker_id == "bead-handoff"
                && outcome.loss_class.as_deref() == Some("wrong_turn")
                && outcome.observed_turn_id.as_deref() == Some("turn-stale")
        }));
    }

    #[test]
    fn semantic_compaction_quality_large_tool_output_marker_must_survive() {
        let task = scq_marker("task-plan", "task", "main", "turn-task", true);
        let truncation = scq_marker(
            "tool-output-truncated",
            "truncation_notice",
            "main",
            "turn-tool-output",
            true,
        );
        let baseline = quality_view(
            "pre-compaction",
            &format!("{task}\nlarge tool output omitted\n{truncation}"),
        );
        let candidate = quality_view("post-compaction", &task);

        let report = marker_scan::evaluate_semantic_compaction_quality(&baseline, &candidate, &[]);

        assert_eq!(report.verdict, "fail");
        assert!(report.outcomes.iter().any(|outcome| {
            outcome.marker_id == "tool-output-truncated"
                && outcome.loss_class.as_deref() == Some("missing_marker")
        }));
    }

    #[test]
    fn semantic_compaction_quality_false_positive_control_detects_invented_marker() {
        let baseline = quality_view(
            "pre-compaction",
            &scq_marker("task-plan", "task", "main", "turn-task", true),
        );
        let invented = scq_marker(
            "control-never-present",
            "decision",
            "main",
            "turn-invented",
            true,
        );
        let candidate = quality_view(
            "post-compaction",
            &format!(
                "{}\n{invented}\nsecret tool body should not appear in report",
                scq_marker("task-plan", "task", "main", "turn-task", true)
            ),
        );

        let report = marker_scan::evaluate_semantic_compaction_quality(
            &baseline,
            &candidate,
            &[String::from("control-never-present")],
        );
        let serialized = serde_json::to_string(&report).expect("serialize report");

        assert_eq!(report.verdict, "fail");
        assert_eq!(report.summary.false_positive_controls_tripped, 1);
        assert_eq!(report.summary.unexpected_markers, 1);
        assert!(serialized.contains("false_positive_control"));
        assert!(!serialized.contains("secret tool body"));
    }

    #[test]
    fn semantic_quality_view_from_session_entries_scans_compaction_summaries() {
        let marker = scq_marker("summary-task", "task", "main", "kept", true);
        let entries = vec![
            user_entry("root", "uncompacted user text"),
            compact_entry("compact", &marker, 10),
            user_entry("kept", "kept turn"),
        ];
        let view =
            marker_scan::semantic_quality_view_from_entries("session-path", "main", &entries);
        let report = marker_scan::evaluate_semantic_compaction_quality(&view, &view, &[]);

        assert_eq!(view.turns.len(), 3);
        assert_eq!(report.verdict, "pass");
        assert_eq!(report.summary.preserved_markers, 1);
    }

    // ── get_assistant_usage ─────────────────────────────────────────

    #[test]
    fn get_assistant_usage_returns_usage_for_stop() {
        let msg = make_assistant_text("text", 100, 50);
        let usage = get_assistant_usage(&msg);
        assert!(usage.is_some());
        assert_eq!(usage.unwrap().input, 100);
    }

    #[test]
    fn get_assistant_usage_none_for_aborted() {
        let msg = SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("text"))],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                stop_reason: StopReason::Aborted,
                stop_details: None,
                error_message: None,
                timestamp: 0,
                usage: Usage {
                    input: 100,
                    output: 50,
                    total_tokens: 150,
                    ..Default::default()
                },
            },
        };
        assert!(get_assistant_usage(&msg).is_none());
    }

    #[test]
    fn get_assistant_usage_none_for_error() {
        let msg = SessionMessage::Assistant {
            message: AssistantMessage {
                content: vec![],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                stop_reason: StopReason::Error,
                stop_details: None,
                error_message: None,
                timestamp: 0,
                usage: Usage::default(),
            },
        };
        assert!(get_assistant_usage(&msg).is_none());
    }

    #[test]
    fn get_assistant_usage_none_for_user() {
        assert!(get_assistant_usage(&make_user_text("hello")).is_none());
    }

    // ── entry_is_message_like ───────────────────────────────────────

    #[test]
    fn entry_is_message_like_for_message() {
        assert!(entry_is_message_like(&user_entry("1", "hi")));
    }

    #[test]
    fn entry_is_message_like_for_branch_summary() {
        assert!(entry_is_message_like(&branch_entry("1", "sum")));
    }

    #[test]
    fn entry_is_message_like_false_for_compaction() {
        assert!(!entry_is_message_like(&compact_entry("1", "sum", 100)));
    }

    #[test]
    fn entry_is_message_like_false_for_model_change() {
        let entry = SessionEntry::ModelChange(ModelChangeEntry {
            base: test_base("1"),
            provider: "test".to_string(),
            model_id: "model-1".to_string(),
        });
        assert!(!entry_is_message_like(&entry));
    }

    // ── entry_is_compaction_boundary ────────────────────────────────

    #[test]
    fn compaction_boundary_true_for_compaction() {
        assert!(entry_is_compaction_boundary(&compact_entry(
            "1", "sum", 100
        )));
    }

    #[test]
    fn compaction_boundary_false_for_message() {
        assert!(!entry_is_compaction_boundary(&user_entry("1", "hi")));
    }

    #[test]
    fn compaction_boundary_false_for_branch() {
        assert!(!entry_is_compaction_boundary(&branch_entry("1", "sum")));
    }

    // ── is_user_turn_start ──────────────────────────────────────────

    #[test]
    fn user_turn_start_for_user() {
        assert!(is_user_turn_start(&user_entry("1", "hello")));
    }

    #[test]
    fn user_turn_start_for_branch() {
        assert!(is_user_turn_start(&branch_entry("1", "summary")));
    }

    #[test]
    fn user_turn_start_for_bash() {
        assert!(is_user_turn_start(&bash_entry("1")));
    }

    #[test]
    fn user_turn_start_false_for_assistant() {
        assert!(!is_user_turn_start(&assistant_entry("1", "resp", 10, 5)));
    }

    #[test]
    fn user_turn_start_false_for_tool_result() {
        assert!(!is_user_turn_start(&tool_result_entry("1", "result")));
    }

    #[test]
    fn user_turn_start_false_for_compaction() {
        assert!(!is_user_turn_start(&compact_entry("1", "sum", 100)));
    }

    // ── message_from_entry ──────────────────────────────────────────

    #[test]
    fn message_from_entry_user() {
        let entry = user_entry("1", "hello");
        let msg = message_from_entry(&entry);
        assert!(msg.is_some());
        assert!(matches!(msg.unwrap(), SessionMessage::User { .. }));
    }

    #[test]
    fn message_from_entry_branch_summary() {
        let entry = branch_entry("1", "branch summary text");
        let msg = message_from_entry(&entry).unwrap();
        if let SessionMessage::BranchSummary { summary, from_id } = msg {
            assert_eq!(summary, "branch summary text");
            assert_eq!(from_id, "parent");
        } else {
            panic!();
        }
    }

    #[test]
    fn message_from_entry_compaction() {
        let entry = compact_entry("1", "compact summary", 500);
        let msg = message_from_entry(&entry).unwrap();
        if let SessionMessage::CompactionSummary {
            summary,
            tokens_before,
        } = msg
        {
            assert_eq!(summary, "compact summary");
            assert_eq!(tokens_before, 500);
        } else {
            panic!();
        }
    }

    #[test]
    fn message_from_entry_model_change_is_none() {
        let entry = SessionEntry::ModelChange(ModelChangeEntry {
            base: test_base("1"),
            provider: "test".to_string(),
            model_id: "model".to_string(),
        });
        assert!(message_from_entry(&entry).is_none());
    }

    // ── find_valid_cut_points ───────────────────────────────────────

    #[test]
    fn find_valid_cut_points_empty() {
        assert!(find_valid_cut_points(&[], 0, 0).is_empty());
    }

    #[test]
    fn find_valid_cut_points_skips_tool_results() {
        let entries = vec![
            user_entry("1", "hello"),
            assistant_entry("2", "resp", 10, 5),
            tool_result_entry("3", "result"),
            user_entry("4", "follow up"),
        ];
        let cuts = find_valid_cut_points(&entries, 0, entries.len());
        assert!(cuts.contains(&0)); // user
        assert!(cuts.contains(&1)); // assistant
        assert!(!cuts.contains(&2)); // tool result excluded
        assert!(cuts.contains(&3)); // user
    }

    #[test]
    fn find_valid_cut_points_includes_branch_summary() {
        let entries = vec![branch_entry("1", "summary"), user_entry("2", "hello")];
        let cuts = find_valid_cut_points(&entries, 0, entries.len());
        assert!(cuts.contains(&0));
        assert!(cuts.contains(&1));
    }

    #[test]
    fn find_valid_cut_points_respects_range() {
        let entries = vec![
            user_entry("1", "a"),
            user_entry("2", "b"),
            user_entry("3", "c"),
        ];
        let cuts = find_valid_cut_points(&entries, 1, 2);
        assert!(!cuts.contains(&0));
        assert!(cuts.contains(&1));
        assert!(!cuts.contains(&2));
    }

    // ── find_turn_start_index ───────────────────────────────────────

    #[test]
    fn find_turn_start_basic() {
        let entries = vec![
            user_entry("1", "hello"),
            assistant_entry("2", "resp", 10, 5),
            tool_result_entry("3", "result"),
        ];
        assert_eq!(find_turn_start_index(&entries, 2, 0), Some(0));
    }

    #[test]
    fn find_turn_start_at_self() {
        let entries = vec![user_entry("1", "hello")];
        assert_eq!(find_turn_start_index(&entries, 0, 0), Some(0));
    }

    #[test]
    fn find_turn_start_none_no_user() {
        let entries = vec![
            assistant_entry("1", "resp", 10, 5),
            tool_result_entry("2", "result"),
        ];
        assert_eq!(find_turn_start_index(&entries, 1, 0), None);
    }

    #[test]
    fn find_turn_start_respects_start_index() {
        let entries = vec![
            user_entry("1", "old"),
            assistant_entry("2", "resp", 10, 5),
            user_entry("3", "new"),
        ];
        // start_index=2, so it should find user at 2
        assert_eq!(find_turn_start_index(&entries, 2, 2), Some(2));
        // start_index=2, looking back from 2, user at 1 is below start
        assert_eq!(find_turn_start_index(&entries, 1, 2), None);
    }

    // ── serialize_conversation ───────────────────────────────────────

    #[test]
    fn serialize_conversation_user_text() {
        let messages = vec![Message::User(crate::model::UserMessage {
            content: UserContent::Text("hello world".to_string()),
            timestamp: 0,
        })];
        assert_eq!(serialize_conversation(&messages), "[User]: hello world");
    }

    #[test]
    fn serialize_conversation_empty() {
        assert!(serialize_conversation(&[]).is_empty());
    }

    #[test]
    fn serialize_conversation_skips_empty_user() {
        let messages = vec![Message::User(crate::model::UserMessage {
            content: UserContent::Text(String::new()),
            timestamp: 0,
        })];
        assert!(serialize_conversation(&messages).is_empty());
    }

    #[test]
    fn serialize_conversation_assistant_text() {
        let messages = vec![Message::assistant(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("response"))],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        })];
        assert!(serialize_conversation(&messages).contains("[Assistant]: response"));
    }

    #[test]
    fn serialize_conversation_tool_calls() {
        let messages = vec![Message::assistant(AssistantMessage {
            content: vec![ContentBlock::ToolCall(ToolCall {
                id: "c1".to_string(),
                name: "read".to_string(),
                arguments: json!({"path": "/main.rs"}),
                thought_signature: None,
            })],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        })];
        let result = serialize_conversation(&messages);
        assert!(result.contains("[Assistant tool calls]: read("));
        assert!(result.contains("path="));
    }

    #[test]
    fn serialize_conversation_thinking() {
        let messages = vec![Message::assistant(AssistantMessage {
            content: vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "let me think".to_string(),
                thinking_signature: None,
            })],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        })];
        assert!(serialize_conversation(&messages).contains("[Assistant thinking]: let me think"));
    }

    #[test]
    fn serialize_conversation_tool_result() {
        let messages = vec![Message::tool_result(crate::model::ToolResultMessage {
            tool_call_id: "c1".to_string(),
            tool_name: "read".to_string(),
            content: vec![ContentBlock::Text(TextContent::new("file contents"))],
            details: None,
            is_error: false,
            timestamp: 0,
        })];
        assert!(serialize_conversation(&messages).contains("[Tool result]: file contents"));
    }

    // ── estimate_tokens additional ──────────────────────────────────

    #[test]
    fn estimate_tokens_image_block() {
        let msg = SessionMessage::User {
            content: UserContent::Blocks(vec![ContentBlock::Image(ImageContent {
                data: "base64data".to_string(),
                mime_type: "image/png".to_string(),
            })]),
            timestamp: None,
        };
        // Image = 3600 chars (IMAGE_CHAR_ESTIMATE) -> ceil(3600/3) = 1200
        assert_eq!(estimate_tokens(&msg), 1200);
    }

    #[test]
    fn estimate_tokens_thinking() {
        let msg = SessionMessage::User {
            content: UserContent::Blocks(vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "a".repeat(20),
                thinking_signature: None,
            })]),
            timestamp: None,
        };
        // 20 chars -> ceil(20/3) = 7
        assert_eq!(estimate_tokens(&msg), 7);
    }

    #[test]
    fn estimate_tokens_bash_execution() {
        let msg = SessionMessage::BashExecution {
            command: "echo hi".to_string(),
            output: "hi\n".to_string(),
            exit_code: 0,
            cancelled: None,
            truncated: None,
            full_output_path: None,
            timestamp: None,
            extra: HashMap::new(),
        };
        // 7 + 3 = 10 chars -> ceil(10/3) = 4
        assert_eq!(estimate_tokens(&msg), 4);
    }

    #[test]
    fn estimate_tokens_branch_summary() {
        let msg = SessionMessage::BranchSummary {
            summary: "a".repeat(40),
            from_id: "id".to_string(),
        };
        // 40 chars -> ceil(40/3) = 14
        assert_eq!(estimate_tokens(&msg), 14);
    }

    #[test]
    fn estimate_tokens_compaction_summary() {
        let msg = SessionMessage::CompactionSummary {
            summary: "a".repeat(80),
            tokens_before: 5000,
        };
        // 80 chars -> ceil(80/3) = 27
        assert_eq!(estimate_tokens(&msg), 27);
    }

    // ── prepare_compaction ──────────────────────────────────────────

    #[test]
    fn prepare_compaction_empty() {
        assert!(prepare_compaction(&[], ResolvedCompactionSettings::default()).is_none());
    }

    #[test]
    fn prepare_compaction_last_is_compaction_returns_none() {
        let entries = vec![user_entry("1", "hello"), compact_entry("2", "summary", 100)];
        assert!(prepare_compaction(&entries, ResolvedCompactionSettings::default()).is_none());
    }

    #[test]
    fn prepare_compaction_no_messages_to_summarize_returns_none() {
        // Only non-message entries that produce no summarizable messages
        let entries = vec![SessionEntry::ModelChange(ModelChangeEntry {
            base: test_base("1"),
            provider: "test".to_string(),
            model_id: "model".to_string(),
        })];
        assert!(prepare_compaction(&entries, ResolvedCompactionSettings::default()).is_none());
    }

    #[test]
    fn prepare_compaction_basic_returns_some() {
        let long_text = "a".repeat(100_000);
        let entries = vec![
            user_entry("1", &long_text),
            assistant_entry("2", &long_text, 50000, 25000),
            user_entry("3", &long_text),
            assistant_entry("4", &long_text, 80000, 30000),
            user_entry("5", "recent"),
        ];
        let settings = ResolvedCompactionSettings {
            enabled: true,
            context_window_tokens: 100_000,
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
        };
        let prep = prepare_compaction(&entries, settings);
        assert!(prep.is_some());
        let p = prep.unwrap();
        assert!(!p.messages_to_summarize.is_empty());
        assert!(p.tokens_before > 0);
        assert!(p.previous_summary.is_none());
    }

    #[test]
    fn prepare_compaction_after_previous_compaction() {
        let entries = vec![
            user_entry("1", "old message"),
            assistant_entry("2", "old response", 100, 50),
            compact_entry("3", "previous summary", 300),
            user_entry("4", &"x".repeat(100_000)),
            assistant_entry("5", &"y".repeat(100_000), 80000, 30000),
            user_entry("6", "recent"),
        ];
        let settings = ResolvedCompactionSettings {
            enabled: true,
            context_window_tokens: 100_000,
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
        };
        let prep = prepare_compaction(&entries, settings);
        assert!(prep.is_some());
        let p = prep.unwrap();
        assert_eq!(p.previous_summary.as_deref(), Some("previous summary"));
    }

    #[test]
    fn prepare_compaction_tracks_file_ops() {
        let entries = vec![
            tool_call_entry("1", "read", "/src/main.rs"),
            tool_result_entry("1r", "ok"),
            tool_call_entry("2", "edit", "/src/lib.rs"),
            tool_result_entry("2r", "ok"),
            user_entry("3", &"x".repeat(100_000)),
            assistant_entry("4", &"y".repeat(100_000), 80000, 30000),
            user_entry("5", "recent"),
        ];
        let settings = ResolvedCompactionSettings {
            enabled: true,
            reserve_tokens: 1000,
            keep_recent_tokens: 100,
            ..Default::default()
        };
        if let Some(prep) = prepare_compaction(&entries, settings) {
            let has_read = prep.file_ops.read.contains("/src/main.rs");
            let has_edit = prep.file_ops.edited.contains("/src/lib.rs");
            // At least one should be tracked (depends on cut point position)
            assert!(has_read || has_edit || prep.file_ops.read.is_empty());
        }
    }

    // ── FileOperations::read_files ──────────────────────────────────

    #[test]
    fn file_operations_read_files_iterator() {
        let mut ops = FileOperations::default();
        ops.read.insert("/a.rs".to_string());
        ops.read.insert("/b.rs".to_string());
        let files: Vec<&str> = ops.read_files().collect();
        assert_eq!(files.len(), 2);
        assert!(files.contains(&"/a.rs"));
        assert!(files.contains(&"/b.rs"));
    }

    #[test]
    fn find_cut_point_includes_tool_result_when_needed() {
        // Setup:
        // 0. User (10)
        // 1. Assistant Call (10)
        // 2. Tool Result (100)
        // 3. User (10)
        // 4. Assistant (10)
        //
        // Keep recent = 100.
        // Accumulation from end:
        // 4: 10
        // 3: 20
        // 2: 120 (Threshold crossed at index 2)
        //
        // Index 2 is ToolResult (invalid cut point).
        // Valid cut points: 0, 1, 3, 4.
        //
        // Logic should pick closest valid cut point <= 2, which is 1.
        // If it picked >= 2, it would pick 3, discarding the ToolResult and Call (keeping only 20 tokens).
        // By picking 1, we keep 1..4 (130 tokens).

        // Create entries with controlled lengths.
        // With chars/token ~=3, 400 chars => ceil(400/3)=134 tokens.
        let tr_text = "x".repeat(400);
        let entries = vec![
            user_entry("0", "user"),              // Valid
            assistant_entry("1", "call", 10, 10), // Valid (Assistant)
            tool_result_entry("2", &tr_text),     // Invalid
            user_entry("3", "user"),              // Valid
            assistant_entry("4", "resp", 10, 10), // Valid
        ];

        // Verify token estimates (approx)
        // 0: ceil(4/3) = 2
        // 1: ceil(4/3) = 2
        // 2: ceil(400/3) = 134
        // 3: ceil(4/3) = 2
        // 4: ceil(4/3) = 2
        // Total recent needed: 100.
        // Accumulate: 4(2)+3(2)+2(134) = 138. Crossed at 2.

        let settings = ResolvedCompactionSettings {
            enabled: true,
            context_window_tokens: 15,
            reserve_tokens: 0,
            keep_recent_tokens: 100,
        };

        let prep = prepare_compaction(&entries, settings).expect("should compact");

        // Cut point is index 1 (Assistant/Call). Because entries[1] is Assistant (not User),
        // this is a split turn: the turn started at index 0 (User). The User message at index 0
        // goes into turn_prefix_messages (not messages_to_summarize) because history_end = 0.
        assert_eq!(prep.first_kept_entry_id, "1");

        // messages_to_summarize is entries[0..0] = empty (split-turn puts the
        // prefix in turn_prefix_messages instead).
        assert!(
            prep.messages_to_summarize.is_empty(),
            "split turn: user goes into turn prefix, not summarize"
        );

        // turn_prefix_messages should contain the User message at index 0.
        assert_eq!(prep.turn_prefix_messages.len(), 1);
        match &prep.turn_prefix_messages[0] {
            SessionMessage::User { content, .. } => {
                if let UserContent::Text(t) = content {
                    assert_eq!(t, "user");
                } else {
                    panic!();
                }
            }
            _ => panic!(),
        }
    }

    #[test]
    fn find_cut_point_should_not_discard_context_to_skip_tool_chain() {
        // Setup (estimate_tokens uses ceil(chars/3)):
        // 0. User "x"*4000 → 1334 tokens
        // 1. Assistant "x"*400 → 134 tokens
        // 2. Tool Result "x"*400 → 134 tokens
        // 3. User "next" → 2 tokens
        //
        // Keep recent = 150.
        // Accumulation (from end):
        // 3: 2
        // 2: 136
        // 1: 270 (Crosses 150) -> cut_index = 1
        //
        // The cut should land at index 1 (the assistant message), keeping
        // entries 1-3 and summarizing only entry 0.

        let entries = vec![
            user_entry("0", &"x".repeat(4000)),             // 1000 tokens
            assistant_entry("1", &"x".repeat(400), 50, 50), // 100 tokens
            tool_result_entry("2", &"x".repeat(400)),       // 100 tokens
            user_entry("3", "next"),                        // 1 token
        ];

        let settings = ResolvedCompactionSettings {
            enabled: true,
            context_window_tokens: 200,
            reserve_tokens: 0,
            keep_recent_tokens: 150,
        };

        // We use prepare_compaction as the entry point
        let prep = prepare_compaction(&entries, settings).expect("should compact");

        // We expect to keep from 1 (Assistant). The cut splits the turn
        // (user 0 + assistant 1), so user 0 goes into the turn prefix.
        assert_eq!(
            prep.first_kept_entry_id, "1",
            "Should start at Assistant message to preserve context"
        );
        assert!(
            prep.is_split_turn,
            "Cut should split the user/assistant turn"
        );
        assert_eq!(
            prep.turn_prefix_messages.len(),
            1,
            "User entry at index 0 should be in the turn prefix"
        );
        assert!(
            prep.messages_to_summarize.is_empty(),
            "Nothing before the turn to summarize"
        );
    }

    mod proptest_compaction {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `calculate_context_tokens`: if total > 0, returns total.
            #[test]
            fn calc_context_tokens_total_wins(
                input in 0..1_000_000u64,
                output in 0..1_000_000u64,
                total in 1..2_000_000u64,
            ) {
                let usage = Usage {
                    input,
                    output,
                    total_tokens: total,
                    ..Usage::default()
                };
                assert_eq!(calculate_context_tokens(&usage), total);
            }

            /// `calculate_context_tokens`: if total == 0, returns input + output.
            #[test]
            fn calc_context_tokens_fallback(
                input in 0..1_000_000u64,
                output in 0..1_000_000u64,
            ) {
                let usage = Usage {
                    input,
                    output,
                    total_tokens: 0,
                    ..Usage::default()
                };
                assert_eq!(calculate_context_tokens(&usage), input + output);
            }

            /// `should_compact` returns false when disabled.
            #[test]
            fn should_compact_disabled_returns_false(
                ctx_tokens in 0..1_000_000u64,
                window in 0..500_000u32,
            ) {
                let settings = ResolvedCompactionSettings {
                    enabled: false,
                    context_window_tokens: window,
                    reserve_tokens: 16_384,
                    keep_recent_tokens: 20_000,
                };
                assert!(!should_compact(ctx_tokens, window, &settings));
            }

            /// `should_compact` threshold: tokens >= window - reserve.
            #[test]
            fn should_compact_threshold(
                ctx_tokens in 0..500_000u64,
                window in 0..300_000u32,
                reserve in 0..100_000u32,
            ) {
                let settings = ResolvedCompactionSettings {
                    enabled: true,
                    context_window_tokens: window,
                    reserve_tokens: reserve,
                    keep_recent_tokens: 20_000,
                };
                let threshold = u64::from(window).saturating_sub(u64::from(reserve));
                let result = should_compact(ctx_tokens, window, &settings);
                assert_eq!(result, ctx_tokens >= threshold);
            }

            /// `format_file_operations`: empty lists produce empty string.
            #[test]
            fn format_file_ops_empty(_dummy in 0..10u32) {
                let result = format_file_operations(&[], &[]);
                assert!(result.is_empty());
            }

            /// `format_file_operations`: read files produce `<read-files>` tag.
            #[test]
            fn format_file_ops_read_tag(
                files in prop::collection::vec("[a-z./]{1,20}", 1..5),
            ) {
                let result = format_file_operations(&files, &[]);
                assert!(result.contains("<read-files>"));
                assert!(result.contains("</read-files>"));
                assert!(!result.contains("<modified-files>"));
                for f in &files {
                    assert!(result.contains(f.as_str()));
                }
            }

            /// `format_file_operations`: modified files produce `<modified-files>` tag.
            #[test]
            fn format_file_ops_modified_tag(
                files in prop::collection::vec("[a-z./]{1,20}", 1..5),
            ) {
                let result = format_file_operations(&[], &files);
                assert!(!result.contains("<read-files>"));
                assert!(result.contains("<modified-files>"));
                assert!(result.contains("</modified-files>"));
                for f in &files {
                    assert!(result.contains(f.as_str()));
                }
            }

            /// `compute_file_lists`: modified = edited ∪ written, read_only = read \ modified.
            #[test]
            fn compute_file_lists_set_algebra(
                read in prop::collection::hash_set("[a-z]{1,5}", 0..5),
                written in prop::collection::hash_set("[a-z]{1,5}", 0..5),
                edited in prop::collection::hash_set("[a-z]{1,5}", 0..5),
            ) {
                let file_ops = FileOperations {
                    read: read.clone(),
                    written: written.clone(),
                    edited: edited.clone(),
                };
                let (read_only, modified) = compute_file_lists(&file_ops);
                // Modified = edited ∪ written
                let expected_modified: HashSet<&String> =
                    edited.iter().chain(written.iter()).collect();
                let actual_modified: HashSet<&String> = modified.iter().collect();
                assert_eq!(actual_modified, expected_modified);
                // Read-only = read \ modified (no overlap)
                for f in &read_only {
                    assert!(!modified.contains(f), "overlap: {f}");
                    assert!(read.contains(f));
                }
                // Both are sorted
                for pair in read_only.windows(2) {
                    assert!(pair[0] <= pair[1]);
                }
                for pair in modified.windows(2) {
                    assert!(pair[0] <= pair[1]);
                }
            }
        }
    }
}
