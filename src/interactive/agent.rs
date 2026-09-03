use super::conversation::{
    add_usage, build_content_blocks_for_input, content_blocks_to_text, last_assistant_message,
    split_content_blocks_for_input,
};
use super::ext_session::{format_extension_ui_prompt, parse_extension_ui_response};
use super::*;
use crate::extension_events::{BeforeAgentStartOutcome, apply_before_agent_start_response};

pub fn extension_commands_for_catalog(
    manager: &ExtensionManager,
) -> Vec<crate::autocomplete::NamedEntry> {
    manager
        .list_commands()
        .into_iter()
        .filter_map(|cmd| {
            let name = cmd.get("name")?.as_str()?.to_string();
            let description = cmd
                .get("description")
                .and_then(|d| d.as_str())
                .map(std::string::ToString::to_string);
            Some(crate::autocomplete::NamedEntry { name, description })
        })
        .collect()
}

pub(super) fn build_user_message(text: String) -> ModelMessage {
    ModelMessage::User(UserMessage {
        content: UserContent::Text(text),
        timestamp: Utc::now().timestamp_millis(),
    })
}

fn append_turn_artifacts(
    session: &mut Session,
    mut messages: Vec<ModelMessage>,
    repairs: &[crate::dialects::RepairEntry],
    keyword_activations: &[crate::magic_keywords::KeywordActivation],
) {
    // Session retry cleanup requires an Error/Aborted assistant to remain the
    // leaf. Audit entries are durable completed state, so place them before
    // the incomplete assistant rather than allowing a Custom entry to mask it.
    let incomplete_assistant = messages
        .iter()
        .rposition(|message| {
            matches!(
                message,
                ModelMessage::Assistant(assistant)
                    if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted)
            )
        })
        .map(|index| messages.remove(index));
    for message in messages {
        session.append_model_message(message);
    }
    crate::agent::append_dialect_repair_telemetry(session, repairs);
    crate::magic_keywords::append_session_telemetry(session, keyword_activations);
    if let Some(message) = incomplete_assistant {
        session.append_model_message(message);
    }
}

async fn dispatch_input_event(
    manager: &ExtensionManager,
    text: String,
    images: Vec<ImageContent>,
) -> crate::error::Result<InputEventOutcome> {
    let images_value = serde_json::to_value(&images).unwrap_or(Value::Null);
    let attachments_value = images_value.clone();
    let text_clone = text.clone();
    let payload = json!({
        "text": text,
        "content": text_clone,
        "images": images_value,
        "attachments": attachments_value,
        "source": "interactive",
    });
    let response = manager
        .dispatch_event_with_response(
            ExtensionEventName::Input,
            Some(payload),
            EXTENSION_EVENT_TIMEOUT_MS,
        )
        .await?;
    Ok(apply_input_event_response(response, text, images))
}

fn before_agent_start_payload(prompt: &str, images: &[ImageContent], system_prompt: &str) -> Value {
    let images_value = serde_json::to_value(images).unwrap_or(Value::Null);
    json!({
        "prompt": prompt,
        "images": images_value,
        "systemPrompt": system_prompt,
    })
}

async fn dispatch_before_agent_start_event(
    manager: &ExtensionManager,
    prompt: &str,
    images: &[ImageContent],
    system_prompt: &str,
) -> BeforeAgentStartOutcome {
    let payload = before_agent_start_payload(prompt, images, system_prompt);
    let response = manager
        .dispatch_event_with_response(
            ExtensionEventName::BeforeAgentStart,
            Some(payload),
            EXTENSION_EVENT_TIMEOUT_MS,
        )
        .await;

    match response {
        Ok(value) => apply_before_agent_start_response(value, Utc::now().timestamp_millis()),
        Err(err) => {
            tracing::warn!("before_agent_start extension hook failed (fail-open): {err}");
            BeforeAgentStartOutcome {
                messages: Vec::new(),
                system_prompt: None,
            }
        }
    }
}

struct TurnSystemPromptGuard<'a> {
    agent: &'a mut crate::agent::Agent,
    base_system_prompt: Option<String>,
}

impl<'a> TurnSystemPromptGuard<'a> {
    fn new(
        agent: &'a mut crate::agent::Agent,
        base_system_prompt: Option<String>,
        turn_system_prompt: Option<String>,
    ) -> Self {
        agent.set_system_prompt(turn_system_prompt.or_else(|| base_system_prompt.clone()));
        Self {
            agent,
            base_system_prompt,
        }
    }
}

impl std::ops::Deref for TurnSystemPromptGuard<'_> {
    type Target = crate::agent::Agent;

    fn deref(&self) -> &Self::Target {
        self.agent
    }
}

impl std::ops::DerefMut for TurnSystemPromptGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.agent
    }
}

impl Drop for TurnSystemPromptGuard<'_> {
    fn drop(&mut self) {
        self.agent.set_system_prompt(self.base_system_prompt.take());
    }
}

const UI_STREAM_DELTA_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(45);
const UI_STREAM_DELTA_MAX_BUFFER_BYTES: usize = 2 * 1024;

/// System prompt for automatic session titling (bd-cv653.3.1).
const TITLE_SYSTEM_PROMPT: &str = "You name coding sessions. Reply with ONLY a short plain-text title: 3-7 words, no quotes, no markdown, no trailing punctuation.";

/// One-shot title generation against a tiny/smol-role model entry.
///
/// Returns None on ANY failure (auth, provider construction, stream error,
/// empty/oversized output) — titling is strictly best-effort and must never
/// disturb the session. Never logs prompt content.
async fn generate_session_title(
    entry: &ModelEntry,
    user_text: &str,
    assistant_excerpt: &str,
) -> Option<String> {
    use crate::model::{Message, StreamEvent};
    use crate::provider::{Context, StreamOptions};
    use futures::StreamExt;

    if crate::models::model_requires_configured_credential(entry) {
        // Cheap-role titling silently disables itself when credentials are
        // missing — the user never asked for this call.
        let key = super::commands::resolve_model_key_from_default_auth(entry)?;
        if key.trim().is_empty() {
            return None;
        }
    }
    let provider = providers::create_provider(entry, None).ok()?;

    let excerpt = |text: &str, cap: usize| {
        let trimmed = text.trim();
        if trimmed.chars().count() <= cap {
            trimmed.to_string()
        } else {
            let mut s: String = trimmed.chars().take(cap).collect();
            s.push('…');
            s
        }
    };
    let prompt_text = format!(
        "Name this coding session.\n\nUser:\n{}\n\nAssistant (excerpt):\n{}",
        excerpt(user_text, 2000),
        excerpt(assistant_excerpt, 800)
    );

    let context = Context {
        system_prompt: Some(TITLE_SYSTEM_PROMPT.to_string().into()),
        messages: vec![Message::User(UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::Text(TextContent::new(prompt_text))]),
            timestamp: chrono::Utc::now().timestamp_millis(),
        })]
        .into(),
        tools: Vec::new().into(),
    };
    let options = StreamOptions {
        api_key: None,
        max_tokens: Some(96),
        thinking_level: Some(entry.clamp_thinking_level(ThinkingLevel::Minimal)),
        ..Default::default()
    };

    let mut stream = provider.stream(&context, &options).await.ok()?;
    let mut collected = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamEvent::TextDelta { delta, .. }) => collected.push_str(&delta),
            Ok(StreamEvent::Done { .. }) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    sanitize_session_title(&collected)
}

/// Normalize a raw model reply into a valid session title: single line,
/// stripped of quotes/markdown/noise, bounded to 60 chars. None when empty.
fn sanitize_session_title(raw: &str) -> Option<String> {
    let first_line = raw.lines().next().unwrap_or("").trim();
    let cleaned: String = first_line
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '*' || c == '#')
        .trim_end_matches(['.', '!', ':'])
        .chars()
        .take(60)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

const EXTENSION_CUSTOM_WIDGET_KEY: &str = "__pi_custom_overlay";
const EXTENSION_CUSTOM_MIN_WIDTH: usize = 20;
// Interactive slash commands may host long-running custom UIs (e.g. games).
// Keep the command budget long enough to avoid timing out active sessions.
const EXTENSION_INTERACTIVE_COMMAND_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamDeltaKind {
    Text,
    Thinking,
}

fn content_blocks_estimated_output_bytes(content: &[ContentBlock]) -> usize {
    content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.len(),
            ContentBlock::Thinking(thinking) => thinking.thinking.len(),
            ContentBlock::RedactedThinking(redacted) => redacted.data.len(),
            ContentBlock::Image(image) => image.data.len().saturating_add(image.mime_type.len()),
            ContentBlock::ToolCall(tool) => tool
                .id
                .len()
                .saturating_add(tool.name.len())
                .saturating_add(tool.arguments.to_string().len()),
        })
        .sum()
}

struct UiStreamDeltaBatcher {
    sender: mpsc::Sender<PiMsg>,
    pending: std::collections::VecDeque<PiMsg>,
    pending_bytes: usize,
    flush_interval: std::time::Duration,
    max_pending_bytes: usize,
    last_flush: std::time::Instant,
    frame_p99_us: Arc<AtomicU64>,
    pending_tool_update: Option<PiMsg>,
    pending_tool_update_bytes: usize,
    pending_tool_update_events: usize,
    last_tool_update_flush: std::time::Instant,
    /// Set once `AgentEnd` carried a provider error for this run (#209): the
    /// turn-end card is already in the transcript, so the task must not add
    /// a second `AgentError` block for the same failure.
    turn_error_surfaced: bool,
}

impl UiStreamDeltaBatcher {
    #[cfg(test)]
    fn new(sender: mpsc::Sender<PiMsg>) -> Self {
        Self::new_with_frame_p99(sender, Arc::new(AtomicU64::new(0)))
    }

    fn new_with_frame_p99(sender: mpsc::Sender<PiMsg>, frame_p99_us: Arc<AtomicU64>) -> Self {
        let now = std::time::Instant::now();
        let flush_interval = UI_STREAM_DELTA_FLUSH_INTERVAL;
        Self {
            sender,
            pending: std::collections::VecDeque::new(),
            pending_bytes: 0,
            flush_interval,
            max_pending_bytes: UI_STREAM_DELTA_MAX_BUFFER_BYTES,
            // Prime the first delta flush so the UI shows immediate output.
            last_flush: now.checked_sub(flush_interval).unwrap_or(now),
            frame_p99_us,
            pending_tool_update: None,
            pending_tool_update_bytes: 0,
            pending_tool_update_events: 0,
            turn_error_surfaced: false,
            last_tool_update_flush: now,
        }
    }

    fn push_delta(&mut self, kind: StreamDeltaKind, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(last) = self.pending.back_mut() {
            match (kind, last) {
                (StreamDeltaKind::Text, PiMsg::TextDelta(text))
                | (StreamDeltaKind::Thinking, PiMsg::ThinkingDelta(text)) => {
                    text.push_str(delta);
                    self.pending_bytes += delta.len();
                    self.flush(false);
                    return;
                }
                _ => {}
            }
        }

        let msg = match kind {
            StreamDeltaKind::Text => PiMsg::TextDelta(delta.to_string()),
            StreamDeltaKind::Thinking => PiMsg::ThinkingDelta(delta.to_string()),
        };
        self.pending.push_back(msg);
        self.pending_bytes += delta.len();
        self.flush(false);
    }

    fn send_immediate(&mut self, msg: PiMsg) {
        if matches!(msg, PiMsg::ToolUpdate { .. }) {
            self.push_tool_update(msg);
            return;
        }
        self.flush_tool_update(true);
        self.pending.push_back(msg);
        self.flush(true);
    }

    const fn delta_bytes_for_msg(msg: &PiMsg) -> usize {
        match msg {
            PiMsg::TextDelta(text) | PiMsg::ThinkingDelta(text) => text.len(),
            _ => 0,
        }
    }

    fn push_tool_update(&mut self, msg: PiMsg) {
        let output_bytes = match &msg {
            PiMsg::ToolUpdate { content, .. } => content_blocks_estimated_output_bytes(content),
            _ => 0,
        };
        let pending_tool_output_bytes = self.pending_tool_update_bytes.saturating_add(output_bytes);
        let pending_tool_events = self.pending_tool_update_events.saturating_add(1);
        let decision = TuiPressureController::decide(
            self.frame_p99_us.load(Ordering::Relaxed),
            pending_tool_output_bytes,
            pending_tool_events,
        );

        if !decision.throttle_tool_updates {
            self.flush_tool_update(true);
            self.pending.push_back(msg);
            self.flush(true);
            return;
        }

        self.pending_tool_update = Some(msg);
        self.pending_tool_update_bytes = pending_tool_output_bytes;
        self.pending_tool_update_events = pending_tool_events;

        if pending_tool_events >= decision.max_pending_tool_events
            || self.pending_tool_update_bytes >= decision.max_pending_tool_output_bytes
            || self.last_tool_update_flush.elapsed() >= decision.flush_interval
        {
            self.flush_tool_update(true);
        }
    }

    fn enqueue_pending_tool_update(&mut self) {
        if let Some(msg) = self.pending_tool_update.take() {
            self.pending.push_back(msg);
            self.pending_tool_update_bytes = 0;
            self.pending_tool_update_events = 0;
            self.last_tool_update_flush = std::time::Instant::now();
        }
    }

    fn flush_tool_update(&mut self, force_channel_flush: bool) {
        self.enqueue_pending_tool_update();
        if force_channel_flush {
            self.flush(true);
        }
    }

    fn flush(&mut self, force: bool) {
        if force {
            self.enqueue_pending_tool_update();
        }

        if self.pending.is_empty() {
            return;
        }

        if !force
            && self.pending_bytes < self.max_pending_bytes
            && self.last_flush.elapsed() < self.flush_interval
        {
            return;
        }

        let mut sent_any = false;

        while let Some(msg) = self.pending.pop_front() {
            let delta_bytes = Self::delta_bytes_for_msg(&msg);
            match self.sender.try_send(msg) {
                Ok(()) => {
                    self.pending_bytes = self.pending_bytes.saturating_sub(delta_bytes);
                    sent_any = true;
                }
                Err(err) => {
                    match err {
                        mpsc::SendError::Full(msg) => {
                            self.pending.push_front(msg);
                        }
                        mpsc::SendError::Disconnected(_) | mpsc::SendError::Cancelled(_) => {
                            self.pending.clear();
                            self.pending_bytes = 0;
                        }
                    }
                    break;
                }
            }
        }

        if sent_any {
            self.last_flush = std::time::Instant::now();
        }
    }
}

fn build_agent_done_pi_msg(messages: &[ModelMessage]) -> PiMsg {
    let last = last_assistant_message(messages);
    let mut usage = Usage::default();
    for message in messages {
        if let ModelMessage::Assistant(assistant) = message {
            add_usage(&mut usage, &assistant.usage);
        }
    }
    let stop_reason = last
        .as_ref()
        .map_or(StopReason::Stop, |msg| msg.stop_reason);
    // #209: a provider failure ends the turn as a structured card (provider ·
    // HTTP status · retry status · bounded detail); aborts keep their plain
    // message so the "Request aborted" status stays untouched.
    let error_message = last.as_ref().and_then(|msg| {
        msg.error_message.as_ref().map(|raw| {
            if stop_reason == StopReason::Error {
                crate::error::ProviderErrorSummary::from_error_text(Some(&msg.provider), raw)
                    .turn_end_card(raw, None)
            } else {
                raw.clone()
            }
        })
    });
    PiMsg::AgentDone {
        usage: Some(usage),
        stop_reason,
        error_message,
    }
}

fn dispatch_agent_event_to_ui(event: &AgentEvent, batcher: &mut UiStreamDeltaBatcher) {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                batcher.push_delta(StreamDeltaKind::Text, delta);
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                batcher.push_delta(StreamDeltaKind::Thinking, delta);
            }
            _ => {}
        },
        AgentEvent::AgentStart { .. } => {
            batcher.send_immediate(PiMsg::AgentStart);
        }
        AgentEvent::ToolExecutionStart {
            tool_name,
            tool_call_id,
            args,
        } => {
            batcher.send_immediate(PiMsg::ToolStart {
                name: tool_name.clone(),
                tool_id: tool_call_id.clone(),
            });
            // Surface *what* the tool was asked to do (e.g. the bash command
            // line) so the transcript is not just "Running bash ..." with an
            // anonymous output block.
            if let Some(summary) = tool_invocation_summary(tool_name, args) {
                batcher.send_immediate(PiMsg::ToolInvocation {
                    tool_id: tool_call_id.clone(),
                    summary,
                });
            }
        }
        AgentEvent::ToolExecutionUpdate {
            tool_name,
            tool_call_id,
            partial_result,
            ..
        } => {
            batcher.send_immediate(PiMsg::ToolUpdate {
                name: tool_name.clone(),
                tool_id: tool_call_id.clone(),
                content: partial_result.content.clone(),
                details: partial_result.details.clone(),
            });
        }
        AgentEvent::ToolExecutionEnd {
            tool_name,
            tool_call_id,
            is_error,
            result,
        } => {
            // Todo footer (bd-cv653.3.9): state-driven off the tool result's
            // todo_list.v1 details, never a side channel.
            if tool_name == "todo"
                && !is_error
                && let Some(details) = &result.details
                && details.get("schema").and_then(serde_json::Value::as_str)
                    == Some(crate::todo::TODO_LIST_SCHEMA)
            {
                batcher.send_immediate(PiMsg::TodoSummary {
                    summary: details
                        .get("summary")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string),
                });
            }
            // The end result is the authoritative cumulative output. Send it
            // as a final ToolUpdate so the transcript block reflects the full
            // result even when intermediate updates were coalesced away (or
            // the tool never emitted streaming updates at all).
            batcher.send_immediate(PiMsg::ToolUpdate {
                name: tool_name.clone(),
                tool_id: tool_call_id.clone(),
                content: result.content.clone(),
                details: result.details.clone(),
            });
            batcher.send_immediate(PiMsg::ToolEnd {
                name: tool_name.clone(),
                tool_id: tool_call_id.clone(),
                is_error: *is_error,
                output: None,
            });
        }
        AgentEvent::AgentEnd {
            messages, error, ..
        } => {
            let done = build_agent_done_pi_msg(messages);
            if error.is_some()
                && matches!(
                    &done,
                    PiMsg::AgentDone {
                        stop_reason: StopReason::Error,
                        error_message: Some(_),
                        ..
                    }
                )
            {
                batcher.turn_error_surfaced = true;
            }
            batcher.send_immediate(done);
        }
        _ => {}
    }
}

/// Whether the run already ended with a provider-error card via `AgentEnd`
/// (#209), so an `Err` from the agent loop must not be surfaced twice.
fn turn_error_already_surfaced(batcher: &Arc<StdMutex<UiStreamDeltaBatcher>>) -> bool {
    batcher
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .turn_error_surfaced
}

/// Strategy used to derive the head of a TUI tool card from its invocation.
///
/// This is the renderer registry promised by bd-cv653.9.2. Keeping the
/// registry separate from the rendering logic makes missing tool coverage a
/// testable condition instead of silently falling through to a generic card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolInvocationRenderer {
    Field(&'static str),
    FieldOrDefault {
        field: &'static str,
        default: &'static str,
    },
    Search {
        pattern: &'static str,
        scope: &'static str,
    },
    Action {
        action: &'static str,
        context: &'static [&'static str],
        default_action: Option<&'static str>,
    },
    Questions,
    Subagent,
    Mcp,
}

/// Resolve every native agent-facing tool to its card renderer. Mounted MCP
/// tools form a dynamic namespace and share one renderer; extension-defined
/// tools retain the extension/custom-renderer fallback.
fn tool_invocation_renderer(tool_name: &str) -> Option<ToolInvocationRenderer> {
    use ToolInvocationRenderer::{Action, Field, FieldOrDefault, Mcp, Questions, Search, Subagent};

    if tool_name
        .strip_prefix("mcp__")
        .is_some_and(|mounted| !mounted.is_empty())
    {
        return Some(Mcp);
    }

    Some(match tool_name {
        "bash" => Field("command"),
        "read" | "write" | "edit" | "hashline_edit" | "inspect_image" => Field("path"),
        "ls" => FieldOrDefault {
            field: "path",
            default: ".",
        },
        "grep" | "find" | "ast_grep" => Search {
            pattern: "pattern",
            scope: "path",
        },
        "eval" => Field("code"),
        "web_search" | "recall" => Field("query"),
        "generate_image" => Field("prompt"),
        "tts" => Field("text"),
        "retain" => Field("content"),
        "reflect" => Field("question"),
        "learn" => Field("lesson"),
        "submit_plan" => Field("plan"),
        "jobs" => Action {
            action: "action",
            context: &["jobId"],
            default_action: None,
        },
        "hub" => Action {
            action: "op",
            context: &["name", "application"],
            default_action: None,
        },
        "security_scan" => Action {
            action: "op",
            context: &["sarifOut", "fingerprint", "baseline"],
            default_action: None,
        },
        "github" => Action {
            action: "op",
            context: &["repo", "number", "query", "run_id"],
            default_action: None,
        },
        "ast_edit" => Action {
            action: "action",
            context: &["path", "proposalId"],
            default_action: Some("stage"),
        },
        "lsp" => Action {
            action: "action",
            context: &["file", "symbol", "query", "method"],
            default_action: None,
        },
        "debug" => Action {
            action: "action",
            context: &["program", "file", "expression", "command"],
            default_action: None,
        },
        "computer" => Action {
            action: "action",
            context: &["output_path", "window_id", "display_id", "key"],
            default_action: None,
        },
        "browser" => Action {
            action: "action",
            context: &["url", "selector", "tab", "key", "output_path"],
            default_action: None,
        },
        "memory_edit" => Action {
            action: "op",
            context: &["id"],
            default_action: None,
        },
        "manage_skill" => Action {
            action: "op",
            context: &["name"],
            default_action: None,
        },
        "todo" => Action {
            action: "op",
            context: &["task", "phase"],
            default_action: None,
        },
        "xdev" => Action {
            action: "action",
            context: &["name"],
            default_action: None,
        },
        "ask" => Questions,
        "subagent" => Subagent,
        _ => return None,
    })
}

/// Compact, single-line description of what a tool invocation will do,
/// derived through the per-tool renderer registry (the bash command line,
/// file path, operation and target, first question, ...). A registered
/// renderer may still return `None` for malformed/incomplete arguments.
/// LOAD-BEARING VISIBILITY: `pub(super)` is re-exported as `pub(crate)` by
/// `src/interactive.rs` when `feature = "ftui"` is active (interactive_ftui
/// calls `crate::interactive::tool_invocation_summary`; bd-cv653.9.2).
#[allow(clippy::too_many_lines)]
pub fn tool_invocation_summary(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    fn str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        args.get(key).and_then(serde_json::Value::as_str)
    }

    fn nonblank_str_arg<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
        str_arg(args, key).filter(|value| !value.trim().is_empty())
    }
    /// First non-blank line only, collapsed to at most `max` characters.
    /// Control characters (incl. ESC) are dropped so a hostile or binary
    /// command string can never inject escape sequences into the transcript
    /// header, which bypasses the tool-output sanitizer.
    fn clip(text: &str, max: usize) -> String {
        let text = text.trim();
        let first_line = text.lines().next().unwrap_or("").trim_end();
        let mut out: String = first_line
            .chars()
            .filter(|c| !c.is_control() || *c == '\t')
            .take(max)
            .collect();
        if first_line.chars().count() > max || text.lines().count() > 1 {
            out.push('…');
        }
        out
    }

    fn scalar_arg(args: &serde_json::Value, key: &str) -> Option<String> {
        let value = args.get(key)?;
        match value {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Number(number) => Some(number.to_string()),
            serde_json::Value::Bool(value) => Some(value.to_string()),
            _ => None,
        }
    }

    fn action_summary(
        args: &serde_json::Value,
        action: &str,
        context: &[&str],
        default_action: Option<&str>,
        max: usize,
    ) -> Option<String> {
        let action = match str_arg(args, action) {
            Some(value) if !value.trim().is_empty() => value.trim(),
            Some(_) => return None,
            None => default_action?,
        };
        let detail = context
            .iter()
            .find_map(|key| scalar_arg(args, key))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        Some(clip(
            &detail.map_or_else(
                || action.to_string(),
                |detail| format!("{action} · {detail}"),
            ),
            max,
        ))
    }

    const MAX: usize = 96;
    let summary = match tool_invocation_renderer(tool_name)? {
        ToolInvocationRenderer::Field(field) => clip(nonblank_str_arg(args, field)?, MAX),
        ToolInvocationRenderer::FieldOrDefault { field, default } => match args.get(field) {
            None | Some(serde_json::Value::Null) => default.to_string(),
            Some(serde_json::Value::String(value)) if value.trim().is_empty() => {
                default.to_string()
            }
            Some(serde_json::Value::String(value)) => clip(value, MAX),
            Some(_) => return None,
        },
        ToolInvocationRenderer::Search { pattern, scope } => {
            let pattern = nonblank_str_arg(args, pattern)?.trim();
            nonblank_str_arg(args, scope).map_or_else(
                || clip(pattern, MAX),
                |scope| clip(&format!("{pattern} in {}", scope.trim()), MAX),
            )
        }
        ToolInvocationRenderer::Action {
            action,
            context,
            default_action,
        } => action_summary(args, action, context, default_action, MAX)?,
        ToolInvocationRenderer::Questions => {
            let question = args
                .get("questions")?
                .as_array()?
                .first()?
                .get("question")?
                .as_str()
                .filter(|question| !question.trim().is_empty())?;
            clip(question, MAX)
        }
        ToolInvocationRenderer::Subagent => {
            let has_single = args.get("agent").is_some() || args.get("task").is_some();
            let has_tasks = args.get("tasks").is_some();
            let has_chain = args.get("chain").is_some();
            if usize::from(has_single) + usize::from(has_tasks) + usize::from(has_chain) != 1 {
                return None;
            }

            if has_single {
                let agent = nonblank_str_arg(args, "agent")?.trim();
                let task = nonblank_str_arg(args, "task")?.trim();
                clip(&format!("{agent}: {task}"), MAX)
            } else if has_tasks {
                let tasks = args.get("tasks")?.as_array()?;
                if tasks.is_empty() {
                    return None;
                }
                format!("{} parallel tasks", tasks.len())
            } else if has_chain {
                let chain = args.get("chain")?.as_array()?;
                if chain.is_empty() {
                    return None;
                }
                format!("{} chained tasks", chain.len())
            } else {
                return None;
            }
        }
        ToolInvocationRenderer::Mcp => {
            let mounted = tool_name.strip_prefix("mcp__")?;
            clip(&format!("MCP {}", mounted.replace("__", " · ")), MAX)
        }
    };
    if summary.is_empty() {
        None
    } else {
        Some(summary)
    }
}

