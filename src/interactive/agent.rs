use super::conversation::{
    add_usage, build_content_blocks_for_input, content_blocks_to_text, last_assistant_message,
    split_content_blocks_for_input,
};
use super::ext_session::{format_extension_ui_prompt, parse_extension_ui_response};
use super::*;
use crate::extension_events::{BeforeAgentStartOutcome, apply_before_agent_start_response};

pub(super) fn extension_commands_for_catalog(
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

const UI_STREAM_DELTA_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(45);
const UI_STREAM_DELTA_MAX_BUFFER_BYTES: usize = 2 * 1024;
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
    PiMsg::AgentDone {
        usage: Some(usage),
        stop_reason: last
            .as_ref()
            .map_or(StopReason::Stop, |msg| msg.stop_reason),
        error_message: last.as_ref().and_then(|msg| msg.error_message.clone()),
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
            ..
        } => {
            batcher.send_immediate(PiMsg::ToolStart {
                name: tool_name.clone(),
                tool_id: tool_call_id.clone(),
            });
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
            ..
        } => {
            batcher.send_immediate(PiMsg::ToolEnd {
                name: tool_name.clone(),
                tool_id: tool_call_id.clone(),
                is_error: *is_error,
            });
        }
        AgentEvent::AgentEnd { messages, .. } => {
            batcher.send_immediate(build_agent_done_pi_msg(messages));
        }
        _ => {}
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

impl PiApp {
    /// Handle custom Pi messages from the agent.
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_pi_message(&mut self, msg: PiMsg) -> Option<Cmd> {
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
            PiMsg::EnqueuePendingInput(input) => {
                self.pending_inputs.push_back(input);
                if self.agent_state == AgentState::Idle {
                    return self.run_next_pending();
                }
            }
            PiMsg::UiShutdown => {
                // Internal signal for shutting down the async→UI bridge; should not normally reach
                // the UI event loop, but handle it defensively.
            }
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
            PiMsg::ToolStart { name, .. } => {
                self.agent_state = AgentState::ToolRunning;
                self.current_tool = Some(name);
                self.tool_progress = Some(ToolProgress::new());
                self.pending_tool_output = None;
            }
            PiMsg::ToolUpdate {
                name,
                content,
                details,
                ..
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
                    self.pending_tool_output = Some(format!("Tool {name} output:\n{output}"));
                }
            }
            PiMsg::ToolEnd { .. } => {
                self.agent_state = AgentState::Processing;
                self.current_tool = None;
                self.tool_progress = None;
                if let Some(output) = self.pending_tool_output.take() {
                    self.messages.push(ConversationMessage::tool(output));
                    self.scroll_to_bottom();
                }
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
                self.abort_handle = None;
                self.extension_streaming.store(false, Ordering::SeqCst);
                self.extension_compacting.store(false, Ordering::SeqCst);

                // Refresh VCS info (may have changed during tool execution)
                self.vcs_info = super::read_vcs_info(&self.cwd);

                if stop_reason == StopReason::Aborted {
                    self.status_message = Some("Request aborted".to_string());
                } else if stop_reason == StopReason::Error {
                    let message = error_message.unwrap_or_else(|| "Request failed".to_string());
                    self.status_message = Some(message.clone());
                    if !had_response {
                        self.messages.push(ConversationMessage {
                            role: MessageRole::System,
                            content: format!("Error: {message}"),
                            thinking: None,
                            collapsed: false,
                        });
                    }
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

                if !self.pending_inputs.is_empty() {
                    return Some(Cmd::new(|| Message::new(PiMsg::RunPending)));
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
                self.abort_handle = None;
                self.extension_streaming.store(false, Ordering::SeqCst);
                self.extension_compacting.store(false, Ordering::SeqCst);
                self.input.focus();
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
                self.abort_handle = None;
                self.extension_streaming.store(false, Ordering::SeqCst);
                self.extension_compacting.store(false, Ordering::SeqCst);
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
            PiMsg::ConversationReset {
                messages,
                usage,
                status,
            } => {
                self.messages = messages;
                self.total_usage = usage;
                self.current_response.clear();
                self.current_thinking.clear();
                self.agent_state = AgentState::Idle;
                self.current_tool = None;
                self.abort_handle = None;
                self.status_message = status;
                self.message_render_cache.clear();
                if let Err(message) = self.sync_runtime_selection_from_session_header() {
                    self.status_message = Some(message);
                }
                self.scroll_to_bottom();
                self.input.focus();
            }
            PiMsg::SetEditorText(text) => {
                self.input.set_value(&text);
                self.input.focus();
            }
            PiMsg::OpenTree {
                initial_selected_id,
                label,
            } => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot open tree while processing".to_string());
                    return None;
                }

                let Ok(session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
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
            PiMsg::ExtensionCommandDone {
                command: _,
                display,
                is_error: _,
            } => {
                self.agent_state = AgentState::Idle;
                self.current_tool = None;

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
        // Capability-specific prompts get a dedicated modal overlay.
        if CapabilityPromptOverlay::is_capability_prompt(&request) {
            self.capability_prompt = Some(CapabilityPromptOverlay::from_request(request));
            return None;
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
            self.extension_ui_queue.push_back(request);
            self.advance_extension_ui_queue();
        } else {
            self.apply_extension_ui_effect(&request);
        }
        None
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

    fn advance_extension_ui_queue(&mut self) {
        if self.active_extension_ui.is_some() {
            return;
        }
        if let Some(next) = self.extension_ui_queue.pop_front() {
            if next.method == "custom" {
                self.handle_custom_extension_ui_request(next);
                self.advance_extension_ui_queue();
                return;
            }
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

    fn run_next_pending(&mut self) -> Option<Cmd> {
        loop {
            if self.agent_state != AgentState::Idle {
                return None;
            }
            let next = self.pending_inputs.pop_front()?;

            let cmd = match next {
                PendingInput::Text(text) => self.submit_message(&text),
                PendingInput::Content(content) => self.submit_content(content),
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
            match kind {
                QueuedMessageKind::Steering => queue.push_steering(expanded),
                QueuedMessageKind::FollowUp => queue.push_follow_up(expanded),
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

        let queued_text = all.join("\n\n");
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
            drop(agent_guard);

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
            for message in new_messages {
                session_guard.append_model_message(message);
            }
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

            if let Err(err) = result {
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
                let images_value = serde_json::to_value(&images).unwrap_or(Value::Null);
                let payload = json!({
                    "prompt": text,
                    "images": images_value,
                    "systemPrompt": base_system_prompt.as_deref().unwrap_or(""),
                });
                let response = manager
                    .dispatch_event_with_response(
                        ExtensionEventName::BeforeAgentStart,
                        Some(payload),
                        EXTENSION_EVENT_TIMEOUT_MS,
                    )
                    .await
                    .unwrap_or(None);
                apply_before_agent_start_response(response, Utc::now().timestamp_millis())
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
            let BeforeAgentStartOutcome {
                messages: before_messages,
                system_prompt,
            } = before_start;
            if let Some(prompt) = system_prompt {
                agent_guard.set_system_prompt(Some(prompt));
            } else {
                agent_guard.set_system_prompt(base_system_prompt.clone());
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
            let user_message = ModelMessage::User(UserMessage {
                content: UserContent::Blocks(content_for_agent),
                timestamp: Utc::now().timestamp_millis(),
            });
            let mut prompts = Vec::with_capacity(1 + before_messages.len());
            prompts.push(user_message);
            prompts.extend(before_messages.into_iter().map(ModelMessage::Custom));

            let result = agent_guard
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

            agent_guard.set_system_prompt(base_system_prompt);

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            drop(agent_guard);

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
            for message in new_messages {
                session_guard.append_model_message(message);
            }
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

            if let Err(err) = result {
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

        if let Some(active) = self.active_extension_ui.take() {
            match parse_extension_ui_response(&active, message) {
                Ok(response) => {
                    self.send_extension_ui_response(response);
                    self.advance_extension_ui_queue();
                }
                Err(err) => {
                    self.status_message = Some(err);
                    self.active_extension_ui = Some(active);
                }
            }
            self.input.reset();
            self.input.focus();
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
            if message.starts_with("Agent busy;") || message.starts_with("Session busy;") {
                tracing::debug!(
                    message,
                    "skipping runtime selection sync while submitting input"
                );
            } else {
                self.status_message = Some(message);
                return None;
            }
        }

        let message_owned = message.to_string();
        let (message_without_refs, file_refs) = self.extract_file_references(&message_owned);
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

            let processed = match process_file_arguments(&file_refs, &self.cwd, auto_resize) {
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
            return self.submit_content_with_display(content, &display);
        }
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let extensions = self.extensions.clone();
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

        // Spawn async task to run the agent
        let runtime_handle_for_agent = runtime_handle.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        runtime_handle.spawn(async move {
            let mut message_for_agent = message_for_agent;
            let mut input_images = Vec::new();
            if let Some(manager) = extensions.clone() {
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
            let previous_len = agent_guard.messages().len();

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
            let result = if input_images.is_empty() {
                let ui_stream_batcher_for_events = Arc::clone(&ui_stream_batcher);
                agent_guard
                    .run_with_abort(message_for_agent, Some(abort_signal), move |event| {
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
                    .await
            } else {
                let content_for_agent =
                    build_content_blocks_for_input(&message_for_agent, &input_images);
                let ui_stream_batcher_for_events = Arc::clone(&ui_stream_batcher);
                agent_guard
                    .run_with_content_with_abort(
                        content_for_agent,
                        Some(abort_signal),
                        move |event| {
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
                        },
                    )
                    .await
            };
            flush_ui_stream_batcher_with_backpressure(&ui_stream_batcher).await;

            let new_messages: Vec<crate::model::Message> =
                agent_guard.messages()[previous_len..].to_vec();
            drop(agent_guard);

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
            for message in new_messages {
                session_guard.append_model_message(message);
            }
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

            if let Err(err) = result {
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
mod stream_delta_batcher_tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::config::Config;
    use crate::keybindings::KeyBindings;
    use crate::model::{AssistantMessage, StreamEvent, Usage};
    use crate::provider::{Context, InputType, Model, ModelCost, Provider, StreamOptions};
    use crate::resources::{ResourceCliOptions, ResourceLoader};
    use crate::session::Session;
    use crate::tools::ToolRegistry;
    use asupersync::runtime::RuntimeBuilder;
    use futures::stream;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    struct DummyProvider;

    #[async_trait::async_trait]
    impl Provider for DummyProvider {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn api(&self) -> &'static str {
            "dummy"
        }

        fn model_id(&self) -> &'static str {
            "dummy-model"
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
                Vec::new(),
                event_tx,
                runtime_handle(),
                true,
                false,
                None,
                Some(KeyBindings::new()),
                Vec::new(),
                Usage::default(),
            ),
            event_rx,
        )
    }

    fn build_test_app() -> PiApp {
        let (app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        app
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

    #[derive(Default)]
    struct ContinueProbeState {
        calls: AtomicUsize,
        saw_custom_message: AtomicBool,
        saw_user_message: AtomicBool,
    }

    struct ContinueProbeProvider {
        state: Arc<ContinueProbeState>,
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

        let _ = app.handle_pi_message(PiMsg::EnqueuePendingInput(PendingInput::Continue));

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

        let _ = app.handle_pi_message(PiMsg::EnqueuePendingInput(PendingInput::Continue));

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
    fn conversation_reset_syncs_runtime_model_and_thinking_from_session_header() {
        let (mut app, _event_rx) = build_test_app_with_provider(Arc::new(DummyProvider));
        let mut next = model_entry("openai", "gpt-4o");
        next.model.reasoning = false;
        app.available_models.push(next.clone());

        runtime().block_on(async {
            let cx = Cx::for_request();
            let mut session_guard =
                asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&app.session), &cx)
                    .await
                    .expect("lock session");
            session_guard.header.provider = Some(next.model.provider.clone());
            session_guard.header.model_id = Some(next.model.id.clone());
            session_guard.header.thinking_level = Some("high".to_string());
        });

        let _ = app.handle_pi_message(PiMsg::ConversationReset {
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

        let capability_request = ExtensionUiRequest::new(
            "req-cap",
            "confirm",
            json!({
                "extension_id": "snake",
                "capability": "exec",
                "message": "Needs shell access",
            }),
        )
        .with_extension_id(Some("snake".to_string()));
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
}
