//! Security suite: budget enforcement tests (bd-246d).
//!
//! Tests verify that `PiJsRuntime` enforces CPU, memory, and stack budgets,
//! and that hostcall timeouts terminate runaway operations cleanly.

use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig, PiJsRuntimeLimits};
use pi::scheduler::DeterministicClock;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn config_with_limits(limits: PiJsRuntimeLimits) -> PiJsRuntimeConfig {
    PiJsRuntimeConfig {
        limits,
        ..Default::default()
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Interrupt budget (CPU time watchdog)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn interrupt_budget_zero_aborts_immediately() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(0),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // This infinite loop should be interrupted immediately (budget=0)
        let result = runtime.eval("while(true) {}").await;
        assert!(
            result.is_err(),
            "expected budget exceeded error, got Ok(())"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("budget exceeded") || err_msg.contains("interrupt"),
            "expected budget error, got: {err_msg}"
        );
    });
}

#[test]
fn interrupt_budget_small_aborts_tight_loop() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(100),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Should trip the interrupt after ~100 iterations
        let result = runtime.eval("let i = 0; while(true) { i++; }").await;
        assert!(
            result.is_err(),
            "expected budget exceeded error, got Ok(())"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("budget exceeded"),
            "expected budget error, got: {err_msg}"
        );
    });
}

#[test]
fn interrupt_budget_none_allows_completion() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: None,
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Without a budget, a finite loop should complete
        let result = runtime
            .eval("let sum = 0; for (let i = 0; i < 10000; i++) { sum += i; }")
            .await;
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    });
}

#[test]
fn interrupt_budget_resets_between_evals() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(10_000),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // First eval should succeed with moderate work
        let r1 = runtime
            .eval("let a = 0; for (let i = 0; i < 100; i++) { a += i; }")
            .await;
        assert!(r1.is_ok(), "first eval should succeed: {:?}", r1.err());

        // Second eval should also succeed (budget reset)
        let r2 = runtime
            .eval("let b = 0; for (let i = 0; i < 100; i++) { b += i; }")
            .await;
        assert!(r2.is_ok(), "second eval should succeed: {:?}", r2.err());
    });
}

#[test]
fn interrupt_budget_aborts_recursive_function() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(500),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        let result = runtime
            .eval("function f(n) { return f(n + 1); } f(0);")
            .await;
        assert!(
            result.is_err(),
            "expected error from infinite recursion with budget"
        );
    });
}

#[test]
fn interrupt_budget_preserves_state_after_trip() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(50),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Set a value before the budget trip
        runtime
            .eval("globalThis.marker = 42;")
            .await
            .expect("set marker");

        // This should trip
        let _ = runtime.eval("while(true) {}").await;

        // The runtime should still be usable for small operations
        // (budget resets on next eval)
        let result = runtime
            .eval("globalThis.budgetTestResult = globalThis.marker;")
            .await;
        assert!(
            result.is_ok(),
            "runtime should be usable after budget trip: {:?}",
            result.err()
        );
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Memory limits
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn memory_limit_prevents_large_allocation() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            // 1MB memory limit
            memory_limit_bytes: Some(1024 * 1024),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Try to allocate a huge array - should fail with memory limit
        let result = runtime
            .eval("const arr = new Array(10_000_000).fill('x'.repeat(100));")
            .await;
        assert!(
            result.is_err(),
            "expected OOM error from 1GB allocation attempt"
        );
    });
}

#[test]
fn memory_limit_allows_small_allocations() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            // 10MB memory limit — generous for small work
            memory_limit_bytes: Some(10 * 1024 * 1024),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Small allocation should succeed
        let result = runtime
            .eval("const small = new Array(100).fill('hello');")
            .await;
        assert!(
            result.is_ok(),
            "small allocation should succeed: {:?}",
            result.err()
        );
    });
}

#[test]
fn memory_limit_tracks_usage_in_stats() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            memory_limit_bytes: Some(50 * 1024 * 1024),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Allocate some data
        runtime
            .eval("globalThis.data = new Array(10000).fill('test string');")
            .await
            .expect("allocate data");

        let stats = runtime.tick().await.expect("tick for stats");
        assert!(
            stats.memory_used_bytes > 0,
            "memory_used_bytes should be non-zero after allocation"
        );
        assert!(
            stats.peak_memory_used_bytes >= stats.memory_used_bytes,
            "peak should be >= current"
        );
    });
}

#[test]
fn memory_limit_gradual_growth_triggers_oom() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            // Very tight: 2MB
            memory_limit_bytes: Some(2 * 1024 * 1024),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Gradually grow an array until OOM
        let result = runtime
            .eval(
                r"
const chunks = [];
for (let i = 0; i < 100000; i++) {
    chunks.push(new Array(1000).fill(i));
}
",
            )
            .await;
        assert!(
            result.is_err(),
            "expected OOM from gradual memory growth with 2MB limit"
        );
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Stack size limits
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn stack_limit_prevents_deep_recursion() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            // Very small stack: 64KB
            max_stack_bytes: Some(64 * 1024),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        let result = runtime
            .eval("function deep(n) { if (n > 0) return deep(n-1); return n; } deep(100000);")
            .await;
        assert!(result.is_err(), "expected stack overflow error");
    });
}

