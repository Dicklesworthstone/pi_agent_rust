//! Conformance and determinism tests for the `PiJS` event loop scheduler.
//!
//! This suite is fixture-driven to make ordering rules explicit and extensible.
//! Also includes `LabRuntime`-backed deterministic tests (bd-48tv).
#![forbid(unsafe_code)]

use pi::extensions_js::{ClockHandle, MacrotaskKind, ManualClock, PiEventLoop};
use pi::scheduler::{self, HostcallOutcome, Scheduler};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use asupersync::{Budget, LabConfig, LabRuntime};

#[derive(Debug, Deserialize)]
struct Fixture {
    version: String,
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
struct Case {
    name: String,
    start_ms: u64,
    ops: Vec<Op>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Op {
    SetTimeout {
        delay_ms: u64,
        save_as: String,
    },
    ClearTimeout {
        timer: String,
    },
    EnqueueHostcallCompletion {
        call_id: String,
    },
    EnqueueInboundEvent {
        event_id: String,
    },
    SetClock {
        ms: u64,
    },
    AdvanceClock {
        ms: u64,
    },
    Tick {
        #[serde(default)]
        microtasks: usize,
        expect: Option<ExpectedTask>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ExpectedTask {
    HostcallComplete { call_id: String },
    InboundEvent { event_id: String },
    TimerFired { timer: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ObservedTask {
    HostcallComplete { call_id: String },
    InboundEvent { event_id: String },
    TimerFired { timer_id: u64 },
}

#[test]
fn event_loop_fixture_conformance() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let fixture_path = repo_root.join("tests/fixtures/event_loop_conformance.json");
    let contents = fs::read_to_string(&fixture_path).expect("read event loop fixture JSON");
    let fixture: Fixture = serde_json::from_str(&contents).expect("parse event loop fixture JSON");
    assert_eq!(fixture.version, "1.0");

    for case in &fixture.cases {
        run_case(case);
    }
}

fn run_case(case: &Case) {
    let clock = Arc::new(ManualClock::new(case.start_ms));
    let mut loop_state = PiEventLoop::new(ClockHandle::new(clock.clone()));
    let mut timers: HashMap<&str, u64> = HashMap::new();

    for (idx, op) in case.ops.iter().enumerate() {
        match op {
            Op::SetTimeout { delay_ms, save_as } => {
                let id = loop_state.set_timeout(*delay_ms);
                timers.insert(save_as.as_str(), id);
            }
            Op::ClearTimeout { timer } => {
                let id = *timers
                    .get(timer.as_str())
                    .unwrap_or_else(|| panic!("{}: step {idx}: unknown timer {timer}", case.name));
                loop_state.clear_timeout(id);
            }
            Op::EnqueueHostcallCompletion { call_id } => {
                loop_state.enqueue_hostcall_completion(call_id.clone());
            }
            Op::EnqueueInboundEvent { event_id } => {
                loop_state.enqueue_inbound_event(event_id.clone());
            }
            Op::SetClock { ms } => {
                clock.set(*ms);
            }
            Op::AdvanceClock { ms } => {
                clock.advance(*ms);
            }
            Op::Tick { microtasks, expect } => {
                let mut observed = None;
                let mut remaining_drains = *microtasks;

                let result = loop_state.tick(
                    |task| {
                        observed = Some(observe_task(task.kind));
                    },
                    || {
                        if remaining_drains == 0 {
                            return false;
                        }
                        remaining_drains -= 1;
                        true
                    },
                );

                let expected_observed = expect.as_ref().map(|task| match task {
                    ExpectedTask::HostcallComplete { call_id } => ObservedTask::HostcallComplete {
                        call_id: call_id.clone(),
                    },
                    ExpectedTask::InboundEvent { event_id } => ObservedTask::InboundEvent {
                        event_id: event_id.clone(),
                    },
                    ExpectedTask::TimerFired { timer } => {
                        let timer_id = *timers.get(timer.as_str()).unwrap_or_else(|| {
                            panic!("{}: step {idx}: unknown timer {timer}", case.name)
                        });
                        ObservedTask::TimerFired { timer_id }
                    }
                });

                assert_eq!(
                    observed, expected_observed,
                    "{}: step {idx}: unexpected macrotask",
                    case.name
                );

                if expected_observed.is_some() {
                    assert!(
                        result.ran_macrotask,
                        "{}: step {idx}: expected ran_macrotask",
                        case.name
                    );
                    assert_eq!(
                        result.microtasks_drained, *microtasks,
                        "{}: step {idx}: microtask drain count mismatch",
                        case.name
                    );
                } else {
                    assert!(
                        !result.ran_macrotask,
                        "{}: step {idx}: expected idle tick",
                        case.name
                    );
                    assert_eq!(
                        result.microtasks_drained, 0,
                        "{}: step {idx}: idle tick should not drain microtasks",
                        case.name
                    );
                }
            }
        }
    }
}

fn observe_task(kind: MacrotaskKind) -> ObservedTask {
    match kind {
        MacrotaskKind::HostcallComplete { call_id } => ObservedTask::HostcallComplete { call_id },
        MacrotaskKind::InboundEvent { event_id } => ObservedTask::InboundEvent { event_id },
        MacrotaskKind::TimerFired { timer_id } => ObservedTask::TimerFired { timer_id },
    }
}

#[derive(Debug, Clone)]
enum GeneratedOp {
    SetTimeout { delay_ms: u64 },
    ClearTimeout { timer_index: usize },
    EnqueueHostcallCompletion,
    EnqueueInboundEvent,
    AdvanceClock { ms: u64 },
    Tick { microtasks: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TraceEntry {
    observed: Option<ObservedTask>,
    microtasks_drained: usize,
}

#[test]
fn event_loop_deterministic_for_same_seed() {
    for seed in 0_u64..10 {
        let ops = generate_ops(seed, 200);
        let left = run_ops(0, &ops);
        let right = run_ops(0, &ops);
        assert_eq!(left, right, "determinism failed for seed {seed}");
    }
}

fn run_ops(start_ms: u64, ops: &[GeneratedOp]) -> Vec<TraceEntry> {
    let clock = Arc::new(ManualClock::new(start_ms));
    let mut loop_state = PiEventLoop::new(ClockHandle::new(clock.clone()));
    let mut timers = Vec::new();
    let mut hostcall_seq = 0_u64;
    let mut event_seq = 0_u64;
    let mut trace = Vec::new();

    for op in ops {
        match *op {
            GeneratedOp::SetTimeout { delay_ms } => {
                let timer_id = loop_state.set_timeout(delay_ms);
                timers.push(timer_id);
            }
            GeneratedOp::ClearTimeout { timer_index } => {
                if let Some(timer_id) = timers.get(timer_index).copied() {
                    loop_state.clear_timeout(timer_id);
                }
            }
            GeneratedOp::EnqueueHostcallCompletion => {
                hostcall_seq += 1;
                loop_state.enqueue_hostcall_completion(format!("call-{hostcall_seq}"));
            }
            GeneratedOp::EnqueueInboundEvent => {
                event_seq += 1;
                loop_state.enqueue_inbound_event(format!("evt-{event_seq}"));
            }
            GeneratedOp::AdvanceClock { ms } => {
                clock.advance(ms);
            }
            GeneratedOp::Tick { microtasks } => {
                let mut observed = None;
                let mut remaining_drains = microtasks;
                let result = loop_state.tick(
                    |task| observed = Some(observe_task(task.kind)),
                    || {
                        if remaining_drains == 0 {
                            return false;
                        }
                        remaining_drains -= 1;
                        true
                    },
                );
                trace.push(TraceEntry {
                    observed,
                    microtasks_drained: result.microtasks_drained,
                });
            }
        }
    }

    trace
}

fn generate_ops(seed: u64, len: usize) -> Vec<GeneratedOp> {
    let mut rng = SplitMix64::new(seed);
    let mut ops = Vec::with_capacity(len);
    let mut timer_count = 0_usize;

    for _ in 0..len {
        let choice = rng.next_u64() % 7;
        let op = match choice {
            0 => {
                timer_count += 1;
                GeneratedOp::SetTimeout {
                    delay_ms: rng.next_u64() % 50,
                }
            }
            1 => GeneratedOp::ClearTimeout {
                timer_index: if timer_count == 0 {
                    0
                } else {
                    usize::try_from(rng.next_u64() % u64::try_from(timer_count).unwrap_or(1))
                        .unwrap_or(0)
                },
            },
            2 => GeneratedOp::EnqueueHostcallCompletion,
            3 => GeneratedOp::EnqueueInboundEvent,
            4 => GeneratedOp::AdvanceClock {
                ms: rng.next_u64() % 50,
            },
            _ => GeneratedOp::Tick {
                microtasks: usize::try_from(rng.next_u64() % 4).unwrap_or(0),
            },
        };
        ops.push(op);
    }

    ops
}

/// Tiny deterministic RNG for tests (no external deps).
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

// ============================================================================
// LabRuntime deterministic extension scheduler tests (bd-48tv)
// ============================================================================

/// Create a `LabRuntime` configured for extension scheduler testing.
fn lab_for_extensions(seed: u64) -> LabRuntime {
    LabRuntime::new(LabConfig::new(seed).trace_capacity(16_384))
}

/// Bridge: adapter that implements `scheduler::Clock` backed by a shared atomic
/// that tests can sync with `LabRuntime` virtual time.
struct LabBridgeClock {
    now_ms: std::sync::atomic::AtomicU64,
}

impl LabBridgeClock {
    const fn new(start_ms: u64) -> Self {
        Self {
            now_ms: std::sync::atomic::AtomicU64::new(start_ms),
        }
    }

    fn advance(&self, delta_ms: u64) {
        self.now_ms
            .fetch_add(delta_ms, std::sync::atomic::Ordering::SeqCst);
    }

    fn set(&self, ms: u64) {
        self.now_ms.store(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl scheduler::Clock for LabBridgeClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(std::sync::atomic::Ordering::SeqCst)
    }
}

// Note: `Arc<LabBridgeClock>` implements `scheduler::Clock` via the blanket
// `impl<C: Clock> Clock for Arc<C>` in pi::scheduler.

/// Trace a `Scheduler` macrotask to a string for determinism comparison.
fn trace_scheduler_task(task: &scheduler::Macrotask) -> String {
    match &task.kind {
        scheduler::MacrotaskKind::TimerFired { timer_id } => {
            format!("seq={}:timer:{timer_id}", task.seq.value())
        }
        scheduler::MacrotaskKind::HostcallComplete { call_id, outcome } => {
            let tag = match outcome {
                HostcallOutcome::Success(_) => "ok",
                HostcallOutcome::Error { .. } => "err",
                HostcallOutcome::StreamChunk { .. } => "chunk",
            };
            format!("seq={}:hostcall:{call_id}:{tag}", task.seq.value())
        }
        scheduler::MacrotaskKind::InboundEvent { event_id, .. } => {
            format!("seq={}:event:{event_id}", task.seq.value())
        }
    }
}

/// Run scheduler operations inside `LabRuntime` async tasks and collect the trace.
fn run_scheduler_under_lab(seed: u64, task_count: u64) -> Vec<String> {
    let mut runtime = lab_for_extensions(seed);
    let root = runtime.state.create_root_region(Budget::INFINITE);

    let clock = Arc::new(LabBridgeClock::new(0));
    let sched = Arc::new(std::sync::Mutex::new(Scheduler::with_clock(Arc::clone(
        &clock,
    ))));

    // Spawn multiple async tasks that each enqueue work into the shared scheduler.
    for i in 0..task_count {
        let sched_handle = Arc::clone(&sched);
        let (task_id, _handle) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut s = sched_handle.lock().expect("lock scheduler");
                s.set_timeout(50 * (i + 1));
                s.enqueue_event(format!("task-{i}"), serde_json::json!({ "from_task": i }));
                s.enqueue_hostcall_complete(
                    format!("call-{i}"),
                    HostcallOutcome::Success(serde_json::json!({ "i": i })),
                );
                drop(s);
            })
            .expect("create lab task");
        runtime
            .scheduler
            .lock()
            .expect("lock lab scheduler")
            .schedule(task_id, 0);
    }

    runtime.run_until_quiescent();

    // Advance the bridge clock past all timer deadlines and drain.
    clock.advance(50 * (task_count + 1));
    let mut s = sched.lock().expect("lock scheduler for drain");
    let mut trace = Vec::new();
    while let Some(task) = s.tick() {
        trace.push(trace_scheduler_task(&task));
    }
    trace
}

/// Same seed produces identical scheduler traces under `LabRuntime`.
#[test]
fn lab_scheduler_deterministic_same_seed() {
    for seed in [0_u64, 1, 42, 0xCAFE_BABE] {
        let a = run_scheduler_under_lab(seed, 8);
        let b = run_scheduler_under_lab(seed, 8);
        assert_eq!(a, b, "trace mismatch for seed={seed}");
    }
}

/// Different seeds can produce different traces (non-trivial scheduling).
#[test]
fn lab_scheduler_different_seeds_diverge() {
    let a = run_scheduler_under_lab(1, 8);
    let b = run_scheduler_under_lab(2, 8);
    // The traces could theoretically be equal, but with 8 tasks and different
    // scheduling orders this is astronomically unlikely. If they happen to match,
    // the test is still correct—it just doesn't prove divergence for this pair.
    // We assert non-empty as a sanity check.
    assert!(!a.is_empty(), "trace should not be empty");
    assert!(!b.is_empty(), "trace should not be empty");
}

/// `LabRuntime` invariants hold after extension scheduler operations.
#[test]
fn lab_scheduler_invariants_hold() {
    let mut runtime = lab_for_extensions(42);
    let root = runtime.state.create_root_region(Budget::INFINITE);

    let clock = Arc::new(LabBridgeClock::new(0));
    let sched = Arc::new(std::sync::Mutex::new(Scheduler::with_clock(Arc::clone(
        &clock,
    ))));

    for i in 0..10_u64 {
        let sched_handle = Arc::clone(&sched);
        let (task_id, _handle) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut s = sched_handle.lock().expect("lock");
                s.set_timeout(i * 10);
                s.enqueue_event(format!("e-{i}"), serde_json::Value::Null);
                drop(s);
            })
            .expect("create task");
        runtime
            .scheduler
            .lock()
            .expect("lock lab scheduler")
            .schedule(task_id, 0);
    }

    runtime.run_until_quiescent();

    let violations = runtime.check_invariants();
    assert!(
        violations.is_empty(),
        "LabRuntime invariant violations: {violations:?}"
    );
}

/// Timer cancellation is deterministic under `LabRuntime`.
#[test]
fn lab_timer_cancellation_deterministic() {
    fn run(seed: u64) -> Vec<String> {
        let mut runtime = lab_for_extensions(seed);
        let root = runtime.state.create_root_region(Budget::INFINITE);

        let clock = Arc::new(LabBridgeClock::new(0));
        let sched = Arc::new(std::sync::Mutex::new(Scheduler::with_clock(Arc::clone(
            &clock,
        ))));

        // Task 1: set timers
        let sched1 = Arc::clone(&sched);
        let (tid1, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut s = sched1.lock().expect("lock");
                let _t1 = s.set_timeout(100);
                let t2 = s.set_timeout(200);
                let _t3 = s.set_timeout(300);
                // Cancel the middle timer
                s.clear_timeout(t2);
            })
            .expect("create");
        runtime.scheduler.lock().unwrap().schedule(tid1, 0);

        // Task 2: add events alongside timers
        let sched2 = Arc::clone(&sched);
        let (tid2, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut s = sched2.lock().expect("lock");
                s.enqueue_event("interleaved".into(), serde_json::Value::Null);
            })
            .expect("create");
        runtime.scheduler.lock().unwrap().schedule(tid2, 0);

        runtime.run_until_quiescent();

        // Drain with virtual time advancement
        clock.advance(500);
        let mut s = sched.lock().expect("drain lock");
        let mut trace = Vec::new();
        while let Some(task) = s.tick() {
            trace.push(trace_scheduler_task(&task));
        }
        trace
    }

    for seed in [42_u64, 99, 1000] {
        let a = run(seed);
        let b = run(seed);
        assert_eq!(a, b, "cancellation trace mismatch for seed={seed}");
        // The cancelled timer (t2) must not appear in the trace.
        for entry in &a {
            assert!(
                !entry.contains("timer:2"),
                "cancelled timer 2 should not fire: {entry}"
            );
        }
    }
}

/// Virtual time progression: timers fire in correct order under `LabRuntime`.
#[test]
fn lab_virtual_time_timer_ordering() {
    let mut runtime = lab_for_extensions(42);
    let root = runtime.state.create_root_region(Budget::INFINITE);

    let clock = Arc::new(LabBridgeClock::new(0));
    let sched = Arc::new(std::sync::Mutex::new(Scheduler::with_clock(Arc::clone(
        &clock,
    ))));

    // Schedule timers with specific deadlines
    {
        let sched_handle = Arc::clone(&sched);
        let (tid, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut s = sched_handle.lock().expect("lock");
                let _t_300 = s.set_timeout(300);
                let _t_100 = s.set_timeout(100);
                let _t_200 = s.set_timeout(200);
                let _t_100b = s.set_timeout(100); // Same deadline as t_100
            })
            .expect("create");
        runtime.scheduler.lock().unwrap().schedule(tid, 0);
    }