async fn flush_ui_stream_batcher_with_backpressure(batcher: &StdMutex<UiStreamDeltaBatcher>) {
    let (sender, pending) = {
        let mut guard = match batcher.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.flush(true);
        if guard.pending.is_empty() {
            return;
        }
        let sender = guard.sender.clone();
        let pending = std::mem::take(&mut guard.pending);
        guard.pending_bytes = 0;
        drop(guard);
        (sender, pending)
    };

    let cx = Cx::for_request();
    for msg in pending {
        if sender.send(&cx, msg).await.is_err() {
            break;
        }
    }
}

enum SessionEventOwnership {
    Current,
    Stale,
    Busy,
}

impl PiApp {
    fn session_event_ownership(&self, expected_session_id: &str) -> SessionEventOwnership {
        match self.session.try_lock() {
            Ok(session) if session.header.id == expected_session_id => {
                SessionEventOwnership::Current
            }
            Ok(_) => SessionEventOwnership::Stale,
            Err(_) => SessionEventOwnership::Busy,
        }
    }

    fn retry_busy_session_event(&mut self, event: PiMsg, attempts_remaining: u8) -> Option<Cmd> {
        let retry = session_event_retry_cmd(event, attempts_remaining);
        if retry.is_none() {
            // The Session lock is still contended, so ownership remains
            // unknowable. Exhaustion must be observable but non-destructive:
            // mutating even reset-specific state here would let a delayed
            // reset for session A idle or clear a live session-B turn.
            self.status_message =
                Some("Session remained busy; a delayed session update was not applied".to_string());
        }
        retry
    }

    /// Handle custom Pi messages from the agent.
    pub(super) fn handle_pi_message(&mut self, msg: PiMsg) -> Option<Cmd> {
        self.handle_pi_message_with_session_retry(msg, SESSION_EVENT_LOCK_RETRY_ATTEMPTS)
    }

