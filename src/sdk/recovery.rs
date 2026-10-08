//! Recovery for every SDK turn entrypoint, including the default FTUI driver.
//!
//! Only the first attempt may append user input. Recovery goes through the
//! session's persisted continuation/transition APIs, never the bare Agent loop.

mod ownership;

use super::{
    AbortHandle, AbortSignal, AgentEvent, AgentSession, AgentSessionHandle, AssistantMessage,
    ContentBlock, Error, FailoverOptions, ImageContent, Message, Result, RpcControlHandle,
    RpcExtensionUiResponse, SessionPromptResult, SessionTransport, SessionTransportEvent,
    StopReason, TextContent, UserContent,
};
use crate::failover::{RetryPolicy, TurnDecision, TurnOutcome, TurnProgress};
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type EventCallback = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// A failed turn save is an admission failure, not just a failed SDK call.
/// Keep the original error (including any provider/tool failure) and share the
/// quarantine with extension completions and every later SDK entrypoint.
fn fence_session_persistence<T>(
    admission: &crate::agent::ProviderAdmissionGate,
    result: Result<T>,
) -> Result<T> {
    if let Err(error) = &result
        && error.is_session_persistence()
    {
        admission.block(error.to_string());
    }
    result
}

/// Complete only the typed stream hook. The generic SDK event stream and
/// extension observation remain unchanged. `make_combined_callback` already
/// forwards explicit terminal MessageUpdates (notably cancellation), so only
/// synthesize a terminal when an assistant MessageEnd had no such update.
#[derive(Default)]
struct StreamTerminalForwarder {
    terminal_seen: AtomicBool,
}

impl StreamTerminalForwarder {
    fn missing_terminal(&self, event: &AgentEvent) -> Option<super::StreamEvent> {
        use crate::model::AssistantMessageEvent;

        match event {
            AgentEvent::MessageStart {
                message: Message::Assistant(_),
            } => {
                self.terminal_seen.store(false, Ordering::SeqCst);
                None
            }
            AgentEvent::MessageUpdate {
                assistant_message_event:
                    AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. },
                ..
            } => {
                self.terminal_seen.store(true, Ordering::SeqCst);
                None
            }
            AgentEvent::MessageEnd {
                message: Message::Assistant(message),
            } => {
                if self.terminal_seen.swap(true, Ordering::SeqCst) {
                    return None;
                }
                let reason = message.stop_reason;
                if matches!(reason, StopReason::Error | StopReason::Aborted) {
                    Some(super::StreamEvent::Error {
                        reason,
                        error: (**message).clone(),
                    })
                } else {
                    Some(super::StreamEvent::Done {
                        reason,
                        message: (**message).clone(),
                    })
                }
            }
            _ => None,
        }
    }
}

/// Response completion is not a durability acknowledgement: the SDK's logical
/// AgentEnd still waits for persistence and may report a save failure after a
/// typed Done. Keep typed hooks ahead of generic subscribers, as in the normal
/// fan-out, and never hold a mutex while calling embedder code.
fn complete_stream_callback(
    output: EventCallback,
    on_stream_event: Option<super::OnStreamEvent>,
) -> EventCallback {
    let Some(on_stream_event) = on_stream_event else {
        return output;
    };
    let terminals = StreamTerminalForwarder::default();
    Arc::new(move |event| {
        if let Some(terminal) = terminals.missing_terminal(&event) {
            on_stream_event(&terminal);
        }
        output(event);
    })
}

/// One logical SDK call owns its terminal event, not any provider attempt.
/// In particular, the raw AgentEnd precedes AgentSession's persistence step.
struct LogicalTurn {
    output: EventCallback,
    state: Arc<Mutex<LogicalTurnState>>,
}

#[derive(Default)]
struct LogicalTurnState {
    session_id: Option<Arc<str>>,
    messages: Vec<Message>,
    retry: Option<u32>,
    fallback: Option<(String, String)>,
    finished: bool,
}

impl LogicalTurnState {
    fn end_retry(&mut self, success: bool, error: Option<String>) -> Option<AgentEvent> {
        self.retry.take().map(|attempt| AgentEvent::AutoRetryEnd {
            success,
            attempt,
            final_error: error,
        })
    }

    fn end_fallback(&mut self, success: bool) -> Option<AgentEvent> {
        self.fallback
            .take()
            .map(|(provider, model)| AgentEvent::FailoverEnd {
                success,
                provider,
                model,
                restored_primary: false,
            })
    }

    fn forget_incomplete_tail(&mut self) {
        while self.messages.last().is_some_and(|message| {
            matches!(
                message,
                Message::Assistant(assistant)
                    if matches!(assistant.stop_reason, StopReason::Error | StopReason::Aborted)
            )
        }) {
            let _ = self.messages.pop();
        }
    }