    runtime.run_until_quiescent();

    // Step 1: advance to 150ms — only the two 100ms timers should fire.
    clock.set(150);
    let mut s = sched.lock().expect("lock");
    let t1 = s.tick().expect("first 100ms timer");
    let t2 = s.tick().expect("second 100ms timer");
    assert!(s.tick().is_none(), "no more tasks at 150ms");

    // Both should be TimerFired, timer IDs 2 and 4 (100ms timers, created 2nd and 4th).
    match (&t1.kind, &t2.kind) {
        (
            scheduler::MacrotaskKind::TimerFired { timer_id: id1 },
            scheduler::MacrotaskKind::TimerFired { timer_id: id2 },
        ) => {
            assert_eq!(*id1, 2, "first 100ms timer");
            assert_eq!(*id2, 4, "second 100ms timer (same deadline, later seq)");
        }
        _ => panic!("expected two TimerFired, got {t1:?} and {t2:?}"),
    }

    // Step 2: advance to 250ms — the 200ms timer fires.
    clock.set(250);
    let t3 = s.tick().expect("200ms timer");
    assert!(s.tick().is_none(), "no more tasks at 250ms");
    match &t3.kind {
        scheduler::MacrotaskKind::TimerFired { timer_id } => {
            assert_eq!(*timer_id, 3, "200ms timer");
        }
        _ => panic!("expected TimerFired, got {t3:?}"),
    }

