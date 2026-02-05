//! `LabRuntime` deterministic testing for extensions (bd-48tv).
//!
//! Uses asupersync's `LabRuntime` for:
//! - Deterministic scheduling (same seed = same execution order)
//! - Virtual time (no wall-clock delays)
//! - Invariant verification via oracle suite
//! - Budget enforcement testing

use asupersync::lab::{LabConfig, LabRuntime};
use asupersync::types::Budget;
use pi::extensions::{ExtensionManager, PROTOCOL_VERSION, RegisterPayload};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Helper to create a `LabRuntime` configured for extension testing.
fn make_lab(seed: u64) -> LabRuntime {
    LabRuntime::new(
        LabConfig::new(seed)
            .worker_count(2)
            .trace_capacity(8_192)
            .panic_on_leak(false),
    )
}

/// Helper to build a `RegisterPayload` with tools.
fn make_registration(name: &str, tools: Vec<Value>) -> RegisterPayload {
    RegisterPayload {
        name: name.to_string(),
        version: "1.0.0".to_string(),
        api_version: PROTOCOL_VERSION.to_string(),
        capabilities: Vec::new(),
        capability_manifest: None,
        tools,
        slash_commands: Vec::new(),
        shortcuts: Vec::new(),
        flags: Vec::new(),
        event_hooks: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Basic LabRuntime infrastructure tests
// ---------------------------------------------------------------------------

#[test]
fn lab_runtime_empty_is_quiescent() {
    let runtime = make_lab(1);
    assert!(runtime.is_quiescent());
}

#[test]
fn lab_runtime_single_task_completes() {
    let mut runtime = make_lab(42);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();

    let (task_id, _handle) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            c.fetch_add(1, Ordering::SeqCst);
        })
        .expect("create task");

    runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    let steps = runtime.run_until_quiescent();

    assert!(steps > 0, "should have run at least one step");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "task should have executed"
    );
    assert!(runtime.is_quiescent());
}

#[test]
fn lab_runtime_deterministic_ordering() {
    // Running with the same seed twice must produce identical execution order.
    let trace1 = run_ordering_test(99);
    let trace2 = run_ordering_test(99);
    assert_eq!(
        trace1, trace2,
        "same seed should produce identical ordering"
    );
}

fn run_ordering_test(seed: u64) -> Vec<usize> {
    let mut runtime = make_lab(seed);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let n = 5;

    for i in 0..n {
        let o = order.clone();
        let (task_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                o.lock().unwrap().push(i);
            })
            .expect("create task");
        runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    }

    runtime.run_until_quiescent();

    let result = order.lock().unwrap().clone();
    assert_eq!(result.len(), n, "all tasks should have executed");
    result
}

#[test]
fn lab_runtime_different_seeds_may_differ() {
    // Different seeds explore different schedules (not guaranteed different
    // for trivial work, but the mechanism is exercised).
    let trace_a = run_ordering_test(1);
    let trace_b = run_ordering_test(2);
    // We don't assert they differ — trivial tasks may execute in insertion order
    // regardless of seed. But we verify both complete correctly.
    assert_eq!(trace_a.len(), 5);
    assert_eq!(trace_b.len(), 5);
}

// ---------------------------------------------------------------------------
// Extension manager tests under LabRuntime
// ---------------------------------------------------------------------------

#[test]
fn extension_manager_creation_under_lab() {
    let mut runtime = make_lab(7);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let manager = Arc::new(std::sync::Mutex::new(None::<ExtensionManager>));
    let m = manager.clone();

    let (task_id, _) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            let mgr = ExtensionManager::new();
            *m.lock().unwrap() = Some(mgr);
        })
        .expect("create task");

    runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    runtime.run_until_quiescent();

    assert!(
        manager.lock().unwrap().is_some(),
        "manager should have been created"
    );
}

#[test]
fn extension_registration_deterministic() {
    // Registering extensions produces the same result regardless of scheduling.
    let entries1 = register_extensions_under_lab(42);
    let entries2 = register_extensions_under_lab(42);
    assert_eq!(entries1.len(), entries2.len());
    for (a, b) in entries1.iter().zip(entries2.iter()) {
        assert_eq!(a.0, b.0, "category names should match");
        assert_eq!(a.1, b.1, "counts should match");
    }
}

