//! The advisor (bd-cv653.3.3): a second model that reviews each agent turn
//! and injects notes inline — a quiet aside, a concern, or a hard blocker.
//!
//! It runs on its own provider and its own context, so it catches what the
//! doer rushed past. Design rules from the bead:
//! - Zero overhead when unconfigured (no digest is even built).
//! - Failure isolation: advisor errors NEVER fail the main turn; 3
//!   consecutive failures disable the advisor with a user-visible notice.
//! - Emission guard: rate-limited, deduped, and silent on trivial turns.
//! - Stack layering: this module emits structured verdicts; rendering is the
//!   transcript card registry's job (bd-cv653.9.2) — no bespoke painting here.

use crate::model::Message;
use crate::provider::Provider;
use crate::text_completion::{
    MAX_INPUT_BYTES, MAX_TEXT_BYTES, OMITTED_INPUT, RequestStop, collect_text, redact_inputs,
    with_timeout,
};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// Advisor severity levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictLevel {
    Note,
    Concern,
    Blocker,
}

impl VerdictLevel {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Concern => "concern",
            Self::Blocker => "blocker",
        }
    }
}

/// A parsed advisor verdict.
#[derive(Debug, Clone)]
pub struct AdvisorVerdict {
    pub level: VerdictLevel,
    pub rationale: String,
}

/// The compact turn digest sent to the advisor (budget-capped).
#[derive(Debug, Default)]
pub struct TurnDigest {
    pub files_touched: Vec<String>,
    pub commands_run: Vec<String>,
    pub tool_errors: Vec<String>,
    pub final_text: String,
    pub tool_call_count: usize,
    pub is_trivial: bool,
}

impl TurnDigest {
    /// A turn is trivial when no tools ran and the reply is short — nothing
    /// worth an advisor's attention (or tokens).
    #[must_use]
    pub const fn is_trivial(&self) -> bool {
        self.is_trivial
    }
}

const MAX_DIGEST_FILES: usize = 25;
const MAX_DIGEST_COMMANDS: usize = 15;
const MAX_DIGEST_ERRORS: usize = 10;
const MAX_FINAL_TEXT_CHARS: usize = 2_000;
const MAX_DIGEST_PATH_CHARS: usize = 1_024;
// Reserve enough room to name an omission in every field. The retained raw
// fields plus those markers always fit the shared privacy scanner's budget.
const MAX_RAW_DIGEST_BYTES: usize = MAX_INPUT_BYTES
    - (MAX_DIGEST_FILES + MAX_DIGEST_COMMANDS + MAX_DIGEST_ERRORS + 1) * OMITTED_INPUT.len();

fn retain_digest_text(text: &str, remaining: &mut usize) -> String {
    if text.len() > *remaining {
        return OMITTED_INPUT.to_string();
    }
    *remaining -= text.len();
    text.to_string()
}

fn retain_error_text(result: &crate::model::ToolResultMessage, remaining: &mut usize) -> String {
    let mut output = String::new();
    let mut first = true;
    for block in &result.content {
        let crate::model::ContentBlock::Text(text) = block else {
            continue;
        };
        let next = output
            .len()
            .checked_add(usize::from(!first))
            .and_then(|bytes| bytes.checked_add(text.text.len()));
        if !next.is_some_and(|bytes| bytes <= *remaining) {
            // Never retain a partial private-key envelope or an uninspected
            // prefix when a later block makes the whole field too large.
            return OMITTED_INPUT.to_string();
        }
        if !first {
            output.push(' ');
        }
        output.push_str(&text.text);
        first = false;
    }
    *remaining -= output.len();
    output
}

/// Screen all fields together before applying presentation limits. This is
/// also the provider-admission boundary for SDK-supplied TurnDigest values;
/// callers cannot bypass it by avoiding build_digest.
fn screen_digest(digest: &TurnDigest) -> crate::error::Result<TurnDigest> {
    if digest.files_touched.len() > MAX_DIGEST_FILES
        || digest.commands_run.len() > MAX_DIGEST_COMMANDS
        || digest.tool_errors.len() > MAX_DIGEST_ERRORS
    {
        return Err(crate::error::Error::validation(
            "PI_ADVISOR_DIGEST_LIMIT: too many fields in advisor digest",
        ));
    }
    let inputs: Vec<&str> = digest
        .files_touched
        .iter()
        .chain(&digest.commands_run)
        .chain(&digest.tool_errors)
        .chain(std::iter::once(&digest.final_text))
        .map(String::as_str)
        .collect();
    let protected = redact_inputs(&inputs)?;
    if protected.len() != inputs.len() {
        return Err(crate::error::Error::validation(
            "advisor privacy projection changed field count",
        ));
    }
    let mut protected = protected.into_iter();
    let files_touched = protected
        .by_ref()
        .take(digest.files_touched.len())
        .map(|text| text.chars().take(MAX_DIGEST_PATH_CHARS).collect())
        .collect();
    let commands_run = protected
        .by_ref()
        .take(digest.commands_run.len())
        .map(|text| text.chars().take(200).collect())
        .collect();
    let tool_errors = protected
        .by_ref()
        .take(digest.tool_errors.len())
        .map(|text| text.chars().take(200).collect())
        .collect();
    let final_text = protected
        .next()
        .ok_or_else(|| crate::error::Error::validation("advisor privacy projection lost final text"))?
        .chars()
        .take(MAX_FINAL_TEXT_CHARS)
        .collect();
    Ok(TurnDigest {
        files_touched,
        commands_run,
        tool_errors,
        final_text,
        tool_call_count: digest.tool_call_count,
        is_trivial: digest.is_trivial,
    })
}