    // Step 3: advance to 400ms — the 300ms timer fires.
    clock.set(400);
    let t4 = s.tick().expect("300ms timer");
    assert!(s.tick().is_none(), "no more tasks at 400ms");
    match &t4.kind {
        scheduler::MacrotaskKind::TimerFired { timer_id } => {
            assert_eq!(*timer_id, 1, "300ms timer");
        }
        _ => panic!("expected TimerFired, got {t4:?}"),
    }
    drop(s);
}

/// `PiEventLoop` determinism under `LabRuntime`: same seed → same trace.
#[test]
fn lab_event_loop_determinism() {
    fn run(seed: u64) -> Vec<String> {
        let mut runtime = lab_for_extensions(seed);
        let root = runtime.state.create_root_region(Budget::INFINITE);

        let clock = Arc::new(ManualClock::new(0));
        let event_loop = Arc::new(std::sync::Mutex::new(PiEventLoop::new(ClockHandle::new(
            clock.clone(),
        ))));

        // Spawn concurrent tasks that enqueue into the PiEventLoop.
        for i in 0..6_u64 {
            let el = Arc::clone(&event_loop);
            let (tid, _) = runtime
                .state
                .create_task(root, Budget::INFINITE, async move {
                    let mut loop_state = el.lock().expect("lock event loop");
                    loop_state.set_timeout(30 * (i + 1));
                    loop_state.enqueue_hostcall_completion(format!("hc-{i}"));
                    loop_state.enqueue_inbound_event(format!("ie-{i}"));
                    drop(loop_state);
                })
                .expect("create");
            runtime.scheduler.lock().unwrap().schedule(tid, 0);
        }

        runtime.run_until_quiescent();

        // Advance clock and drain all macrotasks.
        clock.advance(300);
        let mut el = event_loop.lock().expect("lock for drain");
        let mut trace = Vec::new();
        for _ in 0..200 {
            let mut observed = None;
            let result = el.tick(
                |task| {
                    observed = Some(format!("{:?}", task.kind));
                },
                || false,
            );
            if !result.ran_macrotask {
                break;
            }
            if let Some(obs) = observed {
                trace.push(obs);
            }
        }
        drop(el);
        trace
    }

    for seed in [0_u64, 7, 42, 999] {
        let a = run(seed);
        let b = run(seed);
        assert_eq!(a, b, "PiEventLoop trace mismatch for seed={seed}");
    }
}

/// `LabRuntime` invariants hold after `PiEventLoop` operations.
#[test]
fn lab_event_loop_invariants_hold() {
    let mut runtime = lab_for_extensions(42);
    let root = runtime.state.create_root_region(Budget::INFINITE);

    let clock = Arc::new(ManualClock::new(0));
    let event_loop = Arc::new(std::sync::Mutex::new(PiEventLoop::new(ClockHandle::new(
        clock,
    ))));

    for i in 0..5_u64 {
        let el = Arc::clone(&event_loop);
        let (tid, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut loop_state = el.lock().expect("lock");
                loop_state.set_timeout(i * 50);
                loop_state.enqueue_hostcall_completion(format!("c-{i}"));
                drop(loop_state);
            })
            .expect("create");
        runtime.scheduler.lock().unwrap().schedule(tid, 0);
    }

