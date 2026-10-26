//! Turn recovery: unexpected-stop classification + auto-continue
//! (bd-cv653.3.15).
//!
//! A model that stops mid-task — token budget exhausted mid-code-block, or a
//! "I will now edit the files" promise with no follow-through — used to end
//! the turn silently, leaving the user to nudge by hand. At each turn end the
//! agent now runs a deterministic, zero-cost heuristic classifier over the
//! final assistant message; empty or actionable stops inject one synthetic
//! continue-nudge user message (visible in the transcript, persisted with the
//! normal message flow) and let the turn loop run again. A hard cap of
//! [`MAX_AUTO_CONTINUATIONS`] per run prevents loops; transport errors and
//! provider failover stay entirely out of scope here (RetrySettings owns
//! them).

use serde::{Deserialize, Serialize};

use crate::model::{AssistantMessage, ContentBlock, Message, StopReason, TextContent};

/// Schema/marker tag carried in nudge messages and logs.
pub const TURN_RECOVERY_SCHEMA: &str = "pi.turn_recovery.v1";

/// Maximum auto-continuations per agent run — then the user decides.
pub const MAX_AUTO_CONTINUATIONS: u8 = 2;

/// How aggressively to auto-continue unexpected stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnRecoveryMode {
    /// Never auto-continue.
    Off,
    /// Auto-continue empty responses and provable interruptions:
    /// token-budget truncation and structurally unclosed output.
    #[default]
    Conservative,
    /// Also auto-continue semantic premature stops (announced-but-unstarted
    /// work).
    Aggressive,
}

/// What the heuristic classifier concluded about a finished turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryClass {
    CleanStop,
    /// A successful stop delivered no text, media, tool call, or authenticated
    /// provider-owned terminal content.
    EmptyResponse,
    /// The provider hit max_tokens (`StopReason::Length`).
    BudgetTruncated,
    /// Output ends inside an unclosed code fence or on a dangling list
    /// bullet — structurally cut off even though the stop looked clean.
    UnclosedStructure,
    /// The message announces imminent work ("I will now ...") and then
    /// stops without doing it.
    SemanticPrematureStop,
}

impl RecoveryClass {
    /// Human-readable reason used in the transcript marker.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::CleanStop => "clean stop",
            Self::EmptyResponse => "response contained no final output",
            Self::BudgetTruncated => "response truncated by token budget",
            Self::UnclosedStructure => "response ended inside unfinished output",
            Self::SemanticPrematureStop => "announced work was not started",
        }
    }

    const fn actionable_in(self, mode: TurnRecoveryMode) -> bool {
        match self {
            Self::CleanStop => false,
            Self::EmptyResponse | Self::BudgetTruncated | Self::UnclosedStructure => {
                !matches!(mode, TurnRecoveryMode::Off)
            }
            Self::SemanticPrematureStop => matches!(mode, TurnRecoveryMode::Aggressive),
        }
    }
}

/// Empty successful responses need another provider turn, not a successful
/// completion with nothing for the caller to see. Inspect the complete content
/// rather than visible text alone: media, tool calls, signed thinking and
/// redacted thinking are real provider-owned output. In particular, opaque or
/// authenticated reasoning may be an intentional withheld-output terminal and
/// must not be retried as though the provider lost its answer.
#[must_use]
pub(crate) fn is_empty_assistant_response(message: &AssistantMessage) -> bool {
    matches!(message.stop_reason, StopReason::Stop | StopReason::ToolUse)
        && message.stop_details.is_none()
        && message.error_message.is_none()
        && message.content.iter().all(|block| match block {
            ContentBlock::Text(text) => {
                text.text.trim().is_empty()
                    && text
                        .text_signature
                        .as_deref()
                        .is_none_or(|signature| signature.trim().is_empty())
            }
            ContentBlock::Thinking(thinking) => thinking
                .thinking_signature
                .as_deref()
                .is_none_or(|signature| signature.trim().is_empty()),
            ContentBlock::RedactedThinking(_)
            | ContentBlock::Image(_)
            | ContentBlock::Media(_)
            | ContentBlock::ToolCall(_) => false,
        })
}

/// Retain usage and chronology in the durable transcript, but do not replay
/// non-actionable empty assistant turns as invalid provider history. Applying
/// this projection again after reopening a session gives the same request.
#[must_use]
pub(crate) fn context_excludes_empty_response(message: &Message) -> bool {
    matches!(message, Message::Assistant(assistant) if is_empty_assistant_response(assistant))
}