    /// At most two pending lifecycles close before an incoming event. A fixed
    /// array avoids allocating a Vec for every streamed text delta.
    fn observe(&mut self, mut event: AgentEvent) -> [Option<AgentEvent>; 3] {
        if self.finished {
            return [None, None, None];
        }
        if let AgentEvent::AgentEnd { messages, .. } = event {
            self.messages.extend(messages);
            return [None, None, None];
        }
        let mut retry_end = None;
        let mut fallback_end = None;
        let forward = match &mut event {
            AgentEvent::AgentStart { session_id } => {
                if self.session_id.is_some() {
                    false
                } else {
                    self.session_id = Some(Arc::clone(session_id));
                    true
                }
            }
            AgentEvent::AutoRetryStart {
                attempt,
                error_message,
                ..
            } => {
                retry_end = self.end_retry(false, Some(error_message.clone()));
                self.retry = Some(*attempt);
                true
            }
            AgentEvent::AutoRetryEnd { attempt, .. } => {
                let matches = self.retry == Some(*attempt);
                if matches {
                    self.retry = None;
                }
                matches
            }
            AgentEvent::FailoverStart {
                to_provider,
                to_model,
                ..
            } => {
                retry_end = self.end_retry(false, Some("Retry budget exhausted".to_string()));
                fallback_end = self.end_fallback(false);
                self.fallback = Some((to_provider.clone(), to_model.clone()));
                true
            }
            AgentEvent::FailoverEnd {
                provider,
                model,
                restored_primary: false,
                ..
            } => {
                // Pair the end with the target that its Start announced,
                // even if another runtime mutation changed the live model.
                self.fallback.take().is_some_and(|target| {
                    (*provider, *model) = target;
                    true
                })
            }
            AgentEvent::AutoCompactionEnd {
                aborted: false,
                will_retry: true,
                error_message: None,
                ..
            } => {
                self.forget_incomplete_tail();
                true
            }
            _ => true,
        };
        [retry_end, fallback_end, forward.then_some(event)]
    }
}

impl LogicalTurn {
    fn new(output: EventCallback) -> Self {
        Self {
            output,
            state: Arc::new(Mutex::new(LogicalTurnState::default())),
        }
    }

    fn callback(&self) -> EventCallback {
        let output = Arc::clone(&self.output);
        let state = Arc::clone(&self.state);
        Arc::new(move |event| Self::dispatch(&output, &state, event))
    }

    fn emit(&self, event: AgentEvent) {
        Self::dispatch(&self.output, &self.state, event);
    }

    fn dispatch(output: &EventCallback, state: &Mutex<LogicalTurnState>, event: AgentEvent) {
        let events = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observe(event);
        // Never invoke user callbacks or subscribers while holding state.
        for event in events.into_iter().flatten() {
            output(event);
        }
    }

    fn restored_tail(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forget_incomplete_tail();
    }

    fn finish(&self, outcome: &Result<AssistantMessage>) {
        let error = match outcome {
            Ok(message) if message.stop_reason == StopReason::Error => Some(
                message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Request error".to_string()),
            ),
            Ok(message) if message.stop_reason == StopReason::Aborted => Some(
                message
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Aborted".to_string()),
            ),
            Ok(_) => None,
            Err(error) => Some(error.to_string()),
        };
        let events = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.finished {
                return;
            }
            state.finished = true;
            let success = error.is_none();
            let retry_end = state.end_retry(success, error.clone());
            let fallback_end = state.end_fallback(success);
            let terminal = state
                .session_id
                .take()
                .map(|session_id| AgentEvent::AgentEnd {
                    session_id,
                    messages: std::mem::take(&mut state.messages),
                    error,
                });
            drop(state);
            [retry_end, fallback_end, terminal]
        };
        for event in events.into_iter().flatten() {
            (self.output)(event);
        }
    }
}

impl AgentSessionHandle {
    /// Send one user prompt, applying this handle's configured retry/failover policy.
    ///
    /// Per-prompt callbacks, session subscribers and typed hooks share one event
    /// fan-out. With no retry policy the first outcome is returned unchanged.
    /// A session-persistence error fences this handle: start a new or resumed
    /// session before issuing another prompt, even after repairing the save path.
    /// Dropping an admitted prompt future also fences the handle: it cannot run
    /// its final save. To cancel and keep using the handle, signal its abort and
    /// await the outcome so completed tool work can be persisted.
    pub async fn prompt(
        &mut self,
        input: impl Into<String>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let (_handle, signal) = AbortHandle::new();
        self.prompt_with_abort(input, signal, on_event).await
    }

    /// Send one user prompt with an explicit abort signal.
    ///
    /// A signal already aborted at entry performs no startup synchronization,
    /// primary restoration, prompt append or provider call.
    pub async fn prompt_with_abort(
        &mut self,
        input: impl Into<String>,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_recoverable_turn(
            Some(UserContent::Text(input.into())),
            abort_signal,
            on_event,
        )
        .await
    }

    /// Send text and image attachments as one recoverable user prompt.
    ///
    /// Images contain base64-encoded data and a MIME type; this method does not
    /// read files or resolve URLs. An empty text with nonempty images is an
    /// image-only prompt. Empty images use the ordinary text path unchanged.
    ///
    /// Input extensions receive both text and images. The session persists the
    /// resulting user message once, and retries/failovers resume that history
    /// instead of re-appending or re-running the input hooks. Image blocking
    /// and the active model's image capability remain enforced by the Agent.
    pub async fn prompt_with_images(
        &mut self,
        input: impl Into<String>,
        images: Vec<ImageContent>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let (_handle, signal) = AbortHandle::new();
        self.prompt_with_images_with_abort(input, images, signal, on_event)
            .await
    }

    /// Send image attachments with the same cancellation and durable recovery
    /// boundaries as [`Self::prompt_with_abort`].
    pub async fn prompt_with_images_with_abort(
        &mut self,
        input: impl Into<String>,
        images: Vec<ImageContent>,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        ensure_not_aborted(&abort_signal)?;
        let text = input.into();
        let content = if images.is_empty() {
            UserContent::Text(text)
        } else {
            let mut blocks = Vec::with_capacity(images.len().saturating_add(1));
            if !text.is_empty() {
                blocks.push(ContentBlock::Text(TextContent::new(text)));
            }
            blocks.extend(images.into_iter().map(ContentBlock::Image));
            UserContent::Blocks(blocks)
        };
        self.run_recoverable_turn(Some(content), abort_signal, on_event)
            .await
    }

