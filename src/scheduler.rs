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
use std::collections::BTreeMap;
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
        Self(self.0.saturating_add(1))
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
    /// Incremental stream chunk.
    StreamChunk {
        /// Monotonically increasing sequence number per call.
        sequence: u64,
        /// Arbitrary JSON payload for this chunk.
        chunk: serde_json::Value,
        /// `true` on the final chunk.
        is_final: bool,
    },
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
            .unwrap_or_default()
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
    /// All timer IDs currently in the heap (active or cancelled).
    heap_timer_ids: std::collections::HashSet<u64>,
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
            heap_timer_ids: std::collections::HashSet::new(),
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
        !self.macrotask_queue.is_empty()
            || self
                .timer_heap
                .iter()
                .any(|entry| !self.cancelled_timers.contains(&entry.timer_id))
    }

    /// Get the number of pending macrotasks.
    #[must_use]
    pub fn macrotask_count(&self) -> usize {
        self.macrotask_queue.len()
    }

    /// Get the number of pending timers.
    #[must_use]
    pub fn timer_count(&self) -> usize {
        self.timer_heap
            .iter()
            .filter(|entry| !self.cancelled_timers.contains(&entry.timer_id))
            .count()
    }

    /// Schedule a timer to fire at the given deadline.
    ///
    /// Returns the timer ID for cancellation.
    pub fn set_timeout(&mut self, delay_ms: u64) -> u64 {
        let timer_id = self.allocate_timer_id();
        let deadline_ms = self.clock.now_ms().saturating_add(delay_ms);
        let seq = self.next_seq();

        self.timer_heap
            .push(TimerEntry::new(timer_id, deadline_ms, seq));
        self.heap_timer_ids.insert(timer_id);

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

    fn timer_id_in_use(&self, timer_id: u64) -> bool {
        self.heap_timer_ids.contains(&timer_id)
    }

    fn allocate_timer_id(&mut self) -> u64 {
        let start = self.next_timer_id;
        let mut candidate = start;

        loop {
            // Calculate the next ID to try after this one
            self.next_timer_id = if candidate == u64::MAX {
                1
            } else {
                candidate + 1
            };

            if !self.timer_id_in_use(candidate) {
                return candidate;
            }

            candidate = self.next_timer_id;

            // If we've looped all the way around back to where we started, we're exhausted.
            if candidate == start {
                break;
            }
        }

        tracing::error!(
            event = "scheduler.timer_id.exhausted",
            "Timer ID namespace exhausted; falling back to u64::MAX reuse"
        );
        u64::MAX
    }

    /// Cancel a timer by ID.
    ///
    /// Returns true if the timer was found and cancelled.
    pub fn clear_timeout(&mut self, timer_id: u64) -> bool {
        let pending =
            self.heap_timer_ids.contains(&timer_id) && !self.cancelled_timers.contains(&timer_id);

        let cancelled = if pending {
            self.cancelled_timers.insert(timer_id)
        } else {
            false
        };

        tracing::trace!(
            event = "scheduler.timer.cancel",
            timer_id,
            cancelled,
            "Timer cancelled"
        );

        cancelled
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

    /// Enqueue multiple hostcall completions in one scheduler mutation pass.
    pub fn enqueue_hostcall_completions<I>(&mut self, completions: I)
    where
        I: IntoIterator<Item = (String, HostcallOutcome)>,
    {
        for (call_id, outcome) in completions {
            self.enqueue_hostcall_complete(call_id, outcome);
        }
    }

    /// Convenience: enqueue a stream chunk for a hostcall.
    pub fn enqueue_stream_chunk(
        &mut self,
        call_id: String,
        sequence: u64,
        chunk: serde_json::Value,
        is_final: bool,
    ) {
        self.enqueue_hostcall_complete(
            call_id,
            HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            },
        );
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
            self.heap_timer_ids.remove(&entry.timer_id);

            // Skip cancelled timers
            if self.cancelled_timers.remove(&entry.timer_id) {
                tracing::trace!(
                    event = "scheduler.timer.skip_cancelled",
                    timer_id = entry.timer_id,
                    "Skipped cancelled timer"
                );
                continue;
            }

            // Preserve (deadline, timer-seq) order while assigning a fresh
            // macrotask seq so queue ordering remains globally monotone.
            let task_seq = self.next_seq();
            let task = Macrotask::new(
                task_seq,
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
                timer_seq = %entry.seq,
                macrotask_seq = %task_seq,
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

// ============================================================================
// Core-pinned reactor mesh (URPC-style SPSC lanes)
// ============================================================================

/// Configuration for [`ReactorMesh`].
#[derive(Debug, Clone)]
pub struct ReactorMeshConfig {
    /// Number of core-pinned shards (one SPSC lane per shard).
    pub shard_count: usize,
    /// Maximum queued envelopes per shard lane.
    pub lane_capacity: usize,
    /// Optional topology snapshot for deterministic shard placement planning.
    pub topology: Option<ReactorTopologySnapshot>,
}

impl Default for ReactorMeshConfig {
    fn default() -> Self {
        Self {
            shard_count: 4,
            lane_capacity: 1024,
            topology: None,
        }
    }
}

/// Core descriptor used by topology-aware shard placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReactorTopologyCore {
    pub core_id: usize,
    pub numa_node: usize,
}

/// Lightweight machine-provided topology snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReactorTopologySnapshot {
    pub cores: Vec<ReactorTopologyCore>,
}

impl ReactorTopologySnapshot {
    /// Build a normalized topology snapshot from `(core_id, numa_node)` pairs.
    #[must_use]
    pub fn from_core_node_pairs(pairs: &[(usize, usize)]) -> Self {
        let mut cores = pairs
            .iter()
            .map(|(core_id, numa_node)| ReactorTopologyCore {
                core_id: *core_id,
                numa_node: *numa_node,
            })
            .collect::<Vec<_>>();
        cores.sort_unstable();
        cores.dedup();
        Self { cores }
    }
}

/// Explicit fallback reason emitted by topology planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactorPlacementFallbackReason {
    /// No topology snapshot was available at planning time.
    TopologyUnavailable,
    /// A topology snapshot was provided but had no usable cores.
    TopologyEmpty,
    /// Topology is available but only one NUMA node exists.
    SingleNumaNode,
}

impl ReactorPlacementFallbackReason {
    #[must_use]
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::TopologyUnavailable => "topology_unavailable",
            Self::TopologyEmpty => "topology_empty",
            Self::SingleNumaNode => "single_numa_node",
        }
    }
}

/// Deterministic shard binding produced by placement planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReactorShardBinding {
    pub shard_id: usize,
    pub core_id: usize,
    pub numa_node: usize,
}

/// Machine-readable shard placement manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactorPlacementManifest {
    pub shard_count: usize,
    pub numa_node_count: usize,
    pub bindings: Vec<ReactorShardBinding>,
    pub fallback_reason: Option<ReactorPlacementFallbackReason>,
}

impl ReactorPlacementManifest {
    /// Plan deterministic shard placement from optional topology.
    #[must_use]
    pub fn plan(shard_count: usize, topology: Option<&ReactorTopologySnapshot>) -> Self {
        if shard_count == 0 {
            return Self {
                shard_count: 0,
                numa_node_count: 0,
                bindings: Vec::new(),
                fallback_reason: None,
            };
        }

        let Some(topology) = topology else {
            let bindings = (0..shard_count)
                .map(|shard_id| ReactorShardBinding {
                    shard_id,
                    core_id: shard_id,
                    numa_node: 0,
                })
                .collect::<Vec<_>>();
            return Self {
                shard_count,
                numa_node_count: 1,
                bindings,
                fallback_reason: Some(ReactorPlacementFallbackReason::TopologyUnavailable),
            };
        };

        if topology.cores.is_empty() {
            let bindings = (0..shard_count)
                .map(|shard_id| ReactorShardBinding {
                    shard_id,
                    core_id: shard_id,
                    numa_node: 0,
                })
                .collect::<Vec<_>>();
            return Self {
                shard_count,
                numa_node_count: 1,
                bindings,
                fallback_reason: Some(ReactorPlacementFallbackReason::TopologyEmpty),
            };
        }

        let mut by_node = BTreeMap::<usize, Vec<usize>>::new();
        for core in &topology.cores {
            by_node
                .entry(core.numa_node)
                .or_default()
                .push(core.core_id);
        }
        for cores in by_node.values_mut() {
            cores.sort_unstable();
            cores.dedup();
        }
        let nodes = by_node
            .into_iter()
            .filter(|(_, cores)| !cores.is_empty())
            .collect::<Vec<_>>();

        if nodes.is_empty() {
            let bindings = (0..shard_count)
                .map(|shard_id| ReactorShardBinding {
                    shard_id,
                    core_id: shard_id,
                    numa_node: 0,
                })
                .collect::<Vec<_>>();
            return Self {
                shard_count,
                numa_node_count: 1,
                bindings,
                fallback_reason: Some(ReactorPlacementFallbackReason::TopologyEmpty),
            };
        }

        let node_count = nodes.len();
        let fallback_reason = if node_count == 1 {
            Some(ReactorPlacementFallbackReason::SingleNumaNode)
        } else {
            None
        };

        let mut bindings = Vec::with_capacity(shard_count);
        for shard_id in 0..shard_count {
            let node_idx = shard_id % node_count;
            let (numa_node, cores) = &nodes[node_idx];
            let core_idx = (shard_id / node_count) % cores.len();
            bindings.push(ReactorShardBinding {
                shard_id,
                core_id: cores[core_idx],
                numa_node: *numa_node,
            });
        }

        Self {
            shard_count,
            numa_node_count: node_count,
            bindings,
            fallback_reason,
        }
    }

    /// Render placement manifest as stable machine-readable JSON.
    #[must_use]
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "shard_count": self.shard_count,
            "numa_node_count": self.numa_node_count,
            "fallback_reason": self.fallback_reason.map(ReactorPlacementFallbackReason::as_code),
            "bindings": self.bindings.iter().map(|binding| {
                serde_json::json!({
                    "shard_id": binding.shard_id,
                    "core_id": binding.core_id,
                    "numa_node": binding.numa_node
                })
            }).collect::<Vec<_>>()
        })
    }
}

/// Per-envelope metadata produced by [`ReactorMesh`].
#[derive(Debug, Clone)]
pub struct ReactorEnvelope {
    /// Global sequence for deterministic cross-shard ordering.
    pub global_seq: Seq,
    /// Monotone sequence scoped to the destination shard.
    pub shard_seq: u64,
    /// Destination shard that owns this envelope.
    pub shard_id: usize,
    /// Payload to execute on the shard.
    pub task: MacrotaskKind,
}

impl ReactorEnvelope {
    const fn new(global_seq: Seq, shard_seq: u64, shard_id: usize, task: MacrotaskKind) -> Self {
        Self {
            global_seq,
            shard_seq,
            shard_id,
            task,
        }
    }
}

/// Backpressure signal for rejected mesh enqueue operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReactorBackpressure {
    pub shard_id: usize,
    pub depth: usize,
    pub capacity: usize,
}

/// Lightweight telemetry snapshot for mesh queueing behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactorMeshTelemetry {
    pub queue_depths: Vec<usize>,
    pub max_queue_depths: Vec<usize>,
    pub rejected_enqueues: u64,
    pub shard_bindings: Vec<ReactorShardBinding>,
    pub fallback_reason: Option<ReactorPlacementFallbackReason>,
}

impl ReactorMeshTelemetry {
    /// Render telemetry as machine-readable JSON for diagnostics.
    #[must_use]
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "queue_depths": self.queue_depths,
            "max_queue_depths": self.max_queue_depths,
            "rejected_enqueues": self.rejected_enqueues,
            "fallback_reason": self.fallback_reason.map(ReactorPlacementFallbackReason::as_code),
            "shard_bindings": self.shard_bindings.iter().map(|binding| {
                serde_json::json!({
                    "shard_id": binding.shard_id,
                    "core_id": binding.core_id,
                    "numa_node": binding.numa_node,
                })
            }).collect::<Vec<_>>()
        })
    }
}

/// Deterministic SPSC-style lane.
///
/// This models the semantics of a bounded SPSC ring without unsafe code.
#[derive(Debug, Clone)]
struct SpscLane<T> {
    capacity: usize,
    queue: VecDeque<T>,
    max_depth: usize,
}

impl<T> SpscLane<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: VecDeque::with_capacity(capacity),
            max_depth: 0,
        }
    }

    fn len(&self) -> usize {
        self.queue.len()
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    fn push(&mut self, value: T) -> Result<(), usize> {
        if self.queue.len() >= self.capacity {
            return Err(self.queue.len());
        }
        self.queue.push_back(value);
        self.max_depth = self.max_depth.max(self.queue.len());
        Ok(())
    }

    fn pop(&mut self) -> Option<T> {
        self.queue.pop_front()
    }

    fn front(&self) -> Option<&T> {
        self.queue.front()
    }
}

/// Deterministic multi-shard reactor mesh using bounded per-shard SPSC lanes.
///
/// Routing policy:
/// - Hostcall completions are hash-routed by `call_id` for shard affinity.
/// - Inbound events use deterministic round-robin distribution across shards.
///
/// Drain policy:
/// - `drain_global_order()` emits envelopes in ascending global sequence
///   across all shard heads, preserving deterministic external ordering.
#[derive(Debug, Clone)]
pub struct ReactorMesh {
    seq: Seq,
    lanes: Vec<SpscLane<ReactorEnvelope>>,
    shard_seq: Vec<u64>,
    rr_cursor: usize,
    rejected_enqueues: u64,
    placement_manifest: ReactorPlacementManifest,
}

impl ReactorMesh {
    /// Create a mesh using the provided config.
    ///
    /// The mesh is fail-closed for invalid config values:
    /// `shard_count == 0` or `lane_capacity == 0` returns an empty mesh.
    #[must_use]
    #[allow(clippy::needless_pass_by_value)]
    pub fn new(config: ReactorMeshConfig) -> Self {
        if config.shard_count == 0 || config.lane_capacity == 0 {
            return Self {
                seq: Seq::zero(),
                lanes: Vec::new(),
                shard_seq: Vec::new(),
                rr_cursor: 0,
                rejected_enqueues: 0,
                placement_manifest: ReactorPlacementManifest::plan(0, config.topology.as_ref()),
            };
        }

        let shard_count = config.shard_count;
        let lane_capacity = config.lane_capacity;
        let placement_manifest =
            ReactorPlacementManifest::plan(shard_count, config.topology.as_ref());
        let lanes = (0..shard_count)
            .map(|_| SpscLane::new(lane_capacity))
            .collect::<Vec<_>>();
        Self {
            seq: Seq::zero(),
            lanes,
            shard_seq: vec![0; shard_count],
            rr_cursor: 0,
            rejected_enqueues: 0,
            placement_manifest,
        }
    }