/// Normalize before MessageEnd so every observer sees one terminal Error when
/// the shared run budget is exhausted. The visible diagnostic also keeps the
/// final error safe to include in later provider context.
pub(crate) fn finish_empty_response(message: &mut AssistantMessage, exhausted: bool) {
    if exhausted && is_empty_assistant_response(message) {
        let diagnostic = format!(
            "PI_EMPTY_RESPONSE: provider returned no final output after \
             {MAX_AUTO_CONTINUATIONS} automatic continuations; select another model \
             or send a new request"
        );
        message
            .content
            .push(ContentBlock::Text(TextContent::new(diagnostic.clone())));
        message.stop_reason = StopReason::Error;
        message.error_message = Some(diagnostic);
    }
}

/// Classify a finished turn from its stop reason and final text.
///
/// Only `Stop` and `Length` stops are examined: errors, aborts, refusals,
/// tool-use and pause-turn stops all have dedicated handling elsewhere.
#[must_use]
pub fn classify(stop_reason: StopReason, text: &str) -> RecoveryClass {
    match stop_reason {
        StopReason::Length => return RecoveryClass::BudgetTruncated,
        StopReason::Stop => {}
        _ => return RecoveryClass::CleanStop,
    }

    let trimmed = text.trim_end();
    if trimmed.is_empty() {
        return RecoveryClass::CleanStop;
    }

    if has_unclosed_fence(trimmed) || ends_on_dangling_bullet(trimmed) {
        return RecoveryClass::UnclosedStructure;
    }
    if ends_on_unfulfilled_promise(trimmed) {
        return RecoveryClass::SemanticPrematureStop;
    }
    RecoveryClass::CleanStop
}

/// Track top-level fenced code blocks, including the delimiter's type and
/// length. A fence may contain shorter fences or fences of the other type
/// as literal code; counting delimiter-looking lines misclassifies both
/// completed examples and genuinely truncated output.
fn has_unclosed_fence(text: &str) -> bool {
    let mut open_fence: Option<(u8, usize)> = None;
    for line in text.lines() {
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        // Four spaces (or a leading tab) introduce indented code, not a
        // top-level fence. Only ASCII spaces were counted, so this slice
        // always starts on a UTF-8 boundary.
        if indent > 3 {
            continue;
        }
        let line = &line[indent..];
        let Some(marker @ (b'`' | b'~')) = line.as_bytes().first().copied() else {
            continue;
        };
        let length = line.bytes().take_while(|byte| *byte == marker).count();
        if length < 3 {
            continue;
        }
        let suffix = &line[length..];
        match open_fence {
            Some((open_marker, open_length))
                if marker == open_marker
                    && length >= open_length
                    && suffix.bytes().all(|byte| matches!(byte, b' ' | b'\t')) =>
            {
                open_fence = None;
            }
            // Backticks in a backtick fence's info string invalidate the
            // opener. Tilde fences do not have that restriction.
            None if marker != b'`' || !suffix.contains('`') => {
                open_fence = Some((marker, length));
            }
            _ => {}
        }
    }
    open_fence.is_some()
}

/// The final line is an empty list item. A bare ordered marker is only
/// actionable after another item in the same list: a complete numeric
/// answer such as "42." must not spend another model call or the recovery
/// budget. Prefer a false negative over inventing work from ambiguous text.
fn ends_on_dangling_bullet(text: &str) -> bool {
    let mut lines = text.lines().rev();
    let Some(last) = lines.next() else {
        return false;
    };
    let indent = last.bytes().take_while(|byte| *byte == b' ').count();
    if indent > 3 || last[indent..].starts_with('\t') {
        return false;
    }
    let last = last.trim();
    if matches!(last, "-" | "*" | "+") {
        return true;
    }
    let Some((delimiter, remainder)) = ordered_bullet(last) else {
        return false;
    };
    if !remainder.is_empty() {
        return false;
    }
    let Some(previous) = lines.find(|line| !line.trim().is_empty()) else {
        return false;
    };
    let previous_indent = previous.bytes().take_while(|byte| *byte == b' ').count();
    if previous_indent != indent || previous[previous_indent..].starts_with('\t') {
        return false;
    }
    ordered_bullet(previous.trim()).is_some_and(|(previous_delimiter, content)| {
        previous_delimiter == delimiter && !content.trim().is_empty()
    })
}

/// Recognize the one-to-nine-digit ordered-list markers supported by
/// Markdown. Nonempty item content must be separated by a space or tab.
fn ordered_bullet(line: &str) -> Option<(u8, &str)> {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=9).contains(&digits) {
        return None;
    }
    let delimiter @ (b'.' | b')') = line.as_bytes().get(digits).copied()? else {
        return None;
    };
    let remainder = &line[digits + 1..];
    if remainder
        .as_bytes()
        .first()
        .copied()
        .is_some_and(|byte| !matches!(byte, b' ' | b'\t'))
    {
        return None;
    }
    Some((delimiter, remainder))
}

