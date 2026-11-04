//! Invocation-owned tool progress for the FTUI event channel.
//!
//! Lifecycle messages remain ordered and lossless. Progress is a replaceable,
//! bounded text snapshot sampled by the UI's existing pending-card heartbeat.
//! Progress never queues notifications, raw tool content, images or details,
//! even when the UI falls behind. Scope and slot identities are host-owned;
//! provider call IDs are opaque lookup keys, never terminal-sanitized identities.
//! Producers must use this bridge: the stateless parent event translator cannot
//! safely queue or coalesce `ToolExecutionUpdate` events by itself.

use crate::agent::AgentEvent;
use crate::interactive::PiMsg;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

const MAX_ACTIVE: usize = 128;
const MAX_SEEN: usize = 1024;
const MAX_KEY_BYTES: usize = 4096;

/// One presentation operation's scope: a native prompt, or a whole goal with
/// its outer lifecycle filtered by the goal display. Its `Arc` identity
/// distinguishes operations even when they use the same session and call IDs.
#[derive(Debug)]
pub struct Scope {
    owner_session_id: Option<String>,
    closed: AtomicBool,
}

impl Scope {
    #[must_use]
    pub fn matches_session(&self, session_id: Option<&str>) -> bool {
        self.owner_session_id.as_deref() == session_id
    }

    /// Retire producer callbacks without taking the bridge or any slot lock.
    /// Already-queued lifecycle messages remain valid and must be consumed in
    /// order; the UI checks scope identity, not this producer-only latch.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// The complete replacement for one pending card's progress preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub preview: Option<String>,
}

#[derive(Debug, Default)]
struct SlotState {
    latest: Option<Snapshot>,
    closed: bool,
}

/// A host-generated identity and one bounded latest-state mailbox.
#[derive(Debug)]
pub struct Slot {
    scope: Arc<Scope>,
    id: String,
    state: Mutex<SlotState>,
}

impl Slot {
    fn new(scope: Arc<Scope>) -> Self {
        Self {
            scope,
            id: uuid::Uuid::new_v4().to_string(),
            state: Mutex::new(SlotState::default()),
        }
    }

    #[must_use]
    pub const fn scope(&self) -> &Arc<Scope> {
        &self.scope
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Drain the latest replacement on the UI's existing pending-card tick.
    /// No progress notification is queued before or after this operation.
    pub fn take(&self) -> Option<Snapshot> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed || self.scope.is_closed() {
            state.latest = None;
            return None;
        }
        state.latest.take()
    }

    /// Discard provisional output when authoritative completion or retirement
    /// wins. This does not invalidate an already-queued Start or End event.
    pub fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        state.latest = None;
    }

    fn publish(&self, snapshot: Snapshot) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed || self.scope.is_closed() {
            return;
        }
        state.latest = Some(snapshot);
    }
}

/// Presentation-only messages; none changes the persisted or model-visible
/// tool result. `Message` scopes the existing non-progress translator output.
#[derive(Debug, Clone)]
pub enum Event {
    Begin {
        scope: Arc<Scope>,
    },
    Message {
        scope: Arc<Scope>,
        message: Box<PiMsg>,
    },
    Start {
        slot: Arc<Slot>,
        name: String,
        summary: Option<String>,
    },
    End {
        slot: Arc<Slot>,
        name: String,
        is_error: bool,
        output: Option<String>,
        interim: Option<String>,
    },
}

struct Active {
    name: String,
    slot: Arc<Slot>,
    progress_enabled: bool,
}

#[derive(Default)]
struct State {
    active: HashMap<String, Active>,
    // Remember IDs through completion so late callbacks cannot attach to a
    // reused ID. The value counts starts deliberately sent through the legacy
    // lifecycle path; it does not grant permission to receive progress.
    seen: HashMap<String, usize>,
}

struct Shared {
    scope: Arc<Scope>,
    state: Mutex<State>,
    send: Box<dyn Fn(PiMsg) + Send + Sync>,
}

impl Shared {
    fn emit(&self, event: Event) {
        (self.send)(PiMsg::FtuiToolProgress(event));
    }

    fn mapped(&self, event: &AgentEvent) {
        for message in super::agent_event_to_pi_msgs(event) {
            self.emit(Event::Message {
                scope: Arc::clone(&self.scope),
                message: Box::new(message),
            });
        }
    }

    fn retire(&self, state: &mut State) {
        self.scope.close();
        for active in state.active.values() {
            active.slot.close();
        }
        state.active.clear();
        state.seen.clear();
    }

