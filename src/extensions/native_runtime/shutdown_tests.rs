//! Exercise real native runtime entry points and shared shutdown admission.

use super::*;
use futures::executor::block_on;

fn loaded_extension(chunks: Arc<[Value]>) -> NativeRustLoadedExtension {
    NativeRustLoadedExtension {
        snapshot: JsExtensionSnapshot {
            id: "fixture".into(),
            name: "Fixture".into(),
            version: "1".into(),
            api_version: "1".into(),
            tools: vec![
                json!({"name":"fixture", "description":"Fixture", "parameters":{"type":"object"}}),
            ],
            slash_commands: vec![json!({"name":"command"})],
            shortcuts: vec![json!({"key_id":"ctrl+k"})],
            providers: Vec::new(),
            mcp_servers: Vec::new(),
            flags: Vec::new(),
            event_hooks: vec!["test".into()],
            active_tools: None,
        },
        event_responses: HashMap::from([("test".into(), json!({"event":"ran"}))]),
        tool_outputs: HashMap::from([("fixture".into(), json!({"tool":"ran"}))]),
        command_outputs: HashMap::from([("command".into(), json!({"command":"ran"}))]),
        shortcut_outputs: HashMap::from([("ctrl+k".into(), json!({"shortcut":"ran"}))]),
        provider_streams: HashMap::from([("fixture".into(), chunks)]),
    }
}

fn fixture() -> (
    NativeRustExtensionRuntimeHandle,
    std::sync::Weak<[Value]>,
    String,
) {
    let runtime = block_on(NativeRustExtensionRuntimeHandle::start()).unwrap();
    let chunks: Arc<[Value]> = vec![json!("first"), json!("second")].into();
    let weak = Arc::downgrade(&chunks);
    runtime
        .state
        .write()
        .unwrap()
        .load_extensions(vec![loaded_extension(chunks)]);
    block_on(runtime.set_flag_value("fixture".into(), "private-flag".into(), json!("value")))
        .unwrap();
    let id = block_on(runtime.provider_stream_simple_start(
        "fixture".into(),
        Value::Null,
        Value::Null,
        Value::Null,
        1000,
    ))
    .unwrap();
    (runtime, weak, id)
}

fn assert_closed<T>(result: Result<T>) {
    match result {
        Ok(_) => panic!("closed native runtime admitted work"),
        Err(error) => assert!(
            error.to_string().contains("PI_NATIVE_RUNTIME_CLOSED"),
            "{error}"
        ),
    }
}

#[test]
fn shutdown_releases_descriptors_stream_payloads_flags_and_indexes() {
    let (native, weak, id) = fixture();
    assert!(weak.upgrade().is_some());
    assert_eq!(
        block_on(native.execute_tool_ref("fixture", "call", Value::Null, 1000)).unwrap(),
        json!({"tool":"ran"})
    );
    assert!(block_on(native.shutdown(Duration::ZERO)));
    assert!(weak.upgrade().is_none());
    let mut state = native.state.write().unwrap();
    assert!(state.extensions.is_empty());
    assert!(state.registered_tools.is_empty());
    assert!(state.tool_extension_index.is_empty());
    assert!(state.command_extension_index.is_empty());
    assert!(state.shortcut_extension_index.is_empty());
    assert!(state.provider_stream_extension_index.is_empty());
    assert!(state.event_hook_extension_indexes.is_empty());
    assert!(state.flags.is_empty());
    assert!(state.repair_events.is_empty());
    assert!(state.streams.next(&id).is_err());
    drop(state);
}

#[test]
fn every_work_entrypoint_through_a_retained_clone_rejects_after_shutdown() {
    let (native, _, id) = fixture();
    let runtime = ExtensionRuntimeHandle::NativeRust(native.clone());
    assert!(block_on(native.shutdown(Duration::ZERO)));
    block_on(async {
        assert_closed(runtime.get_registered_tools().await);
        assert_closed(runtime.pump_once().await);
        assert_closed(
            runtime
                .dispatch_event("test".into(), Value::Null, Arc::new(Value::Null), 1000)
                .await,
        );
        assert_closed(
            runtime
                .dispatch_event_batch(
                    vec![("test".into(), Value::Null)],
                    Arc::new(Value::Null),
                    1000,
                )
                .await,
        );
        assert_closed(
            runtime
                .execute_tool(
                    "fixture".into(),
                    "call".into(),
                    Value::Null,
                    Arc::new(Value::Null),
                    1000,
                )
                .await,
        );
        assert_closed(
            runtime
                .execute_tool_ref("fixture", "call", Value::Null, Arc::new(Value::Null), 1000)
                .await,
        );
        assert_closed(
            runtime
                .execute_command("command".into(), "args".into(), Arc::new(Value::Null), 1000)
                .await,
        );
        assert_closed(
            runtime
                .execute_shortcut("ctrl+k".into(), Arc::new(Value::Null), 1000)
                .await,
        );
        assert_closed(
            runtime
                .set_flag_value("fixture".into(), "flag".into(), Value::Null)
                .await,
        );
        assert_closed(runtime.reset_transient_state().await);
        assert_closed(runtime.load_native_extensions_snapshots(Vec::new()).await);
        assert_closed(
            runtime
                .provider_stream_simple_start(
                    "fixture".into(),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    1000,
                )
                .await,
        );
        assert_closed(runtime.provider_stream_simple_next(id, 1000).await);
    });
}

