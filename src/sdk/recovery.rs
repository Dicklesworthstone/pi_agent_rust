//! Recovery for every SDK turn entrypoint, including the default FTUI driver.
//!
//! Only the first attempt may append user input. Recovery goes through the
//! session's persisted continuation/transition APIs, never the bare Agent loop.

use super::{
    AbortHandle, AbortSignal, AgentEvent, AgentSessionHandle, AssistantMessage, Error,
    FailoverOptions, Result, StopReason,
};
use crate::failover::{RetryPolicy, TurnDecision, TurnOutcome, TurnProgress};
use std::sync::Arc;
use std::time::{Duration, Instant};

type EventCallback = Arc<dyn Fn(AgentEvent) + Send + Sync>;

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
        self.run_recoverable_turn(Some(input.into()), abort_signal, on_event)
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
        input: Option<String>,
        abort_signal: AbortSignal,
        on_event: impl Fn(AgentEvent) + Send + Sync + 'static,
    ) -> Result<AssistantMessage> {
        ensure_not_aborted(&abort_signal)?;
        self.sync_extension_mcp_registrations().await;
        ensure_not_aborted(&abort_signal)?;

        // Construct this once for the entire call. Recovery events must reach
        // subscribers too, and a resumed attempt must not fan out a second time.
        let shared: EventCallback = Arc::new(self.make_combined_callback(on_event));
        self.maybe_restore_primary(&shared).await?;
        ensure_not_aborted(&abort_signal)?;
        let forwarded = Arc::clone(&shared);
        let first = match input {
            Some(input) => {
                self.session
                    .run_text_with_abort(input, Some(abort_signal.clone()), move |event| {
                        forwarded(event);
                    })
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
        self.apply_retry_policy(first, &abort_signal, &shared).await
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

    /// Install (or clear) the recovery policy used by all four turn entrypoints.
    #[must_use]
    pub const fn with_retry(mut self, policy: Option<RetryPolicy>) -> Self {
        self.retry = policy;
        self
    }

    /// A chain belongs to its original primary, not the currently installed
    /// fallback. Candidate preparation and durable installation remain shared
    /// with print and RPC in `AgentSession::try_failover`.
    async fn try_chain_failover(
        &mut self,
        current: &Result<AssistantMessage>,
        require_incomplete_tail: bool,
        retry_attempt_to_end: Option<u32>,
        shared: &EventCallback,
    ) -> Result<bool> {
        let Some(options) = self.failover.clone() else {
            return Ok(false);
        };
        let Some(error_text) = Self::turn_error_text_for(current) else {
            return Ok(false);
        };
        let Some(class) = crate::failover::classify_failover(&error_text) else {
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
            lifecycle_id: self.failover_state.lifecycle_id(),
        };
        let outcome = self.session.try_failover(&cx, &attempt).await?;
        let Some(committed) = outcome.committed else {
            // An uncredentialed candidate may become usable on a later turn.
            // Exhaustion alone must not permanently advance the stored cursor.
            return Ok(false);
        };
        if self.failover_state.lifecycle_id().is_none() {
            self.failover_state
                .set_lifecycle_id(Some(uuid::Uuid::new_v4().to_string()));
        }
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
            attempt: 0,
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
        let Some(restored) = self.session.restore_primary(&cx, &request).await? else {
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
                        shared,
                    )
                    .await
                {
                    Ok(true) => {
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
                    shared,
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
        shared: &EventCallback,
    ) -> Result<()> {
        shared(AgentEvent::AutoRetryStart {
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
            self.session.restore_retry_tail(&cx, current.is_ok()).await?;
            // Do not race cancellation against a durability operation: let it
            // settle, then refuse provider re-entry when the signal was raised.
            ensure_not_aborted(abort_signal)
        }
        .await;
        if let Err(error) = &result {
            shared(AgentEvent::AutoRetryEnd {
                success: false,
                attempt,
                final_error: Some(error.to_string()),
            });
        }
        result
    }
}

fn ensure_not_aborted(signal: &AbortSignal) -> Result<()> {
    if signal.is_aborted() {
        Err(Error::Aborted)
    } else {
        Ok(())
    }
}