    #[allow(clippy::too_many_lines)]
    fn handle_pi_message_with_session_retry(
        &mut self,
        msg: PiMsg,
        attempts_remaining: u8,
    ) -> Option<Cmd> {
        match msg {
            PiMsg::AgentStart => {
                self.agent_state = AgentState::Processing;
                self.current_response.clear();
                self.current_thinking.clear();
                self.extension_streaming.store(true, Ordering::SeqCst);
            }
            PiMsg::RunPending => {
                return self.run_next_pending();
            }
            PiMsg::EnqueuePendingInput { session_id, input } => {
                if self.agent_state != AgentState::Idle {
                    return None;
                }
                match self.session_event_ownership(&session_id) {
                    SessionEventOwnership::Current => {}
                    SessionEventOwnership::Stale => return None,
                    SessionEventOwnership::Busy => {
                        return self.retry_busy_session_event(
                            PiMsg::EnqueuePendingInput { session_id, input },
                            attempts_remaining,
                        );
                    }
                }
                self.pending_inputs.push_back(input);
                if self.agent_state == AgentState::Idle {
                    return self.run_next_pending();
                }
            }
            PiMsg::SessionEventRetry {
                event,
                attempts_remaining,
            } => {
                if matches!(event.as_ref(), PiMsg::SessionEventRetry { .. }) {
                    self.status_message =
                        Some("Rejected nested session-event retry envelope".to_string());
                    return None;
                }
                return self.handle_pi_message_with_session_retry(*event, attempts_remaining);
            }
            // UiShutdown: internal signal for shutting down the async→UI
            // bridge; should not normally reach the UI event loop, but handle
            // it defensively. TerminalTitle: driver-pushed title updates are
            // an ftui affordance (issue #200) — the charmed stack re-emits
            // the terminal title from render_header every frame.
            PiMsg::UiShutdown | PiMsg::TerminalTitle(_) | PiMsg::AutocompleteCatalog(_) => {}
            PiMsg::AutocompleteRefresh => {
                self.autocomplete.provider.refresh_background();
                return Self::autocomplete_refresh_cmd();
            }
            PiMsg::TextDelta(text) => {
                self.current_response.push_str(&text);
                // While tail-following, `view()` computes the bottom slice
                // directly, so we can skip full viewport rebuilds on every
                // token to reduce redraw jitter.
                if !self.follow_stream_tail {
                    self.refresh_conversation_viewport(false);
                }
            }
            PiMsg::ThinkingDelta(text) => {
                self.current_thinking.push_str(&text);
                if !self.follow_stream_tail {
                    self.refresh_conversation_viewport(false);
                }
            }
            PiMsg::ToolStart { name, tool_id } => {
                self.agent_state = AgentState::ToolRunning;
                self.current_tool = Some(name);
                // The status row looks its summary up by THIS id; the map
                // keeps sibling summaries alive for parallel batches (all
                // ToolStart/ToolInvocation events arrive up front).
                self.current_tool_id = Some(tool_id);
                self.tool_progress = Some(ToolProgress::new());
                self.pending_tool_output = None;
            }
            PiMsg::ToolInvocation { tool_id, summary } => {
                self.current_tool_summary.insert(tool_id, summary);
            }
            PiMsg::ToolUpdate {
                name,
                tool_id,
                content,
                details,
            } => {
                // Update progress metrics from details if present.
                if let Some(ref mut progress) = self.tool_progress {
                    progress.update_from_details(details.as_ref());
                } else {
                    let mut progress = ToolProgress::new();
                    progress.update_from_details(details.as_ref());
                    self.tool_progress = Some(progress);
                }
                if let Some(output) = format_tool_output(
                    &content,
                    details.as_ref(),
                    self.config.terminal_show_images(),
                ) {
                    // Include the invocation (e.g. the bash command) in the
                    // transcript header so the reader can see what ran, not
                    // just its output. Matched by tool_id so interleaved
                    // (parallel) tool events cannot stamp one tool's command
                    // onto another tool's output block.
                    let invocation = self.current_tool_summary.get(&tool_id);
                    self.pending_tool_output = Some(invocation.map_or_else(
                        || format!("Tool {name} output:\n{output}"),
                        |invocation| {
                            let prefix = if name == "bash" { "$ " } else { "→ " };
                            format!("Tool {name} output:\n{prefix}{invocation}\n{output}")
                        },
                    ));
                }
            }
            PiMsg::ToolEnd { tool_id, .. } => {
                self.agent_state = AgentState::Processing;
                self.current_tool = None;
                // Drop only THIS tool's summary; interleaved siblings keep
                // theirs until their own ToolEnd.
                self.current_tool_summary.remove(&tool_id);
                if self.current_tool_id.as_deref() == Some(tool_id.as_str()) {
                    self.current_tool_id = None;
                }
                self.tool_progress = None;
                if let Some(output) = self.pending_tool_output.take() {
                    self.messages.push(ConversationMessage::tool(output));
                    // Respect the user's scroll position: only snap to the
                    // bottom when they were already following the tail.
                    // Yanking a scrolled-up reader down on every tool
                    // completion made long tool-heavy turns unreadable.
                    let follow_tail = self.follow_stream_tail;
                    self.refresh_conversation_viewport(follow_tail);
                }
            }
            PiMsg::TodoSummary { summary } => {
                self.todo_summary = summary;
            }
            PiMsg::AskUiRequest(request) => {
                if self
                    .ask_tool
                    .as_ref()
                    .is_some_and(|tool| !tool.channel_ui_request_is_pending(&request.id))
                {
                    return None;
                }
                self.input_card_order.push_back(InputCardKind::Ask);
                self.ask_ui_queue.push_back(request);
                self.advance_ask_ui_queue();
            }
            PiMsg::AgentDone {
                usage,
                stop_reason,
                error_message,
            } => {
                // Snapshot follow-tail *before* we mutate conversation state so
                // we preserve the user's scroll intent.
                let follow_tail = self.follow_stream_tail;

                // Finalize the response: move streaming buffers into the
                // permanent message list and clear them so they are not
                // double-rendered by build_conversation_content().
                let had_response =
                    !self.current_response.is_empty() || !self.current_thinking.is_empty();
                if had_response {
                    self.messages.push(ConversationMessage::new(
                        MessageRole::Assistant,
                        std::mem::take(&mut self.current_response),
                        if self.current_thinking.is_empty() {
                            None
                        } else {
                            Some(std::mem::take(&mut self.current_thinking))
                        },
                    ));
                }
                // Defensively clear both buffers even if they were already
                // taken — this prevents a stale streaming section from
                // appearing in the next view() frame.
                self.current_response.clear();
                self.current_thinking.clear();

                // Update usage
                if let Some(ref u) = usage {
                    add_usage(&mut self.total_usage, u);
                }

                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.current_tool_id = None;
                self.current_tool_summary.clear();
                self.abort_handle = None;
                self.extension_streaming.store(false, Ordering::SeqCst);
                self.extension_compacting.store(false, Ordering::SeqCst);

                // Refresh VCS info (may have changed during tool execution)
                self.vcs_info = super::read_vcs_info(&self.cwd);

                if stop_reason == StopReason::Aborted {
                    self.status_message = Some("Request aborted".to_string());
                } else if stop_reason == StopReason::Error {
                    let message = error_message.unwrap_or_else(|| "Request failed".to_string());
                    // The status bar is one line: show the headline there and
                    // the full card in the transcript. The card is pushed even
                    // when partial text streamed first (#209) — a mid-stream
                    // 503 used to leave only the status line behind, which is
                    // invisible once anything else overwrites it.
                    self.status_message = message.lines().next().map(str::to_string);
                    let content = if message.starts_with("Provider error:") {
                        message
                    } else {
                        format!("Error: {message}")
                    };
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content,
                        thinking: None,
                        collapsed: false,
                    });
                }

                // Re-focus input BEFORE syncing the viewport — focus()
                // can change the input height, and the viewport offset
                // calculation depends on view_effective_conversation_height()
                // which accounts for the input area.
                self.input.focus();

                // Sync the viewport so the finalized (markdown-rendered)
                // message is visible. This is critical: without it the
                // viewport's stored content would still reflect the raw
                // streaming text, causing the final message to appear
                // overwritten or missing.
                self.refresh_conversation_viewport(follow_tail);

                // Auto-titling (bd-cv653.3.1): after the first completed
                // exchange of an unnamed, persisted session, ask a tiny/smol
                // role model for a short session name. Async and fire-and-
                // forget: never blocks the turn, silently no-ops on failure.
                if !matches!(stop_reason, StopReason::Aborted | StopReason::Error) {
                    self.maybe_request_session_title();
                }
                // bd-1qol9: terminal/abort cleanup invalidates every card
                // from this turn BEFORE idle input or RunPending runs.
                self.invalidate_input_cards_for_turn_end();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::SessionTitleSuggestion {
                owner_session_id,
                title,
            } => {
                // Apply only when the session is STILL unnamed (a manual /name
                // during the title call always wins), the originating session
                // is still current, and persistence is on.
                if self.save_enabled {
                    let session = Arc::clone(&self.session);
                    match session.try_lock() {
                        Ok(mut guard)
                            if guard.header.id == owner_session_id
                                && guard.get_name().is_none() =>
                        {
                            guard.set_name(&title);
                            drop(guard);
                            self.status_message = Some(format!("Session named: {title}"));
                        }
                        Ok(_) => {}
                        Err(_) => {
                            return self.retry_busy_session_event(
                                PiMsg::SessionTitleSuggestion {
                                    owner_session_id,
                                    title,
                                },
                                attempts_remaining,
                            );
                        }
                    }
                }
            }
            PiMsg::AgentError(error) => {
                self.current_response.clear();
                self.current_thinking.clear();
                let content = if error.contains('\n') || error.starts_with("Error:") {
                    error
                } else {
                    format!("Error: {error}")
                };
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content,
                    thinking: None,
                    collapsed: false,
                });
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.current_tool_id = None;
                self.current_tool_summary.clear();
                self.abort_handle = None;
                self.extension_streaming.store(false, Ordering::SeqCst);
                self.extension_compacting.store(false, Ordering::SeqCst);
                self.input.focus();
                // bd-1qol9: an errored turn cancels its outstanding cards.
                self.invalidate_input_cards_for_turn_end();
                self.refresh_conversation_viewport(true);

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::CredentialUpdated { provider } => {
                self.sync_active_provider_credentials(&provider);
            }
            PiMsg::UpdateLastUserMessage(content) => {
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.role == MessageRole::User)
                {
                    message.content = content;
                }
                self.scroll_to_bottom();
            }
            PiMsg::System(message) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: message,
                    thinking: None,
                    collapsed: false,
                });
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.current_tool_id = None;
                self.current_tool_summary.clear();
                self.abort_handle = None;
                self.extension_streaming.store(false, Ordering::SeqCst);
                self.extension_compacting.store(false, Ordering::SeqCst);
                // bd-1qol9: hard reset also cancels outstanding cards.
                self.invalidate_input_cards_for_turn_end();
                self.scroll_to_bottom();
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::SystemNote(message) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: message,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
            }
            PiMsg::SessionSystemNote {
                owner_session_id,
                message,
            } => {
                match self.session_event_ownership(&owner_session_id) {
                    SessionEventOwnership::Current => {}
                    SessionEventOwnership::Stale => return None,
                    SessionEventOwnership::Busy => {
                        return self.retry_busy_session_event(
                            PiMsg::SessionSystemNote {
                                owner_session_id,
                                message,
                            },
                            attempts_remaining,
                        );
                    }
                }
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: message,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
            }
            PiMsg::BashResult {
                display,
                content_for_agent,
            } => {
                self.bash_running = false;
                self.current_tool = None;
                self.agent_state = AgentState::Idle;

                if let Some(content) = content_for_agent {
                    self.scroll_to_bottom();
                    return self.submit_content(content);
                }

                // `!cmd` output is raw terminal bytes; strip escapes/controls
                // before it enters the transcript (bd-p45xh).
                let display = match super::tool_render::sanitize_terminal_text(&display) {
                    std::borrow::Cow::Borrowed(_) => display,
                    std::borrow::Cow::Owned(clean) => clean,
                };
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: display,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::OAuthDeviceFlowStarted {
                provider,
                device_code,
                user_code,
                verification_uri,
                expires_in,
            } => {
                let message = format!(
                    "OAuth login: {provider}\n\n\
Open this URL:\n{verification_uri}\n\n\
If prompted, enter this code: {user_code}\n\
Code expires in {expires_in} seconds.\n\n\
After approving access in the browser, press Enter in Pi to complete login."
                );
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: message,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.pending_oauth = Some(PendingOAuth {
                    provider,
                    kind: PendingLoginKind::DeviceFlow,
                    verifier: String::new(),
                    oauth_config: None,
                    device_code: Some(device_code),
                    redirect_uri: None,
                });
                self.input_mode = InputMode::SingleLine;
                self.set_input_height(3);
                self.input.focus();
                self.status_message = None;
            }
            PiMsg::RetryCommitted {
                session_id,
                messages,
                usage,
                text,
                status,
            } => {
                match self.session_event_ownership(&session_id) {
                    SessionEventOwnership::Current => {}
                    SessionEventOwnership::Stale => return None,
                    SessionEventOwnership::Busy => {
                        return self.retry_busy_session_event(
                            PiMsg::RetryCommitted {
                                session_id,
                                messages,
                                usage,
                                text,
                                status,
                            },
                            attempts_remaining,
                        );
                    }
                }
                let reset_cmd = self.handle_pi_message_with_session_retry(
                    PiMsg::ConversationReset {
                        session_id,
                        messages,
                        usage,
                        status,
                    },
                    attempts_remaining,
                );
                if reset_cmd.is_some() {
                    return reset_cmd;
                }
                if self.agent_state != AgentState::Idle {
                    return None;
                }
                self.pending_inputs
                    .push_back(PendingInput::GeneratedText(text));
                return self.run_next_pending();
            }
            PiMsg::ConversationReset {
                session_id,
                messages,
                usage,
                status,
            } => {
                match self.session_event_ownership(&session_id) {
                    SessionEventOwnership::Current => {}
                    SessionEventOwnership::Stale => return None,
                    SessionEventOwnership::Busy => {
                        return self.retry_busy_session_event(
                            PiMsg::ConversationReset {
                                session_id,
                                messages,
                                usage,
                                status,
                            },
                            attempts_remaining,
                        );
                    }
                }
                let is_replacement =
                    self.displayed_session_id.as_deref() != Some(session_id.as_str());
                if is_replacement {
                    // A draft captured before an old-session card belongs to
                    // that session. Prevent the generic card drain below from
                    // restoring it into the replacement session's editor.
                    self.card_draft_snapshot = None;
                }
                // Compaction, fork, and session replacement are all terminal
                // boundaries for UI requests admitted by the previous turn.
                self.invalidate_input_cards_for_turn_end();
                self.displayed_session_id = Some(session_id);
                self.messages = messages;
                self.total_usage = usage;
                self.current_response.clear();
                self.current_thinking.clear();
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.current_tool_id = None;
                self.current_tool_summary.clear();
                self.abort_handle = None;
                if is_replacement {
                    self.title_requested = false;
                    self.todo_summary = None;
                    self.pending_oauth = None;
                    self.role_model_overrides.clear();
                    self.tree_ui = None;
                    self.extension_custom_active = false;
                    self.extension_custom_key_queue.clear();
                    self.extension_custom_overlay = None;
                }
                self.status_message = status;
                self.message_render_cache.clear();
                if let Err(message) = self.sync_runtime_selection_from_session_header() {
                    self.status_message = Some(message);
                }
                self.scroll_to_bottom();
                self.input.focus();
            }
            PiMsg::SetEditorText {
                owner_session_id,
                text,
            } => {
                match self.session_event_ownership(&owner_session_id) {
                    SessionEventOwnership::Current => {}
                    SessionEventOwnership::Stale => return None,
                    SessionEventOwnership::Busy => {
                        return self.retry_busy_session_event(
                            PiMsg::SetEditorText {
                                owner_session_id,
                                text,
                            },
                            attempts_remaining,
                        );
                    }
                }
                self.input.set_value(&text);
                self.input.focus();
            }
            PiMsg::OpenTree {
                owner_session_id,
                initial_selected_id,
                label,
            } => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot open tree while processing".to_string());
                    return None;
                }

                let session = Arc::clone(&self.session);
                let Ok(session_guard) = session.try_lock() else {
                    return self.retry_busy_session_event(
                        PiMsg::OpenTree {
                            owner_session_id,
                            initial_selected_id,
                            label,
                        },
                        attempts_remaining,
                    );
                };
                if session_guard.header.id != owner_session_id {
                    return None;
                }
                let selector = TreeSelectorState::new(
                    &session_guard,
                    self.term_height,
                    initial_selected_id.as_deref(),
                    label,
                );
                self.tree_ui = Some(TreeUiState::Selector(selector));
            }
            PiMsg::ResourcesReloaded {
                resources,
                status,
                diagnostics,
            } => {
                let mut autocomplete_catalog = AutocompleteCatalog::from_resources(&resources);
                if let Some(manager) = &self.extensions {
                    autocomplete_catalog.extension_commands =
                        extension_commands_for_catalog(manager);
                }
                self.autocomplete.provider.set_catalog(autocomplete_catalog);
                self.autocomplete.close();
                self.resources = resources;
                self.apply_theme(Theme::resolve(&self.config, &self.cwd));
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.current_tool_id = None;
                self.current_tool_summary.clear();
                self.abort_handle = None;
                self.status_message = Some(status);
                if let Some(message) = diagnostics {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: message,
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_bottom();
                }
                self.input.focus();
            }
            PiMsg::ExtensionUiRequest(request) => {
                return self.handle_extension_ui_request(request);
            }
            PiMsg::CapabilityPromptTick {
                id,
                generation,
                timer_generation,
            } => {
                return self.handle_capability_prompt_tick(&id, generation, timer_generation);
            }
            PiMsg::ExtensionCommandDone {
                command: _,
                display,
                is_error: _,
            } => {
                self.agent_state = AgentState::Idle;
                self.current_tool = None;

                // Extension command output is arbitrary text; strip
                // escapes/controls before it enters the transcript
                // (bd-p45xh, same class as tool and !cmd output).
                let display = match super::tool_render::sanitize_terminal_text(&display) {
                    std::borrow::Cow::Borrowed(_) => display,
                    std::borrow::Cow::Owned(clean) => clean,
                };
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: display,
                    thinking: None,
                    collapsed: false,
                });
                self.extension_custom_active = false;
                self.extension_custom_key_queue.clear();
                self.extension_custom_overlay = None;
                self.scroll_to_bottom();
                self.input.focus();

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
                }
            }
            PiMsg::OAuthCallbackReceived(callback_url) => {
                // Auto-submit the OAuth code received from the local callback server.
                if let Some(pending) = self.pending_oauth.take() {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: "Authorization callback received from browser.".to_string(),
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_bottom();
                    return self.submit_oauth_code(&callback_url, pending);
                }
            }
        }
        None
    }

    fn handle_extension_ui_request(&mut self, request: ExtensionUiRequest) -> Option<Cmd> {
        // Capability-specific prompts get a dedicated modal overlay fed by
        // an ordered, generation-bound queue so a second concurrent request
        // can no longer overwrite (and orphan) the first (bd-yllbn).
        if CapabilityPromptOverlay::is_capability_prompt(&request) {
            return self.enqueue_capability_prompt(CapabilityPromptOverlay::from_request(request));
        }
        match request.method.as_str() {
            "getEditorText" | "get_editor_text" => {
                let value = Value::String(self.input.value());
                self.send_extension_ui_response(ExtensionUiResponse {
                    id: request.id,
                    value: Some(value),
                    cancelled: false,
                });
                return None;
            }
            "getAllThemes" | "get_all_themes" => {
                let value = Value::Array(self.collect_extension_theme_infos());
                self.send_extension_ui_response(ExtensionUiResponse {
                    id: request.id,
                    value: Some(value),
                    cancelled: false,
                });
                return None;
            }
            "getTheme" | "get_theme" => {
                let name = request
                    .payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let value = if name.is_empty() {
                    Value::Null
                } else {
                    Theme::resolve_spec(&name, &self.cwd)
                        .ok()
                        .and_then(|theme| serde_json::to_value(theme).ok())
                        .unwrap_or(Value::Null)
                };
                self.send_extension_ui_response(ExtensionUiResponse {
                    id: request.id,
                    value: Some(value),
                    cancelled: false,
                });
                return None;
            }
            "setTheme" | "set_theme" => {
                let name = request
                    .payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let mut response = serde_json::Map::new();
                if name.is_empty() {
                    response.insert("success".to_string(), Value::Bool(false));
                    response.insert(
                        "error".to_string(),
                        Value::String("Theme name is required".to_string()),
                    );
                } else {
                    match Theme::resolve_spec(&name, &self.cwd) {
                        Ok(theme) => {
                            let theme_name = theme.name.clone();
                            self.apply_theme(theme);
                            self.config.theme = Some(theme_name);
                            response.insert("success".to_string(), Value::Bool(true));
                        }
                        Err(err) => {
                            response.insert("success".to_string(), Value::Bool(false));
                            response.insert("error".to_string(), Value::String(err.to_string()));
                        }
                    }
                }
                self.send_extension_ui_response(ExtensionUiResponse {
                    id: request.id,
                    value: Some(Value::Object(response)),
                    cancelled: false,
                });
                return None;
            }
            _ => {}
        }
        if request.method == "custom" {
            self.handle_custom_extension_ui_request(request);
            return None;
        }
        if request.expects_response() {
            self.input_card_order.push_back(InputCardKind::Extension);
            self.extension_ui_queue.push_back(request);
            self.advance_extension_ui_queue();
        } else {
            self.apply_extension_ui_effect(&request);
        }
        None
    }

    /// Invalidate every live or queued input card bound to the finished turn
    /// BEFORE idle handling hands control back (bd-1qol9): asks respond as
    /// dismissed and extension prompts receive cancelled responses so their
    /// senders fail fast instead of leaking past the turn. QUEUED items are
    /// drained first so resolution side effects cannot resurrect card
    /// activations behind this sweep's back.
    pub(super) fn invalidate_input_cards_for_turn_end(&mut self) {
        let _ = std::mem::take(&mut self.input_card_order);

        while let Some(request) = self.ask_ui_queue.pop_front() {
            if let Some(tool) = &self.ask_tool {
                let _ = tool.respond_ui(
                    &request.id,
                    crate::ask::AskResponse {
                        answers: Vec::new(),
                        dismissed: true,
                    },
                );
            }
        }

        while let Some(request) = self.extension_ui_queue.pop_front() {
            self.send_extension_ui_response_quiet(ExtensionUiResponse {
                id: request.id,
                value: None,
                cancelled: true,
            });
        }

        if let Some(card) = self.active_ask_ui.take() {
            self.finish_ask_ui(&card, true);
        }
        if let Some(active) = self.active_extension_ui.take() {
            self.send_extension_ui_response_quiet(ExtensionUiResponse {
                id: active.id,
                value: None,
                cancelled: true,
            });
            self.complete_input_card_transition(InputCardKind::Extension);
        }

        self.active_input_card_kind = None;
        self.restore_card_draft_after_cards_settle();
        self.drain_capability_prompts_for_session_reset();
    }

    /// FIFO capacity for queued capability prompts (bd-yllbn).
    const MAX_CAPABILITY_PROMPT_QUEUE: usize = 8;

    const fn next_capability_prompt_generation(&mut self) -> u64 {
        self.capability_prompt_generation += 1;
        self.capability_prompt_generation
    }

    /// Enqueue an incoming capability prompt or activate it immediately.
    ///
    /// Arrival order is preserved exactly; a prompt arriving beyond the
    /// bound is denied fail-closed immediately so every request receives
    /// exactly one terminal response and no extension can strand on the
    /// TUI bridge waiting for a decision that was silently discarded.
    ///
    /// A QUEUED prompt also schedules its OWN expiry wake (bd-yllbn reopen
    /// audit): its bounded lifetime must not depend on the active overlay
    /// resolving first, otherwise an unbudgeted embedder-sourced prompt can
    /// strand bounded successors past their deadlines indefinitely.
    fn enqueue_capability_prompt(&mut self, mut overlay: CapabilityPromptOverlay) -> Option<Cmd> {
        overlay.generation = self.next_capability_prompt_generation();
        if overlay
            .expires_at
            .is_some_and(|deadline| deadline <= std::time::Instant::now())
        {
            overlay.cancel_timer();
            self.send_extension_ui_response_quiet(overlay.request.auto_deny_response());
            return None;
        }
        if self.capability_prompt.is_none() {
            return self.activate_capability_prompt(overlay);
        }
        if self.capability_prompt_queue.len() >= Self::MAX_CAPABILITY_PROMPT_QUEUE {
            let response = ExtensionUiResponse {
                id: overlay.request.id,
                value: Some(Value::Bool(false)),
                cancelled: true,
            };
            self.send_extension_ui_response_quiet(response);
            return None;
        }
        let queue_wake = Self::capability_prompt_queue_deadline_cmd(&overlay);
        self.capability_prompt_queue.push_back(overlay);
        queue_wake
    }

    /// Promote an overlay to the active slot and schedule its first redraw.
    fn activate_capability_prompt(&mut self, mut overlay: CapabilityPromptOverlay) -> Option<Cmd> {
        // Replace any queued deadline waiter. A queued wake already in the
        // program mailbox carries the previous timer epoch and is ignored.
        overlay.restart_timer();
        let tick_cmd = Self::capability_prompt_tick_cmd(&overlay);
        self.capability_prompt = Some(overlay);
        tick_cmd
    }

    /// Cancellable periodic wake for one exact prompt generation.
    ///
    /// Each wait is at most one second, causing an idle TUI repaint so the
    /// displayed countdown visibly decreases. The overlay-owned cancellation
    /// signal interrupts the wait immediately on resolution/reset/quit, while
    /// `(id, generation, timer_generation)` makes already-enqueued stale
    /// ticks harmless.
    fn capability_prompt_tick_cmd(overlay: &CapabilityPromptOverlay) -> Option<Cmd> {
        let expires_at = overlay.expires_at?;
        let id = overlay.request.id.clone();
        let generation = overlay.generation;
        let timer_generation = overlay.timer_generation();
        let remaining = expires_at.saturating_duration_since(std::time::Instant::now());
        let delay = remaining.min(std::time::Duration::from_secs(1));
        let timer = overlay.timer();
        Some(Cmd::new_optional(move || {
            timer.wait(delay).then(|| {
                Message::new(PiMsg::CapabilityPromptTick {
                    id,
                    generation,
                    timer_generation,
                })
            })
        }))
    }

    /// Queued prompts do not need repaint ticks. Wait directly for their own
    /// absolute deadline; promotion cancels and replaces this command.
    fn capability_prompt_queue_deadline_cmd(overlay: &CapabilityPromptOverlay) -> Option<Cmd> {
        let expires_at = overlay.expires_at?;
        let id = overlay.request.id.clone();
        let generation = overlay.generation;
        let timer_generation = overlay.timer_generation();
        let remaining = expires_at.saturating_duration_since(std::time::Instant::now());
        let timer = overlay.timer();
        Some(Cmd::new_optional(move || {
            timer.wait(remaining).then(|| {
                Message::new(PiMsg::CapabilityPromptTick {
                    id,
                    generation,
                    timer_generation,
                })
            })
        }))
    }

    /// Pop the next queued prompt into the active slot (bd-yllbn).
    ///
    /// `pub(super)`: shared with `keybindings`, which promotes the FIFO
    /// successor whenever the user manually resolves the active prompt.
    pub(super) fn activate_next_capability_prompt(&mut self) -> Option<Cmd> {
        let next = self.capability_prompt_queue.pop_front()?;
        self.activate_capability_prompt(next)
    }

    /// Auto-deny path for an elapsed capability-prompt deadline (bd-yllbn).
    ///
    /// Two independent resolution targets carry the exact request, prompt,
    /// and timer generation so stale, late, or foreign wakes can never
    /// dismiss, answer, or rearm a different prompt:
    ///
    /// 1. the ACTIVE overlay — denied, then the FIFO successor promotes;
    /// 2. any QUEUED prompt whose own deadline passed while it waits behind
    ///    the active slot — removed and denied WITHOUT touching or promoting
    ///    anything else. This keeps every bounded successor honest even when
    ///    the currently displayed prompt carries no timeout of its own
    ///    (bd-yllbn reopen audit).
    fn handle_capability_prompt_tick(
        &mut self,
        id: &str,
        generation: u64,
        timer_generation: u64,
    ) -> Option<Cmd> {
        let now = std::time::Instant::now();
        let matches_active = self.capability_prompt.as_ref().is_some_and(|prompt| {
            prompt.request.id == id
                && prompt.generation == generation
                && prompt.timer_generation() == timer_generation
        });
        if !matches_active {
            let queue_index = self.capability_prompt_queue.iter().position(|prompt| {
                prompt.request.id == id
                    && prompt.generation == generation
                    && prompt.timer_generation() == timer_generation
            });
            let queue_index = queue_index?;
            let queued = self.capability_prompt_queue.get(queue_index)?;
            if queued.has_time_remaining(now) {
                let queued = self.capability_prompt_queue.get_mut(queue_index)?;
                // Invalidate this delivered epoch before arming its
                // replacement. A duplicate copy of the current tick can then
                // never create a second live waiter chain.
                queued.restart_timer();
                return Self::capability_prompt_queue_deadline_cmd(queued);
            }
            // Ids are unique per request and generations are monotonic, so at
            // most one queued item can carry this identity.
            if let Some(expired) = self.capability_prompt_queue.remove(queue_index) {
                expired.cancel_timer();
                let response = expired.request.auto_deny_response();
                self.send_extension_ui_response_quiet(response);
            }
            return None;
        }
        if self
            .capability_prompt
            .as_ref()
            .is_some_and(|prompt| prompt.has_time_remaining(now))
        {
            let active = self.capability_prompt.as_mut()?;
            // Every scheduled waiter owns a fresh epoch. Replaying the tick
            // that led here is therefore inert instead of forking another
            // periodic repaint chain.
            active.restart_timer();
            return Self::capability_prompt_tick_cmd(active);
        }
        let expired = self.capability_prompt.take()?;
        expired.cancel_timer();
        let response = expired.request.auto_deny_response();
        self.send_extension_ui_response_quiet(response);
        self.activate_next_capability_prompt()
    }

    /// Session transitions (compaction/fork/reset) drop prompts wholesale:
    /// each is answered cancelled so manager pending entries clear promptly
    /// and hostcalls fail closed. Dropped prompts are never answered allow
    /// and persistent permission state is untouched (bd-yllbn).
    pub(super) fn drain_capability_prompts_for_session_reset(&mut self) {
        let mut dropped = std::mem::take(&mut self.capability_prompt_queue);
        if let Some(active) = self.capability_prompt.take() {
            dropped.push_front(active);
        }
        for prompt in dropped {
            prompt.cancel_timer();
            let response = ExtensionUiResponse {
                id: prompt.request.id,
                value: Some(Value::Bool(false)),
                cancelled: true,
            };
            self.send_extension_ui_response_quiet(response);
        }
    }

    /// Respond without the user-visible "no pending request" status noise;
    /// used by timer/drain paths racing the manager's own timeout sweep.
    fn send_extension_ui_response_quiet(&self, response: ExtensionUiResponse) {
        if let Some(manager) = &self.extensions {
            let _ = manager.respond_ui(response);
        }
    }

    fn collect_extension_theme_infos(&self) -> Vec<Value> {
        let mut entries: Vec<(String, Option<String>)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        let mut push_entry = |name: &str, path: Option<String>| {
            let key = name.to_ascii_lowercase();
            if seen.insert(key) {
                entries.push((name.to_string(), path));
            }
        };

        push_entry("dark", None);
        push_entry("light", None);
        push_entry("solarized", None);

        for path in Theme::discover_themes(&self.cwd) {
            if let Ok(theme) = Theme::load(&path) {
                push_entry(&theme.name, Some(path.display().to_string()));
            }
        }

        entries.sort_by_key(|entry| entry.0.to_ascii_lowercase());

        entries
            .into_iter()
            .map(|(name, path)| {
                let mut map = serde_json::Map::new();
                map.insert("name".to_string(), Value::String(name));
                map.insert("path".to_string(), path.map_or(Value::Null, Value::String));
                Value::Object(map)
            })
            .collect()
    }

    fn handle_custom_extension_ui_request(&mut self, request: ExtensionUiRequest) {
        let mode = request
            .payload
            .get("mode")
            .or_else(|| request.payload.get("phase"))
            .and_then(Value::as_str)
            .unwrap_or("poll");
        let closing = mode.eq_ignore_ascii_case("close")
            || request
                .payload
                .get("close")
                .and_then(Value::as_bool)
                .unwrap_or(false);

        if closing {
            self.extension_custom_active = false;
            self.extension_custom_overlay = None;
            self.extension_custom_key_queue.clear();
        } else {
            self.extension_custom_active = true;
            if self.extension_custom_overlay.is_none() {
                self.extension_custom_overlay = Some(ExtensionCustomOverlay::default());
            }
            if let Some(overlay) = self.extension_custom_overlay.as_mut() {
                if request.extension_id.is_some() {
                    overlay.extension_id.clone_from(&request.extension_id);
                }
                if let Some(title) = request.payload.get("title") {
                    overlay.title = title.as_str().map(std::string::ToString::to_string);
                }
            }
        }

        let mut response = serde_json::Map::new();
        let width = self.custom_overlay_width_from_payload(&request.payload);
        response.insert(
            "width".to_string(),
            Value::from(u64::try_from(width).unwrap_or(80)),
        );
        if let Some(key) = self.extension_custom_key_queue.pop_front() {
            response.insert("key".to_string(), Value::String(key));
        }
        if !self.extension_custom_active {
            response.insert("closed".to_string(), Value::Bool(true));
        }

        self.send_extension_ui_response(ExtensionUiResponse {
            id: request.id,
            value: Some(Value::Object(response)),
            cancelled: false,
        });
    }

    fn custom_overlay_width_from_payload(&self, payload: &Value) -> usize {
        fn parse_percent_basis_points(raw: &str) -> Option<u32> {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }

            let mut parts = trimmed.split('.');
            let whole_part = parts.next()?;
            let frac_part = parts.next();
            if parts.next().is_some() || whole_part.is_empty() {
                return None;
            }
            if !whole_part.chars().all(|ch| ch.is_ascii_digit()) {
                return None;
            }

            let whole = whole_part.parse::<u32>().ok()?;
            let mut basis_points = whole.checked_mul(100)?;

            if let Some(frac_part) = frac_part {
                if !frac_part.chars().all(|ch| ch.is_ascii_digit()) {
                    return None;
                }
                let mut digits = frac_part.chars();
                let first = digits.next().and_then(|ch| ch.to_digit(10)).unwrap_or(0);
                let second = digits.next().and_then(|ch| ch.to_digit(10)).unwrap_or(0);
                let third = digits.next().and_then(|ch| ch.to_digit(10)).unwrap_or(0);

                let mut fractional = first * 10 + second;
                if third >= 5 {
                    fractional = fractional.saturating_add(1);
                }
                basis_points = basis_points.checked_add(fractional)?;
            }

            Some(basis_points)
        }

        fn parse_width_spec(spec: &Value, base: usize) -> Option<usize> {
            match spec {
                Value::Number(num) => num
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n > 0),
                Value::String(raw) => {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    if let Some(percent) = trimmed.strip_suffix('%') {
                        let basis_points = parse_percent_basis_points(percent)?;
                        if basis_points == 0 {
                            return None;
                        }
                        let base = u128::try_from(base).ok()?;
                        let width = base
                            .checked_mul(u128::from(basis_points))?
                            .checked_add(5_000)?
                            / 10_000;
                        let width = usize::try_from(width).ok()?;
                        return Some(width.max(1));
                    }
                    trimmed.parse::<usize>().ok().filter(|n| *n > 0)
                }
                _ => None,
            }
        }

        let base = self
            .term_width
            .saturating_sub(4)
            .max(EXTENSION_CUSTOM_MIN_WIDTH);
        let spec = payload
            .pointer("/overlayOptions/width")
            .or_else(|| payload.get("width"));
        spec.and_then(|value| parse_width_spec(value, base))
            .unwrap_or(base)
            .max(EXTENSION_CUSTOM_MIN_WIDTH)
    }

    fn apply_extension_ui_effect(&mut self, request: &ExtensionUiRequest) {
        match request.method.as_str() {
            "notify" => self.apply_extension_notify_effect(request),
            "setStatus" | "set_status" => self.apply_extension_status_effect(request),
            "setWidget" | "set_widget" => self.apply_extension_widget_effect(request),
            "setTitle" | "set_title" => self.apply_extension_title_effect(request),
            "set_editor_text" => self.apply_extension_editor_text_effect(request),
            _ => {}
        }
    }

    fn apply_extension_notify_effect(&mut self, request: &ExtensionUiRequest) {
        let title = request
            .payload
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Notification");
        let message = request
            .payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("");
        let level = request
            .payload
            .get("level")
            .and_then(Value::as_str)
            .or_else(|| request.payload.get("notifyType").and_then(Value::as_str))
            .or_else(|| request.payload.get("notify_type").and_then(Value::as_str))
            .unwrap_or("info");
        self.messages.push(ConversationMessage {
            role: MessageRole::System,
            content: format!("Extension notify ({level}): {title} {message}"),
            thinking: None,
            collapsed: false,
        });
        self.scroll_to_bottom();
    }

    fn apply_extension_status_effect(&mut self, request: &ExtensionUiRequest) {
        let status_text = request
            .payload
            .get("statusText")
            .and_then(Value::as_str)
            .or_else(|| request.payload.get("status_text").and_then(Value::as_str))
            .or_else(|| request.payload.get("text").and_then(Value::as_str))
            .unwrap_or("");
        if status_text.is_empty() {
            return;
        }

        let status_key = request
            .payload
            .get("statusKey")
            .and_then(Value::as_str)
            .or_else(|| request.payload.get("status_key").and_then(Value::as_str))
            .unwrap_or("");

        self.status_message = Some(if status_key.is_empty() {
            status_text.to_string()
        } else {
            format!("{status_key}: {status_text}")
        });
    }

    fn apply_extension_widget_effect(&mut self, request: &ExtensionUiRequest) {
        let widget_key = request
            .payload
            .get("widgetKey")
            .and_then(Value::as_str)
            .or_else(|| request.payload.get("widget_key").and_then(Value::as_str))
            .unwrap_or("widget");

        let lines = request
            .payload
            .get("widgetLines")
            .or_else(|| request.payload.get("widget_lines"))
            .or_else(|| request.payload.get("lines"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if widget_key == EXTENSION_CUSTOM_WIDGET_KEY {
            self.apply_custom_overlay_widget_effect(request, lines);
            return;
        }

        let content = request
            .payload
            .get("content")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| (!lines.is_empty()).then(|| lines.join("\n")));

        if let Some(content) = content {
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: format!("Extension widget ({widget_key}):\n{content}"),
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_bottom();
        }
    }

    fn apply_custom_overlay_widget_effect(
        &mut self,
        request: &ExtensionUiRequest,
        lines: Vec<String>,
    ) {
        let should_clear = request
            .payload
            .get("clear")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if should_clear {
            self.extension_custom_overlay = None;
            self.extension_custom_active = false;
            self.extension_custom_key_queue.clear();
            return;
        }

        self.extension_custom_active = true;
        if self.extension_custom_overlay.is_none() {
            self.extension_custom_overlay = Some(ExtensionCustomOverlay::default());
        }
        if let Some(overlay) = self.extension_custom_overlay.as_mut() {
            if request.extension_id.is_some() {
                overlay.extension_id.clone_from(&request.extension_id);
            }
            if let Some(title) = request.payload.get("title") {
                overlay.title = title.as_str().map(std::string::ToString::to_string);
            }
            overlay.lines = lines;
        }
    }

    fn apply_extension_title_effect(&mut self, request: &ExtensionUiRequest) {
        if let Some(title) = request.payload.get("title").and_then(Value::as_str) {
            self.status_message = Some(format!("Title: {title}"));
        }
    }

    fn apply_extension_editor_text_effect(&mut self, request: &ExtensionUiRequest) {
        if let Some(text) = request.payload.get("text").and_then(Value::as_str) {
            self.input.set_value(text);
        }
    }

    pub(super) fn send_extension_ui_response(&mut self, response: ExtensionUiResponse) {
        if let Some(manager) = &self.extensions {
            if !manager.respond_ui(response) {
                self.status_message = Some("No pending extension UI request".to_string());
            }
        } else {
            self.status_message = Some("Extensions are disabled".to_string());
        }
    }

    /// Ask cards (bd-cv653.3.8): show the next queued request's current
    /// question as a conversation card and focus the input for the reply.
    fn advance_ask_ui_queue(&mut self) {
        // bd-1qol9: cards are globally serialized — whichever kind is active
        // owns the editor until it resolves; nothing else may activate.
        if self.active_input_card_kind.is_some() {
            return;
        }
        let Some(request) = self.ask_ui_queue.pop_front() else {
            return;
        };
        self.capture_preexisting_card_draft();
        self.active_input_card_kind = Some(InputCardKind::Ask);
        self.active_ask_ui = Some(crate::interactive::ActiveAskCard {
            request,
            question_index: 0,
            answers: Vec::new(),
        });
        self.show_active_ask_question();
    }

    fn show_active_ask_question(&mut self) {
        let Some(card) = &self.active_ask_ui else {
            return;
        };
        let questions = &card.request.request.questions;
        let Some(question) = questions.get(card.question_index) else {
            return;
        };
        let prompt =
            crate::ask::format_question_card(question, card.question_index, questions.len());
        self.messages.push(ConversationMessage {
            role: MessageRole::System,
            content: prompt,
            thinking: None,
            collapsed: false,
        });
        self.scroll_to_bottom();
        self.input.focus();
    }

    /// Handle one line of input for the active ask card. Returns `true`
    /// when the input was consumed by the card.
    fn handle_ask_ui_input(&mut self, message: &str) -> bool {
        let Some(mut card) = self.active_ask_ui.take() else {
            return false;
        };
        let questions = &card.request.request.questions;
        let Some(question) = questions.get(card.question_index) else {
            // Defensive: malformed state — dismiss rather than wedge input.
            self.finish_ask_ui(&card, true);
            return true;
        };
        match crate::ask::parse_question_reply(question, message) {
            Err(error) => {
                self.status_message = Some(error);
                self.active_ask_ui = Some(card);
            }
            Ok(crate::ask::QuestionReply::Cancel) => {
                self.finish_ask_ui(&card, true);
            }
            Ok(reply) => {
                let (selected, other) = match reply {
                    crate::ask::QuestionReply::Selected(selected) => (selected, None),
                    crate::ask::QuestionReply::Other(other) => (Vec::new(), Some(other)),
                    crate::ask::QuestionReply::Cancel => unreachable!("handled above"),
                };
                card.answers.push(crate::ask::AskAnswer {
                    question_id: crate::ask::effective_question_id(question, card.question_index),
                    selected,
                    other,
                });
                card.question_index += 1;
                if card.question_index < card.request.request.questions.len() {
                    // The previous answer belongs to the resolved question,
                    // never to the successor editor state.
                    self.input.reset();
                    self.input.focus();
                    self.active_ask_ui = Some(card);
                    self.show_active_ask_question();
                } else {
                    self.finish_ask_ui(&card, false);
                }
            }
        }
        true
    }

    /// Whether a pending ask or extension UI card currently owns the input
    /// line. Cards arrive mid-turn, so callers must check this before any
    /// "agent is busy" gating of the editor.
    pub(super) const fn has_pending_input_card(&self) -> bool {
        self.active_ask_ui.is_some() || self.active_extension_ui.is_some()
    }

    /// Route the editor content to the pending ask/extension card, if any.
    /// Returns `None` when no card is pending (the caller continues with its
    /// normal submit/queue handling); otherwise the card consumed the line.
    /// An empty editor is a no-op so Enter cannot accidentally answer.
    // Outer None = "no card pending, caller keeps its normal submit path";
    // inner Option is the routed command itself. Flattening would lose the
    // pending/not-pending distinction.
    #[allow(clippy::option_option)]
    pub(super) fn submit_pending_card_answer(&mut self) -> Option<Option<Cmd>> {
        if !self.has_pending_input_card() {
            return None;
        }
        let value = self.input.value();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Some(None);
        }
        Some(self.submit_message(trimmed))
    }

    /// Dismiss the active ask card (Escape). Returns `true` when a card was
    /// pending and has been resolved as dismissed.
    pub(super) fn dismiss_active_ask_ui(&mut self) -> bool {
        let Some(card) = self.active_ask_ui.take() else {
            return false;
        };
        self.finish_ask_ui(&card, true);
        true
    }

    fn finish_ask_ui(&mut self, card: &crate::interactive::ActiveAskCard, dismissed: bool) {
        let response = crate::ask::AskResponse {
            answers: if dismissed {
                Vec::new()
            } else {
                card.answers.clone()
            },
            dismissed,
        };
        let delivered = self
            .ask_tool
            .as_ref()
            .is_some_and(|tool| tool.respond_ui(&card.request.id, response));
        if !delivered {
            self.status_message = Some("Ask request expired before the answer".to_string());
        } else if dismissed {
            self.status_message = Some("Question dismissed".to_string());
        }
        self.complete_input_card_transition(InputCardKind::Ask);
    }

    /// Resolve an ACTIVE extension text-card by parsing the editor line.
    /// Mirrors the ask-side bookkeeping (bd-1qol9).
    fn resolve_active_extension_ui_with_line(&mut self, message: &str) -> bool {
        let Some(active) = self.active_extension_ui.take() else {
            return false;
        };
        match parse_extension_ui_response(&active, message) {
            Ok(response) => {
                self.send_extension_ui_response(response);
                self.complete_input_card_transition(InputCardKind::Extension);
            }
            Err(err) => {
                self.status_message = Some(err);
                self.active_extension_ui = Some(active);
            }
        }
        true
    }

    /// Pop the resolved kind off the global order ledger. Returns `true` when
    /// the head matched (caller should then try to activate the successor).
    fn resolve_order_head_after(&mut self, kind: InputCardKind) -> bool {
        if self.input_card_order.front() == Some(&kind) {
            self.input_card_order.pop_front();
            true
        } else {
            false
        }
    }

    /// Atomically relinquish the editor after a terminal card resolution.
    ///
    /// Consumed input must be cleared before a successor activates; otherwise
    /// its draft-capture step can mistake the previous card's answer for a
    /// pre-card user draft. This is also the single place that advances the
    /// global order ledger and restores the original draft once the burst is
    /// fully settled (bd-q66i1).
    fn complete_input_card_transition(&mut self, kind: InputCardKind) {
        self.input.reset();
        self.input.focus();
        if self.active_input_card_kind == Some(kind) {
            self.active_input_card_kind = None;
        }
        if self.resolve_order_head_after(kind) {
            self.try_activate_next_input_card_impl();
        } else {
            match kind {
                InputCardKind::Ask => self.advance_ask_ui_queue(),
                InputCardKind::Extension => self.advance_extension_ui_queue(),
            }
        }
        self.restore_card_draft_after_cards_settle();
    }

    /// Snapshot the user's in-progress draft exactly once per card burst
    /// (bd-1qol9). Subsequent arrivals and queue promotions never clobber an
    /// existing snapshot.
    fn capture_preexisting_card_draft(&mut self) {
        if self.card_draft_snapshot.is_none() && !self.input.value().trim().is_empty() {
            self.card_draft_snapshot = Some(self.input.value());
            // AC: first activation snapshots AND clears the preexisting draft
            // (bd-1qol9); the editor now belongs to the card.
            self.input.reset();
        }
    }

    /// Explicit merge policy: once the LAST card resolves, restore the
    /// captured draft only into an empty editor (the modal ownership model
    /// means anything typed meanwhile would have been consumed as answers).
    pub(super) fn restore_card_draft_after_cards_settle(&mut self) {
        if self.has_pending_input_card() || self.active_input_card_kind.is_some() {
            return;
        }
        if !self.ask_ui_queue.is_empty() || !self.extension_ui_queue.is_empty() {
            return;
        }
        if self.input.value().trim().is_empty()
            && let Some(draft) = self.card_draft_snapshot.take()
        {
            self.input.set_value(&draft);
        }
    }

    /// Generically dismiss whichever card currently owns the editor (Escape,
    /// bd-1qol9): ask cards resolve as dismissed; extension prompts receive
    /// a cancelled response. The provider turn is never aborted.
    pub(super) fn dismiss_active_input_card(&mut self) -> bool {
        match self.active_input_card_kind {
            Some(InputCardKind::Ask) => self.dismiss_active_ask_ui(),
            Some(InputCardKind::Extension) => {
                if let Some(active) = self.active_extension_ui.take() {
                    self.send_extension_ui_response_quiet(ExtensionUiResponse {
                        id: active.id,
                        value: None,
                        cancelled: true,
                    });
                    self.complete_input_card_transition(InputCardKind::Extension);
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    /// Global activation stepper: promote the next queued card of ANY kind
    /// while its order head still stands, honoring mutual exclusion.
    pub(super) fn try_activate_next_input_card_impl(&mut self) {
        loop {
            if self.active_input_card_kind.is_some() {
                return;
            }
            let Some(head) = self.input_card_order.front().copied() else {
                return;
            };
            match head {
                InputCardKind::Ask => {
                    if self.ask_ui_queue.is_empty() {
                        self.input_card_order.pop_front();
                        continue;
                    }
                    self.advance_ask_ui_queue();
                    return;
                }
                InputCardKind::Extension => {
                    if self.extension_ui_queue.is_empty() {
                        self.input_card_order.pop_front();
                        continue;
                    }
                    self.advance_extension_ui_queue();
                    return;
                }
            }
        }
    }

    fn advance_extension_ui_queue(&mut self) {
        // bd-1qol9: global serialization — see advance_ask_ui_queue.
        if self.active_input_card_kind.is_some() {
            return;
        }
        if let Some(next) = self.extension_ui_queue.pop_front() {
            if next.method == "custom" {
                // Custom overlays poll independent of the text-card slot.
                self.handle_custom_extension_ui_request(next);
                self.advance_extension_ui_queue();
                return;
            }
            self.capture_preexisting_card_draft();
            self.active_input_card_kind = Some(InputCardKind::Extension);
            let prompt = format_extension_ui_prompt(&next);
            self.active_extension_ui = Some(next);
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: prompt,
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_bottom();
            self.input.focus();
        }
    }

    fn dispatch_extension_command(&mut self, command: &str, args: &str) -> Option<Cmd> {
        let Some(manager) = &self.extensions else {
            self.status_message = Some("Extensions are disabled".to_string());
            return None;
        };

        let Some(runtime) = manager.runtime() else {
            self.status_message = Some(format!(
                "Extension command '/{command}' is not available (runtime not enabled)"
            ));
            return None;
        };

        self.agent_state = AgentState::ToolRunning;
        self.current_tool = Some(format!("/{command}"));

        let command_name = command.to_string();
        let args_str = args.to_string();
        let cwd = self.cwd.display().to_string();
        let event_tx = self.event_tx.clone();
        let runtime_handle = self.runtime_handle.clone();

        let ctx_payload = serde_json::json!({
            "cwd": cwd,
            "hasUI": true,
        });

        let cmd_for_msg = command_name.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        runtime_handle.spawn(async move {
            let result = runtime
                .execute_command(
                    command_name,
                    args_str,
                    std::sync::Arc::new(ctx_payload),
                    EXTENSION_INTERACTIVE_COMMAND_TIMEOUT_MS,
                )
                .await;

            match result {
                Ok(value) => {
                    let display = if value.is_null() || value == serde_json::Value::Null {
                        format!("/{cmd_for_msg} completed.")
                    } else if let Some(s) = value.as_str() {
                        s.to_string()
                    } else {
                        format!("/{cmd_for_msg} completed: {value}")
                    };
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::ExtensionCommandDone {
                            command: cmd_for_msg,
                            display,
                            is_error: false,
                        },
                    )
                    .await;
                }
                Err(err) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::ExtensionCommandDone {
                            command: cmd_for_msg,
                            display: format!("Extension command error: {err}"),
                            is_error: true,
                        },
                    )
                    .await;
                }
            }
        });

        None
    }

    pub(super) fn dispatch_extension_shortcut(&mut self, key_id: &str) -> Option<Cmd> {
        let Some(manager) = &self.extensions else {
            self.status_message = Some("Extensions are disabled".to_string());
            return None;
        };

        let Some(runtime) = manager.runtime() else {
            self.status_message =
                Some("Extension shortcut not available (runtime not enabled)".to_string());
            return None;
        };

        self.agent_state = AgentState::ToolRunning;
        self.current_tool = Some(format!("shortcut:{key_id}"));

        let key_id_owned = key_id.to_string();
        let cwd = self.cwd.display().to_string();
        let event_tx = self.event_tx.clone();
        let runtime_handle = self.runtime_handle.clone();

        let ctx_payload = serde_json::json!({
            "cwd": cwd,
            "hasUI": true,
        });

        let key_for_msg = key_id_owned.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        runtime_handle.spawn(async move {
            let result = runtime
                .execute_shortcut(
                    key_id_owned,
                    std::sync::Arc::new(ctx_payload),
                    crate::extensions::EXTENSION_SHORTCUT_BUDGET_MS,
                )
                .await;

            match result {
                Ok(_) => {
                    let display = format!("Shortcut [{key_for_msg}] executed.");
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::ExtensionCommandDone {
                            command: key_for_msg,
                            display,
                            is_error: false,
                        },
                    )
                    .await;
                }
                Err(err) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::ExtensionCommandDone {
                            command: key_for_msg,
                            display: format!("Shortcut error: {err}"),
                            is_error: true,
                        },
                    )
                    .await;
                }
            }
        });

        None
    }

    /// Fire the one-shot auto-titling task when eligible (bd-cv653.3.1):
    /// persisted session, still unnamed, exactly one user message so far, a
    /// tiny/smol-role model resolved, and titling not yet requested.
    fn maybe_request_session_title(&mut self) {
        if self.title_requested || !self.save_enabled {
            return;
        }
        let Some(entry) = self.title_model_entry.clone() else {
            return;
        };
        let Some(owner_session_id) = self
            .session
            .try_lock()
            .ok()
            .and_then(|guard| guard.get_name().is_none().then(|| guard.header.id.clone()))
        else {
            return;
        };
        let mut user_texts = self
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .map(|m| m.content.clone());
        let Some(user_text) = user_texts.next() else {
            return;
        };
        if user_texts.next().is_some() {
            // Only title on the first exchange — later arrivals suggest the
            // session already has an established topic.
            return;
        }
        if user_text.trim().is_empty() {
            return;
        }
        let assistant_excerpt = self
            .messages
            .iter()
            .rev()
            .find(|m| m.role == MessageRole::Assistant)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        self.title_requested = true;
        let event_tx = self.event_tx.clone();
        self.runtime_handle.spawn(async move {
            let cx = Cx::for_request();
            if let Some(title) =
                generate_session_title(&entry, &user_text, &assistant_excerpt).await
            {
                crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &cx,
                    PiMsg::SessionTitleSuggestion {
                        owner_session_id,
                        title,
                    },
                )
                .await;
            }
        });
    }

    fn run_next_pending(&mut self) -> Option<Cmd> {
        loop {
            if self.agent_state != AgentState::Idle {
                return None;
            }
            let next = self.pending_inputs.pop_front()?;

            let cmd = match next {
                PendingInput::Text(text) => self.submit_message(&text),
                PendingInput::GeneratedText(text) => self
                    .submit_content_with_display_and_keyword_source(
                        vec![ContentBlock::Text(TextContent::new(text.clone()))],
                        &text,
                        Some(String::new()),
                    ),
                PendingInput::Content(content) => self.submit_content(content),
                PendingInput::ContentWithKeywordSource {
                    content,
                    keyword_scan_source,
                } => {
                    let display = content_blocks_to_text(&content);
                    self.submit_content_with_display_and_keyword_source(
                        content,
                        &display,
                        Some(keyword_scan_source),
                    )
                }
                PendingInput::Continue => self.submit_continue(),
            };

            if cmd.is_some() {
                return cmd;
            }
        }
    }

    pub(super) fn queue_input(&mut self, kind: QueuedMessageKind) {
        let raw_text = self.input.value();
        let trimmed = raw_text.trim();
        if trimmed.is_empty() {
            self.status_message = Some("No input to queue".to_string());
            return;
        }

        if let Some((command, _args)) = parse_extension_command(trimmed)
            && let Some(manager) = &self.extensions
            && manager.has_command(&command)
        {
            self.status_message = Some(format!(
                "Extension command '/{command}' cannot be queued while busy"
            ));
            return;
        }

        let expanded = self.resources.expand_input(trimmed);

        // Track input history
        self.history.push(trimmed.to_string());

        if let Ok(mut queue) = self.message_queue.lock() {
            let queued = QueuedAgentMessage::authored(build_user_message(expanded), trimmed);
            match kind {
                QueuedMessageKind::Steering => queue.push_steering(queued),
                QueuedMessageKind::FollowUp => queue.push_follow_up(queued),
            }
        }

        // Clear input and reset to single-line mode
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        let label = match kind {
            QueuedMessageKind::Steering => "steering",
            QueuedMessageKind::FollowUp => "follow-up",
        };
        self.status_message = Some(format!("Queued {label} message"));
    }

    pub(super) fn restore_queued_messages_to_editor(&mut self, abort: bool) -> usize {
        let (steering, follow_up) = self
            .message_queue
            .lock()
            .map_or_else(|_| (Vec::new(), Vec::new()), |mut queue| queue.clear_all());
        let mut all = steering;
        all.extend(follow_up);
        if all.is_empty() {
            if abort {
                self.abort_agent();
            }
            return 0;
        }

        let queued_text = all
            .iter()
            .filter_map(QueuedAgentMessage::text_for_display)
            .collect::<Vec<_>>()
            .join("\n\n");
        let current_text = self.input.value();
        let combined = [queued_text, current_text]
            .into_iter()
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.input.set_value(&combined);
        if combined.contains('\n') {
            self.input_mode = InputMode::MultiLine;
            self.set_input_height(6);
        }
        self.input.focus();

        if abort {
            self.abort_agent();
        }

        all.len()
    }

    fn abort_agent(&self) {
        if let Some(handle) = &self.abort_handle {
            handle.abort();
        }
    }

    #[allow(clippy::too_many_lines)]
    fn submit_continue(&mut self) -> Option<Cmd> {
        if let Err(message) = self.sync_runtime_selection_from_session_header() {
            self.status_message = Some(message);
            return None;
        }

        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let extensions = self.extensions.clone();
        let mcp_manager = self.mcp_manager.clone();
        let runtime_handle = self.runtime_handle.clone();
        let tui_pressure_frame_p99_us = Arc::clone(&self.tui_pressure_frame_p99_us);
        let (abort_handle, abort_signal) = AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        self.agent_state = AgentState::Processing;
        self.scroll_to_bottom();

        let runtime_handle_for_task = runtime_handle.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        runtime_handle.spawn(async move {
            #[cfg(test)]
            emit_submit_continue_deadline_probe(task_cx.budget().deadline);
            if let Some(manager) = extensions.clone() {
                let _ = manager
                    .dispatch_event(ExtensionEventName::BeforeAgentStart, None)
                    .await;
            }

            let mut agent_guard =
                match asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&agent), &task_cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &Cx::for_request(),
                            PiMsg::AgentError(format!("Failed to lock agent: {err}")),
                        )
                        .await;
                        return;
                    }
                };
            // MCP servers an extension registered after startup reach this
            // turn's agent (bd-8m21l); same seam as the SDK prompt entry.
            if let (Some(mcp), Some(ext)) = (mcp_manager.as_ref(), extensions.as_ref()) {
                crate::mcp::sync_extension_registrations(mcp, ext, &mut agent_guard).await;
            }
            let previous_len = agent_guard.messages().len();

            let event_sender = event_tx.clone();
            let extensions = extensions.clone();
            let runtime_handle = runtime_handle_for_task.clone();
            let coalescer = extensions
                .as_ref()
                .map(|m| crate::extensions::EventCoalescer::new(m.clone()));
            let ui_stream_batcher =
                Arc::new(StdMutex::new(UiStreamDeltaBatcher::new_with_frame_p99(
                    event_sender.clone(),
                    Arc::clone(&tui_pressure_frame_p99_us),
                )));
            let ui_stream_batcher_for_events = Arc::clone(&ui_stream_batcher);
            let result = agent_guard
                .run_continue_with_abort(Some(abort_signal), move |event| {
                    {
                        let mut batcher = match ui_stream_batcher_for_events.lock() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        dispatch_agent_event_to_ui(&event, &mut batcher);
                    }

                    if let Some(coal) = &coalescer {
                        coal.dispatch_agent_event_lazy(&event, &runtime_handle);
                    }
                })
                .await;
            flush_ui_stream_batcher_with_backpressure(&ui_stream_batcher).await;

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            let mut session_guard =
                match asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&session), &task_cx).await
                {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &Cx::for_request(),
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
            let repairs = match agent_guard.drain_repair_ledger() {
                Ok(repairs) => repairs,
                Err(err) => {
                    drop(session_guard);
                    drop(agent_guard);
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &Cx::for_request(),
                        PiMsg::AgentError(format!("Failed to drain turn audit ledger: {err}")),
                    )
                    .await;
                    return;
                }
            };
            let keyword_activations = agent_guard.drain_keyword_ledger();
            drop(agent_guard);
            append_turn_artifacts(
                &mut session_guard,
                new_messages,
                &repairs,
                &keyword_activations,
            );
            let save_error = if save_enabled && let Err(err) = session_guard.save().await {
                Some(format!("Failed to save session: {err}"))
            } else {
                None
            };
            drop(session_guard);

            if let Some(err) = save_error {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &Cx::for_request(),
                    PiMsg::AgentError(err),
                )
                .await;
            }

            if let Err(err) = result
                && !turn_error_already_surfaced(&ui_stream_batcher)
            {
                let formatted = crate::error_hints::format_error_with_hints(&err);
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &Cx::for_request(),
                    PiMsg::AgentError(formatted),
                )
                .await;
            }
        });

        None
    }

    #[allow(clippy::too_many_lines)]
    fn submit_content(&mut self, content: Vec<ContentBlock>) -> Option<Cmd> {
        let display = content_blocks_to_text(&content);
        self.submit_content_with_display(content, &display)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn submit_content_with_display(
        &mut self,
        content: Vec<ContentBlock>,
        display: &str,
    ) -> Option<Cmd> {
        self.submit_content_with_display_and_keyword_source(content, display, None)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn submit_content_with_display_and_keyword_source(
        &mut self,
        content: Vec<ContentBlock>,
        display: &str,
        keyword_scan_source: Option<String>,
    ) -> Option<Cmd> {
        if content.is_empty() {
            return None;
        }

        if let Err(message) = self.sync_runtime_selection_from_session_header() {
            self.status_message = Some(message);
            return None;
        }

        let display_owned = display.to_string();
        if !display_owned.trim().is_empty() {
            self.messages.push(ConversationMessage {
                role: MessageRole::User,
                content: display_owned.clone(),
                thinking: None,
                collapsed: false,
            });
        }

        // Clear input and reset to single-line mode
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        // Start processing
        self.agent_state = AgentState::Processing;

        // Auto-scroll to bottom when new message is added
        self.scroll_to_bottom();

        let content_for_agent = content;
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let extensions = self.extensions.clone();
        let mcp_manager = self.mcp_manager.clone();
        let runtime_handle = self.runtime_handle.clone();
        let tui_pressure_frame_p99_us = Arc::clone(&self.tui_pressure_frame_p99_us);
        let (abort_handle, abort_signal) = AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        let runtime_handle_for_task = runtime_handle.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        runtime_handle.spawn(async move {
            let mut content_for_agent = content_for_agent;
            let base_system_prompt = {
                let guard =
                    match asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&agent), &task_cx)
                        .await
                    {
                        Ok(guard) => guard,
                        Err(err) => {
                            let _ = crate::interactive::enqueue_pi_event(
                                &event_tx,
                                &Cx::for_request(),
                                PiMsg::AgentError(format!("Failed to lock agent: {err}")),
                            )
                            .await;
                            return;
                        }
                    };
                let prompt = guard.system_prompt().map(str::to_string);
                drop(guard);
                prompt
            };
            let before_start = if let Some(manager) = extensions.clone() {
                let (text, images) = split_content_blocks_for_input(&content_for_agent);
                match dispatch_input_event(&manager, text, images).await {
                    Ok(InputEventOutcome::Continue { text, images }) => {
                        content_for_agent = build_content_blocks_for_input(&text, &images);
                        let updated = content_blocks_to_text(&content_for_agent);
                        if updated != display_owned {
                            let _ = crate::interactive::enqueue_pi_event(
                                &event_tx,
                                &task_cx,
                                PiMsg::UpdateLastUserMessage(updated),
                            )
                            .await;
                        }
                    }
                    Ok(InputEventOutcome::Block { reason }) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::UpdateLastUserMessage("[input blocked]".to_string()),
                        )
                        .await;
                        let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::AgentError(message),
                        )
                        .await;
                        return;
                    }
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::AgentError(err.to_string()),
                        )
                        .await;
                        return;
                    }
                }

                let (text, images) = split_content_blocks_for_input(&content_for_agent);
                dispatch_before_agent_start_event(
                    &manager,
                    &text,
                    &images,
                    base_system_prompt.as_deref().unwrap_or(""),
                )
                .await
            } else {
                BeforeAgentStartOutcome {
                    messages: Vec::new(),
                    system_prompt: None,
                }
            };

            let mut agent_guard =
                match asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&agent), &task_cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &Cx::for_request(),
                            PiMsg::AgentError(format!("Failed to lock agent: {err}")),
                        )
                        .await;
                        return;
                    }
                };
            // MCP servers an extension registered after startup reach this
            // turn's agent (bd-8m21l); same seam as the SDK prompt entry.
            if let (Some(mcp), Some(ext)) = (mcp_manager.as_ref(), extensions.as_ref()) {
                crate::mcp::sync_extension_registrations(mcp, ext, &mut agent_guard).await;
            }
            let BeforeAgentStartOutcome {
                messages: before_messages,
                system_prompt,
            } = before_start;
            let mut turn_agent =
                TurnSystemPromptGuard::new(&mut agent_guard, base_system_prompt, system_prompt);
            let preserve_plain_text_shape = keyword_scan_source.as_deref() == Some("");
            turn_agent.set_magic_keyword_scan_override(keyword_scan_source);
            let previous_len = turn_agent.messages().len();

            let event_sender = event_tx.clone();
            let extensions = extensions.clone();
            let runtime_handle = runtime_handle_for_task.clone();
            let coalescer = extensions
                .as_ref()
                .map(|m| crate::extensions::EventCoalescer::new(m.clone()));
            let ui_stream_batcher =
                Arc::new(StdMutex::new(UiStreamDeltaBatcher::new_with_frame_p99(
                    event_sender.clone(),
                    Arc::clone(&tui_pressure_frame_p99_us),
                )));
            let ui_stream_batcher_for_events = Arc::clone(&ui_stream_batcher);
            let user_content = match content_for_agent.as_slice() {
                [ContentBlock::Text(text)] if preserve_plain_text_shape => {
                    UserContent::Text(text.text.clone())
                }
                _ => UserContent::Blocks(content_for_agent),
            };
            let user_message = ModelMessage::User(UserMessage {
                content: user_content,
                timestamp: Utc::now().timestamp_millis(),
            });
            let mut prompts = Vec::with_capacity(1 + before_messages.len());
            prompts.push(user_message);
            prompts.extend(before_messages.into_iter().map(ModelMessage::Custom));

            let result = turn_agent
                .run_with_messages_with_abort(prompts, Some(abort_signal), move |event| {
                    {
                        let mut batcher = match ui_stream_batcher_for_events.lock() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        dispatch_agent_event_to_ui(&event, &mut batcher);
                    }

                    if let Some(coal) = &coalescer {
                        coal.dispatch_agent_event_lazy(&event, &runtime_handle);
                    }
                })
                .await;
            flush_ui_stream_batcher_with_backpressure(&ui_stream_batcher).await;

            drop(turn_agent);

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            let mut session_guard =
                match asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&session), &task_cx).await
                {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &Cx::for_request(),
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
            let repairs = match agent_guard.drain_repair_ledger() {
                Ok(repairs) => repairs,
                Err(err) => {
                    drop(session_guard);
                    drop(agent_guard);
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &Cx::for_request(),
                        PiMsg::AgentError(format!("Failed to drain turn audit ledger: {err}")),
                    )
                    .await;
                    return;
                }
            };
            let keyword_activations = agent_guard.drain_keyword_ledger();
            drop(agent_guard);
            append_turn_artifacts(
                &mut session_guard,
                new_messages,
                &repairs,
                &keyword_activations,
            );
            let save_error = if save_enabled && let Err(err) = session_guard.save().await {
                Some(format!("Failed to save session: {err}"))
            } else {
                None
            };
            drop(session_guard);

            if let Some(err) = save_error {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &Cx::for_request(),
                    PiMsg::AgentError(err),
                )
                .await;
            }

            if let Err(err) = result
                && !turn_error_already_surfaced(&ui_stream_batcher)
            {
                let formatted = crate::error_hints::format_error_with_hints(&err);
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &Cx::for_request(),
                    PiMsg::AgentError(formatted),
                )
                .await;
            }
        });

        None
    }

    /// Submit a message to the agent.
    #[allow(clippy::too_many_lines)]
    pub(super) fn submit_message(&mut self, message: &str) -> Option<Cmd> {
        let message = message.trim();
        if message.is_empty() {
            return None;
        }

        // bd-1qol9: exactly ONE card owns the editor — the ACTIVE slot wins
        // regardless of kind, so a newer extension card can no longer have
        // its answer stolen by an older ask card (or vice versa).
        if self.active_input_card_kind == Some(InputCardKind::Extension)
            && self.resolve_active_extension_ui_with_line(message)
        {
            return None;
        }
        if self.handle_ask_ui_input(message) {
            return None;
        }

        if let Some(pending) = self.pending_oauth.take() {
            return self.submit_oauth_code(message, pending);
        }

        if let Some((command, exclude_from_context)) = parse_bash_command(message) {
            return self.submit_bash_command(message, command, exclude_from_context);
        }

        // Check for slash commands
        if let Some((cmd, args)) = SlashCommand::parse(message) {
            return self.handle_slash_command(cmd, args);
        }

        if let Some((command, args)) = parse_extension_command(message)
            && let Some(manager) = &self.extensions
            && manager.has_command(&command)
        {
            return self.dispatch_extension_command(&command, args);
        }

        // Bare prompt-template invocation (#172): autocomplete advertises
        // `/name` for every loaded prompt template, so `/name [args]` must
        // expand the template instead of falling through to
        // "Unknown command". Built-in and extension commands keep priority.
        if let Some(rest) = message.strip_prefix('/')
            && !message.starts_with("/skill:")
        {
            let name = rest.split_whitespace().next().unwrap_or("");
            let is_template = !name.is_empty()
                && self
                    .resources
                    .prompts()
                    .iter()
                    .any(|template| template.name == name);
            if is_template {
                return self.handle_slash_template(rest);
            }
        }

        if message.starts_with('/') && !message.starts_with("/skill:") {
            let command = message.split_whitespace().next().unwrap_or(message);
            let error = format!("Unknown command: {command}");
            self.status_message = Some(error.clone());
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: error,
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_bottom();
            self.input.reset();
            self.input.focus();
            return None;
        }

        if let Err(message) = self.sync_runtime_selection_from_session_header() {
            self.status_message = Some(message);
            return None;
        }

        let message_owned = message.to_string();
        let (message_without_refs, file_refs) = self.extract_file_references(&message_owned);
        let keyword_scan_source = message_without_refs.trim().to_string();
        let message_for_agent = if file_refs.is_empty() {
            self.resources.expand_input(&message_owned)
        } else {
            self.resources.expand_input(message_without_refs.trim())
        };

        if !file_refs.is_empty() {
            let auto_resize = self
                .config
                .images
                .as_ref()
                .and_then(|images| images.auto_resize)
                .unwrap_or(true);

            let processed = match process_file_arguments(
                &file_refs,
                &self.cwd,
                auto_resize,
                self.workspace(),
            ) {
                Ok(processed) => processed,
                Err(err) => {
                    self.status_message = Some(err.to_string());
                    return None;
                }
            };

            let mut text = processed.text;
            if !message_for_agent.trim().is_empty() {
                text.push_str(&message_for_agent);
            }

            let mut content = Vec::new();
            if !text.trim().is_empty() {
                content.push(ContentBlock::Text(TextContent::new(text)));
            }
            for image in processed.images {
                content.push(ContentBlock::Image(image));
            }

            self.history.push(message_owned.clone());

            let display = content_blocks_to_text(&content);
            return self.submit_content_with_display_and_keyword_source(
                content,
                &display,
                Some(keyword_scan_source),
            );
        }
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let extensions = self.extensions.clone();
        let mcp_manager = self.mcp_manager.clone();
        let tui_pressure_frame_p99_us = Arc::clone(&self.tui_pressure_frame_p99_us);
        let (abort_handle, abort_signal) = AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        // Add to history
        self.history.push(message_owned.clone());

        // Add user message to display
        self.messages.push(ConversationMessage {
            role: MessageRole::User,
            content: message_for_agent.clone(),
            thinking: None,
            collapsed: false,
        });
        let displayed_message = message_for_agent.clone();

        // Clear input and reset to single-line mode
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        // Start processing
        self.agent_state = AgentState::Processing;

        // Auto-scroll to bottom when new message is added
        self.scroll_to_bottom();

        let runtime_handle = self.runtime_handle.clone();
        let keyword_scan_source = message_owned;

        // Spawn async task to run the agent
        let runtime_handle_for_agent = runtime_handle.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        runtime_handle.spawn(async move {
            let mut message_for_agent = message_for_agent;
            let mut input_images = Vec::new();
            let base_system_prompt = {
                let guard =
                    match asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&agent), &task_cx)
                        .await
                    {
                        Ok(guard) => guard,
                        Err(err) => {
                            let _ = crate::interactive::enqueue_pi_event(
                                &event_tx,
                                &Cx::for_request(),
                                PiMsg::AgentError(format!("Failed to lock agent: {err}")),
                            )
                            .await;
                            return;
                        }
                    };
                let prompt = guard.system_prompt().map(str::to_string);
                drop(guard);
                prompt
            };
            let before_start = if let Some(manager) = extensions.clone() {
                match dispatch_input_event(&manager, message_for_agent.clone(), Vec::new()).await {
                    Ok(InputEventOutcome::Continue { text, images }) => {
                        message_for_agent = text;
                        input_images = images;
                        if message_for_agent != displayed_message {
                            let _ = crate::interactive::enqueue_pi_event(
                                &event_tx,
                                &task_cx,
                                PiMsg::UpdateLastUserMessage(message_for_agent.clone()),
                            )
                            .await;
                        }
                    }
                    Ok(InputEventOutcome::Block { reason }) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::UpdateLastUserMessage("[input blocked]".to_string()),
                        )
                        .await;
                        let message = reason.unwrap_or_else(|| "Input blocked".to_string());
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::AgentError(message),
                        )
                        .await;
                        return;
                    }
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::AgentError(err.to_string()),
                        )
                        .await;
                        return;
                    }
                }
                dispatch_before_agent_start_event(
                    &manager,
                    &message_for_agent,
                    &input_images,
                    base_system_prompt.as_deref().unwrap_or(""),
                )
                .await
            } else {
                BeforeAgentStartOutcome {
                    messages: Vec::new(),
                    system_prompt: None,
                }
            };

            let mut agent_guard =
                match asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&agent), &task_cx).await {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &Cx::for_request(),
                            PiMsg::AgentError(format!("Failed to lock agent: {err}")),
                        )
                        .await;
                        return;
                    }
                };
            // MCP servers an extension registered after startup reach this
            // turn's agent (bd-8m21l); same seam as the SDK prompt entry.
            if let (Some(mcp), Some(ext)) = (mcp_manager.as_ref(), extensions.as_ref()) {
                crate::mcp::sync_extension_registrations(mcp, ext, &mut agent_guard).await;
            }
            let BeforeAgentStartOutcome {
                messages: before_messages,
                system_prompt,
            } = before_start;
            let mut turn_agent =
                TurnSystemPromptGuard::new(&mut agent_guard, base_system_prompt, system_prompt);
            let previous_len = turn_agent.messages().len();
            turn_agent.set_magic_keyword_scan_override(Some(keyword_scan_source));

            let event_sender = event_tx.clone();
            let extensions = extensions.clone();
            let coalescer = extensions
                .as_ref()
                .map(|m| crate::extensions::EventCoalescer::new(m.clone()));
            let ui_stream_batcher =
                Arc::new(StdMutex::new(UiStreamDeltaBatcher::new_with_frame_p99(
                    event_sender.clone(),
                    Arc::clone(&tui_pressure_frame_p99_us),
                )));
            let user_content = if input_images.is_empty() {
                UserContent::Text(message_for_agent)
            } else {
                UserContent::Blocks(build_content_blocks_for_input(
                    &message_for_agent,
                    &input_images,
                ))
            };
            let user_message = ModelMessage::User(UserMessage {
                content: user_content,
                timestamp: Utc::now().timestamp_millis(),
            });
            let mut prompts = Vec::with_capacity(1 + before_messages.len());
            prompts.push(user_message);
            prompts.extend(before_messages.into_iter().map(ModelMessage::Custom));
            let ui_stream_batcher_for_events = Arc::clone(&ui_stream_batcher);
            let result = turn_agent
                .run_with_messages_with_abort(prompts, Some(abort_signal), move |event| {
                    {
                        let mut batcher = match ui_stream_batcher_for_events.lock() {
                            Ok(guard) => guard,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        dispatch_agent_event_to_ui(&event, &mut batcher);
                    }

                    if let Some(coal) = &coalescer {
                        coal.dispatch_agent_event_lazy(&event, &runtime_handle_for_agent);
                    }
                })
                .await;
            flush_ui_stream_batcher_with_backpressure(&ui_stream_batcher).await;

            drop(turn_agent);

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            let mut session_guard =
                match asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&session), &task_cx).await
                {
                    Ok(guard) => guard,
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &Cx::for_request(),
                            PiMsg::AgentError(format!("Failed to lock session: {err}")),
                        )
                        .await;
                        return;
                    }
                };
            let repairs = match agent_guard.drain_repair_ledger() {
                Ok(repairs) => repairs,
                Err(err) => {
                    drop(session_guard);
                    drop(agent_guard);
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &Cx::for_request(),
                        PiMsg::AgentError(format!("Failed to drain turn audit ledger: {err}")),
                    )
                    .await;
                    return;
                }
            };
            let keyword_activations = agent_guard.drain_keyword_ledger();
            drop(agent_guard);
            append_turn_artifacts(
                &mut session_guard,
                new_messages,
                &repairs,
                &keyword_activations,
            );
            let save_error = if save_enabled && let Err(err) = session_guard.save().await {
                Some(format!("Failed to save session: {err}"))
            } else {
                None
            };
            drop(session_guard);

            if let Some(err) = save_error {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &Cx::for_request(),
                    PiMsg::AgentError(err),
                )
                .await;
            }

            if let Err(err) = result
                && !turn_error_already_surfaced(&ui_stream_batcher)
            {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &Cx::for_request(),
                    PiMsg::AgentError(err.to_string()),
                )
                .await;
            }
        });

        None
    }
}

