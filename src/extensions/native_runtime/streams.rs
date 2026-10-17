//! Native provider stream identities and bounded, explicit terminal receipts.
//!
//! A descriptor reload, reset or cancellation is not successful exhaustion.
//! Keep that distinction until the provider adapter observes it, and never let
//! a stale handle address a replacement stream, including in another runtime.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use super::{Error, Result};

const MAX_ACTIVE_STREAMS: usize = 256;
const MAX_COMPLETED_RECEIPTS: usize = 1024;
static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(0);

struct Cursor {
    chunks: Arc<[Value]>,
    next_index: usize,
}

#[derive(Default)]
pub(super) struct StreamRegistry {
    active: HashMap<String, Cursor>,
    completed: HashSet<String>,
    completion_order: VecDeque<String>,
}

impl fmt::Debug for StreamRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamRegistry")
            .field("active", &self.active.len())
            .field("completed", &self.completed.len())
            .field("completion_order", &self.completion_order.len())
            .finish()
    }
}

fn allocate_id(sequence: &AtomicU64) -> Result<String> {
    let previous = sequence
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| {
            Error::extension("PI_NATIVE_STREAM_LIMIT: native stream identity space exhausted")
        })?;
    // These process-local correlation IDs are not bearer authorization tokens.
    Ok(format!("native-stream-{}", previous + 1))
}

impl StreamRegistry {
    pub(super) fn start(&mut self, chunks: Arc<[Value]>) -> Result<String> {
        if self.active.len() >= MAX_ACTIVE_STREAMS {
            return Err(Error::extension(
                "PI_NATIVE_STREAM_LIMIT: too many active native provider streams; finish or cancel an existing stream",
            ));
        }
        let id = allocate_id(&NEXT_STREAM_ID)?;
        self.active.insert(
            id.clone(),
            Cursor {
                chunks,
                next_index: 0,
            },
        );
        Ok(id)
    }

    pub(super) fn next(&mut self, id: &str) -> Result<Option<Value>> {
        if let Some(cursor) = self.active.get_mut(id) {
            if let Some(value) = cursor.chunks.get(cursor.next_index) {
                let value = value.clone();
                cursor.next_index += 1;
                // Keep ownership until the caller observes EOF. Cancellation
                // after the final value but before EOF must still be an error.
                return Ok(Some(value));
            }
            self.active.remove(id);
            self.completed.insert(id.to_string());
            self.completion_order.push_back(id.to_string());
            if self.completion_order.len() > MAX_COMPLETED_RECEIPTS
                && let Some(expired) = self.completion_order.pop_front()
            {
                self.completed.remove(&expired);
            }
            return Ok(None);
        }
        if self.completed.contains(id) {
            return Ok(None);
        }
        Err(Error::extension(
            "PI_NATIVE_STREAM_RETIRED: native stream was cancelled, invalidated, or is unknown; it did not report normal exhaustion",
        ))
    }

    pub(super) fn cancel(&mut self, id: &str) {
        self.active.remove(id);
        // An already observed EOF stays EOF while its bounded receipt lives.
        // An unobserved EOF has no receipt and becomes a retired-handle error.
    }