/// Build the digest from the tail of the conversation (last user message
/// onward), budgeted. Complete selected fields are screened for built-in
/// credential shapes before clipping; tool-free advisor projections never
/// need reversible credentials. The source conversation is not modified.
#[must_use]
pub fn build_digest(messages: &[Message]) -> TurnDigest {
    // Start from the last user message (turn boundary).
    let boundary = messages
        .iter()
        .rposition(|m| matches!(m, Message::User(_)))
        .unwrap_or(0);
    let tail = &messages[boundary..];

    let mut digest = TurnDigest::default();
    let mut remaining = MAX_RAW_DIGEST_BYTES;
    let mut seen_files = HashSet::new();
    let mut final_text = "";
    for message in tail {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    match block {
                        crate::model::ContentBlock::ToolCall(call) => {
                            digest.tool_call_count = digest.tool_call_count.saturating_add(1);
                            match call.name.as_str() {
                                "write" | "edit" | "hashline_edit" | "ast_edit" => {
                                    if let Some(path) =
                                        call.arguments.get("path").and_then(Value::as_str)
                                        && digest.files_touched.len() < MAX_DIGEST_FILES
                                        && seen_files.insert(path)
                                    {
                                        digest.files_touched.push(retain_digest_text(path, &mut remaining));
                                    }
                                }
                                "bash" => {
                                    if let Some(command) =
                                        call.arguments.get("command").and_then(Value::as_str)
                                        && digest.commands_run.len() < MAX_DIGEST_COMMANDS
                                    {
                                        digest.commands_run.push(retain_digest_text(command, &mut remaining));
                                    }
                                }
                                _ => {}
                            }
                        }
                        crate::model::ContentBlock::Text(text) => {
                            // Keep only a borrow until the final block is known;
                            // discarded drafts must not consume the scan budget.
                            final_text = &text.text;
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result)
                if result.is_error && digest.tool_errors.len() < MAX_DIGEST_ERRORS =>
            {
                digest.tool_errors.push(retain_error_text(result, &mut remaining));
            }
            _ => {}
        }
    }
    // Classification describes the source turn, not a short redaction marker.
    digest.is_trivial = digest.tool_call_count == 0 && final_text.len() < 400;
    digest.final_text = retain_digest_text(final_text, &mut remaining);
    match screen_digest(&digest) {
        Ok(protected) => protected,
        Err(_) => {
            tracing::warn!(
                event = "pi.advisor.privacy_projection_refused",
                "Advisor digest omitted because its privacy projection failed"
            );
            TurnDigest {
                final_text: OMITTED_INPUT.to_string(),
                tool_call_count: digest.tool_call_count,
                is_trivial: digest.is_trivial,
                ..Default::default()
            }
        }
    }
}

/// The review rubric prompt.
pub const REVIEW_SYSTEM_PROMPT: &str = "You are the advisor: a senior reviewer watching another agent's turn. \
Review the digest and answer with EXACTLY one verdict line, then a short rationale.\n\
Line 1 must be one of: NOTE, CONCERN, BLOCKER.\n\
- NOTE: fine, or a minor observation worth one sentence.\n\
- CONCERN: something likely wrong/risky the doer should fix before continuing.\n\
- BLOCKER: a hard problem (data loss risk, broken build, wrong target) that must stop work until addressed.\n\
Be terse. Cite file paths when relevant. No markdown, no headers.";

/// Render the digest as the advisor's user prompt. This is formatting only;
/// the runtime screens the typed fields before calling it.
#[must_use]
pub fn digest_prompt(digest: &TurnDigest) -> String {
    let mut out = String::from("Turn digest:\n");
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!("tool calls: {}\n", digest.tool_call_count),
    );
    if !digest.files_touched.is_empty() {
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!("files touched: {}\n", digest.files_touched.join(", ")),
        );
    }
    if !digest.commands_run.is_empty() {
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!("commands run:\n{}\n", digest.commands_run.join("\n")),
        );
    }
    if !digest.tool_errors.is_empty() {
        let _ = std::fmt::Write::write_fmt(
            &mut out,
            format_args!("tool errors:\n{}\n", digest.tool_errors.join("\n")),
        );
    }
    let _ = std::fmt::Write::write_fmt(
        &mut out,
        format_args!("final message:\n{}\n", digest.final_text),
    );
    out
}