#[cfg(test)]
fn submit_continue_deadline_probe()
-> &'static std::sync::Mutex<Option<std::sync::mpsc::Sender<Option<asupersync::Time>>>> {
    static PROBE: std::sync::OnceLock<
        std::sync::Mutex<Option<std::sync::mpsc::Sender<Option<asupersync::Time>>>>,
    > = std::sync::OnceLock::new();
    PROBE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn emit_submit_continue_deadline_probe(deadline: Option<asupersync::Time>) {
    let probe = submit_continue_deadline_probe();
    let guard = probe.lock().expect("lock submit_continue deadline probe");
    if let Some(tx) = guard.as_ref() {
        let _ = tx.send(deadline);
    }
}

#[cfg(test)]
mod tool_invocation_summary_tests {
    use super::tool_invocation_summary;
    use serde_json::json;

    #[test]
    fn bash_command_first_line_clipped() {
        let summary = tool_invocation_summary("bash", &json!({"command": "cargo test --lib"}));
        assert_eq!(summary.as_deref(), Some("cargo test --lib"));
    }

    #[test]
    fn bash_multiline_command_keeps_first_nonblank_line_with_ellipsis() {
        let summary =
            tool_invocation_summary("bash", &json!({"command": "\n  cargo build\ncargo test"}));
        assert_eq!(summary.as_deref(), Some("cargo build…"));
    }