    fn start(&self, state: &mut State, event: &AgentEvent, id: &str, name: &str) {
        if let Some(active) = state.active.get_mut(id) {
            // Native events have no token that could distinguish concurrent
            // reuse of a provider ID. Retain the original lifecycle, but stop
            // accepting progress for either ambiguous invocation.
            active.progress_enabled = false;
            active.slot.close();
        }
        if id.len() > MAX_KEY_BYTES {
            self.mapped(event);
            return;
        }
        if let Some(fallback_pending) = state.seen.get_mut(id) {
            *fallback_pending = fallback_pending.saturating_add(1);
            self.mapped(event);
            return;
        }
        if state.seen.len() >= MAX_SEEN {
            self.mapped(event);
            return;
        }
        let eligible = name.len() <= MAX_KEY_BYTES && state.active.len() < MAX_ACTIVE;
        state.seen.insert(id.to_owned(), usize::from(!eligible));
        if !eligible {
            self.mapped(event);
            return;
        }
        let AgentEvent::ToolExecutionStart { args, .. } = event else {
            return;
        };
        let slot = Arc::new(Slot::new(Arc::clone(&self.scope)));
        state.active.insert(
            id.to_owned(),
            Active {
                name: name.to_owned(),
                slot: Arc::clone(&slot),
                progress_enabled: true,
            },
        );
        self.emit(Event::Start {
            slot,
            name: name.to_owned(),
            summary: crate::interactive::tool_invocation_summary(name, args),
        });
    }

    fn observe(&self, event: &AgentEvent) {
        // The nonblocking sink is called under this lock so concurrent native
        // callbacks cannot publish an end before its matching start. Sink
        // implementations must not synchronously reenter this bridge.
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if self.scope.is_closed() {
            return;
        }
        match event {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                ..
            } => self.start(&mut state, event, tool_call_id, tool_name),
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                partial_result,
                ..
            } => {
                let Some(active) = state
                    .active
                    .get(tool_call_id)
                    .filter(|active| active.name == *tool_name && active.progress_enabled)
                else {
                    return;
                };
                // Only accepted final write out:// results become assistant
                // prose; a provisional reply must not appear on a tool card.
                if tool_name == "write"
                    && crate::tools::interim_response_text(partial_result).is_some()
                {
                    return;
                }
                active.slot.publish(Snapshot {
                    preview: super::tool_output_preview(partial_result),
                });
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                if !state
                    .active
                    .get(tool_call_id)
                    .is_some_and(|active| active.name == *tool_name)
                {
                    let fallback = if let Some(pending) = state.seen.get_mut(tool_call_id) {
                        let expected = *pending > 0;
                        *pending = pending.saturating_sub(1);
                        expected
                    } else {
                        // Metadata caps intentionally disable tracking, while
                        // preserving the existing lifecycle presentation.
                        tool_call_id.len() > MAX_KEY_BYTES || state.seen.len() >= MAX_SEEN
                    };
                    if fallback {
                        self.mapped(event);
                    }
                    return;
                }
                let Some(active) = state.active.remove(tool_call_id) else {
                    return;
                };
                active.slot.close();
                let interim = (tool_name == "write" && !is_error)
                    .then(|| crate::tools::interim_response_text(result))
                    .flatten()
                    .map(str::to_owned);
                let output = if interim.is_some() {
                    None
                } else {
                    super::tool_output_preview(result)
                };
                self.emit(Event::End {
                    slot: active.slot,
                    name: tool_name.clone(),
                    is_error: *is_error,
                    output,
                    interim,
                });
            }
            AgentEvent::AgentEnd { .. } => {
                self.mapped(event);
                self.retire(&mut state);
            }
            _ => self.mapped(event),
        }
    }
}

/// Keep this non-clone guard alive for one presentation operation. Its callbacks
/// may outlive it, but become inert when the guard drops or observes AgentEnd.
/// A goal display filters its per-prompt lifecycle and keeps one tools-only
/// bridge through the whole goal, preserving seen-ID history across prompts.
/// The sink must be nonblocking and must not reenter the bridge or synchronously
/// invoke cancellation that can reenter an agent callback.
pub struct Bridge {
    shared: Arc<Shared>,
}

