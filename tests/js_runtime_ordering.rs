//! Unit tests: JS runtime + shims + event loop ordering (bd-39u).
//!
//! These tests exercise the PiJsRuntime through its public API only,
//! verifying promise bridge behavior, hostcall completion ordering,
//! timer scheduling, and event loop semantics.
#![forbid(unsafe_code)]

mod common;

use common::TestHarness;
use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig, PiJsRuntimeLimits};
use pi::scheduler::{DeterministicClock, HostcallOutcome};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a PiJsRuntime with a deterministic clock starting at 0.
async fn make_runtime() -> (
    PiJsRuntime<Arc<DeterministicClock>>,
    Arc<DeterministicClock>,
) {
    let clock = Arc::new(DeterministicClock::new(0));
    let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
        .await
        .expect("create runtime");
    (runtime, clock)
}

/// Create a PiJsRuntime with custom limits and a deterministic clock.
async fn make_runtime_with_limits(
    limits: PiJsRuntimeLimits,
) -> (
    PiJsRuntime<Arc<DeterministicClock>>,
    Arc<DeterministicClock>,
) {
    let clock = Arc::new(DeterministicClock::new(0));
    let config = PiJsRuntimeConfig {
        limits,
        ..Default::default()
    };
    let runtime = PiJsRuntime::with_clock_and_config(Arc::clone(&clock), config)
        .await
        .expect("create runtime");
    (runtime, clock)
}

/// Eval JS that stores a global variable and then read it back via eval assertion.
async fn get_global_json(
    runtime: &PiJsRuntime<Arc<DeterministicClock>>,
    name: &str,
) -> serde_json::Value {
    // Copy the value to a known temporary variable, then read it back via a round-trip.
    runtime
        .eval(&format!(
            "globalThis.__test_tmp = JSON.stringify(globalThis.{name});"
        ))
        .await
        .expect("eval stringify");

    // We can't directly read JS values from integration tests, so we use a trick:
    // eval throws if the assertion fails, otherwise succeeds.
    // Instead, let's use get_registered_tools as a communication channel, or use
    // a simpler pattern: evaluate JS that throws if condition is not met.
    //
    // For now, we use the approach of registering a tool with the value serialized
    // in its description, then reading it back.
    //
    // Actually, the simplest approach is to have JS throw on assertion failure.
    // We'll use the pi.events() hostcall as a side channel.
    runtime
        .eval(&format!(
            r#"pi.events("__test_read_global", {{ name: "{name}", value: globalThis.{name} }});"#
        ))
        .await
        .expect("eval read global");

    let requests = runtime.drain_hostcall_requests();
    let req = requests
        .iter()
        .find(|r| r.payload.get("name").and_then(|v| v.as_str()) == Some(name))
        .unwrap_or_else(|| panic!("no __test_read_global request for {name}"));
    req.payload
        .get("value")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Assert a global JS variable equals an expected JSON value.
async fn assert_global(
    runtime: &PiJsRuntime<Arc<DeterministicClock>>,
    name: &str,
    expected: serde_json::Value,
) {
    let actual = get_global_json(runtime, name).await;
    assert_eq!(actual, expected, "globalThis.{name} mismatch");
}

// ---------------------------------------------------------------------------
// Promise Bridge: Hostcall Completion
// ---------------------------------------------------------------------------

#[test]
fn hostcall_completion_resolves_promise() {
    let _harness = TestHarness::new("hostcall_completion_resolves_promise");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        runtime
            .eval(
                r#"
                globalThis.result = null;
                pi.tool("read", { path: "test.txt" }).then(r => {
                    globalThis.result = r;
                });
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);

        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Success(serde_json::json!({ "content": "hello" })),
        );

        let stats = runtime.tick().await.expect("tick");
        assert!(stats.ran_macrotask);

        assert_global(
            &runtime,
            "result",
            serde_json::json!({ "content": "hello" }),
        )
        .await;
    });
}

#[test]
fn hostcall_error_rejects_promise() {
    let _harness = TestHarness::new("hostcall_error_rejects_promise");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        runtime
            .eval(
                r#"
                globalThis.caught = null;
                pi.tool("read", { path: "bad" }).catch(e => {
                    globalThis.caught = { code: e.code, message: e.message };
                });
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Error {
                code: "ENOENT".to_string(),
                message: "not found".to_string(),
            },
        );

        runtime.tick().await.expect("tick");
        assert_global(
            &runtime,
            "caught",
            serde_json::json!({ "code": "ENOENT", "message": "not found" }),
        )
        .await;
    });
}

