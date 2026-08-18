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
    pub fn is_trivial(&self) -> bool {
        self.is_trivial
    }
}

const MAX_DIGEST_FILES: usize = 25;
const MAX_DIGEST_COMMANDS: usize = 15;
const MAX_DIGEST_ERRORS: usize = 10;
const MAX_FINAL_TEXT_CHARS: usize = 2_000;

/// Build the digest from the tail of the conversation (last user message
/// onward), budgeted.
#[must_use]
pub fn build_digest(messages: &[Message]) -> TurnDigest {
    // Start from the last user message (turn boundary).
    let boundary = messages
        .iter()
        .rposition(|m| matches!(m, Message::User(_)))
        .unwrap_or(0);
    let tail = &messages[boundary..];

    let mut digest = TurnDigest::default();
    for message in tail {
        match message {
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    match block {
                        crate::model::ContentBlock::ToolCall(call) => {
                            digest.tool_call_count += 1;
                            match call.name.as_str() {
                                "write" | "edit" | "hashline_edit" | "ast_edit" => {
                                    if let Some(path) = call
                                        .arguments
                                        .get("path")
                                        .and_then(Value::as_str)
                                        && digest.files_touched.len() < MAX_DIGEST_FILES
                                        && !digest.files_touched.iter().any(|p| p == path)
                                    {
                                        digest.files_touched.push(path.to_string());
                                    }
                                }
                                "bash" => {
                                    if let Some(command) = call
                                        .arguments
                                        .get("command")
                                        .and_then(Value::as_str)
                                        && digest.commands_run.len() < MAX_DIGEST_COMMANDS
                                    {
                                        digest.commands_run.push(
                                            command.chars().take(200).collect(),
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                        crate::model::ContentBlock::Text(text) => {
                            digest.final_text = text
                                .text
                                .chars()
                                .take(MAX_FINAL_TEXT_CHARS)
                                .collect();
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result) => {
                if result.is_error
                    && digest.tool_errors.len() < MAX_DIGEST_ERRORS
                {
                    let text = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            crate::model::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    digest.tool_errors.push(text.chars().take(200).collect());
                }
            }
            _ => {}
        }
    }
    digest.is_trivial = digest.tool_call_count == 0 && digest.final_text.len() < 400;
    digest
}

/// The review rubric prompt.
pub const REVIEW_SYSTEM_PROMPT: &str = "You are the advisor: a senior reviewer watching another agent's turn. \
Review the digest and answer with EXACTLY one verdict line, then a short rationale.\n\
Line 1 must be one of: NOTE, CONCERN, BLOCKER.\n\
- NOTE: fine, or a minor observation worth one sentence.\n\
- CONCERN: something likely wrong/risky the doer should fix before continuing.\n\
- BLOCKER: a hard problem (data loss risk, broken build, wrong target) that must stop work until addressed.\n\
Be terse. Cite file paths when relevant. No markdown, no headers.";

/// Render the digest as the advisor's user prompt.
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
    let upper = first_line.trim().trim_matches([':', '.', '*', '#', ' ']).to_ascii_uppercase();
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
            first_line.trim().trim_start_matches(|c: char| c.is_ascii_uppercase() || c == ' ').trim_start_matches([':', '-', ' ']).chars().take(600).collect()
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
pub static ADVISOR_PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The advisor runtime: owns the second provider, the guard, and failure
/// isolation state.
pub struct AdvisorRuntime {
    provider: Arc<dyn Provider>,
    label: String,
    timeout: Duration,
    guard: EmissionGuard,
    consecutive_failures: u32,
    /// Set when disabled after repeated failures (user notice rides once).
    pub disabled_notice: Option<String>,
}

/// What a review returned.
#[derive(Debug)]
pub enum AdvisorOutcome {
    /// A verdict worth injecting.
    Inject(AdvisorVerdict),
    /// Suppressed (trivial turn, guard, or advisor found nothing).
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
            disabled_notice: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_guard(mut self, max_per_window: usize, window_turns: usize) -> Self {
        self.guard = EmissionGuard::new(max_per_window, window_turns);
        self
    }

    #[must_use]
    pub fn is_disabled(&self) -> bool {
        self.disabled_notice.is_some()
    }

    /// Review one turn. Never fails the caller.
    pub async fn review_turn(&mut self, digest: &TurnDigest, turn_index: u64) -> AdvisorOutcome {
        if self.is_disabled() || ADVISOR_PAUSED.load(std::sync::atomic::Ordering::SeqCst) {
            return AdvisorOutcome::Quiet;
        }
        if digest.is_trivial() {
            return AdvisorOutcome::Quiet;
        }
        let call = self.call_advisor(digest);
        let reply = match with_timeout(self.timeout, call).await {
            Some(Ok(text)) => text,
            Some(Err(_)) | None => {
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
        let context = crate::provider::Context {
            system_prompt: Some(REVIEW_SYSTEM_PROMPT.to_string().into()),
            messages: vec![crate::model::Message::User(crate::model::UserMessage {
                content: crate::model::UserContent::Text(digest_prompt(digest)),
                timestamp: chrono::Utc::now().timestamp_millis(),
            })]
            .into(),
            tools: Vec::new().into(),
        };
        let options = crate::provider::StreamOptions {
            max_tokens: Some(512),
            ..Default::default()
        };
        let mut stream = self.provider.stream(&context, &options).await?;
        let mut text = String::new();
        use futures::StreamExt;
        while let Some(event) = stream.next().await {
            match event {
                Ok(crate::model::StreamEvent::TextDelta { delta, .. }) => text.push_str(&delta),
                Ok(crate::model::StreamEvent::Done { .. }) => break,
                Ok(_) => {}
                Err(err) => return Err(err),
            }
        }
        if text.trim().is_empty() {
            return Err(crate::error::Error::api("advisor returned empty reply"));
        }
        Ok(text)
    }
}

/// Minimal wall-clock timeout that works with any future (no runtime driver
/// dependency: it polls the future against a millisecond sleep loop).
async fn with_timeout<F>(timeout: Duration, future: F) -> Option<F::Output>
where
    F: std::future::Future,
{
    use std::pin::pin;
    let mut future = pin!(future);
    let start = std::time::Instant::now();
    std::future::poll_fn(move |cx| {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(cx) {
            return std::task::Poll::Ready(Some(output));
        }
        if start.elapsed() >= timeout {
            return std::task::Poll::Ready(None);
        }
        cx.waker().wake_by_ref();
        std::task::Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verdict_levels() {
        let blocker = parse_verdict("BLOCKER: the edit deletes the migration\nIt removes drop columns.");
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
        assert!(!guard.allow(&concern(), 1), "dedupe: same rationale blocked");
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
                content: vec![crate::model::ContentBlock::Text(crate::model::TextContent::new(
                    "permission denied",
                ))],
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
    fn timeout_returns_none_on_slow_future() {
        asupersync::test_utils::run_test(|| async {
            let outcome = with_timeout(Duration::from_millis(30), async {
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_secs(60)).await;
                42
            })
            .await;
            assert!(outcome.is_none(), "slow advisor call must time out");
        });
    }
}