    /// Send native text, image, audio and video blocks as one recoverable prompt.
    ///
    /// Blocks retain their order and payloads. Input hooks can transform text
    /// and images while audio/video blocks remain attached. This method does
    /// not read paths or fetch URLs; provider-specific media transport and
    /// unsupported-media degradation use the ordinary Agent/provider boundary.
    /// The accepted user message is persisted once and reused on recovery.
    pub async fn prompt_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let (_handle, signal) = AbortHandle::new();
        self.prompt_with_content_with_abort(content, signal, on_event)
            .await
    }

    /// Send native content with explicit cancellation and this handle's
    /// configured retry/failover policy. Assistant-only blocks are rejected.
    pub async fn prompt_with_content_with_abort(
        &mut self,
        content: Vec<ContentBlock>,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_recoverable_turn(Some(UserContent::Blocks(content)), abort_signal, on_event)
            .await
    }

    /// Continue without synthesizing a user message, with the same recovery,
    /// persistence and provider-admission rules as [`Self::prompt`].
    pub async fn continue_turn(
        &mut self,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        let (_handle, signal) = AbortHandle::new();
        self.continue_turn_with_abort(signal, on_event).await
    }

    /// Continue with an explicit abort signal and this handle's recovery policy.
    pub async fn continue_turn_with_abort(
        &mut self,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        self.run_recoverable_turn(None, abort_signal, on_event)
            .await
    }

    pub(crate) async fn run_recoverable_turn(
        &mut self,
        input: Option<UserContent>,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        ensure_not_aborted(&abort_signal)?;
        self.session.ensure_provider_reentry_allowed()?;
        if let Some(UserContent::Blocks(blocks)) = &input
            && let Err(error) = AgentSession::validate_user_content_blocks(blocks)
        {
            // Rejected input consumes its one-shot keyword provenance just
            // like the AgentSession validation boundary below.
            self.session.agent.set_magic_keyword_scan_override(None);
            return Err(error);
        }
        self.sync_extension_mcp_registrations().await;
        ensure_not_aborted(&abort_signal)?;
        self.session.ensure_provider_reentry_allowed()?;

        // Construct this once for the entire call. Recovery events must reach
        // subscribers too, and a resumed attempt must not fan out a second time.
        let shared: EventCallback = Arc::new(self.make_combined_callback(on_event));
        let shared = complete_stream_callback(shared, self.listeners.on_stream_event.clone());
        self.maybe_restore_primary(&shared).await?;
        ensure_not_aborted(&abort_signal)?;
        let turn = LogicalTurn::new(shared);
        let _ownership = ownership::TurnGuard::new(&turn, self.session.provider_admission_gate());
        let shared = turn.callback();
        let forwarded = Arc::clone(&shared);
        // Hooks run only for the first prompt. Keep their effective base in
        // this logical turn so retries and provider swaps reuse it after the
        // individual attempt has restored the session's ordinary prompt.
        let mut turn_system_prompt = self.session.agent.system_prompt().map(str::to_string);
        let first = match input {
            Some(UserContent::Text(input)) => {
                self.session
                    .run_text_with_abort_capturing_prompt(
                        input,
                        Some(abort_signal.clone()),
                        &mut turn_system_prompt,
                        move |event| forwarded(event),
                    )
                    .await
            }
            Some(UserContent::Blocks(content)) => {
                self.session
                    .run_with_content_with_abort_capturing_prompt(
                        content,
                        Some(abort_signal.clone()),
                        &mut turn_system_prompt,
                        move |event| forwarded(event),
                    )
                    .await
            }
            None => {
                self.session
                    .run_continue_with_abort_and_system_prompt(
                        Some(abort_signal.clone()),
                        turn_system_prompt.as_deref(),
                        move |event| forwarded(event),
                    )
                    .await
            }
        };
        let admission = self.session.provider_admission_gate();
        let first = fence_session_persistence(&admission, first);
        let result = self
            .apply_retry_policy(
                first,
                &abort_signal,
                turn_system_prompt.as_deref(),
                &shared,
                &turn,
            )
            .await;
        // Also cover failures while preparing a retry/failover. Install the
        // fence before observers receive the logical turn's terminal event.
        let result = fence_session_persistence(&admission, result);
        turn.finish(&result);
        result
    }

    /// Install (or clear) fallback-chain configuration on a pre-built handle.
    #[must_use]
    pub fn with_failover(mut self, options: Option<FailoverOptions>) -> Self {
        self.failover_state = options
            .as_ref()
            .map_or_else(crate::failover::FailoverState::new_empty, |options| {
                crate::failover::FailoverState::with_cooldown_secs(options.cooldown_secs)
            });
        self.failover = options.map(|mut options| {
            self.session.scope_auth_storage(&mut options.auth);
            Arc::new(options)
        });
        self
    }

    /// Install (or clear) the recovery policy used by every turn entrypoint.
    #[must_use]
    pub const fn with_retry(mut self, policy: Option<RetryPolicy>) -> Self {
        self.retry = policy;
        self
    }

    /// Extension selections and branch navigation update Session state without
    /// going through the SDK's explicit model-selection method. Treat the active
    /// branch's provenance as authoritative before consulting cached recovery
    /// state. Callers hold provider and session-action admission through the
    /// ensuing swap, in that order, so provider callbacks can finish first.
    /// Returns false when the branch has selected a model that the runtime has
    /// not installed yet; an error from the old model cannot replace that choice.
    async fn reconcile_failover_state(
        &mut self,
        configured_cooldown_secs: u64,
        cx: &crate::agent_cx::AgentCx,
    ) -> Result<bool> {
        let session = self
            .session
            .session
            .lock(cx.cx())
            .await
            .map_err(|err| Error::session(format!("failover provenance lock failed: {err}")))?;
        let runtime = self.session.agent.provider();
        let runtime_matches_session =
            session
                .effective_model_for_current_path()
                .is_none_or(|(provider, model)| {
                    crate::provider_metadata::provider_ids_match(runtime.name(), &provider)
                        && runtime.model_id().eq_ignore_ascii_case(&model)
                });
        self.failover_state.reconcile_from_session(
            &session,
            configured_cooldown_secs,
            chrono::Utc::now(),
        );
        Ok(runtime_matches_session)
    }

    /// A chain belongs to its original primary, not the currently installed
    /// fallback. Candidate preparation and durable installation remain shared
    /// with print and RPC in `AgentSession::try_failover`.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn try_chain_failover(
        &mut self,
        current: &Result<AssistantMessage>,
        require_incomplete_tail: bool,
        retry_attempt_to_end: Option<u32>,
        swap_attempt: u32,
        shared: &EventCallback,
    ) -> Result<bool> {
        let Some(options) = self.failover.clone() else {
            return Ok(false);
        };
        let Some(error_text) = Self::turn_error_text_for(current) else {
            return Ok(false);
        };
        let Some(class) = crate::failover::classify_failover(&error_text).or_else(|| {
            // A typed transport drop need not spell its kind in Display.
            // Reuse the same refusal-aware typed classifier as the decision.
            current
                .as_ref()
                .err()
                .filter(|error| crate::failover::call_error_is_retryable(error))
                .map(|_| crate::failover::FailoverClass::Transient)
        }) else {
            return Ok(false);
        };
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let admission = self.session.provider_admission_gate();
        let provider_authority = admission.acquire_transition_authority(cx.cx()).await?;
        let session_actions = self.session.session_action_admission_gate();
        let session_action_permit = session_actions.acquire(cx.cx()).await?;
        if !self
            .reconcile_failover_state(options.cooldown_secs, &cx)
            .await?
        {
            return Ok(false);
        }
        let live = {
            let provider = self.session.agent.provider();
            crate::failover::FailoverPrimary {
                provider: provider.name().to_string(),
                model_id: provider.model_id().to_string(),
                requested_thinking_level: self
                    .session
                    .agent
                    .stream_options()
                    .thinking_level
                    .unwrap_or_default(),
            }
        };
        let primary = self.failover_state.primary_for_swap(live);
        let Some(chain) = crate::failover::chain_for(
            &options.chains,
            "default",
            &primary.provider,
            &primary.model_id,
        ) else {
            return Ok(false);
        };
        admission.ensure_allowed()?;
        // The first durable record and the in-memory state must carry the
        // same identity. Generating this after persistence gives them two
        // different UUIDs because the session candidate supplies its own.
        let lifecycle_id = self
            .failover_state
            .lifecycle_id()
            .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
        let attempt = crate::agent::FailoverSwapAttempt {
            chain: &chain,
            start_position: self.failover_state.chain_position(),
            available_models: &options.available_models,
            auth: &options.auth,
            cli_api_key: options.cli_api_key.as_deref(),
            class,
            thinking_level_to_clamp: primary.requested_thinking_level,
            require_incomplete_tail,
            primary: Some(&primary),
            cooldown_secs: Some(options.cooldown_secs),
            lifecycle_id: Some(&lifecycle_id),
        };
        let outcome = self
            .session
            .try_failover_with_authority(&cx, &attempt, &provider_authority)
            .await?;
        admission.ensure_allowed()?;
        let Some(committed) = outcome.committed else {
            // An uncredentialed candidate may become usable on a later turn.
            // Exhaustion alone must not permanently advance the stored cursor.
            return Ok(false);
        };
        self.failover_state.set_lifecycle_id(Some(lifecycle_id));
        self.failover_state
            .set_chain_position(outcome.next_position);
        self.failover_state.record_swap(
            primary,
            (committed.to_provider.clone(), committed.to_model.clone()),
            Instant::now(),
        );
        drop(session_action_permit);
        drop(provider_authority);
        if let Some(attempt) = retry_attempt_to_end {
            shared(AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(error_text),
            });
        }
        shared(AgentEvent::FailoverStart {
            from_provider: committed.from_provider,
            from_model: committed.from_model,
            to_provider: committed.to_provider,
            to_model: committed.to_model,
            class: format!("{class:?}").to_ascii_lowercase(),
            attempt: swap_attempt,
            chain_index: u32::try_from(committed.entry_index).unwrap_or(u32::MAX),
        });
        Ok(true)
    }

    fn close_failover_lifecycle(&self, failed_over: bool, success: bool, shared: &EventCallback) {
        if failed_over {
            let provider = self.session.agent.provider();
            shared(AgentEvent::FailoverEnd {
                success,
                provider: provider.name().to_string(),
                model: provider.model_id().to_string(),
                restored_primary: false,
            });
        }
    }

    /// Restore only between public calls, never between attempts of one turn.
    /// A declined candidate keeps the fallback; a failed durable transition is
    /// an error, not permission to enter another provider on uncertain state.
    pub(super) async fn maybe_restore_primary(&mut self, shared: &EventCallback) -> Result<()> {
        let Some(options) = self.failover.clone() else {
            return Ok(());
        };
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let admission = self.session.provider_admission_gate();
        let provider_authority = admission.acquire_transition_authority(cx.cx()).await?;
        let session_actions = self.session.session_action_admission_gate();
        let session_action_permit = session_actions.acquire(cx.cx()).await?;
        if !self
            .reconcile_failover_state(options.cooldown_secs, &cx)
            .await?
        {
            return Ok(());
        }
        let Some(active) = self.failover_state.active().cloned() else {
            return Ok(());
        };
        let Some(primary) = self.failover_state.primary().cloned() else {
            return Ok(());
        };
        let request = crate::agent::PrimaryRestoreRequest {
            primary: &primary,
            active: &active,
            cooldown_elapsed: self.failover_state.should_restore_primary(Instant::now()),
            available_models: &options.available_models,
            auth: &options.auth,
            cli_api_key: options.cli_api_key.as_deref(),
            strict_invariants: false,
            invalidate_background_compaction: true,
        };
        admission.ensure_allowed()?;
        let restored = self
            .session
            .restore_primary_with_authority(&cx, &request, &provider_authority)
            .await?;
        // Lenient restoration may decline an unavailable primary, but it may
        // never turn an indeterminate save into permission to keep issuing.
        // The transition guard quarantines interrupted saves as well.
        admission.ensure_allowed()?;
        let Some(restored) = restored else {
            return Ok(());
        };
        self.failover_state.clear();
        drop(session_action_permit);
        drop(provider_authority);
        shared(AgentEvent::FailoverEnd {
            success: true,
            provider: restored.provider,
            model: restored.model,
            restored_primary: true,
        });
        Ok(())
    }

    fn turn_error_text_for(outcome: &Result<AssistantMessage>) -> Option<String> {
        match outcome {
            Ok(message) => message.error_message.clone(),
            Err(error) => Some(error.to_string()),
        }
    }

    async fn await_retry_backoff(delay_ms: u32, abort_signal: &AbortSignal) -> bool {
        const POLL: Duration = Duration::from_millis(25);
        let deadline = Instant::now() + Duration::from_millis(u64::from(delay_ms));
        loop {
            if abort_signal.is_aborted() {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            asupersync::time::sleep(asupersync::time::wall_now(), POLL.min(remaining)).await;
        }
    }

    fn finish_recovery(
        &self,
        current: &Result<AssistantMessage>,
        retry_count: u32,
        failed_over: bool,
        shared: &EventCallback,
    ) {
        let success = matches!(current, Ok(message) if !matches!(
            message.stop_reason, StopReason::Error | StopReason::Aborted
        ));
        if retry_count > 0 {
            shared(AgentEvent::AutoRetryEnd {
                success,
                attempt: retry_count,
                final_error: Self::turn_error_text_for(current),
            });
        }
        self.close_failover_lifecycle(failed_over, success, shared);
    }

    async fn resume_recovery_attempt(
        &mut self,
        abort_signal: &AbortSignal,
        turn_system_prompt: Option<&str>,
        shared: &EventCallback,
    ) -> Result<AssistantMessage> {
        ensure_not_aborted(abort_signal)?;
        let forwarded = Arc::clone(shared);
        let result = self
            .session
            .run_continue_with_abort_and_system_prompt(
                Some(abort_signal.clone()),
                turn_system_prompt,
                move |event| forwarded(event),
            )
            .await;
        // A continuation can finish its provider work but fail its own save.
        // Fence before retry/failover terminal callbacks are dispatched, not
        // only after the outer SDK call returns.
        fence_session_persistence(&self.session.provider_admission_gate(), result)
    }

    #[allow(clippy::too_many_lines)]
    async fn apply_retry_policy(
        &mut self,
        first: Result<AssistantMessage>,
        abort_signal: &AbortSignal,
        turn_system_prompt: Option<&str>,
        shared: &EventCallback,
        turn: &LogicalTurn,
    ) -> Result<AssistantMessage> {
        let Some(policy) = self.retry else {
            return first;
        };
        let mut progress = TurnProgress {
            retry_count: 0,
            failovers_this_turn: 0,
            stream_can_retry: true,
        };
        let mut failed_over = false;
        let mut current = first;
        loop {
            // Re-resolve after every committed swap. Compaction settings may
            // be overridden by an embedder and are not model-capacity evidence.
            let window = self
                .session
                .current_model_entry()
                .map(|entry| entry.model.context_window)
                .filter(|window| *window > 0);
            let outcome = match &current {
                Ok(message) => TurnOutcome::Completed(message),
                Err(error) => TurnOutcome::Failed(error),
            };
            let decision = crate::failover::decide(outcome, &progress, &policy, window);
            if matches!(
                decision,
                TurnDecision::Retry { .. } | TurnDecision::FailOver
            ) && abort_signal.is_aborted()
            {
                current = Err(Error::Aborted);
                self.finish_recovery(&current, progress.retry_count, failed_over, shared);
                return current;
            }

            if decision == TurnDecision::FailOver {
                match self
                    .try_chain_failover(
                        &current,
                        current.is_ok(),
                        (progress.retry_count > 0).then_some(progress.retry_count),
                        progress.failovers_this_turn.saturating_add(1),
                        shared,
                    )
                    .await
                {
                    Ok(true) => {
                        turn.restored_tail();
                        failed_over = true;
                        progress.failovers_this_turn += 1;
                        progress.retry_count = 0;
                        current = self
                            .resume_recovery_attempt(abort_signal, turn_system_prompt, shared)
                            .await;
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        current = Err(error);
                        self.finish_recovery(&current, progress.retry_count, failed_over, shared);
                        return current;
                    }
                }
            }

            let TurnDecision::Retry { attempt, delay_ms } = decision else {
                self.finish_recovery(&current, progress.retry_count, failed_over, shared);
                return current;
            };
            if let Err(error) = self
                .prepare_same_provider_retry(
                    &current,
                    attempt,
                    delay_ms,
                    policy,
                    abort_signal,
                    turn,
                )
                .await
            {
                // Preparation has already closed the retry it announced.
                self.close_failover_lifecycle(failed_over, false, shared);
                return Err(error);
            }
            progress.retry_count = attempt;
            current = self
                .resume_recovery_attempt(abort_signal, turn_system_prompt, shared)
                .await;
        }
    }

    async fn prepare_same_provider_retry(
        &mut self,
        current: &Result<AssistantMessage>,
        attempt: u32,
        delay_ms: u32,
        policy: RetryPolicy,
        abort_signal: &AbortSignal,
        turn: &LogicalTurn,
    ) -> Result<()> {
        turn.emit(AgentEvent::AutoRetryStart {
            attempt,
            max_attempts: policy.max_retries,
            delay_ms: u64::from(delay_ms),
            error_message: Self::turn_error_text_for(current)
                .unwrap_or_else(|| "Request error".to_string()),
        });
        let result = async {
            if !Self::await_retry_backoff(delay_ms, abort_signal).await {
                return Err(Error::Aborted);
            }
            let cx = crate::agent_cx::AgentCx::for_current_or_request();
            let admission = self.session.provider_admission_gate();
            admission.ensure_allowed()?;
            self.session
                .restore_retry_tail_with_admission(&cx, current.is_ok(), Some(&admission))
                .await?;
            admission.ensure_allowed()?;
            turn.restored_tail();
            // Do not race cancellation against a durability operation: let it
            // settle, then refuse provider re-entry when the signal was raised.
            ensure_not_aborted(abort_signal)
        }
        .await;
        if let Err(error) = &result {
            turn.emit(AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(error.to_string()),
            });
        }
        result
    }
}