// ---------------------------------------------------------------------------
// Promise Bridge: Multiple Hostcall FIFO Ordering
// ---------------------------------------------------------------------------

#[test]
fn multiple_hostcalls_complete_in_fifo_order() {
    let harness = TestHarness::new("multiple_hostcalls_complete_in_fifo_order");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        runtime
            .eval(
                r#"
                globalThis.order = [];
                pi.tool("a", {}).then(() => globalThis.order.push("a"));
                pi.tool("b", {}).then(() => globalThis.order.push("b"));
                pi.tool("c", {}).then(() => globalThis.order.push("c"));
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 3);
        harness.log().info(
            "hostcalls",
            format!("Enqueued {} hostcalls", requests.len()),
        );

        // Complete in FIFO order (a, b, c)
        for req in &requests {
            runtime.complete_hostcall(
                &req.call_id,
                HostcallOutcome::Success(serde_json::Value::Null),
            );
        }

        // Each completion is a macrotask — tick once per hostcall
        for _ in 0..3 {
            runtime.tick().await.expect("tick");
        }

        assert_global(&runtime, "order", serde_json::json!(["a", "b", "c"])).await;
    });
}

#[test]
fn out_of_order_hostcall_completion_delivers_correctly() {
    let _harness = TestHarness::new("out_of_order_hostcall_completion");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        runtime
            .eval(
                r#"
                globalThis.results = {};
                pi.tool("first", {}).then(r => { globalThis.results.first = r; });
                pi.tool("second", {}).then(r => { globalThis.results.second = r; });
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 2);

        // Complete second before first
        runtime.complete_hostcall(
            &requests[1].call_id,
            HostcallOutcome::Success(serde_json::json!("val-second")),
        );
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Success(serde_json::json!("val-first")),
        );

        // Tick to deliver both
        for _ in 0..2 {
            runtime.tick().await.expect("tick");
        }

        assert_global(
            &runtime,
            "results",
            serde_json::json!({ "first": "val-first", "second": "val-second" }),
        )
        .await;
    });
}

// ---------------------------------------------------------------------------
// Promise Bridge: Timeout
// ---------------------------------------------------------------------------

#[test]
fn hostcall_timeout_rejects_with_timeout_code() {
    let _harness = TestHarness::new("hostcall_timeout_rejects_with_timeout_code");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime_with_limits(PiJsRuntimeLimits {
            hostcall_timeout_ms: Some(100),
            ..Default::default()
        })
        .await;

        runtime
            .eval(
                r#"
                globalThis.code = null;
                pi.tool("slow", {}).catch(e => { globalThis.code = e.code; });
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);

        // Advance clock past timeout
        clock.set(100);
        let stats = runtime.tick().await.expect("tick");
        assert!(stats.ran_macrotask);
        assert_eq!(stats.hostcalls_timed_out, 1);

        assert_global(&runtime, "code", serde_json::json!("timeout")).await;
    });
}

#[test]
fn multiple_hostcalls_all_timeout() {
    let _harness = TestHarness::new("multiple_hostcalls_all_timeout");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime_with_limits(PiJsRuntimeLimits {
            hostcall_timeout_ms: Some(50),
            ..Default::default()
        })
        .await;

        runtime
            .eval(
                r#"
                globalThis.timeouts = 0;
                pi.tool("a", {}).catch(e => { if (e.code === "timeout") globalThis.timeouts++; });
                pi.tool("b", {}).catch(e => { if (e.code === "timeout") globalThis.timeouts++; });
                pi.tool("c", {}).catch(e => { if (e.code === "timeout") globalThis.timeouts++; });
            "#,
            )
            .await
            .expect("eval");

        assert_eq!(runtime.drain_hostcall_requests().len(), 3);

        clock.set(50);
        // Tick multiple times to drain all timeout macrotasks
        for _ in 0..5 {
            runtime.tick().await.expect("tick");
        }

        assert_global(&runtime, "timeouts", serde_json::json!(3)).await;
    });
}

#[test]
fn late_completion_after_timeout_is_ignored() {
    let _harness = TestHarness::new("late_completion_after_timeout_is_ignored");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime_with_limits(PiJsRuntimeLimits {
            hostcall_timeout_ms: Some(50),
            ..Default::default()
        })
        .await;

        runtime
            .eval(
                r#"
                globalThis.resolved = false;
                globalThis.rejected = false;
                pi.tool("slow", {})
                    .then(() => { globalThis.resolved = true; })
                    .catch(() => { globalThis.rejected = true; });
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();

        // Timeout fires
        clock.set(50);
        runtime.tick().await.expect("tick timeout");

        assert_global(&runtime, "rejected", serde_json::json!(true)).await;

        // Late completion arrives
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Success(serde_json::json!("late")),
        );
        runtime.tick().await.expect("tick late");

        // resolved should still be false — the late completion was ignored
        assert_global(&runtime, "resolved", serde_json::json!(false)).await;
    });
}