impl Bridge {
    #[must_use]
    pub fn new(
        owner_session_id: Option<String>,
        send: impl Fn(PiMsg) + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            scope: Arc::new(Scope {
                owner_session_id,
                closed: AtomicBool::new(false),
            }),
            state: Mutex::new(State::default()),
            send: Box::new(send),
        });
        shared.emit(Event::Begin {
            scope: Arc::clone(&shared.scope),
        });
        Self { shared }
    }

    #[must_use]
    pub fn callback(&self) -> impl Fn(AgentEvent) + Send + Sync + 'static + use<> {
        let shared = Arc::clone(&self.shared);
        move |event| shared.observe(&event)
    }

    pub fn observe(&self, event: &AgentEvent) {
        self.shared.observe(event);
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.shared.retire(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ContentBlock, TextContent};
    use crate::tools::ToolOutput;
    use serde_json::json;
    use std::collections::HashSet;
    use std::sync::mpsc::{Receiver, channel};

    fn fixture() -> (Bridge, Receiver<PiMsg>, Arc<Scope>) {
        let (sender, receiver) = channel();
        let bridge = Bridge::new(Some("session".to_owned()), move |message| {
            let _ = sender.send(message);
        });
        let scope = match next(&receiver) {
            Event::Begin { scope } => Some(scope),
            _ => None,
        }
        .expect("bridge begins a scope");
        (bridge, receiver, scope)
    }

    fn next(receiver: &Receiver<PiMsg>) -> Event {
        match receiver
            .try_recv()
            .expect("expected a queued presentation event")
        {
            PiMsg::FtuiToolProgress(event) => Some(event),
            _ => None,
        }
        .expect("bridge emits only scoped presentation events")
    }

    fn start_event(id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolExecutionStart {
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            args: json!({}),
        }
    }

    fn start(bridge: &Bridge, receiver: &Receiver<PiMsg>, id: &str, name: &str) -> Arc<Slot> {
        bridge.observe(&start_event(id, name));
        match next(receiver) {
            Event::Start { slot, .. } => Some(slot),
            _ => None,
        }
        .expect("eligible tool has its own slot")
    }

    fn output(text: &str) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: None,
            is_error: false,
        }
    }

    fn update(id: &str, name: &str, text: &str) -> AgentEvent {
        AgentEvent::ToolExecutionUpdate {
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            args: json!({}),
            partial_result: output(text),
        }
    }

    fn end(id: &str, name: &str, text: &str) -> AgentEvent {
        AgentEvent::ToolExecutionEnd {
            tool_call_id: id.to_owned(),
            tool_name: name.to_owned(),
            result: output(text),
            is_error: false,
        }
    }

    #[test]
    fn flood_keeps_only_latest_snapshot_and_queues_no_progress_notifications() {
        let (bridge, receiver, _) = fixture();
        let slot = start(&bridge, &receiver, "call", "subagent");
        for index in 0..10_000 {
            bridge.observe(&update("call", "subagent", &format!("partial {index}")));
        }
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(
            slot.take().expect("latest snapshot").preview.as_deref(),
            Some("partial 9999")
        );
        bridge.observe(&update("call", "subagent", "short"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(
            slot.take()
                .expect("replacement snapshot")
                .preview
                .as_deref(),
            Some("short")
        );
    }

    #[test]
    fn draining_before_queued_lifecycle_cannot_accumulate_notifications() {
        let (bridge, receiver, _) = fixture();
        bridge.observe(&start_event("call", "subagent"));
        let slot = Arc::clone(
            &bridge
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .active
                .get("call")
                .expect("tracked invocation")
                .slot,
        );
        // Leave Start queued while repeatedly publishing and draining. A
        // pending heartbeat cannot create notifications faster than the
        // channel consumes them because updates never enter the channel.
        for index in 0..1000 {
            let text = format!("partial {index}");
            bridge.observe(&update("call", "subagent", &text));
            // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
            assert_eq!(slot.take().expect("latest replacement").preview, Some(text));
        }
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::Start { slot: started, .. } if Arc::ptr_eq(&started, &slot))
        );
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn interleaved_native_and_extension_progress_uses_opaque_call_ids() {
        let (bridge, receiver, scope) = fixture();
        let native = start(&bridge, &receiver, "same\u{1b}[31m", "bash");
        let extension = start(&bridge, &receiver, "same", "subagent");
        bridge.observe(&update("same", "subagent", "child running"));
        bridge.observe(&update("same\u{1b}[31m", "bash", "native running"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_ne!(native.id(), extension.id());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(Arc::ptr_eq(native.scope(), &scope));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(
            native.take().expect("native progress").preview.as_deref(),
            Some("native running")
        );
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(
            extension
                .take()
                .expect("extension progress")
                .preview
                .as_deref(),
            Some("child running")
        );
    }

    #[test]
    fn unknown_or_mismatched_events_do_not_queue_or_replace_a_card() {
        let (bridge, receiver, _) = fixture();
        let slot = start(&bridge, &receiver, "call", "subagent");
        bridge.observe(&update("missing", "subagent", "unknown"));
        bridge.observe(&update("call", "bash", "wrong tool"));
        bridge.observe(&end("missing", "subagent", "unknown final"));
        bridge.observe(&end("call", "bash", "wrong final"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(slot.take().is_none());
    }

    #[test]
    fn empty_progress_is_a_replacement_and_fast_empty_final_is_authoritative() {
        let (bridge, receiver, _) = fixture();
        let slot = start(&bridge, &receiver, "call", "subagent");
        bridge.observe(&update("call", "subagent", "partial"));
        let _ = slot.take();
        bridge.observe(&update("call", "subagent", ""));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(slot.take(), Some(Snapshot { preview: None }));
        bridge.observe(&update("call", "subagent", "never drawn"));
        bridge.observe(&end("call", "subagent", ""));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::End { slot: ended, output: None, interim: None, .. } if Arc::ptr_eq(&ended, &slot))
        );
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(slot.take().is_none());
        bridge.observe(&update("call", "subagent", "late"));
        bridge.observe(&end("call", "subagent", "duplicate final"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn dropping_owner_retires_callbacks_that_outlive_it() {
        let (bridge, receiver, scope) = fixture();
        let slot = start(&bridge, &receiver, "call", "subagent");
        let callback = bridge.callback();
        callback(update("call", "subagent", "pending"));
        drop(bridge);
        callback(update("call", "subagent", "late"));
        callback(end("call", "subagent", "late final"));
        callback(start_event("another", "bash"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(scope.is_closed());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(slot.take().is_none());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn terminal_event_is_delivered_before_scope_retirement() {
        let (bridge, receiver, scope) = fixture();
        let _ = start(&bridge, &receiver, "call", "subagent");
        bridge.observe(&AgentEvent::AgentEnd {
            session_id: Arc::from("session"),
            messages: Vec::new(),
            error: None,
        });
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::Message { scope: message_scope, message } if Arc::ptr_eq(&scope, &message_scope) && matches!(*message, PiMsg::AgentDone { .. }))
        );
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(scope.is_closed());
        bridge.observe(&start_event("late", "bash"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn fast_completion_keeps_queued_lifecycle_after_guard_retirement() {
        let (bridge, receiver, scope) = fixture();
        bridge.observe(&start_event("fast", "subagent"));
        bridge.observe(&end("fast", "subagent", "authoritative"));
        drop(bridge);
        let slot = match next(&receiver) {
            Event::Start { slot, .. } => Some(slot),
            _ => None,
        }
        .expect("queued start survives producer retirement");
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(scope.is_closed());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::End { slot: ended, output: Some(text), .. } if Arc::ptr_eq(&ended, &slot) && text == "authoritative")
        );
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(slot.take().is_none());
    }

    #[test]
    fn concurrent_callbacks_keep_each_invocations_lifecycle_order() {
        let (bridge, receiver, _) = fixture();
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let threads = (0..4)
            .map(|index| {
                let callback = bridge.callback();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let id = format!("concurrent-{index}");
                    barrier.wait();
                    callback(start_event(&id, "subagent"));
                    for update_index in 0..100 {
                        callback(update(&id, "subagent", &format!("partial {update_index}")));
                    }
                    callback(end(&id, "subagent", "final"));
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("concurrent callback producer");
        }
        let mut starts = HashSet::new();
        let mut ends = HashSet::new();
        let mut unexpected = 0;
        for message in receiver.try_iter() {
            match message {
                PiMsg::FtuiToolProgress(Event::Start { slot, .. }) => {
                    // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
                    assert!(starts.insert(slot.id().to_owned()));
                }
                PiMsg::FtuiToolProgress(Event::End { slot, .. }) => {
                    // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
                    assert!(starts.contains(slot.id()));
                    // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
                    assert!(ends.insert(slot.id().to_owned()));
                }
                _ => unexpected += 1,
            }
        }
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(starts.len(), 4);
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(starts, ends);
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(unexpected, 0);
    }

    #[test]
    fn reused_ids_keep_lifecycle_but_cannot_receive_old_progress() {
        let (bridge, receiver, _) = fixture();
        let _ = start(&bridge, &receiver, "reused", "subagent");
        bridge.observe(&end("reused", "subagent", "first final"));
        let _ = next(&receiver);
        bridge.observe(&start_event("reused", "subagent"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::Message { message, .. } if matches!(*message, PiMsg::ToolStart { .. }))
        );
        bridge.observe(&update("reused", "subagent", "ambiguous"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        bridge.observe(&end("reused", "subagent", ""));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::Message { message, .. } if matches!(*message, PiMsg::ToolEnd { output: Some(ref text), .. } if text.is_empty()))
        );
    }

    #[test]
    fn overlapping_reused_id_retires_only_its_progress_slot() {
        let (bridge, receiver, _) = fixture();
        let reused = start(&bridge, &receiver, "reused", "subagent");
        let peer = start(&bridge, &receiver, "peer", "bash");
        bridge.observe(&update("reused", "subagent", "old partial"));
        bridge.observe(&start_event("reused", "subagent"));
        let _ = next(&receiver);
        bridge.observe(&update("reused", "subagent", "ambiguous"));
        bridge.observe(&update("peer", "bash", "healthy"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(reused.take().is_none());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(
            peer.take().expect("peer progress").preview.as_deref(),
            Some("healthy")
        );
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn active_slot_cap_preserves_fallback_lifecycle_without_progress() {
        let (bridge, receiver, _) = fixture();
        for index in 0..MAX_ACTIVE {
            let _ = start(&bridge, &receiver, &format!("call-{index}"), "subagent");
        }
        bridge.observe(&start_event("overflow", "subagent"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::Message { message, .. } if matches!(*message, PiMsg::ToolStart { .. }))
        );
        bridge.observe(&update("overflow", "subagent", "suppressed"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        bridge.observe(&end("overflow", "subagent", "final"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::Message { message, .. } if matches!(*message, PiMsg::ToolEnd { .. }))
        );
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(
            bridge
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .active
                .len(),
            MAX_ACTIVE
        );
    }

    #[test]
    fn seen_id_cap_bounds_retained_identity_history() {
        let (bridge, receiver, _) = fixture();
        for index in 0..MAX_SEEN {
            let id = format!("completed-{index}");
            let _ = start(&bridge, &receiver, &id, "subagent");
            bridge.observe(&end(&id, "subagent", "final"));
            let _ = next(&receiver);
        }
        bridge.observe(&start_event("after-cap", "subagent"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::Message { message, .. } if matches!(*message, PiMsg::ToolStart { .. }))
        );
        bridge.observe(&update("after-cap", "subagent", "suppressed"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert_eq!(
            bridge
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .seen
                .len(),
            MAX_SEEN
        );
    }

    #[test]
    fn oversized_identity_falls_back_without_retaining_a_progress_key() {
        let (bridge, receiver, _) = fixture();
        let id = "x".repeat(MAX_KEY_BYTES + 1);
        bridge.observe(&start_event(&id, "subagent"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::Message { message, .. } if matches!(*message, PiMsg::ToolStart { tool_id, .. } if tool_id == id))
        );
        bridge.observe(&update(&id, "subagent", "suppressed"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            bridge
                .shared
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .seen
                .is_empty()
        );
    }

    #[test]
    fn session_match_is_exact_and_ui_retirement_disables_callbacks() {
        let (bridge, receiver, scope) = fixture();
        let slot = start(&bridge, &receiver, "call", "subagent");
        bridge.observe(&update("call", "subagent", "pending"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(scope.matches_session(Some("session")));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(!scope.matches_session(Some("another")));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(!scope.matches_session(None));
        scope.close();
        bridge.observe(&update("call", "subagent", "retired"));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(slot.take().is_none());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn unknown_session_scope_never_matches_a_known_session() {
        let (sender, receiver) = channel();
        let _bridge = Bridge::new(None, move |message| {
            let _ = sender.send(message);
        });
        let scope = match next(&receiver) {
            Event::Begin { scope } => Some(scope),
            _ => None,
        }
        .expect("unknown owner still has a unique prompt scope");
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(scope.matches_session(None));
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(!scope.matches_session(Some("session")));
    }

    #[test]
    fn provisional_interim_reply_is_hidden_until_accepted_final_result() {
        let (bridge, receiver, _) = fixture();
        let slot = start(&bridge, &receiver, "reply", "write");
        let mut result = output("accepted reply");
        result.details = Some(json!({
            "schema": "pi.interim_response.v1",
            "text": "accepted reply",
        }));
        bridge.observe(&AgentEvent::ToolExecutionUpdate {
            tool_call_id: "reply".to_owned(),
            tool_name: "write".to_owned(),
            args: json!({ "path": "out://" }),
            partial_result: result.clone(),
        });
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(receiver.try_recv().is_err());
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(slot.take().is_none());
        bridge.observe(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "reply".to_owned(),
            tool_name: "write".to_owned(),
            result,
            is_error: false,
        });
        // ubs:ignore[rust.panic.assert-macros] -- cfg(test) regression invariant.
        assert!(
            matches!(next(&receiver), Event::End { output: None, interim: Some(text), .. } if text == "accepted reply")
        );
    }
}