#[test]
fn contended_shutdown_closes_admission_without_parking_and_can_be_retried() {
    let (native, weak, _) = fixture();
    let reader = native.state.read().unwrap();
    assert!(!block_on(native.shutdown(Duration::from_secs(1))));
    assert!(
        weak.upgrade().is_some(),
        "a failed drain must not claim to have reclaimed state"
    );
    // These calls must reject before attempting the state lock, including a
    // write request while this same thread still holds the read guard.
    assert_closed(block_on(native.get_registered_tools()));
    assert_closed(block_on(native.set_flag_value(
        "fixture".into(),
        "flag".into(),
        Value::Null,
    )));
    drop(reader);
    assert!(block_on(native.shutdown(Duration::ZERO)));
    assert!(weak.upgrade().is_none());
    assert!(block_on(native.shutdown(Duration::ZERO)));
}

#[test]
fn futures_created_before_shutdown_cannot_later_admit_provider_or_load_work() {
    let (native, _, _) = fixture();
    let start = native.provider_stream_simple_start(
        "fixture".into(),
        Value::Null,
        Value::Null,
        Value::Null,
        1000,
    );
    let load = native.load_extensions_snapshots(Vec::new());
    let other = native.clone();
    assert!(block_on(other.shutdown(Duration::ZERO)));
    assert_closed(block_on(start));
    assert_closed(block_on(load));
    assert_closed(native.write_running());
}

#[test]
fn shutdown_during_a_prepared_reload_prevents_installation() {
    let (native, _, _) = fixture();
    let prepared = vec![loaded_extension(vec![json!("replacement")].into())];
    assert!(block_on(native.shutdown(Duration::ZERO)));
    // This is the production post-read installation gate. Already-read values
    // cannot authorize an installation after another handle closes admission.
    let installed = native
        .write_running()
        .map(|mut state| state.load_extensions(prepared));
    assert_closed(installed);
    assert!(native.state.read().unwrap().extensions.is_empty());
}

#[test]
fn cleanup_remains_idempotent_after_shutdown_but_reset_cannot_reopen_it() {
    let (native, _, id) = fixture();
    assert!(block_on(native.shutdown(Duration::ZERO)));
    block_on(native.provider_stream_simple_cancel(id.clone(), 1000)).unwrap();
    native.provider_stream_simple_cancel_best_effort(id);
    assert!(block_on(native.drain_repair_events()).is_empty());
    assert_closed(block_on(native.reset_transient_state()));
    assert_closed(block_on(native.load_extensions_snapshots(Vec::new())));
    assert!(block_on(native.shutdown(Duration::ZERO)));
    let (fresh, _, fresh_id) = fixture();
    assert_eq!(
        block_on(fresh.provider_stream_simple_next(fresh_id, 1000)).unwrap(),
        Some(json!("first"))
    );
}

#[test]
fn poisoned_runtime_can_be_retired_without_reopening_its_admission() {
    let (native, weak, _) = fixture();
    let state = Arc::clone(&native.state);
    assert!(
        std::thread::spawn(move || {
            let _guard = state.write().unwrap();
            panic!("intentional state-lock poison");
        })
        .join()
        .is_err()
    );
    assert!(native.state.is_poisoned());
    assert!(block_on(native.shutdown(Duration::ZERO)));
    assert!(weak.upgrade().is_none());
    assert_closed(block_on(native.get_registered_tools()));
    assert!(block_on(native.shutdown(Duration::ZERO)));
}

#[test]
fn ordinary_reload_clears_old_transient_state_without_shutting_down() {
    let (native, _, old) = fixture();
    native
        .write_running()
        .unwrap()
        .load_extensions(vec![loaded_extension(vec![json!("replacement")].into())]);
    assert!(native.read_running().unwrap().flags.is_empty());
    assert!(block_on(native.provider_stream_simple_next(old, 1000)).is_err());
    let new = block_on(native.provider_stream_simple_start(
        "fixture".into(),
        Value::Null,
        Value::Null,
        Value::Null,
        1000,
    ))
    .unwrap();
    assert_eq!(
        block_on(native.provider_stream_simple_next(new, 1000)).unwrap(),
        Some(json!("replacement"))
    );
    assert_eq!(
        block_on(native.execute_tool_ref("fixture", "call", Value::Null, 1000)).unwrap(),
        json!({"tool":"ran"})
    );
}