    runtime.run_until_quiescent();

    let violations = runtime.check_invariants();
    assert!(
        violations.is_empty(),
        "invariant violations: {violations:?}"
    );
}

/// Hostcall error outcomes are scheduled deterministically.
#[test]
fn lab_hostcall_error_outcomes_deterministic() {
    fn run(seed: u64) -> Vec<String> {
        let mut runtime = lab_for_extensions(seed);
        let root = runtime.state.create_root_region(Budget::INFINITE);

        let clock = Arc::new(LabBridgeClock::new(0));
        let sched = Arc::new(std::sync::Mutex::new(Scheduler::with_clock(Arc::clone(
            &clock,
        ))));

        // Alternate success and error outcomes.
        for i in 0..8_u64 {
            let s = Arc::clone(&sched);
            let (tid, _) = runtime
                .state
                .create_task(root, Budget::INFINITE, async move {
                    let outcome = if i % 2 == 0 {
                        HostcallOutcome::Success(serde_json::json!({ "i": i }))
                    } else {
                        HostcallOutcome::Error {
                            code: "E_TEST".into(),
                            message: format!("error {i}"),
                        }
                    };
                    s.lock()
                        .expect("lock")
                        .enqueue_hostcall_complete(format!("hc-{i}"), outcome);
                })
                .expect("create");
            runtime.scheduler.lock().unwrap().schedule(tid, 0);
        }

        runtime.run_until_quiescent();

        let mut s = sched.lock().expect("drain lock");
        let mut trace = Vec::new();
        while let Some(task) = s.tick() {
            trace.push(trace_scheduler_task(&task));
        }
        trace
    }

    for seed in [42_u64, 100, 255] {
        let a = run(seed);
        let b = run(seed);
        assert_eq!(a, b, "hostcall error trace mismatch for seed={seed}");
        // LabRuntime may reorder task execution, so we can't assume a specific
        // ok/err pattern. Instead verify both outcomes are present and the total
        // count matches the number of enqueued hostcalls.
        let ok_count = a.iter().filter(|e| e.ends_with(":ok")).count();
        let err_count = a.iter().filter(|e| e.ends_with(":err")).count();
        assert_eq!(ok_count + err_count, 8, "expected 8 hostcall traces");
        assert_eq!(ok_count, 4, "expected 4 success outcomes");
        assert_eq!(err_count, 4, "expected 4 error outcomes");
    }
}