    #[test]
    fn bash_long_command_is_truncated() {
        let long = "x".repeat(300);
        let summary = tool_invocation_summary("bash", &json!({ "command": long }));
        let summary = summary.expect("summary for long command");
        assert!(summary.chars().count() <= 97, "96 chars + ellipsis");
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn read_uses_path_and_grep_combines_pattern_with_path() {
        assert_eq!(
            tool_invocation_summary("read", &json!({"path": "src/main.rs"})).as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            tool_invocation_summary("grep", &json!({"pattern": "TODO", "path": "src/"})).as_deref(),
            Some("TODO in src/")
        );
    }

    #[test]
    fn control_chars_are_stripped_from_summaries() {
        // Escape bytes in a command string must not reach the transcript
        // header (the output sanitizer runs on tool output, not on args).
        let summary = tool_invocation_summary(
            "bash",
            &json!({"command": "echo \u{1b}[31mred\u{7}\u{7f} done"}),
        );
        assert_eq!(summary.as_deref(), Some("echo [31mred done"));
    }

    #[test]
    fn unknown_tools_and_missing_or_blank_args_are_handled_consistently() {
        assert_eq!(
            tool_invocation_summary("todo", &json!({"op": "view"})).as_deref(),
            Some("view")
        );
        assert!(tool_invocation_summary("extension_owned", &json!({})).is_none());
        assert!(tool_invocation_summary("bash", &json!({})).is_none());
        assert!(tool_invocation_summary("bash", &json!({"command": "   \n  "})).is_none());
        assert!(tool_invocation_summary("bash", &json!({"command": 42})).is_none());
    }
}

#[cfg(test)]
mod stream_delta_batcher_tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::config::Config;
    use crate::keybindings::KeyBindings;
    use crate::model::{AssistantMessage, StreamEvent, Usage};
    use crate::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
    use crate::resources::{ResourceCliOptions, ResourceLoader};
    use crate::session::Session;
    use crate::tools::{Tool, ToolRegistry};
    use asupersync::runtime::RuntimeBuilder;
    use futures::stream;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

    struct DummyProvider;

    // Test doubles report the identity of the app's model entry
    // (`continue-probe` / `continue-probe-model`, see
    // `build_test_app_with_provider`): every submission first runs
    // `sync_runtime_selection_from_session_header`, which replaces a provider
    // whose name/model differ from the entry with a real client built from the
    // entry's `https://example.invalid` base URL, and the turn then fails on
    // DNS instead of exercising the double.
    #[async_trait::async_trait]
    impl Provider for DummyProvider {
        fn name(&self) -> &'static str {
            "continue-probe"
        }