    /// Number of shard lanes.
    #[must_use]
    pub const fn shard_count(&self) -> usize {
        self.lanes.len()
    }

    /// Total pending envelopes across all shards.
    #[must_use]
    pub fn total_depth(&self) -> usize {
        self.lanes.iter().map(SpscLane::len).sum()
    }

    /// Whether any lane has pending envelopes.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.total_depth() > 0
    }

    /// Per-shard queue depth.
    #[must_use]
    pub fn queue_depth(&self, shard_id: usize) -> Option<usize> {
        self.lanes.get(shard_id).map(SpscLane::len)
    }

    /// Snapshot queueing telemetry for diagnostics.
    #[must_use]
    pub fn telemetry(&self) -> ReactorMeshTelemetry {
        ReactorMeshTelemetry {
            queue_depths: self.lanes.iter().map(SpscLane::len).collect(),
            max_queue_depths: self.lanes.iter().map(|lane| lane.max_depth).collect(),
            rejected_enqueues: self.rejected_enqueues,
            shard_bindings: self.placement_manifest.bindings.clone(),
            fallback_reason: self.placement_manifest.fallback_reason,
        }
    }

    /// Deterministic shard placement manifest used by this mesh.
    #[must_use]
    pub const fn placement_manifest(&self) -> &ReactorPlacementManifest {
        &self.placement_manifest
    }

    const fn next_global_seq(&mut self) -> Seq {
        let current = self.seq;
        self.seq = self.seq.next();
        current
    }

    fn next_shard_seq(&mut self, shard_id: usize) -> u64 {
        let Some(seq) = self.shard_seq.get_mut(shard_id) else {
            return 0;
        };
        let current = *seq;
        *seq = seq.saturating_add(1);
        current
    }

    fn stable_hash(input: &str) -> u64 {
        // FNV-1a 64-bit for deterministic process-independent routing.
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in input.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3_u64);
        }
        hash
    }

    fn hash_route(&self, call_id: &str) -> usize {
        if self.lanes.len() <= 1 {
            return 0;
        }
        let lanes = u64::try_from(self.lanes.len()).unwrap_or(1);
        let slot = Self::stable_hash(call_id) % lanes;
        usize::try_from(slot).unwrap_or(0)
    }

    const fn rr_route(&mut self) -> usize {
        if self.lanes.len() <= 1 {
            return 0;
        }
        let idx = self.rr_cursor % self.lanes.len();
        self.rr_cursor = self.rr_cursor.wrapping_add(1);
        idx
    }

    fn enqueue_with_route(
        &mut self,
        shard_id: usize,
        task: MacrotaskKind,
    ) -> Result<ReactorEnvelope, ReactorBackpressure> {
        let global_seq = self.next_global_seq();
        let shard_seq = self.next_shard_seq(shard_id);
        let envelope = ReactorEnvelope::new(global_seq, shard_seq, shard_id, task);
        let Some(lane) = self.lanes.get_mut(shard_id) else {
            self.rejected_enqueues = self.rejected_enqueues.saturating_add(1);
            return Err(ReactorBackpressure {
                shard_id,
                depth: 0,
                capacity: 0,
            });
        };
        match lane.push(envelope.clone()) {
            Ok(()) => Ok(envelope),
            Err(depth) => {
                self.rejected_enqueues = self.rejected_enqueues.saturating_add(1);
                Err(ReactorBackpressure {
                    shard_id,
                    depth,
                    capacity: lane.capacity,
                })
            }
        }
    }

    /// Enqueue a hostcall completion using deterministic hash routing.
    pub fn enqueue_hostcall_complete(
        &mut self,
        call_id: String,
        outcome: HostcallOutcome,
    ) -> Result<ReactorEnvelope, ReactorBackpressure> {
        let shard_id = self.hash_route(&call_id);
        self.enqueue_with_route(
            shard_id,
            MacrotaskKind::HostcallComplete { call_id, outcome },
        )
    }

    /// Enqueue an inbound event using deterministic round-robin routing.
    pub fn enqueue_event(
        &mut self,
        event_id: String,
        payload: serde_json::Value,
    ) -> Result<ReactorEnvelope, ReactorBackpressure> {
        let shard_id = self.rr_route();
        self.enqueue_with_route(shard_id, MacrotaskKind::InboundEvent { event_id, payload })
    }

    /// Drain one shard up to `budget` envelopes.
    pub fn drain_shard(&mut self, shard_id: usize, budget: usize) -> Vec<ReactorEnvelope> {
        let Some(lane) = self.lanes.get_mut(shard_id) else {
            return Vec::new();
        };
        let mut drained = Vec::with_capacity(budget.min(lane.len()));
        for _ in 0..budget {
            let Some(item) = lane.pop() else {
                break;
            };
            drained.push(item);
        }
        drained
    }

    /// Drain across shards in deterministic global sequence order.
    pub fn drain_global_order(&mut self, budget: usize) -> Vec<ReactorEnvelope> {
        let mut drained = Vec::with_capacity(budget);
        for _ in 0..budget {
            let mut best_lane: Option<usize> = None;
            let mut best_seq: Option<Seq> = None;
            for (idx, lane) in self.lanes.iter().enumerate() {
                let Some(front) = lane.front() else {
                    continue;
                };
                if best_seq.is_none_or(|seq| front.global_seq < seq) {
                    best_seq = Some(front.global_seq);
                    best_lane = Some(idx);
                }
            }
            let Some(chosen_lane) = best_lane else {
                break;
            };
            if let Some(item) = self.lanes[chosen_lane].pop() {
                drained.push(item);
            }
        }
        drained
    }
}

// ============================================================================
// NUMA-aware slab allocator for extension hot paths
// ============================================================================

/// Configuration for hugepage-backed slab allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HugepageConfig {
    /// Hugepage size in bytes (default 2 MiB = `2_097_152`).
    pub page_size_bytes: usize,
    /// Whether hugepage backing is requested (advisory — falls back gracefully).
    pub enabled: bool,
}

impl Default for HugepageConfig {
    fn default() -> Self {
        Self {
            page_size_bytes: 2 * 1024 * 1024, // 2 MiB
            enabled: true,
        }
    }
}

/// Reason why hugepage-backed allocation was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HugepageFallbackReason {
    /// Hugepage support was explicitly disabled in configuration.
    Disabled,
    /// No hugepage information available from the OS.
    DetectionUnavailable,
    /// System reports zero free hugepages.
    InsufficientHugepages,
    /// Requested slab size does not align to hugepage boundaries.
    AlignmentMismatch,
}

impl HugepageFallbackReason {
    #[must_use]
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Disabled => "hugepage_disabled",
            Self::DetectionUnavailable => "detection_unavailable",
            Self::InsufficientHugepages => "insufficient_hugepages",
            Self::AlignmentMismatch => "alignment_mismatch",
        }
    }
}

/// Hugepage availability snapshot from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HugepageStatus {
    /// Total hugepages configured on the system.
    pub total_pages: u64,
    /// Free hugepages available for allocation.
    pub free_pages: u64,
    /// Page size in bytes (typically 2 MiB).
    pub page_size_bytes: usize,
    /// Whether hugepages are actually being used by this pool.
    pub active: bool,
    /// If not active, why.
    pub fallback_reason: Option<HugepageFallbackReason>,
}

impl HugepageStatus {
    /// Determine hugepage viability from system metrics.
    #[must_use]
    pub const fn evaluate(config: &HugepageConfig, total: u64, free: u64) -> Self {
        if !config.enabled {
            return Self {
                total_pages: total,
                free_pages: free,
                page_size_bytes: config.page_size_bytes,
                active: false,
                fallback_reason: Some(HugepageFallbackReason::Disabled),
            };
        }
        if total == 0 && free == 0 {
            return Self {
                total_pages: 0,
                free_pages: 0,
                page_size_bytes: config.page_size_bytes,
                active: false,
                fallback_reason: Some(HugepageFallbackReason::DetectionUnavailable),
            };
        }
        if free == 0 {
            return Self {
                total_pages: total,
                free_pages: 0,
                page_size_bytes: config.page_size_bytes,
                active: false,
                fallback_reason: Some(HugepageFallbackReason::InsufficientHugepages),
            };
        }
        Self {
            total_pages: total,
            free_pages: free,
            page_size_bytes: config.page_size_bytes,
            active: true,
            fallback_reason: None,
        }
    }

    /// Render as stable JSON for diagnostics.
    #[must_use]
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "total_pages": self.total_pages,
            "free_pages": self.free_pages,
            "page_size_bytes": self.page_size_bytes,
            "active": self.active,
            "fallback_reason": self.fallback_reason.map(HugepageFallbackReason::as_code),
        })
    }
}

/// Configuration for a NUMA-local slab pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumaSlabConfig {
    /// Maximum entries per NUMA-node slab.
    pub slab_capacity: usize,
    /// Logical size of each entry in bytes (for telemetry, not enforced at runtime).
    pub entry_size_bytes: usize,
    /// Hugepage configuration.
    pub hugepage: HugepageConfig,
}

impl Default for NumaSlabConfig {
    fn default() -> Self {
        Self {
            slab_capacity: 256,
            entry_size_bytes: 512,
            hugepage: HugepageConfig::default(),
        }
    }
}

impl NumaSlabConfig {
    #[must_use]
    pub const fn slab_footprint_bytes(&self) -> Option<usize> {
        self.slab_capacity.checked_mul(self.entry_size_bytes)
    }

    #[must_use]
    pub const fn hugepage_alignment_ok(&self) -> bool {
        if !self.hugepage.enabled {
            return true;
        }
        let page = self.hugepage.page_size_bytes;
        if page == 0 {
            return false;
        }
        match self.slab_footprint_bytes() {
            Some(bytes) => bytes != 0 && bytes % page == 0,
            None => false,
        }
    }

    #[must_use]
    pub const fn alignment_mismatch_status(&self) -> HugepageStatus {
        HugepageStatus {
            total_pages: 0,
            free_pages: 0,
            page_size_bytes: self.hugepage.page_size_bytes,
            active: false,
            fallback_reason: Some(HugepageFallbackReason::AlignmentMismatch),
        }
    }
}

/// Handle returned on successful slab allocation.
///
/// Contains enough information to deallocate safely and detect use-after-free
/// via generation tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumaSlabHandle {
    /// NUMA node that owns this allocation.
    pub node_id: usize,
    /// Slot index within the node's slab.
    pub slot_index: usize,
    /// Generation at allocation time (prevents ABA on reuse).
    pub generation: u64,
}

/// Per-NUMA-node slab with free-list recycling.
#[derive(Debug, Clone)]
pub struct NumaSlab {
    node_id: usize,
    capacity: usize,
    generations: Vec<u64>,
    allocated: Vec<bool>,
    free_list: Vec<usize>,
    // Telemetry counters.
    total_allocs: u64,
    total_frees: u64,
    high_water_mark: usize,
}

impl NumaSlab {
    /// Create a new slab for the given NUMA node with the specified capacity.
    #[must_use]
    pub fn new(node_id: usize, capacity: usize) -> Self {
        let capacity = capacity.max(1);
        let mut free_list = Vec::with_capacity(capacity);
        // Fill free list in reverse so slot 0 is popped first (LIFO recycle).
        for idx in (0..capacity).rev() {
            free_list.push(idx);
        }
        Self {
            node_id,
            capacity,
            generations: vec![0; capacity],
            allocated: vec![false; capacity],
            free_list,
            total_allocs: 0,
            total_frees: 0,
            high_water_mark: 0,
        }
    }

    /// Number of currently allocated slots.
    #[must_use]
    pub const fn in_use(&self) -> usize {
        self.capacity.saturating_sub(self.free_list.len())
    }

    /// Whether the slab has available capacity.
    #[must_use]
    pub const fn has_capacity(&self) -> bool {
        !self.free_list.is_empty()
    }

    /// Allocate a slot, returning a handle or `None` if exhausted.
    pub fn allocate(&mut self) -> Option<NumaSlabHandle> {
        let slot_index = self.free_list.pop()?;
        self.allocated[slot_index] = true;
        self.generations[slot_index] = self.generations[slot_index].saturating_add(1);
        self.total_allocs = self.total_allocs.saturating_add(1);
        self.high_water_mark = self.high_water_mark.max(self.in_use());
        Some(NumaSlabHandle {
            node_id: self.node_id,
            slot_index,
            generation: self.generations[slot_index],
        })
    }

    /// Deallocate a slot identified by handle.
    ///
    /// Returns `true` if deallocation succeeded, `false` if the handle is stale
    /// (wrong generation) or already freed.
    pub fn deallocate(&mut self, handle: &NumaSlabHandle) -> bool {
        if handle.node_id != self.node_id {
            return false;
        }
        if handle.slot_index >= self.capacity {
            return false;
        }
        if !self.allocated[handle.slot_index] {
            return false;
        }
        if self.generations[handle.slot_index] != handle.generation {
            return false;
        }
        self.allocated[handle.slot_index] = false;
        self.free_list.push(handle.slot_index);
        self.total_frees = self.total_frees.saturating_add(1);
        true
    }
}

/// Cross-node allocation reason for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossNodeReason {
    /// Local node slab is exhausted; allocated from nearest neighbor.
    LocalExhausted,
}

impl CrossNodeReason {
    #[must_use]
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::LocalExhausted => "local_exhausted",
        }
    }
}

/// Multi-node slab pool that routes allocations to NUMA-local slabs.
#[derive(Debug, Clone)]
pub struct NumaSlabPool {
    slabs: Vec<NumaSlab>,
    config: NumaSlabConfig,
    hugepage_status: HugepageStatus,
    cross_node_allocs: u64,
    hugepage_backed_allocs: u64,
}

