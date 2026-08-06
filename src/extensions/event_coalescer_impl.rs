//! Coalescing dispatch implementation for [`super::EventCoalescer`].

use super::{
    AgentEvent, CoalescedPayload, EventCoalescer, ExtensionEventName, ExtensionManager,
    extension_event_name_from_agent, is_coalescable_event, is_lifecycle_event,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

impl EventCoalescer {
    /// Create a new coalescer backed by the given extension manager.
    pub fn new(manager: ExtensionManager) -> Self {
        Self {
            manager,
            pending: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            batch_buffer: Arc::new(Mutex::new(Vec::new())),
            batch_drain_scheduled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Dispatch a fire-and-forget event, coalescing if applicable.
    ///
    /// For coalescable events (`MessageUpdate`, `ToolExecutionUpdate`):
    /// - If no dispatch is in-flight for this event type, spawns one immediately.
    /// - If a dispatch is already in-flight, replaces the pending payload so
    ///   the in-flight task will dispatch the latest version on completion.
    ///
    /// For non-coalescable events, buffers the event and schedules a batch
    /// drain task that dispatches all buffered events in a single JS bridge
    /// call.  This saves ~21µs of fixed overhead per additional event in the
    /// batch.
    #[allow(clippy::too_many_lines)]
    pub(super) fn dispatch_fire_and_forget(
        &self,
        event: ExtensionEventName,
        data: CoalescedPayload,
        runtime_handle: &asupersync::runtime::RuntimeHandle,
    ) {
        let event_name_str = event.to_string();

        // Fast path: skip entirely if no hooks registered.
        if !self.manager.has_hook_for(&event_name_str) {
            return;
        }

        if !is_coalescable_event(&event) {
            // Non-coalescable: buffer for batch dispatch.
            {
                let mut buf = self
                    .batch_buffer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                buf.push((event, data));
            }

            // Schedule a drain task if one isn't already pending.
            if !self
                .batch_drain_scheduled
                .swap(true, std::sync::atomic::Ordering::AcqRel)
            {
                let manager = self.manager.clone();
                let buffer = self.batch_buffer.clone();
                let flag = self.batch_drain_scheduled.clone();
                runtime_handle.spawn(async move {
                    loop {
                        // Drain the buffer; events that arrived between scheduling
                        // and execution are included in this batch.
                        let raw = {
                            let mut buf = buffer
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            std::mem::take(&mut *buf)
                        };

                        if raw.is_empty() {
                            // No work left. Release the scheduled flag, but guard against
                            // a race where producers appended while the flag was still true.
                            flag.store(false, std::sync::atomic::Ordering::Release);
                            let should_continue = {
                                if buffer
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .is_empty()
                                {
                                    false
                                } else {
                                    !flag.swap(true, std::sync::atomic::Ordering::AcqRel)
                                }
                            };
                            if should_continue {
                                continue;
                            }
                            break;
                        }

                        // Resolve lazy payloads off the main thread.
                        let events = raw
                            .into_iter()
                            .map(|(evt, payload)| (evt, payload.resolve()))
                            .collect::<Vec<_>>();
                        let _ = manager.dispatch_event_batch(events).await;
                    }
                });
            }
            return;
        }

        // Coalescable path: check if a dispatch is already in-flight.
        {
            let mut in_flight = self
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if in_flight.contains(&event_name_str) {
                // Replace pending payload; the in-flight task will pick it up.
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(event_name_str, data);
                return;
            }
            in_flight.insert(event_name_str.clone());
        }

        let manager = self.manager.clone();
        let pending = self.pending.clone();
        let in_flight = self.in_flight.clone();
        let event_name_owned = event_name_str;
        runtime_handle.spawn(async move {
            let mut next_payload = Some(data);
            loop {
                let Some(payload) = next_payload.take() else {
                    break;
                };

                // Re-parse the event name back.
                let dispatch_event = match event_name_owned.as_str() {
                    "message_update" => ExtensionEventName::MessageUpdate,
                    "tool_execution_update" => ExtensionEventName::ToolExecutionUpdate,
                    _ => {
                        in_flight
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&event_name_owned);
                        break;
                    }
                };
                let _ = manager
                    .dispatch_event(dispatch_event, payload.resolve())
                    .await;

                // Fast path: drain pending replacement payload if present.
                if let Some(new_data) = {
                    let mut p = pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    p.remove(&event_name_owned)
                } {
                    next_payload = Some(new_data);
                    continue;
                }

                // Hand off atomically with writers (which lock in_flight then pending)
                // so we don't strand a payload that arrives right before completion.
                let maybe_new_data = {
                    let mut f = in_flight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let mut p = pending
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    p.remove(&event_name_owned).or_else(|| {
                        f.remove(&event_name_owned);
                        None
                    })
                };
                if let Some(new_data) = maybe_new_data {
                    next_payload = Some(new_data);
                    continue;
                }

                break;
            }
        });
    }

    /// Like [`dispatch_fire_and_forget`](Self::dispatch_fire_and_forget) but
    /// takes the raw [`AgentEvent`] and defers serialization until after
    /// verifying that a hook is actually registered.  This avoids the
    /// `serde_json::to_value()` cost (~2-5µs) for events that no extension
    /// listens to.
    pub fn dispatch_agent_event_lazy(
        &self,
        event: &AgentEvent,
        runtime_handle: &asupersync::runtime::RuntimeHandle,
    ) {
        let Some(event_name) = extension_event_name_from_agent(event) else {
            return;
        };
        if is_lifecycle_event(&event_name) {
            return;
        }
        let event_name_str = event_name.to_string();
        if !self.manager.has_hook_for(&event_name_str) {
            return;
        }
        // Hook exists — defer serialization to the async task.
        let event_clone = event.clone();
        let lazy = Box::new(move || serde_json::to_value(&event_clone).ok());
        self.dispatch_fire_and_forget(event_name, CoalescedPayload::Lazy(lazy), runtime_handle);
    }
}
