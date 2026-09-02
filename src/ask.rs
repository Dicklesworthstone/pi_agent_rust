//! The `ask` tool: structured mid-turn option picker (bd-cv653.3.8).
//!
//! Ambiguity routes through `ask` instead of prose: the model presents 2-5
//! options per question (optionally multi-select, optionally with a
//! recommended choice) and the host surfaces a picker — the TUI as an
//! option card, RPC/ACP as request frames — then the selection returns as
//! the tool result.
//!
//! Layering (deliberate, per the chrome-epic stack-layering rule): this
//! module owns the QUESTION MODEL, validation, policy resolution, and
//! answer formatting only. Rendering lives entirely behind [`AskHandler`],
//! which hosts install; a session with no handler (print/JSON mode, SDK
//! embedders without UI) resolves through [`AskPolicy`] instead of hanging.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Schema identifier for the tool-result details payload.
pub const ASK_RESPONSE_SCHEMA: &str = "ask_response.v1";

/// Bounds on the option list, per the omp card contract.
pub const MIN_OPTIONS: usize = 2;
pub const MAX_OPTIONS: usize = 5;
/// Bound on questions per call (one card each).
pub const MAX_QUESTIONS: usize = 4;

/// One selectable option.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One question card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestion {
    /// Stable identifier for pairing answers; defaults to the question index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub question: String,
    /// Short chip/tag label for compact card headers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub options: Vec<AskOption>,
    /// Index into `options` of the recommended choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
    /// Allow selecting multiple options.
    #[serde(default)]
    pub multi: bool,
}

/// A validated ask request (invariants enforced by [`validate_request`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRequest {
    pub questions: Vec<AskQuestion>,
}

/// Answer to one question.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AskAnswer {
    /// The question's `id` (or its index rendered as a string).
    pub question_id: String,
    /// Selected option labels (one unless the question was `multi`).
    pub selected: Vec<String>,
    /// Free-text answer when the user chose "Other".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

/// The host's response to an ask request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskResponse {
    pub answers: Vec<AskAnswer>,
    /// The user dismissed/cancelled the card instead of answering.
    #[serde(default)]
    pub dismissed: bool,
}

/// Host-installed picker surface. Implementations own their timeout; a
/// dismissal is expressed via [`AskResponse::dismissed`], errors via `Err`.
pub type AskHandler = Arc<
    dyn Fn(AskRequest) -> futures::future::BoxFuture<'static, Result<AskResponse>> + Send + Sync,
>;

/// Non-interactive resolution policy when no picker surface exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskPolicy {
    /// Auto-answer with the recommended option (or the first option when no
    /// recommendation is given), loudly annotating the result.
    #[default]
    Recommended,
    /// Fail the tool call: this session cannot answer questions.
    Error,
}

impl AskPolicy {
    /// Parse a settings string; unknown values fall back to the default with
    /// a warning.
    #[must_use]
    pub fn from_config(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("error") => Self::Error,
            Some("recommended" | "") | None => Self::Recommended,
            Some(other) => {
                tracing::warn!(
                    "unknown ask_policy setting '{other}' (expected 'recommended' or 'error'); using recommended"
                );
                Self::Recommended
            }
        }
    }
}

/// Validate the raw tool input into an [`AskRequest`].
pub fn validate_request(request: AskRequest) -> Result<AskRequest> {
    if request.questions.is_empty() {
        return Err(Error::validation("ask requires at least one question"));
    }
    if request.questions.len() > MAX_QUESTIONS {
        return Err(Error::validation(format!(
            "ask accepts at most {MAX_QUESTIONS} questions per call"
        )));
    }
    let mut seen_ids = std::collections::HashSet::new();
    // ubs:ignore-block validation loop: every format! below is a cold
    // return-error exit taken at most once, not a hot-loop allocation.
    for (index, question) in request.questions.iter().enumerate() {
        if question.question.trim().is_empty() {
            return Err(Error::validation(format!(
                "question {index} must not be blank"
            )));
        }
        if question.options.len() < MIN_OPTIONS || question.options.len() > MAX_OPTIONS {
            return Err(Error::validation(format!(
                "question {index} needs {MIN_OPTIONS}-{MAX_OPTIONS} options (got {})",
                question.options.len()
            )));
        }
        if let Some(option) = question
            .options
            .iter()
            .find(|option| option.label.trim().is_empty())
        {
            let _ = option;
            return Err(Error::validation(format!(
                "question {index} has an option with a blank label"
            )));
        }
        if let Some(recommended) = question.recommended
            && recommended >= question.options.len()
        {
            return Err(Error::validation(format!(
                "question {index}: recommended index {recommended} is out of bounds for {} options",
                question.options.len()
            )));
        }
        if !seen_ids.insert(effective_question_id(question, index)) {
            return Err(Error::validation(format!(
                "question {index} reuses an id; ids must be unique"
            )));
        }
    }
    Ok(request)
}

/// A question's answer-pairing id: explicit `id`, else its index.
#[must_use]
pub fn effective_question_id(question: &AskQuestion, index: usize) -> String {
    question.id.clone().unwrap_or_else(|| index.to_string())
}

/// Resolve a validated request under a non-interactive policy.
pub fn resolve_by_policy(request: &AskRequest, policy: AskPolicy) -> Result<AskResponse> {
    match policy {
        AskPolicy::Error => Err(Error::tool(
            "ask",
            "this session has no interactive picker and ask_policy is 'error'",
        )),
        AskPolicy::Recommended => Ok(AskResponse {
            answers: request
                .questions
                .iter()
                .enumerate()
                .map(|(index, question)| {
                    let choice = question.recommended.unwrap_or(0);
                    AskAnswer {
                        question_id: effective_question_id(question, index),
                        selected: question
                            .options
                            .get(choice)
                            .map(|option| vec![option.label.clone()])
                            .unwrap_or_default(),
                        other: None,
                    }
                })
                .collect(),
            dismissed: false,
        }),
    }
}