        fn api(&self) -> &'static str {
            "dummy"
        }

        fn model_id(&self) -> &'static str {
            "continue-probe-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(stream::empty()))
        }
    }

    /// Provider double whose every request fails with an HTTP 503 (#209).
    struct Overloaded503Provider;

    const OVERLOADED_503_BODY: &str = "OpenAI API error (HTTP 503): \
{\"error\":{\"code\":\"service_unavailable_error\",\"message\":\"Server Overloaded\"}}";

    #[async_trait::async_trait]
    impl Provider for Overloaded503Provider {
        fn name(&self) -> &'static str {
            "continue-probe"
        }

        fn api(&self) -> &'static str {
            "dummy"
        }

        fn model_id(&self) -> &'static str {
            "continue-probe-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Err(crate::error::Error::provider(
                "continue-probe",
                OVERLOADED_503_BODY,
            ))
        }
    }

    /// #209: a terminal provider error (HTTP 503) ends the turn with exactly
    /// one structured card in the transcript plus a one-line status, and the
    /// task's `Err` path does not add a duplicate `AgentError` block.
    #[test]
    fn provider_503_surfaces_one_turn_end_card_and_no_duplicate() {
        let (mut app, mut event_rx) = build_test_app_with_provider(Arc::new(Overloaded503Provider));
        let _ = app.submit_message("hello");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_done = false;
        while std::time::Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(msg) => {
                    let is_done = matches!(msg, PiMsg::AgentDone { .. });
                    if let PiMsg::AgentError(err) = &msg {
                        panic!("error must surface via the turn-end card, got AgentError: {err}");
                    }
                    let _ = app.handle_pi_message(msg);
                    if is_done {
                        saw_done = true;
                        break;
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        assert!(saw_done, "turn did not finish before the deadline");

        // The task sends its (deduplicated) Err notice after AgentDone; give
        // it a moment and make sure nothing extra arrives.
        let settle = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < settle {
            match event_rx.try_recv() {
                Ok(PiMsg::AgentError(err)) => {
                    panic!("duplicate error block after the turn-end card: {err}")
                }
                Ok(msg) => {
                    let _ = app.handle_pi_message(msg);
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }

        let headline =
            "Provider error: continue-probe: HTTP 503 (service unavailable / overloaded)";
        let cards: Vec<&str> = app
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::System && m.content.starts_with("Provider error:"))
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(cards.len(), 1, "expected one turn-end card, got {cards:?}");
        let card = cards[0];
        assert!(card.starts_with(headline), "headline missing: {card}");
        assert!(
            card.contains("not auto-retried"),
            "retry status missing: {card}"
        );
        assert!(
            card.contains("Detail: OpenAI API error (HTTP 503)"),
            "detail missing: {card}"
        );
        assert_eq!(app.status_message.as_deref(), Some(headline));
        assert_eq!(app.agent_state, AgentState::Idle);
    }

    /// #209: a mid-stream failure after partial text used to leave only the
    /// status line behind; the card must be pushed regardless.
    #[test]
    fn agent_done_error_after_partial_text_still_pushes_card() {
        let mut app = build_test_app();
        app.agent_state = AgentState::Processing;
        app.current_response = "partial answer".to_string();
        let card = crate::error::ProviderErrorSummary::from_error_text(
            Some("deepseek"),
            OVERLOADED_503_BODY,
        )
        .turn_end_card(OVERLOADED_503_BODY, None);
        let _ = app.handle_pi_message(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Error,
            error_message: Some(card),
        });

        assert!(
            app.messages
                .iter()
                .any(|m| m.role == MessageRole::Assistant && m.content == "partial answer"),
            "partial text must be kept"
        );
        assert!(
            app.messages.iter().any(|m| m.role == MessageRole::System
                && m.content.starts_with("Provider error: deepseek: HTTP 503")),
            "turn-end card missing: {:?}",
            app.messages
        );
        assert_eq!(
            app.status_message.as_deref(),
            Some("Provider error: deepseek: HTTP 503 (service unavailable / overloaded)")
        );
        assert_eq!(app.agent_state, AgentState::Idle);
    }

    #[test]
    fn turn_system_prompt_guard_restores_base_prompt_when_guard_is_dropped() {
        let mut agent = Agent::new(
            Arc::new(DummyProvider),
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig::default(),
        );
        agent.set_system_prompt(Some("base-system".to_string()));
        {
            let turn_agent = TurnSystemPromptGuard::new(
                &mut agent,
                Some("base-system".to_string()),
                Some("hook-system".to_string()),
            );
            assert_eq!(turn_agent.system_prompt(), Some("hook-system"));
        }
        assert_eq!(agent.system_prompt(), Some("base-system"));
    }

    #[test]
    fn before_agent_start_payload_preserves_prompt_images_and_system_prompt() {
        let image = ImageContent {
            data: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
        };
        let payload = before_agent_start_payload("hello", &[image], "base-system");
        assert_eq!(payload["prompt"], json!("hello"));
        assert_eq!(
            payload["images"],
            json!([{"data": "aW1hZ2U=", "mimeType": "image/png"}])
        );
        assert_eq!(payload["systemPrompt"], json!("base-system"));
    }

    fn runtime() -> &'static asupersync::runtime::Runtime {
        static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| {
            RuntimeBuilder::multi_thread()
                .blocking_threads(1, 8)
                .build()
                .expect("build runtime")
        })
    }

    fn runtime_handle() -> asupersync::runtime::RuntimeHandle {
        runtime().handle()
    }

    fn text_tool_update(text: &str) -> PiMsg {
        PiMsg::ToolUpdate {
            name: "bash".to_string(),
            tool_id: "t1".to_string(),
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({
                "progress": {
                    "byteCount": text.len(),
                    "lineCount": text.lines().count(),
                }
            })),
        }
    }

    fn model_entry(provider: &str, id: &str) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-completions".to_string(),
                provider: provider.to_string(),
                base_url: "https://example.invalid".to_string(),
                reasoning: true,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: Some("test-key".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    }

    fn build_test_app_with_provider(provider: Arc<dyn Provider>) -> (PiApp, mpsc::Receiver<PiMsg>) {
        let current = model_entry("continue-probe", "continue-probe-model");
        let agent = Agent::new(
            provider,
            ToolRegistry::new(&[], Path::new("."), None),
            AgentConfig::default(),
        );
        let session = Arc::new(asupersync::sync::Mutex::new(Session::in_memory()));
        let resources = ResourceLoader::empty(false);
        let resource_cli = ResourceCliOptions {
            no_skills: false,
            no_prompt_templates: false,
            no_extensions: false,
            no_themes: false,
            skill_paths: Vec::new(),
            prompt_paths: Vec::new(),
            extension_paths: Vec::new(),
            theme_paths: Vec::new(),
        };
        let (event_tx, event_rx) = asupersync::channel::mpsc::channel(64);
        let config = Config {
            last_changelog_version: Some(crate::platform::VERSION.to_string()),
            ..Config::default()
        };
        (
            PiApp::new(
                agent,
                session,
                config,
                resources,
                resource_cli,
                Path::new(".").to_path_buf(),
                current.clone(),
                Vec::new(),
                vec![current],
                None,
                Vec::new(),
                event_tx,
                runtime_handle(),
                true,
                false,
                None,
                Some(KeyBindings::new()),
                Vec::new(),
                Usage::default(),
                None,
            ),
            event_rx,
        )
    }

    fn build_test_app() -> PiApp {
        let (app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app
    }

    #[test]
    fn submit_message_expands_bare_prompt_template_command() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app.resources
            .push_prompt_for_tests(crate::resources::PromptTemplate {
                name: "tc-plan".to_string(),
                description: "Planning template".to_string(),
                content: "Plan the following: $ARGUMENTS".to_string(),
                source: "test".to_string(),
                file_path: std::path::PathBuf::from("tc-plan.md"),
            });

        let _ = app.submit_message("/tc-plan ship it");

        assert!(
            !app.messages
                .iter()
                .any(|m| m.content.contains("Unknown command")),
            "bare template invocation must not report Unknown command"
        );
        assert!(
            app.history
                .entries()
                .iter()
                .any(|entry| entry.value.contains("tc-plan")),
            "template expansion should record a history entry"
        );
    }

    #[test]
    fn queued_tui_template_keeps_raw_keyword_source_and_expanded_payload() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app.resources
            .push_prompt_for_tests(crate::resources::PromptTemplate {
                name: "queued".to_string(),
                description: "Queued provenance fixture".to_string(),
                content: "generated ultrathink workflowz payload; argument=$ARGUMENTS".to_string(),
                source: "test".to_string(),
                file_path: std::path::PathBuf::from("queued.md"),
            });
        app.input.set_value("/queued orchestrate");

        app.queue_input(QueuedMessageKind::Steering);

        let queued = app
            .message_queue
            .lock()
            .expect("message queue")
            .pop_steering();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].keyword_scan_source(), Some("/queued orchestrate"));
        assert!(matches!(
            queued[0].message(),
            ModelMessage::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) if text == "generated ultrathink workflowz payload; argument=orchestrate"
        ));
    }

    #[test]
    fn submit_message_unknown_slash_command_still_errors() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));

        let _ = app.submit_message("/definitely-not-a-template");

        assert!(
            app.messages
                .iter()
                .any(|m| m.content.contains("Unknown command")),
            "unknown commands must still be reported"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn interactive_text_turn_persists_keyword_and_repair_ledgers() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("fixture.txt"), "hello-fixture").expect("write fixture");
        let provider = Arc::new(InteractiveRepairProvider {
            calls: AtomicUsize::new(0),
        });
        let (mut app, mut event_rx) =
            build_test_app_with_provider(Arc::clone(&provider) as Arc<dyn Provider>);
        let mut entry = model_entry(provider.name(), provider.model_id());
        entry.compat = Some(crate::models::CompatConfig {
            tool_call_dialect: Some(crate::dialects::Dialect::Xmlish),
            ..crate::models::CompatConfig::default()
        });
        let agent = Agent::new(
            Arc::clone(&provider) as Arc<dyn Provider>,
            ToolRegistry::new(&["read"], temp.path(), None),
            AgentConfig::default(),
        );
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(temp.path().join("sessions")),
        )));
        app.agent = Arc::new(asupersync::sync::Mutex::new(agent));
        app.session = Arc::clone(&session);
        app.cwd = temp.path().to_path_buf();
        app.model_entry = entry.clone();
        app.available_models = vec![entry.clone()];
        if let Ok(mut shared) = app.model_entry_shared.lock() {
            *shared = entry;
        }
        app.save_enabled = true;

        let _ = app.submit_message("ultrathink check the fixture");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut saw_done = false;
        while std::time::Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(PiMsg::AgentDone { error_message, .. }) => {
                    assert!(error_message.is_none(), "turn error: {error_message:?}");
                    saw_done = true;
                    break;
                }
                Ok(PiMsg::AgentError(err)) => panic!("interactive turn failed: {err}"),
                Ok(_) | Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        assert!(
            saw_done,
            "interactive turn did not finish before the deadline"
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

        let persisted_path = runtime().block_on(async {
            let cx = Cx::for_request();
            let agent_guard = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.agent), &cx)
                .await
                .expect("agent completion lock");
            drop(agent_guard);
            let session_guard = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("session completion lock");
            session_guard
                .path
                .clone()
                .expect("interactive autosave created a session file")
        });
        let reopened = runtime()
            .block_on(Session::open(persisted_path.to_string_lossy().as_ref()))
            .expect("reopen interactive session");
        let entries = reopened.entries_for_current_path();
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(
                    entry,
                    crate::session::SessionEntry::Custom(custom)
                        if custom.custom_type == "dialect_repair"
                            && custom.data.as_ref().is_some_and(|data| data["tool"] == json!("read"))
                ))
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| matches!(
                    entry,
                    crate::session::SessionEntry::Custom(custom)
                        if custom.custom_type == "magic_keyword"
                            && custom.data.as_ref().is_some_and(|data| {
                                data["schema"] == json!("pi.magic_keyword.v1")
                                    && data["word"] == json!("ultrathink")
                            })
                ))
                .count(),
            1
        );
    }

    fn build_test_extension_manager_with_command_output(
        output: &Value,
    ) -> crate::extensions::ExtensionManager {
        let manager = crate::extensions::ExtensionManager::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("test-extension.native.json");
        let descriptor = json!({
            "id": "test-extension",
            "name": "test-extension",
            "version": "1.0.0",
            "apiVersion": crate::extensions::PROTOCOL_VERSION,
            "slashCommands": [
                {
                    "name": "deploy",
                    "description": "Deploy"
                }
            ],
            "commandOutputs": {
                "deploy": output
            }
        });
        std::fs::write(
            &entry,
            serde_json::to_vec(&descriptor).expect("serialize native extension descriptor"),
        )
        .expect("write native extension descriptor");

        runtime().block_on(async {
            let native_runtime = crate::extensions::NativeRustExtensionRuntimeHandle::start()
                .await
                .expect("start native runtime");
            manager.set_native_runtime(native_runtime);
            manager
                .load_native_extensions(vec![
                    crate::extensions::NativeRustExtensionLoadSpec::from_entry_path(&entry)
                        .expect("build native extension load spec"),
                ])
                .await
                .expect("load native extension");
        });

        manager
    }

    fn build_test_extension_manager_with_before_agent_start_output(
        output: &Value,
    ) -> crate::extensions::ExtensionManager {
        let manager = crate::extensions::ExtensionManager::new();
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("before-agent-start.native.json");
        let descriptor = json!({
            "id": "before-agent-start-test",
            "name": "before-agent-start-test",
            "version": "1.0.0",
            "apiVersion": crate::extensions::PROTOCOL_VERSION,
            "eventHooks": ["before_agent_start"],
            "eventResponses": {
                "before_agent_start": output
            }
        });
        std::fs::write(
            &entry,
            serde_json::to_vec(&descriptor).expect("serialize native extension descriptor"),
        )
        .expect("write native extension descriptor");

        runtime().block_on(async {
            let native_runtime = crate::extensions::NativeRustExtensionRuntimeHandle::start()
                .await
                .expect("start native runtime");
            manager.set_native_runtime(native_runtime);
            manager
                .load_native_extensions(vec![
                    crate::extensions::NativeRustExtensionLoadSpec::from_entry_path(&entry)
                        .expect("build native extension load spec"),
                ])
                .await
                .expect("load native extension");
        });

        manager
    }

    #[derive(Default)]
    struct BeforeAgentStartProbeState {
        calls: AtomicUsize,
        saw_user_message: AtomicBool,
        saw_custom_message: AtomicBool,
        saw_user_before_custom: AtomicBool,
        saw_expected_system_prompt: AtomicBool,
    }

    struct BeforeAgentStartProbeProvider {
        state: Arc<BeforeAgentStartProbeState>,
        expected_system_prompt: &'static str,
    }

    // Same identity rule as `DummyProvider`: match the app's model entry so the
    // pre-submission runtime sync keeps this double installed.
    #[async_trait::async_trait]
    impl Provider for BeforeAgentStartProbeProvider {
        fn name(&self) -> &'static str {
            "continue-probe"
        }

        fn api(&self) -> &'static str {
            "before-agent-start-probe"
        }

        fn model_id(&self) -> &'static str {
            "continue-probe-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.state.calls.fetch_add(1, Ordering::SeqCst);
            let user_index = context
                .messages
                .iter()
                .position(|message| matches!(message, ModelMessage::User(_)));
            let custom_index = context.messages.iter().position(|message| {
                matches!(
                    message,
                    ModelMessage::Custom(CustomMessage { custom_type, content, .. })
                        if custom_type == "hook-note" && content == "injected"
                )
            });
            self.state
                .saw_user_message
                .store(user_index.is_some(), Ordering::SeqCst);
            self.state
                .saw_custom_message
                .store(custom_index.is_some(), Ordering::SeqCst);
            self.state.saw_user_before_custom.store(
                user_index
                    .zip(custom_index)
                    .is_some_and(|(user, custom)| user < custom),
                Ordering::SeqCst,
            );
            self.state.saw_expected_system_prompt.store(
                context
                    .system_prompt
                    .as_deref()
                    .is_some_and(|prompt| prompt == self.expected_system_prompt),
                Ordering::SeqCst,
            );

            let message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("done"))],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            })])))
        }
    }

    fn wait_for_agent_done(event_rx: &mut mpsc::Receiver<PiMsg>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(PiMsg::AgentDone { error_message, .. }) => {
                    assert!(error_message.is_none(), "turn error: {error_message:?}");
                    return;
                }
                Ok(PiMsg::AgentError(err)) => panic!("interactive turn failed: {err}"),
                Ok(_) | Err(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
        panic!("interactive turn did not finish before the deadline");
    }

    #[derive(Default)]
    struct ContinueProbeState {
        calls: AtomicUsize,
        saw_custom_message: AtomicBool,
        saw_user_message: AtomicBool,
    }

    struct ContinueProbeProvider {
        state: Arc<ContinueProbeState>,
    }

    struct InteractiveRepairProvider {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for InteractiveRepairProvider {
        fn name(&self) -> &'static str {
            "repair-probe"
        }

        fn api(&self) -> &'static str {
            "openai-completions"
        }

        fn model_id(&self) -> &'static str {
            "qwen3-repair-probe"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let content = if call == 0 {
                vec![ContentBlock::Text(TextContent::new(
                    "Checking. <tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"fixture.txt\"}}</tool_call>",
                ))]
            } else {
                vec![ContentBlock::Text(TextContent::new("done"))]
            };
            let message = AssistantMessage {
                content,
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            };
            Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            })])))
        }
    }

    impl ContinueProbeProvider {
        fn assistant_message(&self, content: &str) -> AssistantMessage {
            AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(content))],
                api: self.api().to_string(),
                provider: self.name().to_string(),
                model: self.model_id().to_string(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for ContinueProbeProvider {
        fn name(&self) -> &'static str {
            "continue-probe"
        }

        fn api(&self) -> &'static str {
            "continue-probe"
        }

        fn model_id(&self) -> &'static str {
            "continue-probe-model"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.state.calls.fetch_add(1, Ordering::SeqCst);
            self.state.saw_custom_message.store(
                context.messages.iter().any(|message| {
                    matches!(
                        message,
                        ModelMessage::Custom(CustomMessage { custom_type, content, .. })
                            if custom_type == "note" && content == "continue-now"
                    )
                }),
                Ordering::SeqCst,
            );
            self.state.saw_user_message.store(
                context
                    .messages
                    .iter()
                    .any(|message| matches!(message, ModelMessage::User(_))),
                Ordering::SeqCst,
            );

            let partial = self.assistant_message("");
            let message = self.assistant_message("continued");
            Ok(Box::pin(stream::iter(vec![
                Ok(StreamEvent::Start { partial }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                }),
            ])))
        }
    }

    #[test]
    fn coalesces_adjacent_deltas_of_same_kind() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut batcher = UiStreamDeltaBatcher::new(tx);
        batcher.flush_interval = std::time::Duration::from_secs(60);
        batcher.last_flush = std::time::Instant::now();

        batcher.push_delta(StreamDeltaKind::Text, "Hel");
        batcher.push_delta(StreamDeltaKind::Text, "lo");
        assert!(rx.try_recv().is_err());

        batcher.flush(true);
        let msg = rx.try_recv().expect("expected coalesced text delta");
        assert!(matches!(msg, PiMsg::TextDelta(text) if text == "Hello"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn send_immediate_flushes_pending_before_tool_event() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut batcher = UiStreamDeltaBatcher::new(tx);
        batcher.flush_interval = std::time::Duration::from_secs(60);
        batcher.last_flush = std::time::Instant::now();

        batcher.push_delta(StreamDeltaKind::Text, "partial");
        batcher.send_immediate(PiMsg::ToolStart {
            name: "bash".to_string(),
            tool_id: "t1".to_string(),
        });

        let first = rx.try_recv().expect("expected flushed text delta first");
        let second = rx.try_recv().expect("expected immediate tool start second");
        assert!(matches!(first, PiMsg::TextDelta(text) if text == "partial"));
        assert!(
            matches!(second, PiMsg::ToolStart { name, tool_id } if name == "bash" && tool_id == "t1")
        );
    }

    #[test]
    fn normal_tool_updates_flush_immediately() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut batcher = UiStreamDeltaBatcher::new(tx);

        batcher.send_immediate(text_tool_update("first"));

        let msg = rx.try_recv().expect("expected immediate tool update");
        assert!(matches!(
            msg,
            PiMsg::ToolUpdate { content, .. }
                if matches!(content.first(), Some(ContentBlock::Text(text)) if text.text == "first")
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn pressure_coalesces_tool_updates_until_control_flush() {
        let (tx, mut rx) = mpsc::channel(8);
        let frame_p99 = Arc::new(AtomicU64::new(TuiPressureController::HIGH_FRAME_P99_US));
        let mut batcher = UiStreamDeltaBatcher::new_with_frame_p99(tx, frame_p99);
        batcher.last_tool_update_flush = std::time::Instant::now();

        batcher.send_immediate(text_tool_update("first"));
        batcher.send_immediate(text_tool_update("second"));
        assert!(rx.try_recv().is_err());

        batcher.send_immediate(PiMsg::ToolEnd {
            name: "bash".to_string(),
            tool_id: "t1".to_string(),
            is_error: false,
            output: None,
        });

        let first = rx.try_recv().expect("expected coalesced latest update");
        let second = rx.try_recv().expect("expected tool end after update");
        assert!(matches!(
            first,
            PiMsg::ToolUpdate { content, .. }
                if matches!(content.first(), Some(ContentBlock::Text(text)) if text.text == "second")
        ));
        assert!(matches!(second, PiMsg::ToolEnd { tool_id, .. } if tool_id == "t1"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn pressure_flushes_tool_update_when_pending_event_cap_is_hit() {
        let (tx, mut rx) = mpsc::channel(8);
        let frame_p99 = Arc::new(AtomicU64::new(TuiPressureController::HIGH_FRAME_P99_US));
        let mut batcher = UiStreamDeltaBatcher::new_with_frame_p99(tx, frame_p99);
        batcher.last_tool_update_flush = std::time::Instant::now();

        for idx in 0..TuiPressureController::HIGH_PENDING_TOOL_EVENTS {
            batcher.send_immediate(text_tool_update(&format!("chunk-{idx}")));
        }

        let msg = rx
            .try_recv()
            .expect("expected latest update after pending cap");
        assert!(matches!(
            msg,
            PiMsg::ToolUpdate { content, .. }
                if matches!(
                    content.first(),
                    Some(ContentBlock::Text(text))
                        if text.text
                            == format!(
                                "chunk-{}",
                                TuiPressureController::HIGH_PENDING_TOOL_EVENTS - 1
                            )
                )
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn pressure_flushes_tool_update_when_pending_byte_cap_is_hit() {
        let (tx, mut rx) = mpsc::channel(8);
        let frame_p99 = Arc::new(AtomicU64::new(TuiPressureController::HIGH_FRAME_P99_US));
        let mut batcher = UiStreamDeltaBatcher::new_with_frame_p99(tx, frame_p99);
        batcher.last_tool_update_flush = std::time::Instant::now();

        let chunks = ["a", "b", "c"]
            .map(|prefix| prefix.repeat(TuiPressureController::HIGH_TOOL_OUTPUT_BYTES));
        let expected_latest = "d".repeat(TuiPressureController::HIGH_TOOL_OUTPUT_BYTES);
        for chunk in &chunks {
            batcher.send_immediate(text_tool_update(chunk));
        }
        batcher.send_immediate(text_tool_update(&expected_latest));

        let msg = rx
            .try_recv()
            .expect("expected latest update after pending byte cap");
        assert!(matches!(
            msg,
            PiMsg::ToolUpdate { content, .. }
                if matches!(content.first(), Some(ContentBlock::Text(text)) if text.text == expected_latest)
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn retains_unsent_chunk_when_channel_is_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut batcher = UiStreamDeltaBatcher::new(tx);
        batcher.flush_interval = std::time::Duration::from_secs(60);
        batcher.last_flush = std::time::Instant::now();

        batcher.send_immediate(PiMsg::System("occupy".to_string()));
        batcher.push_delta(StreamDeltaKind::Text, "later");
        batcher.flush(true);
        assert_eq!(batcher.pending_bytes, "later".len());

        let _ = rx.try_recv().expect("expected occupied slot message");
        batcher.flush(true);

        let msg = rx.try_recv().expect("expected retained text delta");
        assert!(matches!(msg, PiMsg::TextDelta(text) if text == "later"));
        assert_eq!(batcher.pending_bytes, 0);
    }

    #[test]
    fn retains_immediate_events_when_channel_is_full() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut batcher = UiStreamDeltaBatcher::new(tx);
        batcher.flush_interval = std::time::Duration::from_secs(60);
        batcher.last_flush = std::time::Instant::now();

        // Occupy the single slot.
        batcher.send_immediate(PiMsg::System("occupy".to_string()));

        // Queue a delta and a control event while the channel is full.
        batcher.push_delta(StreamDeltaKind::Text, "before-done");
        batcher.send_immediate(PiMsg::AgentDone {
            usage: None,
            stop_reason: StopReason::Stop,
            error_message: None,
        });

        // Nothing should be dropped; queue should still hold both messages.
        assert_eq!(batcher.pending_bytes, "before-done".len());
        assert_eq!(batcher.pending.len(), 2);

        // Free slot and flush repeatedly; ordering must be preserved.
        let _ = rx.try_recv().expect("expected occupied slot message");
        batcher.flush(true);
        let first = rx.try_recv().expect("expected retained text delta");
        assert!(matches!(first, PiMsg::TextDelta(text) if text == "before-done"));

        batcher.flush(true);
        let second = rx.try_recv().expect("expected retained agent_done event");
        assert!(matches!(second, PiMsg::AgentDone { .. }));
    }

    #[test]
    fn session_bound_events_reject_stale_input_and_display_notes() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();
        app.agent_state = AgentState::Processing;
        let original_messages = app.messages.len();

        let _ = app.handle_pi_message(PiMsg::EnqueuePendingInput {
            session_id: "replaced-session".to_string(),
            input: PendingInput::GeneratedText("stale input".to_string()),
        });
        let _ = app.handle_pi_message(PiMsg::SessionSystemNote {
            owner_session_id: "replaced-session".to_string(),
            message: "stale tan card".to_string(),
        });
        assert!(
            app.pending_inputs.is_empty(),
            "an in-transit input from the old session must not enter the new queue"
        );
        assert_eq!(
            app.messages.len(),
            original_messages,
            "an old session's display card must not enter the new transcript"
        );

        let _ = app.handle_pi_message(PiMsg::EnqueuePendingInput {
            session_id: session_id.clone(),
            input: PendingInput::GeneratedText("current input".to_string()),
        });
        assert!(
            app.pending_inputs.is_empty(),
            "session-bound input must also fail closed while a transition owns the UI"
        );
        app.agent_state = AgentState::Idle;
        // Deliver the current session's card while no turn is running: an
        // accepted input starts its turn on a spawned task that takes the
        // Session lock, and `session_event_ownership` would then defer the
        // card through the busy-retry path instead of appending it.
        let _ = app.handle_pi_message(PiMsg::SessionSystemNote {
            owner_session_id: session_id.clone(),
            message: "current tan card".to_string(),
        });
        assert!(matches!(
            app.messages.last(),
            Some(ConversationMessage { role: MessageRole::System, content, .. })
                if content == "current tan card"
        ));
        let _ = app.handle_pi_message(PiMsg::EnqueuePendingInput {
            session_id,
            input: PendingInput::GeneratedText("current input".to_string()),
        });
        // Submissions run the turn on a spawned task and return no `Cmd`
        // (`submit_content_with_display_and_keyword_source` ends with `None`),
        // so the observable effect of an accepted input is the state change.
        assert!(
            matches!(app.agent_state, AgentState::Processing),
            "current idle-session input must still run"
        );
    }

    #[test]
    fn open_tree_event_is_bound_to_its_originating_session() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let current_session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();

        let _ = app.handle_pi_message(PiMsg::OpenTree {
            owner_session_id: "replaced-session".to_string(),
            initial_selected_id: None,
            label: Some("stale tree".to_string()),
        });
        assert!(
            app.tree_ui.is_none(),
            "an old session's async tree request must not open in its replacement"
        );

        let _ = app.handle_pi_message(PiMsg::OpenTree {
            owner_session_id: current_session_id,
            initial_selected_id: None,
            label: Some("current tree".to_string()),
        });
        assert!(
            app.tree_ui.is_some(),
            "the current idle session's tree request must still open"
        );
    }

    #[test]
    fn set_editor_text_event_is_bound_to_its_originating_session() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let current_session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();
        app.input.set_value("current draft");

        let _ = app.handle_pi_message(PiMsg::SetEditorText {
            owner_session_id: "replaced-session".to_string(),
            text: "stale branch prompt".to_string(),
        });
        assert_eq!(
            app.input.value(),
            "current draft",
            "an old session's branch prompt must not overwrite the replacement editor"
        );

        let _ = app.handle_pi_message(PiMsg::SetEditorText {
            owner_session_id: current_session_id,
            text: "current branch prompt".to_string(),
        });
        assert_eq!(app.input.value(), "current branch prompt");
    }

    #[test]
    fn continue_pending_input_runs_agent_without_new_user_message() {
        let state = Arc::new(ContinueProbeState::default());
        let provider: Arc<dyn Provider> = Arc::new(ContinueProbeProvider {
            state: Arc::clone(&state),
        });
        let (mut app, mut event_rx) = build_test_app_with_provider(provider);

        runtime().block_on(async {
            let cx = Cx::for_request();
            let mut guard = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.agent), &cx)
                .await
                .expect("lock agent");
            guard.add_message(ModelMessage::Custom(CustomMessage {
                content: "continue-now".to_string(),
                custom_type: "note".to_string(),
                display: true,
                details: None,
                timestamp: 0,
            }));
        });

        let session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();
        let _ = app.handle_pi_message(PiMsg::EnqueuePendingInput {
            session_id,
            input: PendingInput::Continue,
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut saw_done = false;
        while std::time::Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(PiMsg::AgentDone { error_message, .. }) => {
                    saw_done = true;
                    if let Some(err) = error_message {
                        println!("AgentDone error: {}", err);
                    }
                }
                Ok(PiMsg::AgentError(err)) => {
                    println!("AgentError: {}", err);
                }
                Ok(_) => {}
                Err(_) => {}
            }

            if saw_done && state.calls.load(Ordering::SeqCst) == 1 {
                break;
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        if state.calls.load(Ordering::SeqCst) == 0 {
            println!("Status message: {:?}", app.status_message);
        }

        assert!(saw_done, "submit_message path should finish an agent turn");
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        assert!(
            state.saw_custom_message.load(Ordering::SeqCst),
            "continue path should reuse the injected custom message as provider context"
        );
        assert!(
            !state.saw_user_message.load(Ordering::SeqCst),
            "continue path should not synthesize a user message"
        );
    }

    #[test]
    fn ordinary_prompt_applies_before_agent_start_messages_and_system_prompt_once() {
        let state = Arc::new(BeforeAgentStartProbeState::default());
        let provider: Arc<dyn Provider> = Arc::new(BeforeAgentStartProbeProvider {
            state: Arc::clone(&state),
            expected_system_prompt: "hook-system",
        });
        let (mut app, mut event_rx) = build_test_app_with_provider(provider);
        runtime().block_on(async {
            let cx = Cx::for_request();
            let mut guard = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.agent), &cx)
                .await
                .expect("lock agent");
            guard.set_system_prompt(Some("base-system".to_string()));
        });
        app.extensions = Some(build_test_extension_manager_with_before_agent_start_output(
            &json!({
                "systemPrompt": "hook-system",
                "messages": [{
                    "customType": "hook-note",
                    "content": "injected",
                    "display": false
                }]
            }),
        ));

        let _ = app.submit_message("hello");
        wait_for_agent_done(&mut event_rx);

        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        assert!(state.saw_user_message.load(Ordering::SeqCst));
        assert!(state.saw_custom_message.load(Ordering::SeqCst));
        assert!(state.saw_user_before_custom.load(Ordering::SeqCst));
        assert!(state.saw_expected_system_prompt.load(Ordering::SeqCst));
        runtime().block_on(async {
            let cx = Cx::for_request();
            let guard = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.agent), &cx)
                .await
                .expect("lock agent");
            assert_eq!(guard.system_prompt(), Some("base-system"));
        });
    }

    #[test]
    fn ordinary_prompt_keeps_base_context_when_before_agent_start_has_no_mutation() {
        let state = Arc::new(BeforeAgentStartProbeState::default());
        let provider: Arc<dyn Provider> = Arc::new(BeforeAgentStartProbeProvider {
            state: Arc::clone(&state),
            expected_system_prompt: "base-system",
        });
        let (mut app, mut event_rx) = build_test_app_with_provider(provider);
        runtime().block_on(async {
            let cx = Cx::for_request();
            let mut guard = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.agent), &cx)
                .await
                .expect("lock agent");
            guard.set_system_prompt(Some("base-system".to_string()));
        });
        app.extensions = Some(build_test_extension_manager_with_before_agent_start_output(
            &json!({}),
        ));

        let _ = app.submit_message("hello");
        wait_for_agent_done(&mut event_rx);

        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        assert!(state.saw_user_message.load(Ordering::SeqCst));
        assert!(!state.saw_custom_message.load(Ordering::SeqCst));
        assert!(!state.saw_user_before_custom.load(Ordering::SeqCst));
        assert!(state.saw_expected_system_prompt.load(Ordering::SeqCst));
    }

    #[test]
    fn submit_message_preserves_raw_extension_command_args() {
        let raw_args = r#"--message "hello world"   --force"#;
        let (mut app, mut event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app.extensions = Some(build_test_extension_manager_with_command_output(&json!(
            raw_args
        )));

        let _ = app.submit_message(r#"/deploy   --message "hello world"   --force"#);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut completion = None;
        let mut agent_error = None;
        while std::time::Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(PiMsg::ExtensionCommandDone {
                    display, is_error, ..
                }) => {
                    assert!(!is_error, "unexpected extension command error: {display}");
                    completion = Some(display);
                    break;
                }
                Ok(PiMsg::AgentError(err)) => {
                    agent_error = Some(err);
                    break;
                }
                Ok(_) | Err(_) => {}
            }

            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            agent_error.is_none(),
            "unexpected agent error while running extension command: {}",
            agent_error.unwrap_or_default()
        );
        assert_eq!(
            completion.as_deref(),
            Some(raw_args),
            "timed out waiting for extension command completion"
        );
    }

    #[test]
    fn spawn_save_session_inherits_cancelled_context_when_session_lock_is_held() {
        let (app, mut event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));

        runtime().block_on(async {
            let hold_cx = Cx::for_request();
            let _held_guard =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &hold_cx)
                    .await
                    .expect("lock session");

            let ambient_cx = Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = Cx::set_current(Some(ambient_cx));

            app.spawn_save_session();

            let recv_cx = Cx::for_testing();
            let wait_for_error = async {
                loop {
                    match event_rx.recv(&recv_cx).await {
                        Ok(PiMsg::AgentError(message))
                            if message.contains("Failed to lock session") =>
                        {
                            break message;
                        }
                        Ok(_) => {}
                        Err(err) => break format!("event receive failed: {err}"),
                    }
                }
            };
            futures::pin_mut!(wait_for_error);
            let err = asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(1),
                wait_for_error,
            )
            .await
            .expect("cancelled save task should finish before timeout");

            assert!(
                err.contains("Failed to lock session"),
                "unexpected save-task error: {err}"
            );
        });
    }

    #[test]
    fn submit_continue_inherits_cancelled_context_when_agent_lock_is_attempted() {
        let (mut app, mut event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));

        runtime().block_on(async {
            let ambient_cx = Cx::for_testing();
            ambient_cx.set_cancel_requested(true);
            let _current = Cx::set_current(Some(ambient_cx));

            let _ = app.submit_continue();

            let recv_cx = Cx::for_testing();
            let wait_for_terminal = async {
                loop {
                    match event_rx.recv(&recv_cx).await {
                        Ok(PiMsg::AgentError(message)) => break format!("error:{message}"),
                        Ok(PiMsg::AgentDone { error_message, .. }) => {
                            break format!("done:{}", error_message.unwrap_or_default());
                        }
                        Ok(_) => {}
                        Err(err) => break format!("receive-error:{err}"),
                    }
                }
            };
            futures::pin_mut!(wait_for_terminal);
            let outcome = asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(1),
                wait_for_terminal,
            )
            .await
            .expect("cancelled continue task should reach provider before timeout");

            assert!(
                outcome.contains("Failed to lock agent"),
                "unexpected continue-task outcome: {outcome}"
            );
        });
    }

    #[test]
    fn submit_continue_inherits_deadline_into_spawned_task() {
        struct ProbeReset;
        impl Drop for ProbeReset {
            fn drop(&mut self) {
                let mut probe = submit_continue_deadline_probe()
                    .lock()
                    .expect("lock submit_continue deadline probe");
                *probe = None;
            }
        }

        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));

        let (probe_tx, probe_rx) = std::sync::mpsc::channel();
        {
            let mut probe = submit_continue_deadline_probe()
                .lock()
                .expect("lock submit_continue deadline probe");
            assert!(
                probe.is_none(),
                "submit_continue deadline probe already installed"
            );
            *probe = Some(probe_tx);
        }
        let _probe_reset = ProbeReset;

        runtime().block_on(async {
            let cx = Cx::for_request();
            let mut guard = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.agent), &cx)
                .await
                .expect("lock agent");
            guard.add_message(ModelMessage::Custom(CustomMessage {
                content: "continue-now".to_string(),
                custom_type: "note".to_string(),
                display: true,
                details: None,
                timestamp: 0,
            }));
        });

        let expected_deadline = asupersync::time::wall_now() + std::time::Duration::from_secs(30);
        let ambient_cx = Cx::for_testing_with_budget(
            asupersync::Budget::INFINITE.with_deadline(expected_deadline),
        );
        let _current = Cx::set_current(Some(ambient_cx));

        let session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();
        let _ = app.handle_pi_message(PiMsg::EnqueuePendingInput {
            session_id,
            input: PendingInput::Continue,
        });

        let recorded = loop {
            let res = probe_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("submit_continue deadline probe");
            if res == Some(expected_deadline) {
                break res;
            }
        };
        assert_eq!(recorded, Some(expected_deadline));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn conversation_reset_clears_session_scoped_ui_state() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app.title_requested = true;
        app.todo_summary = Some("1 todo pending".to_string());
        app.extension_custom_active = true;
        app.extension_custom_key_queue
            .push_back("old-key".to_string());
        app.extension_custom_overlay = Some(ExtensionCustomOverlay::default());
        app.role_model_overrides.insert(
            crate::models::ModelRole::Smol,
            ("fixture-provider".to_string(), "fixture-model".to_string()),
        );
        let _ = app.handle_pi_message(PiMsg::OAuthDeviceFlowStarted {
            provider: "fixture-provider".to_string(),
            device_code: "device-code".to_string(),
            user_code: "user-code".to_string(),
            verification_uri: "https://example.test/device".to_string(),
            expires_in: 300,
        });
        assert!(app.pending_oauth.is_some());
        let current_session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();
        let _ = app.handle_pi_message(PiMsg::OpenTree {
            owner_session_id: current_session_id,
            initial_selected_id: None,
            label: Some("old session tree".to_string()),
        });
        assert!(app.tree_ui.is_some());

        let ask_tool = crate::ask::AskTool::new(crate::ask::AskPolicy::Recommended);
        let (ask_tx, mut ask_rx) = asupersync::channel::mpsc::channel(1);
        ask_tool.install_channel_ui(ask_tx);
        let mut ask_execution = Box::pin(ask_tool.execute(
            "old-session-ask-call",
            serde_json::json!({
                // validate_request requires MIN_OPTIONS (2) choices per question.
                "questions": [{"question": "Old prompt?", "options": [{"label": "Yes"}, {"label": "No"}]}]
            }),
            None,
        ));
        let ask_request = runtime().block_on(async {
            assert!(futures::poll!(ask_execution.as_mut()).is_pending());
            let cx = Cx::for_request();
            ask_rx.recv(&cx).await.expect("ask request reaches UI")
        });
        app.ask_tool = Some(ask_tool.clone());
        app.input.set_value("old session draft");
        let _ = app.handle_pi_message(PiMsg::AskUiRequest(ask_request));
        assert!(
            app.active_ask_ui.is_some(),
            "fixture must activate a real Ask waiter"
        );
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(ExtensionUiRequest::new(
            "old-session-extension",
            "confirm",
            serde_json::json!({"title": "Old extension prompt"}),
        )));
        app.input.set_value("partial old-session answer");

        let replacement_session_id = "replacement-session".to_string();
        app.session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone_from(&replacement_session_id);

        let _ = app.handle_pi_message(PiMsg::ConversationReset {
            session_id: replacement_session_id,
            messages: Vec::new(),
            usage: Usage::default(),
            status: Some("Session replaced".to_string()),
        });

        assert!(
            !app.title_requested,
            "the replacement session must be eligible for auto-title"
        );
        assert!(
            app.todo_summary.is_none(),
            "the previous session's todo footer must not leak"
        );
        assert!(
            app.pending_oauth.is_none(),
            "the previous session's OAuth continuation must not leak"
        );
        assert!(
            app.role_model_overrides.is_empty(),
            "the previous session's role model overrides must not leak"
        );
        assert!(
            app.tree_ui.is_none(),
            "the previous session's tree must close"
        );
        assert!(!app.extension_custom_active);
        assert!(app.extension_custom_key_queue.is_empty());
        assert!(app.extension_custom_overlay.is_none());
        assert!(app.active_ask_ui.is_none());
        assert!(app.ask_ui_queue.is_empty());
        assert!(app.active_extension_ui.is_none());
        assert!(app.extension_ui_queue.is_empty());
        assert!(app.input_card_order.is_empty());
        assert!(app.card_draft_snapshot.is_none());
        assert!(
            app.input.value().is_empty(),
            "neither the old draft nor a partial old-card answer may enter the replacement"
        );
        let ask_error = runtime()
            .block_on(ask_execution)
            .expect_err("replacement reset must dismiss the old Ask waiter");
        assert!(ask_error.to_string().contains("dismissed"), "{ask_error}");
    }

    #[test]
    fn conversation_reset_rejects_stale_session_payloads() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let old_session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();
        let replacement_session_id = "replacement-session".to_string();
        app.session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone_from(&replacement_session_id);
        app.agent_state = AgentState::Processing;
        app.messages.push(ConversationMessage {
            role: MessageRole::System,
            content: "replacement UI state".to_string(),
            thinking: None,
            collapsed: false,
        });

        let _ = app.handle_pi_message(PiMsg::ConversationReset {
            session_id: old_session_id.clone(),
            messages: Vec::new(),
            usage: Usage::default(),
            status: Some("stale reset".to_string()),
        });
        assert_eq!(app.agent_state, AgentState::Processing);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(
            app.displayed_session_id.as_deref(),
            Some(old_session_id.as_str())
        );

        let _ = app.handle_pi_message(PiMsg::ConversationReset {
            session_id: replacement_session_id.clone(),
            messages: Vec::new(),
            usage: Usage::default(),
            status: Some("current reset".to_string()),
        });
        assert_eq!(app.agent_state, AgentState::Idle);
        assert!(app.messages.is_empty());
        assert_eq!(
            app.displayed_session_id.as_deref(),
            Some(replacement_session_id.as_str())
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn contended_current_session_reset_is_retried_then_applied() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();
        app.agent_state = AgentState::Processing;
        app.messages.push(ConversationMessage {
            role: MessageRole::System,
            content: "pre-reset state".to_string(),
            thinking: None,
            collapsed: false,
        });

        let session = Arc::clone(&app.session);
        let session_guard = session.try_lock().expect("hold session lock");
        let retry = app
            .handle_pi_message(PiMsg::ConversationReset {
                session_id: session_id.clone(),
                messages: Vec::new(),
                usage: Usage::default(),
                status: Some("Conversation compacted".to_string()),
            })
            .expect("a current-session event must be retried when only the lock is busy");
        let retry_message = retry
            .execute()
            .expect("session retry command must emit a message")
            .downcast::<PiMsg>()
            .expect("session retry command must emit PiMsg");
        assert!(
            matches!(
                &retry_message,
                PiMsg::SessionEventRetry {
                    attempts_remaining,
                    ..
                } if *attempts_remaining == SESSION_EVENT_LOCK_RETRY_ATTEMPTS - 1
            ),
            "a current-session event must be retried when only the lock is busy"
        );
        assert_eq!(app.agent_state, AgentState::Processing);
        assert_eq!(app.messages.len(), 1);
        drop(session_guard);

        let completed = app.handle_pi_message(retry_message);
        assert!(completed.is_none());
        assert_eq!(app.agent_state, AgentState::Idle);
        assert!(app.messages.is_empty());
        assert_eq!(
            app.status_message.as_deref(),
            Some("Conversation compacted")
        );

        let stale_session_id = format!("stale-{session_id}");
        app.agent_state = AgentState::Processing;
        app.messages.push(ConversationMessage {
            role: MessageRole::Assistant,
            content: "session B persisted transcript".to_string(),
            thinking: None,
            collapsed: false,
        });
        app.current_response.push_str("session B partial response");
        app.current_thinking.push_str("session B partial thinking");
        app.current_tool = Some("session-b-tool".to_string());
        app.current_tool_id = Some("session-b-tool-id".to_string());
        app.current_tool_summary.insert(
            "session-b-tool-id".to_string(),
            "session B invocation".to_string(),
        );
        let (abort_handle, _abort_signal) = AbortHandle::new();
        app.abort_handle = Some(abort_handle);
        app.total_usage.input = 41;
        app.todo_summary = Some("session B todo".to_string());
        app.title_requested = true;
        app.extension_ui_queue.push_back(ExtensionUiRequest::new(
            "session-b-card",
            "confirm",
            serde_json::json!({"title": "session B prompt"}),
        ));
        app.extension_streaming.store(true, Ordering::SeqCst);
        app.extension_compacting.store(true, Ordering::SeqCst);
        let session = Arc::clone(&app.session);
        let session_guard = session.try_lock().expect("hold session lock");
        let exhausted = app.handle_pi_message(PiMsg::SessionEventRetry {
            event: Box::new(PiMsg::ConversationReset {
                session_id: stale_session_id,
                messages: Vec::new(),
                usage: Usage::default(),
                status: Some("stale session A reset".to_string()),
            }),
            attempts_remaining: 0,
        });
        assert!(exhausted.is_none());
        assert_eq!(app.agent_state, AgentState::Processing);
        assert_eq!(app.messages.len(), 1);
        assert_eq!(app.messages[0].content, "session B persisted transcript");
        assert_eq!(
            app.displayed_session_id.as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(app.current_response, "session B partial response");
        assert_eq!(app.current_thinking, "session B partial thinking");
        assert_eq!(app.current_tool.as_deref(), Some("session-b-tool"));
        assert_eq!(app.current_tool_id.as_deref(), Some("session-b-tool-id"));
        assert_eq!(
            app.current_tool_summary
                .get("session-b-tool-id")
                .map(String::as_str),
            Some("session B invocation")
        );
        assert!(app.abort_handle.is_some());
        assert_eq!(app.total_usage.input, 41);
        assert_eq!(app.todo_summary.as_deref(), Some("session B todo"));
        assert!(app.title_requested);
        assert_eq!(app.extension_ui_queue.len(), 1);
        assert!(app.extension_streaming.load(Ordering::SeqCst));
        assert!(app.extension_compacting.load(Ordering::SeqCst));
        assert_eq!(
            app.status_message.as_deref(),
            Some("Session remained busy; a delayed session update was not applied")
        );
        drop(session_guard);
    }

    #[test]
    fn same_session_conversation_reset_preserves_session_scoped_ui_state() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app.title_requested = true;
        app.todo_summary = Some("1 todo pending".to_string());
        app.role_model_overrides.insert(
            crate::models::ModelRole::Smol,
            ("fixture-provider".to_string(), "fixture-model".to_string()),
        );
        let _ = app.handle_pi_message(PiMsg::OAuthDeviceFlowStarted {
            provider: "fixture-provider".to_string(),
            device_code: "device-code".to_string(),
            user_code: "user-code".to_string(),
            verification_uri: "https://example.test/device".to_string(),
            expires_in: 300,
        });
        let session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();

        let _ = app.handle_pi_message(PiMsg::ConversationReset {
            session_id,
            messages: Vec::new(),
            usage: Usage::default(),
            status: Some("Conversation compacted".to_string()),
        });

        assert!(app.title_requested);
        assert_eq!(app.todo_summary.as_deref(), Some("1 todo pending"));
        assert!(app.pending_oauth.is_some());
        assert_eq!(app.role_model_overrides.len(), 1);
    }

    #[test]
    fn session_title_suggestion_is_bound_to_its_originating_session() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let current_session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();

        let _ = app.handle_pi_message(PiMsg::SessionTitleSuggestion {
            owner_session_id: "replaced-session".to_string(),
            title: "stale title".to_string(),
        });
        assert!(
            app.session
                .try_lock()
                .expect("lock session")
                .get_name()
                .is_none(),
            "a title generated for an old session must not rename its replacement"
        );

        let _ = app.handle_pi_message(PiMsg::SessionTitleSuggestion {
            owner_session_id: current_session_id,
            title: "current title".to_string(),
        });
        assert_eq!(
            app.session
                .try_lock()
                .expect("lock session")
                .get_name()
                .as_deref(),
            Some("current title")
        );
    }

    #[test]
    fn conversation_reset_syncs_runtime_model_and_thinking_from_session_header() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let mut next = model_entry("openai", "gpt-4o");
        next.model.reasoning = false;
        app.available_models.push(next.clone());

        let session_id = runtime().block_on(async {
            let cx = Cx::for_request();
            let mut session_guard =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &cx)
                    .await
                    .expect("lock session");
            session_guard.header.provider = Some(next.model.provider.clone());
            session_guard.header.model_id = Some(next.model.id.clone());
            session_guard.header.thinking_level = Some("high".to_string());
            session_guard.header.id.clone()
        });

        let _ = app.handle_pi_message(PiMsg::ConversationReset {
            session_id,
            messages: Vec::new(),
            usage: Usage::default(),
            status: Some("Session resumed".to_string()),
        });

        assert_eq!(app.model, "openai/gpt-4o");
        assert_eq!(app.model_entry.model.provider, "openai");
        assert_eq!(app.model_entry.model.id, "gpt-4o");
        assert_eq!(app.status_message.as_deref(), Some("Session resumed"));

        let shared = app
            .model_entry_shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(shared.model.provider, "openai");
        assert_eq!(shared.model.id, "gpt-4o");
        drop(shared);

        let agent_guard = app.agent.try_lock().expect("lock agent");
        assert_eq!(agent_guard.provider().name(), "openai");
        assert_eq!(agent_guard.provider().model_id(), "gpt-4o");
        assert_eq!(
            agent_guard.stream_options().thinking_level,
            Some(crate::model::ThinkingLevel::Off)
        );
    }

    #[test]
    fn fast_tree_navigation_syncs_runtime_model_and_thinking_from_target_branch() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let mut next = model_entry("openai", "gpt-4o");
        next.model.reasoning = false;
        app.available_models.push(next.clone());

        let (session_id, current_leaf_id, target_leaf_id) = runtime().block_on(async {
            let cx = Cx::for_request();
            let mut session_guard =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &cx)
                    .await
                    .expect("lock session");
            let root_id = session_guard.append_message(crate::session::SessionMessage::User {
                content: crate::model::UserContent::Text("root".to_string()),
                timestamp: Some(0),
            });
            let current_leaf_id =
                session_guard.append_message(crate::session::SessionMessage::User {
                    content: crate::model::UserContent::Text("current".to_string()),
                    timestamp: Some(0),
                });
            assert!(session_guard.create_branch_from(&root_id));
            session_guard.append_model_change(next.model.provider.clone(), next.model.id.clone());
            session_guard.append_thinking_level_change("high".to_string());
            let target_leaf_id =
                session_guard.append_message(crate::session::SessionMessage::User {
                    content: crate::model::UserContent::Text("target".to_string()),
                    timestamp: Some(0),
                });
            assert!(session_guard.navigate_to(&current_leaf_id));
            (
                session_guard.header.id.clone(),
                Some(current_leaf_id),
                Some(target_leaf_id),
            )
        });

        app.save_enabled = false;
        let switched = app.start_tree_navigation(
            super::super::tree::PendingTreeNavigation {
                session_id,
                old_leaf_id: current_leaf_id,
                new_leaf_id: target_leaf_id,
                editor_text: None,
                entries_to_summarize: Vec::new(),
                summary_from_id: String::new(),
                api_key_present: false,
            },
            super::super::tree::TreeSummaryChoice::NoSummary,
            None,
        );

        assert!(switched, "fast tree navigation should succeed");
        assert_eq!(app.model, "openai/gpt-4o");
        assert_eq!(app.model_entry.model.provider, "openai");
        assert_eq!(app.model_entry.model.id, "gpt-4o");
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|msg| msg.starts_with("Switched to ")),
            "status should still report the branch switch"
        );

        let shared = app
            .model_entry_shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(shared.model.provider, "openai");
        assert_eq!(shared.model.id, "gpt-4o");
        drop(shared);

        let agent_guard = app.agent.try_lock().expect("lock agent");
        assert_eq!(agent_guard.provider().name(), "openai");
        assert_eq!(agent_guard.provider().model_id(), "gpt-4o");
        assert_eq!(
            agent_guard.stream_options().thinking_level,
            Some(crate::model::ThinkingLevel::Off)
        );
    }

    #[test]
    fn fast_tree_navigation_is_atomic_when_agent_is_busy() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let (session_id, current_leaf_id, target_leaf_id, session_before) =
            runtime().block_on(async {
                let cx = Cx::for_request();
                let mut session_guard =
                    asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &cx)
                        .await
                        .expect("lock session");
                let root_id = session_guard.append_message(crate::session::SessionMessage::User {
                    content: crate::model::UserContent::Text("root".to_string()),
                    timestamp: Some(0),
                });
                let current_leaf_id =
                    session_guard.append_message(crate::session::SessionMessage::User {
                        content: crate::model::UserContent::Text("current".to_string()),
                        timestamp: Some(0),
                    });
                assert!(session_guard.create_branch_from(&root_id));
                let target_leaf_id =
                    session_guard.append_message(crate::session::SessionMessage::User {
                        content: crate::model::UserContent::Text("target".to_string()),
                        timestamp: Some(0),
                    });
                assert!(session_guard.navigate_to(&current_leaf_id));
                (
                    session_guard.header.id.clone(),
                    Some(current_leaf_id),
                    Some(target_leaf_id),
                    serde_json::to_value(session_guard.to_messages_for_current_path())
                        .expect("serialize session history"),
                )
            });
        let ui_before = app
            .messages
            .iter()
            .map(|message| (message.role, message.content.clone()))
            .collect::<Vec<_>>();
        let agent = Arc::clone(&app.agent);
        let agent_guard = agent.try_lock().expect("lock agent");
        let agent_before =
            serde_json::to_value(agent_guard.messages()).expect("serialize agent history");

        app.save_enabled = false;
        let switched = app.start_tree_navigation(
            super::super::tree::PendingTreeNavigation {
                session_id,
                old_leaf_id: current_leaf_id.clone(),
                new_leaf_id: target_leaf_id,
                editor_text: None,
                entries_to_summarize: Vec::new(),
                summary_from_id: String::new(),
                api_key_present: false,
            },
            super::super::tree::TreeSummaryChoice::NoSummary,
            None,
        );

        assert!(
            !switched,
            "a busy Agent must reject the entire branch switch"
        );
        assert_eq!(app.status_message.as_deref(), Some("Agent busy; try again"));
        assert_eq!(
            serde_json::to_value(agent_guard.messages()).expect("serialize agent history"),
            agent_before
        );
        drop(agent_guard);
        let session_guard = app.session.try_lock().expect("lock session");
        assert_eq!(session_guard.leaf_id(), current_leaf_id.as_deref());
        assert_eq!(
            serde_json::to_value(session_guard.to_messages_for_current_path())
                .expect("serialize session history"),
            session_before
        );
        drop(session_guard);
        let ui_after = app
            .messages
            .iter()
            .map(|message| (message.role, message.content.clone()))
            .collect::<Vec<_>>();
        assert_eq!(ui_after, ui_before);
    }

    #[test]
    fn persisted_tree_navigation_save_failure_does_not_claim_success() {
        let (mut app, mut event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app.save_enabled = true;
        app.extensions = None;
        let temp = tempfile::TempDir::new().expect("tempdir");
        let blocked_path = temp.path().join("blocked.jsonl");
        std::fs::create_dir(&blocked_path).expect("create directory at session path");

        let (session_id, current_leaf_id, target_leaf_id, session_before) =
            runtime().block_on(async {
                let cx = Cx::for_request();
                let mut session_guard =
                    asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &cx)
                        .await
                        .expect("lock session");
                let root_id = session_guard.append_message(crate::session::SessionMessage::User {
                    content: crate::model::UserContent::Text("root".to_string()),
                    timestamp: Some(0),
                });
                let current_leaf_id =
                    session_guard.append_message(crate::session::SessionMessage::User {
                        content: crate::model::UserContent::Text("current".to_string()),
                        timestamp: Some(0),
                    });
                assert!(session_guard.create_branch_from(&root_id));
                let target_leaf_id =
                    session_guard.append_message(crate::session::SessionMessage::User {
                        content: crate::model::UserContent::Text("target".to_string()),
                        timestamp: Some(0),
                    });
                assert!(session_guard.navigate_to(&current_leaf_id));
                session_guard.path = Some(blocked_path);
                (
                    session_guard.header.id.clone(),
                    Some(current_leaf_id),
                    Some(target_leaf_id),
                    serde_json::to_value(session_guard.to_messages_for_current_path())
                        .expect("serialize session history"),
                )
            });

        let switched = app.start_tree_navigation(
            super::super::tree::PendingTreeNavigation {
                session_id,
                old_leaf_id: current_leaf_id.clone(),
                new_leaf_id: target_leaf_id,
                editor_text: None,
                entries_to_summarize: Vec::new(),
                summary_from_id: String::new(),
                api_key_present: false,
            },
            super::super::tree::TreeSummaryChoice::NoSummary,
            None,
        );
        assert!(
            switched,
            "persistence-enabled navigation should be admitted then fail closed"
        );

        let error = runtime().block_on(async {
            let recv_cx = Cx::for_testing();
            let wait_for_error = async {
                loop {
                    match event_rx.recv(&recv_cx).await {
                        Ok(PiMsg::AgentError(message))
                            if message.contains("could not be confirmed") =>
                        {
                            break message;
                        }
                        Ok(PiMsg::ConversationReset { .. }) => {
                            break "unexpected ConversationReset".to_string();
                        }
                        Ok(_) => {}
                        Err(err) => break format!("event receive failed: {err}"),
                    }
                }
            };
            futures::pin_mut!(wait_for_error);
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(5),
                wait_for_error,
            )
            .await
            .expect("save-failure tree navigation should finish before timeout")
        });
        assert!(
            error.contains("could not be confirmed"),
            "unexpected terminal event: {error}"
        );

        let session_guard = app.session.try_lock().expect("lock session");
        assert_eq!(session_guard.leaf_id(), current_leaf_id.as_deref());
        assert_eq!(
            serde_json::to_value(session_guard.to_messages_for_current_path())
                .expect("serialize session history"),
            session_before
        );
        drop(session_guard);
        assert!(
            app.status_message
                .as_deref()
                .is_none_or(|msg| !msg.starts_with("Switched to ")),
            "UI must not claim a durable switch after a failed save"
        );
    }

    #[test]
    fn shake_compact_save_failure_does_not_claim_success() {
        let (mut app, mut event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app.save_enabled = true;
        app.extensions = None;
        app.config.compaction = Some(crate::config::CompactionSettings {
            enabled: Some(true),
            reserve_tokens: Some(128_000),
            keep_recent_tokens: Some(1),
            mode: None,
        });
        let temp = tempfile::TempDir::new().expect("tempdir");
        let blocked_path = temp.path().join("blocked.jsonl");
        std::fs::create_dir(&blocked_path).expect("create directory at session path");

        let bulky = "history ".repeat(200);
        let expected_entries = runtime().block_on(async {
            let cx = Cx::for_request();
            let mut session_guard =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &cx)
                    .await
                    .expect("lock session");
            for i in 0..4 {
                session_guard.append_message(crate::session::SessionMessage::User {
                    content: crate::model::UserContent::Text(format!("user-{i} {bulky}")),
                    timestamp: Some(i),
                });
                session_guard.append_message(crate::session::SessionMessage::Assistant {
                    message: AssistantMessage {
                        content: vec![ContentBlock::Text(TextContent::new(format!(
                            "assistant-{i} {bulky}"
                        )))],
                        api: "test".to_string(),
                        provider: "continue-probe".to_string(),
                        model: "continue-probe-model".to_string(),
                        usage: Usage::default(),
                        stop_reason: StopReason::Stop,
                        stop_details: None,
                        error_message: None,
                        timestamp: i,
                    },
                });
            }
            session_guard.path = Some(blocked_path);
            serde_json::to_value(&session_guard.entries).expect("serialize entries")
        });

        let _ = app.handle_slash_compact("shake");
        let terminal = runtime().block_on(async {
            let recv_cx = Cx::for_testing();
            let wait = async {
                loop {
                    match event_rx.recv(&recv_cx).await {
                        Ok(PiMsg::AgentError(message)) => break format!("error:{message}"),
                        Ok(PiMsg::ConversationReset { status, .. }) => {
                            break format!("reset:{status:?}");
                        }
                        Ok(PiMsg::System(message)) => break format!("system:{message}"),
                        Ok(_) => {}
                        Err(err) => break format!("event receive failed: {err}"),
                    }
                }
            };
            futures::pin_mut!(wait);
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(5),
                wait,
            )
            .await
            .expect("shake compaction should finish before timeout")
        });
        assert!(
            terminal.starts_with("error:") && terminal.contains("could not be confirmed"),
            "compaction save failure must not emit success: {terminal}"
        );

        let session_guard = app.session.try_lock().expect("lock session");
        assert_eq!(
            serde_json::to_value(&session_guard.entries).expect("serialize live entries"),
            expected_entries
        );
        assert!(
            !session_guard
                .entries
                .iter()
                .any(|entry| matches!(entry, crate::session::SessionEntry::Compaction(_))),
            "failed compaction must not land on the live session"
        );
    }

    #[test]
    fn empty_custom_overlay_frame_keeps_overlay_visible() {
        let mut app = build_test_app();
        let poll_request = ExtensionUiRequest::new(
            "req-poll",
            "custom",
            json!({ "title": "Snake", "overlayOptions": { "width": "75%" } }),
        )
        .with_extension_id(Some("snake".to_string()));
        app.handle_custom_extension_ui_request(poll_request);

        let frame_request =
            ExtensionUiRequest::new("req-frame", "setWidget", json!({ "title": "Snake" }))
                .with_extension_id(Some("snake".to_string()));
        app.apply_custom_overlay_widget_effect(&frame_request, Vec::new());

        let overlay = app
            .extension_custom_overlay
            .as_ref()
            .expect("empty frames should keep placeholder overlay active");
        assert_eq!(overlay.extension_id.as_deref(), Some("snake"));
        assert_eq!(overlay.title.as_deref(), Some("Snake"));
        assert!(
            overlay.lines.is_empty(),
            "empty frame should preserve the waiting-state overlay"
        );
        assert!(
            app.extension_custom_active,
            "empty frame must not silently deactivate custom UI input handling"
        );
    }

    #[test]
    fn custom_overlay_poll_without_title_preserves_existing_title() {
        let mut app = build_test_app();
        let initial_request = ExtensionUiRequest::new(
            "req-open",
            "custom",
            json!({ "title": "Snake", "overlay": true }),
        )
        .with_extension_id(Some("snake".to_string()));
        app.handle_custom_extension_ui_request(initial_request);

        let poll_request = ExtensionUiRequest::new(
            "req-poll",
            "custom",
            json!({ "mode": "poll", "widgetKey": "__pi_custom_overlay" }),
        )
        .with_extension_id(Some("snake".to_string()));
        app.handle_custom_extension_ui_request(poll_request);

        let overlay = app
            .extension_custom_overlay
            .as_ref()
            .expect("poll should keep custom overlay alive");
        assert_eq!(overlay.title.as_deref(), Some("Snake"));
        assert!(app.extension_custom_active);
    }

    #[test]
    fn custom_overlay_frame_without_title_preserves_existing_title() {
        let mut app = build_test_app();
        let poll_request = ExtensionUiRequest::new(
            "req-poll",
            "custom",
            json!({ "title": "Snake", "overlay": true }),
        )
        .with_extension_id(Some("snake".to_string()));
        app.handle_custom_extension_ui_request(poll_request);

        let frame_request =
            ExtensionUiRequest::new("req-frame", "setWidget", json!({ "lines": ["score: 1"] }))
                .with_extension_id(Some("snake".to_string()));
        app.apply_custom_overlay_widget_effect(&frame_request, vec!["score: 1".to_string()]);

        let overlay = app
            .extension_custom_overlay
            .as_ref()
            .expect("frame update should keep custom overlay alive");
        assert_eq!(overlay.title.as_deref(), Some("Snake"));
        assert_eq!(overlay.lines, vec!["score: 1".to_string()]);
    }

    #[test]
    fn clear_custom_overlay_frame_still_deactivates_overlay() {
        let mut app = build_test_app();
        let poll_request = ExtensionUiRequest::new("req-poll", "custom", json!({}))
            .with_extension_id(Some("snake".to_string()));
        app.handle_custom_extension_ui_request(poll_request);
        assert!(app.extension_custom_overlay.is_some());
        assert!(app.extension_custom_active);

        let clear_request =
            ExtensionUiRequest::new("req-clear", "setWidget", json!({ "clear": true }))
                .with_extension_id(Some("snake".to_string()));
        app.apply_custom_overlay_widget_effect(&clear_request, Vec::new());

        assert!(app.extension_custom_overlay.is_none());
        assert!(!app.extension_custom_active);
        assert!(app.extension_custom_key_queue.is_empty());
    }

    #[test]
    fn custom_overlay_reduces_conversation_height_budget() {
        let mut app = build_test_app();
        app.term_height = 24;

        let idle_height = app.view_effective_conversation_height();

        app.extension_custom_overlay = Some(ExtensionCustomOverlay {
            extension_id: Some("snake".to_string()),
            title: Some("Snake".to_string()),
            lines: vec![
                "score: 1".to_string(),
                "score: 2".to_string(),
                "score: 3".to_string(),
                "score: 4".to_string(),
                "score: 5".to_string(),
                "score: 6".to_string(),
            ],
        });

        assert!(
            !app.editor_input_is_available(),
            "custom overlays should hide the normal editor input"
        );
        assert!(
            app.view_effective_conversation_height() < idle_height,
            "custom overlay rows must shrink the conversation viewport budget"
        );
    }

    #[test]
    fn capability_prompt_takes_key_priority_over_custom_overlay() {
        let mut app = build_test_app();
        let poll_request = ExtensionUiRequest::new(
            "req-poll",
            "custom",
            json!({ "title": "Snake", "overlay": true }),
        )
        .with_extension_id(Some("snake".to_string()));
        app.handle_custom_extension_ui_request(poll_request);

        let capability_request = ExtensionUiRequest::new_capability_prompt(
            "req-cap",
            "snake",
            "exec",
            json!({
                "extension_id": "snake",
                "capability": "exec",
                "message": "Needs shell access",
            }),
        );
        app.capability_prompt = Some(CapabilityPromptOverlay::from_request(capability_request));

        let _ = app.update(Message::new(KeyMsg::from_type(KeyType::Right)));

        let prompt = app
            .capability_prompt
            .as_ref()
            .expect("capability prompt should remain active");
        assert_eq!(
            prompt.focused, 1,
            "Right arrow should move capability prompt focus instead of being swallowed by the custom overlay"
        );
        assert!(
            app.extension_custom_key_queue.is_empty(),
            "modal prompt keys must not leak into the custom overlay key queue"
        );
    }

    // ===== bd-yllbn: capability prompt FIFO / generation / expiry semantics =====

    fn capability_request(id: &str, extension_id: &str, capability: &str) -> ExtensionUiRequest {
        let mut request = ExtensionUiRequest::new_capability_prompt(
            id,
            extension_id,
            capability,
            json!({
                "extension_id": extension_id,
                "capability": capability,
                "message": "bd-yllbn probe",
                "timeout_ms": 1_000_u64,
            }),
        )
        .with_timeout_ms(1_000);
        // Production requests bind at manager admission. These state-machine
        // fixtures inject PiMsg directly, so bind at the equivalent seam.
        request.bind_deadline(std::time::Instant::now());
        request
    }

    fn active_capability(app: &PiApp) -> Option<(String, u64, u64)> {
        app.capability_prompt.as_ref().map(|prompt| {
            (
                prompt.request.id.clone(),
                prompt.generation,
                prompt.timer_generation(),
            )
        })
    }

    fn force_capability_deadline_elapsed(app: &mut PiApp, id: &str) {
        let elapsed = std::time::Instant::now();
        if let Some(prompt) = app
            .capability_prompt
            .as_mut()
            .filter(|prompt| prompt.request.id == id)
        {
            prompt.expires_at = Some(elapsed);
            return;
        }
        app.capability_prompt_queue
            .iter_mut()
            .find(|prompt| prompt.request.id == id)
            .expect("capability prompt fixture must exist")
            .expires_at = Some(elapsed);
    }

    #[test]
    fn capability_prompts_queue_fifo_and_resolve_in_order() {
        let mut app = build_test_app();

        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
            "r1", "ext-a", "exec",
        )));
        let (first_id, first_gen, first_timer_gen) =
            active_capability(&app).expect("first request activates");
        assert_eq!(first_id, "r1");
        assert!(app.capability_prompt_queue.is_empty());

        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
            "r2", "ext-b", "http",
        )));
        let unchanged = active_capability(&app).expect("active survives arrival");
        assert_eq!(
            unchanged,
            (first_id.clone(), first_gen, first_timer_gen),
            "a second concurrent request must never overwrite the active overlay"
        );
        assert_eq!(app.capability_prompt_queue.len(), 1);
        assert_eq!(
            app.capability_prompt_queue
                .front()
                .expect("queued prompt")
                .request
                .id,
            "r2"
        );

        // Only the exact (id, generation) identity resolves the live prompt;
        // resolution promotes the FIFO successor and schedules its wake.
        force_capability_deadline_elapsed(&mut app, &first_id);
        let cmd = app.handle_pi_message(PiMsg::CapabilityPromptTick {
            id: first_id.clone(),
            generation: first_gen,
            timer_generation: first_timer_gen,
        });
        assert!(
            cmd.is_some(),
            "successor activation must schedule its own expiry wake"
        );
        let (second_id, second_gen, second_timer_gen) =
            active_capability(&app).expect("successor activates");
        assert_eq!(second_id, "r2");
        assert_ne!(second_gen, first_gen, "generations are unique per request");
        assert!(app.capability_prompt_queue.is_empty());

        // Stale replay of the resolved identity must be ignored outright...
        assert!(
            app.handle_pi_message(PiMsg::CapabilityPromptTick {
                id: first_id,
                generation: first_gen,
                timer_generation: first_timer_gen,
            })
            .is_none()
        );
        // ...as must a foreign id wearing a live generation.
        assert!(
            app.handle_pi_message(PiMsg::CapabilityPromptTick {
                id: "zzz-unknown".to_string(),
                generation: second_gen,
                timer_generation: second_timer_gen,
            })
            .is_none()
        );
        let survived = active_capability(&app).expect("live prompt untouched by stale wakes");
        assert_eq!(survived, (second_id.clone(), second_gen, second_timer_gen));

        // Correct final resolution empties everything; no successor exists.
        force_capability_deadline_elapsed(&mut app, &second_id);
        let tail_cmd = app.handle_pi_message(PiMsg::CapabilityPromptTick {
            id: second_id,
            generation: second_gen,
            timer_generation: second_timer_gen,
        });
        assert!(tail_cmd.is_none());
        assert!(active_capability(&app).is_none());
        assert!(app.capability_prompt_queue.is_empty());
    }

    #[test]
    fn timed_out_first_manager_prompt_auto_denies_and_promotes_second() {
        let mut app = build_test_app();
        let manager = crate::extensions::ExtensionManager::new();
        let (ui_tx, mut ui_rx) = asupersync::channel::mpsc::channel(4);
        manager.set_ui_sender(ui_tx);
        app.extensions = Some(manager.clone());

        let mut first_attempt = Box::pin(manager.request_ui(capability_request(
            "timeout-first",
            "ext-timeout",
            "exec",
        )));
        let first_request = runtime().block_on(async {
            assert!(futures::poll!(first_attempt.as_mut()).is_pending());
            let cx = Cx::for_request();
            ui_rx.recv(&cx).await.expect("first prompt reaches TUI")
        });
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(first_request));

        let mut second_attempt = Box::pin(manager.request_ui(capability_request(
            "timeout-second",
            "ext-timeout",
            "http",
        )));
        let second_request = runtime().block_on(async {
            assert!(futures::poll!(second_attempt.as_mut()).is_pending());
            let cx = Cx::for_request();
            ui_rx.recv(&cx).await.expect("second prompt reaches TUI")
        });
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(second_request));

        force_capability_deadline_elapsed(&mut app, "timeout-first");
        let (_, prompt_generation, timer_generation) =
            active_capability(&app).expect("first prompt remains active");
        let successor_wake = app.handle_pi_message(PiMsg::CapabilityPromptTick {
            id: "timeout-first".to_string(),
            generation: prompt_generation,
            timer_generation,
        });

        let first_response = runtime()
            .block_on(first_attempt)
            .expect("auto-deny responds through manager")
            .expect("capability prompt expects a response");
        assert_eq!(
            first_response.value,
            Some(json!({
                "allow": false,
                "persist": false,
                "remember": false,
                "reason": "auto_deny",
            }))
        );
        assert!(!first_response.cancelled);
        assert!(!manager.ui_request_is_pending("timeout-first"));
        assert!(
            successor_wake.is_some(),
            "second prompt owns the active tick"
        );
        assert_eq!(
            active_capability(&app).map(|active| active.0),
            Some("timeout-second".to_string())
        );

        let _ = app.handle_capability_prompt_key(&KeyMsg::from_type(KeyType::Esc));
        let second_response = runtime()
            .block_on(second_attempt)
            .expect("Escape responds through manager")
            .expect("capability prompt expects a response");
        assert!(second_response.cancelled);
        assert!(!manager.ui_request_is_pending("timeout-second"));
        assert!(app.capability_prompt.is_none());
        assert!(app.capability_prompt_queue.is_empty());
    }

    #[test]
    fn capability_prompt_queue_bound_denies_excess_fail_closed() {
        let mut app = build_test_app();
        let total = PiApp::MAX_CAPABILITY_PROMPT_QUEUE + 3;
        for i in 0..total {
            let id = format!("cap-{i:02}");
            let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
                &id,
                "ext-flood",
                "exec",
            )));
        }

        // The bound holds exactly; ordering of admitted prompts is FIFO.
        assert_eq!(
            app.capability_prompt_queue.len(),
            PiApp::MAX_CAPABILITY_PROMPT_QUEUE
        );
        assert_eq!(
            app.capability_prompt
                .as_ref()
                .map(|prompt| prompt.request.id.as_str()),
            Some("cap-00")
        );
        for (slot, expected) in (1..=PiApp::MAX_CAPABILITY_PROMPT_QUEUE).enumerate() {
            let want = format!("cap-{expected:02}");
            let got = app
                .capability_prompt_queue
                .get(slot)
                .map(|prompt| prompt.request.id.clone())
                .unwrap_or_default();
            assert_eq!(got, want);
        }
        // Admitted identities are exactly active (00) + FIFO (01..=MAX);
        // everything beyond that bound was denied on arrival.
        for i in PiApp::MAX_CAPABILITY_PROMPT_QUEUE + 1..total {
            let id = format!("cap-{i:02}");
            assert!(
                app.capability_prompt_queue
                    .iter()
                    .all(|prompt| prompt.request.id != id)
            );
        }
    }

    #[test]
    fn conversation_reset_drains_prompts_fail_closed_without_permissions() {
        let mut app = build_test_app();
        let manager = crate::extensions::ExtensionManager::new();
        let (ui_tx, mut ui_rx) = asupersync::channel::mpsc::channel(4);
        manager.set_ui_sender(ui_tx);
        app.extensions = Some(manager.clone());

        let requests = [
            capability_request("d1", "ext-d", "http"),
            capability_request("d2", "ext-d", "exec"),
            capability_request("d3", "ext-e", "exec"),
        ];
        let mut attempts = Vec::new();
        let mut wakes = Vec::new();
        for request in requests {
            let mut attempt = Box::pin(manager.request_ui(request));
            let delivered = runtime().block_on(async {
                assert!(futures::poll!(attempt.as_mut()).is_pending());
                let cx = Cx::for_request();
                ui_rx
                    .recv(&cx)
                    .await
                    .expect("manager publishes capability prompt")
            });
            assert!(manager.ui_request_is_pending(&delivered.id));
            wakes.push(
                app.handle_pi_message(PiMsg::ExtensionUiRequest(delivered))
                    .expect("bounded prompt schedules a cancellable wake"),
            );
            attempts.push(attempt);
        }
        assert!(active_capability(&app).is_some());
        assert_eq!(app.capability_prompt_queue.len(), 2);
        let session_id = app
            .session
            .try_lock()
            .expect("lock session")
            .header
            .id
            .clone();

        app.handle_pi_message(PiMsg::ConversationReset {
            session_id,
            messages: Vec::new(),
            usage: Usage::default(),
            status: Some("session reset".to_string()),
        });

        assert!(
            active_capability(&app).is_none(),
            "session reset clears the active prompt"
        );
        assert!(
            app.capability_prompt_queue.is_empty(),
            "and the entire FIFO"
        );
        for (id, attempt) in ["d1", "d2", "d3"].into_iter().zip(attempts) {
            let response = runtime()
                .block_on(attempt)
                .expect("reset delivers a typed response")
                .expect("capability prompts expect a response");
            assert_eq!(response.id, id);
            assert_eq!(response.value, Some(Value::Bool(false)));
            assert!(response.cancelled);
            assert!(!manager.ui_request_is_pending(id));
        }
        for wake in wakes {
            assert!(
                wake.execute().is_none(),
                "session reset must interrupt every outstanding timer command"
            );
        }
    }

    #[test]
    fn live_queued_tick_rearms_once_with_a_fresh_timer_epoch() {
        let mut app = build_test_app();
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
            "active", "ext-a", "exec",
        )));
        let active_before = active_capability(&app).expect("first prompt activates");
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
            "queued", "ext-b", "http",
        )));
        let queued = app
            .capability_prompt_queue
            .front_mut()
            .expect("second prompt queues");
        queued.expires_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
        let prompt_generation = queued.generation;
        let timer_generation = queued.timer_generation();

        let next_waiter = app.handle_pi_message(PiMsg::CapabilityPromptTick {
            id: "queued".to_string(),
            generation: prompt_generation,
            timer_generation,
        });

        assert!(
            next_waiter.is_some(),
            "a premature queued wake must retain the absolute-deadline waiter"
        );
        assert_eq!(active_capability(&app), Some(active_before));
        let rearmed = app
            .capability_prompt_queue
            .front()
            .expect("queued prompt remains queued");
        assert_eq!(rearmed.generation, prompt_generation);
        assert_ne!(rearmed.timer_generation(), timer_generation);
        assert!(
            app.handle_pi_message(PiMsg::CapabilityPromptTick {
                id: "queued".to_string(),
                generation: prompt_generation,
                timer_generation,
            })
            .is_none(),
            "a duplicate of the delivered queued tick cannot fork a waiter chain"
        );
        assert_eq!(app.capability_prompt_queue.len(), 1);
    }

    /// bd-yllbn reopen audit: a queued prompt's budget elapses on its own
    /// clock. With an UNBUDGETED active prompt blocking the slot, the queued
    /// item must still resolve at its own deadline without touching — or
    /// promoting past — anything else in the queue.
    #[test]
    fn queued_prompt_expires_independently_of_unbudgeted_active() {
        let mut app = build_test_app();

        // An embedder-sourced confirm carries NO timeout: unbudgeted active.
        let unbudgeted = ExtensionUiRequest::new_capability_prompt(
            "r0",
            "ext-embed",
            "exec",
            json!({
                "extension_id": "ext-embed",
                "capability": "exec",
                "message": "no deadline supplied"
            }),
        );
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(unbudgeted));
        let active_before = active_capability(&app).expect("unbudgeted r0 activates");
        assert!(
            app.capability_prompt
                .as_ref()
                .expect("active")
                .expires_at
                .is_none(),
            "fixture sanity: r0 really is unbudgeted"
        );

        // Budgeted successor arrives and queues; enqueue must schedule its
        // own independent wake now (the audited stranding gap).
        let wake = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
            "r2", "ext-b", "http",
        )));
        assert!(
            wake.is_some(),
            "queued bounded prompts schedule their own expiry wake"
        );

        // Its deadline passes while r0 lingers: resolve r2 by identity only.
        force_capability_deadline_elapsed(&mut app, "r2");
        let queued = app
            .capability_prompt_queue
            .front()
            .expect("r2 still queued");
        let queued_generation = queued.generation;
        let queued_timer_generation = queued.timer_generation();
        let expired_wake = app.handle_pi_message(PiMsg::CapabilityPromptTick {
            id: "r2".to_string(),
            generation: queued_generation,
            timer_generation: queued_timer_generation,
        });
        assert!(
            expired_wake.is_none(),
            "queue-path resolution never promotes or schedules anything"
        );
        assert_eq!(
            active_capability(&app),
            Some(active_before),
            "the unbudgeted active overlay is untouched by another prompt's expiry"
        );
        assert!(
            app.capability_prompt_queue.is_empty(),
            "r2 alone was denied"
        );

        // A replay of the same identity after removal is inert (stale guard).
        assert!(
            app.handle_pi_message(PiMsg::CapabilityPromptTick {
                id: "r2".to_string(),
                generation: u64::MAX,
                timer_generation: u64::MAX,
            })
            .is_none()
        );
    }

    /// Unbudgeted arrivals activate without scheduling any timer; their wake
    /// machinery stays fully dormant so no spurious auto-deny can fire.
    #[test]
    fn unbudgeted_prompt_schedules_no_expiry_wake() {
        let mut app = build_test_app();
        let unbudgeted = ExtensionUiRequest::new_capability_prompt(
            "u1",
            "ext-e",
            "http",
            json!({
                "extension_id": "ext-e",
                "capability": "http",
                "message": "no deadline"
            }),
        );

        let wake = app.handle_pi_message(PiMsg::ExtensionUiRequest(unbudgeted));
        assert!(wake.is_none(), "no timeout means no timer");
        assert!(
            app.capability_prompt
                .as_ref()
                .and_then(|prompt| prompt.expires_at)
                .is_none()
        );
    }

    #[test]
    fn capability_prompt_tick_repaints_before_expiry_without_resolving() {
        let mut app = build_test_app();
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
            "tick-1", "ext-tick", "exec",
        )));
        app.capability_prompt
            .as_mut()
            .expect("prompt activates")
            .expires_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
        let (id, generation, timer_generation) = active_capability(&app).expect("prompt activates");

        let next_tick = app.handle_pi_message(PiMsg::CapabilityPromptTick {
            id: id.clone(),
            generation,
            timer_generation,
        });

        assert!(
            next_tick.is_some(),
            "a live prompt schedules its next repaint"
        );
        let rearmed = active_capability(&app).expect("prompt remains active");
        assert_eq!(rearmed.0, id);
        assert_eq!(rearmed.1, generation);
        assert_ne!(
            rearmed.2, timer_generation,
            "each periodic waiter must own a fresh timer epoch"
        );
        assert!(
            app.handle_pi_message(PiMsg::CapabilityPromptTick {
                id: rearmed.0,
                generation,
                timer_generation,
            })
            .is_none(),
            "a duplicate of the delivered tick cannot fork a waiter chain"
        );
    }

    #[test]
    fn capability_prompt_render_countdown_visibly_decreases_without_input() {
        let mut app = build_test_app();
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
            "render-tick",
            "ext-tick",
            "exec",
        )));
        let base = std::time::Instant::now();
        let prompt = app.capability_prompt.as_mut().expect("prompt activates");
        prompt.expires_at = Some(base + std::time::Duration::from_nanos(1));
        assert_eq!(prompt.remaining_secs(base), Some(1));
        prompt.expires_at = Some(base + std::time::Duration::from_secs(1));
        assert_eq!(prompt.remaining_secs(base), Some(1));
        prompt.expires_at =
            Some(base + std::time::Duration::from_secs(1) + std::time::Duration::from_nanos(1));
        assert_eq!(prompt.remaining_secs(base), Some(2));
        prompt.expires_at = Some(base + std::time::Duration::from_secs(2));
        let prompt = app
            .capability_prompt
            .as_ref()
            .expect("prompt remains active");

        let initial = app.render_capability_prompt_at(prompt, 0, base);
        let next =
            app.render_capability_prompt_at(prompt, 0, base + std::time::Duration::from_secs(1));

        assert!(initial.contains("Auto-deny in 2s"), "{initial}");
        assert!(next.contains("Auto-deny in 1s"), "{next}");
        assert_ne!(initial, next, "a timer tick must produce a changed frame");
    }

    #[test]
    fn promoting_queued_prompt_cancels_and_invalidates_queued_timer() {
        let mut app = build_test_app();
        let _ = app.handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
            "active", "ext-a", "exec",
        )));
        let queued_wake = app
            .handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
                "queued", "ext-b", "http",
            )))
            .expect("queued bounded prompt owns a deadline waiter");
        let queued = app
            .capability_prompt_queue
            .front()
            .expect("queued prompt retained");
        let stale_prompt_generation = queued.generation;
        let stale_timer_generation = queued.timer_generation();

        let _ = app.handle_capability_prompt_key(&KeyMsg::from_type(KeyType::Enter));
        let promoted = active_capability(&app).expect("queued prompt promotes");
        assert_eq!(promoted.0, "queued");
        assert_ne!(promoted.2, stale_timer_generation);
        assert!(
            queued_wake.execute().is_none(),
            "promotion must interrupt the queued deadline waiter"
        );
        assert!(
            app.handle_pi_message(PiMsg::CapabilityPromptTick {
                id: "queued".to_string(),
                generation: stale_prompt_generation,
                timer_generation: stale_timer_generation,
            })
            .is_none(),
            "an already-enqueued queued-era wake cannot rearm the active timer"
        );
        assert_eq!(active_capability(&app), Some(promoted));
    }

    #[test]
    fn resolving_capability_prompt_cancels_outstanding_tick_command() {
        let mut app = build_test_app();
        let wake = app
            .handle_pi_message(PiMsg::ExtensionUiRequest(capability_request(
                "cancel-tick",
                "ext-tick",
                "exec",
            )))
            .expect("bounded prompt schedules a wake");

        let _ = app.handle_capability_prompt_key(&KeyMsg::from_type(KeyType::Enter));

        assert!(app.capability_prompt.is_none());
        assert!(
            wake.execute().is_none(),
            "the cancelled command must exit without publishing a stale tick"
        );
    }

    struct CountingProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for CountingProvider {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn api(&self) -> &'static str {
            "counting"
        }

        fn model_id(&self) -> &'static str {
            "counting-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::pin(stream::empty()))
        }
    }

    fn session_user(text: &str) -> crate::session::SessionMessage {
        crate::session::SessionMessage::User {
            content: crate::model::UserContent::Text(text.to_string()),
            timestamp: Some(0),
        }
    }

    fn session_assistant(text: &str) -> crate::session::SessionMessage {
        crate::session::SessionMessage::from(crate::model::Message::Assistant(Arc::new(
            AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                ..Default::default()
            },
        )))
    }

    fn seed_linear_retry_turn(app: &PiApp) -> (String, String, String) {
        runtime().block_on(async {
            let cx = Cx::for_request();
            let mut session_guard =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &cx)
                    .await
                    .expect("lock session");
            session_guard.append_message(session_user("first question"));
            let first_answer = session_guard.append_message(session_assistant("first answer"));
            let abandoned = session_guard.append_message(session_user("second question"));
            session_guard.append_message(session_assistant("second answer"));
            let messages = session_guard.to_messages_for_current_path();
            let session_id = session_guard.header.id.clone();
            drop(session_guard);
            let mut agent_guard =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.agent), &cx)
                    .await
                    .expect("lock agent");
            agent_guard.replace_messages(messages);
            (first_answer, abandoned, session_id)
        })
    }

    fn wait_for_retry_terminal(event_rx: &mut mpsc::Receiver<PiMsg>) -> PiMsg {
        runtime().block_on(async {
            let recv_cx = Cx::for_testing();
            let wait = async {
                loop {
                    match event_rx.recv(&recv_cx).await {
                        Ok(
                            msg @ (PiMsg::RetryCommitted { .. }
                            | PiMsg::AgentError(_)
                            | PiMsg::System(_)),
                        ) => break msg,
                        Ok(_) => {}
                        Err(err) => panic!("retry event receive failed: {err}"),
                    }
                }
            };
            futures::pin_mut!(wait);
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(5),
                wait,
            )
            .await
            .expect("retry should emit a terminal event before timeout")
        })
    }

    #[test]
    fn slash_retry_via_submit_message_commits_sibling_parent_without_reparse() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let (mut app, mut event_rx) = build_test_app_with_provider(provider);
        app.save_enabled = false;
        app.extensions = None;
        let (first_answer, abandoned, session_id) = seed_linear_retry_turn(&app);
        let (ui_messages, usage) = {
            let session_guard = app.session.try_lock().expect("lock session");
            crate::interactive::conversation_from_session(&session_guard)
        };
        app.messages = ui_messages;
        app.total_usage = usage;

        let _ = app.submit_message("/retry");
        let terminal = wait_for_retry_terminal(&mut event_rx);
        let PiMsg::RetryCommitted {
            session_id: committed_id,
            text,
            ..
        } = terminal.clone()
        else {
            panic!("expected RetryCommitted, got {terminal:?}");
        };
        assert_eq!(committed_id, session_id);
        assert_eq!(text, "second question");

        {
            let session_guard = app.session.try_lock().expect("lock session");
            assert_eq!(session_guard.leaf_id(), Some(first_answer.as_str()));
            let abandoned_on_path = session_guard
                .entries_for_current_path()
                .iter()
                .any(|entry| entry.base().id.as_ref() == Some(&abandoned));
            assert!(!abandoned_on_path);
            assert!(session_guard.get_entry(&abandoned).is_some());
        }

        let _ = app.handle_pi_message(terminal);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while calls.load(std::sync::atomic::Ordering::SeqCst) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            app.messages
                .iter()
                .any(|message| message.content.contains("second question")),
            "UI should show the retried prompt; status={:?} state={:?} calls={}",
            app.status_message,
            app.agent_state,
            calls.load(std::sync::atomic::Ordering::SeqCst)
        );
        assert!(
            app.messages
                .iter()
                .all(|message| !message.content.contains("second answer")),
            "UI must drop the abandoned assistant turn: {:?}",
            app.messages
                .iter()
                .map(|message| &message.content)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn slash_retry_refuses_pending_input_without_touching_session_or_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let (mut app, mut event_rx) = build_test_app_with_provider(provider);
        app.save_enabled = false;
        let (_first_answer, _abandoned, _session_id) = seed_linear_retry_turn(&app);
        let leaf_before = app
            .session
            .try_lock()
            .expect("lock session")
            .leaf_id()
            .map(str::to_string);
        app.pending_inputs
            .push_back(PendingInput::Text("queued".to_string()));
        let _ = app.submit_message("/retry");
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|message| message.contains("Queued input")),
            "pending input must refuse retry: {:?}",
            app.status_message
        );
        assert_eq!(
            app.session
                .try_lock()
                .expect("lock session")
                .leaf_id()
                .map(str::to_string),
            leaf_before
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn slash_retry_refuses_user_blocks_barrier_without_provider_calls() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let (mut app, mut event_rx) = build_test_app_with_provider(provider);
        app.save_enabled = false;
        runtime().block_on(async {
            let cx = Cx::for_request();
            let mut session_guard =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &cx)
                    .await
                    .expect("lock session");
            session_guard.append_message(session_user("older prompt"));
            session_guard.append_message(crate::session::SessionMessage::User {
                content: crate::model::UserContent::Blocks(vec![ContentBlock::Text(
                    TextContent::new("image prompt"),
                )]),
                timestamp: Some(0),
            });
        });
        let leaf_before = app
            .session
            .try_lock()
            .expect("lock session")
            .leaf_id()
            .map(str::to_string);
        let _ = app.submit_message("/retry");
        assert_eq!(app.status_message.as_deref(), Some("No user turn to retry"));
        assert_eq!(
            app.session
                .try_lock()
                .expect("lock session")
                .leaf_id()
                .map(str::to_string),
            leaf_before
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn slash_retry_save_failure_leaves_session_and_ui_untouched() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(CountingProvider {
            calls: Arc::clone(&calls),
        });
        let (mut app, mut event_rx) = build_test_app_with_provider(provider);
        app.save_enabled = true;
        app.extensions = None;
        let temp = tempfile::TempDir::new().expect("tempdir");
        let blocked_path = temp.path().join("blocked.jsonl");
        std::fs::create_dir(&blocked_path).expect("create directory at session path");
        let (first_answer, abandoned, _session_id) = seed_linear_retry_turn(&app);
        let (leaf_before, entries_before) = {
            let mut session_guard = app.session.try_lock().expect("lock session");
            session_guard.path = Some(blocked_path);
            (
                session_guard.leaf_id().map(str::to_string),
                serde_json::to_value(&session_guard.entries).expect("serialize"),
            )
        };
        let ui_before = app.messages.clone();

        let _ = app.submit_message("/retry");
        let terminal = wait_for_retry_terminal(&mut event_rx);
        assert!(
            matches!(terminal, PiMsg::AgentError(ref message) if message.contains("could not be confirmed")),
            "save failure must be terminal: {terminal:?}"
        );
        let _ = app.handle_pi_message(terminal);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let session_guard = app.session.try_lock().expect("lock session");
        assert_eq!(session_guard.leaf_id().map(str::to_string), leaf_before);
        assert_eq!(
            serde_json::to_value(&session_guard.entries).expect("serialize live"),
            entries_before
        );
        assert_eq!(session_guard.leaf_id(), leaf_before.as_deref());
        assert!(session_guard.get_entry(&abandoned).is_some());
        assert!(session_guard.get_entry(&first_answer).is_some());
        assert!(
            app.messages.iter().any(|message| {
                message.content.contains("could not be confirmed")
                    || message.content.contains("Retry")
            }),
            "save failure must surface one terminal UI error: {:?}",
            app.messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
        );
        let _ = ui_before;
    }
}