// ---------------------------------------------------------------------------
// Event Loop: Hostcalls Before Timers
// ---------------------------------------------------------------------------

#[test]
fn hostcall_completions_processed_before_timers() {
    let harness = TestHarness::new("hostcall_completions_before_timers");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime().await;

        runtime.eval(r"globalThis.order = [];").await.expect("init");

        // Create a timer at delay=0 (fires immediately when clock >= creation time)
        let timer_id = runtime.set_timeout(0);
        runtime
            .eval(&format!(
                r#"__pi_register_timer({timer_id}, () => globalThis.order.push("timer"));"#
            ))
            .await
            .expect("register timer");

        // Create a hostcall
        runtime
            .eval(r#"pi.tool("test", {}).then(() => globalThis.order.push("hostcall"));"#)
            .await
            .expect("enqueue hostcall");

        let requests = runtime.drain_hostcall_requests();
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Success(serde_json::Value::Null),
        );

        // Tick 1: hostcall should run first
        runtime.tick().await.expect("tick 1");
        let order = get_global_json(&runtime, "order").await;
        harness
            .log()
            .info("ordering", format!("after tick 1: {order}"));
        assert_eq!(order, serde_json::json!(["hostcall"]));

        // Tick 2: timer should run
        runtime.tick().await.expect("tick 2");
        let order = get_global_json(&runtime, "order").await;
        harness
            .log()
            .info("ordering", format!("after tick 2: {order}"));
        assert_eq!(order, serde_json::json!(["hostcall", "timer"]));
    });
}

// ---------------------------------------------------------------------------
// Event Loop: Timer Ordering by Deadline
// ---------------------------------------------------------------------------

#[test]
fn timers_fire_in_deadline_order() {
    let _harness = TestHarness::new("timers_fire_in_deadline_order");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime().await;

        runtime.eval(r"globalThis.fired = [];").await.expect("init");

        // Create timers with different delays
        let t1 = runtime.set_timeout(30); // fires at 30ms
        let t2 = runtime.set_timeout(10); // fires at 10ms
        let t3 = runtime.set_timeout(20); // fires at 20ms

        runtime
            .eval(&format!(
                r#"
                __pi_register_timer({t1}, () => globalThis.fired.push("30ms"));
                __pi_register_timer({t2}, () => globalThis.fired.push("10ms"));
                __pi_register_timer({t3}, () => globalThis.fired.push("20ms"));
            "#
            ))
            .await
            .expect("register timers");

        // Advance clock past all deadlines
        clock.set(30);

        // Tick 3 times to fire all timers
        for _ in 0..3 {
            runtime.tick().await.expect("tick");
        }

        // Should fire in deadline order: 10ms, 20ms, 30ms
        assert_global(
            &runtime,
            "fired",
            serde_json::json!(["10ms", "20ms", "30ms"]),
        )
        .await;
    });
}

#[test]
fn same_deadline_timers_fire_in_creation_order() {
    let _harness = TestHarness::new("same_deadline_timers_fire_in_creation_order");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime().await;

        runtime.eval(r"globalThis.fired = [];").await.expect("init");

        let t1 = runtime.set_timeout(10);
        let t2 = runtime.set_timeout(10);
        let t3 = runtime.set_timeout(10);

        runtime
            .eval(&format!(
                r#"
                __pi_register_timer({t1}, () => globalThis.fired.push("first"));
                __pi_register_timer({t2}, () => globalThis.fired.push("second"));
                __pi_register_timer({t3}, () => globalThis.fired.push("third"));
            "#
            ))
            .await
            .expect("register timers");

        clock.set(10);
        for _ in 0..3 {
            runtime.tick().await.expect("tick");
        }

        assert_global(
            &runtime,
            "fired",
            serde_json::json!(["first", "second", "third"]),
        )
        .await;
    });
}

// ---------------------------------------------------------------------------
// Event Loop: Clear Timeout
// ---------------------------------------------------------------------------

