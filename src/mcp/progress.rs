//! Request-owned MCP progress. Readers only publish into a bounded mailbox;
//! user callbacks run on the calling task, never on a transport reader.
//!
//! Progress is advisory, not a result, heartbeat, or authority to replay work.
//! The request's existing deadline and dispatch/cancellation lease are retained.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::{Future, poll_fn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use serde_json::{Number, Value, json};

use super::transport::RequestDispatch;
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolUpdate;

const MAX_SUBSCRIPTIONS: usize = 1024;
const MAX_UPDATES: usize = 128;
const MAX_MESSAGE_BYTES: usize = 4096;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A single transport's progress mailboxes. Sharing this across independent
/// connections would violate request isolation. HTTP session generations must
/// be supplied both at registration and when receiving a notification.
///
/// Debug output deliberately excludes tokens and server-provided messages.
pub struct ProgressRegistry {
    prefix: String,
    next_id: AtomicU64,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

impl Default for ProgressRegistry {
    fn default() -> Self {
        Self {
            prefix: uuid::Uuid::new_v4().to_string(),
            next_id: AtomicU64::new(1),
            slots: Mutex::new(HashMap::new()),
        }
    }
}

impl std::fmt::Debug for ProgressRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProgressRegistry")
            .finish_non_exhaustive()
    }
}

struct Slot {
    generation: u64,
    dispatch: Arc<RequestDispatch>,
    state: Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    closed: bool,
    delivered: usize,
    last_progress: Option<Number>,
    pending: Option<Update>,
    waiter: Option<Waker>,
}

struct Update {
    progress: Number,
    total: Option<Number>,
    message: Option<String>,
    message_truncated: bool,
}

fn finite_nonnegative(value: &Value) -> Option<Number> {
    let Value::Number(number) = value else {
        return None;
    };
    let finite = number.as_f64()?;
    (finite.is_finite() && finite >= 0.0).then(|| number.clone())
}

fn strictly_greater(next: &Number, previous: &Number) -> bool {
    // Do not round integer counters above 2^53 through an f64 comparison.
    match (next.as_u64(), previous.as_u64()) {
        (Some(next), Some(previous)) => next > previous,
        _ => next.as_f64() > previous.as_f64(),
    }
}

/// Bound the escaped representation, not just its source. No partial escape,
/// control sequence, bidi override, or raw server string enters ToolUpdate.
fn display_message(message: &str) -> (String, bool) {
    let mut escaped = String::new();
    for character in message.chars() {
        let representation = character.escape_default();
        if representation.clone().count() > MAX_MESSAGE_BYTES - escaped.len() {
            return (escaped, true);
        }
        escaped.extend(representation);
    }
    (escaped, false)
}

impl Update {
    fn parse(params: &Value) -> Option<Self> {
        let progress = finite_nonnegative(params.get("progress")?)?;
        let total = match params.get("total") {
            None => None,
            Some(total) => Some(finite_nonnegative(total)?),
        };
        // A total is an estimate, not a completion assertion. It can change
        // between notifications and need not accompany every update.
        let (message, message_truncated) = match params.get("message") {
            None => (None, false),
            Some(message) => {
                let (message, truncated) = display_message(message.as_str()?);
                (Some(message), truncated)
            }
        };
        Some(Self {
            progress,
            total,
            message,
            message_truncated,
        })
    }

    fn into_tool_update(self, delivery_limited: bool) -> ToolUpdate {
        let mut text = format!("MCP progress: {}", self.progress);
        if let Some(total) = &self.total {
            let _ = write!(text, " / {total}");
        }
        if let Some(message) = &self.message {
            let _ = write!(text, " — \"{message}\"");
        }
        if self.message_truncated {
            text.push_str(" [message truncated]");
        }
        if delivery_limited {
            text.push_str(" [further progress updates suppressed; awaiting the tool result]");
        }
        ToolUpdate {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({"mcpProgress": {
                "progress": self.progress,
                "total": self.total,
                "message": self.message,
                "messageTruncated": self.message_truncated,
                "deliveryLimited": delivery_limited,
            }})),
        }
    }
}

impl ProgressRegistry {
    /// Register before dispatch, but accept progress only after this exact
    /// dispatch lease is claimed. Capacity exhaustion disables observation;
    /// it does not reject a valid tool call or retire its connection.
    #[must_use]
    pub fn subscribe(
        self: &Arc<Self>,
        dispatch: Arc<RequestDispatch>,
        generation: u64,
    ) -> Option<ProgressSubscription> {
        let id = self
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .ok()?;
        let token = format!("pi-mcp-{}-{id}", self.prefix);
        let slot = Arc::new(Slot {
            generation,
            dispatch,
            state: Mutex::new(SlotState::default()),
        });
        {
            let mut slots = lock(&self.slots);
            if slots.len() >= MAX_SUBSCRIPTIONS {
                return None;
            }
            slots.insert(token.clone(), Arc::clone(&slot));
        }
        Some(ProgressSubscription {
            registry: Arc::clone(self),
            token,
            slot,
        })
    }