impl SessionTransport {
    /// Send ordered native text, image, audio, and video content over either
    /// session transport.
    ///
    /// In-process sessions use [`AgentSessionHandle::prompt_with_content`],
    /// including its durable retry and failover behavior. Subprocess sessions
    /// send the RPC `content` field without flattening or regrouping blocks and
    /// deliver acknowledged events live through the same callback contract as
    /// [`Self::prompt_with_images`]. Empty or assistant-only input is rejected
    /// before the session is advanced or a subprocess request is dispatched.
    pub async fn prompt_with_content(
        &mut self,
        content: Vec<ContentBlock>,
        on_event: impl Fn(SessionTransportEvent) + Send + Sync + 'static,
    ) -> Result<SessionPromptResult> {
        match self {
            Self::InProcess(handle) => {
                let assistant = handle
                    .prompt_with_content(content, move |event| {
                        on_event(SessionTransportEvent::InProcess(Box::new(event)));
                    })
                    .await?;
                Ok(SessionPromptResult::InProcess(Box::new(assistant)))
            }
            Self::RpcSubprocess(client) => {
                let events = client
                    .prompt_with_content_streaming(content, None, move |event| {
                        on_event(SessionTransportEvent::Rpc(event));
                    })
                    .await?;
                Ok(SessionPromptResult::RpcEvents(events))
            }
        }
    }