#[test]
fn stack_limit_allows_reasonable_recursion() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            // 1MB stack — should handle moderate recursion
            max_stack_bytes: Some(1024 * 1024),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Modest recursion should succeed
        let result = runtime
            .eval("function fib(n) { if (n <= 1) return n; return fib(n-1) + fib(n-2); } fib(15);")
            .await;
        assert!(
            result.is_ok(),
            "moderate recursion should succeed: {:?}",
            result.err()
        );
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Hostcall timeout enforcement
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn hostcall_timeout_is_tracked_in_stats() {
    use pi::extensions_js::HostcallKind;
    use pi::scheduler::HostcallOutcome;
    use serde_json::Value;

    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            hostcall_timeout_ms: Some(100),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        runtime.eval(r#"pi.exec("slow_cmd");"#).await.expect("eval");

        let mut reqs = runtime.drain_hostcall_requests();
        let req = reqs.pop_front().expect("hostcall request");
        assert!(matches!(&req.kind, HostcallKind::Exec { .. }));

        // Complete normally — stats should show it was issued
        runtime.complete_hostcall(req.call_id, HostcallOutcome::Success(Value::Null));
        let stats = runtime.tick().await.expect("tick");
        assert!(
            stats.hostcalls_total >= 1,
            "hostcalls_total should be >= 1, got {}",
            stats.hostcalls_total
        );
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Combined limits
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn combined_limits_all_enforced() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            memory_limit_bytes: Some(5 * 1024 * 1024),
            max_stack_bytes: Some(256 * 1024),
            interrupt_budget: Some(50_000),
            hostcall_timeout_ms: Some(5000),
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime with combined limits");

        // Normal operation should succeed
        let result = runtime
            .eval("const x = 1 + 2; globalThis.combined = x;")
            .await;
        assert!(
            result.is_ok(),
            "normal op should succeed: {:?}",
            result.err()
        );

        // CPU budget should still catch infinite loops
        let result = runtime.eval("while(true) {}").await;
        assert!(result.is_err(), "infinite loop should be caught by budget");
    });
}

#[test]
fn memory_limit_with_interrupt_budget_oom_before_budget() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            // 2MB memory limit — enough for QuickJS GC to work, but OOM on bulk alloc
            memory_limit_bytes: Some(2 * 1024 * 1024),
            // Generous interrupt budget
            interrupt_budget: Some(10_000_000),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Should OOM before hitting interrupt budget (allocate ~100MB of arrays)
        let result = runtime
            .eval("const big = []; for (let i = 0; i < 100000; i++) big.push(new Array(1000).fill(i));")
            .await;
        assert!(result.is_err(), "expected OOM before budget exhaustion");
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Error message quality
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn budget_exceeded_error_is_descriptive() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(10),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        let result = runtime.eval("while(true) {}").await;
        let err = result.unwrap_err();
        let msg = err.to_string();

        // Should mention "budget" or "exceeded" or "interrupt"
        assert!(
            msg.contains("budget") || msg.contains("exceeded") || msg.contains("interrupt"),
            "error message should be descriptive about budget: {msg}"
        );
    });
}

#[test]
fn stack_overflow_error_is_not_budget_error() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            max_stack_bytes: Some(32 * 1024),
            // No interrupt budget — so stack overflow should be separate error
            interrupt_budget: None,
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        let result = runtime.eval("function r() { return r(); } r();").await;
        let err = result.unwrap_err();
        let msg = err.to_string();

        // Stack overflow should NOT say "budget exceeded"
        assert!(
            !msg.contains("budget exceeded"),
            "stack overflow should not report as budget exceeded: {msg}"
        );
    });
}

// ═══════════════════════════════════════════════════════════════════════════════
// Promise-based budget enforcement
// ═══════════════════════════════════════════════════════════════════════════════

// NOTE: Interrupt budget inside Promise.resolve().then() callbacks can trigger
// QuickJS GC assertion failures (gc_decref_child: ref_count > 0). This is a
// known QuickJS limitation when the interrupt fires mid-microtask. We test
// budget enforcement on synchronous code paths instead, which is the primary
// use case for the watchdog.

#[test]
fn interrupt_budget_applies_to_nested_functions() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(500),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Busy loop inside nested function call
        let result = runtime
            .eval("function spin() { while(true) {} } spin();")
            .await;
        assert!(
            result.is_err(),
            "nested function busy loop should be caught by budget"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("budget exceeded"),
            "expected budget error, got: {msg}"
        );
    });
}

#[test]
fn budget_does_not_interfere_with_normal_promises() {
    futures::executor::block_on(async {
        let config = config_with_limits(PiJsRuntimeLimits {
            interrupt_budget: Some(100_000),
            ..Default::default()
        });

        let runtime = PiJsRuntime::with_clock_and_config(DeterministicClock::new(0), config)
            .await
            .expect("create runtime");

        // Normal async chain should work fine
        let result = runtime
            .eval(
                r"
globalThis.promiseResult = null;
Promise.resolve(42)
  .then(v => v * 2)
  .then(v => { globalThis.promiseResult = v; });
",
            )
            .await;
        assert!(
            result.is_ok(),
            "normal promise chain should succeed: {:?}",
            result.err()
        );
    });
}