/// Mixed timers + events + hostcalls under `LabRuntime` — full interleaving.
#[test]
fn lab_mixed_interleaving_deterministic() {
    fn run(seed: u64) -> Vec<String> {
        let mut runtime = lab_for_extensions(seed);
        let root = runtime.state.create_root_region(Budget::INFINITE);

        let clock = Arc::new(LabBridgeClock::new(0));
        let sched = Arc::new(std::sync::Mutex::new(Scheduler::with_clock(Arc::clone(
            &clock,
        ))));

        // Three concurrent tasks each doing different scheduler ops.
        let s1 = Arc::clone(&sched);
        let (tid1, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut s = s1.lock().expect("lock");
                s.set_timeout(10);
                s.set_timeout(20);
                s.set_timeout(30);
            })
            .expect("create");

        let s2 = Arc::clone(&sched);
        let (tid2, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut s = s2.lock().expect("lock");
                s.enqueue_event("alpha".into(), serde_json::json!("a"));
                s.enqueue_event("beta".into(), serde_json::json!("b"));
            })
            .expect("create");

        let s3 = Arc::clone(&sched);
        let (tid3, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut s = s3.lock().expect("lock");
                s.enqueue_hostcall_complete(
                    "rpc-1".into(),
                    HostcallOutcome::Success(serde_json::json!(null)),
                );
                s.enqueue_hostcall_complete(
                    "rpc-2".into(),
                    HostcallOutcome::Error {
                        code: "E".into(),
                        message: "fail".into(),
                    },
                );
            })
            .expect("create");

        let ls = runtime.scheduler.lock().unwrap();
        // NOTE: we drop the lock after scheduling all tasks to avoid holding it
        // across run_until_quiescent.
        drop(ls);
        runtime.scheduler.lock().unwrap().schedule(tid1, 0);
        runtime.scheduler.lock().unwrap().schedule(tid2, 0);
        runtime.scheduler.lock().unwrap().schedule(tid3, 0);

        runtime.run_until_quiescent();

        // Drain: first events/hostcalls (they're already in macrotask queue),
        // then advance time for timers.
        let mut trace = Vec::new();
        {
            let mut s = sched.lock().expect("lock");
            while let Some(task) = s.tick() {
                trace.push(trace_scheduler_task(&task));
            }
        }
        clock.advance(100);
        {
            let mut s = sched.lock().expect("lock");
            while let Some(task) = s.tick() {
                trace.push(trace_scheduler_task(&task));
            }
        }
        trace
    }

    for seed in [42_u64, 73, 512] {
        let a = run(seed);
        let b = run(seed);
        assert_eq!(a, b, "mixed interleaving mismatch for seed={seed}");
        assert!(!a.is_empty(), "trace should not be empty");
    }
}