/// Render the tool-result text for a resolved response.
#[must_use]
pub fn render_answers(request: &AskRequest, response: &AskResponse, auto: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    if auto {
        let _ = writeln!(
            out,
            "[non-interactive session: answered with the recommended option(s)]"
        );
    }
    for (index, question) in request.questions.iter().enumerate() {
        let id = effective_question_id(question, index);
        let answer = response
            .answers
            .iter()
            .find(|answer| answer.question_id == id);
        let rendered = answer.map_or_else(
            || "(unanswered)".to_string(),
            |answer| {
                answer.other.as_deref().map_or_else(
                    || {
                        if answer.selected.is_empty() {
                            "(unanswered)".to_string()
                        } else {
                            answer.selected.join(", ")
                        }
                    },
                    |other| format!("Other: {other}"),
                )
            },
        );
        let _ = writeln!(out, "Q: {}\nA: {rendered}", question.question.trim());
    }
    out.trim_end().to_string()
}

/// Picker card wait budget for interactive surfaces.
pub const ASK_UI_TIMEOUT_MS: u64 = 300_000;

/// A picker request in flight to an interactive surface.
#[derive(Debug, Clone)]
pub struct AskUiRequest {
    pub id: String,
    pub request: AskRequest,
}

/// One question's parsed reply from raw picker input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionReply {
    /// The user cancelled the card.
    Cancel,
    /// Selected option labels (possibly several for `multi`).
    Selected(Vec<String>),
    /// Free-text "Other" answer.
    Other(String),
}

/// Parse raw picker input for one question.
///
/// `cancel` dismisses; numbers (1-based) or exact labels select —
/// comma-separated when `multi`; anything else is a free-text "Other"
/// answer. Returns `Err(message)` for selections that reference unknown
/// options mixed with known ones (likely typos).
pub fn parse_question_reply(
    question: &AskQuestion,
    input: &str,
) -> std::result::Result<QuestionReply, String> {
    let trimmed = input.trim();
    if trimmed.eq_ignore_ascii_case("cancel") {
        return Ok(QuestionReply::Cancel);
    }
    let resolve = |token: &str| -> Option<String> {
        let token = token.trim();
        if let Ok(number) = token.parse::<usize>()
            && (1..=question.options.len()).contains(&number)
        {
            return question
                .options
                .get(number - 1)
                .map(|option| option.label.clone());
        }
        question
            .options
            .iter()
            .find(|option| option.label.eq_ignore_ascii_case(token))
            .map(|option| option.label.clone())
    };

    if question.multi && trimmed.contains(',') {
        let tokens: Vec<&str> = trimmed.split(',').map(str::trim).collect();
        let resolved: Vec<Option<String>> = tokens.iter().map(|token| resolve(token)).collect();
        if resolved.iter().all(Option::is_some) {
            let mut selected: Vec<String> = resolved.into_iter().flatten().collect();
            selected.dedup();
            return Ok(QuestionReply::Selected(selected));
        }
        if resolved.iter().any(Option::is_some) {
            return Err("mixed known and unknown selections; use option numbers, exact labels, or free text".to_string());
        }
        return Ok(QuestionReply::Other(trimmed.to_string()));
    }

    resolve(trimmed).map_or_else(
        || Ok(QuestionReply::Other(trimmed.to_string())),
        |label| Ok(QuestionReply::Selected(vec![label])),
    )
}

/// Plain-text card rendering for one question (host styles it).
#[must_use]
pub fn format_question_card(question: &AskQuestion, index: usize, total: usize) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let header = question.header.as_deref().unwrap_or("ask");
    let _ = writeln!(out, "[{header}] question {} of {total}", index + 1);
    let _ = writeln!(out, "{}", question.question.trim());
    for (option_index, option) in question.options.iter().enumerate() {
        let badge = if question.recommended == Some(option_index) {
            " (recommended)"
        } else {
            ""
        };
        match &option.description {
            Some(description) => {
                let _ = writeln!(
                    out,
                    "  {}) {}{badge} — {description}",
                    option_index + 1,
                    option.label
                );
            }
            None => {
                let _ = writeln!(out, "  {}) {}{badge}", option_index + 1, option.label);
            }
        }
    }
    let hint = if question.multi {
        "Enter numbers/labels (comma-separated for several), free text for Other, or 'cancel'."
    } else {
        "Enter a number, label, free text for Other, or 'cancel'."
    };
    out.push_str(hint);
    out
}

type PendingAskReplies = Arc<
    std::sync::Mutex<
        std::collections::HashMap<String, asupersync::channel::oneshot::Sender<AskResponse>>,
    >,
>;

struct PendingAskReplyLease {
    pending: PendingAskReplies,
    id: String,
}

impl Drop for PendingAskReplyLease {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

/// The `ask` tool. Logic-only: rendering is the installed handler's job.
///
/// Cloning shares the handler slot, so the host can keep one clone to
/// install its picker surface after the registry boxed another.
#[derive(Clone)]
pub struct AskTool {
    handler: Arc<std::sync::RwLock<Option<AskHandler>>>,
    pending_ui: PendingAskReplies,
    channel_ui_open: Arc<std::sync::atomic::AtomicBool>,
    policy: AskPolicy,
}

impl AskTool {
    #[must_use]
    pub fn new(policy: AskPolicy) -> Self {
        Self {
            handler: Arc::new(std::sync::RwLock::new(None)),
            pending_ui: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            channel_ui_open: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            policy,
        }
    }

