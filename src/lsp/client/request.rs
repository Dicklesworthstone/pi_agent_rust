//! One owner and deadline from request admission through response delivery.
//!
//! A dedicated transport pump owns blocking pipe writes. These checks bound
//! async lane/response/retry waits, including time spent queued for that pump.
//! Abandonment withdraws an unclaimed request or schedules protocol cancellation
//! after a claimed write; it does not undo server side effects.

use std::sync::Arc;
use std::sync::atomic::Ordering;
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

mod workspace_diagnostics;

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

fn is_retryable_request_error(method: &str, error: &LspCallError) -> bool {
    // Only idempotent lookups participate in the warmup policy. A failed
    // command is not evidence that its effects were undone.
    let diagnostic = matches!(method, "textDocument/diagnostic" | "workspace/diagnostic");
    if !diagnostic && !is_warmup_empty_retryable(method) {
        return false;
    }
    let LspCallError::Transport(TransportError::Server(error)) = error else {
        return false;
    };
    match error.code {
        -32602 => error.message.contains("No references found"),
        -32801 => true,
        -32802 => {
            // LSP 3.17 diagnostic cancellation can explicitly decline a new
            // request (for example, the document no longer has a provider).
            // Keep the bounded legacy retry for servers omitting this data.
            !diagnostic
                || error
                    .data
                    .as_ref()
                    .and_then(|data| data.get("retriggerRequest"))
                    .and_then(Value::as_bool)
                    != Some(false)
        }
        _ => false,
    }
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
            self.poll_notifications();
            let retryable = attempt
                .as_ref()
                .err()
                .is_some_and(|error| is_retryable_request_error(method, error));
            let empty_during_warmup = matches!(&attempt, Ok(value) if is_empty_result(value))
                && is_warmup_empty_retryable(method)
                && !self.quiescent.load(Ordering::SeqCst)
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

#[cfg(test)]
mod diagnostic_cancellation_tests {
    use super::super::test_server::Fixture;
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostic_cancellation_opt_out_preserves_error_without_replay() {
        let temp = tempfile::tempdir().unwrap();
        let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
            return;
        };
        peer.configure(json!({
            "textDocument/diagnostic": [
                {"error": {
                    "code": -32802,
                    "message": "diagnostic provider detached",
                    "data": {"retriggerRequest": false}
                }},
                {"result": {"kind": "full", "items": []}}
            ]
        }));
        let error = peer
            .runtime
            .block_on(peer.client.call(
                "textDocument/diagnostic",
                json!({"textDocument": {"uri": "file:///test.rs"}}),
                Duration::from_secs(5),
            ))
            .unwrap_err();
        let LspCallError::Transport(TransportError::Server(error)) = error else {
            panic!("expected the original server cancellation");
        };
        assert_eq!(error.code, -32802);
        assert_eq!(error.data, Some(json!({"retriggerRequest": false})));
        let frames = peer.frames();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "textDocument/diagnostic")
                .count(),
            1,
            "a server opt-out must not become an automatic second request"
        );
        assert!(
            !frames
                .iter()
                .any(|frame| frame["method"] == "$/cancelRequest")
        );
        assert!(peer.client.is_alive());
    }

    #[test]
    fn retrigger_and_legacy_diagnostic_cancellations_retry_within_the_same_call() {
        for data in [Some(json!({"retriggerRequest": true})), None] {
            let temp = tempfile::tempdir().unwrap();
            let Some(peer) = Fixture::connect(temp.path(), json!({})) else {
                return;
            };
            let mut error = json!({"code": -32802, "message": "diagnostics recomputing"});
            if let Some(data) = data {
                error["data"] = data;
            }
            let report = json!({"kind": "full", "resultId": "ready", "items": []});
            peer.configure(json!({
                "textDocument/diagnostic": [{"error": error}, {"result": report}]
            }));
            let result = peer
                .runtime
                .block_on(peer.client.call(
                    "textDocument/diagnostic",
                    json!({"textDocument": {"uri": "file:///test.rs"}}),
                    Duration::from_secs(5),
                ))
                .unwrap();
            assert_eq!(result, report);
            assert_eq!(
                peer.frames()
                    .iter()
                    .filter(|frame| frame["method"] == "textDocument/diagnostic")
                    .count(),
                2
            );
        }
    }
}