/// Parse the advisor's reply into a verdict. Unparseable replies degrade to
/// NOTE (never blocker-by-accident).
#[must_use]
pub fn parse_verdict(reply: &str) -> AdvisorVerdict {
    let trimmed = reply.trim();
    let first_line = trimmed.lines().next().unwrap_or("");
    let upper = first_line
        .trim()
        .trim_matches([':', '.', '*', '#', ' '])
        .to_ascii_uppercase();
    let level = if upper.starts_with("BLOCKER") {
        VerdictLevel::Blocker
    } else if upper.starts_with("CONCERN") {
        VerdictLevel::Concern
    } else {
        VerdictLevel::Note
    };
    let rationale: String = if matches!(level, VerdictLevel::Note) && !upper.starts_with("NOTE") {
        // No protocol line at all: the whole reply is the note.
        trimmed.chars().take(600).collect()
    } else {
        trimmed
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .chars()
            .take(600)
            .collect()
    };
    AdvisorVerdict {
        level,
        rationale: if rationale.is_empty() {
            first_line
                .trim()
                .trim_start_matches(|c: char| c.is_ascii_uppercase() || c == ' ')
                .trim_start_matches([':', '-', ' '])
                .chars()
                .take(600)
                .collect()
        } else {
            rationale
        },
    }
}

/// Rate/dedupe guard.
#[derive(Debug)]
pub struct EmissionGuard {
    pub max_per_window: usize,
    pub window_turns: usize,
    notes_in_window: usize,
    window_start_turn: u64,
    seen_hashes: HashSet<u64>,
}

impl EmissionGuard {
    #[must_use]
    pub fn new(max_per_window: usize, window_turns: usize) -> Self {
        Self {
            max_per_window,
            window_turns,
            notes_in_window: 0,
            window_start_turn: 0,
            seen_hashes: HashSet::new(),
        }
    }

    /// Whether this verdict may emit at `turn_index`.
    pub fn allow(&mut self, verdict: &AdvisorVerdict, turn_index: u64) -> bool {
        if turn_index >= self.window_start_turn + self.window_turns as u64 {
            self.window_start_turn = turn_index;
            self.notes_in_window = 0;
            self.seen_hashes.clear();
        }
        // Blockers always emit (they gate work); notes/concerns are limited.
        if verdict.level != VerdictLevel::Blocker {
            if self.notes_in_window >= self.max_per_window {
                return false;
            }
            let hash = fnv1a(&verdict.rationale);
            if !self.seen_hashes.insert(hash) {
                return false; // repeated rationale
            }
        }
        self.notes_in_window += 1;
        true
    }
}