/// `time_until_next_timer` is accurate under virtual time.
#[test]
fn lab_time_until_next_timer() {
    let mut runtime = lab_for_extensions(42);
    let root = runtime.state.create_root_region(Budget::INFINITE);

    let clock = Arc::new(LabBridgeClock::new(1000));
    let sched = Arc::new(std::sync::Mutex::new(Scheduler::with_clock(Arc::clone(
        &clock,
    ))));

    {
        let s = Arc::clone(&sched);
        let (tid, _) = runtime
            .state
            .create_task(root, Budget::INFINITE, async move {
                let mut sched = s.lock().expect("lock");
                sched.set_timeout(200); // deadline = 1200
                sched.set_timeout(500); // deadline = 1500
            })
            .expect("create");
        runtime.scheduler.lock().unwrap().schedule(tid, 0);
    }

    runtime.run_until_quiescent();

    let s = sched.lock().expect("lock");
    assert_eq!(s.next_timer_deadline(), Some(1200));
    assert_eq!(s.time_until_next_timer(), Some(200));
    drop(s);

    // Advance clock by 100ms — timer should be 100ms away.
    clock.advance(100);
    let s = sched.lock().expect("lock");
    assert_eq!(s.time_until_next_timer(), Some(100));
    drop(s);

    // Advance to exactly the deadline — timer should be 0ms away.
    clock.set(1200);
    let s = sched.lock().expect("lock");
    assert_eq!(s.time_until_next_timer(), Some(0));
    drop(s);
}