    /// Install the host's picker surface (shared across clones).
    pub fn set_handler(&self, handler: AskHandler) {
        *self
            .handler
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handler);
    }

    /// Install a channel-based picker surface (the TUI pattern): requests
    /// flow out through `sender`, and the host answers via
    /// [`Self::respond_ui`]. Mirrors the extension-UI request/response
    /// bridge, including the wait budget.
    // Guard scope is deliberate; tightening drops would change lock-hold semantics.
    #[allow(clippy::significant_drop_tightening)]
    pub fn install_channel_ui(&self, sender: asupersync::channel::mpsc::Sender<AskUiRequest>) {
        let pending = Arc::clone(&self.pending_ui);
        let channel_ui_open = Arc::clone(&self.channel_ui_open);
        let handler: AskHandler = Arc::new(move |request: AskRequest| {
            let sender = sender.clone();
            let pending = Arc::clone(&pending);
            let channel_ui_open = Arc::clone(&channel_ui_open);
            Box::pin(async move {
                let cx = crate::agent_cx::AgentCx::for_current_or_request();
                if !channel_ui_open.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(Error::tool("ask", "picker surface closed"));
                }
                let id = uuid::Uuid::new_v4().to_string();
                let (reply_tx, mut reply_rx) = asupersync::channel::oneshot::channel();
                let ui_request = AskUiRequest {
                    id: id.clone(),
                    request,
                };
                {
                    let mut pending = pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !channel_ui_open.load(std::sync::atomic::Ordering::Acquire) {
                        return Err(Error::tool("ask", "picker surface closed"));
                    }
                    pending.insert(id.clone(), reply_tx);
                    if sender.try_send(ui_request).is_err() {
                        pending.remove(&id);
                        return Err(Error::tool("ask", "picker surface unavailable"));
                    }
                }
                // Cancellation can drop this future without entering any of
                // the result branches below. Keep removal tied to ownership
                // of the pending wait rather than to selected exit paths.
                let _pending_reply = PendingAskReplyLease {
                    pending: Arc::clone(&pending),
                    id: id.clone(),
                };
                let waited = asupersync::time::timeout(
                    asupersync::time::wall_now(),
                    std::time::Duration::from_millis(ASK_UI_TIMEOUT_MS),
                    reply_rx.recv(cx.cx()),
                )
                .await;
                match waited {
                    Ok(Ok(response)) => Ok(response),
                    Ok(Err(_)) => {
                        pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&id);
                        Err(Error::tool("ask", "picker surface closed"))
                    }
                    Err(_) => {
                        pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&id);
                        Err(Error::tool(
                            "ask",
                            "question timed out waiting for the user",
                        ))
                    }
                }
            })
        });
        let _pending_guard = self
            .pending_ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.channel_ui_open
            .store(true, std::sync::atomic::Ordering::Release);
        self.set_handler(handler);
    }

    /// Deliver the host's answer for a pending picker request.
    pub fn respond_ui(&self, id: &str, response: AskResponse) -> bool {
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let sender = self
            .pending_ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        sender.is_some_and(|sender| sender.send(cx.cx(), response).is_ok())
    }

    /// Forward one queued picker request only while its reply surface remains
    /// open and the request is still pending.
    ///
    /// The nonblocking emitter runs under the same mutex as [`Self::close_channel_ui`],
    /// so a host either publishes the request before close linearizes or drops
    /// it after close. If host backpressure rejects the frame, the waiter is
    /// dismissed immediately rather than becoming unreachable. `emit` must be
    /// nonblocking and must not re-enter this `AskTool`.
    pub(crate) fn try_forward_channel_ui_request(
        &self,
        id: &str,
        emit: impl FnOnce() -> bool,
    ) -> bool {
        let failed_sender = {
            let mut pending = self
                .pending_ui
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !self
                .channel_ui_open
                .load(std::sync::atomic::Ordering::Acquire)
                || !pending.contains_key(id)
            {
                return false;
            }
            if emit() {
                return true;
            }
            pending.remove(id)
        };
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        if let Some(sender) = failed_sender {
            let _ = sender.send(
                cx.cx(),
                AskResponse {
                    answers: Vec::new(),
                    dismissed: true,
                },
            );
        }
        false
    }

    /// Whether this exact channel request still owns a reachable reply sender.
    /// UI consumers recheck after queueing so a card dismissed by terminal
    /// close cannot become visible later from a buffered host event.
    pub(crate) fn channel_ui_request_is_pending(&self, id: &str) -> bool {
        self.channel_ui_open
            .load(std::sync::atomic::Ordering::Acquire)
            && self
                .pending_ui
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains_key(id)
    }

    /// Test-only: mark `id` pending exactly as the installed channel handler
    /// does before it emits an `AskUiRequest`, so cards that tests inject
    /// directly pass the pending-id guard in the interactive hosts. The reply
    /// receiver is dropped on purpose: answering such a card reports expiry,
    /// which is what the direct-injection tests assert.
    #[cfg(test)]
    pub(crate) fn register_channel_ui_request_for_tests(&self, id: &str) {
        let (reply_tx, _reply_rx) = asupersync::channel::oneshot::channel();
        self.channel_ui_open
            .store(true, std::sync::atomic::Ordering::Release);
        self.pending_ui
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), reply_tx);
    }

    /// Close the installed channel UI and dismiss every outstanding card.
    ///
    /// Hosts call this when their reply surface disconnects so an agent turn
    /// blocked in `ask` cannot outlive the UI by the full picker timeout.
    pub fn close_channel_ui(&self) -> usize {
        let pending = {
            let mut pending = self
                .pending_ui
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.channel_ui_open
                .store(false, std::sync::atomic::Ordering::Release);
            let closed_handler: AskHandler = Arc::new(|_request: AskRequest| {
                Box::pin(async { Err(Error::tool("ask", "picker surface closed")) })
            });
            self.set_handler(closed_handler);
            std::mem::take(&mut *pending)
        };
        let count = pending.len();
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        for (_, sender) in pending {
            let _ = sender.send(
                cx.cx(),
                AskResponse {
                    answers: Vec::new(),
                    dismissed: true,
                },
            );
        }
        count
    }

    fn handler(&self) -> Option<AskHandler> {
        self.handler
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Route a request through the installed interactive picker surface.
    ///
    /// Unlike tool execution, this NEVER falls back to [`AskPolicy`]
    /// auto-answering: callers use it for host-originated prompts (tool
    /// approval, issue #196) where an automatic "recommended" answer would
    /// silently approve on the user's behalf. With no surface installed
    /// (print/JSON mode, headless SDK embedders) it errors, which approval
    /// callers map to a deny.
    pub async fn prompt_installed(&self, request: AskRequest) -> Result<AskResponse> {
        let Some(handler) = self.handler() else {
            return Err(Error::tool(
                "ask",
                "no interactive picker surface is installed in this session",
            ));
        };
        handler(request).await
    }
}

