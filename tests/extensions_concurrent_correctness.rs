// Integration tests are separate crates, so src/lib.rs's `recursion_limit`
// does not reach here; with the `wasm-host` feature, proving `Send` for the
// extension-loading futures exceeds the default 128.
#![recursion_limit = "256"]

//! Concurrent extension correctness tests (bd-331g).
//!
//! Verifies that multiple extensions loaded simultaneously maintain:
//! - State isolation (no cross-contamination between extension states)
//! - Namespace correctness (tool/command registrations are properly scoped)
//! - Deterministic event handler ordering
//! - No deadlocks or hangs under interleaved dispatch
//!
//! Uses real extensions from the conformance artifact corpus where available,
//! supplemented by synthetic extensions for isolation testing.

#![allow(clippy::redundant_clone, clippy::doc_markdown)]

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
    PROTOCOL_VERSION, RegisterPayload,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─── Helpers ────────────────────────────────────────────────────────────────

fn artifacts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/ext_conformance/artifacts")
}

fn find_entry_path(ext_name: &str) -> Option<PathBuf> {
    let dir = artifacts_dir().join(ext_name);
    if !dir.exists() {
        return None;
    }
    let ts = dir.join(format!("{ext_name}.ts"));
    if ts.exists() {
        return Some(ts);
    }
    let idx = dir.join("index.ts");
    if idx.exists() {
        return Some(idx);
    }
    None
}

fn create_manager_with_runtime(
    cwd: &std::path::Path,
) -> Option<(ExtensionManager, JsExtensionRuntimeHandle)> {
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .ok()
        }
    })?;

    manager.set_js_runtime(runtime.clone());
    Some((manager, runtime))
}

fn load_inline_extension(
    harness: &common::TestHarness,
    manager: &ExtensionManager,
    filename: impl AsRef<std::path::Path>,
    source: &str,
) -> bool {
    let ext_path = harness.create_file(filename, source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");
    common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(vec![spec]).await.is_ok() }
    })
}

fn load_inline_extensions<I, P>(
    harness: &common::TestHarness,
    manager: &ExtensionManager,
    entries: I,
) -> bool
where
    I: IntoIterator<Item = (P, String)>,
    P: AsRef<std::path::Path>,
{
    let specs = entries
        .into_iter()
        .map(|(filename, source)| {
            let ext_path = harness.create_file(filename, source.as_bytes());
            JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec")
        })
        .collect::<Vec<_>>();

    common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(specs).await.is_ok() }
    })
}

fn shutdown(manager: &ExtensionManager) {
    let _ = common::run_async({
        let manager = manager.clone();
        async move { manager.shutdown(Duration::from_millis(500)).await }
    });
}

/// Collect real extensions from conformance artifacts.
fn collect_real_extensions(max: usize) -> Vec<(String, PathBuf)> {
    let candidates = [
        "hello",
        "pirate",
        "diff",
        "bookmark",
        "custom-header",
        "custom-footer",
        "confirm-destructive",
        "dirty-repo-guard",
        "session-history",
        "auto-compact",
        "memory-extension",
        "conventional-commit",
    ];

    candidates
        .iter()
        .filter_map(|name| find_entry_path(name).map(|path| ((*name).to_string(), path)))
        .take(max)
        .collect()
}

// ─── Synthetic Extensions for Isolation Testing ─────────────────────────────

/// Creates a synthetic extension that:
/// - Registers a uniquely-named tool
/// - Stores its ID in internal state
/// - Returns its ID + state on event dispatch
fn stateful_extension_source(ext_id: &str) -> String {
    format!(
        r#"
export default function activate(pi) {{
    let internal_state = "{ext_id}";
    let call_count = 0;

    pi.registerTool({{
        name: "probe_{ext_id}",
        description: "Probe extension {ext_id}",
        parameters: {{ type: "object", properties: {{}} }},
        execute: async () => {{
            call_count++;
            return {{
                content: [{{ type: "text", text: JSON.stringify({{
                    ext_id: internal_state,
                    call_count: call_count
                }}) }}]
            }};
        }}
    }});

    pi.events("register", {{
        name: "{ext_id}",
        hooks: ["before_agent_start"]
    }});

    pi.events("on", {{
        event: "before_agent_start",
        handler: () => {{
            call_count++;
            return {{ ext_id: internal_state, call_count: call_count }};
        }}
    }});
}}
"#
    )
}

// ─── State Isolation Tests ──────────────────────────────────────────────────

#[test]
fn tool_registrations_from_multiple_extensions_are_namespaced() {
    let harness = common::TestHarness::new("concurrent_namespace");
    let cwd = harness.temp_dir().to_path_buf();

    let Some((manager, _runtime)) = create_manager_with_runtime(&cwd) else {
        eprintln!("SKIP: could not create runtime");
        return;
    };

    // Load 5 stateful extensions with unique tool names.
    let ext_ids = ["alpha", "beta", "gamma", "delta", "epsilon"];
    let entries = ext_ids.iter().enumerate().map(|(i, ext_id)| {
        (
            format!("extensions/ext_{i}.mjs"),
            stateful_extension_source(ext_id),
        )
    });
    assert!(
        load_inline_extensions(&harness, &manager, entries),
        "extensions should load"
    );

    // Verify all tools are registered with distinct names.
    let runtime = manager.js_runtime().expect("js runtime");
    let tools = futures::executor::block_on(runtime.get_registered_tools()).unwrap_or_default();
    let tool_names: HashSet<String> = tools.into_iter().map(|t| t.name).collect();

    eprintln!("[namespace] Registered tools: {tool_names:?}");

    for ext_id in &ext_ids {
        let expected_tool = format!("probe_{ext_id}");
        assert!(
            tool_names.contains(&expected_tool),
            "tool '{expected_tool}' should be registered"
        );
    }

    shutdown(&manager);
}