fn fnv1a(text: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Format the injection text delivered into the next turn.
#[must_use]
pub fn format_injection(verdict: &AdvisorVerdict) -> String {
    let tag = match verdict.level {
        VerdictLevel::Note => "ADVISOR:NOTE",
        VerdictLevel::Concern => "ADVISOR:CONCERN",
        VerdictLevel::Blocker => "ADVISOR:BLOCKER",
    };
    format!("[{tag}] {}", verdict.rationale)
}

/// Process-global advisor pause flag (bd-cv653.3.3): pi runs one session per
/// process, so /advisor pause|resume flips this single flag.
pub static ADVISOR_PAUSED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// What a session needs to build its own [`AdvisorRuntime`]: hosts that
/// replace sessions (`/new`, `/resume`, `/fork`) keep this and build a fresh
/// runtime per session.
#[derive(Clone)]
pub struct AdvisorOptions {
    pub provider: Arc<dyn Provider>,
    pub label: String,
    pub timeout: Duration,
    pub api_key: Option<String>,
}

impl AdvisorOptions {
    #[must_use]
    pub fn runtime(&self) -> AdvisorRuntime {
        AdvisorRuntime::new(Arc::clone(&self.provider), self.label.clone())
            .with_timeout(self.timeout)
            .with_api_key(self.api_key.clone())
    }
}

/// The advisor runtime: owns the second provider, the guard, and failure
/// isolation state.
pub struct AdvisorRuntime {
    provider: Arc<dyn Provider>,
    label: String,
    timeout: Duration,
    guard: EmissionGuard,
    consecutive_failures: u32,
    /// Resolved credential for the advisor's provider; forwarded on every
    /// review call so keyed providers authenticate exactly like the doer.
    api_key: Option<String>,
    /// One-shot user notice. Consuming it does not clear the disabled state.
    pub disabled_notice: Option<String>,
}

/// What a review returned.
#[derive(Debug)]
pub enum AdvisorOutcome {
    /// A verdict worth injecting.
    Inject(AdvisorVerdict),
    /// Suppressed (trivial turn, guard, owner cancellation, or no useful note).
    Quiet,
    /// The advisor failed (isolated; counted toward the disable threshold).
    Failed,
}

const MAX_CONSECUTIVE_FAILURES: u32 = 3;

impl AdvisorRuntime {
    #[must_use]
    pub fn new(provider: Arc<dyn Provider>, label: String) -> Self {
        Self {
            provider,
            label,
            timeout: Duration::from_secs(15),
            guard: EmissionGuard::new(3, 10),
            consecutive_failures: 0,
            api_key: None,
            disabled_notice: None,
        }
    }

    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key;
        self
    }

    #[must_use]
    pub fn with_guard(mut self, max_per_window: usize, window_turns: usize) -> Self {
        self.guard = EmissionGuard::new(max_per_window, window_turns);
        self
    }

    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES
    }

    /// Review one turn. Never fails the caller. Built-in credential shapes
    /// are always redacted for this tool-free secondary model, independently
    /// of the primary agent's configurable reversible secret mode.
    /// Owner cancellation suppresses the verdict without changing the failure
    /// streak or emission guard; it is not evidence of a broken provider.
    pub async fn review_turn(&mut self, digest: &TurnDigest, turn_index: u64) -> AdvisorOutcome {
        if self.is_disabled() || ADVISOR_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
            return AdvisorOutcome::Quiet;
        }
        if digest.is_trivial() {
            return AdvisorOutcome::Quiet;
        }
        let call = self.call_advisor(digest);
        let reply = match with_timeout(self.timeout, call).await {
            Ok(Ok(reply)) => reply,
            Err(RequestStop::Cancelled) => return AdvisorOutcome::Quiet,
            Ok(Err(_)) | Err(RequestStop::TimedOut | RequestStop::TimeUnavailable) => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    self.disabled_notice = Some(format!(
                        "advisor ({}) disabled after {} consecutive failures",
                        self.label, self.consecutive_failures
                    ));
                }
                return AdvisorOutcome::Failed;
            }
        };
        self.consecutive_failures = 0;
        let verdict = parse_verdict(&reply);
        if verdict.level == VerdictLevel::Note && verdict.rationale.len() < 12 {
            return AdvisorOutcome::Quiet; // "looks fine" notes add noise
        }
        if !self.guard.allow(&verdict, turn_index) {
            return AdvisorOutcome::Quiet;
        }
        AdvisorOutcome::Inject(verdict)
    }

    async fn call_advisor(&self, digest: &TurnDigest) -> crate::error::Result<String> {
        let digest = screen_digest(digest)?;
        let context = crate::provider::Context {
            system_prompt: Some(REVIEW_SYSTEM_PROMPT.to_string().into()),
            messages: vec![crate::model::Message::User(crate::model::UserMessage {
                content: crate::model::UserContent::Text(digest_prompt(&digest)),
                timestamp: chrono::Utc::now().timestamp_millis(),
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = crate::provider::StreamOptions {
            max_tokens: Some(512),
            api_key: self.api_key.clone(),
            ..Default::default()
        };
        let stream = self.provider.stream(&context, &options).await?;
        collect_text(stream, MAX_TEXT_BYTES).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_levels() {
        let blocker =
            parse_verdict("BLOCKER: the edit deletes the migration\nIt removes drop columns.");
        assert_eq!(blocker.level, VerdictLevel::Blocker);
        assert!(blocker.rationale.contains("drop columns"));
        let concern = parse_verdict("CONCERN: broad catch\nThis swallows IO errors.");
        assert_eq!(concern.level, VerdictLevel::Concern);
        assert!(concern.rationale.contains("IO errors"));
        let note = parse_verdict("NOTE: fine\nLooks right.");
        assert_eq!(note.level, VerdictLevel::Note);
        // No protocol line → whole reply becomes a note (never a blocker).
        let free = parse_verdict("seems okay to me");
        assert_eq!(free.level, VerdictLevel::Note);
        assert!(free.rationale.contains("seems okay"));
    }

    #[test]
    fn guard_rate_limits_and_dedupes() {
        let mut guard = EmissionGuard::new(2, 5);
        let concern = || AdvisorVerdict {
            level: VerdictLevel::Concern,
            rationale: "same concern".to_string(),
        };
        assert!(guard.allow(&concern(), 0));
        assert!(
            !guard.allow(&concern(), 1),
            "dedupe: same rationale blocked"
        );
        let other = AdvisorVerdict {
            level: VerdictLevel::Concern,
            rationale: "different concern".to_string(),
        };
        assert!(guard.allow(&other, 2));
        let third = AdvisorVerdict {
            level: VerdictLevel::Concern,
            rationale: "third distinct concern".to_string(),
        };
        assert!(!guard.allow(&third, 3), "rate limit: 2 per 5-turn window");
        // New window resets.
        assert!(guard.allow(&third, 6));
        // Blockers always emit.
        let blocker = AdvisorVerdict {
            level: VerdictLevel::Blocker,
            rationale: "stop".to_string(),
        };
        assert!(guard.allow(&blocker, 3));
    }

    #[test]
    fn digest_collects_files_commands_errors_and_text() {
        let messages = vec![
            Message::User(crate::model::UserMessage {
                content: crate::model::UserContent::Text("fix it".to_string()),
                timestamp: 0,
            }),
            Message::Assistant(Arc::new(crate::model::AssistantMessage {
                content: vec![
                    crate::model::ContentBlock::ToolCall(crate::model::ToolCall {
                        id: "1".to_string(),
                        name: "edit".to_string(),
                        arguments: serde_json::json!({"path": "src/a.rs"}),
                        thought_signature: None,
                    }),
                    crate::model::ContentBlock::ToolCall(crate::model::ToolCall {
                        id: "2".to_string(),
                        name: "bash".to_string(),
                        arguments: serde_json::json!({"command": "cargo test"}),
                        thought_signature: None,
                    }),
                    crate::model::ContentBlock::Text(crate::model::TextContent::new("done")),
                ],
                api: "x".to_string(),
                provider: "y".to_string(),
                model: "z".to_string(),
                usage: crate::model::Usage::default(),
                stop_reason: crate::model::StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            })),
            Message::ToolResult(Arc::new(crate::model::ToolResultMessage {
                tool_call_id: "2".to_string(),
                tool_name: "bash".to_string(),
                content: vec![crate::model::ContentBlock::Text(
                    crate::model::TextContent::new("permission denied"),
                )],
                is_error: true,
                details: None,
                timestamp: 0,
            })),
        ];
        let digest = build_digest(&messages);
        assert!(!digest.is_trivial());
        assert_eq!(digest.files_touched, vec!["src/a.rs".to_string()]);
        assert_eq!(digest.commands_run, vec!["cargo test".to_string()]);
        assert_eq!(digest.tool_errors.len(), 1);
        assert!(digest.tool_errors[0].contains("permission denied"));
        assert_eq!(digest.final_text, "done");
        assert_eq!(digest.tool_call_count, 2);
    }

    #[test]
    fn digest_flags_trivial_turns() {
        let messages = vec![Message::User(crate::model::UserMessage {
            content: crate::model::UserContent::Text("hi".to_string()),
            timestamp: 0,
        })];
        let digest = build_digest(&messages);
        assert!(digest.is_trivial(), "no-tool short turns are trivial");
    }

    #[test]
    fn injection_format_tags_level() {
        let verdict = AdvisorVerdict {
            level: VerdictLevel::Blocker,
            rationale: "stops now".to_string(),
        };
        assert_eq!(format_injection(&verdict), "[ADVISOR:BLOCKER] stops now");
    }

    #[test]
    fn timeout_reports_deadline_on_slow_future() {
        asupersync::test_utils::run_test(|| async {
            let outcome = with_timeout(Duration::from_millis(30), async {
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_secs(60))
                    .await;
                42
            })
            .await;
            assert_eq!(outcome, Err(RequestStop::TimedOut));
        });
    }

    struct ScriptedProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<Vec<crate::model::StreamEvent>>>,
        calls: std::sync::atomic::AtomicUsize,
        prompts: std::sync::Mutex<Vec<String>>,
        cancel_on_call: Option<asupersync::Cx>,
    }

    #[allow(clippy::unnecessary_literal_bound)]
    #[async_trait::async_trait]
    impl Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "advisor-test"
        }

        fn api(&self) -> &str {
            "advisor-test"
        }

        fn model_id(&self) -> &str {
            "advisor-test-model"
        }

        async fn stream(
            &self,
            context: &crate::provider::Context<'_>,
            options: &crate::provider::StreamOptions,
        ) -> crate::error::Result<
            std::pin::Pin<
                Box<dyn futures::Stream<Item = crate::error::Result<crate::model::StreamEvent>> + Send>,
            >,
        > {
            assert!(context.tools.is_empty());
            assert_eq!(context.system_prompt.as_deref(), Some(REVIEW_SYSTEM_PROMPT));
            assert_eq!(options.api_key.as_deref(), Some("test-key"));
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let Some(Message::User(user)) = context.messages.first() else {
                panic!("expected an advisor user prompt");
            };
            let crate::model::UserContent::Text(text) = &user.content else {
                panic!("expected a text-only advisor prompt");
            };
            self.prompts.lock().unwrap().push(text.clone());
            let events = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected advisor provider call");
            if let Some(owner) = &self.cancel_on_call {
                owner.cancel_with(asupersync::types::CancelKind::User, Some("review cancelled"));
            }
            Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
        }
    }

    fn scripted_runtime(
        responses: Vec<Vec<crate::model::StreamEvent>>,
    ) -> (AdvisorRuntime, Arc<ScriptedProvider>) {
        let provider = Arc::new(ScriptedProvider {
            responses: std::sync::Mutex::new(responses.into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
            cancel_on_call: None,
        });
        let runtime = AdvisorRuntime::new(provider.clone(), "test".to_string())
            .with_api_key(Some("test-key".to_string()));
        (runtime, provider)
    }

    fn review_digest() -> TurnDigest {
        TurnDigest {
            tool_call_count: 1,
            final_text: "edited src/example.rs".to_string(),
            ..Default::default()
        }
    }

    fn completed_reply(text: &str) -> Vec<crate::model::StreamEvent> {
        vec![crate::model::StreamEvent::Done {
            reason: crate::model::StopReason::Stop,
            message: crate::model::AssistantMessage {
                content: vec![crate::model::ContentBlock::Text(
                    crate::model::TextContent::new(text),
                )],
                stop_reason: crate::model::StopReason::Stop,
                ..Default::default()
            },
        }]
    }

    fn disconnected_reply() -> Vec<crate::model::StreamEvent> {
        vec![crate::model::StreamEvent::TextDelta {
            content_index: 0,
            delta: "BLOCKER\nThis is only an unfinished review".to_string(),
        }]
    }

    #[test]
    fn terminal_only_review_produces_a_verdict() {
        asupersync::test_utils::run_test(|| async {
            let (mut runtime, _) = scripted_runtime(vec![completed_reply(
                "CONCERN\nCheck the error path in src/example.rs.",
            )]);
            let AdvisorOutcome::Inject(verdict) = runtime.review_turn(&review_digest(), 0).await
            else {
                panic!("a completed terminal-only review must be usable");
            };
            assert_eq!(verdict.level, VerdictLevel::Concern);
            assert!(verdict.rationale.contains("src/example.rs"));
        });
    }

    #[test]
    fn disconnected_blocker_is_a_failure_not_an_injection() {
        asupersync::test_utils::run_test(|| async {
            let (mut runtime, _) = scripted_runtime(vec![disconnected_reply()]);
            assert!(matches!(
                runtime.review_turn(&review_digest(), 0).await,
                AdvisorOutcome::Failed
            ));
            assert_eq!(runtime.consecutive_failures, 1);
        });
    }

    #[test]
    fn truncated_terminal_review_is_not_injected() {
        asupersync::test_utils::run_test(|| async {
            let mut events = completed_reply("BLOCKER\nIncomplete review");
            if let crate::model::StreamEvent::Done { reason, message } = &mut events[0] {
                *reason = crate::model::StopReason::Length;
                message.stop_reason = crate::model::StopReason::Length;
            }
            let (mut runtime, _) = scripted_runtime(vec![events]);
            assert!(matches!(
                runtime.review_turn(&review_digest(), 0).await,
                AdvisorOutcome::Failed
            ));
        });
    }

    #[test]
    fn disable_survives_consumption_of_one_shot_notice() {
        asupersync::test_utils::run_test(|| async {
            let (mut runtime, provider) = scripted_runtime(vec![
                disconnected_reply(),
                disconnected_reply(),
                disconnected_reply(),
            ]);
            for turn in 0..3 {
                assert!(matches!(
                    runtime.review_turn(&review_digest(), turn).await,
                    AdvisorOutcome::Failed
                ));
            }
            assert!(runtime.is_disabled());
            assert!(runtime.disabled_notice.take().is_some());
            assert!(runtime.is_disabled(), "delivering a notice must not resume billing");
            assert!(matches!(
                runtime.review_turn(&review_digest(), 3).await,
                AdvisorOutcome::Quiet
            ));
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
            assert!(runtime.disabled_notice.is_none());
        });
    }

    #[test]
    fn completed_review_resets_only_consecutive_failure_count() {
        asupersync::test_utils::run_test(|| async {
            let (mut runtime, _) = scripted_runtime(vec![
                disconnected_reply(),
                completed_reply("NOTE\nThe edit preserves the existing error handling."),
                disconnected_reply(),
                disconnected_reply(),
                disconnected_reply(),
            ]);
            assert!(matches!(
                runtime.review_turn(&review_digest(), 0).await,
                AdvisorOutcome::Failed
            ));
            assert!(matches!(
                runtime.review_turn(&review_digest(), 1).await,
                AdvisorOutcome::Inject(_)
            ));
            assert_eq!(runtime.consecutive_failures, 0);
            for turn in 2..4 {
                assert!(matches!(
                    runtime.review_turn(&review_digest(), turn).await,
                    AdvisorOutcome::Failed
                ));
                assert!(!runtime.is_disabled());
            }
            assert!(matches!(
                runtime.review_turn(&review_digest(), 4).await,
                AdvisorOutcome::Failed
            ));
            assert!(runtime.is_disabled());
        });
    }

    #[test]
    fn zero_deadline_does_not_admit_provider_request() {
        asupersync::test_utils::run_test(|| async {
            let (runtime, provider) = scripted_runtime(Vec::new());
            let mut runtime = runtime.with_timeout(Duration::ZERO);
            assert!(matches!(
                runtime.review_turn(&review_digest(), 0).await,
                AdvisorOutcome::Failed
            ));
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        });
    }

    fn assistant(content: Vec<crate::model::ContentBlock>) -> Message {
        Message::Assistant(Arc::new(crate::model::AssistantMessage {
            content,
            ..Default::default()
        }))
    }

    fn call(name: &str, arguments: Value) -> crate::model::ContentBlock {
        crate::model::ContentBlock::ToolCall(crate::model::ToolCall {
            id: "digest-test".to_string(),
            name: name.to_string(),
            arguments,
            thought_signature: None,
        })
    }

    fn text(value: impl Into<String>) -> crate::model::ContentBlock {
        crate::model::ContentBlock::Text(crate::model::TextContent::new(value))
    }

    fn tool_error(content: Vec<crate::model::ContentBlock>) -> Message {
        Message::ToolResult(Arc::new(crate::model::ToolResultMessage {
            tool_call_id: "digest-test".to_string(),
            tool_name: "bash".to_string(),
            content,
            is_error: true,
            details: None,
            timestamp: 0,
        }))
    }

    #[test]
    fn digest_screens_commands_errors_and_final_text_before_clipping() {
        let key = "sk-abcdefghijklmnopqrstuvwxyz012345";
        let messages = vec![
            assistant(vec![
                call("bash", serde_json::json!({"command": format!("{} {key}", "x".repeat(190))})),
                text(format!("{} {key}", "x".repeat(1_990))),
            ]),
            tool_error(vec![text(format!("{} {key}", "x".repeat(190)))]),
        ];
        let projection = build_digest(&messages);
        let rendered = digest_prompt(&projection);
        assert!(!rendered.contains("sk-"), "a clipped key prefix must never escape");
        assert!(!rendered.contains("abcdef"));
        assert!(projection.commands_run[0].chars().count() <= 200);
        assert!(projection.tool_errors[0].chars().count() <= 200);
        assert!(projection.final_text.chars().count() <= MAX_FINAL_TEXT_CHARS);
        assert!(serde_json::to_string(&messages).unwrap().contains(key));
    }

    #[test]
    fn discovery_beyond_final_text_limit_protects_an_earlier_command_echo() {
        let secret = "r4nd0mCredentialValue123456";
        let messages = vec![assistant(vec![
            call("bash", serde_json::json!({"command": format!("echo {secret}")})),
            text(format!("{} API_KEY={secret}", "x".repeat(2_100))),
        ])];
        let projection = build_digest(&messages);
        assert_eq!(projection.commands_run, ["echo <pi-secret:redacted>"]);
        assert!(!digest_prompt(&projection).contains(secret));
    }

    #[test]
    fn error_blocks_are_screened_as_one_complete_private_key_envelope() {
        let projection = build_digest(&[tool_error(vec![
            text("-----BEGIN PRIVATE KEY-----"),
            text("PRIVATE-MATERIAL-CANARY"),
            text("-----END PRIVATE KEY-----"),
        ])]);
        assert_eq!(projection.tool_errors, ["<pi-secret:redacted>"]);
        assert!(!digest_prompt(&projection).contains("PRIVATE-MATERIAL-CANARY"));
    }

    #[test]
    fn oversized_fields_are_omitted_without_losing_safe_digest_fields() {
        let huge = format!("PRIVATE-PREFIX{}", "x".repeat(MAX_INPUT_BYTES));
        let messages = vec![
            assistant(vec![
                call("edit", serde_json::json!({"path": huge})),
                call("bash", serde_json::json!({"command": "cargo test"})),
                text("finished"),
            ]),
            tool_error(vec![text(&huge)]),
        ];
        let projection = build_digest(&messages);
        assert_eq!(projection.files_touched, [OMITTED_INPUT]);
        assert_eq!(projection.commands_run, ["cargo test"]);
        assert_eq!(projection.tool_errors, [OMITTED_INPUT]);
        assert_eq!(projection.final_text, "finished");
        assert_eq!(projection.tool_call_count, 2);
        assert!(!digest_prompt(&projection).contains("PRIVATE-PREFIX"));
    }

    #[test]
    fn direct_digest_is_screened_at_provider_admission_without_mutating_the_caller() {
        asupersync::test_utils::run_test(|| async {
            let secret = "r4nd0mCredentialValue123456";
            let digest = TurnDigest {
                files_touched: vec![secret.to_string()],
                commands_run: vec![format!("echo {secret}")],
                tool_errors: vec![format!("API_KEY={secret}")],
                final_text: "finished".to_string(),
                tool_call_count: 2,
                is_trivial: false,
            };
            let (mut runtime, provider) = scripted_runtime(vec![completed_reply(
                "CONCERN\nAvoid placing credentials in command arguments.",
            )]);
            assert!(matches!(runtime.review_turn(&digest, 0).await, AdvisorOutcome::Inject(_)));
            let prompts = provider.prompts.lock().unwrap();
            assert_eq!(prompts.len(), 1);
            assert!(!prompts[0].contains(secret));
            assert!(prompts[0].contains("<pi-secret:redacted>"));
            assert!(!prompts[0].contains("<pi-secret:000001>"));
            assert!(digest.commands_run[0].contains(secret));
            assert_eq!(runtime.consecutive_failures, 0);
        });
    }

    #[test]
    fn oversized_direct_digest_is_isolated_and_never_admits_a_provider_request() {
        asupersync::test_utils::run_test(|| async {
            let (mut runtime, provider) = scripted_runtime(Vec::new());
            let digest = TurnDigest {
                final_text: format!("PRIVATE-CANARY{}", "x".repeat(MAX_INPUT_BYTES)),
                tool_call_count: 1,
                ..Default::default()
            };
            for turn in 0..3 {
                assert!(matches!(runtime.review_turn(&digest, turn).await, AdvisorOutcome::Failed));
            }
            assert!(runtime.is_disabled());
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(provider.prompts.lock().unwrap().is_empty());
            assert!(!runtime.disabled_notice.as_deref().unwrap().contains("PRIVATE-CANARY"));
        });
    }

    #[test]
    fn direct_digest_field_count_is_bounded_before_projection_allocation() {
        let digest = TurnDigest {
            files_touched: vec![String::new(); MAX_DIGEST_FILES + 1],
            ..Default::default()
        };
        let error = screen_digest(&digest).unwrap_err();
        assert!(error.to_string().contains("PI_ADVISOR_DIGEST_LIMIT"));
    }

    #[test]
    fn cancelled_reviews_preserve_failure_streak_and_do_not_disable_the_advisor() {
        asupersync::test_utils::run_test(|| async {
            let (mut runtime, provider) = scripted_runtime(vec![completed_reply(
                "CONCERN\nCheck the preserved error path before continuing.",
            )]);
            runtime.consecutive_failures = 2;
            let owner = crate::agent_cx::AgentCx::for_request();
            owner.cancel_with(asupersync::types::CancelKind::User, Some("review cancelled"));
            for turn in 0..4 {
                assert!(matches!(
                    owner.with_current(runtime.review_turn(&review_digest(), turn)).await,
                    AdvisorOutcome::Quiet
                ));
            }
            assert_eq!(runtime.consecutive_failures, 2);
            assert!(!runtime.is_disabled());
            assert!(runtime.disabled_notice.is_none());
            assert_eq!(runtime.guard.notes_in_window, 0);
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(matches!(
                runtime.review_turn(&review_digest(), 4).await,
                AdvisorOutcome::Inject(_)
            ));
            assert_eq!(runtime.consecutive_failures, 0);
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn cancellation_racing_ready_blocker_does_not_inject_or_count_a_failure() {
        let executor = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = crate::agent_cx::AgentCx::from_cx(
            executor.request_cx_with_budget(asupersync::Budget::new()),
        );
        let provider = Arc::new(ScriptedProvider {
            responses: std::sync::Mutex::new(vec![completed_reply(
                "BLOCKER\nThis completed review belongs to cancelled work.",
            )].into()),
            calls: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
            cancel_on_call: Some(owner.cx().clone()),
        });
        let mut runtime = AdvisorRuntime::new(provider.clone(), "test".to_string())
            .with_api_key(Some("test-key".to_string()));
        runtime.consecutive_failures = 2;
        let outcome = executor.block_on(owner.with_current(
            runtime.review_turn(&review_digest(), 0),
        ));
        assert!(matches!(outcome, AdvisorOutcome::Quiet));
        assert_eq!(runtime.consecutive_failures, 2);
        assert!(!runtime.is_disabled());
        assert!(runtime.disabled_notice.is_none());
        assert_eq!(runtime.guard.notes_in_window, 0);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn timerless_review_is_isolated_without_admitting_the_provider() {
        let executor = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = {
            let _guard = asupersync::Cx::for_request()
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            crate::agent_cx::AgentCx::for_current_or_request()
        };
        let (mut runtime, provider) = scripted_runtime(Vec::new());
        let outcome = executor.block_on(owner.with_current(
            runtime.review_turn(&review_digest(), 0),
        ));
        assert!(matches!(outcome, AdvisorOutcome::Failed));
        assert_eq!(runtime.consecutive_failures, 1);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