/// Option labels for the approval card built by [`approval_handler_via_ask`].
pub const APPROVAL_ALLOW_LABEL: &str = "Allow";
pub const APPROVAL_DENY_LABEL: &str = "Deny";

/// Bounded preview of a tool call's arguments for the approval card.
///
/// Bash-style calls show the command line directly; everything else shows
/// pretty-printed JSON, truncated on a char boundary so a huge `write` body
/// cannot flood the card.
fn approval_arguments_preview(arguments: &serde_json::Value) -> String {
    const MAX_PREVIEW_CHARS: usize = 700;

    let rendered = arguments
        .get("command")
        .or_else(|| arguments.get("cmd"))
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || serde_json::to_string_pretty(arguments).unwrap_or_else(|_| arguments.to_string()),
            |command| format!("$ {command}"),
        );
    if rendered.chars().count() > MAX_PREVIEW_CHARS {
        let truncated: String = rendered.chars().take(MAX_PREVIEW_CHARS).collect();
        format!("{truncated}\n… (arguments truncated)")
    } else {
        rendered
    }
}

/// Builds a [`crate::agent::ToolApprovalHandler`] that surfaces approval
/// prompts through the session's interactive ask surface (issue #196).
///
/// Approval-mode gating used to deny silently in interactive sessions
/// because nothing installed a `tool_approval` handler. This bridge reuses
/// the ask question-card machinery every interactive surface (classic TUI,
/// ftui, RPC) already implements: the prompt renders as a standard
/// Allow/Deny card, the reply maps onto [`crate::agent::ToolApprovalDecision`],
/// and every non-affirmative outcome — deny, dismissal, free-text answer,
/// timeout, or no surface installed at all — fails closed to a deny with an
/// explicit reason the model can relay.
#[must_use]
pub fn approval_handler_via_ask(ask: AskTool) -> crate::agent::ToolApprovalHandler {
    Arc::new(move |request: crate::agent::ToolApprovalRequest| {
        let ask = ask.clone();
        Box::pin(async move {
            use crate::agent::ToolApprovalDecision;

            let question = AskQuestion {
                id: Some(format!("approval:{}", request.tool_call_id)),
                question: format!(
                    "Allow the `{}` tool to run?\n{}",
                    request.tool_name,
                    approval_arguments_preview(&request.arguments)
                ),
                header: Some("approval".to_string()),
                options: vec![
                    AskOption {
                        label: APPROVAL_ALLOW_LABEL.to_string(),
                        description: Some("Run this tool call".to_string()),
                    },
                    AskOption {
                        label: APPROVAL_DENY_LABEL.to_string(),
                        description: Some("Reject this tool call".to_string()),
                    },
                ],
                recommended: None,
                multi: false,
            };
            match ask
                .prompt_installed(AskRequest {
                    questions: vec![question],
                })
                .await
            {
                Ok(response) => {
                    if response.dismissed {
                        return ToolApprovalDecision::deny(format!(
                            "user dismissed the approval prompt for `{}`",
                            request.tool_name
                        ));
                    }
                    let allowed = response.answers.first().is_some_and(|answer| {
                        answer.other.is_none()
                            && answer
                                .selected
                                .iter()
                                .any(|label| label.eq_ignore_ascii_case(APPROVAL_ALLOW_LABEL))
                    });
                    if allowed {
                        ToolApprovalDecision::Allow
                    } else {
                        ToolApprovalDecision::deny(format!(
                            "user denied approval for `{}`",
                            request.tool_name
                        ))
                    }
                }
                Err(error) => ToolApprovalDecision::deny(format!(
                    "approval required for `{}` but the prompt could not be completed: {error}",
                    request.tool_name
                )),
            }
        })
    })
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl crate::tools::Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }

    fn label(&self) -> &str {
        "Ask"
    }

    fn description(&self) -> &str {
        "Ask the user structured questions mid-turn instead of guessing. Each question card offers 2-5 mutually exclusive options (set multi=true to allow several); mark the option you would pick with `recommended` (its index) and put it first. Use ONLY when different answers lead to materially different work — never for choices with an obvious default. The user can always answer with free text ('Other'). In non-interactive sessions the recommended option is auto-selected (or the call errors, per ask_policy)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_QUESTIONS,
                    "items": {
                        "type": "object",
                        "required": ["question", "options"],
                        "properties": {
                            "id": {"type": "string", "description": "Stable id for pairing the answer (defaults to the question index)."},
                            "question": {"type": "string", "description": "The complete question, ending with a question mark."},
                            "header": {"type": "string", "description": "Very short chip label (max ~12 chars)."},
                            "options": {
                                "type": "array",
                                "minItems": MIN_OPTIONS,
                                "maxItems": MAX_OPTIONS,
                                "items": {
                                    "type": "object",
                                    "required": ["label"],
                                    "properties": {
                                        "label": {"type": "string", "description": "Concise display text (1-5 words)."},
                                        "description": {"type": "string", "description": "What choosing this means."}
                                    }
                                }
                            },
                            "recommended": {"type": "integer", "description": "Index of the recommended option."},
                            "multi": {"type": "boolean", "default": false}
                        }
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        })
    }

    fn effects(&self) -> crate::tools::ToolEffects {
        // A host exposes one modal picker at a time. Treat user interaction as
        // a scheduling barrier so one model turn cannot strand concurrent Ask
        // requests behind a single active-card slot.
        crate::tools::ToolEffects::write()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(crate::tools::ToolUpdate) + Send + Sync>>,
    ) -> Result<crate::tools::ToolOutput> {
        let request: AskRequest = serde_json::from_value(input)
            .map_err(|error| Error::validation(format!("Invalid ask input: {error}")))?; // ubs:ignore false positive: cold error path
        let request = validate_request(request)?;

        let (response, auto) = if let Some(handler) = self.handler() {
            let response = handler(request.clone()).await?;
            if response.dismissed {
                return Err(Error::tool("ask", "user dismissed the question"));
            }
            (response, false)
        } else {
            (resolve_by_policy(&request, self.policy)?, true)
        };

        if auto {
            tracing::info!(
                event = "pi.ask.auto_answered",
                questions = request.questions.len(),
                "ask auto-answered with recommended options (non-interactive session)"
            );
        }

        let details = serde_json::json!({
            "schema": ASK_RESPONSE_SCHEMA,
            "answers": response.answers,
            "autoAnswered": auto,
        });
        Ok(crate::tools::ToolOutput {
            content: vec![crate::model::ContentBlock::Text(
                crate::model::TextContent::new(render_answers(&request, &response, auto)),
            )],
            details: Some(details),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool as _;

    fn question(json: serde_json::Value) -> AskRequest {
        serde_json::from_value(json).expect("parse ask request")
    }

    /// Validation matrix: option-count bounds, recommended bounds, blank
    /// content, duplicate ids, question-count bound.
    #[test]
    fn validation_matrix() {
        let valid = question(serde_json::json!({
            "questions": [{
                "question": "Which path?",
                "options": [{"label": "A"}, {"label": "B"}],
                "recommended": 1
            }]
        }));
        assert!(validate_request(valid).is_ok());

        for (label, bad) in [
            (
                "one option",
                serde_json::json!({"questions": [{"question": "Q?", "options": [{"label": "A"}]}]}),
            ),
            (
                "six options",
                serde_json::json!({"questions": [{"question": "Q?", "options": [
                    {"label": "1"}, {"label": "2"}, {"label": "3"},
                    {"label": "4"}, {"label": "5"}, {"label": "6"}
                ]}]}),
            ),
            (
                "recommended out of bounds",
                serde_json::json!({"questions": [{"question": "Q?", "recommended": 2,
                    "options": [{"label": "A"}, {"label": "B"}]}]}),
            ),
            (
                "blank question",
                serde_json::json!({"questions": [{"question": "  ",
                    "options": [{"label": "A"}, {"label": "B"}]}]}),
            ),
            (
                "blank option label",
                serde_json::json!({"questions": [{"question": "Q?",
                    "options": [{"label": "A"}, {"label": " "}]}]}),
            ),
            (
                "duplicate ids",
                serde_json::json!({"questions": [
                    {"id": "x", "question": "Q1?", "options": [{"label": "A"}, {"label": "B"}]},
                    {"id": "x", "question": "Q2?", "options": [{"label": "A"}, {"label": "B"}]}
                ]}),
            ),
            ("no questions", serde_json::json!({"questions": []})),
        ] {
            assert!(
                validate_request(question(bad)).is_err(),
                "must reject: {label}"
            );
        }
    }

    /// Policy resolution: recommended picks the marked option (or the first
    /// without a mark); error policy fails.
    #[test]
    fn policy_resolution() {
        let request = validate_request(question(serde_json::json!({
            "questions": [
                {"id": "q1", "question": "Pick?", "recommended": 1,
                 "options": [{"label": "A"}, {"label": "B"}]},
                {"question": "Fallback?", "options": [{"label": "X"}, {"label": "Y"}]}
            ]
        })))
        .expect("valid");

        let resolved = resolve_by_policy(&request, AskPolicy::Recommended).expect("resolve");
        assert_eq!(resolved.answers.len(), 2);
        assert_eq!(resolved.answers[0].question_id, "q1");
        assert_eq!(resolved.answers[0].selected, vec!["B"]);
        assert_eq!(resolved.answers[1].question_id, "1");
        assert_eq!(resolved.answers[1].selected, vec!["X"]);

        assert!(resolve_by_policy(&request, AskPolicy::Error).is_err());
        assert_eq!(AskPolicy::from_config(Some("error")), AskPolicy::Error);
        assert_eq!(AskPolicy::from_config(None), AskPolicy::Recommended);
        assert_eq!(
            AskPolicy::from_config(Some("bogus")),
            AskPolicy::Recommended
        );
    }

    #[test]
    fn ask_tool_is_a_parallel_scheduling_barrier() {
        let effects = AskTool::new(AskPolicy::Error).effects();
        assert!(effects.writes());
        assert!(!effects.parallel_safe());
    }

    /// Acceptance 3: no handler + recommended policy auto-answers with a
    /// loud notice and autoAnswered details flag.
    #[test]
    fn auto_answer_path_via_tool() {
        asupersync::test_utils::run_test(|| async {
            let tool = AskTool::new(AskPolicy::Recommended);
            let output = tool
                .execute(
                    "ask-1",
                    serde_json::json!({
                        "questions": [{
                            "question": "Deploy now?",
                            "recommended": 0,
                            "options": [
                                {"label": "Deploy", "description": "ship it"},
                                {"label": "Wait"}
                            ]
                        }]
                    }),
                    None,
                )
                .await
                .expect("auto-answer");
            let crate::model::ContentBlock::Text(text) = &output.content[0] else {
                unreachable!("ask renders text");
            };
            assert!(text.text.contains("non-interactive session"));
            assert!(text.text.contains("A: Deploy"));
            let details = output.details.expect("details");
            assert_eq!(details["schema"], ASK_RESPONSE_SCHEMA);
            assert_eq!(details["autoAnswered"], true);
        });
    }

    /// Handler path: selections flow through; dismissal becomes the
    /// documented tool error (acceptance 4's logic half).
    #[test]
    fn handler_path_and_dismissal() {
        asupersync::test_utils::run_test(|| async {
            let tool = AskTool::new(AskPolicy::Recommended);
            tool.set_handler(Arc::new(|request: AskRequest| {
                Box::pin(async move {
                    Ok(AskResponse {
                        answers: request
                            .questions
                            .iter()
                            .enumerate()
                            .map(|(index, question)| AskAnswer {
                                question_id: effective_question_id(question, index),
                                selected: vec![question.options[1].label.clone()],
                                other: None,
                            })
                            .collect(),
                        dismissed: false,
                    })
                })
            }));
            let output = tool
                .execute(
                    "ask-2",
                    serde_json::json!({
                        "questions": [{
                            "question": "Pick?",
                            "options": [{"label": "A"}, {"label": "B"}]
                        }]
                    }),
                    None,
                )
                .await
                .expect("handler answer");
            let crate::model::ContentBlock::Text(text) = &output.content[0] else {
                unreachable!("ask renders text");
            };
            assert!(text.text.contains("A: B"));
            assert!(!text.text.contains("non-interactive"));

            tool.set_handler(Arc::new(|_request: AskRequest| {
                Box::pin(async move {
                    Ok(AskResponse {
                        answers: Vec::new(),
                        dismissed: true,
                    })
                })
            }));
            let error = tool
                .execute(
                    "ask-3",
                    serde_json::json!({
                        "questions": [{
                            "question": "Pick?",
                            "options": [{"label": "A"}, {"label": "B"}]
                        }]
                    }),
                    None,
                )
                .await
                .expect_err("dismissal errors");
            assert!(error.to_string().contains("dismissed"), "{error}"); // ubs:ignore test assertion
        });
    }

    /// Channel-UI bridge round trip: the handler queues an AskUiRequest,
    /// respond_ui delivers the answer back into the awaiting execute call.
    #[test]
    fn channel_ui_round_trip() {
        asupersync::test_utils::run_test(|| async {
            let tool = AskTool::new(AskPolicy::Error);
            let (tx, mut rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
            tool.install_channel_ui(tx);

            let responder_tool = tool.clone();
            let cx = crate::agent_cx::AgentCx::for_request();
            let responder = async {
                let request = rx.recv(cx.cx()).await.expect("ui request arrives");
                assert_eq!(request.request.questions.len(), 1);
                let answered = responder_tool.respond_ui(
                    &request.id,
                    AskResponse {
                        answers: vec![AskAnswer {
                            question_id: "q".to_string(),
                            selected: vec!["B".to_string()],
                            other: None,
                        }],
                        dismissed: false,
                    },
                );
                assert!(answered, "pending reply slot must exist");
            };
            let execute = tool.execute(
                "ask-rt",
                serde_json::json!({
                    "questions": [{
                        "id": "q",
                        "question": "Pick?",
                        "options": [{"label": "A"}, {"label": "B"}]
                    }]
                }),
                None,
            );
            let (output, ()) = futures::join!(execute, responder);
            let output = output.expect("round trip");
            let crate::model::ContentBlock::Text(text) = &output.content[0] else {
                unreachable!("ask renders text");
            };
            assert!(text.text.contains("A: B"));

            // Unknown id → no pending slot.
            assert!(!tool.respond_ui(
                "missing",
                AskResponse {
                    answers: Vec::new(),
                    dismissed: true
                }
            ));
        });
    }

    /// Closing the host reply surface dismisses cards already in flight and
    /// rejects later cards before they can enter the five-minute wait path.
    #[test]
    fn channel_ui_close_drains_pending_and_rejects_late_requests() {
        asupersync::test_utils::run_test(|| async {
            let tool = AskTool::new(AskPolicy::Error);
            let (tx, mut rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(4);
            tool.install_channel_ui(tx);

            let closer_tool = tool.clone();
            let cx = crate::agent_cx::AgentCx::for_request();
            let closer = async {
                let _request = rx.recv(cx.cx()).await.expect("ui request arrives");
                assert_eq!(closer_tool.close_channel_ui(), 1);
            };
            let execute = tool.execute(
                "ask-close",
                serde_json::json!({
                    "questions": [{
                        "question": "Pick?",
                        "options": [{"label": "A"}, {"label": "B"}]
                    }]
                }),
                None,
            );
            let (result, ()) = futures::join!(execute, closer);
            let error = result.expect_err("closing the picker dismisses the pending card");
            assert!(error.to_string().contains("dismissed"), "{error}");

            let late_error = tool
                .execute(
                    "ask-after-close",
                    serde_json::json!({
                        "questions": [{
                            "question": "Too late?",
                            "options": [{"label": "A"}, {"label": "B"}]
                        }]
                    }),
                    None,
                )
                .await
                .expect_err("a closed picker must reject later cards immediately");
            assert!(
                late_error.to_string().contains("picker surface closed"),
                "{late_error}"
            );
        });
    }

    #[test]
    fn channel_ui_full_outbound_queue_fails_without_pending_waiter() {
        asupersync::test_utils::run_test(|| async {
            let tool = AskTool::new(AskPolicy::Error);
            let (tx, _rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(1);
            tx.try_send(AskUiRequest {
                id: String::from("occupy-capacity"),
                request: question(serde_json::json!({
                    "questions": [{
                        "question": "Occupied?",
                        "options": [{"label": "A"}, {"label": "B"}]
                    }]
                })),
            })
            .expect("occupy the single outbound slot");
            tool.install_channel_ui(tx);

            let execution = asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(20),
                tool.execute(
                    "ask-full",
                    serde_json::json!({
                        "questions": [{
                            "question": "Pick?",
                            "options": [{"label": "A"}, {"label": "B"}]
                        }]
                    }),
                    None,
                ),
            )
            .await
            .expect("a full picker queue must fail before the outer guard expires");
            let error = execution.expect_err("a full picker queue must fail without waiting");
            assert!(
                error.to_string().contains("picker surface unavailable"),
                "{error}"
            );
            assert_eq!(
                tool.close_channel_ui(),
                0,
                "failed outbound dispatch must remove its pending waiter"
            );
        });
    }

    #[test]
    fn channel_ui_close_prevents_forwarding_an_already_queued_card() {
        asupersync::test_utils::run_test(|| async {
            let tool = AskTool::new(AskPolicy::Error);
            let (tx, mut rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(1);
            tool.install_channel_ui(tx);

            let forwarder_tool = tool.clone();
            let cx = crate::agent_cx::AgentCx::for_request();
            let forwarded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let forwarded_for_task = std::sync::Arc::clone(&forwarded);
            let forwarder = async move {
                let request = rx.recv(cx.cx()).await.expect("queued UI request");
                assert_eq!(forwarder_tool.close_channel_ui(), 1);
                assert!(
                    !forwarder_tool.try_forward_channel_ui_request(&request.id, || {
                        forwarded_for_task.store(true, std::sync::atomic::Ordering::Release);
                        true
                    })
                );
                let closed = asupersync::time::timeout(
                    asupersync::time::wall_now(),
                    std::time::Duration::from_millis(20),
                    rx.recv(cx.cx()),
                )
                .await
                .expect("close must release the installed channel sender");
                assert!(closed.is_err(), "closed picker channel must disconnect");
            };
            let execute = tool.execute(
                "ask-close-before-forward",
                serde_json::json!({
                    "questions": [{
                        "question": "Pick?",
                        "options": [{"label": "A"}, {"label": "B"}]
                    }]
                }),
                None,
            );
            let (result, ()) = futures::join!(execute, forwarder);
            let error = result.expect_err("close dismisses the queued card");
            assert!(error.to_string().contains("dismissed"), "{error}");
            assert!(
                !forwarded.load(std::sync::atomic::Ordering::Acquire),
                "a card dismissed by close must not be forwarded afterward"
            );
        });
    }

    #[test]
    fn dropping_channel_ui_execution_removes_its_pending_reply() {
        asupersync::test_utils::run_test(|| async {
            let tool = AskTool::new(AskPolicy::Error);
            let (tx, mut rx) = asupersync::channel::mpsc::channel::<AskUiRequest>(1);
            tool.install_channel_ui(tx);

            let mut execution = Box::pin(tool.execute(
                "ask-cancelled",
                serde_json::json!({
                    "questions": [{
                        "question": "Pick?",
                        "options": [{"label": "A"}, {"label": "B"}]
                    }]
                }),
                None,
            ));
            assert!(futures::poll!(execution.as_mut()).is_pending());
            let cx = crate::agent_cx::AgentCx::for_request();
            rx.recv(cx.cx()).await.expect("UI request was published");

            drop(execution);

            assert_eq!(
                tool.close_channel_ui(),
                0,
                "dropping the execution future must release its pending reply lease"
            );
        });
    }

    /// Card parsing matrix: numbers, labels, multi comma lists, cancel,
    /// free-text Other, and mixed-typo rejection.
    #[test]
    fn question_reply_parsing_matrix() {
        let single: AskQuestion = serde_json::from_value(serde_json::json!({
            "question": "Pick?",
            "options": [{"label": "Alpha"}, {"label": "Beta"}]
        }))
        .expect("question");
        assert_eq!(
            parse_question_reply(&single, "2"),
            Ok(QuestionReply::Selected(vec!["Beta".to_string()]))
        );
        assert_eq!(
            parse_question_reply(&single, "alpha"),
            Ok(QuestionReply::Selected(vec!["Alpha".to_string()]))
        );
        assert_eq!(
            parse_question_reply(&single, "CANCEL"),
            Ok(QuestionReply::Cancel)
        );
        assert_eq!(
            parse_question_reply(&single, "do something else"),
            Ok(QuestionReply::Other("do something else".to_string()))
        );

        let multi: AskQuestion = serde_json::from_value(serde_json::json!({
            "question": "Pick several?",
            "multi": true,
            "options": [{"label": "Alpha"}, {"label": "Beta"}, {"label": "Gamma"}]
        }))
        .expect("multi question");
        assert_eq!(
            parse_question_reply(&multi, "1, gamma"),
            Ok(QuestionReply::Selected(vec![
                "Alpha".to_string(),
                "Gamma".to_string()
            ]))
        );
        assert!(parse_question_reply(&multi, "1, nonsense").is_err());
    }

    /// Free-text 'Other' answers render distinctly.
    #[test]
    fn other_answers_render() {
        let request = validate_request(question(serde_json::json!({
            "questions": [{"id": "q", "question": "Pick?",
                "options": [{"label": "A"}, {"label": "B"}]}]
        })))
        .expect("valid");
        let response = AskResponse {
            answers: vec![AskAnswer {
                question_id: "q".to_string(),
                selected: Vec::new(),
                other: Some("do it differently".to_string()),
            }],
            dismissed: false,
        };
        let rendered = render_answers(&request, &response, false);
        assert!(rendered.contains("A: Other: do it differently"));
    }

    fn approval_request(tool_name: &str) -> crate::agent::ToolApprovalRequest {
        crate::agent::ToolApprovalRequest {
            tool_call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            arguments: serde_json::json!({"command": "rm -rf build"}),
        }
    }

    /// Answer the bridge's approval card with the given reply.
    fn install_canned_reply(tool: &AskTool, selected: Vec<&str>, other: Option<&str>) {
        let selected: Vec<String> = selected.into_iter().map(str::to_string).collect();
        let other = other.map(str::to_string);
        tool.set_handler(Arc::new(move |request: AskRequest| {
            let selected = selected.clone();
            let other = other.clone();
            Box::pin(async move {
                Ok(AskResponse {
                    answers: vec![AskAnswer {
                        question_id: effective_question_id(&request.questions[0], 0),
                        selected,
                        other,
                    }],
                    dismissed: false,
                })
            })
        }));
    }

    /// Issue #196: the ask-surface approval bridge maps card replies onto
    /// approval decisions, failing closed on every non-affirmative outcome.
    #[test]
    fn approval_bridge_decision_mapping() {
        asupersync::test_utils::run_test(|| async {
            use crate::agent::ToolApprovalDecision;

            // Allow selection approves.
            let tool = AskTool::new(AskPolicy::Recommended);
            install_canned_reply(&tool, vec![APPROVAL_ALLOW_LABEL], None);
            let handler = approval_handler_via_ask(tool.clone());
            assert_eq!(
                handler(approval_request("bash")).await,
                ToolApprovalDecision::Allow
            );

            // Deny selection denies.
            install_canned_reply(&tool, vec![APPROVAL_DENY_LABEL], None);
            let handler = approval_handler_via_ask(tool.clone());
            let decision = handler(approval_request("bash")).await;
            assert!(
                matches!(decision, ToolApprovalDecision::Deny { ref reason } if reason.contains("denied")),
                "deny selection must deny, got {decision:?}"
            );

            // A free-text "Other" answer is not an approval, even when it
            // happens to spell out "Allow".
            install_canned_reply(&tool, vec![], Some("Allow"));
            let handler = approval_handler_via_ask(tool.clone());
            assert!(matches!(
                handler(approval_request("bash")).await,
                ToolApprovalDecision::Deny { .. }
            ));

            // Dismissal denies.
            tool.set_handler(Arc::new(|_request: AskRequest| {
                Box::pin(async {
                    Ok(AskResponse {
                        answers: Vec::new(),
                        dismissed: true,
                    })
                })
            }));
            let handler = approval_handler_via_ask(tool.clone());
            let decision = handler(approval_request("write")).await;
            assert!(
                matches!(decision, ToolApprovalDecision::Deny { ref reason } if reason.contains("dismissed")),
                "dismissal must deny, got {decision:?}"
            );
        });
    }

    /// Issue #196: with no interactive picker surface installed the bridge
    /// must deny with an explicit reason — never auto-answer via
    /// `AskPolicy::Recommended`, which would silently self-approve.
    #[test]
    fn approval_bridge_without_surface_denies_and_never_auto_answers() {
        asupersync::test_utils::run_test(|| async {
            use crate::agent::ToolApprovalDecision;

            let tool = AskTool::new(AskPolicy::Recommended);
            let handler = approval_handler_via_ask(tool);
            let decision = handler(approval_request("bash")).await;
            assert!(
                matches!(
                    decision,
                    ToolApprovalDecision::Deny { ref reason }
                        if reason.contains("could not be completed")
                ),
                "no surface must fail closed with an explicit reason, got {decision:?}"
            );
        });
    }

    /// The approval card preview stays bounded and shows bash commands
    /// directly.
    #[test]
    fn approval_arguments_preview_shapes() {
        let bash = approval_arguments_preview(&serde_json::json!({"command": "cargo build"}));
        assert_eq!(bash, "$ cargo build");

        let huge = "x".repeat(5000);
        let bounded =
            approval_arguments_preview(&serde_json::json!({"path": "a.txt", "content": huge}));
        assert!(bounded.chars().count() < 800);
        assert!(bounded.contains("truncated"));
    }
}