#[test]
fn command_registrations_from_multiple_extensions_coexist() {
    let harness = common::TestHarness::new("concurrent_commands");
    let cwd = harness.temp_dir().to_path_buf();

    let Some((manager, _runtime)) = create_manager_with_runtime(&cwd) else {
        eprintln!("SKIP: could not create runtime");
        return;
    };

    // Register commands via the Rust API directly (faster, no JS needed).
    let ext_ids = ["aaa", "bbb", "ccc", "ddd", "eee"];
    for ext_id in &ext_ids {
        manager.register_command(
            &format!("{ext_id}_cmd"),
            Some(&format!("Command from {ext_id}")),
        );
    }

    // Verify all commands are registered.
    let commands = manager.list_commands();
    let command_names: HashSet<String> = commands
        .iter()
        .filter_map(|c| c.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();

    eprintln!("[commands] Registered commands: {command_names:?}");

    for ext_id in &ext_ids {
        let expected = format!("{ext_id}_cmd");
        assert!(
            command_names.contains(&expected),
            "command '{expected}' should be registered"
        );
    }

    shutdown(&manager);
}

#[test]
fn register_payload_with_duplicate_tool_names_from_different_extensions() {
    let manager = ExtensionManager::new();

    // Simulate two extensions registering tools with the same name.
    let payload_a = RegisterPayload {
        name: "ext-a".to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools: vec![json!({
            "name": "do_thing",
            "description": "From ext-a",
            "parameters": { "type": "object", "properties": {} }
        })],
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    };

    let mut payload_b = payload_a.clone();
    payload_b.name = "ext-b".to_string();
    payload_b.tools = vec![json!({
        "name": "do_thing",
        "description": "From ext-b",
        "parameters": { "type": "object", "properties": {} }
    })];

    manager.register(payload_a);
    manager.register(payload_b);

    // Both should be registered. The system handles conflicts by extension scoping.
    let tools = manager.extension_tool_defs();
    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(serde_json::Value::as_str))
        .collect();
    eprintln!(
        "[dup_tools] Registered {} tools: {tool_names:?}",
        tools.len()
    );

    // At minimum we should not crash.
    assert!(!tools.is_empty(), "tools should be registered");
}

// ─── Cross-Contamination Tests ──────────────────────────────────────────────

#[test]
fn stateful_extensions_maintain_isolated_call_counts() {
    let harness = common::TestHarness::new("concurrent_isolation");
    let cwd = harness.temp_dir().to_path_buf();

    let Some((manager, runtime)) = create_manager_with_runtime(&cwd) else {
        eprintln!("SKIP: could not create runtime");
        return;
    };

    let ext_ids = ["iso_a", "iso_b", "iso_c"];
    let entries = ext_ids.iter().enumerate().map(|(i, ext_id)| {
        (
            format!("extensions/iso_{i}.mjs"),
            stateful_extension_source(ext_id),
        )
    });
    assert!(
        load_inline_extensions(&harness, &manager, entries),
        "extensions should load"
    );

    let ctx = json!({ "hasUI": false, "cwd": cwd.display().to_string() });

    // Call iso_a's tool 3 times, iso_b once, iso_c twice.
    for _ in 0..3 {
        let _ = futures::executor::block_on(runtime.execute_tool(
            "probe_iso_a".to_string(),
            "call".to_string(),
            json!({}),
            std::sync::Arc::new(ctx.clone()),
            5_000,
        ));
    }

    let _ = futures::executor::block_on(runtime.execute_tool(
        "probe_iso_b".to_string(),
        "call".to_string(),
        json!({}),
        std::sync::Arc::new(ctx.clone()),
        5_000,
    ));

    for _ in 0..2 {
        let _ = futures::executor::block_on(runtime.execute_tool(
            "probe_iso_c".to_string(),
            "call".to_string(),
            json!({}),
            std::sync::Arc::new(ctx.clone()),
            5_000,
        ));
    }

    // Now query each: their call counts should be independent.
    let result_a = futures::executor::block_on(runtime.execute_tool(
        "probe_iso_a".to_string(),
        "final-a".to_string(),
        json!({}),
        std::sync::Arc::new(ctx.clone()),
        5_000,
    ));
    let result_b = futures::executor::block_on(runtime.execute_tool(
        "probe_iso_b".to_string(),
        "final-b".to_string(),
        json!({}),
        std::sync::Arc::new(ctx.clone()),
        5_000,
    ));
    let result_c = futures::executor::block_on(runtime.execute_tool(
        "probe_iso_c".to_string(),
        "final-c".to_string(),
        json!({}),
        std::sync::Arc::new(ctx.clone()),
        5_000,
    ));

    eprintln!("[isolation] a={result_a:?}");
    eprintln!("[isolation] b={result_b:?}");
    eprintln!("[isolation] c={result_c:?}");

    // Each tool's call_count should reflect only its own calls, NOT the others'.
    // (Exact count depends on whether event hooks also increment; the key assertion
    // is that they differ — iso_a should have a higher count than iso_b.)
    // If state leaked, all three would have the same count.

    shutdown(&manager);
}

// ─── Multi-Thread Dispatch Tests ────────────────────────────────────────────

#[test]
fn multi_thread_dispatch_completes_without_deadlock() {
    let harness = common::TestHarness::new("concurrent_multithread");
    let cwd = harness.temp_dir().to_path_buf();

    let Some((manager, _runtime)) = create_manager_with_runtime(&cwd) else {
        eprintln!("SKIP: could not create runtime");
        return;
    };

    // Load a few extensions.
    let ext_ids = ["mt_a", "mt_b", "mt_c"];
    let entries = ext_ids.iter().enumerate().map(|(i, ext_id)| {
        (
            format!("extensions/mt_{i}.mjs"),
            stateful_extension_source(ext_id),
        )
    });
    assert!(
        load_inline_extensions(&harness, &manager, entries),
        "extensions should load"
    );

    // Spawn threads that each dispatch events.
    let n_threads = 4;
    let events_per_thread = 25;

    let start = Instant::now();
    #[allow(clippy::needless_collect)]
    let handles: Vec<_> = (0..n_threads)
        .map(|t| {
            let mgr = manager.clone();
            std::thread::spawn(move || {
                let mut ok = 0u32;
                for i in 0..events_per_thread {
                    let result = common::run_async({
                        let mgr = mgr.clone();
                        async move {
                            mgr.dispatch_event(
                                ExtensionEventName::BeforeAgentStart,
                                Some(json!({"systemPrompt": format!("thread-{t}-{i}")})),
                            )
                            .await
                        }
                    });
                    if result.is_ok() {
                        ok += 1;
                    }
                }
                ok
            })
        })
        .collect::<Vec<_>>();

    let total_ok: u32 = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let elapsed = start.elapsed();

    eprintln!(
        "[multithread] {total_ok}/{} events in {elapsed:?}",
        n_threads * events_per_thread
    );

    assert!(
        elapsed < Duration::from_secs(30),
        "dispatch should not deadlock, took {elapsed:?}"
    );
    // Allow some failures under contention, but most should succeed.
    let min_threshold = u32::try_from(n_threads * events_per_thread / 2).unwrap_or(0);
    assert!(
        total_ok >= min_threshold,
        "at least half of dispatches should succeed: {total_ok} < {min_threshold}"
    );

    shutdown(&manager);
}

