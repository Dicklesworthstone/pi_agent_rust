//! Deterministic event loop scheduler for PiJS runtime.
//!
//! Implements the spec from EXTENSIONS.md §1A.4.5:
//! - Queue model: microtasks (handled by JS engine), macrotasks, timers
//! - Timer heap with stable ordering guarantees
//! - Hostcall completion enqueue with stable tie-breaking
//! - Single-threaded scheduler loop reproducible under fixed inputs
//!
//! # Invariants
//!
//! - **I1 (single macrotask):** at most one macrotask executes per tick
//! - **I2 (microtask fixpoint):** after any macrotask, microtasks drain to empty
//! - **I3 (stable timers):** timers with equal deadlines fire in increasing seq order
//! - **I4 (no reentrancy):** hostcall completions enqueue macrotasks, never re-enter
//! - **I5 (total order):** all observable scheduling is ordered by seq

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

/// Monotonically increasing sequence counter for deterministic ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(u64);

impl Seq {
    /// Create the initial sequence value.
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Get the next sequence value, incrementing the counter.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// Get the raw value.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "seq:{}", self.0)
    }
}

/// A timer entry in the timer heap.
#[derive(Debug, Clone)]
pub struct TimerEntry {
    /// Timer ID for cancellation.
    pub timer_id: u64,
    /// Absolute deadline in milliseconds.
    pub deadline_ms: u64,
    /// Sequence number for stable ordering.
    pub seq: Seq,
}

impl TimerEntry {
    /// Create a new timer entry.
    #[must_use]
    pub const fn new(timer_id: u64, deadline_ms: u64, seq: Seq) -> Self {
        Self {
            timer_id,
            deadline_ms,
            seq,
        }
    }
}

// Order by (deadline_ms, seq) ascending - min-heap needs reversed comparison.
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline_ms == other.deadline_ms && self.seq == other.seq
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for min-heap: smaller deadline/seq = higher priority
        match other.deadline_ms.cmp(&self.deadline_ms) {
            Ordering::Equal => other.seq.cmp(&self.seq),
            ord => ord,
        }
    }
}

/// Type of macrotask in the queue.
#[derive(Debug, Clone)]
pub enum MacrotaskKind {
    /// A timer fired.
    TimerFired { timer_id: u64 },
    /// A hostcall completed.
    HostcallComplete {
        call_id: String,
        outcome: HostcallOutcome,
    },
    /// An inbound event from the host.
    InboundEvent {
        event_id: String,
        payload: serde_json::Value,
    },
}

/// Outcome of a hostcall.
#[derive(Debug, Clone)]
pub enum HostcallOutcome {
    /// Successful result.
    Success(serde_json::Value),
    /// Error result.
    Error { code: String, message: String },
}

/// A macrotask in the queue.
#[derive(Debug, Clone)]
pub struct Macrotask {
    /// Sequence number for deterministic ordering.
    pub seq: Seq,
    /// The task kind and payload.
    pub kind: MacrotaskKind,
}

impl Macrotask {
    /// Create a new macrotask.
    #[must_use]
    pub const fn new(seq: Seq, kind: MacrotaskKind) -> Self {
        Self { seq, kind }
    }
}

// Order by seq ascending.
impl PartialEq for Macrotask {
    fn eq(&self, other: &Self) -> bool {
        self.seq == other.seq
    }
}

impl Eq for Macrotask {}

impl PartialOrd for Macrotask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Macrotask {
    fn cmp(&self, other: &Self) -> Ordering {
        // VecDeque FIFO order - no reordering needed, but we use seq for verification
        self.seq.cmp(&other.seq)
    }
}

/// A monotonic clock source for the scheduler.
pub trait Clock: Send + Sync {
    /// Get the current time in milliseconds since epoch.
    fn now_ms(&self) -> u64;
}

impl<C: Clock> Clock for Arc<C> {
    fn now_ms(&self) -> u64 {
        self.as_ref().now_ms()
    }
}