/// The message's closing sentence announces imminent action and nothing
/// follows it.
fn ends_on_unfulfilled_promise(text: &str) -> bool {
    let tail: String = text
        .chars()
        .rev()
        .take(240)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let tail_lower = tail.to_lowercase();
    let Some(position) = PROMISE_PHRASES
        .iter()
        .filter_map(|phrase| tail_lower.rfind(phrase))
        .max()
    else {
        return false;
    };
    // Nothing but one closing sentence may follow the announcement: if
    // another sentence terminator appears well before the end, the promise
    // was presumably followed through in text.
    let after = &tail_lower[position..]; // ubs:ignore position from rfind on this same string, always a valid boundary
    let interior = after.trim_end_matches(['.', ':', '!', '…', ' ', '\n']);
    !interior.contains(". ") && !interior.contains(":\n\n")
}

const PROMISE_PHRASES: &[&str] = &[
    "i will now",
    "i'll now",
    "let me now",
    "i am going to",
    "i'm going to",
    "next, i will",
    "next, i'll",
    "now i will",
    "now i'll",
    "proceeding to",
];

/// A recovery decision: the class plus the nudge to inject.
#[derive(Debug, Clone)]
pub struct RecoveryAction {
    pub class: RecoveryClass,
    pub nudge_text: String,
}

/// Per-run auto-continuation state (mode gating + hard cap).
#[derive(Debug)]
pub struct TurnRecoveryState {
    mode: TurnRecoveryMode,
    continuations: u8,
}

impl TurnRecoveryState {
    #[must_use]
    pub const fn new(mode: TurnRecoveryMode) -> Self {
        Self {
            mode,
            continuations: 0,
        }
    }

    /// Number of auto-continuations issued so far this run.
    #[must_use]
    pub const fn continuations(&self) -> u8 {
        self.continuations
    }

    /// Empty-response recovery shares the run's existing continuation budget.
    #[must_use]
    pub(crate) const fn empty_response_limit_reached(&self) -> bool {
        !matches!(self.mode, TurnRecoveryMode::Off) && self.continuations >= MAX_AUTO_CONTINUATIONS
    }