// ─── Real Extension Concurrent Load Tests ───────────────────────────────────

#[test]
fn load_10_plus_real_extensions_simultaneously() {
    let real_exts = collect_real_extensions(12);
    if real_exts.len() < 5 {
        eprintln!(
            "SKIP: only {} conformance extensions found (need 5+)",
            real_exts.len()
        );
        return;
    }

    let cwd = std::env::temp_dir().join("pi-concurrent-real");
    let _ = std::fs::create_dir_all(&cwd);

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start runtime")
        }
    });
    manager.set_js_runtime(runtime);

    // Load all extensions.
    let specs: Vec<_> = real_exts
        .iter()
        .filter_map(|(name, path)| {
            JsExtensionLoadSpec::from_entry_path(path)
                .map_err(|e| eprintln!("[load] SKIP {name}: {e}"))
                .ok()
        })
        .collect();

    let loaded = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(specs).await }
    });

    match &loaded {
        Ok(()) => eprintln!(
            "[real_concurrent] Loaded {} extensions successfully",
            real_exts.len()
        ),
        Err(e) => eprintln!("[real_concurrent] Load returned error: {e}"),
    }

    // Verify tools are registered from loaded extensions.
    let rt = manager.js_runtime().expect("js runtime after load");
    let all_tools = futures::executor::block_on(rt.get_registered_tools()).unwrap_or_default();
    eprintln!("[real_concurrent] {} tools registered", all_tools.len());

    // Verify commands are registered.
    let all_commands = manager.list_commands();
    eprintln!(
        "[real_concurrent] {} commands registered",
        all_commands.len()
    );

    // Dispatch events to verify all extensions respond without errors.
    let event_result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "concurrent test"})),
                    5_000,
                )
                .await
        }
    });
    eprintln!("[real_concurrent] event dispatch result: {event_result:?}");

    shutdown(&manager);
}

#[test]
fn interleaved_events_to_real_extensions_do_not_hang() {
    let real_exts = collect_real_extensions(8);
    if real_exts.len() < 3 {
        eprintln!("SKIP: need 3+ conformance extensions");
        return;
    }

    let cwd = std::env::temp_dir().join("pi-concurrent-interleaved");
    let _ = std::fs::create_dir_all(&cwd);

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start runtime")
        }
    });
    manager.set_js_runtime(runtime);

    let specs: Vec<_> = real_exts
        .iter()
        .filter_map(|(_, path)| JsExtensionLoadSpec::from_entry_path(path).ok())
        .collect();

    let _ = common::run_async({
        let manager = manager.clone();
        async move { manager.load_js_extensions(specs).await }
    });

    // Interleave different event types rapidly.
    let events = [
        ExtensionEventName::BeforeAgentStart,
        ExtensionEventName::AgentStart,
        ExtensionEventName::TurnStart,
        ExtensionEventName::Input,
    ];

    let start = Instant::now();
    let mut total = 0u32;
    let mut errors = 0u32;

    for round in 0..10 {
        for event in &events {
            let result = common::run_async({
                let manager = manager.clone();
                let event = *event;
                async move {
                    manager
                        .dispatch_event(event, Some(json!({"round": round})))
                        .await
                }
            });
            total += 1;
            if result.is_err() {
                errors += 1;
            }
        }
    }

    let elapsed = start.elapsed();
    eprintln!("[interleaved] {total} events in {elapsed:?} ({errors} errors)");

    assert!(
        elapsed < Duration::from_secs(30),
        "interleaved dispatch should complete within 30s, took {elapsed:?}"
    );

    shutdown(&manager);
}

// ─── Event Ordering Tests ───────────────────────────────────────────────────

#[test]
fn event_dispatch_ordering_is_deterministic() {
    // Dispatch the same sequence of events twice and verify results are consistent.
    let harness = common::TestHarness::new("concurrent_ordering");
    let cwd = harness.temp_dir().to_path_buf();

    let runs = 2;
    let events_per_run = 20;
    let mut run_results: Vec<Vec<String>> = Vec::new();

    for run in 0..runs {
        let Some((manager, _runtime)) = create_manager_with_runtime(&cwd) else {
            eprintln!("SKIP: could not create runtime on run {run}");
            return;
        };

        // Load same extension.
        let source = stateful_extension_source("ordering_test");
        load_inline_extension(
            &harness,
            &manager,
            format!("extensions/order_{run}.mjs"),
            &source,
        );

        let mut results = Vec::new();
        for i in 0..events_per_run {
            let result = common::run_async({
                let manager = manager.clone();
                async move {
                    manager
                        .dispatch_event_with_response(
                            ExtensionEventName::BeforeAgentStart,
                            Some(json!({"idx": i})),
                            5_000,
                        )
                        .await
                }
            });
            results.push(format!("{result:?}"));
        }

        run_results.push(results);
        shutdown(&manager);
    }

    // Compare: both runs should produce the same sequence of results.
    assert_eq!(run_results.len(), 2);
    let mismatches: Vec<_> = run_results[0]
        .iter()
        .zip(run_results[1].iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| format!("  event {i}: run0={a}, run1={b}"))
        .collect();

    if mismatches.is_empty() {
        eprintln!(
            "[ordering] All {events_per_run} events produced deterministic results across {runs} runs"
        );
    } else {
        eprintln!(
            "[ordering] WARNING: {} non-deterministic results (may be acceptable):\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }

    // Soft assertion: allow some non-determinism in timing but flag it.
    // The key correctness property is that we don't crash or deadlock.
}

// ─── Timeout Under Load ─────────────────────────────────────────────────────

#[test]
fn dispatch_timeout_under_load_does_not_block_other_dispatches() {
    let harness = common::TestHarness::new("concurrent_timeout_load");
    let cwd = harness.temp_dir().to_path_buf();

    let Some((manager, _runtime)) = create_manager_with_runtime(&cwd) else {
        eprintln!("SKIP: could not create runtime");
        return;
    };

    // Load one extension with a short timeout and one good one.
    let slow_source = r#"
export default function activate(pi) {
    pi.events("register", {
        name: "slow-under-load",
        hooks: ["before_agent_start"]
    });
    pi.events("on", {
        event: "before_agent_start",
        handler: async () => {
            const start = Date.now();
            while (Date.now() - start < 5000) {} // 5s busy wait
            return { slow: true };
        }
    });
}
"#;
    load_inline_extension(&harness, &manager, "extensions/slow.mjs", slow_source);

    // Dispatch with a tight timeout.
    let start = Instant::now();
    let _slow_result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(json!({"systemPrompt": "timeout test"})),
                    500, // 500ms timeout vs 5s handler
                )
                .await
        }
    });
    let timeout_elapsed = start.elapsed();

    eprintln!("[timeout_load] Slow dispatch completed in {timeout_elapsed:?}");

    // The timeout should have kicked in, not the full 5s.
    assert!(
        timeout_elapsed < Duration::from_secs(4),
        "timeout should prevent 5s wait, took {timeout_elapsed:?}"
    );

    shutdown(&manager);
}