    /// Send text and image attachments over either session transport.
    ///
    /// In-process sessions use the same durable, recoverable path as
    /// [`AgentSessionHandle::prompt_with_images`]. Subprocess sessions send the
    /// RPC protocol's `images` field and deliver events as they arrive, rather
    /// than flattening the prompt to text or waiting to replay its events.
    /// Cancellation of an RPC prompt remains available through
    /// [`super::RpcTransportClient::control_handle`].
    pub async fn prompt_with_images(
        &mut self,
        input: impl Into<String>,
        images: Vec<ImageContent>,
        on_event: impl Fn(SessionTransportEvent) + Send + Sync + 'static,
    ) -> Result<SessionPromptResult> {
        let input = input.into();
        match self {
            Self::InProcess(handle) => {
                let assistant = handle
                    .prompt_with_images(input, images, move |event| {
                        on_event(SessionTransportEvent::InProcess(Box::new(event)));
                    })
                    .await?;
                Ok(SessionPromptResult::InProcess(Box::new(assistant)))
            }
            Self::RpcSubprocess(client) => {
                let images = (!images.is_empty()).then_some(images);
                let events = client
                    .prompt_with_options_streaming(input, images, None, move |event| {
                        on_event(SessionTransportEvent::Rpc(event));
                    })
                    .await?;
                Ok(SessionPromptResult::RpcEvents(events))
            }
        }
    }
}