fn register_extensions_under_lab(seed: u64) -> Vec<(String, usize)> {
    let mut runtime = make_lab(seed);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let result = Arc::new(std::sync::Mutex::new(Vec::new()));
    let r = result.clone();

    let (task_id, _) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            let manager = ExtensionManager::new();

            // Register an extension with tools and event hooks.
            manager.register(RegisterPayload {
                name: "lab-ext".to_string(),
                version: "1.0.0".to_string(),
                api_version: PROTOCOL_VERSION.to_string(),
                capabilities: Vec::new(),
                capability_manifest: None,
                tools: vec![
                    json!({
                        "name": "lab-tool-1",
                        "description": "First lab tool",
                        "parameters": {}
                    }),
                    json!({
                        "name": "lab-tool-2",
                        "description": "Second lab tool",
                        "parameters": {}
                    }),
                ],
                slash_commands: Vec::new(),
                shortcuts: Vec::new(),
                flags: Vec::new(),
                event_hooks: vec!["tool_call".to_string()],
            });

            // Register provider.
            manager.register_provider(json!({
                "id": "lab-provider",
                "name": "Lab Provider",
                "api": "openai-completions",
                "baseUrl": "https://lab.test/v1",
                "models": [{ "id": "lab-model", "name": "Lab Model" }]
            }));

            let tools = manager.extension_tool_defs();
            let providers = manager.extension_providers();

            *r.lock().unwrap() = vec![
                ("tools".to_string(), tools.len()),
                ("providers".to_string(), providers.len()),
            ];
        })
        .expect("create task");

    runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    runtime.run_until_quiescent();

    result.lock().unwrap().clone()
}

// ---------------------------------------------------------------------------
// Budget enforcement under LabRuntime
// ---------------------------------------------------------------------------

#[test]
fn budget_set_and_get_deterministic() {
    let mut runtime = make_lab(42);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let result = Arc::new(std::sync::Mutex::new(Vec::new()));
    let r = result.clone();

    let (task_id, _) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            // Default manager has INFINITE budget.
            let manager = ExtensionManager::new();
            let b1 = manager.budget();
            assert!(b1.deadline.is_none(), "default budget has no deadline");
            r.lock().unwrap().push("default_ok".to_string());

            // with_budget constructor sets the budget.
            let budget = Budget {
                deadline: Some(asupersync::types::Time::from_millis(5_000)),
                ..Budget::INFINITE
            };
            let mgr2 = ExtensionManager::with_budget(budget);
            let b2 = mgr2.budget();
            assert!(b2.deadline.is_some(), "budget deadline should be set");
            r.lock().unwrap().push("with_budget_ok".to_string());

            // set_budget modifies it.
            mgr2.set_budget(Budget::INFINITE);
            let b3 = mgr2.budget();
            assert!(b3.deadline.is_none(), "budget should be cleared");
            r.lock().unwrap().push("set_budget_ok".to_string());
        })
        .expect("create task");

    runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    runtime.run_until_quiescent();

    let values = result.lock().unwrap().clone();
    assert_eq!(values.len(), 3, "all budget checks should have run");
}

#[test]
fn budget_constants_under_lab() {
    // Verify budget constants are accessible and reasonable.
    let mut runtime = make_lab(7);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let passed = Arc::new(AtomicUsize::new(0));
    let p = passed.clone();

    let (task_id, _) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            const {
                assert!(pi::extensions::EXTENSION_EVENT_TIMEOUT_MS >= 1_000);
                assert!(pi::extensions::EXTENSION_EVENT_TIMEOUT_MS <= 60_000);
                assert!(pi::extensions::EXTENSION_TOOL_BUDGET_MS >= 5_000);
                assert!(pi::extensions::EXTENSION_TOOL_BUDGET_MS <= 300_000);
                assert!(pi::extensions::EXTENSION_UI_BUDGET_MS >= 100);
                assert!(pi::extensions::EXTENSION_UI_BUDGET_MS <= 10_000);
            };
            p.fetch_add(1, Ordering::SeqCst);
        })
        .expect("create task");

    runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    runtime.run_until_quiescent();

    assert_eq!(
        passed.load(Ordering::SeqCst),
        1,
        "all assertions should have passed"
    );
}

// ---------------------------------------------------------------------------
// Concurrent extension operations under LabRuntime
// ---------------------------------------------------------------------------

#[test]
fn concurrent_extension_registrations_deterministic() {
    // Multiple tasks registering tools concurrently should produce deterministic results.
    let counts1 = concurrent_registration_test(42);
    let counts2 = concurrent_registration_test(42);
    assert_eq!(
        counts1, counts2,
        "same seed should produce same tool counts"
    );
}

fn concurrent_registration_test(seed: u64) -> usize {
    let mut runtime = make_lab(seed);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let manager = Arc::new(ExtensionManager::new());
    let n_tasks = 4;
    let tools_per_task = 3;

    for task_idx in 0..n_tasks {
        let mgr = manager.clone();
        let (task_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let tools: Vec<serde_json::Value> = (0..tools_per_task)
                    .map(|tool_idx| {
                        json!({
                            "name": format!("tool-{task_idx}-{tool_idx}"),
                            "description": format!("Tool {tool_idx} from task {task_idx}"),
                            "parameters": {}
                        })
                    })
                    .collect();
                mgr.register(make_registration(&format!("ext-{task_idx}"), tools));
            })
            .expect("create task");
        runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    }

    runtime.run_until_quiescent();
    manager.extension_tool_defs().len()
}