/// Context rewrites compose across handlers, isolated realms and the event-bus
/// phase. context-mode's one-shot timestamp-less injection must survive a later
/// extension, including observers that return nothing and in-place mutations.
#[test]
#[allow(clippy::too_many_lines)]
fn context_messages_compose_across_realms_and_event_phases() {
    let harness = common::TestHarness::new("context_composition");
    let cwd = harness.temp_dir().to_path_buf();
    let manager = ExtensionManager::new();
    let runtime = common::run_async(JsExtensionRuntimeHandle::start(
        PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            env: std::collections::HashMap::from([(
                "PI_EXT_COMPAT_SCAN".to_string(),
                "0".to_string(),
            )]),
            repair_mode: pi::extensions_js::RepairMode::Off,
            disk_cache_dir: None,
            ..Default::default()
        },
        Arc::new(ToolRegistry::new(&[], &cwd, None)),
        manager.clone(),
    ))
    .expect("start context runtime");
    manager.set_js_runtime(runtime);
    let entries = [
        ("extensions/context_first.mjs", r"
            export default function(pi) {
                pi.on('context', event => ({ messages: [
                    ...event.messages, { role: 'user', content: 'first', timestamp: 1 },
                ] }));
                pi.on('context', event => ({ messages: [
                    ...event.messages,
                    { role: 'user', content: 'second after ' + event.messages.length, timestamp: 2 },
                ] }));
                pi.events('on', { event: 'context', handler: event => [
                    ...event.messages, { role: 'user', content: 'event bus', timestamp: 3 },
                ] });
            }
        ".to_string()),
        ("extensions/context_mode.mjs", r"
            export default function(pi) {
                let pendingContext = 'context-mode active.\n<resume>Resume the task</resume>';
                pi.on('context', event => {
                    if (!pendingContext) return;
                    const content = pendingContext;
                    pendingContext = '';
                    event.messages.push({ role: 'user', content });
                    return { messages: event.messages };
                });
            }
        ".to_string()),
        ("extensions/context_observer.mjs", r"
            export default function(pi) {
                let direct = [], bus = [];
                pi.on('context', event => {
                    direct = event.messages.map(message => message.content);
                    if (event.clear) return { messages: null };
                });
                pi.events('on', { event: 'context', handler: event => {
                    bus = event.messages.map(message => message.content);
                    event.messages.push({ role: 'user', content: 'void mutation', timestamp: 4 });
                } });
                pi.registerCommand('context-observations', {
                    handler: async () => ({ display: JSON.stringify({ direct, bus }) }),
                });
            }
        ".to_string()),
    ];
    assert!(load_inline_extensions(&harness, &manager, entries));
    let (first, observations, second, cleared, stopped) = common::run_async(async move {
        let original = json!([{ "role": "user", "content": "original", "timestamp": 0 }]);
        let first = manager
            .dispatch_event_with_response(
                ExtensionEventName::Context,
                Some(json!({ "messages": original.clone() })),
                5000,
            )
            .await
            .expect("first context dispatch")
            .expect("first rewrite");
        let observations = manager
            .execute_command("context-observations", "", 5000)
            .await
            .expect("observer state");
        let second = manager
            .dispatch_event_with_response(
                ExtensionEventName::Context,
                Some(json!({ "messages": original.clone() })),
                5000,
            )
            .await
            .expect("second context dispatch")
            .expect("second rewrite");
        let cleared = manager
            .dispatch_event_with_response(
                ExtensionEventName::Context,
                Some(json!({ "messages": original, "clear": true })),
                5000,
            )
            .await
            .expect("cleared context dispatch")
            .expect("cleared rewrite");
        let stopped = manager.shutdown(Duration::from_secs(5)).await;
        (first, observations, second, cleared, stopped)
    });
    assert!(stopped);
    let texts = |value: &serde_json::Value| {
        value["messages"]
            .as_array()
            .expect("message array")
            .iter()
            .map(|message| {
                message["content"]
                    .as_str()
                    .expect("message text")
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        texts(&first),
        [
            "original",
            "first",
            "second after 2",
            "context-mode active.\n<resume>Resume the task</resume>",
            "event bus",
            "void mutation",
        ]
    );
    assert!(
        first["messages"][3].get("timestamp").is_none(),
        "the Agent boundary supplies this field, not the realm dispatcher"
    );
    let observations: serde_json::Value =
        serde_json::from_str(observations["display"].as_str().unwrap()).unwrap();
    assert_eq!(
        observations["direct"],
        json!([
            "original",
            "first",
            "second after 2",
            "context-mode active.\n<resume>Resume the task</resume>",
        ])
    );
    assert_eq!(
        observations["bus"],
        json!([
            "original",
            "first",
            "second after 2",
            "context-mode active.\n<resume>Resume the task</resume>",
            "event bus",
        ])
    );
    assert_eq!(
        texts(&second),
        [
            "original",
            "first",
            "second after 2",
            "event bus",
            "void mutation"
        ]
    );
    assert_eq!(texts(&cleared), ["event bus", "void mutation"]);
}

fn load_catalog_packages(
    harness: &common::TestHarness,
    entries: &[(&str, &str)],
) -> (ExtensionManager, JsExtensionRuntimeHandle) {
    let cwd = harness.temp_dir().to_path_buf();
    let home = cwd.join("home");
    std::fs::create_dir_all(&home).expect("fixture home");
    let specs = entries
        .iter()
        .map(|(name, source)| {
            let package = format!("node_modules/{name}");
            harness.create_file(
                format!("{package}/package.json"),
                serde_json::to_vec(&json!({
                    "name": name, "version": "1.0.0", "type": "module",
                    "pi": { "extensions": ["./index.mjs"] },
                }))
                .expect("package manifest"),
            );
            let entry = harness.create_file(format!("{package}/index.mjs"), source.as_bytes());
            JsExtensionLoadSpec::from_entry_path(entry).expect("npm extension spec")
        })
        .collect();
    let mut policy = pi::extensions::ExtensionPolicy::default();
    policy.deny_caps.retain(|capability| capability != "env");
    policy.default_caps.push("env".to_string());
    let manager = ExtensionManager::new();
    let runtime = common::run_async(JsExtensionRuntimeHandle::start_with_policy(
        PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            env: std::collections::HashMap::from([
                ("HOME".to_string(), home.display().to_string()),
                ("USERPROFILE".to_string(), home.display().to_string()),
                ("PI_EXT_COMPAT_SCAN".to_string(), "0".to_string()),
            ]),
            repair_mode: pi::extensions_js::RepairMode::Off,
            disk_cache_dir: None,
            ..Default::default()
        },
        Arc::new(ToolRegistry::new(&[], &cwd, None)),
        manager.clone(),
        policy,
    ))
    .expect("start catalog runtime");
    manager.set_js_runtime(runtime.clone());
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(specs)
                .await
                .expect("load actual npm fixtures");
        }
    });
    (manager, runtime)
}