    pub(super) fn clear(&mut self) {
        self.active.clear();
        self.completed.clear();
        self.completion_order.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunks() -> Arc<[Value]> {
        vec![json!("first"), json!("second")].into()
    }

    #[test]
    fn normal_exhaustion_delivers_every_value_and_retains_a_bounded_eof_receipt() {
        let mut registry = StreamRegistry::default();
        let id = registry.start(chunks()).unwrap();
        assert_eq!(registry.next(&id).unwrap(), Some(json!("first")));
        assert_eq!(registry.next(&id).unwrap(), Some(json!("second")));
        assert!(registry.next(&id).unwrap().is_none());
        registry.cancel(&id);
        assert!(registry.next(&id).unwrap().is_none());
        assert!(registry.active.is_empty());
        assert_eq!(registry.completed.len(), 1);
    }

    #[test]
    fn cancellation_and_unknown_handles_are_not_successful_eof() {
        let mut registry = StreamRegistry::default();
        let id = registry.start(chunks()).unwrap();
        registry.next(&id).unwrap();
        registry.cancel(&id);
        for id in [id.as_str(), "private-forged-handle"] {
            let message = registry.next(id).unwrap_err().to_string();
            assert!(message.contains("PI_NATIVE_STREAM_RETIRED"));
            assert!(!message.contains("private-forged-handle"));
        }
    }

    #[test]
    fn cancellation_between_last_value_and_eof_is_not_a_completion() {
        let mut registry = StreamRegistry::default();
        let id = registry.start(vec![json!("last")].into()).unwrap();
        assert_eq!(registry.next(&id).unwrap(), Some(json!("last")));
        registry.cancel(&id);
        assert!(registry.next(&id).is_err());
        assert!(registry.completed.is_empty());
    }

    #[test]
    fn reset_never_reuses_a_handle_or_cancels_a_replacement() {
        let mut registry = StreamRegistry::default();
        let old = registry.start(chunks()).unwrap();
        registry.clear();
        let new = registry.start(vec![json!("replacement")].into()).unwrap();
        assert_ne!(old, new);
        registry.cancel(&old);
        assert!(registry.next(&old).is_err());
        assert_eq!(registry.next(&new).unwrap(), Some(json!("replacement")));
    }

    #[test]
    fn handles_from_other_runtime_instances_cannot_address_local_streams() {
        let mut first = StreamRegistry::default();
        let mut second = StreamRegistry::default();
        let old = first.start(chunks()).unwrap();
        let new = second.start(chunks()).unwrap();
        assert_ne!(old, new);
        second.cancel(&old);
        assert!(second.next(&old).is_err());
        assert_eq!(second.next(&new).unwrap(), Some(json!("first")));
    }

    #[test]
    fn live_stream_capacity_never_evicts_accepted_work() {
        let mut registry = StreamRegistry::default();
        let ids = (0..MAX_ACTIVE_STREAMS)
            .map(|_| registry.start(chunks()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            registry
                .start(chunks())
                .unwrap_err()
                .to_string()
                .contains("PI_NATIVE_STREAM_LIMIT")
        );
        for id in &ids {
            assert_eq!(registry.next(id).unwrap(), Some(json!("first")));
        }
        registry.cancel(&ids[0]);
        assert!(registry.start(chunks()).is_ok());
    }

    #[test]
    fn completed_receipt_eviction_fails_closed_without_evicting_live_streams() {
        let mut registry = StreamRegistry::default();
        let live = registry.start(chunks()).unwrap();
        let first = registry.start(Vec::new().into()).unwrap();
        assert!(registry.next(&first).unwrap().is_none());
        for _ in 0..MAX_COMPLETED_RECEIPTS {
            let id = registry.start(Vec::new().into()).unwrap();
            assert!(registry.next(&id).unwrap().is_none());
        }
        assert_eq!(registry.completed.len(), MAX_COMPLETED_RECEIPTS);
        assert_eq!(registry.completion_order.len(), MAX_COMPLETED_RECEIPTS);
        assert!(registry.next(&first).is_err());
        assert_eq!(registry.next(&live).unwrap(), Some(json!("first")));
    }

    #[test]
    fn identity_exhaustion_is_an_error_instead_of_aliasing_the_last_stream() {
        let sequence = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_id(&sequence).unwrap(),
            format!("native-stream-{}", u64::MAX)
        );
        assert!(allocate_id(&sequence).is_err());
        assert!(allocate_id(&sequence).is_err());
        assert_eq!(sequence.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn cancellation_reset_and_completion_release_retained_payloads() {
        for action in ["cancel", "clear", "complete"] {
            let mut registry = StreamRegistry::default();
            let payload = chunks();
            let weak = Arc::downgrade(&payload);
            let id = registry.start(payload).unwrap();
            assert!(weak.upgrade().is_some());
            match action {
                "cancel" => registry.cancel(&id),
                "clear" => registry.clear(),
                _ => while registry.next(&id).unwrap().is_some() {},
            }
            assert!(weak.upgrade().is_none(), "{action}");
        }
    }

    async fn runtime(chunks: Arc<[Value]>) -> super::super::ExtensionRuntimeHandle {
        use super::super::{
            ExtensionRuntimeHandle, JsExtensionSnapshot, NativeRustExtensionRuntimeHandle,
            NativeRustLoadedExtension,
        };
        let runtime = NativeRustExtensionRuntimeHandle::start().await.unwrap();
        runtime
            .state
            .write()
            .unwrap()
            .load_extensions(vec![NativeRustLoadedExtension {
                snapshot: JsExtensionSnapshot {
                    id: "fixture".into(),
                    name: "Fixture".into(),
                    version: "1".into(),
                    api_version: "1".into(),
                    tools: Vec::new(),
                    slash_commands: Vec::new(),
                    shortcuts: Vec::new(),
                    providers: Vec::new(),
                    mcp_servers: Vec::new(),
                    flags: Vec::new(),
                    event_hooks: Vec::new(),
                    active_tools: None,
                },
                event_responses: HashMap::new(),
                tool_outputs: HashMap::new(),
                command_outputs: HashMap::new(),
                shortcut_outputs: HashMap::new(),
                provider_streams: HashMap::from([("fixture".to_string(), chunks)]),
            }]);
        ExtensionRuntimeHandle::NativeRust(runtime)
    }

    async fn start(runtime: &super::super::ExtensionRuntimeHandle) -> String {
        runtime
            .provider_stream_simple_start(
                "fixture".into(),
                Value::Null,
                Value::Null,
                Value::Null,
                1000,
            )
            .await
            .unwrap()
    }

    #[test]
    fn public_runtime_reset_retires_streams_without_affecting_the_next_request() {
        futures::executor::block_on(async {
            let runtime = runtime(chunks()).await;
            let old = start(&runtime).await;
            assert_eq!(
                runtime
                    .provider_stream_simple_next(old.clone(), 1000)
                    .await
                    .unwrap(),
                Some(json!("first"))
            );
            runtime.reset_transient_state().await.unwrap();
            let new = start(&runtime).await;
            runtime.provider_stream_simple_cancel_best_effort(old.clone());
            assert!(
                runtime
                    .provider_stream_simple_next(old, 1000)
                    .await
                    .is_err()
            );
            assert_eq!(
                runtime
                    .provider_stream_simple_next(new, 1000)
                    .await
                    .unwrap(),
                Some(json!("first"))
            );
        });
    }

    #[test]
    fn descriptor_replacement_cannot_rebind_an_old_stream_identity() {
        futures::executor::block_on(async {
            let runtime = runtime(chunks()).await;
            let old = start(&runtime).await;
            let super::super::ExtensionRuntimeHandle::NativeRust(native) = &runtime else {
                unreachable!()
            };
            {
                let mut state = native.state.write().unwrap();
                let mut replacement = state.extensions.clone();
                replacement[0]
                    .provider_streams
                    .insert("fixture".into(), vec![json!("new")].into());
                state.load_extensions(replacement);
            }
            let new = start(&runtime).await;
            assert_ne!(old, new);
            runtime
                .provider_stream_simple_cancel(old.clone(), 1000)
                .await
                .unwrap();
            assert!(
                runtime
                    .provider_stream_simple_next(old, 1000)
                    .await
                    .is_err()
            );
            assert_eq!(
                runtime
                    .provider_stream_simple_next(new, 1000)
                    .await
                    .unwrap(),
                Some(json!("new"))
            );
        });
    }

    #[test]
    fn runtime_cancellation_after_last_chunk_does_not_manufacture_success() {
        futures::executor::block_on(async {
            let runtime = runtime(vec![json!("partial")].into()).await;
            let id = start(&runtime).await;
            runtime
                .provider_stream_simple_next(id.clone(), 1000)
                .await
                .unwrap();
            runtime
                .provider_stream_simple_cancel(id.clone(), 1000)
                .await
                .unwrap();
            assert!(runtime.provider_stream_simple_next(id, 1000).await.is_err());
            let completed = start(&runtime).await;
            runtime
                .provider_stream_simple_next(completed.clone(), 1000)
                .await
                .unwrap();
            assert!(
                runtime
                    .provider_stream_simple_next(completed.clone(), 1000)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                runtime
                    .provider_stream_simple_next(completed, 1000)
                    .await
                    .unwrap()
                    .is_none()
            );
        });
    }
}