#[test]
fn clear_timeout_prevents_callback() {
    let _harness = TestHarness::new("clear_timeout_prevents_callback");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime().await;

        runtime
            .eval(r"globalThis.fired = false;")
            .await
            .expect("init");

        let timer_id = runtime.set_timeout(10);
        runtime
            .eval(&format!(
                r#"__pi_register_timer({timer_id}, () => {{ globalThis.fired = true; }});"#
            ))
            .await
            .expect("register timer");

        assert!(runtime.clear_timeout(timer_id));

        clock.set(10);
        let stats = runtime.tick().await.expect("tick");
        assert!(!stats.ran_macrotask);

        assert_global(&runtime, "fired", serde_json::json!(false)).await;
    });
}

// ---------------------------------------------------------------------------
// Event Loop: Event Delivery
// ---------------------------------------------------------------------------

#[test]
fn enqueued_event_is_processed_as_macrotask() {
    let _harness = TestHarness::new("enqueued_event_as_macrotask");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        // Enqueue an event — it becomes a macrotask
        runtime.enqueue_event("test_event", serde_json::json!({ "key": "value" }));

        assert!(runtime.has_pending());

        // Tick to deliver the event macrotask
        let stats = runtime.tick().await.expect("tick");
        assert!(stats.ran_macrotask);

        // After delivery, no more pending work
        assert!(!runtime.has_pending());
    });
}

// ---------------------------------------------------------------------------
// has_pending State Tracking
// ---------------------------------------------------------------------------

#[test]
fn has_pending_reflects_hostcall_state() {
    let _harness = TestHarness::new("has_pending_reflects_hostcall_state");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        assert!(!runtime.has_pending());

        runtime.eval(r#"pi.tool("test", {});"#).await.expect("eval");
        assert!(runtime.has_pending());

        let requests = runtime.drain_hostcall_requests();
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Success(serde_json::Value::Null),
        );

        // Still pending until tick delivers the completion
        assert!(runtime.has_pending());

        runtime.tick().await.expect("tick");
        assert!(!runtime.has_pending());
    });
}

#[test]
fn has_pending_reflects_timer_state() {
    let _harness = TestHarness::new("has_pending_reflects_timer_state");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime().await;

        assert!(!runtime.has_pending());

        let timer_id = runtime.set_timeout(10);
        runtime
            .eval(&format!(r#"__pi_register_timer({timer_id}, () => {{}});"#))
            .await
            .expect("register timer");

        assert!(runtime.has_pending());

        clock.set(10);
        runtime.tick().await.expect("tick");
        assert!(!runtime.has_pending());
    });
}

// ---------------------------------------------------------------------------
// Microtask Drain: Promise Chains
// ---------------------------------------------------------------------------

#[test]
fn microtasks_drain_before_next_macrotask() {
    let harness = TestHarness::new("microtasks_drain_before_next_macrotask");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime().await;

        runtime.eval(r"globalThis.order = [];").await.expect("init");

        // Timer that spawns a microtask
        let timer_id = runtime.set_timeout(10);
        runtime
            .eval(&format!(
                r#"__pi_register_timer({timer_id}, () => {{
                    globalThis.order.push("timer");
                    Promise.resolve().then(() => globalThis.order.push("timer-micro"));
                }});"#
            ))
            .await
            .expect("register timer");

        // Hostcall that spawns a microtask
        runtime
            .eval(
                r#"
                pi.tool("read", {}).then(() => {
                    globalThis.order.push("hostcall");
                    Promise.resolve().then(() => globalThis.order.push("hostcall-micro"));
                });
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Success(serde_json::Value::Null),
        );

        clock.set(10);

        // Tick 1: hostcall + its microtasks
        runtime.tick().await.expect("tick 1");
        let order = get_global_json(&runtime, "order").await;
        harness
            .log()
            .info("ordering", format!("after tick 1: {order}"));
        assert_eq!(order, serde_json::json!(["hostcall", "hostcall-micro"]));

        // Tick 2: timer + its microtasks
        runtime.tick().await.expect("tick 2");
        let order = get_global_json(&runtime, "order").await;
        harness
            .log()
            .info("ordering", format!("after tick 2: {order}"));
        assert_eq!(
            order,
            serde_json::json!(["hostcall", "hostcall-micro", "timer", "timer-micro"])
        );
    });
}

// ---------------------------------------------------------------------------
// Runtime Limits: Interrupt Budget
// ---------------------------------------------------------------------------