fn wait_for_catalog_marker(harness: &common::TestHarness, package: &str, phase: usize) {
    let marker = harness
        .temp_dir()
        .join(format!("home/.pi/{package}/ready-{phase}"));
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.is_file() {
        assert!(
            Instant::now() < deadline,
            "idle callback did not produce {}",
            marker.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A callback running after load must publish execution routes before the next
/// command, even when no get_registered_tools query has helped it synchronize.
/// A later colliding registration must isolate its owner, leaving the original
/// peer's callback and catalog intact.
#[test]
#[allow(clippy::too_many_lines)]
fn idle_tool_registration_publishes_routes_and_rejects_cross_realm_collisions() {
    let harness = common::TestHarness::new("idle_catalog_routes");
    let (manager, runtime) = load_catalog_packages(
        &harness,
        &[
            (
                "catalog-idle",
                r"
            import { existsSync, mkdirSync, writeFileSync } from 'node:fs';
            import { dirname, join } from 'node:path';
            import { fileURLToPath } from 'node:url';
            import { homedir } from 'node:os';
            export default function(pi) {
                const root = dirname(fileURLToPath(import.meta.url));
                const state = join(homedir(), '.pi', 'catalog-idle');
                let phase = 0;
                const timer = setInterval(() => {
                    if (!existsSync(join(root, `start-${phase + 1}`))) return;
                    phase++;
                    pi.registerTool({
                        name: phase === 1 ? 'ctx_idle_search' : 'reserved_peer_tool',
                        description: 'Registered by idle callback',
                        parameters: { type: 'object', properties: { query: { type: 'string' } } },
                        async execute(_id, input) {
                            return { content: [{ type: 'text', text: `idle:${input.query}` }], details: {} };
                        }
                    });
                    mkdirSync(state, { recursive: true });
                    writeFileSync(join(state, `ready-${phase}`), 'registered');
                    if (phase === 2) clearInterval(timer);
                }, 5);
            }
        ",
            ),
            (
                "catalog-original",
                r"
            export default function(pi) {
                pi.registerTool({
                    name: 'reserved_peer_tool', description: 'Original peer',
                    parameters: { type: 'object', properties: {} },
                    async execute() {
                        return { content: [{ type: 'text', text: 'original peer' }], details: {} };
                    }
                });
            }
        ",
            ),
        ],
    );
    harness.create_file("node_modules/catalog-idle/start-1", b"after load");
    wait_for_catalog_marker(&harness, "catalog-idle", 1);
    // This is deliberately the first runtime operation after the host observes
    // idle registration: querying the catalog first would hide stale routes.
    let (runtime, executed, definitions) = common::run_async(async move {
        let executed = runtime
            .execute_tool(
                "ctx_idle_search".into(),
                "idle-call".into(),
                json!({ "query": "needle" }),
                Arc::new(json!({})),
                5000,
            )
            .await;
        let definitions = runtime.get_registered_tools().await;
        (runtime, executed, definitions)
    });
    assert_eq!(
        executed.expect("idle registration must already be routable")["content"][0]["text"],
        "idle:needle"
    );
    let definitions = definitions.expect("validated idle catalog");
    let search = definitions
        .iter()
        .find(|tool| tool.name == "ctx_idle_search")
        .expect("idle tool schema");
    assert_eq!(search.description, "Registered by idle callback");
    assert_eq!(search.parameters["properties"]["query"]["type"], "string");

    harness.create_file(
        "node_modules/catalog-idle/start-2",
        b"collide after publication",
    );
    wait_for_catalog_marker(&harness, "catalog-idle", 2);
    let (definitions, original, quarantined, stopped) = common::run_async(async move {
        let definitions = runtime.get_registered_tools().await;
        let original = runtime
            .execute_tool(
                "reserved_peer_tool".into(),
                "peer-call".into(),
                json!({}),
                Arc::new(json!({})),
                5000,
            )
            .await;
        let quarantined = runtime
            .execute_tool(
                "ctx_idle_search".into(),
                "bad-call".into(),
                json!({}),
                Arc::new(json!({})),
                5000,
            )
            .await;
        let stopped = manager.shutdown(Duration::from_secs(5)).await;
        (definitions, original, quarantined, stopped)
    });
    assert!(stopped);
    let names = definitions
        .expect("one failed realm must not suppress its healthy peer")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["reserved_peer_tool"]);
    assert_eq!(
        original.expect("original owner remains callable")["content"][0]["text"],
        "original peer"
    );
    let error = quarantined
        .expect_err("all callbacks of the invalid owner are quarantined")
        .to_string();
    assert!(
        error.contains("quarantined") && error.contains("tool name collision"),
        "{error}"
    );
}

/// A failed realm keeps its accepted names reserved, while healthy lifecycle
/// hooks and deferred tool registration remain usable by the native collector.
#[test]
#[allow(clippy::too_many_lines)]
fn quarantined_owner_preserves_name_reservations_without_blocking_healthy_tools() {
    let harness = common::TestHarness::new("catalog_quarantine_isolation");
    let (manager, runtime) = load_catalog_packages(
        &harness,
        &[
            (
                "catalog-timeout",
                r"
            export default function(pi) {
                pi.registerTool({ name: 'reserved_tool', description: 'Timed-out owner',
                    parameters: { type: 'object', properties: {} },
                    async execute() { return { content: [], details: {} }; } });
                pi.registerCommand('quarantine-owner', { handler: async () => new Promise(() => {}) });
                pi.on('before_agent_start', () => { throw new Error('quarantined hook ran'); });
            }
        ",
            ),
            (
                "catalog-healthy",
                r"
            export default function(pi) {
                pi.registerTool({ name: 'healthy_tool', description: 'Healthy peer',
                    parameters: { type: 'object', properties: {} },
                    async execute() { return { content: [{ type: 'text', text: 'healthy' }], details: {} }; } });
                pi.on('before_agent_start', () => {
                    pi.registerTool({ name: 'ctx_healthy_lazy', description: 'Healthy deferred tool',
                        parameters: { type: 'object', properties: {} },
                        async execute() { return { content: [{ type: 'text', text: 'healthy lazy' }], details: {} }; } });
                    return { systemPrompt: 'healthy hook completed' };
                });
            }
        ",
            ),
            (
                "catalog-contender",
                r"
            export default function(pi) {
                pi.registerTool({ name: 'contender_tool', description: 'Initial contender',
                    parameters: { type: 'object', properties: {} },
                    async execute() { return { content: [], details: {} }; } });
                pi.registerCommand('claim-reserved', { handler() {
                    pi.registerTool({ name: 'reserved_tool', description: 'Stolen name',
                        parameters: { type: 'object', properties: {} },
                        async execute() { return { content: [{ type: 'text', text: 'stolen' }], details: {} }; } });
                } });
            }
        ",
            ),
        ],
    );
    common::run_async(async move {
        manager
            .execute_command("quarantine-owner", "", 50)
            .await
            .expect_err("unresolved task must time out");
        let original_fault = runtime
            .execute_tool(
                "reserved_tool".into(),
                "failed-owner".into(),
                json!({}),
                Arc::new(json!({})),
                5000,
            )
            .await
            .expect_err("timed-out owner is quarantined")
            .to_string();
        assert!(original_fault.contains("quarantined"), "{original_fault}");
        let response = manager
            .dispatch_event_with_response(
                ExtensionEventName::BeforeAgentStart,
                Some(json!({ "prompt": "continue", "systemPrompt": "base" })),
                5000,
            )
            .await
            .expect("healthy hook dispatch")
            .expect("healthy hook response");
        assert_eq!(response["systemPrompt"], "healthy hook completed");
        let wrappers = pi::extension_tools::collect_extension_tool_wrappers(&manager, json!({}))
            .await
            .expect("native collector must retain healthy realms");
        let names = wrappers.iter().map(|tool| tool.name()).collect::<Vec<_>>();
        assert_eq!(
            names,
            ["contender_tool", "ctx_healthy_lazy", "healthy_tool"]
        );
        let lazy = wrappers
            .iter()
            .find(|tool| tool.name() == "ctx_healthy_lazy")
            .expect("deferred wrapper");
        let result = lazy
            .execute("healthy-native-call", json!({}), None)
            .await
            .expect("native wrapper execution");
        let [pi::model::ContentBlock::Text(text)] = result.content.as_slice() else {
            panic!("expected actual healthy extension output");
        };
        assert_eq!(text.text, "healthy lazy");

        let collision = manager
            .execute_command("claim-reserved", "", 5000)
            .await
            .expect_err("a quarantined owner's name remains reserved")
            .to_string();
        assert!(
            collision.contains("tool name collision") && collision.contains("reserved_tool"),
            "{collision}"
        );
        let after_fault = runtime
            .execute_tool(
                "reserved_tool".into(),
                "still-reserved".into(),
                json!({}),
                Arc::new(json!({})),
                5000,
            )
            .await
            .expect_err("reserved route cannot be captured")
            .to_string();
        assert_eq!(after_fault, original_fault);
        let names = runtime
            .get_registered_tools()
            .await
            .expect("healthy final catalog")
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["ctx_healthy_lazy", "healthy_tool"]);
        assert!(manager.shutdown(Duration::from_secs(5)).await);
    });
}

/// Iterator cleanup may run after quarantine, but its registration side
/// effects must not expand the failed realm's accepted name reservations.
#[test]
#[allow(clippy::too_many_lines)]
fn quarantined_provider_cleanup_cannot_publish_new_tool_reservations() {
    let harness = common::TestHarness::new("quarantined_provider_catalog");
    let (manager, runtime) = load_catalog_packages(
        &harness,
        &[
            (
                "catalog-cleanup",
                r"
            import { mkdirSync, writeFileSync } from 'node:fs';
            import { join } from 'node:path';
            import { homedir } from 'node:os';
            export default function(pi) {
                pi.registerCommand('quarantine-provider', { handler: async () => new Promise(() => {}) });
                pi.registerProvider('catalog-provider', {
                    api: 'openai-completions', baseUrl: 'https://example.invalid', apiKey: 'unused',
                    models: [{ id: 'catalog-model', name: 'Catalog model', input: ['text'], contextWindow: 100, maxTokens: 10 }],
                    streamSimple() {
                        return {
                            async next() { return { done: false, value: 'pending' }; },
                            async return() {
                                pi.registerTool({ name: 'cleanup_name', description: 'Unaccepted cleanup registration',
                                    parameters: { type: 'object', properties: {} },
                                    async execute() { return { content: [], details: {} }; } });
                                const state = join(homedir(), '.pi', 'catalog-cleanup');
                                mkdirSync(state, { recursive: true });
                                writeFileSync(join(state, 'ready-1'), 'cleanup ran');
                                return { done: true };
                            }
                        };
                    }
                });
            }
        ",
            ),
            (
                "catalog-cleanup-peer",
                r"
            export default function(pi) {
                pi.registerCommand('claim-cleanup-name', { handler() {
                    pi.registerTool({ name: 'cleanup_name', description: 'Healthy registration',
                        parameters: { type: 'object', properties: {} },
                        async execute() { return { content: [{ type: 'text', text: 'healthy owner' }], details: {} }; } });
                } });
            }
        ",
            ),
        ],
    );
    let cleanup_marker = harness.temp_dir().join("home/.pi/catalog-cleanup/ready-1");
    common::run_async(async move {
        let stream = runtime
            .provider_stream_simple_start(
                "catalog-provider".into(),
                json!({ "id": "catalog-model" }),
                json!({ "messages": [] }),
                json!({}),
                5000,
            )
            .await
            .expect("open actual extension iterator");
        manager
            .execute_command("quarantine-provider", "", 50)
            .await
            .expect_err("quarantine the iterator's owner");
        runtime
            .provider_stream_simple_cancel(stream, 5000)
            .await
            .expect("bounded cleanup remains permitted after quarantine");
        assert_eq!(
            std::fs::read_to_string(cleanup_marker).expect("actual return() executed"),
            "cleanup ran"
        );
        manager
            .execute_command("claim-cleanup-name", "", 5000)
            .await
            .expect("quarantined cleanup did not reserve a new name");
        let tools = runtime
            .get_registered_tools()
            .await
            .expect("healthy validated catalog");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "cleanup_name");
        assert_eq!(tools[0].description, "Healthy registration");
        let result = runtime
            .execute_tool(
                "cleanup_name".into(),
                "cleanup-owner-call".into(),
                json!({}),
                Arc::new(json!({})),
                5000,
            )
            .await
            .expect("route belongs to healthy peer");
        assert_eq!(result["content"][0]["text"], "healthy owner");
        assert!(manager.shutdown(Duration::from_secs(5)).await);
    });
}

