#![cfg(feature = "loom-tests")]

use loom::sync::{Arc, Condvar, Mutex};
use loom::thread;
use pi::hostcall_queue::{
    BravoBiasMode, ContentionSample, ContentionSignature, HostcallQueueMode, HostcallRequestQueue,
};

// `generator`, which backs Loom's simulated threads, defaults to a 4 KiB
// coroutine stack. The queue's contention classifier legitimately exceeds
// that budget in debug/all-feature builds, causing the model to abort before
// exploring any interleavings. Keep the model and assertions unchanged while
// giving every simulated thread a deterministic, bounded stack.
const LOOM_THREAD_STACK_BYTES: usize = 128 * 1024;

fn spawn_loom_thread<F, T>(f: F) -> thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new()
        .stack_size(LOOM_THREAD_STACK_BYTES)
        .spawn(f)
        .expect("spawn Loom thread")
}

// The reviewed upstream Loom revision exposes one default for the root and all
// spawned model threads. Set it explicitly rather than relying on the
// generator crate's 4 KiB default; individual Builder-spawned threads below
// retain the same bound explicitly as defense in depth.
fn check_loom_model(model: fn()) {
    let mut builder = loom::model::Builder::new();
    builder.stack_size = Some(LOOM_THREAD_STACK_BYTES);
    builder.check(model);
}

struct PinGate {
    pin_ready: bool,
    release_pin: bool,
}

fn wait_until_pin_ready(sync: &Arc<(Mutex<PinGate>, Condvar)>) {
    let (lock, cvar) = &**sync;
    let mut gate = lock.lock().expect("lock pin gate");
    while !gate.pin_ready {
        gate = cvar.wait(gate).expect("wait for pin");
    }
}

fn mark_pin_ready(sync: &Arc<(Mutex<PinGate>, Condvar)>) {
    let (lock, cvar) = &**sync;
    let mut gate = lock.lock().expect("lock pin gate");
    gate.pin_ready = true;
    cvar.notify_all();
    while !gate.release_pin {
        gate = cvar.wait(gate).expect("wait for pin release");
    }
}

fn release_pin(sync: &Arc<(Mutex<PinGate>, Condvar)>) {
    let (lock, cvar) = &**sync;
    let mut gate = lock.lock().expect("lock pin gate");
    gate.release_pin = true;
    cvar.notify_all();
}

const fn starvation_sample() -> ContentionSample {
    ContentionSample {
        read_acquires: 80,
        write_acquires: 20,
        read_wait_p95_us: 120,
        write_wait_p95_us: 9_000,
        write_timeouts: 3,
    }
}

const fn mixed_sample() -> ContentionSample {
    ContentionSample {
        read_acquires: 45,
        write_acquires: 55,
        read_wait_p95_us: 150,
        write_wait_p95_us: 450,
        write_timeouts: 0,
    }
}

fn loom_epoch_pin_blocks_reclamation_until_release() {
    check_loom_model(|| {
        let queue = Arc::new(Mutex::new(HostcallRequestQueue::<u8>::with_mode(
            1,
            2,
            HostcallQueueMode::Ebr,
        )));
        let pin_gate = Arc::new((
            Mutex::new(PinGate {
                pin_ready: false,
                release_pin: false,
            }),
            Condvar::new(),
        ));

        let queue_for_pin = Arc::clone(&queue);
        let pin_gate_for_thread = Arc::clone(&pin_gate);
        let pin_thread = spawn_loom_thread(move || {
            let pin = queue_for_pin.lock().expect("lock queue").pin_epoch();
            mark_pin_ready(&pin_gate_for_thread);
            drop(pin);
        });

        let queue_for_worker = Arc::clone(&queue);
        let pin_gate_for_worker = Arc::clone(&pin_gate);
        let worker = spawn_loom_thread(move || {
            wait_until_pin_ready(&pin_gate_for_worker);

            let mut queue = queue_for_worker.lock().expect("lock queue");
            let _ = queue.push_back(1_u8);
            let _ = queue.push_back(2_u8);
            let drained = queue.drain_all();
            assert_eq!(drained.len(), 2);

            queue.force_reclaim();
            let snapshot = queue.snapshot();
            assert_eq!(snapshot.reclamation_mode, HostcallQueueMode::Ebr);
            assert!(snapshot.retired_backlog >= 2);
            assert_eq!(snapshot.reclaimed_total, 0);
            drop(queue);

            release_pin(&pin_gate_for_worker);
        });

        worker.join().expect("worker join");
        pin_thread.join().expect("pin thread join");

        let mut queue = queue.lock().expect("lock queue");
        queue.force_reclaim();
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.retired_backlog, 0);
        assert!(snapshot.reclaimed_total >= 2);
        drop(queue);
    });
}

