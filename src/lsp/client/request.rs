//! One owner and deadline from request admission through response delivery.
//!
//! The transport still performs synchronous pipe writes. These checks bound
//! async lane/response/retry waits, not a blocked OS write. Abandonment sends
//! the protocol cancellation notification; it does not undo server side effects.

use std::sync::Arc;
use std::time::Duration;

use asupersync::sync::{Mutex, OwnedMutexGuard};
use asupersync::types::Time;
use futures::future::{Either, select};
use serde_json::Value;

use super::{
    JsonRpcClient, LspCallError, LspClient, TransportError, WAIT_TICK, WARMUP_EMPTY_RESULT_WINDOW,
    WARMUP_RETRY_CADENCE, is_empty_result, is_warmup_empty_retryable,
};
use crate::agent_cx::AgentCx;

pub(super) struct RequestBudget {
    owner: AgentCx,
    start: Time,
    timeout: Duration,
}

impl RequestBudget {
    pub(super) fn new(timeout: Duration) -> Self {
        let owner = AgentCx::for_current_or_request();
        let start = now(&owner);
        Self {
            owner,
            start,
            timeout,
        }
    }

    pub(super) fn remaining(&self) -> Result<Duration, LspCallError> {
        self.owner
            .checkpoint()
            .map_err(|_| LspCallError::Cancelled)?;
        let elapsed = Duration::from_nanos(now(&self.owner).duration_since(self.start));
        let remaining = self.timeout.saturating_sub(elapsed);
        if remaining.is_zero() {
            Err(LspCallError::Timeout {
                timeout_ms: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
            })
        } else {
            Ok(remaining)
        }
    }

    pub(super) async fn pause(&self, interval: Duration) -> Result<(), LspCallError> {
        let remaining = self.remaining()?;
        self.owner.time().sleep(interval.min(remaining)).await;
        self.remaining().map(|_| ())
    }

    /// Keep the same pending acquisition across ticks, preserving lock queue
    /// position. Neither queuing nor a successful late grant renews the budget.
    pub(super) async fn acquire(
        &self,
        lane: &Arc<Mutex<()>>,
    ) -> Result<OwnedMutexGuard<()>, LspCallError> {
        let mut acquisition =
            std::pin::pin!(OwnedMutexGuard::lock(Arc::clone(lane), self.owner.cx(),));
        loop {
            let remaining = self.remaining()?;
            let time = self.owner.time();
            let mut tick = std::pin::pin!(time.sleep(WAIT_TICK.min(remaining)));
            if let Either::Left((result, _)) = select(acquisition.as_mut(), tick.as_mut()).await {
                let guard = result.map_err(|_| LspCallError::Cancelled)?;
                self.remaining()?;
                return Ok(guard);
            }
        }
    }
}

fn now(owner: &AgentCx) -> Time {
    owner
        .cx()
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now())
}

/// A posted request remains owned even when its calling future is dropped.
/// Declare this after the lane guard so cancellation happens before another
/// request can enter the serialized lane.
struct PendingRequest<'a> {
    rpc: &'a JsonRpcClient,
    id: u64,
    completed: bool,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.rpc.cancel_request(self.id);
        }
    }
}

impl LspClient {
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspCallError> {
        let budget = RequestBudget::new(timeout);
        self.call_with_budget(method, params, &budget).await
    }

    pub(super) async fn call_with_budget(
        &self,
        method: &str,
        params: Value,
        budget: &RequestBudget,
    ) -> Result<Value, LspCallError> {
        loop {
            let attempt = self.call_once(method, params.clone(), budget).await;
            // Only idempotent lookups participate in the warmup policy.
            // A failed command is not evidence that its effects were undone.
            let retryable = (is_warmup_empty_retryable(method)
                || method == "textDocument/diagnostic")
                && matches!(
                    &attempt, Err(LspCallError::Transport(TransportError::Server(err)))
                        if (err.code == -32602 && err.message.contains("No references found"))
                            || err.code == -32801
                            || err.code == -32802
                );
            let empty_during_warmup = matches!(&attempt, Ok(value) if is_empty_result(value))
                && is_warmup_empty_retryable(method)
                && self.connected_at.elapsed() < WARMUP_EMPTY_RESULT_WINDOW;
            if !retryable && !empty_during_warmup {
                return attempt;
            }
            if budget.remaining()? < WARMUP_RETRY_CADENCE * 2 {
                return attempt;
            }
            budget.pause(WARMUP_RETRY_CADENCE).await?;
        }
    }

    async fn call_once(
        &self,
        method: &str,
        params: Value,
        budget: &RequestBudget,
    ) -> Result<Value, LspCallError> {
        let _lane = budget.acquire(&self.request_lane).await?;
        budget.remaining()?;
        let (id, rx) = self
            .rpc
            .request(method, params)
            .map_err(LspCallError::Transport)?;
        let mut pending = PendingRequest {
            rpc: &self.rpc,
            id,
            completed: false,
        };
        loop {
            // Cancellation and expiry win over an already-buffered late
            // response. No subsequent retry gets a fresh request timeout.
            budget.remaining()?;
            match rx.try_recv() {
                Ok(result) => {
                    pending.completed = true;
                    budget.remaining()?;
                    return result.map_err(LspCallError::Transport);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(LspCallError::Transport(TransportError::Closed(
                        "completion channel dropped".to_string(),
                    )));
                }
            }
            budget.pause(WAIT_TICK).await?;
        }
    }
}

#[cfg(test)]
mod tests;