impl RpcControlHandle {
    /// Answer an extension's UI request while an RPC prompt is still streaming.
    ///
    /// Clone the control handle before starting the prompt, then use it from
    /// the live event callback or a separate UI thread. The prompt remains the
    /// only stdout reader. Echo the exact request ID and `requestGeneration`
    /// from the response-bearing `extension_ui_request`; this method neither
    /// selects a default answer nor substitutes the current generation.
    ///
    /// The returned ID identifies the dispatched command, not the UI request.
    /// Success means the JSON line was written and flushed, not that a pending
    /// request was resolved. The server retains responsibility for rejecting
    /// stale generations and expired requests. Use the client's async method
    /// when idle and an acknowledged `resolved` result is required.
    pub fn extension_ui_response(
        &self,
        request_id: &str,
        request_generation: u64,
        response: RpcExtensionUiResponse,
    ) -> Result<String> {
        let mut payload = rpc_ui_response_payload(request_id)?;
        payload.insert(
            "requestGeneration".to_string(),
            Value::from(request_generation),
        );
        match response {
            RpcExtensionUiResponse::Value { value } => {
                payload.insert("value".to_string(), value);
            }
            RpcExtensionUiResponse::Confirmed { confirmed } => {
                payload.insert("confirmed".to_string(), Value::Bool(confirmed));
            }
            RpcExtensionUiResponse::Cancelled => {
                payload.insert("cancelled".to_string(), Value::Bool(true));
            }
        }
        self.send("extension_ui_response", payload)
    }