fn loom_concurrent_enqueue_dequeue_keeps_values_unique() {
    check_loom_model(|| {
        let queue = Arc::new(Mutex::new(HostcallRequestQueue::<u8>::with_mode(
            2,
            2,
            HostcallQueueMode::SafeFallback,
        )));

        let queue_a = Arc::clone(&queue);
        let producer_a = spawn_loom_thread(move || {
            let mut queue = queue_a.lock().expect("lock queue");
            let _ = queue.push_back(10_u8);
        });

        let queue_b = Arc::clone(&queue);
        let producer_b = spawn_loom_thread(move || {
            let mut queue = queue_b.lock().expect("lock queue");
            let _ = queue.push_back(11_u8);
        });

        producer_a.join().expect("producer_a join");
        producer_b.join().expect("producer_b join");

        let mut queue = queue.lock().expect("lock queue");
        let drained = queue.drain_all();
        drop(queue);
        let mut values = drained.into_iter().collect::<Vec<_>>();
        values.sort_unstable();
        assert_eq!(values, vec![10, 11]);
    });
}

fn loom_repeated_safe_fallback_switch_is_idempotent() {
    check_loom_model(|| {
        let queue = Arc::new(Mutex::new(HostcallRequestQueue::<u8>::with_mode(
            2,
            2,
            HostcallQueueMode::Ebr,
        )));

        let queue_a = Arc::clone(&queue);
        let switcher_a = spawn_loom_thread(move || {
            let mut queue = queue_a.lock().expect("lock queue");
            queue.force_safe_fallback();
        });

        let queue_b = Arc::clone(&queue);
        let switcher_b = spawn_loom_thread(move || {
            let mut queue = queue_b.lock().expect("lock queue");
            queue.force_safe_fallback();
        });

        switcher_a.join().expect("switcher_a join");
        switcher_b.join().expect("switcher_b join");

        let snapshot = queue.lock().expect("lock queue").snapshot();
        assert_eq!(snapshot.reclamation_mode, HostcallQueueMode::SafeFallback);
        assert_eq!(snapshot.fallback_transitions, 1);
    });
}

fn loom_bravo_writer_recovery_is_bounded_under_concurrent_starvation() {
    check_loom_model(|| {
        let queue = Arc::new(Mutex::new(HostcallRequestQueue::<u8>::with_mode(
            4,
            4,
            HostcallQueueMode::SafeFallback,
        )));

        let queue_a = Arc::clone(&queue);
        let starvation_a = spawn_loom_thread(move || {
            let mut queue = queue_a.lock().expect("lock queue");
            let _ = queue.observe_contention_window(starvation_sample());
        });

        let queue_b = Arc::clone(&queue);
        let starvation_b = spawn_loom_thread(move || {
            let mut queue = queue_b.lock().expect("lock queue");
            let _ = queue.observe_contention_window(starvation_sample());
        });

        starvation_a.join().expect("starvation_a join");
        starvation_b.join().expect("starvation_b join");

        let snapshot = queue.lock().expect("lock queue").snapshot();
        assert_eq!(snapshot.bravo_mode, BravoBiasMode::WriterRecovery);
        assert_eq!(
            snapshot.bravo_last_signature,
            ContentionSignature::WriterStarvationRisk
        );
        assert!(
            snapshot.bravo_writer_recovery_remaining <= 2,
            "writer recovery window should stay bounded by config default (2)"
        );
        assert!(
            snapshot.bravo_rollbacks >= 1,
            "starvation must trigger at least one rollback"
        );
    });
}

fn loom_bravo_writer_recovery_returns_to_balanced_without_stale_counters() {
    check_loom_model(|| {
        let queue = Arc::new(Mutex::new(HostcallRequestQueue::<u8>::with_mode(
            4,
            4,
            HostcallQueueMode::SafeFallback,
        )));

        let queue_for_starvation = Arc::clone(&queue);
        let starvation_thread = spawn_loom_thread(move || {
            let mut queue = queue_for_starvation.lock().expect("lock queue");
            let _ = queue.observe_contention_window(starvation_sample());
        });
        starvation_thread.join().expect("starvation thread join");

        {
            let mut queue = queue.lock().expect("lock queue");
            for _ in 0..2 {
                let _ = queue.observe_contention_window(mixed_sample());
            }
        }

        let snapshot = queue.lock().expect("lock queue").snapshot();
        assert_eq!(snapshot.bravo_mode, BravoBiasMode::Balanced);
        assert_eq!(snapshot.bravo_writer_recovery_remaining, 0);
        assert_eq!(
            snapshot.bravo_last_signature,
            ContentionSignature::MixedContention
        );
    });
}

#[test]
fn loom_hostcall_queue_models() {
    // The generator crate's process-global stack-overflow machinery can
    // deadlock when several independent Loom models initialize concurrently.
    // A single harness test runs every unchanged model and assertion in a
    // deterministic sequence while each model still exhaustively schedules
    // its own simulated threads.
    loom_epoch_pin_blocks_reclamation_until_release();
    loom_concurrent_enqueue_dequeue_keeps_values_unique();
    loom_repeated_safe_fallback_switch_is_idempotent();
    loom_bravo_writer_recovery_is_bounded_under_concurrent_starvation();
    loom_bravo_writer_recovery_returns_to_balanced_without_stale_counters();
}