/// Coverage rule (bd-cv653.9.2 renderer registry): every native agent-facing
/// tool has an explicit renderer entry. Only extension-defined tools use the
/// generic/custom-renderer fallback. These tests also pin representative
/// argument shaping and hostile-input clipping.
#[cfg(test)]
mod tool_invocation_summary_coverage {
    use super::{tool_invocation_renderer, tool_invocation_summary};

    #[test]
    fn every_native_tool_has_a_registered_card_renderer() {
        let native_tools = [
            "read",
            "bash",
            "edit",
            "write",
            "grep",
            "find",
            "ls",
            "hashline_edit",
            "jobs",
            "hub",
            "security_scan",
            "web_search",
            "eval",
            "github",
            "ast_grep",
            "ast_edit",
            "lsp",
            "debug",
            "inspect_image",
            "generate_image",
            "tts",
            "computer",
            "browser",
            "subagent",
            "retain",
            "recall",
            "reflect",
            "memory_edit",
            "learn",
            "manage_skill",
            "ask",
            "todo",
            "submit_plan",
            "xdev",
        ];
        for name in native_tools {
            assert!(
                tool_invocation_renderer(name).is_some(),
                "{name} must not use the generic card fallback"
            );
        }
        assert!(
            tool_invocation_renderer("mcp__docs__search").is_some(),
            "the dynamic MCP namespace needs a renderer"
        );
        assert!(
            tool_invocation_renderer("extension_owned_tool").is_none(),
            "extension tools retain their custom/generic renderer bridge"
        );
        assert!(
            tool_invocation_renderer("mcp__").is_none(),
            "the MCP namespace prefix alone is not a mounted tool"
        );
    }

