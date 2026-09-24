//! Recovery for every SDK turn entrypoint, including the default FTUI driver.
//!
//! Only the first attempt may append user input. Recovery goes through the
//! session's persisted continuation/transition APIs, never the bare Agent loop.

use super::{
    AbortHandle, AbortSignal, AgentEvent, AgentSessionHandle, AssistantMessage, ContentBlock,
    Error, FailoverOptions, ImageContent, Message, Result, RpcControlHandle,
    RpcExtensionUiResponse, SessionPromptResult, SessionTransport, SessionTransportEvent,
    StopReason, TextContent, UserContent,
};
use crate::failover::{RetryPolicy, TurnDecision, TurnOutcome, TurnProgress};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type EventCallback = Arc<dyn Fn(AgentEvent) + Send + Sync>;

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
                if let Some(target) = self.fallback.take() {
                    // Pair the end with the target that its Start announced,
                    // even if another runtime mutation changed the live model.
                    (*provider, *model) = target;
                    true
                } else {
                    false
                }
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
        self.run_recoverable_turn(Some(UserContent::Text(input.into())), abort_signal, on_event)
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
        self.run_recoverable_turn(None, abort_signal, on_event).await
    }

    async fn run_recoverable_turn(
        &mut self,
        input: Option<UserContent>,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        ensure_not_aborted(&abort_signal)?;
        self.session.ensure_provider_reentry_allowed()?;
        self.sync_extension_mcp_registrations().await;
        ensure_not_aborted(&abort_signal)?;
        self.session.ensure_provider_reentry_allowed()?;

        // Construct this once for the entire call. Recovery events must reach
        // subscribers too, and a resumed attempt must not fan out a second time.
        let shared: EventCallback = Arc::new(self.make_combined_callback(on_event));
        self.maybe_restore_primary(&shared).await?;
        ensure_not_aborted(&abort_signal)?;
        let turn = LogicalTurn::new(shared);
        let shared = turn.callback();
        let forwarded = Arc::clone(&shared);
        let first = match input {
            Some(UserContent::Text(input)) => {
                self.session
                    .run_text_with_abort(input, Some(abort_signal.clone()), move |event| {
                        forwarded(event);
                    })
                    .await
            }
            Some(UserContent::Blocks(content)) => {
                self.session
                    .run_with_content_with_abort(
                        content,
                        Some(abort_signal.clone()),
                        move |event| forwarded(event),
                    )
                    .await
            }
            None => {
                self.session
                    .run_continue_with_abort(Some(abort_signal.clone()), move |event| {
                        forwarded(event);
                    })
                    .await
            }
        };
        let result = self
            .apply_retry_policy(first, &abort_signal, &shared, &turn)
            .await;
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
        self.failover = options.map(Arc::new);
        self
    }

    /// Install (or clear) the recovery policy used by every turn entrypoint.
    #[must_use]
    pub const fn with_retry(mut self, policy: Option<RetryPolicy>) -> Self {
        self.retry = policy;
        self
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
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let admission = self.session.provider_admission_gate();
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
            .try_failover_swap(&cx, &attempt, Some(&admission))
            .await?;
        admission.ensure_allowed()?;
        let Some(committed) = outcome.committed else {
            // An uncredentialed candidate may become usable on a later turn.
            // Exhaustion alone must not permanently advance the stored cursor.
            return Ok(false);
        };
        self.failover_state.set_lifecycle_id(Some(lifecycle_id));
        self.failover_state.set_chain_position(outcome.next_position);
        self.failover_state.record_swap(
            primary,
            (committed.to_provider.clone(), committed.to_model.clone()),
            Instant::now(),
        );
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

    fn close_failover_lifecycle(
        &self,
        failed_over: bool,
        success: bool,
        shared: &EventCallback,
    ) {
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
        let cx = crate::agent_cx::AgentCx::for_current_or_request();
        let admission = self.session.provider_admission_gate();
        admission.ensure_allowed()?;
        let restored = self
            .session
            .restore_primary_swap(&cx, &request, Some(&admission))
            .await?;
        // Lenient restoration may decline an unavailable primary, but it may
        // never turn an indeterminate save into permission to keep issuing.
        // The transition guard quarantines interrupted saves as well.
        admission.ensure_allowed()?;
        let Some(restored) = restored else {
            return Ok(());
        };
        self.failover_state.clear();
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
        shared: &EventCallback,
    ) -> Result<AssistantMessage> {
        ensure_not_aborted(abort_signal)?;
        let forwarded = Arc::clone(shared);
        self.session
            .run_continue_with_abort(Some(abort_signal.clone()), move |event| forwarded(event))
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn apply_retry_policy(
        &mut self,
        first: Result<AssistantMessage>,
        abort_signal: &AbortSignal,
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
            if matches!(decision, TurnDecision::Retry { .. } | TurnDecision::FailOver)
                && abort_signal.is_aborted()
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
                        current = self.resume_recovery_attempt(abort_signal, shared).await;
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
            current = self.resume_recovery_attempt(abort_signal, shared).await;
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
        return Err(Error::validation("RPC UI response requires a nonempty request ID"));
    }
    let mut payload = Map::new();
    payload.insert("requestId".to_string(), Value::String(request_id.to_string()));
    Ok(payload)
}

fn ensure_not_aborted(signal: &AbortSignal) -> Result<()> {
    if signal.is_aborted() {
        Err(Error::Aborted)
    } else {
        Ok(())
    }
}