impl NumaSlabPool {
    /// Create a pool with one slab per NUMA node identified in the placement manifest.
    #[must_use]
    pub fn from_manifest(manifest: &ReactorPlacementManifest, config: NumaSlabConfig) -> Self {
        let mut node_ids = manifest
            .bindings
            .iter()
            .map(|b| b.numa_node)
            .collect::<Vec<_>>();
        node_ids.sort_unstable();
        node_ids.dedup();

        if node_ids.is_empty() {
            node_ids.push(0);
        }

        let slabs = node_ids
            .iter()
            .map(|&node_id| NumaSlab::new(node_id, config.slab_capacity))
            .collect();

        // Hugepage evaluation with zero system data (caller can override via
        // `set_hugepage_status` once real data is available).
        let hugepage_status = if config.hugepage.enabled && !config.hugepage_alignment_ok() {
            config.alignment_mismatch_status()
        } else {
            HugepageStatus::evaluate(&config.hugepage, 0, 0)
        };

        Self {
            slabs,
            config,
            hugepage_status,
            cross_node_allocs: 0,
            hugepage_backed_allocs: 0,
        }
    }

    /// Override hugepage status after querying the host.
    pub const fn set_hugepage_status(&mut self, status: HugepageStatus) {
        self.hugepage_status = if !self.config.hugepage.enabled {
            HugepageStatus::evaluate(&self.config.hugepage, status.total_pages, status.free_pages)
        } else if !self.config.hugepage_alignment_ok() {
            self.config.alignment_mismatch_status()
        } else {
            status
        };
    }

    /// Number of NUMA nodes in this pool.
    #[must_use]
    pub const fn node_count(&self) -> usize {
        self.slabs.len()
    }

    /// Allocate from the preferred NUMA node, falling back to any node with capacity.
    ///
    /// Returns `(handle, cross_node_reason)` where the reason is `Some` when
    /// the allocation was served from a non-preferred node.
    pub fn allocate(
        &mut self,
        preferred_node: usize,
    ) -> Option<(NumaSlabHandle, Option<CrossNodeReason>)> {
        // Try preferred node first.
        if let Some(slab) = self.slabs.iter_mut().find(|s| s.node_id == preferred_node)
            && let Some(handle) = slab.allocate()
        {
            if self.hugepage_status.active {
                self.hugepage_backed_allocs = self.hugepage_backed_allocs.saturating_add(1);
            }
            return Some((handle, None));
        }
        // Fallback: scan all nodes for available capacity.
        for slab in &mut self.slabs {
            if slab.node_id == preferred_node {
                continue;
            }
            if let Some(handle) = slab.allocate() {
                self.cross_node_allocs = self.cross_node_allocs.saturating_add(1);
                if self.hugepage_status.active {
                    self.hugepage_backed_allocs = self.hugepage_backed_allocs.saturating_add(1);
                }
                return Some((handle, Some(CrossNodeReason::LocalExhausted)));
            }
        }
        None
    }

    /// Deallocate a previously allocated handle.
    ///
    /// Returns `true` if deallocation succeeded.
    pub fn deallocate(&mut self, handle: &NumaSlabHandle) -> bool {
        for slab in &mut self.slabs {
            if slab.node_id == handle.node_id {
                return slab.deallocate(handle);
            }
        }
        false
    }

    /// Snapshot telemetry for this pool.
    #[must_use]
    pub fn telemetry(&self) -> NumaSlabTelemetry {
        let per_node = self
            .slabs
            .iter()
            .map(|slab| NumaSlabNodeTelemetry {
                node_id: slab.node_id,
                capacity: slab.capacity,
                in_use: slab.in_use(),
                total_allocs: slab.total_allocs,
                total_frees: slab.total_frees,
                high_water_mark: slab.high_water_mark,
            })
            .collect();
        NumaSlabTelemetry {
            per_node,
            cross_node_allocs: self.cross_node_allocs,
            hugepage_backed_allocs: self.hugepage_backed_allocs,
            hugepage_status: self.hugepage_status,
            config: self.config,
        }
    }
}

/// Per-node telemetry counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumaSlabNodeTelemetry {
    pub node_id: usize,
    pub capacity: usize,
    pub in_use: usize,
    pub total_allocs: u64,
    pub total_frees: u64,
    pub high_water_mark: usize,
}

/// Aggregate slab pool telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumaSlabTelemetry {
    pub per_node: Vec<NumaSlabNodeTelemetry>,
    pub cross_node_allocs: u64,
    pub hugepage_backed_allocs: u64,
    pub hugepage_status: HugepageStatus,
    pub config: NumaSlabConfig,
}

impl NumaSlabTelemetry {
    const RATIO_SCALE_BPS: u64 = 10_000;

    #[must_use]
    fn ratio_basis_points(numerator: u64, denominator: u64) -> u64 {
        if denominator == 0 {
            return 0;
        }
        let scaled =
            (u128::from(numerator) * u128::from(Self::RATIO_SCALE_BPS)) / u128::from(denominator);
        u64::try_from(scaled).unwrap_or(Self::RATIO_SCALE_BPS)
    }

    #[must_use]
    const fn pressure_band(value_bps: u64) -> &'static str {
        if value_bps >= 7_500 {
            "high"
        } else if value_bps >= 2_500 {
            "medium"
        } else {
            "low"
        }
    }

    /// Render as stable machine-readable JSON for diagnostics.
    #[must_use]
    pub fn as_json(&self) -> serde_json::Value {
        let total_allocs: u64 = self.per_node.iter().map(|n| n.total_allocs).sum();
        let total_frees: u64 = self.per_node.iter().map(|n| n.total_frees).sum();
        let total_in_use: usize = self.per_node.iter().map(|n| n.in_use).sum();
        let total_capacity: usize = self.per_node.iter().map(|n| n.capacity).sum();
        let total_high_water: usize = self.per_node.iter().map(|n| n.high_water_mark).sum();
        let remote_allocs = self.cross_node_allocs.min(total_allocs);
        let local_allocs = total_allocs.saturating_sub(remote_allocs);
        let local_ratio_bps = Self::ratio_basis_points(local_allocs, total_allocs);
        let remote_ratio_bps = Self::ratio_basis_points(remote_allocs, total_allocs);
        let hugepage_backed_allocs = self.hugepage_backed_allocs.min(total_allocs);
        let hugepage_hit_rate_bps = Self::ratio_basis_points(hugepage_backed_allocs, total_allocs);
        let total_capacity_u64 = u64::try_from(total_capacity).unwrap_or(u64::MAX);
        let total_in_use_u64 = u64::try_from(total_in_use).unwrap_or(u64::MAX);
        let total_high_water_u64 = u64::try_from(total_high_water).unwrap_or(u64::MAX);
        let occupancy_pressure_bps = Self::ratio_basis_points(total_in_use_u64, total_capacity_u64);
        let cache_miss_pressure_bps =
            Self::ratio_basis_points(total_high_water_u64, total_capacity_u64);
        // Remote allocations are a practical proxy for TLB/cache pressure from cross-node traffic.
        let tlb_miss_pressure_bps = remote_ratio_bps;
        let cross_node_fallback_reason = if self.cross_node_allocs > 0 {
            Some(CrossNodeReason::LocalExhausted.as_code())
        } else {
            None
        };
        serde_json::json!({
            "node_count": self.per_node.len(),
            "total_allocs": total_allocs,
            "total_frees": total_frees,
            "total_in_use": total_in_use,
            "cross_node_allocs": self.cross_node_allocs,
            "hugepage_backed_allocs": hugepage_backed_allocs,
            "local_allocs": local_allocs,
            "remote_allocs": remote_allocs,
            "allocation_ratio_bps": {
                "scale": Self::RATIO_SCALE_BPS,
                "local": local_ratio_bps,
                "remote": remote_ratio_bps,
            },
            "hugepage_hit_rate_bps": {
                "scale": Self::RATIO_SCALE_BPS,
                "value": hugepage_hit_rate_bps,
            },
            "latency_proxies_bps": {
                "scale": Self::RATIO_SCALE_BPS,
                "tlb_miss_pressure": tlb_miss_pressure_bps,
                "cache_miss_pressure": cache_miss_pressure_bps,
                "occupancy_pressure": occupancy_pressure_bps,
            },
            "pressure_bands": {
                "tlb_miss": Self::pressure_band(tlb_miss_pressure_bps),
                "cache_miss": Self::pressure_band(cache_miss_pressure_bps),
                "occupancy": Self::pressure_band(occupancy_pressure_bps),
            },
            "fallback_reasons": {
                "cross_node": cross_node_fallback_reason,
                "hugepage": self.hugepage_status.fallback_reason.map(HugepageFallbackReason::as_code),
            },
            "config": {
                "slab_capacity": self.config.slab_capacity,
                "entry_size_bytes": self.config.entry_size_bytes,
                "hugepage_enabled": self.config.hugepage.enabled,
                "hugepage_page_size_bytes": self.config.hugepage.page_size_bytes,
            },
            "hugepage": self.hugepage_status.as_json(),
            "per_node": self.per_node.iter().map(|n| serde_json::json!({
                "node_id": n.node_id,
                "capacity": n.capacity,
                "in_use": n.in_use,
                "total_allocs": n.total_allocs,
                "total_frees": n.total_frees,
                "high_water_mark": n.high_water_mark,
            })).collect::<Vec<_>>(),
        })
    }
}

// ============================================================================
// Thread affinity advisory
// ============================================================================

/// Enforcement level for thread-to-core affinity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AffinityEnforcement {
    /// Affinity is advisory only; scheduler may override.
    Advisory,
    /// Strict enforcement requested (requires OS support).
    Strict,
    /// Affinity enforcement is disabled.
    Disabled,
}

impl AffinityEnforcement {
    #[must_use]
    pub const fn as_code(self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Strict => "strict",
            Self::Disabled => "disabled",
        }
    }
}

/// Advisory thread-to-core binding produced from placement manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadAffinityAdvice {
    pub shard_id: usize,
    pub recommended_core: usize,
    pub recommended_numa_node: usize,
    pub enforcement: AffinityEnforcement,
}

impl ThreadAffinityAdvice {
    /// Render as JSON for diagnostics.
    #[must_use]
    pub fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "shard_id": self.shard_id,
            "recommended_core": self.recommended_core,
            "recommended_numa_node": self.recommended_numa_node,
            "enforcement": self.enforcement.as_code(),
        })
    }
}

impl ReactorPlacementManifest {
    /// Generate affinity advice for all shards in this manifest.
    #[must_use]
    pub fn affinity_advice(&self, enforcement: AffinityEnforcement) -> Vec<ThreadAffinityAdvice> {
        self.bindings
            .iter()
            .map(|binding| ThreadAffinityAdvice {
                shard_id: binding.shard_id,
                recommended_core: binding.core_id,
                recommended_numa_node: binding.numa_node,
                enforcement,
            })
            .collect()
    }

    /// Look up the NUMA node for a specific shard.
    #[must_use]
    pub fn numa_node_for_shard(&self, shard_id: usize) -> Option<usize> {
        self.bindings
            .iter()
            .find(|b| b.shard_id == shard_id)
            .map(|b| b.numa_node)
    }
}

// ============================================================================
// ReactorMesh ↔ NUMA slab integration
// ============================================================================

impl ReactorMesh {
    /// Look up the preferred NUMA node for a shard via the placement manifest.
    #[must_use]
    pub fn preferred_numa_node(&self, shard_id: usize) -> usize {
        self.placement_manifest
            .numa_node_for_shard(shard_id)
            .unwrap_or(0)
    }

    /// Generate thread affinity advice from the mesh's placement manifest.
    #[must_use]
    pub fn affinity_advice(&self, enforcement: AffinityEnforcement) -> Vec<ThreadAffinityAdvice> {
        self.placement_manifest.affinity_advice(enforcement)
    }
}