/// Real wall clock implementation.
#[derive(Debug, Clone, Copy, Default)]
pub struct WallClock;

impl Clock for WallClock {
    fn now_ms(&self) -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time before epoch")
            .as_millis();
        u64::try_from(millis).unwrap_or(u64::MAX)
    }
}

/// A deterministic clock for testing.
#[derive(Debug)]
pub struct DeterministicClock {
    current_ms: std::sync::atomic::AtomicU64,
}

impl DeterministicClock {
    /// Create a new deterministic clock starting at the given time.
    #[must_use]
    pub const fn new(start_ms: u64) -> Self {
        Self {
            current_ms: std::sync::atomic::AtomicU64::new(start_ms),
        }
    }

    /// Advance the clock by the given duration.
    pub fn advance(&self, ms: u64) {
        self.current_ms
            .fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }

    /// Set the clock to a specific time.
    pub fn set(&self, ms: u64) {
        self.current_ms
            .store(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for DeterministicClock {
    fn now_ms(&self) -> u64 {
        self.current_ms.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The deterministic event loop scheduler state.
pub struct Scheduler<C: Clock = WallClock> {
    /// Monotone sequence counter.
    seq: Seq,
    /// Macrotask queue (FIFO, ordered by seq).
    macrotask_queue: VecDeque<Macrotask>,
    /// Timer heap (min-heap by deadline_ms, seq).
    timer_heap: BinaryHeap<TimerEntry>,
    /// Next timer ID.
    next_timer_id: u64,
    /// Cancelled timer IDs.
    cancelled_timers: std::collections::HashSet<u64>,
    /// Clock source.
    clock: C,
}

impl Scheduler<WallClock> {
    /// Create a new scheduler with the default wall clock.
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock(WallClock)
    }
}

impl Default for Scheduler<WallClock> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: Clock> Scheduler<C> {
    /// Create a new scheduler with a custom clock.
    #[must_use]
    pub fn with_clock(clock: C) -> Self {
        Self {
            seq: Seq::zero(),
            macrotask_queue: VecDeque::new(),
            timer_heap: BinaryHeap::new(),
            next_timer_id: 1,
            cancelled_timers: std::collections::HashSet::new(),
            clock,
        }
    }

    /// Get the current sequence number.
    #[must_use]
    pub const fn current_seq(&self) -> Seq {
        self.seq
    }

    /// Get the next sequence number and increment the counter.
    const fn next_seq(&mut self) -> Seq {
        let current = self.seq;
        self.seq = self.seq.next();
        current
    }

    /// Get the current time from the clock.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Check if there are pending tasks.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.macrotask_queue.is_empty() || !self.timer_heap.is_empty()
    }

    /// Get the number of pending macrotasks.
    #[must_use]
    pub fn macrotask_count(&self) -> usize {
        self.macrotask_queue.len()
    }

    /// Get the number of pending timers.
    #[must_use]
    pub fn timer_count(&self) -> usize {
        self.timer_heap.len()
    }

    /// Schedule a timer to fire at the given deadline.
    ///
    /// Returns the timer ID for cancellation.
    pub fn set_timeout(&mut self, delay_ms: u64) -> u64 {
        let timer_id = self.next_timer_id;
        self.next_timer_id += 1;
        let deadline_ms = self.clock.now_ms() + delay_ms;
        let seq = self.next_seq();

        self.timer_heap
            .push(TimerEntry::new(timer_id, deadline_ms, seq));

        tracing::trace!(
            event = "scheduler.timer.set",
            timer_id,
            delay_ms,
            deadline_ms,
            %seq,
            "Timer scheduled"
        );

        timer_id
    }

    /// Cancel a timer by ID.
    ///
    /// Returns true if the timer was found and cancelled.
    pub fn clear_timeout(&mut self, timer_id: u64) -> bool {
        // Mark as cancelled; will be skipped when popped
        let inserted = self.cancelled_timers.insert(timer_id);

        tracing::trace!(
            event = "scheduler.timer.cancel",
            timer_id,
            cancelled = inserted,
            "Timer cancelled"
        );

        inserted
    }

    /// Enqueue a hostcall completion.
    pub fn enqueue_hostcall_complete(&mut self, call_id: String, outcome: HostcallOutcome) {
        let seq = self.next_seq();
        tracing::trace!(
            event = "scheduler.hostcall.enqueue",
            call_id = %call_id,
            %seq,
            "Hostcall completion enqueued"
        );
        let task = Macrotask::new(seq, MacrotaskKind::HostcallComplete { call_id, outcome });
        self.macrotask_queue.push_back(task);
    }

    /// Enqueue an inbound event from the host.
    pub fn enqueue_event(&mut self, event_id: String, payload: serde_json::Value) {
        let seq = self.next_seq();
        tracing::trace!(
            event = "scheduler.event.enqueue",
            event_id = %event_id,
            %seq,
            "Inbound event enqueued"
        );
        let task = Macrotask::new(seq, MacrotaskKind::InboundEvent { event_id, payload });
        self.macrotask_queue.push_back(task);
    }

    /// Move due timers from the timer heap to the macrotask queue.
    ///
    /// This is step 2 of the tick() algorithm.
    fn move_due_timers(&mut self) {
        let now = self.clock.now_ms();

        while let Some(entry) = self.timer_heap.peek() {
            if entry.deadline_ms > now {
                break;
            }

            let entry = self.timer_heap.pop().expect("peeked");

            // Skip cancelled timers
            if self.cancelled_timers.remove(&entry.timer_id) {
                tracing::trace!(
                    event = "scheduler.timer.skip_cancelled",
                    timer_id = entry.timer_id,
                    "Skipped cancelled timer"
                );
                continue;
            }

            // The timer keeps its original seq for ordering
            let task = Macrotask::new(
                entry.seq,
                MacrotaskKind::TimerFired {
                    timer_id: entry.timer_id,
                },
            );
            self.macrotask_queue.push_back(task);

            tracing::trace!(
                event = "scheduler.timer.fire",
                timer_id = entry.timer_id,
                deadline_ms = entry.deadline_ms,
                now_ms = now,
                seq = %entry.seq,
                "Timer fired"
            );
        }
    }

    /// Execute one tick of the event loop.
    ///
    /// Algorithm (from spec):
    /// 1. Ingest host completions (done externally via enqueue methods)
    /// 2. Move due timers to macrotask queue
    /// 3. Run one macrotask (if any)
    /// 4. Drain microtasks (done externally by JS engine)
    ///
    /// Returns the macrotask that was executed, if any.
    pub fn tick(&mut self) -> Option<Macrotask> {
        // Step 2: Move due timers
        self.move_due_timers();

        // Step 3: Run one macrotask
        let task = self.macrotask_queue.pop_front();

        if let Some(ref task) = task {
            tracing::debug!(
                event = "scheduler.tick.execute",
                seq = %task.seq,
                kind = ?std::mem::discriminant(&task.kind),
                "Executing macrotask"
            );
        } else {
            tracing::trace!(event = "scheduler.tick.idle", "No macrotask to execute");
        }

        task
    }

    /// Get the deadline of the next timer, if any.
    #[must_use]
    pub fn next_timer_deadline(&self) -> Option<u64> {
        self.timer_heap
            .iter()
            .filter(|entry| !self.cancelled_timers.contains(&entry.timer_id))
            .map(|entry| entry.deadline_ms)
            .min()
    }

    /// Get the time until the next timer fires, if any.
    #[must_use]
    pub fn time_until_next_timer(&self) -> Option<u64> {
        self.next_timer_deadline()
            .map(|deadline| deadline.saturating_sub(self.clock.now_ms()))
    }
}

impl<C: Clock> fmt::Debug for Scheduler<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler")
            .field("seq", &self.seq)
            .field("macrotask_count", &self.macrotask_queue.len())
            .field("timer_count", &self.timer_heap.len())
            .field("next_timer_id", &self.next_timer_id)
            .field("cancelled_timers", &self.cancelled_timers.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_ordering() {
        let a = Seq::zero();
        let b = a.next();
        let c = b.next();

        assert!(a < b);
        assert!(b < c);
        assert_eq!(a.value(), 0);
        assert_eq!(b.value(), 1);
        assert_eq!(c.value(), 2);
    }

    #[test]
    fn timer_ordering() {
        // Earlier deadline = higher priority (lower in min-heap)
        let t1 = TimerEntry::new(1, 100, Seq(0));
        let t2 = TimerEntry::new(2, 200, Seq(1));

        assert!(t1 > t2); // Reversed for min-heap

        // Same deadline, earlier seq = higher priority
        let t3 = TimerEntry::new(3, 100, Seq(5));
        let t4 = TimerEntry::new(4, 100, Seq(10));

        assert!(t3 > t4); // Reversed for min-heap
    }

    #[test]
    fn deterministic_clock() {
        let clock = DeterministicClock::new(1000);
        assert_eq!(clock.now_ms(), 1000);

        clock.advance(500);
        assert_eq!(clock.now_ms(), 1500);

        clock.set(2000);
        assert_eq!(clock.now_ms(), 2000);
    }

    #[test]
    fn scheduler_basic_timer() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        // Set a timer for 100ms
        let timer_id = sched.set_timeout(100);
        assert_eq!(timer_id, 1);
        assert_eq!(sched.timer_count(), 1);
        assert!(!sched.macrotask_queue.is_empty() || sched.timer_count() > 0);

        // Tick before deadline - nothing happens
        let task = sched.tick();
        assert!(task.is_none());

        // Advance past deadline
        sched.clock.advance(150);
        let task = sched.tick();
        assert!(task.is_some());
        match task.unwrap().kind {
            MacrotaskKind::TimerFired { timer_id: id } => assert_eq!(id, timer_id),
            _ => panic!("Expected TimerFired"),
        }
    }

    #[test]
    fn scheduler_timer_ordering() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        // Set timers in reverse order
        let t3 = sched.set_timeout(300);
        let t1 = sched.set_timeout(100);
        let t2 = sched.set_timeout(200);

        // Advance past all deadlines
        sched.clock.advance(400);

        // Should fire in deadline order
        let task1 = sched.tick().unwrap();
        let task2 = sched.tick().unwrap();
        let task3 = sched.tick().unwrap();

        match task1.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t1),
            _ => panic!("Expected t1"),
        }
        match task2.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t2),
            _ => panic!("Expected t2"),
        }
        match task3.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t3),
            _ => panic!("Expected t3"),
        }
    }

    #[test]
    fn scheduler_same_deadline_seq_ordering() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        // Set timers with same deadline - should fire in seq order
        let t1 = sched.set_timeout(100);
        let t2 = sched.set_timeout(100);
        let t3 = sched.set_timeout(100);

        sched.clock.advance(150);

        let task1 = sched.tick().unwrap();
        let task2 = sched.tick().unwrap();
        let task3 = sched.tick().unwrap();

        // Must fire in order they were created (by seq)
        match task1.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t1),
            _ => panic!("Expected t1"),
        }
        match task2.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t2),
            _ => panic!("Expected t2"),
        }
        match task3.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t3),
            _ => panic!("Expected t3"),
        }
    }

    #[test]
    fn scheduler_cancel_timer() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        let t1 = sched.set_timeout(100);
        let t2 = sched.set_timeout(200);

        // Cancel t1
        assert!(sched.clear_timeout(t1));

        // Advance past both deadlines
        sched.clock.advance(250);

        // Only t2 should fire
        let task = sched.tick().unwrap();
        match task.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t2),
            _ => panic!("Expected t2"),
        }

        // No more tasks
        assert!(sched.tick().is_none());
    }

    #[test]
    fn scheduler_hostcall_completion() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        sched.enqueue_hostcall_complete(
            "call-1".to_string(),
            HostcallOutcome::Success(serde_json::json!({"result": 42})),
        );

        let task = sched.tick().unwrap();
        match task.kind {
            MacrotaskKind::HostcallComplete { call_id, outcome } => {
                assert_eq!(call_id, "call-1");
                match outcome {
                    HostcallOutcome::Success(v) => assert_eq!(v["result"], 42),
                    HostcallOutcome::Error { .. } => panic!("Expected success"),
                }
            }
            _ => panic!("Expected HostcallComplete"),
        }
    }

    #[test]
    fn scheduler_event_ordering() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        // Enqueue events in order
        sched.enqueue_event("evt-1".to_string(), serde_json::json!({"n": 1}));
        sched.enqueue_event("evt-2".to_string(), serde_json::json!({"n": 2}));

        // Should dequeue in FIFO order
        let task1 = sched.tick().unwrap();
        let task2 = sched.tick().unwrap();

        match task1.kind {
            MacrotaskKind::InboundEvent { event_id, .. } => assert_eq!(event_id, "evt-1"),
            _ => panic!("Expected evt-1"),
        }
        match task2.kind {
            MacrotaskKind::InboundEvent { event_id, .. } => assert_eq!(event_id, "evt-2"),
            _ => panic!("Expected evt-2"),
        }
    }

    #[test]
    fn scheduler_mixed_tasks_ordering() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        // Set a timer
        let _t1 = sched.set_timeout(50);

        // Enqueue an event (gets earlier seq)
        sched.enqueue_event("evt-1".to_string(), serde_json::json!({}));

        // Advance past timer
        sched.clock.advance(100);

        // Event should come first (enqueued before timer moved to queue)
        let task1 = sched.tick().unwrap();
        match task1.kind {
            MacrotaskKind::InboundEvent { event_id, .. } => assert_eq!(event_id, "evt-1"),
            _ => panic!("Expected event first"),
        }

        // Then timer
        let task2 = sched.tick().unwrap();
        match task2.kind {
            MacrotaskKind::TimerFired { .. } => {}
            _ => panic!("Expected timer second"),
        }
    }

    #[test]
    fn scheduler_invariant_single_macrotask_per_tick() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        sched.enqueue_event("evt-1".to_string(), serde_json::json!({}));
        sched.enqueue_event("evt-2".to_string(), serde_json::json!({}));
        sched.enqueue_event("evt-3".to_string(), serde_json::json!({}));

        // Each tick returns exactly one task (I1)
        assert!(sched.tick().is_some());
        assert_eq!(sched.macrotask_count(), 2);

        assert!(sched.tick().is_some());
        assert_eq!(sched.macrotask_count(), 1);

        assert!(sched.tick().is_some());
        assert_eq!(sched.macrotask_count(), 0);

        assert!(sched.tick().is_none());
    }

    #[test]
    fn scheduler_next_timer_deadline() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        assert!(sched.next_timer_deadline().is_none());

        sched.set_timeout(200);
        sched.set_timeout(100);
        sched.set_timeout(300);

        assert_eq!(sched.next_timer_deadline(), Some(100));
        assert_eq!(sched.time_until_next_timer(), Some(100));

        sched.clock.advance(50);
        assert_eq!(sched.time_until_next_timer(), Some(50));
    }

    #[test]
    fn scheduler_next_timer_skips_cancelled_timers() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        let t1 = sched.set_timeout(100);
        let _t2 = sched.set_timeout(200);
        let _t3 = sched.set_timeout(300);

        assert!(sched.clear_timeout(t1));
        assert_eq!(sched.next_timer_deadline(), Some(200));
        assert_eq!(sched.time_until_next_timer(), Some(200));
    }

    #[test]
    fn scheduler_debug_format() {
        let clock = DeterministicClock::new(0);
        let sched = Scheduler::with_clock(clock);
        let debug = format!("{sched:?}");
        assert!(debug.contains("Scheduler"));
        assert!(debug.contains("seq"));
    }

    #[derive(Debug, Clone)]
    struct XorShift64 {
        state: u64,
    }

    impl XorShift64 {
        const fn new(seed: u64) -> Self {
            // Avoid the all-zero state so the stream doesn't get stuck.
            let seed = seed ^ 0x9E37_79B9_7F4A_7C15;
            Self { state: seed }
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.state;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.state = x;
            x
        }

        fn next_range_u64(&mut self, upper_exclusive: u64) -> u64 {
            if upper_exclusive == 0 {
                return 0;
            }
            self.next_u64() % upper_exclusive
        }

        fn next_usize(&mut self, upper_exclusive: usize) -> usize {
            let upper = u64::try_from(upper_exclusive).expect("usize fits in u64");
            let value = self.next_range_u64(upper);
            usize::try_from(value).expect("value < upper_exclusive")
        }
    }

    fn trace_entry(task: &Macrotask) -> String {
        match &task.kind {
            MacrotaskKind::TimerFired { timer_id } => {
                format!("seq={}:timer:{timer_id}", task.seq.value())
            }
            MacrotaskKind::HostcallComplete { call_id, outcome } => {
                let outcome_tag = match outcome {
                    HostcallOutcome::Success(_) => "ok",
                    HostcallOutcome::Error { .. } => "err",
                };
                format!("seq={}:hostcall:{call_id}:{outcome_tag}", task.seq.value())
            }
            MacrotaskKind::InboundEvent { event_id, payload } => {
                format!(
                    "seq={}:event:{event_id}:payload={payload}",
                    task.seq.value()
                )
            }
        }
    }

    fn run_seeded_script(seed: u64) -> Vec<String> {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);
        let mut rng = XorShift64::new(seed);
        let mut timers = Vec::new();
        let mut trace = Vec::new();

        for step in 0..256u64 {
            match rng.next_range_u64(6) {
                0 => {
                    let delay_ms = rng.next_range_u64(250);
                    let timer_id = sched.set_timeout(delay_ms);
                    timers.push(timer_id);
                }
                1 => {
                    if !timers.is_empty() {
                        let idx = rng.next_usize(timers.len());
                        let _cancelled = sched.clear_timeout(timers[idx]);
                    }
                }
                2 => {
                    let call_id = format!("call-{step}-{}", rng.next_u64());
                    let outcome = HostcallOutcome::Success(serde_json::json!({ "step": step }));
                    sched.enqueue_hostcall_complete(call_id, outcome);
                }
                3 => {
                    let event_id = format!("evt-{step}");
                    let payload = serde_json::json!({ "step": step, "entropy": rng.next_u64() });
                    sched.enqueue_event(event_id, payload);
                }
                4 => {
                    let delta_ms = rng.next_range_u64(50);
                    sched.clock.advance(delta_ms);
                }
                _ => {}
            }

            if rng.next_range_u64(3) == 0 {
                if let Some(task) = sched.tick() {
                    trace.push(trace_entry(&task));
                }
            }
        }

        // Drain remaining tasks and timers deterministically.
        for _ in 0..10_000 {
            if let Some(task) = sched.tick() {
                trace.push(trace_entry(&task));
                continue;
            }

            let Some(next_deadline) = sched.next_timer_deadline() else {
                break;
            };

            let now = sched.now_ms();
            assert!(
                next_deadline > now,
                "expected future timer deadline (deadline={next_deadline}, now={now})"
            );
            sched.clock.set(next_deadline);
        }

        trace
    }

    #[test]
    fn scheduler_seeded_trace_is_deterministic() {
        for seed in [0_u64, 1, 2, 3, 0xDEAD_BEEF] {
            let a = run_seeded_script(seed);
            let b = run_seeded_script(seed);
            assert_eq!(a, b, "trace mismatch for seed={seed}");
        }
    }
}