    /// Content-aware entrypoint used by the common Agent loop on every surface.
    pub(crate) fn evaluate_assistant(&mut self, message: &AssistantMessage) -> Option<RecoveryAction> {
        if message.stop_details.is_some() || message.error_message.is_some() {
            return None;
        }
        if is_empty_assistant_response(message) {
            return self.evaluate_class(RecoveryClass::EmptyResponse);
        }
        let mut text = String::new();
        for block in &message.content {
            if let ContentBlock::Text(part) = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&part.text);
            }
        }
        self.evaluate(message.stop_reason, &text)
    }

    /// Classify a finished turn and decide whether to auto-continue.
    ///
    /// Consumes one continuation from the cap when it returns `Some`.
    pub fn evaluate(&mut self, stop_reason: StopReason, text: &str) -> Option<RecoveryAction> {
        self.evaluate_class(classify(stop_reason, text))
    }

    fn evaluate_class(&mut self, class: RecoveryClass) -> Option<RecoveryAction> {
        if matches!(self.mode, TurnRecoveryMode::Off) {
            return None;
        }
        if !class.actionable_in(self.mode) {
            return None;
        }
        // After a continuation the model may legitimately open or close
        // structures spanning message boundaries (finishing an earlier code
        // fence looks "unclosed" in isolation), so the text-shape heuristics
        // only apply to the first stop; re-continuation needs an empty
        // response or the provider's own truncation signal.
        if self.continuations > 0
            && !matches!(
                class,
                RecoveryClass::BudgetTruncated | RecoveryClass::EmptyResponse
            )
        {
            return None;
        }
        if self.continuations >= MAX_AUTO_CONTINUATIONS {
            tracing::info!(
                schema = TURN_RECOVERY_SCHEMA,
                class = ?class,
                cap = MAX_AUTO_CONTINUATIONS,
                "auto-continuation cap reached; leaving the stop to the user"
            );
            return None;
        }
        self.continuations += 1;
        tracing::info!(
            schema = TURN_RECOVERY_SCHEMA,
            class = ?class,
            continuation = self.continuations,
            "auto-continuing unexpected stop"
        );
        Some(RecoveryAction {
            class,
            nudge_text: format!(
                "[auto-continue {}/{}: {}] {}",
                self.continuations,
                MAX_AUTO_CONTINUATIONS,
                class.reason(),
                if matches!(class, RecoveryClass::EmptyResponse) {
                    "The previous response provided no final output. Continue the existing task \
                     and provide an answer or the necessary tool action. Do not repeat completed \
                     tool actions."
                } else {
                    "Continue from exactly where you stopped. Do not repeat content you already \
                     produced; finish the remaining work."
                }
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession};
    use crate::model::{
        ImageContent, MediaContent, RedactedThinkingContent, StopDetails, StreamEvent,
        ThinkingContent, ToolCall, UserContent,
    };
    use crate::provider::{Context, Provider, StreamOptions};
    use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
    use futures::Stream;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn response(reason: StopReason, content: Vec<ContentBlock>) -> AssistantMessage {
        AssistantMessage {
            content,
            api: "empty-response-probe".to_string(),
            provider: "empty-response-probe".to_string(),
            model: "empty-response-probe".to_string(),
            stop_reason: reason,
            usage: crate::model::Usage {
                output: 7,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn completed_response() -> AssistantMessage {
        response(
            StopReason::Stop,
            vec![ContentBlock::Text(TextContent::new(
                "Completed the requested work.",
            ))],
        )
    }

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    struct RecoveryProbe {
        replies: Vec<AssistantMessage>,
        calls: AtomicUsize,
        contexts: Mutex<Vec<Vec<Message>>>,
    }

    impl RecoveryProbe {
        fn new(replies: Vec<AssistantMessage>) -> Self {
            Self {
                replies,
                calls: AtomicUsize::new(0),
                contexts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Provider for RecoveryProbe {
        fn name(&self) -> &str {
            "empty-response-probe"
        }

        fn api(&self) -> &str {
            "empty-response-probe"
        }

        fn model_id(&self) -> &str {
            "empty-response-probe"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.contexts.lock().unwrap().push(context.messages.to_vec());
            let message = self
                .replies
                .get(index)
                .expect("unexpected provider re-entry")
                .clone();
            Ok(Box::pin(futures::stream::iter([Ok(
                StreamEvent::Done {
                    reason: message.stop_reason,
                    message,
                },
            )])))
        }
    }

    fn agent(probe: Arc<RecoveryProbe>, config: AgentConfig) -> Agent {
        Agent::new(probe, ToolRegistry::from_tools(Vec::new()), config)
    }

    fn recovery_nudge(message: &Message) -> bool {
        matches!(
            message,
            Message::User(user)
                if matches!(&user.content, UserContent::Text(text)
                    if text.starts_with("[auto-continue "))
        )
    }

    #[test]
    fn empty_response_policy_preserves_provider_terminal_and_nontext_output() {
        for reason in [StopReason::Stop, StopReason::ToolUse] {
            for content in [
                Vec::new(),
                vec![ContentBlock::Text(TextContent::new(" \n\t "))],
                vec![ContentBlock::Thinking(ThinkingContent {
                    thinking: "Unfinished internal reasoning".to_string(),
                    thinking_signature: None,
                })],
            ] {
                let message = response(reason, content);
                assert!(is_empty_assistant_response(&message));
                assert!(context_excludes_empty_response(&Message::assistant(message)));
            }
        }
        for block in [
            ContentBlock::Text(TextContent::new("An answer.")),
            ContentBlock::Text(TextContent {
                text: String::new(),
                text_signature: Some("provider-terminal".to_string()),
            }),
            ContentBlock::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some("provider-terminal".to_string()),
            }),
            ContentBlock::RedactedThinking(RedactedThinkingContent {
                data: "opaque-provider-payload".to_string(),
            }),
            ContentBlock::Image(ImageContent {
                data: "aW1hZ2U=".to_string(),
                mime_type: "image/png".to_string(),
            }),
            ContentBlock::Media(MediaContent {
                data: "YXVkaW8=".to_string(),
                mime_type: "audio/wav".to_string(),
                name: None,
            }),
            ContentBlock::ToolCall(ToolCall {
                id: "call-one".to_string(),
                name: "count_action".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            }),
        ] {
            let message = response(StopReason::Stop, vec![block]);
            assert!(!is_empty_assistant_response(&message));
            assert!(!context_excludes_empty_response(&Message::assistant(message)));
        }
        for reason in [
            StopReason::Length,
            StopReason::Error,
            StopReason::Aborted,
            StopReason::PauseTurn,
            StopReason::Refusal,
        ] {
            assert!(!is_empty_assistant_response(&response(reason, Vec::new())));
        }
        let mut refusal = response(StopReason::Stop, Vec::new());
        refusal.stop_details = Some(StopDetails {
            kind: "refusal".to_string(),
            category: None,
            explanation: None,
        });
        assert!(!is_empty_assistant_response(&refusal));
        finish_empty_response(&mut refusal, true);
        assert_eq!(refusal.stop_reason, StopReason::Stop);
        refusal.content = vec![ContentBlock::Text(TextContent::new("~~~withheld"))];
        assert!(
            TurnRecoveryState::new(TurnRecoveryMode::Conservative)
                .evaluate_assistant(&refusal)
                .is_none()
        );
    }

    #[test]
    fn empty_responses_share_the_existing_run_budget_with_truncation() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        assert!(state.evaluate(StopReason::Length, "partial").is_some());
        let empty = response(StopReason::Stop, Vec::new());
        let action = state.evaluate_assistant(&empty).unwrap();
        assert_eq!(action.class, RecoveryClass::EmptyResponse);
        assert!(action.nudge_text.contains("Do not repeat completed"));
        assert!(state.empty_response_limit_reached());
        assert!(state.evaluate_assistant(&empty).is_none());
        assert_eq!(state.continuations(), MAX_AUTO_CONTINUATIONS);
        let mut off = TurnRecoveryState::new(TurnRecoveryMode::Off);
        assert!(off.evaluate_assistant(&empty).is_none());
        assert!(!off.empty_response_limit_reached());
    }

    #[test]
    fn native_empty_response_recovers_without_replaying_empty_provider_history() {
        run(async {
            for (reason, content) in [
                (StopReason::Stop, Vec::new()),
                (
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(" \n\t "))],
                ),
                (
                    StopReason::Stop,
                    vec![ContentBlock::Thinking(ThinkingContent {
                        thinking: "Need to finish the answer".to_string(),
                        thinking_signature: None,
                    })],
                ),
                (StopReason::ToolUse, Vec::new()),
            ] {
                let probe = Arc::new(RecoveryProbe::new(vec![
                    response(reason, content),
                    completed_response(),
                ]));
                let mut agent = agent(Arc::clone(&probe), AgentConfig::default());
                let result = agent.run("Complete this task", |_| {}).await.unwrap();
                assert_eq!(result.stop_reason, StopReason::Stop);
                assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
                assert_eq!(
                    agent
                        .messages()
                        .iter()
                        .filter(|message| recovery_nudge(message))
                        .count(),
                    1
                );
                let empty_attempt = agent
                    .messages()
                    .iter()
                    .find_map(|message| match message {
                        Message::Assistant(message) if is_empty_assistant_response(message) => {
                            Some(message)
                        }
                        _ => None,
                    })
                    .expect("retain the attempted response for usage and audit");
                assert_eq!(empty_attempt.usage.output, 7);
                let requests = probe.contexts.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert!(
                    requests[1]
                        .iter()
                        .all(|message| !context_excludes_empty_response(message))
                );
                let nudge_index = agent.messages().iter().position(recovery_nudge).unwrap();
                assert!(!agent.is_authored_queue_delivery(nudge_index));
            }
        });
    }

    #[test]
    fn native_empty_response_cap_is_a_terminal_error_without_outer_retry() {
        run(async {
            let probe = Arc::new(RecoveryProbe::new(vec![
                response(StopReason::Stop, Vec::new());
                usize::from(MAX_AUTO_CONTINUATIONS) + 1
            ]));
            let mut agent = agent(Arc::clone(&probe), AgentConfig::default());
            let events = Arc::new(Mutex::new(Vec::new()));
            let collected = Arc::clone(&events);
            let result = agent
                .run("Complete this task", move |event| {
                    collected.lock().unwrap().push(event);
                })
                .await
                .unwrap();
            assert_eq!(result.stop_reason, StopReason::Error);
            let diagnostic = result.error_message.as_deref().unwrap();
            assert!(diagnostic.starts_with("PI_EMPTY_RESPONSE:"));
            assert!(!crate::failover::error_result_is_retryable(&result, None));
            assert!(crate::failover::classify_failover(diagnostic).is_none());
            assert_eq!(probe.calls.load(Ordering::SeqCst), 3);
            assert_eq!(
                agent
                    .messages()
                    .iter()
                    .filter(|message| recovery_nudge(message))
                    .count(),
                2
            );
            let events = events.lock().unwrap();
            let terminals = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::MessageEnd {
                        message: Message::Assistant(message),
                    } => Some(message.stop_reason),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(
                terminals,
                vec![StopReason::Stop, StopReason::Stop, StopReason::Error]
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, AgentEvent::AgentEnd { .. }))
                    .count(),
                1
            );
        });
    }

    #[test]
    fn native_empty_response_respects_manual_dispatch_disable_and_cancellation() {
        run(async {
            for manual in [false, true] {
                let probe = Arc::new(RecoveryProbe::new(vec![
                    response(StopReason::Stop, Vec::new()),
                ]));
                let mut agent = agent(
                    Arc::clone(&probe),
                    AgentConfig {
                        turn_recovery: if manual {
                            TurnRecoveryMode::Conservative
                        } else {
                            TurnRecoveryMode::Off
                        },
                        ..Default::default()
                    },
                );
                if manual {
                    agent.set_automatic_queue_dispatch(true, false);
                }
                assert_eq!(
                    agent
                        .run("Accept an empty terminal", |_| {})
                        .await
                        .unwrap()
                        .stop_reason,
                    StopReason::Stop
                );
                assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
                assert!(!agent.messages().iter().any(recovery_nudge));
            }
            let probe = Arc::new(RecoveryProbe::new(vec![
                response(StopReason::Stop, Vec::new()),
            ]));
            let mut agent = agent(Arc::clone(&probe), AgentConfig::default());
            let (abort, signal) = AbortHandle::new();
            agent
                .run_with_abort("Cancel at completion", Some(signal), move |event| {
                    if matches!(
                        event,
                        AgentEvent::MessageEnd {
                            message: Message::Assistant(_)
                        }
                    ) {
                        abort.abort();
                    }
                })
                .await
                .unwrap();
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            assert!(!agent.messages().iter().any(recovery_nudge));
        });
    }

    #[test]
    fn native_provider_owned_terminal_output_is_preserved_without_recovery() {
        run(async {
            let mut terminals = [
                ContentBlock::Text(TextContent {
                    text: String::new(),
                    text_signature: Some("provider-terminal".to_string()),
                }),
                ContentBlock::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some("provider-terminal".to_string()),
                }),
                ContentBlock::RedactedThinking(RedactedThinkingContent {
                    data: "opaque-provider-payload".to_string(),
                }),
                ContentBlock::Image(ImageContent {
                    data: "aW1hZ2U=".to_string(),
                    mime_type: "image/png".to_string(),
                }),
                ContentBlock::Media(MediaContent {
                    data: "YXVkaW8=".to_string(),
                    mime_type: "audio/wav".to_string(),
                    name: None,
                }),
            ]
            .into_iter()
            .map(|block| response(StopReason::Stop, vec![block]))
            .collect::<Vec<_>>();
            let mut refusal = response(StopReason::Stop, Vec::new());
            refusal.stop_details = Some(StopDetails {
                kind: "refusal".to_string(),
                category: None,
                explanation: None,
            });
            terminals.push(refusal);
            for message in terminals {
                let expected_content = serde_json::to_value(&message.content).unwrap();
                let probe = Arc::new(RecoveryProbe::new(vec![message, completed_response()]));
                let mut agent = agent(Arc::clone(&probe), AgentConfig::default());
                let result = agent.run("Respect this provider terminal", |_| {}).await.unwrap();
                assert_eq!(result.stop_reason, StopReason::Stop);
                assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
                agent.run("A new authored request", |_| {}).await.unwrap();
                assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
                assert!(!agent.messages().iter().any(recovery_nudge));
                let requests = probe.contexts.lock().unwrap();
                let carried = requests[1]
                    .iter()
                    .find_map(|message| match message {
                        Message::Assistant(message) => Some(message),
                        _ => None,
                    })
                    .expect("provider-owned terminal blocks must remain in later context");
                assert_eq!(
                    serde_json::to_value(&carried.content).unwrap(),
                    expected_content
                );
            }
        });
    }

    #[derive(Debug)]
    struct CountAction(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    #[allow(clippy::unnecessary_literal_bound)]
    impl Tool for CountAction {
        fn name(&self) -> &str {
            "count_action"
        }

        fn label(&self) -> &str {
            "count action"
        }

        fn description(&self) -> &str {
            "record a side effect"
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            _input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> crate::error::Result<ToolOutput> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new("Action completed"))],
                details: None,
                is_error: false,
            })
        }
    }

    #[test]
    fn native_empty_response_after_a_tool_keeps_the_result_without_reexecution() {
        run(async {
            let probe = Arc::new(RecoveryProbe::new(vec![
                response(
                    StopReason::ToolUse,
                    vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-one".to_string(),
                        name: "count_action".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    })],
                ),
                response(StopReason::Stop, Vec::new()),
                completed_response(),
            ]));
            let calls = Arc::new(AtomicUsize::new(0));
            let mut agent = Agent::new(
                probe.clone(),
                ToolRegistry::from_tools(vec![Box::new(CountAction(Arc::clone(&calls)))]),
                AgentConfig::default(),
            );
            let result = agent
                .run("Perform one action and report it", |_| {})
                .await
                .unwrap();
            assert_eq!(result.stop_reason, StopReason::Stop);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(probe.calls.load(Ordering::SeqCst), 3);
            let requests = probe.contexts.lock().unwrap();
            assert_eq!(
                requests[2]
                    .iter()
                    .filter(|message| {
                        matches!(message, Message::ToolResult(result)
                            if result.tool_call_id == "call-one")
                    })
                    .count(),
                1
            );
            assert!(
                requests[2]
                    .iter()
                    .all(|message| !context_excludes_empty_response(message))
            );
        });
    }

    #[test]
    fn saved_empty_response_recovery_preserves_usage_and_authored_input_provenance() {
        run(async {
            let root = tempfile::tempdir().unwrap();
            let mut saved =
                crate::session::Session::create_with_dir(Some(root.path().join("sessions")));
            saved.header.cwd = root.path().display().to_string();
            let shared = Arc::new(asupersync::sync::Mutex::new(saved));
            let probe = Arc::new(RecoveryProbe::new(vec![
                response(StopReason::Stop, Vec::new()),
                completed_response(),
            ]));
            let mut session = AgentSession::new(
                agent(Arc::clone(&probe), AgentConfig::default()),
                Arc::clone(&shared),
                true,
                crate::compaction::ResolvedCompactionSettings {
                    enabled: false,
                    ..Default::default()
                },
            )
            .with_memory_settings(Some(&crate::config::MemorySettings {
                backend: Some("local".to_string()),
                ..Default::default()
            }));
            session
                .run_text("Remember the authored request".to_string(), |_| {})
                .await
                .unwrap();
            let cx = crate::agent_cx::AgentCx::for_request();
            let path = shared.lock(cx.cx()).await.unwrap().path.clone().unwrap();
            let reopened = crate::session::Session::open(path.to_str().unwrap())
                .await
                .unwrap();
            let messages = reopened.to_messages_for_current_path();
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| recovery_nudge(message))
                    .count(),
                1
            );
            assert!(messages.iter().any(|message| {
                matches!(message, Message::Assistant(assistant)
                    if is_empty_assistant_response(assistant) && assistant.usage.output == 7)
            }));
            assert_eq!(
                reopened
                    .entries_for_current_path()
                    .iter()
                    .filter(|entry| {
                        matches!(entry, crate::session::SessionEntry::Custom(custom)
                            if custom.custom_type == "pi.memory.input.v1")
                    })
                    .count(),
                1,
                "generated recovery must not count as another authored memory-learning turn"
            );
            assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
        });
    }

    #[test]
    fn length_stop_is_budget_truncated() {
        assert_eq!(
            classify(StopReason::Length, "half a sentence"),
            RecoveryClass::BudgetTruncated
        );
    }

    #[test]
    fn clean_prose_is_clean() {
        assert_eq!(
            classify(StopReason::Stop, "All done. The tests pass."),
            RecoveryClass::CleanStop
        );
    }

    #[test]
    fn non_stop_reasons_never_classify() {
        for reason in [
            StopReason::ToolUse,
            StopReason::Error,
            StopReason::Aborted,
            StopReason::PauseTurn,
        ] {
            assert_eq!(
                classify(reason, "```\nunclosed"),
                RecoveryClass::CleanStop,
                "{reason:?}"
            );
        }
    }

    #[test]
    fn unclosed_fence_detected() {
        let text = "Here is the fix:\n```rust\nfn main() {\n    let x = 1;";
        assert_eq!(
            classify(StopReason::Stop, text),
            RecoveryClass::UnclosedStructure
        );
        let closed = "Here is the fix:\n```rust\nfn main() {}\n```\nDone.";
        assert_eq!(classify(StopReason::Stop, closed), RecoveryClass::CleanStop);
    }

    #[test]
    fn completed_fences_can_contain_literal_delimiters() {
        for text in [
            "````markdown\n```rust\nfn main() {}\n```\n````",
            "~~~text\n``` is literal code\n~~~",
            "```text\n~~~ is literal code\n```",
            "~~~rust\nlet x = 1;\n~~~~",
            "  ```text\npayload\n   ```\t",
            "~~~example with `backticks`\ncontent\n~~~",
            "```rust\nfn main() {}\n```\n~~~text\ndone\n~~~",
        ] {
            assert_eq!(
                classify(StopReason::Stop, text),
                RecoveryClass::CleanStop,
                "completed fence: {text:?}"
            );
        }
    }

    #[test]
    fn fence_closers_must_match_the_opener() {
        for text in [
            "~~~rust\nfn main() {",
            "````text\ncontent\n```",
            "~~~text\ncontent\n```",
            "```text\ncontent\n~~~",
            "```text\ncontent\n```not a closing fence",
            "```text\ncontent\n    ```",
            "~~~text\ncontent\n~~",
        ] {
            assert_eq!(
                classify(StopReason::Stop, text),
                RecoveryClass::UnclosedStructure,
                "unfinished fence: {text:?}"
            );
        }
    }

    #[test]
    fn literal_backticks_do_not_open_a_fence() {
        for text in [
            "    ```rust\n    literal indented code",
            "\t```rust\n\tliteral indented code",
            "```invalid`info\nordinary prose",
            "Inline ```code``` is complete.",
            "Unicode prose: 日本語, café, 🦀.",
        ] {
            assert_eq!(
                classify(StopReason::Stop, text),
                RecoveryClass::CleanStop,
                "not a fence: {text:?}"
            );
        }
    }

    #[test]
    fn dangling_bullet_detected() {
        let text = "Plan:\n1. read the file\n2.";
        assert_eq!(
            classify(StopReason::Stop, text),
            RecoveryClass::UnclosedStructure
        );
        let fine = "Plan:\n1. read the file\n2. edit it";
        assert_eq!(classify(StopReason::Stop, fine), RecoveryClass::CleanStop);
    }

    #[test]
    fn ordered_dangling_bullets_require_list_context() {
        for text in [
            "42.",
            "The answer is:\n42.",
            "1.",
            "Plan:\n1.read the file\n2.",
            "Plan:\n1. read the file\n2)",
            "Plan:\n1. read the file\n  2.",
            "Plan:\n1. read the file\n1234567890.",
            "    1. literal code\n    2.",
            "    -",
            "\t-",
        ] {
            assert_eq!(
                classify(StopReason::Stop, text),
                RecoveryClass::CleanStop,
                "ambiguous or literal marker: {text:?}"
            );
        }
        for text in [
            "1. read the file\n2.",
            "1) read the file\n2)",
            "1. read the file\n\n2.   \n",
            "  1. read the file\n  2.",
            "1.\tread the file\n2.",
            "Plan:\n-",
            "Plan:\n*",
            "Plan:\n+",
        ] {
            assert_eq!(
                classify(StopReason::Stop, text),
                RecoveryClass::UnclosedStructure,
                "unfinished list: {text:?}"
            );
        }
    }

    #[test]
    fn completed_markdown_does_not_spend_recovery_budget() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        for text in [
            "42.",
            "~~~markdown\n``` literal fence\n~~~",
            "    ``` literal indented code",
        ] {
            assert!(state.evaluate(StopReason::Stop, text).is_none(), "{text:?}");
        }
        assert_eq!(state.continuations(), 0);
        assert!(state.evaluate(StopReason::Length, "cut off").is_some());
        assert_eq!(state.continuations(), 1);
    }

    #[test]
    fn unfulfilled_promise_detected() {
        let text = "The bug is in parse(). I will now edit the three files.";
        assert_eq!(
            classify(StopReason::Stop, text),
            RecoveryClass::SemanticPrematureStop
        );
        let fulfilled =
            "I will now edit the file. Done — the change is applied and the test passes.";
        assert_eq!(
            classify(StopReason::Stop, fulfilled),
            RecoveryClass::CleanStop
        );
    }

    #[test]
    fn mode_gating_matrix() {
        let semantic = "I'll now update the config.";
        let budget_text = "cut off";

        let mut off = TurnRecoveryState::new(TurnRecoveryMode::Off);
        assert!(off.evaluate(StopReason::Length, budget_text).is_none());

        let mut conservative = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        assert!(
            conservative
                .evaluate(StopReason::Length, budget_text)
                .is_some(),
            "conservative handles budget truncation"
        );
        assert!(
            conservative.evaluate(StopReason::Stop, semantic).is_none(),
            "conservative ignores the semantic class"
        );

        let mut aggressive = TurnRecoveryState::new(TurnRecoveryMode::Aggressive);
        assert!(
            aggressive.evaluate(StopReason::Stop, semantic).is_some(),
            "aggressive handles the semantic class"
        );
    }

    #[test]
    fn cap_stops_after_two() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        assert!(state.evaluate(StopReason::Length, "a").is_some());
        assert!(state.evaluate(StopReason::Length, "b").is_some());
        assert!(
            state.evaluate(StopReason::Length, "c").is_none(),
            "third auto-continue must be refused"
        );
        assert_eq!(state.continuations(), 2);
    }

    #[test]
    fn clean_stops_do_not_consume_the_cap() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        for _ in 0..10 {
            assert!(state.evaluate(StopReason::Stop, "all done.").is_none());
        }
        assert_eq!(state.continuations(), 0);
    }

    #[test]
    fn structure_heuristics_only_apply_to_the_first_stop() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        assert!(
            state
                .evaluate(StopReason::Length, "```rust\nfn main() {")
                .is_some()
        );
        assert!(
            state
                .evaluate(StopReason::Stop, "}\n```\nAll done.")
                .is_none(),
            "a continuation closing an earlier fence must not re-trigger"
        );
        assert!(
            state
                .evaluate(StopReason::Length, "more truncation")
                .is_some(),
            "provider-signaled truncation still continues"
        );
    }

    #[test]
    fn nudge_text_carries_reason_and_counter() {
        let mut state = TurnRecoveryState::new(TurnRecoveryMode::Conservative);
        let action = state
            .evaluate(StopReason::Length, "partial")
            .expect("actionable");
        assert!(action.nudge_text.contains("auto-continue 1/2"));
        assert!(action.nudge_text.contains("token budget"));
    }
}