    /// Answer or dismiss a live `ask_request` without borrowing the prompt's
    /// stdout reader. This also supports host permission cards that use `ask`.
    ///
    /// Answers retain their exact question IDs, selected labels and free text.
    /// Explicit dismissal takes precedence and sends no stale answers. No
    /// recommended option or approval is chosen automatically. The server
    /// validates answers against the pending request and its timeout.
    ///
    /// Like [`Self::extension_ui_response`], returns a dispatch ID after the
    /// serialized writer flushes; it does not await an acknowledgement.
    pub fn ask_response(
        &self,
        request_id: &str,
        response: crate::ask::AskResponse,
    ) -> Result<String> {
        let mut payload = rpc_ui_response_payload(request_id)?;
        if response.dismissed {
            payload.insert("dismissed".to_string(), Value::Bool(true));
        } else {
            payload.insert(
                "answers".to_string(),
                serde_json::to_value(response.answers)
                    .map_err(|error| Error::Json(Box::new(error)))?,
            );
        }
        self.send("ask_response", payload)
    }
}

/// Keep UI correlation distinct from the transport's separately allocated ID.
/// Reject missing correlation before consuming an ID or touching the pipe.
fn rpc_ui_response_payload(request_id: &str) -> Result<Map<String, Value>> {
    if request_id.trim().is_empty() {
        return Err(Error::validation(
            "RPC UI response requires a nonempty request ID",
        ));
    }
    let mut payload = Map::new();
    payload.insert(
        "requestId".to_string(),
        Value::String(request_id.to_string()),
    );
    Ok(payload)
}

fn ensure_not_aborted(signal: &AbortSignal) -> Result<()> {
    if signal.is_aborted() {
        Err(Error::Aborted)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod persistence_fence_tests {
    use super::*;
    use crate::agent::ProviderAdmissionGate;

    #[test]
    fn persistence_failure_fences_shared_admission_and_preserves_the_error() {
        let admission = ProviderAdmissionGate::default();
        let shared = admission.clone();
        let error = Error::session_persistence(
            "disk flush failed; primary provider/tool turn also failed: connection lost",
        );
        let expected = error.to_string();
        let result = fence_session_persistence::<()>(&admission, Err(error));
        let returned = result.expect_err("preserve the failed save");
        assert!(returned.is_session_persistence());
        assert_eq!(returned.to_string(), expected);
        let refused = shared.ensure_allowed().expect_err("shared gate must fence");
        assert!(refused.is_session_persistence());
        assert!(refused.to_string().contains("disk flush failed"));
        assert!(refused.to_string().contains("connection lost"));
    }

    #[test]
    fn provider_errors_and_cancellation_do_not_become_persistence_failures() {
        for error in [
            Error::api("503 service unavailable"),
            Error::session("ordinary session validation failed"),
            Error::Aborted,
        ] {
            let admission = ProviderAdmissionGate::default();
            let expected = error.to_string();
            let returned = fence_session_persistence::<()>(&admission, Err(error))
                .expect_err("original error");
            assert!(!returned.is_session_persistence());
            assert_eq!(returned.to_string(), expected);
            assert!(admission.ensure_allowed().is_ok());
        }
    }

    #[test]
    fn successful_results_do_not_close_admission() {
        let admission = ProviderAdmissionGate::default();
        assert_eq!(fence_session_persistence(&admission, Ok(42)).unwrap(), 42);
        assert!(admission.ensure_allowed().is_ok());
    }

    #[test]
    fn a_later_success_cannot_clear_an_existing_persistence_fence() {
        let admission = ProviderAdmissionGate::default();
        let _ = fence_session_persistence::<()>(
            &admission,
            Err(Error::session_persistence("indeterminate save")),
        );
        let reason = admission.reason();
        fence_session_persistence(&admission, Ok(())).unwrap();
        assert_eq!(admission.reason(), reason);
        assert!(admission.ensure_allowed().is_err());
    }

    #[test]
    fn terminal_observers_see_the_fence_before_the_logical_turn_ends() {
        let admission = ProviderAdmissionGate::default();
        let observed = admission.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let turn = LogicalTurn::new(Arc::new(move |event| {
            if matches!(&event, AgentEvent::AgentEnd { .. }) {
                assert!(
                    observed
                        .ensure_allowed()
                        .is_err_and(|error| error.is_session_persistence())
                );
            }
            captured.lock().unwrap().push(event);
        }));
        turn.emit(AgentEvent::AgentStart {
            session_id: Arc::from("persistence-fence-test"),
        });
        let result = fence_session_persistence::<AssistantMessage>(
            &admission,
            Err(Error::session_persistence("turn save failed")),
        );
        turn.finish(&result);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events.last(),
            Some(AgentEvent::AgentEnd { error: Some(error), .. })
                if error.contains("turn save failed")
        ));
    }
}

#[cfg(test)]
mod stream_terminal_tests {
    use super::*;
    use crate::model::{AssistantMessageEvent, StreamEvent, Usage, UserMessage};