#[test]
fn concurrent_provider_and_tool_registration() {
    let mut runtime = make_lab(99);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let manager = Arc::new(ExtensionManager::new());
    let mgr1 = manager.clone();
    let mgr2 = manager.clone();

    // Task 1: Register providers.
    let (t1, _) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            mgr1.register_provider(json!({
                "id": "prov-a",
                "name": "Provider A",
                "api": "openai-completions",
                "baseUrl": "https://a.test/v1",
                "models": [{ "id": "model-a", "name": "Model A" }]
            }));
            mgr1.register_provider(json!({
                "id": "prov-b",
                "name": "Provider B",
                "api": "openai-completions",
                "baseUrl": "https://b.test/v1",
                "models": [{ "id": "model-b", "name": "Model B" }]
            }));
        })
        .expect("create task");

    // Task 2: Register an extension with a tool.
    let (t2, _) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            mgr2.register(make_registration(
                "concurrent-ext",
                vec![json!({
                    "name": "concurrent-tool",
                    "description": "Registered concurrently with providers",
                    "parameters": {}
                })],
            ));
        })
        .expect("create task");

    runtime.scheduler.lock().unwrap().schedule(t1, 0);
    runtime.scheduler.lock().unwrap().schedule(t2, 0);
    runtime.run_until_quiescent();

    assert_eq!(manager.extension_providers().len(), 2);
    assert_eq!(manager.extension_tool_defs().len(), 1);
}

// ---------------------------------------------------------------------------
// Invariant verification
// ---------------------------------------------------------------------------

#[test]
fn no_invariant_violations_after_extension_work() {
    let mut runtime = make_lab(42);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    let manager = Arc::new(ExtensionManager::new());

    let (task_id, _) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            let tools: Vec<serde_json::Value> = (0..10)
                .map(|i| {
                    json!({
                        "name": format!("inv-tool-{i}"),
                        "description": format!("Invariant test tool {i}"),
                        "parameters": {}
                    })
                })
                .collect();
            manager.register(make_registration("inv-ext", tools));
            manager.register_provider(json!({
                "id": "inv-provider",
                "name": "Invariant Provider",
                "api": "openai-completions",
                "baseUrl": "https://inv.test/v1",
                "models": [{ "id": "inv-model", "name": "Inv Model" }]
            }));
        })
        .expect("create task");

    runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    runtime.run_until_quiescent();

    let violations = runtime.check_invariants();
    assert!(
        violations.is_empty(),
        "no invariant violations expected, got: {violations:?}"
    );
}

#[test]
fn quiescence_reached_after_all_registrations() {
    let mut runtime = make_lab(7);
    let region = runtime.state.create_root_region(Budget::INFINITE);

    for i in 0..5 {
        let (task_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                let _mgr = ExtensionManager::new();
                let _ = format!("task-{i}"); // Prevent unused warnings
            })
            .expect("create task");
        runtime.scheduler.lock().unwrap().schedule(task_id, 0);
    }

    runtime.run_until_quiescent();
    assert!(runtime.is_quiescent());
    assert_eq!(runtime.state.live_task_count(), 0);
}

// ---------------------------------------------------------------------------
// Virtual time testing
// ---------------------------------------------------------------------------

#[test]
fn virtual_time_starts_at_zero() {
    let runtime = make_lab(42);
    assert_eq!(runtime.now(), asupersync::types::Time::ZERO);
}

#[test]
fn virtual_time_advances_manually() {
    let mut runtime = make_lab(42);
    assert_eq!(runtime.now(), asupersync::types::Time::ZERO);

    runtime.advance_time(1_000_000); // 1ms in nanoseconds
    assert!(runtime.now() > asupersync::types::Time::ZERO);
}

// ---------------------------------------------------------------------------
// Seed exploration (smoke test multiple seeds)
// ---------------------------------------------------------------------------

#[test]
fn extension_ops_pass_across_seeds() {
    for seed in 0..10 {
        let mut runtime = make_lab(seed);
        let region = runtime.state.create_root_region(Budget::INFINITE);

        let manager = Arc::new(ExtensionManager::new());
        let mgr = manager.clone();

        let (task_id, _) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                mgr.register(make_registration(
                    "seed-ext",
                    vec![json!({
                        "name": "seed-tool",
                        "description": "Test",
                        "parameters": {}
                    })],
                ));
                mgr.register_provider(json!({
                    "id": "seed-prov",
                    "name": "Seed Provider",
                    "api": "openai-completions",
                    "baseUrl": "https://seed.test/v1",
                    "models": [{ "id": "seed-model", "name": "Seed" }]
                }));
            })
            .expect("create task");

        runtime.scheduler.lock().unwrap().schedule(task_id, 0);
        runtime.run_until_quiescent();

        assert_eq!(
            manager.extension_tool_defs().len(),
            1,
            "seed {seed}: should have 1 tool"
        );
        assert_eq!(
            manager.extension_providers().len(),
            1,
            "seed {seed}: should have 1 provider"
        );

        let violations = runtime.check_invariants();
        assert!(
            violations.is_empty(),
            "seed {seed}: violations: {violations:?}"
        );
    }
}