#[test]
fn interrupt_budget_aborts_infinite_loop() {
    let _harness = TestHarness::new("interrupt_budget_aborts_infinite_loop");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(0),
            ..Default::default()
        })
        .await;

        let err = runtime
            .eval("for (let i = 0; ; i++) {}")
            .await
            .expect_err("should abort");

        assert!(
            err.to_string().contains("budget exceeded"),
            "unexpected error: {err}"
        );
    });
}

// ---------------------------------------------------------------------------
// Tick Stats
// ---------------------------------------------------------------------------

#[test]
fn tick_stats_empty_loop() {
    let _harness = TestHarness::new("tick_stats_empty_loop");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        let stats = runtime.tick().await.expect("tick");
        assert!(!stats.ran_macrotask);
        assert_eq!(stats.pending_hostcalls, 0);
        assert_eq!(stats.hostcalls_total, 0);
        assert_eq!(stats.hostcalls_timed_out, 0);
    });
}

#[test]
fn tick_stats_after_hostcall_completion() {
    let _harness = TestHarness::new("tick_stats_after_hostcall_completion");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        runtime.eval(r#"pi.tool("test", {});"#).await.expect("eval");
        assert_eq!(runtime.pending_hostcall_count(), 1);

        let requests = runtime.drain_hostcall_requests();
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Success(serde_json::Value::Null),
        );

        let stats = runtime.tick().await.expect("tick");
        assert!(stats.ran_macrotask);
        assert_eq!(stats.pending_hostcalls, 0);
        assert!(stats.hostcalls_total >= 1);
    });
}

// ---------------------------------------------------------------------------
// Promise Chain Depth
// ---------------------------------------------------------------------------

#[test]
fn deep_promise_chain_resolves_correctly() {
    let _harness = TestHarness::new("deep_promise_chain_resolves_correctly");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        runtime
            .eval(
                r#"
                globalThis.final_value = null;
                pi.tool("start", {})
                    .then(r => r.value + 1)
                    .then(r => r + 10)
                    .then(r => r * 2)
                    .then(r => { globalThis.final_value = r; });
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Success(serde_json::json!({ "value": 5 })),
        );

        runtime.tick().await.expect("tick");

        // (5 + 1 + 10) * 2 = 32
        assert_global(&runtime, "final_value", serde_json::json!(32)).await;
    });
}

// ---------------------------------------------------------------------------
// Runtime: next_timer_deadline_ms
// ---------------------------------------------------------------------------

#[test]
fn next_timer_deadline_reflects_nearest_timer() {
    let _harness = TestHarness::new("next_timer_deadline_reflects_nearest");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        assert_eq!(runtime.next_timer_deadline_ms(), None);

        let _t1 = runtime.set_timeout(100);
        let _t2 = runtime.set_timeout(50);
        let _t3 = runtime.set_timeout(200);

        assert_eq!(runtime.next_timer_deadline_ms(), Some(50));
    });
}

// ---------------------------------------------------------------------------
// Runtime: now_ms
// ---------------------------------------------------------------------------

#[test]
fn now_ms_tracks_clock() {
    let _harness = TestHarness::new("now_ms_tracks_clock");
    futures::executor::block_on(async {
        let (runtime, clock) = make_runtime().await;

        assert_eq!(runtime.now_ms(), 0);
        clock.advance(42);
        assert_eq!(runtime.now_ms(), 42);
        clock.set(1000);
        assert_eq!(runtime.now_ms(), 1000);
    });
}

// ---------------------------------------------------------------------------
// Hostcall Request Kinds
// ---------------------------------------------------------------------------

#[test]
fn hostcall_kinds_are_correctly_classified() {
    let _harness = TestHarness::new("hostcall_kinds_classified");
    futures::executor::block_on(async {
        let (runtime, _clock) = make_runtime().await;

        runtime
            .eval(
                r#"
                pi.tool("read", { path: "x" });
                pi.exec("ls", ["-la"]);
                pi.http({ url: "https://example.com" });
                pi.session("getName", {});
                pi.events("getModel", {});
            "#,
            )
            .await
            .expect("eval");

        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 5);

        use pi::extensions_js::HostcallKind;
        assert!(matches!(&requests[0].kind, HostcallKind::Tool { name } if name == "read"));
        assert!(matches!(&requests[1].kind, HostcallKind::Exec { cmd } if cmd == "ls"));
        assert!(matches!(&requests[2].kind, HostcallKind::Http));
        assert!(matches!(&requests[3].kind, HostcallKind::Session { op } if op == "getName"));
        assert!(matches!(&requests[4].kind, HostcallKind::Events { op } if op == "getModel"));
    });
}