impl<C: Clock> fmt::Debug for Scheduler<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler")
            .field("seq", &self.seq)
            .field("macrotask_count", &self.macrotask_queue.len())
            .field("timer_count", &self.timer_count())
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
    fn seq_next_saturates_at_u64_max() {
        let max = Seq(u64::MAX);
        assert_eq!(max.next(), max);
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
            other => unreachable!("Expected TimerFired, got {other:?}"),
        }
    }

    #[test]
    fn scheduler_timer_id_wraps_after_u64_max() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);
        sched.next_timer_id = u64::MAX;

        let first = sched.set_timeout(10);
        let second = sched.set_timeout(20);

        assert_eq!(first, u64::MAX);
        assert_eq!(second, 1);
    }

    #[test]
    fn scheduler_timer_id_wrap_preserves_cancellation_semantics() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);
        sched.next_timer_id = u64::MAX;

        let max_id = sched.set_timeout(10);
        let wrapped_id = sched.set_timeout(20);

        assert_eq!(max_id, u64::MAX);
        assert_eq!(wrapped_id, 1);
        assert!(sched.clear_timeout(max_id));
        assert!(sched.clear_timeout(wrapped_id));

        sched.clock.advance(25);
        assert!(sched.tick().is_none());
        assert!(sched.tick().is_none());
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
            other => unreachable!("Expected t1, got {other:?}"),
        }
        match task2.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t2),
            other => unreachable!("Expected t2, got {other:?}"),
        }
        match task3.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t3),
            other => unreachable!("Expected t3, got {other:?}"),
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
            other => unreachable!("Expected t1, got {other:?}"),
        }
        match task2.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t2),
            other => unreachable!("Expected t2, got {other:?}"),
        }
        match task3.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, t3),
            other => unreachable!("Expected t3, got {other:?}"),
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
            other => unreachable!("Expected t2, got {other:?}"),
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
                    other => unreachable!("Expected success, got {other:?}"),
                }
            }
            other => unreachable!("Expected HostcallComplete, got {other:?}"),
        }
    }

    #[test]
    fn scheduler_stream_chunk_sequence_and_finality_invariants() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        sched.enqueue_stream_chunk(
            "call-stream".to_string(),
            0,
            serde_json::json!({ "part": "a" }),
            false,
        );
        sched.enqueue_stream_chunk(
            "call-stream".to_string(),
            1,
            serde_json::json!({ "part": "b" }),
            false,
        );
        sched.enqueue_stream_chunk(
            "call-stream".to_string(),
            2,
            serde_json::json!({ "part": "c" }),
            true,
        );

        let mut seen = Vec::new();
        while let Some(task) = sched.tick() {
            let MacrotaskKind::HostcallComplete { call_id, outcome } = task.kind else {
                unreachable!("expected hostcall completion task");
            };
            let HostcallOutcome::StreamChunk {
                sequence,
                chunk,
                is_final,
            } = outcome
            else {
                unreachable!("expected stream chunk outcome");
            };
            seen.push((call_id, sequence, chunk, is_final));
        }

        assert_eq!(seen.len(), 3);
        assert!(
            seen.iter()
                .all(|(call_id, _, _, _)| call_id == "call-stream")
        );
        assert_eq!(seen[0].1, 0);
        assert_eq!(seen[1].1, 1);
        assert_eq!(seen[2].1, 2);
        assert_eq!(seen[0].2, serde_json::json!({ "part": "a" }));
        assert_eq!(seen[1].2, serde_json::json!({ "part": "b" }));
        assert_eq!(seen[2].2, serde_json::json!({ "part": "c" }));

        let final_count = seen.iter().filter(|(_, _, _, is_final)| *is_final).count();
        assert_eq!(final_count, 1, "expected exactly one final chunk");
        assert!(seen[2].3, "final chunk must be last");
    }

    #[test]
    fn scheduler_stream_chunks_multi_call_interleaving_is_deterministic() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        sched.enqueue_stream_chunk("call-a".to_string(), 0, serde_json::json!("a0"), false);
        sched.enqueue_stream_chunk("call-b".to_string(), 0, serde_json::json!("b0"), false);
        sched.enqueue_stream_chunk("call-a".to_string(), 1, serde_json::json!("a1"), true);
        sched.enqueue_stream_chunk("call-b".to_string(), 1, serde_json::json!("b1"), true);

        let mut trace = Vec::new();
        while let Some(task) = sched.tick() {
            let MacrotaskKind::HostcallComplete { call_id, outcome } = task.kind else {
                unreachable!("expected hostcall completion task");
            };
            let HostcallOutcome::StreamChunk {
                sequence, is_final, ..
            } = outcome
            else {
                unreachable!("expected stream chunk outcome");
            };
            trace.push((call_id, sequence, is_final));
        }

        assert_eq!(
            trace,
            vec![
                ("call-a".to_string(), 0, false),
                ("call-b".to_string(), 0, false),
                ("call-a".to_string(), 1, true),
                ("call-b".to_string(), 1, true),
            ]
        );
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
            other => unreachable!("Expected evt-1, got {other:?}"),
        }
        match task2.kind {
            MacrotaskKind::InboundEvent { event_id, .. } => assert_eq!(event_id, "evt-2"),
            other => unreachable!("Expected evt-2, got {other:?}"),
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
            other => unreachable!("Expected event first, got {other:?}"),
        }

        // Then timer
        let task2 = sched.tick().unwrap();
        match task2.kind {
            MacrotaskKind::TimerFired { .. } => {}
            other => unreachable!("Expected timer second, got {other:?}"),
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

    #[test]
    fn scheduler_debug_reports_live_timer_count() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        let cancelled = sched.set_timeout(10);
        sched.set_timeout(20);

        assert!(sched.clear_timeout(cancelled));

        let debug = format!("{sched:?}");
        assert!(
            debug.contains("timer_count: 1"),
            "unexpected debug: {debug}"
        );
        assert!(
            debug.contains("cancelled_timers: 1"),
            "unexpected debug: {debug}"
        );
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
                    HostcallOutcome::StreamChunk { is_final, .. } => {
                        if *is_final {
                            "stream_final"
                        } else {
                            "chunk"
                        }
                    }
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
                1 if !timers.is_empty() => {
                    let idx = rng.next_usize(timers.len());
                    let _cancelled = sched.clear_timeout(timers[idx]);
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

    #[test]
    #[allow(clippy::too_many_lines)]
    fn scheduler_fairness_replay_under_hostcall_timer_and_event_storms() {
        const ROUNDS: usize = 160;
        const TIMERS_PER_ROUND: usize = 4;
        const HOSTCALLS_PER_ROUND: usize = 12;
        const STREAMS_PER_ROUND: usize = 2;
        const CHUNKS_PER_STREAM: usize = 3;
        const EVENTS_PER_ROUND: usize = 2;
        const MAX_CLASS_GAP_TICKS: u64 = 24;

        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);
        let mut trace = Vec::new();
        let mut class_counts = std::collections::BTreeMap::<&'static str, u64>::new();
        let mut last_class_tick = std::collections::BTreeMap::<&'static str, u64>::new();
        let mut max_class_gap = std::collections::BTreeMap::<&'static str, u64>::new();
        let mut last_stream_seq = std::collections::BTreeMap::<String, u64>::new();
        let mut stream_final_counts = std::collections::BTreeMap::<String, u64>::new();
        let mut timer_groups = Vec::<Vec<u64>>::new();
        let mut max_macrotask_depth = 0usize;
        let mut max_timer_count = 0usize;
        let mut first_violation: Option<String> = None;

        for round in 0..ROUNDS {
            let expected_timers = (0..TIMERS_PER_ROUND)
                .map(|_| sched.set_timeout(1))
                .collect::<Vec<_>>();

            for call_idx in 0..HOSTCALLS_PER_ROUND {
                let call_id = format!("round-{round}-call-{call_idx}");
                let outcome = if call_idx % 5 == 0 {
                    HostcallOutcome::Error {
                        code: "E_REPLAY".to_string(),
                        message: "synthetic replay error".to_string(),
                    }
                } else {
                    HostcallOutcome::Success(
                        serde_json::json!({ "round": round, "call": call_idx }),
                    )
                };
                sched.enqueue_hostcall_complete(call_id, outcome);
            }

            for stream_idx in 0..STREAMS_PER_ROUND {
                let call_id = format!("round-{round}-stream-{stream_idx}");
                for chunk_idx in 0..CHUNKS_PER_STREAM {
                    sched.enqueue_stream_chunk(
                        call_id.clone(),
                        chunk_idx as u64,
                        serde_json::json!({
                            "round": round,
                            "stream": stream_idx,
                            "chunk": chunk_idx,
                        }),
                        chunk_idx + 1 == CHUNKS_PER_STREAM,
                    );
                }
            }

            sched.enqueue_event(
                format!("cancel-{round}"),
                serde_json::json!({
                    "kind": "cancel",
                    "round": round,
                    "target": format!("round-{round}-call-0"),
                }),
            );
            sched.enqueue_event(
                format!("inbound-{round}"),
                serde_json::json!({ "kind": "inbound", "round": round }),
            );

            max_macrotask_depth = max_macrotask_depth.max(sched.macrotask_count());
            max_timer_count = max_timer_count.max(sched.timer_count());

            sched.clock.advance(1);

            let mut round_timers = Vec::new();
            while let Some(task) = sched.tick() {
                max_macrotask_depth = max_macrotask_depth.max(sched.macrotask_count());
                max_timer_count = max_timer_count.max(sched.timer_count());

                let tick = u64::try_from(trace.len()).expect("trace length fits in u64");
                let class = match &task.kind {
                    MacrotaskKind::TimerFired { timer_id } => {
                        round_timers.push(*timer_id);
                        "timer"
                    }
                    MacrotaskKind::HostcallComplete { call_id, outcome } => match outcome {
                        HostcallOutcome::Success(_) => "hostcall_success",
                        HostcallOutcome::Error { .. } => "hostcall_error",
                        HostcallOutcome::StreamChunk {
                            sequence, is_final, ..
                        } => {
                            match last_stream_seq.get(call_id) {
                                Some(previous) if *sequence != previous.saturating_add(1) => {
                                    first_violation.get_or_insert_with(|| {
                                        format!(
                                            "stream {call_id} sequence jumped from {previous} to {sequence}"
                                        )
                                    });
                                }
                                None if *sequence != 0 => {
                                    first_violation.get_or_insert_with(|| {
                                        format!("stream {call_id} started at sequence {sequence}")
                                    });
                                }
                                _ => {}
                            }
                            last_stream_seq.insert(call_id.clone(), *sequence);
                            if *is_final {
                                *stream_final_counts.entry(call_id.clone()).or_default() += 1;
                                "stream_final"
                            } else {
                                "stream_chunk"
                            }
                        }
                    },
                    MacrotaskKind::InboundEvent { event_id, .. } => {
                        if event_id.starts_with("cancel-") {
                            "cancel_event"
                        } else {
                            "inbound_event"
                        }
                    }
                };

                *class_counts.entry(class).or_default() += 1;
                if let Some(previous_tick) = last_class_tick.insert(class, tick) {
                    let gap = tick.saturating_sub(previous_tick);
                    let max_gap = max_class_gap.entry(class).or_default();
                    *max_gap = (*max_gap).max(gap);
                    if gap > MAX_CLASS_GAP_TICKS {
                        first_violation.get_or_insert_with(|| {
                            format!("class {class} starved for {gap} ticks")
                        });
                    }
                }

                trace.push(trace_entry(&task));
            }

            if round_timers != expected_timers {
                first_violation.get_or_insert_with(|| {
                    format!(
                        "round {round} timers fired out of insertion order: expected {expected_timers:?}, got {round_timers:?}"
                    )
                });
            }
            timer_groups.push(round_timers);
        }

        let expected_total = ROUNDS
            * (TIMERS_PER_ROUND
                + HOSTCALLS_PER_ROUND
                + (STREAMS_PER_ROUND * CHUNKS_PER_STREAM)
                + EVENTS_PER_ROUND);
        let expected_streams = ROUNDS * STREAMS_PER_ROUND;
        assert_eq!(trace.len(), expected_total);
        assert_eq!(first_violation, None);
        assert!(
            max_macrotask_depth
                <= HOSTCALLS_PER_ROUND
                    + STREAMS_PER_ROUND * CHUNKS_PER_STREAM
                    + EVENTS_PER_ROUND
                    + TIMERS_PER_ROUND,
            "macrotask queue grew beyond one replay round: {max_macrotask_depth}"
        );
        assert_eq!(max_timer_count, TIMERS_PER_ROUND);
        assert_eq!(timer_groups.len(), ROUNDS);
        assert_eq!(stream_final_counts.len(), expected_streams);
        assert!(
            stream_final_counts.values().all(|count| *count == 1),
            "each stream should complete exactly once: {stream_final_counts:?}"
        );

        for required in [
            "timer",
            "hostcall_success",
            "hostcall_error",
            "stream_chunk",
            "stream_final",
            "cancel_event",
            "inbound_event",
        ] {
            assert!(
                class_counts.get(required).copied().unwrap_or_default() > 0,
                "missing replay class {required}"
            );
            assert!(
                max_class_gap.get(required).copied().unwrap_or_default() <= MAX_CLASS_GAP_TICKS,
                "class {required} exceeded fairness gap: {max_class_gap:?}"
            );
        }

        let summary = serde_json::json!({
            "schema": "pi.scheduler.fairness_replay.v1",
            "rounds": ROUNDS,
            "total_tasks": trace.len(),
            "class_counts": class_counts,
            "max_class_gap_ticks": max_class_gap,
            "max_macrotask_queue_depth": max_macrotask_depth,
            "max_timer_count": max_timer_count,
            "equal_deadline_timer_order": "stable",
            "stream_count": expected_streams,
            "stream_final_count": stream_final_counts.len(),
            "first_violation": first_violation,
        });

        assert_eq!(
            summary,
            serde_json::json!({
                "schema": "pi.scheduler.fairness_replay.v1",
                "rounds": 160,
                "total_tasks": 3840,
                "class_counts": {
                    "cancel_event": 160,
                    "hostcall_error": 480,
                    "hostcall_success": 1440,
                    "inbound_event": 160,
                    "stream_chunk": 640,
                    "stream_final": 320,
                    "timer": 640,
                },
                "max_class_gap_ticks": {
                    "cancel_event": 24,
                    "hostcall_error": 14,
                    "hostcall_success": 14,
                    "inbound_event": 24,
                    "stream_chunk": 20,
                    "stream_final": 21,
                    "timer": 21,
                },
                "max_macrotask_queue_depth": 23,
                "max_timer_count": 4,
                "equal_deadline_timer_order": "stable",
                "stream_count": 320,
                "stream_final_count": 320,
                "first_violation": null,
            })
        );
    }

    // ── Seq Display format ──────────────────────────────────────────

    #[test]
    fn seq_display_format() {
        assert_eq!(format!("{}", Seq::zero()), "seq:0");
        assert_eq!(format!("{}", Seq::zero().next()), "seq:1");
    }

    // ── has_pending / macrotask_count / timer_count ──────────────────

    #[test]
    fn empty_scheduler_has_no_pending() {
        let sched = Scheduler::with_clock(DeterministicClock::new(0));
        assert!(!sched.has_pending());
        assert_eq!(sched.macrotask_count(), 0);
        assert_eq!(sched.timer_count(), 0);
    }

    #[test]
    fn has_pending_with_timer_only() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        sched.set_timeout(100);
        assert!(sched.has_pending());
        assert_eq!(sched.macrotask_count(), 0);
        assert_eq!(sched.timer_count(), 1);
    }

    #[test]
    fn has_pending_with_macrotask_only() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        sched.enqueue_event("e".to_string(), serde_json::json!({}));
        assert!(sched.has_pending());
        assert_eq!(sched.macrotask_count(), 1);
        assert_eq!(sched.timer_count(), 0);
    }

    #[test]
    fn has_pending_ignores_cancelled_timers_without_macrotasks() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        let timer = sched.set_timeout(10_000);
        assert!(sched.clear_timeout(timer));
        assert!(!sched.has_pending());
        assert_eq!(sched.timer_count(), 0);
    }

    #[test]
    fn timer_count_ignores_cancelled_timers_before_they_are_reaped() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        let live = sched.set_timeout(50);
        let cancelled = sched.set_timeout(100);

        assert!(sched.clear_timeout(cancelled));
        assert_eq!(sched.timer_count(), 1);
        assert_eq!(sched.next_timer_deadline(), Some(50));

        sched.clock.advance(60);
        let task = sched.tick().expect("live timer should fire");
        match task.kind {
            MacrotaskKind::TimerFired { timer_id } => assert_eq!(timer_id, live),
            other => unreachable!("Expected live timer, got {other:?}"),
        }
        assert_eq!(sched.timer_count(), 0);
    }

    // ── WallClock ────────────────────────────────────────────────────

    #[test]
    fn wall_clock_returns_positive_ms() {
        let clock = WallClock;
        let now = clock.now_ms();
        assert!(now > 0, "WallClock should return a positive timestamp");
    }

    // ── clear_timeout edge cases ─────────────────────────────────────

    #[test]
    fn clear_timeout_nonexistent_returns_false() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        assert!(!sched.clear_timeout(999));
        assert!(
            sched.cancelled_timers.is_empty(),
            "unknown timer ids should not pollute cancelled set"
        );
    }

    #[test]
    fn clear_timeout_double_cancel_returns_false() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        let t = sched.set_timeout(100);
        assert!(sched.clear_timeout(t));
        // Second cancel - already in set
        assert!(!sched.clear_timeout(t));
    }

    // ── time_until_next_timer ────────────────────────────────────────

    #[test]
    fn time_until_next_timer_none_when_empty() {
        let sched = Scheduler::with_clock(DeterministicClock::new(0));
        assert!(sched.time_until_next_timer().is_none());
    }

    #[test]
    fn time_until_next_timer_saturates_at_zero() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        sched.set_timeout(50);
        sched.clock.advance(100); // Past the deadline
        assert_eq!(sched.time_until_next_timer(), Some(0));
    }

    // ── HostcallOutcome::Error path ──────────────────────────────────

    #[test]
    fn hostcall_error_outcome() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        sched.enqueue_hostcall_complete(
            "err-call".to_string(),
            HostcallOutcome::Error {
                code: "E_TIMEOUT".to_string(),
                message: "Request timed out".to_string(),
            },
        );

        let task = sched.tick().unwrap();
        match task.kind {
            MacrotaskKind::HostcallComplete { call_id, outcome } => {
                assert_eq!(call_id, "err-call");
                match outcome {
                    HostcallOutcome::Error { code, message } => {
                        assert_eq!(code, "E_TIMEOUT");
                        assert_eq!(message, "Request timed out");
                    }
                    other => unreachable!("Expected error, got {other:?}"),
                }
            }
            other => unreachable!("Expected HostcallComplete, got {other:?}"),
        }
    }

    // ── timer_count decreases after tick ─────────────────────────────

    #[test]
    fn timer_count_decreases_after_fire() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        sched.set_timeout(50);
        sched.set_timeout(100);
        assert_eq!(sched.timer_count(), 2);

        sched.clock.advance(75);
        let _task = sched.tick(); // Fires first timer
        assert_eq!(sched.timer_count(), 1);
    }

    // ── empty tick returns None ──────────────────────────────────────

    #[test]
    fn empty_scheduler_tick_returns_none() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        assert!(sched.tick().is_none());
    }

    // ── default constructor ──────────────────────────────────────────

    #[test]
    fn default_scheduler_starts_with_seq_zero() {
        let sched = Scheduler::new();
        assert_eq!(sched.current_seq(), Seq::zero());
    }

    // ── Arc<Clock> impl ──────────────────────────────────────────────

    #[test]
    fn arc_clock_delegation() {
        let clock = Arc::new(DeterministicClock::new(42));
        assert_eq!(Clock::now_ms(&clock), 42);
        clock.advance(10);
        assert_eq!(Clock::now_ms(&clock), 52);
    }

    // ── TimerEntry equality ──────────────────────────────────────────

    #[test]
    fn timer_entry_equality_ignores_timer_id() {
        let a = TimerEntry::new(1, 100, Seq(5));
        let b = TimerEntry::new(2, 100, Seq(5));
        // PartialEq compares (deadline_ms, seq), not timer_id
        assert_eq!(a, b);
    }

    // ── Macrotask PartialEq uses seq only ────────────────────────────

    #[test]
    fn macrotask_equality_uses_seq_only() {
        let a = Macrotask::new(Seq(1), MacrotaskKind::TimerFired { timer_id: 1 });
        let b = Macrotask::new(Seq(1), MacrotaskKind::TimerFired { timer_id: 2 });
        assert_eq!(a, b); // Same seq → equal
    }

    // ── bd-2tl1.5: Streaming concurrency + determinism ──────────────

    #[test]
    fn scheduler_ten_concurrent_streams_complete_independently() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);
        let n_streams: usize = 10;
        let chunks_per_stream: usize = 5;

        // Enqueue N streams with M chunks each, interleaved round-robin.
        for chunk_idx in 0..chunks_per_stream {
            for stream_idx in 0..n_streams {
                let is_final = chunk_idx == chunks_per_stream - 1;
                sched.enqueue_stream_chunk(
                    format!("stream-{stream_idx}"),
                    chunk_idx as u64,
                    serde_json::json!({ "s": stream_idx, "c": chunk_idx }),
                    is_final,
                );
            }
        }

        let mut per_stream: std::collections::HashMap<String, Vec<(u64, bool)>> =
            std::collections::HashMap::new();
        while let Some(task) = sched.tick() {
            let MacrotaskKind::HostcallComplete { call_id, outcome } = task.kind else {
                unreachable!("expected hostcall completion");
            };
            let HostcallOutcome::StreamChunk {
                sequence, is_final, ..
            } = outcome
            else {
                unreachable!("expected stream chunk");
            };
            per_stream
                .entry(call_id)
                .or_default()
                .push((sequence, is_final));
        }

        assert_eq!(per_stream.len(), n_streams);
        for (call_id, chunks) in &per_stream {
            assert_eq!(
                chunks.len(),
                chunks_per_stream,
                "stream {call_id} incomplete"
            );
            // Sequences are monotonically increasing per stream.
            for (i, (seq, _)) in chunks.iter().enumerate() {
                assert_eq!(*seq, i as u64, "stream {call_id}: non-monotonic at {i}");
            }
            // Exactly one final chunk (the last).
            let final_count = chunks.iter().filter(|(_, f)| *f).count();
            assert_eq!(
                final_count, 1,
                "stream {call_id}: expected exactly one final"
            );
            assert!(
                chunks.last().unwrap().1,
                "stream {call_id}: final must be last"
            );
        }
    }

    #[test]
    fn scheduler_mixed_stream_nonstream_ordering() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        // Enqueue: event, stream chunk, success, stream final, event.
        sched.enqueue_event("evt-1".to_string(), serde_json::json!({"n": 1}));
        sched.enqueue_stream_chunk("stream-x".to_string(), 0, serde_json::json!("data"), false);
        sched.enqueue_hostcall_complete(
            "call-y".to_string(),
            HostcallOutcome::Success(serde_json::json!({"ok": true})),
        );
        sched.enqueue_stream_chunk("stream-x".to_string(), 1, serde_json::json!("end"), true);
        sched.enqueue_event("evt-2".to_string(), serde_json::json!({"n": 2}));

        let mut trace = Vec::new();
        while let Some(task) = sched.tick() {
            trace.push(trace_entry(&task));
        }

        // FIFO ordering: all 5 items in enqueue order.
        assert_eq!(trace.len(), 5);
        assert!(trace[0].contains("event:evt-1"));
        assert!(trace[1].contains("stream-x") && trace[1].contains("chunk"));
        assert!(trace[2].contains("call-y") && trace[2].contains("ok"));
        assert!(trace[3].contains("stream-x") && trace[3].contains("stream_final"));
        assert!(trace[4].contains("event:evt-2"));
    }

    #[test]
    fn scheduler_concurrent_streams_deterministic_across_runs() {
        fn run_ten_streams() -> Vec<String> {
            let clock = DeterministicClock::new(0);
            let mut sched = Scheduler::with_clock(clock);

            for chunk in 0..3_u64 {
                for stream in 0..10 {
                    sched.enqueue_stream_chunk(
                        format!("s{stream}"),
                        chunk,
                        serde_json::json!(chunk),
                        chunk == 2,
                    );
                }
            }

            let mut trace = Vec::new();
            while let Some(task) = sched.tick() {
                trace.push(trace_entry(&task));
            }
            trace
        }

        let a = run_ten_streams();
        let b = run_ten_streams();
        assert_eq!(a, b, "10-stream trace must be deterministic");
        assert_eq!(a.len(), 30, "expected 10 streams x 3 chunks = 30 entries");
    }

    #[test]
    fn scheduler_stream_interleaved_with_timers() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        // Set a timer for 100ms.
        let _t = sched.set_timeout(100);

        // Enqueue first stream chunk.
        sched.enqueue_stream_chunk("s1".to_string(), 0, serde_json::json!("a"), false);

        // Advance clock past timer deadline.
        sched.clock.advance(150);

        // Enqueue final stream chunk after timer.
        sched.enqueue_stream_chunk("s1".to_string(), 1, serde_json::json!("b"), true); // seq=3

        let mut trace = Vec::new();
        while let Some(task) = sched.tick() {
            trace.push(trace_entry(&task));
        }

        // Pending macrotasks run first; due timers are enqueued after existing work.
        assert_eq!(trace.len(), 3);
        assert!(
            trace[0].contains("s1") && trace[0].contains("chunk"),
            "first: stream chunk 0, got: {}",
            trace[0]
        );
        assert!(
            trace[1].contains("s1") && trace[1].contains("stream_final"),
            "second: stream final, got: {}",
            trace[1]
        );
        assert!(
            trace[2].contains("timer"),
            "third: timer, got: {}",
            trace[2]
        );
    }

    #[test]
    fn scheduler_due_timers_do_not_preempt_queued_macrotasks() {
        let clock = DeterministicClock::new(0);
        let mut sched = Scheduler::with_clock(clock);

        // 1. Set a timer T1. Deadline = 100ms.
        let t1_id = sched.set_timeout(100);

        // 2. Enqueue an event E1 before timer delivery.
        sched.enqueue_event("E1".to_string(), serde_json::json!({}));

        // 3. Advance time so T1 is due.
        sched.clock.advance(100);

        // 4. Tick 1: queued event executes first.
        let task1 = sched.tick().expect("Should have a task");

        // 5. Tick 2: timer executes next.
        let task2 = sched.tick().expect("Should have a task");

        let seq1 = task1.seq.value();
        let seq2 = task2.seq.value();

        // Global macrotask seq is monotone for externally observed execution.
        assert!(
            seq1 < seq2,
            "Invariant I5 violation: Task execution not ordered by seq. Executed {seq1} then {seq2}"
        );

        if let MacrotaskKind::InboundEvent { event_id, .. } = task1.kind {
            assert_eq!(event_id, "E1");
        } else {
            unreachable!();
        }

        if let MacrotaskKind::TimerFired { timer_id } = task2.kind {
            assert_eq!(timer_id, t1_id);
        } else {
            unreachable!();
        }
    }

    #[test]
    fn reactor_mesh_hash_routing_is_stable_for_call_id() {
        let mut mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 8,
            lane_capacity: 64,
            topology: None,
        });

        let first = mesh
            .enqueue_hostcall_complete(
                "call-affinity".to_string(),
                HostcallOutcome::Success(serde_json::json!({})),
            )
            .expect("first enqueue");
        let second = mesh
            .enqueue_hostcall_complete(
                "call-affinity".to_string(),
                HostcallOutcome::Success(serde_json::json!({})),
            )
            .expect("second enqueue");

        assert_eq!(
            first.shard_id, second.shard_id,
            "call_id hash routing must preserve shard affinity"
        );
        assert_eq!(first.shard_seq + 1, second.shard_seq);
    }

    #[test]
    fn reactor_mesh_round_robin_event_distribution_is_deterministic() {
        let mut mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 3,
            lane_capacity: 64,
            topology: None,
        });

        let mut routed = Vec::new();
        for idx in 0..6 {
            let envelope = mesh
                .enqueue_event(format!("evt-{idx}"), serde_json::json!({"i": idx}))
                .expect("enqueue event");
            routed.push(envelope.shard_id);
        }

        assert_eq!(routed, vec![0, 1, 2, 0, 1, 2]);
    }

    #[test]
    fn reactor_mesh_drain_global_order_preserves_monotone_seq() {
        let mut mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 4,
            lane_capacity: 64,
            topology: None,
        });

        let mut expected = Vec::new();
        expected.push(
            mesh.enqueue_event("evt-1".to_string(), serde_json::json!({"v": 1}))
                .expect("event 1")
                .global_seq
                .value(),
        );
        expected.push(
            mesh.enqueue_hostcall_complete(
                "call-a".to_string(),
                HostcallOutcome::Success(serde_json::json!({"ok": true})),
            )
            .expect("call-a")
            .global_seq
            .value(),
        );
        expected.push(
            mesh.enqueue_event("evt-2".to_string(), serde_json::json!({"v": 2}))
                .expect("event 2")
                .global_seq
                .value(),
        );
        expected.push(
            mesh.enqueue_hostcall_complete(
                "call-b".to_string(),
                HostcallOutcome::Error {
                    code: "E_TEST".to_string(),
                    message: "boom".to_string(),
                },
            )
            .expect("call-b")
            .global_seq
            .value(),
        );

        let drained = mesh.drain_global_order(16);
        let observed = drained
            .iter()
            .map(|entry| entry.global_seq.value())
            .collect::<Vec<_>>();
        assert_eq!(observed, expected);
    }

    #[test]
    fn reactor_mesh_backpressure_tracks_rejected_enqueues() {
        let mut mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 1,
            lane_capacity: 2,
            topology: None,
        });

        mesh.enqueue_event("evt-0".to_string(), serde_json::json!({}))
            .expect("enqueue evt-0");
        mesh.enqueue_event("evt-1".to_string(), serde_json::json!({}))
            .expect("enqueue evt-1");

        let err = mesh
            .enqueue_event("evt-overflow".to_string(), serde_json::json!({}))
            .expect_err("third enqueue should overflow");
        assert_eq!(err.shard_id, 0);
        assert_eq!(err.capacity, 2);
        assert_eq!(err.depth, 2);

        let telemetry = mesh.telemetry();
        assert_eq!(telemetry.rejected_enqueues, 1);
        assert_eq!(telemetry.max_queue_depths, vec![2]);
        assert_eq!(telemetry.queue_depths, vec![2]);
    }

    #[test]
    fn reactor_placement_manifest_is_deterministic_across_runs() {
        let topology =
            ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 0), (2, 1), (3, 1)]);
        let first = ReactorPlacementManifest::plan(8, Some(&topology));
        let second = ReactorPlacementManifest::plan(8, Some(&topology));
        assert_eq!(first, second);
        assert_eq!(first.fallback_reason, None);
    }

    #[test]
    fn reactor_topology_snapshot_normalizes_unsorted_duplicate_pairs() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[
            (7, 5),
            (2, 1),
            (4, 2),
            (2, 1),
            (1, 1),
            (4, 2),
        ]);
        let normalized = topology
            .cores
            .iter()
            .map(|core| (core.core_id, core.numa_node))
            .collect::<Vec<_>>();
        assert_eq!(normalized, vec![(1, 1), (2, 1), (4, 2), (7, 5)]);
    }

    #[test]
    fn reactor_placement_manifest_non_contiguous_numa_ids_is_stable() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[
            (11, 42),
            (7, 5),
            (9, 42),
            (3, 5),
            (11, 42),
            (3, 5),
        ]);
        let first = ReactorPlacementManifest::plan(8, Some(&topology));
        let second = ReactorPlacementManifest::plan(8, Some(&topology));
        assert_eq!(first, second);
        assert_eq!(first.numa_node_count, 2);
        assert_eq!(first.fallback_reason, None);

        let observed_nodes = first
            .bindings
            .iter()
            .map(|binding| binding.numa_node)
            .collect::<Vec<_>>();
        assert_eq!(observed_nodes, vec![5, 42, 5, 42, 5, 42, 5, 42]);

        let observed_cores = first
            .bindings
            .iter()
            .map(|binding| binding.core_id)
            .collect::<Vec<_>>();
        assert_eq!(observed_cores, vec![3, 9, 7, 11, 3, 9, 7, 11]);
    }

    #[test]
    fn reactor_placement_manifest_spreads_across_numa_nodes_round_robin() {
        let topology =
            ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 0), (4, 1), (5, 1)]);
        let manifest = ReactorPlacementManifest::plan(6, Some(&topology));
        let observed_nodes = manifest
            .bindings
            .iter()
            .map(|binding| binding.numa_node)
            .collect::<Vec<_>>();
        assert_eq!(observed_nodes, vec![0, 1, 0, 1, 0, 1]);

        let observed_cores = manifest
            .bindings
            .iter()
            .map(|binding| binding.core_id)
            .collect::<Vec<_>>();
        assert_eq!(observed_cores, vec![0, 4, 1, 5, 0, 4]);
    }

    #[test]
    fn reactor_placement_manifest_records_fallback_when_topology_missing() {
        let manifest = ReactorPlacementManifest::plan(3, None);
        assert_eq!(
            manifest.fallback_reason,
            Some(ReactorPlacementFallbackReason::TopologyUnavailable)
        );
        assert_eq!(manifest.numa_node_count, 1);
        assert_eq!(manifest.bindings.len(), 3);
        assert_eq!(manifest.bindings[0].core_id, 0);
        assert_eq!(manifest.bindings[2].core_id, 2);
    }

    #[test]
    fn reactor_mesh_exposes_machine_readable_placement_manifest() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(2, 0), (3, 0)]);
        let mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 3,
            lane_capacity: 8,
            topology: Some(topology),
        });
        let manifest = mesh.placement_manifest();
        let as_json = manifest.as_json();
        assert_eq!(as_json["shard_count"], serde_json::json!(3));
        assert_eq!(as_json["numa_node_count"], serde_json::json!(1));
        assert_eq!(
            as_json["fallback_reason"],
            serde_json::json!(Some("single_numa_node"))
        );
        assert_eq!(
            as_json["bindings"].as_array().map(std::vec::Vec::len),
            Some(3),
            "expected per-shard binding rows"
        );
    }

    #[test]
    fn reactor_mesh_telemetry_includes_binding_and_fallback_metadata() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(10, 0), (11, 0)]);
        let mut mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 2,
            lane_capacity: 4,
            topology: Some(topology),
        });
        mesh.enqueue_event("evt-0".to_string(), serde_json::json!({}))
            .expect("enqueue event");

        let telemetry = mesh.telemetry();
        assert_eq!(
            telemetry.fallback_reason,
            Some(ReactorPlacementFallbackReason::SingleNumaNode)
        );
        assert_eq!(telemetry.shard_bindings.len(), 2);
        let telemetry_json = telemetry.as_json();
        assert_eq!(
            telemetry_json["fallback_reason"],
            serde_json::json!(Some("single_numa_node"))
        );
        assert_eq!(
            telemetry_json["shard_bindings"]
                .as_array()
                .map(std::vec::Vec::len),
            Some(2)
        );
    }

    // ====================================================================
    // NUMA slab allocator tests
    // ====================================================================

    #[test]
    fn numa_slab_alloc_dealloc_round_trip() {
        let mut slab = NumaSlab::new(0, 4);
        let handle = slab.allocate().expect("should allocate");
        assert_eq!(handle.node_id, 0);
        assert_eq!(handle.generation, 1);
        assert!(slab.deallocate(&handle));
        assert_eq!(slab.in_use(), 0);
    }

    #[test]
    fn numa_slab_exhaustion_returns_none() {
        let mut slab = NumaSlab::new(0, 2);
        let _a = slab.allocate().expect("first alloc");
        let _b = slab.allocate().expect("second alloc");
        assert!(slab.allocate().is_none(), "slab should be exhausted");
    }

    #[test]
    fn numa_slab_generation_prevents_stale_dealloc() {
        let mut slab = NumaSlab::new(0, 2);
        let handle_v1 = slab.allocate().expect("first alloc");
        assert!(slab.deallocate(&handle_v1));
        let _handle_v2 = slab.allocate().expect("reuse slot");
        // The old handle has generation 1, the new allocation has generation 2.
        assert!(
            !slab.deallocate(&handle_v1),
            "stale generation should reject dealloc"
        );
    }

    #[test]
    fn numa_slab_double_free_is_rejected() {
        let mut slab = NumaSlab::new(0, 4);
        let handle = slab.allocate().expect("alloc");
        assert!(slab.deallocate(&handle));
        assert!(!slab.deallocate(&handle), "double free must be rejected");
    }

    #[test]
    fn numa_slab_wrong_node_dealloc_rejected() {
        let mut slab = NumaSlab::new(0, 4);
        let handle = slab.allocate().expect("alloc");
        let wrong_handle = NumaSlabHandle {
            node_id: 99,
            ..handle
        };
        assert!(
            !slab.deallocate(&wrong_handle),
            "wrong node_id should reject dealloc"
        );
    }

    #[test]
    fn numa_slab_high_water_mark_tracks_peak() {
        let mut slab = NumaSlab::new(0, 8);
        let a = slab.allocate().expect("a");
        let b = slab.allocate().expect("b");
        let c = slab.allocate().expect("c");
        assert_eq!(slab.high_water_mark, 3);
        slab.deallocate(&a);
        slab.deallocate(&b);
        assert_eq!(
            slab.high_water_mark, 3,
            "high water mark should not decrease"
        );
        slab.deallocate(&c);
        let _d = slab.allocate().expect("d");
        assert_eq!(slab.high_water_mark, 3);
    }

    #[test]
    fn numa_slab_pool_routes_to_local_node() {
        let topology =
            ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 0), (2, 1), (3, 1)]);
        let manifest = ReactorPlacementManifest::plan(4, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 8,
            entry_size_bytes: 256,
            hugepage: HugepageConfig {
                enabled: false,
                ..HugepageConfig::default()
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);
        assert_eq!(pool.node_count(), 2);

        let (handle, reason) = pool.allocate(1).expect("allocate on node 1");
        assert_eq!(handle.node_id, 1);
        assert!(reason.is_none(), "should be local allocation");
    }

    #[test]
    fn numa_slab_pool_cross_node_fallback_tracks_telemetry() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (2, 1)]);
        let manifest = ReactorPlacementManifest::plan(2, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 1, // Only 1 slot per node
            entry_size_bytes: 64,
            hugepage: HugepageConfig {
                enabled: false,
                ..HugepageConfig::default()
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);

        // Fill node 0's single slot.
        let (h0, _) = pool.allocate(0).expect("fill node 0");
        assert_eq!(h0.node_id, 0);

        // Next alloc for node 0 should fall back to node 1.
        let (h1, reason) = pool.allocate(0).expect("fallback to node 1");
        assert_eq!(h1.node_id, 1);
        assert_eq!(reason, Some(CrossNodeReason::LocalExhausted));

        let telemetry = pool.telemetry();
        assert_eq!(telemetry.cross_node_allocs, 1);
        let json = telemetry.as_json();
        assert_eq!(json["total_allocs"], serde_json::json!(2));
        assert_eq!(json["hugepage_backed_allocs"], serde_json::json!(0));
        assert_eq!(json["local_allocs"], serde_json::json!(1));
        assert_eq!(json["remote_allocs"], serde_json::json!(1));
        assert_eq!(
            json["allocation_ratio_bps"]["local"],
            serde_json::json!(5000)
        );
        assert_eq!(
            json["allocation_ratio_bps"]["remote"],
            serde_json::json!(5000)
        );
        assert_eq!(
            json["allocation_ratio_bps"]["scale"],
            serde_json::json!(10_000)
        );
        assert_eq!(json["hugepage_hit_rate_bps"]["value"], serde_json::json!(0));
        assert_eq!(
            json["latency_proxies_bps"]["tlb_miss_pressure"],
            serde_json::json!(5000)
        );
        assert_eq!(
            json["latency_proxies_bps"]["cache_miss_pressure"],
            serde_json::json!(10_000)
        );
        assert_eq!(
            json["latency_proxies_bps"]["occupancy_pressure"],
            serde_json::json!(10_000)
        );
        assert_eq!(
            json["pressure_bands"]["tlb_miss"],
            serde_json::json!("medium")
        );
        assert_eq!(
            json["pressure_bands"]["cache_miss"],
            serde_json::json!("high")
        );
        assert_eq!(
            json["pressure_bands"]["occupancy"],
            serde_json::json!("high")
        );
        assert_eq!(
            json["fallback_reasons"]["cross_node"],
            serde_json::json!("local_exhausted")
        );
        assert_eq!(
            json["fallback_reasons"]["hugepage"],
            serde_json::json!("hugepage_disabled")
        );
    }

    #[test]
    fn numa_slab_pool_total_exhaustion_returns_none() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 1,
            entry_size_bytes: 64,
            hugepage: HugepageConfig {
                enabled: false,
                ..HugepageConfig::default()
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);
        let _ = pool.allocate(0).expect("fill the only slot");
        assert!(pool.allocate(0).is_none(), "pool should be exhausted");
    }

    #[test]
    fn numa_slab_pool_deallocate_round_trip() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 1)]);
        let manifest = ReactorPlacementManifest::plan(2, Some(&topology));
        let config = NumaSlabConfig::default();
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);

        let (handle, _) = pool.allocate(1).expect("alloc");
        assert!(pool.deallocate(&handle));
        assert!(!pool.deallocate(&handle), "double free must be rejected");
    }

    #[test]
    fn hugepage_status_disabled_reports_fallback() {
        let config = HugepageConfig {
            enabled: false,
            ..HugepageConfig::default()
        };
        let status = HugepageStatus::evaluate(&config, 1024, 512);
        assert!(!status.active);
        assert_eq!(
            status.fallback_reason,
            Some(HugepageFallbackReason::Disabled)
        );
    }

    #[test]
    fn hugepage_status_zero_totals_means_unavailable() {
        let config = HugepageConfig::default();
        let status = HugepageStatus::evaluate(&config, 0, 0);
        assert!(!status.active);
        assert_eq!(
            status.fallback_reason,
            Some(HugepageFallbackReason::DetectionUnavailable)
        );
    }

    #[test]
    fn hugepage_status_zero_free_means_insufficient() {
        let config = HugepageConfig::default();
        let status = HugepageStatus::evaluate(&config, 1024, 0);
        assert!(!status.active);
        assert_eq!(
            status.fallback_reason,
            Some(HugepageFallbackReason::InsufficientHugepages)
        );
    }

    #[test]
    fn hugepage_status_available_is_active() {
        let config = HugepageConfig::default();
        let status = HugepageStatus::evaluate(&config, 1024, 512);
        assert!(status.active);
        assert!(status.fallback_reason.is_none());
        assert_eq!(status.free_pages, 512);
    }

    #[test]
    fn numa_slab_pool_tracks_hugepage_hit_rate_when_active() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 4,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 4096,
                enabled: true,
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);
        pool.set_hugepage_status(HugepageStatus {
            total_pages: 128,
            free_pages: 64,
            page_size_bytes: 4096,
            active: true,
            fallback_reason: None,
        });

        let _ = pool.allocate(0).expect("first hugepage-backed alloc");
        let _ = pool.allocate(0).expect("second hugepage-backed alloc");

        let telemetry = pool.telemetry();
        let json = telemetry.as_json();
        assert_eq!(json["total_allocs"], serde_json::json!(2));
        assert_eq!(json["hugepage_backed_allocs"], serde_json::json!(2));
        assert_eq!(
            json["hugepage_hit_rate_bps"]["value"],
            serde_json::json!(10_000)
        );
        assert_eq!(
            json["hugepage_hit_rate_bps"]["scale"],
            serde_json::json!(10_000)
        );
        assert_eq!(json["hugepage"]["active"], serde_json::json!(true));
    }

    #[test]
    fn numa_slab_pool_misaligned_hugepage_config_reports_alignment_mismatch() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 3,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 2048,
                enabled: true,
            },
        };

        let pool = NumaSlabPool::from_manifest(&manifest, config);
        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::AlignmentMismatch)
        );

        let json = telemetry.as_json();
        assert_eq!(
            json["hugepage"]["fallback_reason"],
            serde_json::json!("alignment_mismatch")
        );
        assert_eq!(
            json["fallback_reasons"]["hugepage"],
            serde_json::json!("alignment_mismatch")
        );
    }

    #[test]
    fn numa_slab_pool_aligned_hugepage_config_defaults_to_detection_unavailable() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 4,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 4096,
                enabled: true,
            },
        };

        let pool = NumaSlabPool::from_manifest(&manifest, config);
        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::DetectionUnavailable)
        );
    }

    #[test]
    fn misaligned_hugepage_config_rejects_external_status_override() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 3,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 2048,
                enabled: true,
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);

        let forced = HugepageStatus::evaluate(&config.hugepage, 256, 64);
        assert!(forced.active);
        assert!(forced.fallback_reason.is_none());

        pool.set_hugepage_status(forced);
        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::AlignmentMismatch)
        );
    }

    #[test]
    fn disabled_hugepage_config_rejects_external_active_status_override() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 4,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 4096,
                enabled: false,
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);

        let forced = HugepageStatus {
            total_pages: 512,
            free_pages: 256,
            page_size_bytes: 4096,
            active: true,
            fallback_reason: None,
        };
        pool.set_hugepage_status(forced);

        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::Disabled)
        );
        assert_eq!(telemetry.hugepage_status.total_pages, 512);
        assert_eq!(telemetry.hugepage_status.free_pages, 256);

        let json = telemetry.as_json();
        assert_eq!(
            json["hugepage"]["fallback_reason"],
            serde_json::json!("hugepage_disabled")
        );
    }

    #[test]
    fn disabled_hugepage_config_uses_disabled_reason_even_if_slab_is_misaligned() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 3,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 2048,
                enabled: false,
            },
        };

        let pool = NumaSlabPool::from_manifest(&manifest, config);
        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::Disabled)
        );
    }

    #[test]
    fn hugepage_alignment_rejects_zero_page_size_and_fails_closed() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 4,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 0,
                enabled: true,
            },
        };
        assert!(!config.hugepage_alignment_ok());

        let pool = NumaSlabPool::from_manifest(&manifest, config);
        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::AlignmentMismatch)
        );
    }

    #[test]
    fn hugepage_alignment_rejects_zero_footprint_and_fails_closed() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 0,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 2048,
                enabled: true,
            },
        };
        assert_eq!(config.slab_footprint_bytes(), Some(0));
        assert!(!config.hugepage_alignment_ok());

        let pool = NumaSlabPool::from_manifest(&manifest, config);
        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::AlignmentMismatch)
        );
    }

    #[test]
    fn hugepage_alignment_rejects_checked_mul_overflow_without_panicking() {
        let config = NumaSlabConfig {
            slab_capacity: usize::MAX,
            entry_size_bytes: 2,
            hugepage: HugepageConfig {
                page_size_bytes: 4096,
                enabled: true,
            },
        };
        assert!(config.slab_footprint_bytes().is_none());
        assert!(!config.hugepage_alignment_ok());

        let status = config.alignment_mismatch_status();
        assert!(!status.active);
        assert_eq!(
            status.fallback_reason,
            Some(HugepageFallbackReason::AlignmentMismatch)
        );
    }

    #[test]
    fn zero_page_size_config_rejects_external_status_override() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 4,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 0,
                enabled: true,
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);

        let forced = HugepageStatus {
            total_pages: 128,
            free_pages: 64,
            page_size_bytes: 0,
            active: true,
            fallback_reason: None,
        };
        pool.set_hugepage_status(forced);

        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::AlignmentMismatch)
        );
    }

    #[test]
    fn zero_footprint_config_rejects_external_status_override() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 0,
            entry_size_bytes: 1024,
            hugepage: HugepageConfig {
                page_size_bytes: 2048,
                enabled: true,
            },
        };
        assert_eq!(config.slab_footprint_bytes(), Some(0));

        let mut pool = NumaSlabPool::from_manifest(&manifest, config);
        let forced = HugepageStatus {
            total_pages: 128,
            free_pages: 64,
            page_size_bytes: 2048,
            active: true,
            fallback_reason: None,
        };
        pool.set_hugepage_status(forced);

        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::AlignmentMismatch)
        );
        assert_eq!(telemetry.hugepage_status.total_pages, 0);
        assert_eq!(telemetry.hugepage_status.free_pages, 0);
    }

    #[test]
    fn checked_mul_overflow_config_rejects_external_status_override() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0)]);
        let manifest = ReactorPlacementManifest::plan(1, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 2,
            entry_size_bytes: usize::MAX,
            hugepage: HugepageConfig {
                page_size_bytes: 4096,
                enabled: true,
            },
        };
        assert!(config.slab_footprint_bytes().is_none());

        let mut pool = NumaSlabPool::from_manifest(&manifest, config);
        let forced = HugepageStatus {
            total_pages: 512,
            free_pages: 256,
            page_size_bytes: 4096,
            active: true,
            fallback_reason: None,
        };
        pool.set_hugepage_status(forced);

        let telemetry = pool.telemetry();
        assert!(!telemetry.hugepage_status.active);
        assert_eq!(
            telemetry.hugepage_status.fallback_reason,
            Some(HugepageFallbackReason::AlignmentMismatch)
        );
        assert_eq!(telemetry.hugepage_status.total_pages, 0);
        assert_eq!(telemetry.hugepage_status.free_pages, 0);
    }

    #[test]
    fn hugepage_status_json_is_stable() {
        let config = HugepageConfig::default();
        let status = HugepageStatus::evaluate(&config, 1024, 128);
        let json = status.as_json();
        assert_eq!(json["total_pages"], serde_json::json!(1024));
        assert_eq!(json["free_pages"], serde_json::json!(128));
        assert_eq!(json["active"], serde_json::json!(true));
        assert!(json["fallback_reason"].is_null());
    }

    #[test]
    fn numa_slab_telemetry_json_has_expected_shape() {
        let topology =
            ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 0), (4, 1), (5, 1)]);
        let manifest = ReactorPlacementManifest::plan(4, Some(&topology));
        let config = NumaSlabConfig {
            slab_capacity: 16,
            entry_size_bytes: 128,
            hugepage: HugepageConfig {
                enabled: false,
                ..HugepageConfig::default()
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);
        let _ = pool.allocate(0);
        let _ = pool.allocate(1);
        let _ = pool.allocate(0);

        let telemetry = pool.telemetry();
        let json = telemetry.as_json();
        assert_eq!(json["node_count"], serde_json::json!(2));
        assert_eq!(json["total_allocs"], serde_json::json!(3));
        assert_eq!(json["total_in_use"], serde_json::json!(3));
        assert_eq!(json["cross_node_allocs"], serde_json::json!(0));
        assert_eq!(json["hugepage_backed_allocs"], serde_json::json!(0));
        assert_eq!(json["local_allocs"], serde_json::json!(3));
        assert_eq!(json["remote_allocs"], serde_json::json!(0));
        assert_eq!(
            json["allocation_ratio_bps"]["local"],
            serde_json::json!(10_000)
        );
        assert_eq!(json["allocation_ratio_bps"]["remote"], serde_json::json!(0));
        assert_eq!(
            json["allocation_ratio_bps"]["scale"],
            serde_json::json!(10_000)
        );
        assert_eq!(json["hugepage_hit_rate_bps"]["value"], serde_json::json!(0));
        assert_eq!(
            json["latency_proxies_bps"]["tlb_miss_pressure"],
            serde_json::json!(0)
        );
        assert_eq!(
            json["latency_proxies_bps"]["cache_miss_pressure"],
            serde_json::json!(937)
        );
        assert_eq!(
            json["latency_proxies_bps"]["occupancy_pressure"],
            serde_json::json!(937)
        );
        assert_eq!(
            json["latency_proxies_bps"]["scale"],
            serde_json::json!(10_000)
        );
        assert_eq!(json["pressure_bands"]["tlb_miss"], serde_json::json!("low"));
        assert_eq!(
            json["pressure_bands"]["cache_miss"],
            serde_json::json!("low")
        );
        assert_eq!(
            json["pressure_bands"]["occupancy"],
            serde_json::json!("low")
        );
        assert_eq!(
            json["fallback_reasons"]["cross_node"],
            serde_json::Value::Null
        );
        assert_eq!(
            json["fallback_reasons"]["hugepage"],
            serde_json::json!("hugepage_disabled")
        );
        assert_eq!(json["config"]["slab_capacity"], serde_json::json!(16));
        assert_eq!(json["per_node"].as_array().map(std::vec::Vec::len), Some(2));
    }

    #[test]
    fn thread_affinity_advice_matches_placement_manifest() {
        let topology =
            ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 0), (4, 1), (5, 1)]);
        let manifest = ReactorPlacementManifest::plan(4, Some(&topology));
        let advice = manifest.affinity_advice(AffinityEnforcement::Advisory);
        assert_eq!(advice.len(), 4);
        assert_eq!(advice[0].shard_id, 0);
        assert_eq!(advice[0].recommended_core, 0);
        assert_eq!(advice[0].recommended_numa_node, 0);
        assert_eq!(advice[0].enforcement, AffinityEnforcement::Advisory);
        assert_eq!(advice[1].recommended_numa_node, 1);
        assert_eq!(advice[3].recommended_numa_node, 1);
    }

    #[test]
    fn thread_affinity_advice_json_is_stable() {
        let advice = ThreadAffinityAdvice {
            shard_id: 0,
            recommended_core: 3,
            recommended_numa_node: 1,
            enforcement: AffinityEnforcement::Strict,
        };
        let json = advice.as_json();
        assert_eq!(json["shard_id"], serde_json::json!(0));
        assert_eq!(json["recommended_core"], serde_json::json!(3));
        assert_eq!(json["enforcement"], serde_json::json!("strict"));
    }

    #[test]
    fn reactor_mesh_preferred_numa_node_uses_manifest() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (4, 1), (8, 2)]);
        let mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 3,
            lane_capacity: 8,
            topology: Some(topology),
        });
        assert_eq!(mesh.preferred_numa_node(0), 0);
        assert_eq!(mesh.preferred_numa_node(1), 1);
        assert_eq!(mesh.preferred_numa_node(2), 2);
        assert_eq!(mesh.preferred_numa_node(99), 0); // fallback
    }

    #[test]
    fn reactor_mesh_affinity_advice_covers_all_shards() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 1)]);
        let mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 2,
            lane_capacity: 8,
            topology: Some(topology),
        });
        let advice = mesh.affinity_advice(AffinityEnforcement::Disabled);
        assert_eq!(advice.len(), 2);
        assert_eq!(advice[0].enforcement, AffinityEnforcement::Disabled);
        assert_eq!(advice[1].enforcement, AffinityEnforcement::Disabled);
    }

    #[test]
    fn numa_slab_pool_from_manifest_with_no_topology_creates_single_node() {
        let manifest = ReactorPlacementManifest::plan(4, None);
        let pool = NumaSlabPool::from_manifest(&manifest, NumaSlabConfig::default());
        assert_eq!(pool.node_count(), 1);
    }

    #[test]
    fn numa_node_for_shard_returns_none_for_unknown() {
        let manifest = ReactorPlacementManifest::plan(2, None);
        assert!(manifest.numa_node_for_shard(0).is_some());
        assert!(manifest.numa_node_for_shard(99).is_none());
    }

    #[test]
    fn numa_slab_capacity_clamp_to_at_least_one() {
        let slab = NumaSlab::new(0, 0);
        assert_eq!(slab.capacity, 1);
    }

    #[test]
    fn cross_node_reason_code_matches() {
        assert_eq!(CrossNodeReason::LocalExhausted.as_code(), "local_exhausted");
    }

    #[test]
    fn affinity_enforcement_code_coverage() {
        assert_eq!(AffinityEnforcement::Advisory.as_code(), "advisory");
        assert_eq!(AffinityEnforcement::Strict.as_code(), "strict");
        assert_eq!(AffinityEnforcement::Disabled.as_code(), "disabled");
    }

    // ── Additional coverage for untested public APIs ──

    #[test]
    fn enqueue_hostcall_completions_batch_preserves_order() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        let completions = vec![
            (
                "c-1".to_string(),
                HostcallOutcome::Success(serde_json::json!(1)),
            ),
            (
                "c-2".to_string(),
                HostcallOutcome::Success(serde_json::json!(2)),
            ),
            (
                "c-3".to_string(),
                HostcallOutcome::Success(serde_json::json!(3)),
            ),
        ];
        sched.enqueue_hostcall_completions(completions);
        assert_eq!(sched.macrotask_count(), 3);

        // Verify FIFO order: c-1, c-2, c-3
        for expected in ["c-1", "c-2", "c-3"] {
            let task = sched.tick().expect("should have macrotask");
            match task.kind {
                MacrotaskKind::HostcallComplete { ref call_id, .. } => {
                    assert_eq!(call_id, expected);
                }
                _ => unreachable!(),
            }
        }
        assert!(sched.tick().is_none());
    }

    #[test]
    fn time_until_next_timer_positive_case() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(100));
        sched.set_timeout(50); // deadline = 150
        assert_eq!(sched.time_until_next_timer(), Some(50));

        sched.clock.advance(20); // now = 120, remaining = 30
        assert_eq!(sched.time_until_next_timer(), Some(30));
    }

    #[test]
    fn deterministic_clock_set_overrides_current_time() {
        let clock = DeterministicClock::new(0);
        assert_eq!(clock.now_ms(), 0);
        clock.advance(50);
        assert_eq!(clock.now_ms(), 50);
        clock.set(1000);
        assert_eq!(clock.now_ms(), 1000);
        clock.advance(5);
        assert_eq!(clock.now_ms(), 1005);
    }

    #[test]
    fn reactor_mesh_queue_depth_per_shard() {
        let config = ReactorMeshConfig {
            shard_count: 4,
            lane_capacity: 64,
            topology: None,
        };
        let mut mesh = ReactorMesh::new(config);

        // All shards start empty
        for shard in 0..4 {
            assert_eq!(mesh.queue_depth(shard), Some(0));
        }
        // Out of range returns None
        assert_eq!(mesh.queue_depth(99), None);

        // Enqueue events via round-robin (hits shards 0, 1, 2, 3 in order)
        for i in 0..4 {
            mesh.enqueue_event(format!("evt-{i}"), serde_json::json!(null))
                .expect("enqueue should succeed");
        }
        // Each shard should have exactly 1
        for shard in 0..4 {
            assert_eq!(mesh.queue_depth(shard), Some(1), "shard {shard} depth");
        }
    }

    #[test]
    fn reactor_mesh_shard_count_and_total_depth() {
        let config = ReactorMeshConfig {
            shard_count: 3,
            lane_capacity: 16,
            topology: None,
        };
        let mut mesh = ReactorMesh::new(config);
        assert_eq!(mesh.shard_count(), 3);
        assert_eq!(mesh.total_depth(), 0);
        assert!(!mesh.has_pending());

        mesh.enqueue_event("e1".to_string(), serde_json::json!(null))
            .unwrap();
        mesh.enqueue_event("e2".to_string(), serde_json::json!(null))
            .unwrap();
        assert_eq!(mesh.total_depth(), 2);
        assert!(mesh.has_pending());
    }

    #[test]
    fn reactor_mesh_drain_shard_out_of_range_returns_empty() {
        let config = ReactorMeshConfig {
            shard_count: 2,
            lane_capacity: 16,
            topology: None,
        };
        let mut mesh = ReactorMesh::new(config);
        mesh.enqueue_event("e1".to_string(), serde_json::json!(null))
            .unwrap();
        let drained = mesh.drain_shard(99, 10);
        assert!(drained.is_empty());
    }

    #[test]
    fn reactor_mesh_zero_shards_is_empty_and_rejects_enqueues() {
        let mut mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 0,
            lane_capacity: 16,
            topology: None,
        });

        assert_eq!(mesh.shard_count(), 0);
        assert_eq!(mesh.total_depth(), 0);
        assert!(!mesh.has_pending());
        assert_eq!(mesh.queue_depth(0), None);
        assert!(mesh.telemetry().queue_depths.is_empty());

        let err = mesh
            .enqueue_event("evt".to_string(), serde_json::json!(null))
            .expect_err("empty mesh should reject enqueues");
        assert_eq!(err.shard_id, 0);
        assert_eq!(err.depth, 0);
        assert_eq!(err.capacity, 0);
        assert_eq!(mesh.telemetry().rejected_enqueues, 1);
    }

    #[test]
    fn reactor_mesh_zero_capacity_is_empty_and_rejects_enqueues() {
        let mut mesh = ReactorMesh::new(ReactorMeshConfig {
            shard_count: 4,
            lane_capacity: 0,
            topology: None,
        });

        assert_eq!(mesh.shard_count(), 0);
        assert_eq!(mesh.total_depth(), 0);
        assert!(!mesh.has_pending());
        assert_eq!(mesh.telemetry().queue_depths, Vec::<usize>::new());

        let err = mesh
            .enqueue_hostcall_complete(
                "call".to_string(),
                HostcallOutcome::Success(serde_json::json!({"ok": true})),
            )
            .expect_err("empty mesh should reject hostcall completions");
        assert_eq!(err.shard_id, 0);
        assert_eq!(err.depth, 0);
        assert_eq!(err.capacity, 0);
        assert_eq!(mesh.telemetry().rejected_enqueues, 1);
    }

    #[test]
    fn reactor_placement_manifest_zero_shards() {
        let manifest = ReactorPlacementManifest::plan(0, None);
        assert_eq!(manifest.shard_count, 0);
        assert!(manifest.bindings.is_empty());
        assert!(manifest.fallback_reason.is_none());
    }

    #[test]
    fn reactor_placement_manifest_as_json_has_expected_fields() {
        let topology =
            ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 0), (2, 1), (3, 1)]);
        let manifest = ReactorPlacementManifest::plan(4, Some(&topology));
        let json = manifest.as_json();

        assert_eq!(json["shard_count"], 4);
        assert_eq!(json["numa_node_count"], 2);
        assert!(json["fallback_reason"].is_null());
        let bindings = json["bindings"].as_array().expect("bindings array");
        assert_eq!(bindings.len(), 4);
        for binding in bindings {
            assert!(binding.get("shard_id").is_some());
            assert!(binding.get("core_id").is_some());
            assert!(binding.get("numa_node").is_some());
        }
    }

    #[test]
    fn reactor_placement_manifest_single_node_fallback() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 0)]);
        let manifest = ReactorPlacementManifest::plan(2, Some(&topology));
        assert_eq!(
            manifest.fallback_reason,
            Some(ReactorPlacementFallbackReason::SingleNumaNode)
        );
    }

    #[test]
    fn reactor_placement_manifest_empty_topology_fallback() {
        let topology = ReactorTopologySnapshot { cores: vec![] };
        let manifest = ReactorPlacementManifest::plan(2, Some(&topology));
        assert_eq!(
            manifest.fallback_reason,
            Some(ReactorPlacementFallbackReason::TopologyEmpty)
        );
    }

    #[test]
    fn reactor_placement_fallback_reason_as_code_all_variants() {
        assert_eq!(
            ReactorPlacementFallbackReason::TopologyUnavailable.as_code(),
            "topology_unavailable"
        );
        assert_eq!(
            ReactorPlacementFallbackReason::TopologyEmpty.as_code(),
            "topology_empty"
        );
        assert_eq!(
            ReactorPlacementFallbackReason::SingleNumaNode.as_code(),
            "single_numa_node"
        );
    }

    #[test]
    fn hugepage_fallback_reason_as_code_all_variants() {
        assert_eq!(
            HugepageFallbackReason::Disabled.as_code(),
            "hugepage_disabled"
        );
        assert_eq!(
            HugepageFallbackReason::DetectionUnavailable.as_code(),
            "detection_unavailable"
        );
        assert_eq!(
            HugepageFallbackReason::InsufficientHugepages.as_code(),
            "insufficient_hugepages"
        );
        assert_eq!(
            HugepageFallbackReason::AlignmentMismatch.as_code(),
            "alignment_mismatch"
        );
    }

    #[test]
    fn numa_slab_pool_set_hugepage_status_and_node_count() {
        let manifest = ReactorPlacementManifest::plan(4, None);
        let config = NumaSlabConfig {
            slab_capacity: 4096,
            entry_size_bytes: 512,
            hugepage: HugepageConfig {
                page_size_bytes: 2 * 1024 * 1024,
                enabled: true,
            },
        };
        let mut pool = NumaSlabPool::from_manifest(&manifest, config);
        assert_eq!(pool.node_count(), 1);

        let status = HugepageStatus::evaluate(&config.hugepage, 512, 256);
        assert!(status.active);
        pool.set_hugepage_status(status);

        let telem = pool.telemetry();
        assert!(telem.hugepage_status.active);
        assert_eq!(telem.hugepage_status.free_pages, 256);
    }

    #[test]
    fn numa_slab_pool_multi_node_node_count() {
        let topology = ReactorTopologySnapshot::from_core_node_pairs(&[(0, 0), (1, 1), (2, 2)]);
        let manifest = ReactorPlacementManifest::plan(3, Some(&topology));
        let pool = NumaSlabPool::from_manifest(&manifest, NumaSlabConfig::default());
        assert_eq!(pool.node_count(), 3);
    }

    #[test]
    fn reactor_mesh_telemetry_as_json_has_expected_shape() {
        let config = ReactorMeshConfig {
            shard_count: 2,
            lane_capacity: 8,
            topology: None,
        };
        let mesh = ReactorMesh::new(config);
        let telem = mesh.telemetry();
        let json = telem.as_json();

        let depths = json["queue_depths"].as_array().expect("queue_depths");
        assert_eq!(depths.len(), 2);
        assert_eq!(json["rejected_enqueues"], 0);
        let bindings = json["shard_bindings"].as_array().expect("shard_bindings");
        assert_eq!(bindings.len(), 2);
        assert!(json.get("fallback_reason").is_some());
    }

    #[test]
    fn numa_slab_in_use_and_has_capacity() {
        let mut slab = NumaSlab::new(0, 3);
        assert_eq!(slab.in_use(), 0);
        assert!(slab.has_capacity());

        let h1 = slab.allocate().expect("alloc 1");
        assert_eq!(slab.in_use(), 1);
        assert!(slab.has_capacity());

        let h2 = slab.allocate().expect("alloc 2");
        let _h3 = slab.allocate().expect("alloc 3");
        assert_eq!(slab.in_use(), 3);
        assert!(!slab.has_capacity());
        assert!(slab.allocate().is_none());

        slab.deallocate(&h1);
        assert_eq!(slab.in_use(), 2);
        assert!(slab.has_capacity());

        slab.deallocate(&h2);
        assert_eq!(slab.in_use(), 1);
    }

    #[test]
    fn scheduler_macrotask_count_tracks_queue_size() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        assert_eq!(sched.macrotask_count(), 0);

        sched.enqueue_event("e1".to_string(), serde_json::json!(null));
        sched.enqueue_event("e2".to_string(), serde_json::json!(null));
        assert_eq!(sched.macrotask_count(), 2);

        sched.tick();
        assert_eq!(sched.macrotask_count(), 1);

        sched.tick();
        assert_eq!(sched.macrotask_count(), 0);
    }

    #[test]
    fn scheduler_timer_count_reflects_pending_timers() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        assert_eq!(sched.timer_count(), 0);

        sched.set_timeout(100);
        sched.set_timeout(200);
        assert_eq!(sched.timer_count(), 2);

        // Advance past first timer and tick to move it
        sched.clock.advance(150);
        sched.tick();
        assert_eq!(sched.timer_count(), 1);
    }

    #[test]
    fn scheduler_current_seq_advances_with_operations() {
        let mut sched = Scheduler::with_clock(DeterministicClock::new(0));
        let initial = sched.current_seq();
        assert_eq!(initial.value(), 0);

        sched.set_timeout(100); // uses one seq
        assert!(sched.current_seq().value() > initial.value());

        let after_timer = sched.current_seq();
        sched.enqueue_event("evt".to_string(), serde_json::json!(null)); // uses another seq
        assert!(sched.current_seq().value() > after_timer.value());
    }

    #[test]
    fn thread_affinity_advice_as_json_structure() {
        let advice = ThreadAffinityAdvice {
            shard_id: 2,
            recommended_core: 5,
            recommended_numa_node: 1,
            enforcement: AffinityEnforcement::Strict,
        };
        let json = advice.as_json();
        assert_eq!(json["shard_id"], 2);
        assert_eq!(json["recommended_core"], 5);
        assert_eq!(json["recommended_numa_node"], 1);
        assert_eq!(json["enforcement"], "strict");
    }

    // ── Property tests ──

    mod proptest_scheduler {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn seq_next_is_monotonic(start in 0..u64::MAX - 100) {
                let s = Seq(start);
                let n = s.next();
                assert!(n >= s, "Seq::next must be monotonically non-decreasing");
                assert!(
                    n.value() == start + 1 || start == u64::MAX,
                    "Seq::next must increment by 1 unless saturated"
                );
            }

            #[test]
            fn seq_next_saturates(start in u64::MAX - 5..=u64::MAX) {
                let s = Seq(start);
                let n = s.next();
                // Verify next() does not panic and produces a valid value.
                let _ = n.value();
                assert!(n >= s, "must be monotonic even at saturation boundary");
            }

            #[test]
            fn timer_entry_ordering_consistent_with_min_heap(
                id_a in 0..1000u64,
                id_b in 0..1000u64,
                deadline_a in 0..10000u64,
                deadline_b in 0..10000u64,
                seq_a in 0..1000u64,
                seq_b in 0..1000u64,
            ) {
                let ta = TimerEntry::new(id_a, deadline_a, Seq(seq_a));
                let tb = TimerEntry::new(id_b, deadline_b, Seq(seq_b));
                // Reversed ordering: earlier deadline = GREATER (for BinaryHeap min-heap)
                if deadline_a < deadline_b {
                    assert!(ta > tb, "earlier deadline must sort greater (min-heap)");
                } else if deadline_a > deadline_b {
                    assert!(ta < tb, "later deadline must sort less (min-heap)");
                } else if seq_a < seq_b {
                    assert!(ta > tb, "same deadline, earlier seq must sort greater");
                } else if seq_a > seq_b {
                    assert!(ta < tb, "same deadline, later seq must sort less");
                } else {
                    assert!(ta == tb, "same deadline+seq must be equal");
                }
            }

            #[test]
            fn stable_hash_is_deterministic(input in "[a-z0-9_.-]{1,64}") {
                let h1 = ReactorMesh::stable_hash(&input);
                let h2 = ReactorMesh::stable_hash(&input);
                assert!(h1 == h2, "stable_hash must be deterministic");
            }

            #[test]
            fn hash_route_returns_valid_shard(
                shard_count in 1..32usize,
                call_id in "[a-z0-9]{1,20}",
            ) {
                let config = ReactorMeshConfig {
                    shard_count,
                    lane_capacity: 16,
                    topology: None,
                };
                let mesh = ReactorMesh::new(config);
                let shard = mesh.hash_route(&call_id);
                assert!(
                    shard < mesh.shard_count(),
                    "hash_route returned {shard} >= shard_count {}",
                    mesh.shard_count(),
                );
            }

            #[test]
            fn rr_route_returns_valid_shard(
                shard_count in 1..32usize,
                iterations in 1..100usize,
            ) {
                let config = ReactorMeshConfig {
                    shard_count,
                    lane_capacity: 16,
                    topology: None,
                };
                let mut mesh = ReactorMesh::new(config);
                for _ in 0..iterations {
                    let shard = mesh.rr_route();
                    assert!(
                        shard < mesh.shard_count(),
                        "rr_route returned {shard} >= shard_count {}",
                        mesh.shard_count(),
                    );
                }
            }

            #[test]
            fn drain_global_order_is_sorted(
                shard_count in 1..8usize,
                lane_capacity in 2..16usize,
                enqueues in 1..30usize,
            ) {
                let config = ReactorMeshConfig {
                    shard_count,
                    lane_capacity,
                    topology: None,
                };
                let mut mesh = ReactorMesh::new(config);
                let mut success_count = 0usize;
                for i in 0..enqueues {
                    let call_id = format!("call_{i}");
                    let outcome = HostcallOutcome::Success(serde_json::Value::Null);
                    if mesh.enqueue_hostcall_complete(call_id, outcome).is_ok() {
                        success_count += 1;
                    }
                }
                let drained = mesh.drain_global_order(success_count);
                // Verify ascending global_seq
                for pair in drained.windows(2) {
                    assert!(
                        pair[0].global_seq < pair[1].global_seq,
                        "drain_global_order must emit ascending seq: {:?} vs {:?}",
                        pair[0].global_seq,
                        pair[1].global_seq,
                    );
                }
            }

            #[test]
            fn mesh_total_depth_bounded_by_capacity(
                shard_count in 1..8usize,
                lane_capacity in 1..16usize,
                enqueues in 0..100usize,
            ) {
                let config = ReactorMeshConfig {
                    shard_count,
                    lane_capacity,
                    topology: None,
                };
                let mut mesh = ReactorMesh::new(config);
                for i in 0..enqueues {
                    let call_id = format!("call_{i}");
                    let outcome = HostcallOutcome::Success(serde_json::Value::Null);
                    let _ = mesh.enqueue_hostcall_complete(call_id, outcome);
                }
                let max_total = shard_count * lane_capacity;
                assert!(
                    mesh.total_depth() <= max_total,
                    "total_depth {} exceeds max possible {}",
                    mesh.total_depth(),
                    max_total,
                );
            }

            #[test]
            fn scheduler_timer_cancel_idempotent(
                timer_count in 1..10usize,
                cancel_idx in 0..10usize,
            ) {
                let clock = DeterministicClock::new(0);
                let mut sched = Scheduler::with_clock(clock);
                let mut timer_ids = Vec::new();
                for i in 0..timer_count {
                    timer_ids.push(sched.set_timeout(u64::try_from(i + 1).unwrap() * 100));
                }
                if cancel_idx < timer_ids.len() {
                    let tid = timer_ids[cancel_idx];
                    let first = sched.clear_timeout(tid);
                    let second = sched.clear_timeout(tid);
                    assert!(first, "first cancel should succeed");
                    assert!(!second, "second cancel should return false");
                }
            }
        }
    }
}