    /// Inspect an already decoded JSON-RPC envelope. Unknown, late, malformed,
    /// regressing and wrong-generation progress is ignored, not promoted to a
    /// result or a fatal transport error. This method never invokes user code.
    pub fn publish(&self, message: &Value, generation: u64) {
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || message.get("method").and_then(Value::as_str) != Some("notifications/progress")
            || message.get("id").is_some()
            || message.get("result").is_some()
            || message.get("error").is_some()
        {
            return;
        }
        let Some(params) = message.get("params").filter(|params| params.is_object()) else {
            return;
        };
        // We generate strings only. Numeric tokens and stringified lookalikes
        // cannot correlate to one of this client's active requests.
        let Some(token) = params.get("progressToken").and_then(Value::as_str) else {
            return;
        };
        let slot = { lock(&self.slots).get(token).cloned() };
        let Some(slot) = slot else {
            return;
        };
        if slot.generation != generation || !slot.dispatch.is_claimed() {
            return;
        }
        let waiter = {
            let mut state = lock(&slot.state);
            if state.closed || state.delivered >= MAX_UPDATES || !slot.dispatch.is_claimed() {
                return;
            }
            let Some(update) = Update::parse(params) else {
                return;
            };
            if state
                .last_progress
                .as_ref()
                .is_some_and(|last| !strictly_greater(&update.progress, last))
            {
                return;
            }
            state.last_progress = Some(update.progress.clone());
            // A slow caller receives the latest state instead of an unbounded
            // queue. Producers never wait for the callback or its executor.
            state.pending = Some(update);
            state.waiter.take()
        };
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}

/// Owns exactly one progress token. Dropping it closes the mailbox and removes
/// registration, including when the enclosing request future is abandoned.
pub struct ProgressSubscription {
    registry: Arc<ProgressRegistry>,
    token: String,
    slot: Arc<Slot>,
}

impl std::fmt::Debug for ProgressSubscription {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProgressSubscription")
            .finish_non_exhaustive()
    }
}

impl ProgressSubscription {
    /// The client-owned token for the request's `_meta.progressToken` field.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The transport session generation captured before dispatch.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.slot.generation
    }

    /// Poll the coalesced status on the calling task, not on the reader.
    /// There is at most one consumer, one pending update, and one waiter.
    pub fn poll_update(&mut self, task: &mut Context<'_>) -> Poll<Option<ToolUpdate>> {
        let (outcome, old_waiter) = {
            let mut state = lock(&self.slot.state);
            if state.closed || state.delivered >= MAX_UPDATES {
                (Poll::Ready(None), state.waiter.take())
            } else if let Some(update) = state.pending.take() {
                if self.slot.dispatch.is_claimed() {
                    state.delivered += 1;
                    (
                        Poll::Ready(Some((update, state.delivered == MAX_UPDATES))),
                        state.waiter.take(),
                    )
                } else {
                    (Poll::Ready(None), state.waiter.take())
                }
            } else {
                let old_waiter = if state
                    .waiter
                    .as_ref()
                    .is_some_and(|waker| waker.will_wake(task.waker()))
                {
                    None
                } else {
                    state.waiter.replace(task.waker().clone())
                };
                (Poll::Pending, old_waiter)
            }
        };
        drop(old_waiter);
        outcome.map(|value| value.map(|(update, limited)| update.into_tool_update(limited)))
    }
}

impl Drop for ProgressSubscription {
    fn drop(&mut self) {
        let waiter = {
            let mut state = lock(&self.slot.state);
            state.closed = true;
            state.pending = None;
            state.waiter.take()
        };
        lock(&self.registry.slots).remove(&self.token);
        drop(waiter);
    }
}

/// Observe at most one coalesced update per poll, with the response taking
/// precedence. The caller owns timeout/cancellation around this whole future.
/// Callback execution is synchronous and cannot preempt blocking user code.
pub(crate) async fn observe<F: Future>(
    operation: F,
    mut progress: Option<ProgressSubscription>,
    on_update: Option<&(dyn Fn(ToolUpdate) + Send + Sync)>,
    current: impl Fn() -> bool,
) -> F::Output {
    if progress.is_none() || on_update.is_none() {
        return operation.await;
    }
    let mut operation = std::pin::pin!(operation);
    poll_fn(|task| {
        if let Poll::Ready(result) = operation.as_mut().poll(task) {
            progress.take();
            return Poll::Ready(result);
        }
        if !current() {
            progress.take();
            return Poll::Pending;
        }
        if let (Some(progress), Some(callback)) = (progress.as_mut(), on_update)
            && let Poll::Ready(Some(update)) = progress.poll_update(task)
        {
            callback(update);
            // Yield to the executor; never drain a hostile stream in one poll.
            // A callback can also make the underlying operation ready.
            task.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests;