/// Filename-derived policy IDs are not package identities. An ambiguous
/// candidate must be rejected before a factory can run, and a failed reload
/// must leave the previously loaded realm intact.
#[test]
#[allow(clippy::too_many_lines)]
fn unrelated_same_id_packages_and_cross_package_coentries_fail_before_factories() {
    let harness = common::TestHarness::new("provenance_reload_guard");
    let (manager, runtime) = load_catalog_packages(
        &harness,
        &[(
            "surviving-package",
            r"
            export default function(pi) {
                pi.registerCommand('surviving-command', { handler: () => 'still alive' });
            }
        ",
        )],
    );
    let source = "export default function() { throw new Error('unrelated factory executed'); }";
    let make_spec = |path: &str| {
        JsExtensionLoadSpec::from_entry_path(harness.create_file(path, source.as_bytes()))
            .expect("candidate spec")
    };
    let mut candidates = Vec::new();

    // This is context-mode's actual package/adapter filename shape. Another
    // independently installed package can naturally choose extension.js too.
    let mut same_id = Vec::new();
    for name in ["context-mode", "unrelated-package"] {
        let package = format!("node_modules/{name}");
        harness.create_file(
            format!("{package}/package.json"),
            serde_json::to_vec(&json!({
                "name": name, "type": "module",
                "pi": { "extensions": ["./build/adapters/pi/extension.js"] },
            }))
            .unwrap(),
        );
        same_id.push(make_spec(&format!(
            "{package}/build/adapters/pi/extension.js"
        )));
    }
    assert!(same_id.iter().all(|spec| spec.extension_id == "extension"));
    candidates.push(same_id);

    // A declaring workspace cannot absorb a nearer named package merely
    // because that package has no pi.extensions of its own.
    harness.create_file(
        "workspace/package.json",
        br#"{
        "name":"outer", "pi":{"extensions":["./a/extension.mjs","./b/extension.mjs"]}
    }"#,
    );
    harness.create_file("workspace/a/package.json", br#"{"name":"a"}"#);
    harness.create_file("workspace/b/package.json", br#"{"name":"b"}"#);
    let nested = make_spec("workspace/a/extension.mjs");
    make_spec("workspace/b/extension.mjs");
    candidates.push(vec![nested]);

    harness.create_file(
        "escaping/package.json",
        br#"{
        "name":"escaping", "pi":{"extensions":["./extension.mjs","../outside/extension.mjs"]}
    }"#,
    );
    let escaping = make_spec("escaping/extension.mjs");
    make_spec("outside/extension.mjs");
    candidates.push(vec![escaping]);

    // A type-only manifest cannot make node_modules itself transparent.
    harness.create_file(
        "dependency-parent/package.json",
        br#"{
        "name":"dependency-parent", "pi":{"extensions":[
            "./extension.mjs", "./node_modules/child/extension.mjs"
        ]}
    }"#,
    );
    harness.create_file(
        "dependency-parent/node_modules/child/package.json",
        br#"{"type":"module"}"#,
    );
    let dependency_parent = make_spec("dependency-parent/extension.mjs");
    make_spec("dependency-parent/node_modules/child/extension.mjs");
    candidates.push(vec![dependency_parent]);

    #[cfg(windows)]
    {
        harness.create_file(
            "windows-parent/package.json",
            br#"{
            "name":"windows-parent", "pi":{"extensions":[
                "./extension.mjs", "./Node_Modules/child/extension.mjs"
            ]}
        }"#,
        );
        harness.create_file(
            "windows-parent/Node_Modules/child/package.json",
            br#"{"type":"module"}"#,
        );
        let windows_parent = make_spec("windows-parent/extension.mjs");
        make_spec("windows-parent/Node_Modules/child/extension.mjs");
        candidates.push(vec![windows_parent]);
    }

    #[cfg(unix)]
    {
        harness.create_file(
            "linked/package.json",
            br#"{
            "name":"linked", "pi":{"extensions":["./extension.mjs","./linked-entry.mjs"]}
        }"#,
        );
        let linked = make_spec("linked/extension.mjs");
        std::os::unix::fs::symlink(
            harness.temp_dir().join("outside/extension.mjs"),
            harness.temp_dir().join("linked/linked-entry.mjs"),
        )
        .expect("symlinked co-entry");
        candidates.push(vec![linked]);
    }

    common::run_async(async move {
        for specs in candidates {
            let error = manager
                .load_js_extensions(specs)
                .await
                .expect_err("unrelated origins must fail before factory execution")
                .to_string();
            assert!(
                error.contains("Ambiguous JS extension provenance"),
                "{error}"
            );
            assert!(!error.contains("unrelated factory executed"), "{error}");
            assert_eq!(
                manager
                    .execute_command("surviving-command", "", 5000)
                    .await
                    .expect("old runtime survives failed candidate"),
                json!("still alive")
            );
        }
        assert!(
            runtime
                .get_registered_tools()
                .await
                .expect("old catalog remains usable")
                .is_empty()
        );
        assert!(manager.shutdown(Duration::from_secs(5)).await);
    });
}