    fn assistant(reason: StopReason) -> Arc<AssistantMessage> {
        Arc::new(AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new("provider output"))],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: reason,
            stop_details: None,
            error_message: matches!(reason, StopReason::Error | StopReason::Aborted)
                .then(|| "terminal diagnostic".to_string()),
            timestamp: 123,
        })
    }

    fn start(message: &Arc<AssistantMessage>) -> AgentEvent {
        AgentEvent::MessageStart {
            message: Message::Assistant(Arc::clone(message)),
        }
    }

    fn end(message: &Arc<AssistantMessage>) -> AgentEvent {
        AgentEvent::MessageEnd {
            message: Message::Assistant(Arc::clone(message)),
        }
    }

    #[test]
    fn success_terminals_keep_stop_reason_and_complete_message_payload() {
        for reason in [
            StopReason::Stop,
            StopReason::Length,
            StopReason::ToolUse,
            StopReason::PauseTurn,
        ] {
            let forwarder = StreamTerminalForwarder::default();
            let message = assistant(reason);
            assert!(forwarder.missing_terminal(&start(&message)).is_none());
            let Some(StreamEvent::Done {
                reason: actual_reason,
                message: actual,
            }) = forwarder.missing_terminal(&end(&message))
            else {
                panic!("expected Done for {reason:?}");
            };
            assert_eq!(actual_reason, reason);
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(message.as_ref()).unwrap()
            );
            assert!(forwarder.missing_terminal(&end(&message)).is_none());
        }
    }

    #[test]
    fn error_and_abort_terminals_are_not_reported_as_success() {
        for reason in [StopReason::Error, StopReason::Aborted] {
            let forwarder = StreamTerminalForwarder::default();
            let message = assistant(reason);
            assert!(forwarder.missing_terminal(&start(&message)).is_none());
            let Some(StreamEvent::Error {
                reason: actual_reason,
                error,
            }) = forwarder.missing_terminal(&end(&message))
            else {
                panic!("expected Error for {reason:?}");
            };
            assert_eq!(actual_reason, reason);
            assert_eq!(
                serde_json::to_value(error).unwrap(),
                serde_json::to_value(message.as_ref()).unwrap()
            );
        }
    }

    #[test]
    fn explicit_terminals_are_left_to_the_existing_fanout_without_duplication() {
        for reason in [StopReason::Stop, StopReason::Error, StopReason::Aborted] {
            let forwarder = StreamTerminalForwarder::default();
            let message = assistant(reason);
            assert!(forwarder.missing_terminal(&start(&message)).is_none());
            let terminal = if reason == StopReason::Stop {
                AssistantMessageEvent::Done {
                    reason,
                    message: Arc::clone(&message),
                }
            } else {
                AssistantMessageEvent::Error {
                    reason,
                    error: Arc::clone(&message),
                }
            };
            let update = AgentEvent::MessageUpdate {
                message: Message::Assistant(Arc::clone(&message)),
                assistant_message_event: terminal,
            };
            assert!(forwarder.missing_terminal(&update).is_none());
            assert!(forwarder.missing_terminal(&end(&message)).is_none());
        }
    }

    #[test]
    fn each_retry_and_tool_continuation_has_its_own_terminal() {
        let forwarder = StreamTerminalForwarder::default();
        for reason in [StopReason::Error, StopReason::ToolUse, StopReason::Stop] {
            let message = assistant(reason);
            assert!(forwarder.missing_terminal(&start(&message)).is_none());
            assert!(forwarder.missing_terminal(&end(&message)).is_some());
            let user_start = AgentEvent::MessageStart {
                message: Message::User(UserMessage {
                    content: UserContent::Text("not an assistant response".to_string()),
                    timestamp: 0,
                }),
            };
            assert!(forwarder.missing_terminal(&user_start).is_none());
            assert!(forwarder.missing_terminal(&end(&message)).is_none());
        }
    }

    #[test]
    fn content_deltas_are_not_replayed_or_mistaken_for_terminals() {
        let forwarder = StreamTerminalForwarder::default();
        let message = assistant(StopReason::Stop);
        assert!(forwarder.missing_terminal(&start(&message)).is_none());
        let delta = AgentEvent::MessageUpdate {
            message: Message::Assistant(Arc::clone(&message)),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "provider output".to_string(),
                partial: Arc::clone(&message),
            },
        };
        assert!(forwarder.missing_terminal(&delta).is_none());
        assert!(matches!(
            forwarder.missing_terminal(&end(&message)),
            Some(StreamEvent::Done { .. })
        ));
    }

    #[test]
    fn typed_terminal_precedes_generic_delivery_without_changing_the_event() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let generic_events = Arc::new(Mutex::new(Vec::new()));
        let generic_order = Arc::clone(&order);
        let captured = Arc::clone(&generic_events);
        let output: EventCallback = Arc::new(move |event| {
            generic_order.lock().unwrap().push("generic");
            captured
                .lock()
                .unwrap()
                .push(serde_json::to_value(event).unwrap());
        });
        let typed_order = Arc::clone(&order);
        let hook: super::super::OnStreamEvent = Arc::new(move |event| {
            assert!(matches!(event, StreamEvent::Done { .. }));
            typed_order.lock().unwrap().push("typed");
        });
        let callback = complete_stream_callback(output, Some(hook));
        let message = assistant(StopReason::Stop);
        let started = start(&message);
        let ended = end(&message);
        let expected = vec![
            serde_json::to_value(&started).unwrap(),
            serde_json::to_value(&ended).unwrap(),
        ];
        callback(started);
        callback(ended);
        assert_eq!(*order.lock().unwrap(), ["generic", "typed", "generic"]);
        assert_eq!(*generic_events.lock().unwrap(), expected);
    }

    #[test]
    fn absent_stream_listener_reuses_the_existing_callback() {
        let output: EventCallback = Arc::new(|_| {});
        let wrapped = complete_stream_callback(Arc::clone(&output), None);
        assert!(Arc::ptr_eq(&wrapped, &output));
    }
}