    #[test]
    fn path_tools_summarize_the_target() {
        let args = serde_json::json!({ "path": "src/main.rs" });
        for name in ["read", "write", "edit", "hashline_edit", "ls"] {
            let summary =
                tool_invocation_summary(name, &args).unwrap_or_else(|| panic!("{name} uncovered"));
            assert!(
                summary.contains("src/main.rs"),
                "{name}: head must cite the target path: {summary}"
            );
        }
    }

    #[test]
    fn ls_uses_the_documented_current_directory_default() {
        assert_eq!(
            tool_invocation_summary("ls", &serde_json::json!({})).as_deref(),
            Some(".")
        );
        assert_eq!(
            tool_invocation_summary("ls", &serde_json::json!({"path": null})).as_deref(),
            Some(".")
        );
        assert!(tool_invocation_summary("ls", &serde_json::json!({"path": 42})).is_none());
    }

    #[test]
    fn bash_elides_multiline_commands_to_the_safe_first_line() {
        let args = serde_json::json!({ "command": "cargo test\nrm -rf /tmp/second-line" });
        let summary = tool_invocation_summary("bash", &args).expect("bash covered");
        assert!(summary.starts_with("cargo test"), "head: {summary}");
        assert!(summary.ends_with('…'), "multiline must elide: {summary}");
        assert!(
            !summary.contains("second-line"),
            "elided lines must not leak"
        );
    }

    #[test]
    fn search_tools_cite_pattern_and_scope() {
        let scoped = tool_invocation_summary(
            "grep",
            &serde_json::json!({ "pattern": "TODO", "path": "src" }),
        )
        .expect("grep scoped");
        assert!(
            scoped.contains("TODO") && scoped.contains("in src"),
            "{scoped}"
        );
        let bare = tool_invocation_summary("grep", &serde_json::json!({ "pattern": "TODO" }))
            .expect("grep bare");
        assert_eq!(bare, "TODO");
    }

    #[test]
    fn hostile_command_has_control_characters_dropped() {
        let args = serde_json::json!({ "command": "echo \u{1b}[31mred\u{1b}[0m" });
        let summary = tool_invocation_summary("bash", &args).expect("bash covered");
        assert!(
            !summary.contains('\x1b'),
            "ESC must be stripped: {summary:?}"
        );
    }

    #[test]
    fn newer_tool_families_render_meaningful_heads() {
        assert_eq!(
            tool_invocation_summary(
                "ask",
                &serde_json::json!({"questions": [{"question": "Which target?"}]})
            )
            .as_deref(),
            Some("Which target?")
        );
        assert_eq!(
            tool_invocation_summary(
                "lsp",
                &serde_json::json!({"action": " definition ", "file": " src/main.rs "})
            )
            .as_deref(),
            Some("definition · src/main.rs")
        );
        assert_eq!(
            tool_invocation_summary(
                "subagent",
                &serde_json::json!({"agent": " reviewer ", "task": " audit the patch "})
            )
            .as_deref(),
            Some("reviewer: audit the patch")
        );
        assert_eq!(
            tool_invocation_summary("mcp__docs__search", &serde_json::json!({})).as_deref(),
            Some("MCP docs · search")
        );
    }

    #[test]
    fn registered_renderers_reject_incomplete_arguments() {
        for name in ["ask", "todo", "web_search", "submit_plan", "xdev"] {
            assert!(tool_invocation_summary(name, &serde_json::json!({})).is_none());
        }
        assert!(
            tool_invocation_summary(
                "lsp",
                &serde_json::json!({"action": "   ", "file": "src/main.rs"})
            )
            .is_none()
        );
        assert!(
            tool_invocation_summary(
                "subagent",
                &serde_json::json!({"agent": "reviewer", "task": "   "})
            )
            .is_none()
        );
        assert!(
            tool_invocation_summary("subagent", &serde_json::json!({"task": "audit"})).is_none()
        );
        assert!(
            tool_invocation_summary("subagent", &serde_json::json!({"agent": "reviewer"}))
                .is_none()
        );
        assert!(tool_invocation_summary("subagent", &serde_json::json!({"tasks": []})).is_none());
        assert!(
            tool_invocation_summary(
                "subagent",
                &serde_json::json!({
                    "agent": "reviewer",
                    "task": "audit",
                    "tasks": [{"agent": "reviewer", "task": "parallel audit"}]
                })
            )
            .is_none()
        );
    }
}