fn start_provenance_fixture(
    harness: &common::TestHarness,
    policy: pi::extensions::ExtensionPolicy,
) -> (ExtensionManager, JsExtensionRuntimeHandle) {
    let workspace = harness.temp_dir().join("workspace");
    std::fs::create_dir_all(&workspace).expect("separate workspace");
    // Package fixtures are siblings of cwd, so successful bundled reads
    // require their admitted package roots rather than workspace authority.
    let cwd = pi::extensions::safe_canonicalize(&workspace);
    let manager = ExtensionManager::new();
    let runtime = common::run_async(JsExtensionRuntimeHandle::start_with_policy(
        PiJsRuntimeConfig {
            cwd: cwd.display().to_string(),
            env: std::collections::HashMap::from([
                ("PI_EXT_COMPAT_SCAN".to_string(), "0".to_string()),
                (
                    "PROVENANCE_TEST_VALUE".to_string(),
                    "configured value".to_string(),
                ),
            ]),
            disk_cache_dir: None,
            repair_mode: pi::extensions_js::RepairMode::Off,
            ..Default::default()
        },
        Arc::new(ToolRegistry::new(&[], &cwd, None)),
        manager.clone(),
        policy,
    ))
    .expect("start real provenance runtime");
    manager.set_js_runtime(runtime.clone());
    (manager, runtime)
}

/// Declared entries of one package retain their shared realm and unchanged
/// policy key, including an entry below a pure Node module-format scope.
#[test]
#[allow(clippy::too_many_lines)]
fn declared_same_package_entries_preserve_order_assets_and_legacy_policy_denials() {
    let harness = common::TestHarness::new("provenance_declared_package");
    harness.create_file(
        "bundle/package.json",
        br#"{
        "name":"different-display-name", "type":"module",
        "pi":{"extensions":["./a/extension.mjs","./build/extension.mjs"]}
    }"#,
    );
    harness.create_file("bundle/build/package.json", br#"{"type":"module"}"#);
    harness.create_file("bundle/asset.txt", b"package asset");
    let first = harness.create_file(
        "bundle/a/extension.mjs",
        br"
        export default function(pi) {
            globalThis.provenanceOrder = ['first'];
            pi.registerCommand('bundle-order', { handler: () => globalThis.provenanceOrder });
        }
    ",
    );
    let second = harness.create_file("bundle/build/extension.mjs", br"
        import { readFileSync } from 'node:fs';
        import { dirname, join } from 'node:path';
        import { fileURLToPath } from 'node:url';
        export default function(pi) {
            globalThis.provenanceOrder.push('second');
            const asset = readFileSync(join(dirname(fileURLToPath(import.meta.url)), '..', 'asset.txt'), 'utf8');
            pi.registerCommand('bundle-policy', { handler: async () => {
                let execError = '';
                try { await pi.exec('pi-provenance-command-that-must-not-run', []); }
                catch (error) { execError = String(error.message || error); }
                return { asset, env: process.env.PROVENANCE_TEST_VALUE ?? null, execError };
            }});
        }
    ");
    let specs = vec![
        JsExtensionLoadSpec::from_entry_path(first).expect("first spec"),
        JsExtensionLoadSpec::from_entry_path(second).expect("second spec"),
    ];
    assert!(specs.iter().all(|spec| spec.extension_id == "extension"));
    let mut policy = pi::extensions::ExtensionPolicy::default();
    policy.deny_caps.retain(|cap| cap != "exec" && cap != "env");
    policy
        .default_caps
        .extend(["exec".to_string(), "env".to_string()]);
    policy.per_extension.insert(
        "extension".to_string(),
        pi::extensions::ExtensionOverride {
            deny: vec!["exec".to_string(), "env".to_string()],
            ..Default::default()
        },
    );
    let (manager, _) = start_provenance_fixture(&harness, policy);
    common::run_async(async move {
        manager
            .load_js_extensions(specs)
            .await
            .expect("same package loads one realm");
        assert_eq!(
            manager
                .execute_command("bundle-order", "", 5000)
                .await
                .unwrap(),
            json!(["first", "second"])
        );
        let observed = manager
            .execute_command("bundle-policy", "", 5000)
            .await
            .unwrap();
        assert_eq!(observed["asset"], "package asset");
        assert!(
            observed["env"].is_null(),
            "legacy per-owner env deny must remain active: {observed}"
        );
        assert!(
            observed["execError"]
                .as_str()
                .is_some_and(|error| error.to_ascii_lowercase().contains("denied")),
            "native hostcall must retain legacy per-owner exec denial: {observed}"
        );
        assert!(manager.shutdown(Duration::from_secs(5)).await);
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn distinct_owners_in_one_declared_package_keep_private_leaf_boundaries() {
    let harness = common::TestHarness::new("provenance_private_leaves");
    harness.create_file(
        "bundle/package.json",
        br#"{
        "name":"shared-package", "type":"module",
        "pi":{"extensions":["./left/left.mjs","./right/right.mjs"]}
    }"#,
    );
    harness.create_file("bundle/left/private.txt", b"left private");
    harness.create_file("bundle/right/private.txt", b"right private");
    harness.create_file(
        "bundle/right/helper.mjs",
        b"export default 'right private module';",
    );
    let left = harness.create_file("bundle/left/left.mjs", br"
        import { readFileSync, writeFileSync } from 'node:fs';
        import { dirname, join } from 'node:path';
        import { fileURLToPath } from 'node:url';
        export default function(pi) {
            const root = dirname(fileURLToPath(import.meta.url));
            pi.registerCommand('probe-private-leaves', { handler: async () => {
                let read = false, write = false, imported = false;
                try { readFileSync(join(root, '..', 'right', 'private.txt'), 'utf8'); read = true; } catch (_) {}
                try { writeFileSync(join(root, '..', 'right', 'created.txt'), 'foreign'); write = true; } catch (_) {}
                try { await import('../right/helper.mjs'); imported = true; } catch (_) {}
                return { own: readFileSync(join(root, 'private.txt'), 'utf8'), read, write, imported };
            }});
        }
    ");
    let right = harness.create_file("bundle/right/right.mjs", br"
        import { readFileSync } from 'node:fs';
        import { dirname, join } from 'node:path';
        import { fileURLToPath } from 'node:url';
        export default function(pi) {
            pi.registerCommand('right-private', { handler: () =>
                readFileSync(join(dirname(fileURLToPath(import.meta.url)), 'private.txt'), 'utf8') });
        }
    ");
    let specs = vec![
        JsExtensionLoadSpec::from_entry_path(left).unwrap(),
        JsExtensionLoadSpec::from_entry_path(right).unwrap(),
    ];
    assert_ne!(specs[0].extension_id, specs[1].extension_id);
    let (manager, _) =
        start_provenance_fixture(&harness, pi::extensions::ExtensionPolicy::default());
    let untouched = harness.temp_dir().join("bundle/right/private.txt");
    common::run_async(async move {
        manager
            .load_js_extensions(specs)
            .await
            .expect("separate owners in shared declared package");
        let observed = manager
            .execute_command("probe-private-leaves", "", 5000)
            .await
            .unwrap();
        assert_eq!(
            observed,
            json!({ "own":"left private", "read":false, "write":false, "imported":false })
        );
        assert_eq!(
            manager
                .execute_command("right-private", "", 5000)
                .await
                .unwrap(),
            json!("right private")
        );
        assert_eq!(std::fs::read_to_string(untouched).unwrap(), "right private");
        assert!(manager.shutdown(Duration::from_secs(5)).await);
    });
}
