//! Hostcall queue primitives with explicit reclamation telemetry.
//!
//! The fast lane uses a bounded lock-free ring (`ArrayQueue`). When pressure
//! exceeds ring capacity, requests spill into a bounded overflow deque to
//! preserve FIFO ordering across the two lanes.

pub use crate::hostcall_s3_fifo::S3FifoFallbackReason;
use crossbeam_queue::ArrayQueue;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const HOSTCALL_FAST_RING_CAPACITY: usize = 256;
pub const HOSTCALL_OVERFLOW_CAPACITY: usize = 2_048;
const SAFE_FALLBACK_BACKLOG_MULTIPLIER: usize = 8;
const SAFE_FALLBACK_BACKLOG_MIN: usize = 32;
const S3_FIFO_GHOST_CAPACITY_MULTIPLIER: usize = 2;
const S3_FIFO_GHOST_CAPACITY_MIN: usize = 16;
const S3_FIFO_MIN_SIGNAL_SAMPLES: u64 = 32;
const S3_FIFO_MAX_SIGNALLESS_STREAK: u64 = 64;
const S3_FIFO_UNSTABLE_REJECTION_STREAK: u64 = 16;

/// BRAVO-style lock bias mode for metadata contention handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BravoBiasMode {
    /// Neutral mode. No explicit read bias is applied.
    Balanced,
    /// Prefer reader throughput under stable read-heavy contention.
    ReadBiased,
    /// Temporary writer-favoring recovery mode after starvation risk.
    WriterRecovery,
}

/// Deterministic contention signature computed from a fixed observation window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentionSignature {
    /// Window does not include enough operations for a stable decision.
    InsufficientSamples,
    /// Read-dominant contention with healthy writer behavior.
    ReadDominant,
    /// Mixed read/write contention without starvation indicators.
    MixedContention,
    /// Writer wait/timeout profile indicates starvation risk.
    WriterStarvationRisk,
    /// Write-dominant contention (or low reader pressure).
    WriteDominant,
}

/// Observation bucket consumed by the BRAVO policy state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContentionSample {
    pub read_acquires: u64,
    pub write_acquires: u64,
    pub read_wait_p95_us: u64,
    pub write_wait_p95_us: u64,
    pub write_timeouts: u64,
}

impl ContentionSample {
    #[must_use]
    pub const fn total_acquires(self) -> u64 {
        self.read_acquires.saturating_add(self.write_acquires)
    }

    #[must_use]
    pub fn read_ratio_permille(self) -> u32 {
        let total = self.total_acquires();
        if total == 0 {
            return 0;
        }
        let numerator = u128::from(self.read_acquires) * 1_000;
        let ratio = numerator / u128::from(total);
        u32::try_from(ratio).unwrap_or(1_000)
    }
}

/// Tuning knobs for deterministic BRAVO contention policy behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BravoContentionConfig {
    pub min_total_acquires: u64,
    pub read_dominant_ratio_permille: u32,
    pub mixed_ratio_floor_permille: u32,
    pub mixed_ratio_ceiling_permille: u32,
    pub writer_starvation_wait_us: u64,
    pub writer_starvation_timeouts: u64,
    pub max_consecutive_read_bias_windows: u32,
    pub writer_recovery_windows: u32,
}

impl Default for BravoContentionConfig {
    fn default() -> Self {
        Self {
            min_total_acquires: 32,
            read_dominant_ratio_permille: 800,
            mixed_ratio_floor_permille: 450,
            mixed_ratio_ceiling_permille: 799,
            writer_starvation_wait_us: 8_000,
            writer_starvation_timeouts: 2,
            max_consecutive_read_bias_windows: 5,
            writer_recovery_windows: 2,
        }
    }
}

/// One policy transition decision generated from an observation window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BravoPolicyDecision {
    pub previous_mode: BravoBiasMode,
    pub next_mode: BravoBiasMode,
    pub signature: ContentionSignature,
    pub switched: bool,
    pub rollback_triggered: bool,
}

/// Snapshot of contention policy internals for diagnostics and regression tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BravoPolicyTelemetry {
    pub mode: BravoBiasMode,
    pub transitions: u64,
    pub rollbacks: u64,
    pub windows_observed: u64,
    pub consecutive_read_bias_windows: u32,
    pub writer_recovery_remaining: u32,
    pub last_signature: ContentionSignature,
}

/// Deterministic BRAVO-style contention policy state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BravoContentionState {
    config: BravoContentionConfig,
    mode: BravoBiasMode,
    transitions: u64,
    rollbacks: u64,
    windows_observed: u64,
    consecutive_read_bias_windows: u32,
    writer_recovery_remaining: u32,
    last_signature: ContentionSignature,
}

impl BravoContentionState {
    #[must_use]
    pub const fn new(config: BravoContentionConfig) -> Self {
        Self {
            config,
            mode: BravoBiasMode::Balanced,
            transitions: 0,
            rollbacks: 0,
            windows_observed: 0,
            consecutive_read_bias_windows: 0,
            writer_recovery_remaining: 0,
            last_signature: ContentionSignature::InsufficientSamples,
        }
    }

    #[must_use]
    pub const fn mode(self) -> BravoBiasMode {
        self.mode
    }

    #[must_use]
    pub const fn snapshot(self) -> BravoPolicyTelemetry {
        BravoPolicyTelemetry {
            mode: self.mode,
            transitions: self.transitions,
            rollbacks: self.rollbacks,
            windows_observed: self.windows_observed,
            consecutive_read_bias_windows: self.consecutive_read_bias_windows,
            writer_recovery_remaining: self.writer_recovery_remaining,
            last_signature: self.last_signature,
        }
    }

    pub fn observe(&mut self, sample: ContentionSample) -> BravoPolicyDecision {
        let previous_mode = self.mode;
        let signature = Self::classify(sample, self.config);
        self.windows_observed = self.windows_observed.saturating_add(1);

        let mut rollback_triggered = false;
        match self.mode {
            BravoBiasMode::Balanced => match signature {
                ContentionSignature::WriterStarvationRisk => {
                    self.mode = BravoBiasMode::WriterRecovery;
                    self.writer_recovery_remaining = self.config.writer_recovery_windows.max(1);
                    self.consecutive_read_bias_windows = 0;
                    self.rollbacks = self.rollbacks.saturating_add(1);
                    rollback_triggered = true;
                }
                ContentionSignature::ReadDominant | ContentionSignature::MixedContention => {
                    self.mode = BravoBiasMode::ReadBiased;
                    self.consecutive_read_bias_windows = 1;
                }
                ContentionSignature::InsufficientSamples | ContentionSignature::WriteDominant => {
                    self.consecutive_read_bias_windows = 0;
                }
            },
            BravoBiasMode::ReadBiased => {
                self.consecutive_read_bias_windows =
                    self.consecutive_read_bias_windows.saturating_add(1);

                let starvation = matches!(signature, ContentionSignature::WriterStarvationRisk);
                let fairness_budget_exhausted = self.consecutive_read_bias_windows
                    >= self.config.max_consecutive_read_bias_windows.max(1);

                if starvation || fairness_budget_exhausted {
                    self.mode = BravoBiasMode::WriterRecovery;
                    self.writer_recovery_remaining = self.config.writer_recovery_windows.max(1);
                    self.consecutive_read_bias_windows = 0;
                    rollback_triggered = starvation;
                    if starvation {
                        self.rollbacks = self.rollbacks.saturating_add(1);
                    }
                } else if matches!(
                    signature,
                    ContentionSignature::InsufficientSamples | ContentionSignature::WriteDominant
                ) {
                    self.mode = BravoBiasMode::Balanced;
                    self.consecutive_read_bias_windows = 0;
                }
            }
            BravoBiasMode::WriterRecovery => {
                self.consecutive_read_bias_windows = 0;
                if matches!(signature, ContentionSignature::WriterStarvationRisk) {
                    self.writer_recovery_remaining = self.config.writer_recovery_windows.max(1);
                } else if self.writer_recovery_remaining > 0 {
                    self.writer_recovery_remaining -= 1;
                }
                if self.writer_recovery_remaining == 0 {
                    self.mode = BravoBiasMode::Balanced;
                }
            }
        }

        let switched = Self::mode_changed(self.mode, previous_mode);
        if switched {
            self.transitions = self.transitions.saturating_add(1);
        }
        self.last_signature = signature;

        BravoPolicyDecision {
            previous_mode,
            next_mode: self.mode,
            signature,
            switched,
            rollback_triggered,
        }
    }

    const fn mode_changed(current: BravoBiasMode, previous: BravoBiasMode) -> bool {
        !matches!(
            (current, previous),
            (BravoBiasMode::Balanced, BravoBiasMode::Balanced)
                | (BravoBiasMode::ReadBiased, BravoBiasMode::ReadBiased)
                | (BravoBiasMode::WriterRecovery, BravoBiasMode::WriterRecovery)
        )
    }

    #[must_use]
    pub fn classify(
        sample: ContentionSample,
        config: BravoContentionConfig,
    ) -> ContentionSignature {
        if sample.total_acquires() < config.min_total_acquires {
            return ContentionSignature::InsufficientSamples;
        }

        if sample.write_wait_p95_us >= config.writer_starvation_wait_us
            || sample.write_timeouts >= config.writer_starvation_timeouts
        {
            return ContentionSignature::WriterStarvationRisk;
        }

        let read_ratio = sample.read_ratio_permille();
        let read_dominant_floor = config.read_dominant_ratio_permille.min(1_000);
        if read_ratio >= read_dominant_floor {
            return ContentionSignature::ReadDominant;
        }

        let mixed_floor = config.mixed_ratio_floor_permille.min(1_000);
        let mixed_ceiling = config
            .mixed_ratio_ceiling_permille
            .clamp(mixed_floor, 1_000);
        if read_ratio >= mixed_floor && read_ratio <= mixed_ceiling {
            return ContentionSignature::MixedContention;
        }

        ContentionSignature::WriteDominant
    }
}

impl Default for BravoContentionState {
    fn default() -> Self {
        Self::new(BravoContentionConfig::default())
    }
}

/// Optional per-request tenant key used for fairness/admission accounting.
///
/// Implement this for queue payloads that can expose extension-level identity.
/// Primitive test payloads may use the default `None` implementation.
pub trait QueueTenant {
    #[must_use]
    fn tenant_key(&self) -> Option<&str> {
        None
    }
}

macro_rules! impl_queue_tenant_none {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl QueueTenant for $ty {}
        )+
    };
}

impl_queue_tenant_none!(
    (),
    bool,
    char,
    u8,
    u16,
    u32,
    u64,
    usize,
    i8,
    i16,
    i32,
    i64,
    isize,
    String,
);

/// Runtime mode for S3-FIFO-inspired queue admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S3FifoMode {
    Active,
    ConservativeFifo,
}

/// Deterministic admission configuration for S3-FIFO-inspired behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S3FifoConfig {
    pub tenant_budget: usize,
    pub ghost_capacity: usize,
    pub min_signal_samples: u64,
    pub max_signalless_streak: u64,
    pub unstable_rejection_streak: u64,
}

impl S3FifoConfig {
    #[must_use]
    pub fn from_capacities(fast_capacity: usize, overflow_capacity: usize) -> Self {
        let tenant_budget = (overflow_capacity / 2).max(1);
        let ghost_capacity = fast_capacity
            .saturating_add(overflow_capacity)
            .saturating_mul(S3_FIFO_GHOST_CAPACITY_MULTIPLIER)
            .max(S3_FIFO_GHOST_CAPACITY_MIN);
        Self {
            tenant_budget,
            ghost_capacity,
            min_signal_samples: S3_FIFO_MIN_SIGNAL_SAMPLES,
            max_signalless_streak: S3_FIFO_MAX_SIGNALLESS_STREAK,
            unstable_rejection_streak: S3_FIFO_UNSTABLE_REJECTION_STREAK,
        }
    }
}

/// Lightweight diagnostics snapshot for S3-FIFO admission internals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct S3FifoTelemetry {
    pub mode: S3FifoMode,
    pub fallback_reason: Option<S3FifoFallbackReason>,
    pub ghost_depth: usize,
    pub ghost_hits_total: u64,
    pub fairness_rejected_total: u64,
    pub signal_samples: u64,
    pub signalless_streak: u64,
    pub fallback_transitions: u64,
    pub tenant_budget: usize,
    pub active_tenants: usize,
}

#[derive(Debug, Clone)]
struct S3FifoState {
    config: S3FifoConfig,
    mode: S3FifoMode,
    fallback_reason: Option<S3FifoFallbackReason>,
    ghost_order: BTreeMap<u64, String>,
    ghost_lookup: BTreeMap<String, u64>,
    ghost_gen: u64,
    tenant_backlog: BTreeMap<String, usize>,
    ghost_hits_total: u64,
    fairness_rejected_total: u64,
    signal_samples: u64,
    signalless_streak: u64,
    unstable_rejection_streak: u64,
    fallback_transitions: u64,
}

impl S3FifoState {
    #[must_use]
    const fn new(config: S3FifoConfig) -> Self {
        Self {
            config,
            mode: S3FifoMode::Active,
            fallback_reason: None,
            ghost_order: BTreeMap::new(),
            ghost_lookup: BTreeMap::new(),
            ghost_gen: 0,
            tenant_backlog: BTreeMap::new(),
            ghost_hits_total: 0,
            fairness_rejected_total: 0,
            signal_samples: 0,
            signalless_streak: 0,
            unstable_rejection_streak: 0,
            fallback_transitions: 0,
        }
    }

    fn observe_signal(&mut self, tenant_key: Option<&str>) {
        if self.mode != S3FifoMode::Active {
            return;
        }
        if tenant_key.is_some() {
            self.signal_samples = self.signal_samples.saturating_add(1);
            self.signalless_streak = 0;
        } else {
            self.signalless_streak = self.signalless_streak.saturating_add(1);
            if self.signalless_streak >= self.config.max_signalless_streak
                && self.signal_samples < self.config.min_signal_samples
            {
                self.transition_to_fallback(S3FifoFallbackReason::SignalQualityInsufficient);
            }
        }
    }

    fn allow_main_admission(&mut self, tenant_key: Option<&str>) -> bool {
        if self.mode != S3FifoMode::Active {
            return true;
        }
        let Some(tenant_key) = tenant_key else {
            return true;
        };
        let backlog = self.tenant_backlog.get(tenant_key).copied().unwrap_or(0);
        if backlog < self.config.tenant_budget {
            return true;
        }
        if self.consume_ghost_hit(tenant_key) {
            self.unstable_rejection_streak = 0;
            return true;
        }

        self.fairness_rejected_total = self.fairness_rejected_total.saturating_add(1);
        self.unstable_rejection_streak = self.unstable_rejection_streak.saturating_add(1);
        self.record_ghost(tenant_key);
        if self.unstable_rejection_streak >= self.config.unstable_rejection_streak {
            self.transition_to_fallback(S3FifoFallbackReason::FairnessInstability);
        }
        false
    }

    fn on_main_enqueued(&mut self, tenant_key: Option<&str>) {
        if self.mode != S3FifoMode::Active {
            return;
        }
        if let Some(tenant_key) = tenant_key {
            if let Some(backlog) = self.tenant_backlog.get_mut(tenant_key) {
                *backlog = backlog.saturating_add(1);
            } else {
                self.tenant_backlog.insert(tenant_key.to_string(), 1);
            }
        }
        self.unstable_rejection_streak = 0;
    }

    fn on_main_dequeued(&mut self, tenant_key: Option<&str>) {
        if self.mode != S3FifoMode::Active {
            return;
        }
        if let Some(tenant_key) = tenant_key {
            if let Some(backlog) = self.tenant_backlog.get_mut(tenant_key) {
                *backlog = backlog.saturating_sub(1);
                if *backlog == 0 {
                    self.tenant_backlog.remove(tenant_key);
                }
            }
            self.record_ghost(tenant_key);
        }
    }

    fn on_main_overflow_reject(&mut self, tenant_key: Option<&str>) {
        if self.mode != S3FifoMode::Active {
            return;
        }
        if let Some(tenant_key) = tenant_key {
            self.record_ghost(tenant_key);
        }
        self.unstable_rejection_streak = self.unstable_rejection_streak.saturating_add(1);
        if self.unstable_rejection_streak >= self.config.unstable_rejection_streak {
            self.transition_to_fallback(S3FifoFallbackReason::FairnessInstability);
        }
    }

    fn transition_to_fallback(&mut self, reason: S3FifoFallbackReason) {
        if self.mode == S3FifoMode::ConservativeFifo {
            return;
        }
        self.mode = S3FifoMode::ConservativeFifo;
        self.fallback_reason = Some(reason);
        self.fallback_transitions = self.fallback_transitions.saturating_add(1);
        self.ghost_order.clear();
        self.ghost_lookup.clear();
        self.ghost_gen = 0;
        self.tenant_backlog.clear();
    }

    fn consume_ghost_hit(&mut self, tenant_key: &str) -> bool {
        if let Some(generation) = self.ghost_lookup.remove(tenant_key) {
            self.ghost_order.remove(&generation);
            self.ghost_hits_total = self.ghost_hits_total.saturating_add(1);
            true
        } else {
            false
        }
    }

    fn record_ghost(&mut self, tenant_key: &str) {
        if tenant_key.is_empty() {
            return;
        }

        // Prevent generation overflow from corrupting the ghost order map.
        if self.ghost_gen == u64::MAX {
            self.ghost_gen = 0;
            self.ghost_order.clear();
            self.ghost_lookup.clear();
        }

        self.ghost_gen = self.ghost_gen.saturating_add(1);

        if let Some(gen_mut) = self.ghost_lookup.get_mut(tenant_key) {
            let old_gen = *gen_mut;
            *gen_mut = self.ghost_gen;
            if let Some(k_reused) = self.ghost_order.remove(&old_gen) {
                self.ghost_order.insert(self.ghost_gen, k_reused);
            }
        } else {
            let key_owned = tenant_key.to_string();
            self.ghost_lookup.insert(key_owned.clone(), self.ghost_gen);
            self.ghost_order.insert(self.ghost_gen, key_owned);
        }

        while self.ghost_lookup.len() > self.config.ghost_capacity {
            if let Some((_, popped_key)) = self.ghost_order.pop_first() {
                self.ghost_lookup.remove(&popped_key);
            } else {
                break;
            }
        }
    }

    #[must_use]
    fn snapshot(&self) -> S3FifoTelemetry {
        S3FifoTelemetry {
            mode: self.mode,
            fallback_reason: self.fallback_reason,
            ghost_depth: self.ghost_lookup.len(),
            ghost_hits_total: self.ghost_hits_total,
            fairness_rejected_total: self.fairness_rejected_total,
            signal_samples: self.signal_samples,
            signalless_streak: self.signalless_streak,
            fallback_transitions: self.fallback_transitions,
            tenant_budget: self.config.tenant_budget,
            active_tenants: self.tenant_backlog.len(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostcallQueueMode {
    /// Use epoch-based retirement bookkeeping.
    Ebr,
    /// Disable EBR retirement and drop popped nodes immediately.
    SafeFallback,
}

impl HostcallQueueMode {
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var("PI_HOSTCALL_QUEUE_RECLAIMER")
            .ok()
            .as_deref()
            .and_then(Self::parse)
            .unwrap_or(Self::SafeFallback)
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ebr" | "epoch" | "epoch-based" => Some(Self::Ebr),
            "fallback" | "safe-fallback" | "off" | "disabled" | "legacy" => {
                Some(Self::SafeFallback)
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostcallQueueEnqueueResult {
    FastPath { depth: usize },
    OverflowPath { depth: usize, overflow_depth: usize },
    Rejected { depth: usize, overflow_depth: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostcallQueueTelemetry {
    pub fast_depth: usize,
    pub overflow_depth: usize,
    pub total_depth: usize,
    pub max_depth_seen: usize,
    pub overflow_enqueued_total: u64,
    pub overflow_rejected_total: u64,
    pub fast_capacity: usize,
    pub overflow_capacity: usize,
    pub reclamation_mode: HostcallQueueMode,
    pub retired_backlog: usize,
    pub reclaimed_total: u64,
    pub current_epoch: u64,
    pub epoch_lag: u64,
    pub max_epoch_lag: u64,
    pub reclamation_latency_max_epochs: u64,
    pub fallback_transitions: u64,
    pub active_epoch_pins: usize,
    pub bravo_mode: BravoBiasMode,
    pub bravo_transitions: u64,
    pub bravo_rollbacks: u64,
    pub bravo_consecutive_read_bias_windows: u32,
    pub bravo_writer_recovery_remaining: u32,
    pub bravo_last_signature: ContentionSignature,
    pub s3fifo_mode: S3FifoMode,
    pub s3fifo_fallback_reason: Option<S3FifoFallbackReason>,
    pub s3fifo_ghost_depth: usize,
    pub s3fifo_ghost_hits_total: u64,
    pub s3fifo_fairness_rejected_total: u64,
    pub s3fifo_signal_samples: u64,
    pub s3fifo_signalless_streak: u64,
    pub s3fifo_fallback_transitions: u64,
    pub s3fifo_tenant_budget: usize,
    pub s3fifo_active_tenants: usize,
}

#[derive(Debug)]
struct RetiredNode<T> {
    value: T,
    retired_epoch: u64,
}

#[derive(Debug)]
pub struct HostcallEpochPin {
    active_epoch_pins: Arc<AtomicUsize>,
}

impl Drop for HostcallEpochPin {
    fn drop(&mut self) {
        let previous = self.active_epoch_pins.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(previous > 0, "epoch pin underflow");
    }
}

#[derive(Debug)]
pub struct HostcallRequestQueue<T: Clone + QueueTenant> {
    fast: ArrayQueue<T>,
    fast_capacity: usize,
    overflow: VecDeque<T>,
    overflow_enqueued_total: u64,
    overflow_rejected_total: u64,
    max_depth_seen: usize,
    overflow_capacity: usize,
    reclamation_mode: HostcallQueueMode,
    active_epoch_pins: Arc<AtomicUsize>,
    current_epoch: u64,
    retired: VecDeque<RetiredNode<T>>,
    reclaimed_total: u64,
    max_epoch_lag: u64,
    reclamation_latency_max_epochs: u64,
    fallback_transitions: u64,
    safe_fallback_backlog_threshold: usize,
    contention_policy: BravoContentionState,
    s3fifo: S3FifoState,
}

impl<T: Clone + QueueTenant> HostcallRequestQueue<T> {
    #[must_use]
    pub fn with_capacities(fast_capacity: usize, overflow_capacity: usize) -> Self {
        Self::with_mode(
            fast_capacity,
            overflow_capacity,
            HostcallQueueMode::from_env(),
        )
    }

    #[must_use]
    pub fn with_mode(
        fast_capacity: usize,
        overflow_capacity: usize,
        reclamation_mode: HostcallQueueMode,
    ) -> Self {
        let fast_capacity = fast_capacity.max(1);
        let overflow_capacity = overflow_capacity.max(1);
        let safe_fallback_backlog_threshold = fast_capacity
            .saturating_add(overflow_capacity)
            .saturating_mul(SAFE_FALLBACK_BACKLOG_MULTIPLIER)
            .max(SAFE_FALLBACK_BACKLOG_MIN);
        let s3fifo = S3FifoState::new(S3FifoConfig::from_capacities(
            fast_capacity,
            overflow_capacity,
        ));
        Self {
            fast: ArrayQueue::new(fast_capacity),
            fast_capacity,
            overflow: VecDeque::new(),
            overflow_enqueued_total: 0,
            overflow_rejected_total: 0,
            max_depth_seen: 0,
            overflow_capacity,
            reclamation_mode,
            active_epoch_pins: Arc::new(AtomicUsize::new(0)),
            current_epoch: 0,
            retired: VecDeque::new(),
            reclaimed_total: 0,
            max_epoch_lag: 0,
            reclamation_latency_max_epochs: 0,
            fallback_transitions: 0,
            safe_fallback_backlog_threshold,
            contention_policy: BravoContentionState::default(),
            s3fifo,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.fast.len() + self.overflow.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fast.is_empty() && self.overflow.is_empty()
    }

    #[must_use]
    pub const fn reclamation_mode(&self) -> HostcallQueueMode {
        self.reclamation_mode
    }

    pub fn pin_epoch(&self) -> HostcallEpochPin {
        self.active_epoch_pins.fetch_add(1, Ordering::SeqCst);
        HostcallEpochPin {
            active_epoch_pins: Arc::clone(&self.active_epoch_pins),
        }
    }

    pub fn clear(&mut self) {
        while self.fast.pop().is_some() {}
        self.overflow.clear();
        self.overflow_enqueued_total = 0;
        self.overflow_rejected_total = 0;
        self.max_depth_seen = 0;
        self.current_epoch = 0;
        self.retired.clear();
        self.reclaimed_total = 0;
        self.max_epoch_lag = 0;
        self.reclamation_latency_max_epochs = 0;
        self.fallback_transitions = 0;
        self.contention_policy = BravoContentionState::default();
        self.s3fifo = S3FifoState::new(S3FifoConfig::from_capacities(
            self.fast_capacity,
            self.overflow_capacity,
        ));
    }

    pub fn push_back(&mut self, request: T) -> HostcallQueueEnqueueResult {
        self.s3fifo.observe_signal(request.tenant_key());
        let mut request = request;

        // Preserve FIFO across lanes by pinning to overflow once spill begins.
        if self.overflow.is_empty() {
            match self.fast.push(request) {
                Ok(()) => {
                    self.bump_epoch();
                    self.try_reclaim();
                    let depth = self.len();
                    self.max_depth_seen = self.max_depth_seen.max(depth);
                    tracing::debug!(
                        target: "pi.extensions.hostcall_queue",
                        event = "hostcall_queue.enqueue",
                        reason = "small_tier",
                        depth,
                        overflow_depth = self.overflow.len(),
                        "hostcall admitted to fast tier"
                    );
                    return HostcallQueueEnqueueResult::FastPath { depth };
                }
                Err(returned) => request = returned,
            }
        }

        let tenant_key = request.tenant_key().map(std::borrow::ToOwned::to_owned);

        if !self.s3fifo.allow_main_admission(tenant_key.as_deref()) {
            self.overflow_rejected_total = self.overflow_rejected_total.saturating_add(1);
            tracing::debug!(
                target: "pi.extensions.hostcall_queue",
                event = "hostcall_queue.reject",
                reason = "fairness_budget",
                depth = self.len(),
                overflow_depth = self.overflow.len(),
                s3fifo_mode = ?self.s3fifo.snapshot().mode,
                "hostcall rejected by S3-FIFO fairness budget"
            );
            return HostcallQueueEnqueueResult::Rejected {
                depth: self.len(),
                overflow_depth: self.overflow.len(),
            };
        }

        if self.overflow.len() < self.overflow_capacity {
            self.overflow.push_back(request);
            self.overflow_enqueued_total = self.overflow_enqueued_total.saturating_add(1);
            self.s3fifo.on_main_enqueued(tenant_key.as_deref());
            self.bump_epoch();
            self.try_reclaim();
            let depth = self.len();
            let overflow_depth = self.overflow.len();
            self.max_depth_seen = self.max_depth_seen.max(depth);
            tracing::debug!(
                target: "pi.extensions.hostcall_queue",
                event = "hostcall_queue.enqueue",
                reason = "main_tier",
                depth,
                overflow_depth,
                "hostcall admitted to overflow/main tier"
            );
            return HostcallQueueEnqueueResult::OverflowPath {
                depth,
                overflow_depth,
            };
        }

        self.overflow_rejected_total = self.overflow_rejected_total.saturating_add(1);
        self.s3fifo.on_main_overflow_reject(tenant_key.as_deref());
        tracing::debug!(
            target: "pi.extensions.hostcall_queue",
            event = "hostcall_queue.reject",
            reason = "overflow_capacity",
            depth = self.len(),
            overflow_depth = self.overflow.len(),
            "hostcall rejected because overflow queue reached capacity"
        );
        HostcallQueueEnqueueResult::Rejected {
            depth: self.len(),
            overflow_depth: self.overflow.len(),
        }
    }

    fn pop_front(&mut self) -> Option<T> {
        let value = if let Some(value) = self.fast.pop() {
            value
        } else {
            let value = self.overflow.pop_front()?;
            let tenant_key = value.tenant_key().map(std::borrow::ToOwned::to_owned);
            self.s3fifo.on_main_dequeued(tenant_key.as_deref());
            value
        };
        self.bump_epoch();
        if self.reclamation_mode == HostcallQueueMode::Ebr {
            self.retire_for_reclamation(value.clone());
        }
        self.try_reclaim();
        Some(value)
    }

    pub fn drain_all(&mut self) -> VecDeque<T> {
        let mut drained = VecDeque::with_capacity(self.len());
        while let Some(request) = self.pop_front() {
            drained.push_back(request);
        }
        drained
    }

    /// Explicit reclamation point used by tests and slow-path maintenance.
    pub fn force_reclaim(&mut self) {
        self.bump_epoch();
        self.try_reclaim();
    }

    /// Immediately disable EBR and switch to the safe fallback mode.
    pub fn force_safe_fallback(&mut self) {
        self.transition_to_safe_fallback();
    }

    /// Feed one deterministic contention observation window into the BRAVO
    /// policy controller.
    pub fn observe_contention_window(&mut self, sample: ContentionSample) -> BravoPolicyDecision {
        self.contention_policy.observe(sample)
    }

    #[must_use]
    pub const fn contention_policy_snapshot(&self) -> BravoPolicyTelemetry {
        self.contention_policy.snapshot()
    }

    #[must_use]
    pub fn snapshot(&self) -> HostcallQueueTelemetry {
        let epoch_lag = self.retired.front().map_or(0, |node| {
            self.current_epoch.saturating_sub(node.retired_epoch)
        });
        let contention = self.contention_policy.snapshot();
        let s3fifo = self.s3fifo.snapshot();

        HostcallQueueTelemetry {
            fast_depth: self.fast.len(),
            overflow_depth: self.overflow.len(),
            total_depth: self.len(),
            max_depth_seen: self.max_depth_seen,
            overflow_enqueued_total: self.overflow_enqueued_total,
            overflow_rejected_total: self.overflow_rejected_total,
            fast_capacity: self.fast_capacity,
            overflow_capacity: self.overflow_capacity,
            reclamation_mode: self.reclamation_mode,
            retired_backlog: self.retired.len(),
            reclaimed_total: self.reclaimed_total,
            current_epoch: self.current_epoch,
            epoch_lag,
            max_epoch_lag: self.max_epoch_lag,
            reclamation_latency_max_epochs: self.reclamation_latency_max_epochs,
            fallback_transitions: self.fallback_transitions,
            active_epoch_pins: self.active_epoch_pins.load(Ordering::SeqCst),
            bravo_mode: contention.mode,
            bravo_transitions: contention.transitions,
            bravo_rollbacks: contention.rollbacks,
            bravo_consecutive_read_bias_windows: contention.consecutive_read_bias_windows,
            bravo_writer_recovery_remaining: contention.writer_recovery_remaining,
            bravo_last_signature: contention.last_signature,
            s3fifo_mode: s3fifo.mode,
            s3fifo_fallback_reason: s3fifo.fallback_reason,
            s3fifo_ghost_depth: s3fifo.ghost_depth,
            s3fifo_ghost_hits_total: s3fifo.ghost_hits_total,
            s3fifo_fairness_rejected_total: s3fifo.fairness_rejected_total,
            s3fifo_signal_samples: s3fifo.signal_samples,
            s3fifo_signalless_streak: s3fifo.signalless_streak,
            s3fifo_fallback_transitions: s3fifo.fallback_transitions,
            s3fifo_tenant_budget: s3fifo.tenant_budget,
            s3fifo_active_tenants: s3fifo.active_tenants,
        }
    }

    const fn bump_epoch(&mut self) {
        self.current_epoch = self.current_epoch.saturating_add(1);
    }

    fn retire_for_reclamation(&mut self, value: T) {
        self.retired.push_back(RetiredNode {
            value,
            retired_epoch: self.current_epoch,
        });
    }

    fn transition_to_safe_fallback(&mut self) {
        if self.reclamation_mode == HostcallQueueMode::SafeFallback {
            return;
        }
        self.reclamation_mode = HostcallQueueMode::SafeFallback;
        self.fallback_transitions = self.fallback_transitions.saturating_add(1);
        self.retired.clear();
    }

    fn try_reclaim(&mut self) {
        if self.reclamation_mode != HostcallQueueMode::Ebr {
            return;
        }

        let active = self.active_epoch_pins.load(Ordering::SeqCst);
        if active > 0 {
            if let Some(front) = self.retired.front() {
                let lag = self.current_epoch.saturating_sub(front.retired_epoch);
                self.max_epoch_lag = self.max_epoch_lag.max(lag);
            }
            if self.retired.len() > self.safe_fallback_backlog_threshold {
                self.transition_to_safe_fallback();
            }
            return;
        }

        while self
            .retired
            .front()
            .is_some_and(|front| front.retired_epoch < self.current_epoch)
        {
            if let Some(node) = self.retired.pop_front() {
                let latency = self.current_epoch.saturating_sub(node.retired_epoch);
                self.reclamation_latency_max_epochs =
                    self.reclamation_latency_max_epochs.max(latency);
                self.reclaimed_total = self.reclaimed_total.saturating_add(1);
                drop(node.value);
            }
        }
    }
}

impl<T: Clone + QueueTenant> Default for HostcallRequestQueue<T> {
    fn default() -> Self {
        Self::with_capacities(HOSTCALL_FAST_RING_CAPACITY, HOSTCALL_OVERFLOW_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deterministic_config() -> BravoContentionConfig {
        BravoContentionConfig {
            min_total_acquires: 10,
            read_dominant_ratio_permille: 750,
            mixed_ratio_floor_permille: 400,
            mixed_ratio_ceiling_permille: 749,
            writer_starvation_wait_us: 4_000,
            writer_starvation_timeouts: 2,
            max_consecutive_read_bias_windows: 3,
            writer_recovery_windows: 2,
        }
    }

    fn sample(
        reads: u64,
        writes: u64,
        read_wait_p95_us: u64,
        write_wait_p95_us: u64,
        write_timeouts: u64,
    ) -> ContentionSample {
        ContentionSample {
            read_acquires: reads,
            write_acquires: writes,
            read_wait_p95_us,
            write_wait_p95_us,
            write_timeouts,
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TenantRequest {
        tenant: Option<&'static str>,
        value: u8,
    }

    impl QueueTenant for TenantRequest {
        fn tenant_key(&self) -> Option<&str> {
            self.tenant
        }
    }

    fn drive_signal_quality_fallback(queue: &mut HostcallRequestQueue<TenantRequest>, seed: u8) {
        for offset in 0..96_u8 {
            let _ = queue.push_back(TenantRequest {
                tenant: None,
                value: seed.wrapping_add(offset),
            });
            let _ = queue.drain_all();
        }
    }

    #[test]
    fn hostcall_queue_mode_parsing_supports_ebr_and_fallback() {
        assert_eq!(
            HostcallQueueMode::parse("ebr"),
            Some(HostcallQueueMode::Ebr)
        );
        assert_eq!(
            HostcallQueueMode::parse("safe-fallback"),
            Some(HostcallQueueMode::SafeFallback)
        );
        assert_eq!(HostcallQueueMode::parse("nope"), None);
    }

    #[test]
    fn contention_classifier_flags_writer_starvation_deterministically() {
        let config = deterministic_config();
        let starvation = sample(90, 10, 100, 10_000, 3);
        let signature = BravoContentionState::classify(starvation, config);
        assert_eq!(signature, ContentionSignature::WriterStarvationRisk);

        let read_dominant = sample(90, 10, 100, 300, 0);
        let signature = BravoContentionState::classify(read_dominant, config);
        assert_eq!(signature, ContentionSignature::ReadDominant);
    }

    #[test]
    fn bravo_policy_rolls_back_on_starvation_and_recovers() {
        let mut policy = BravoContentionState::new(deterministic_config());

        let first = policy.observe(sample(80, 20, 120, 500, 0));
        assert_eq!(first.previous_mode, BravoBiasMode::Balanced);
        assert_eq!(first.next_mode, BravoBiasMode::ReadBiased);
        assert_eq!(first.signature, ContentionSignature::ReadDominant);
        assert!(first.switched);

        let second = policy.observe(sample(85, 15, 100, 8_500, 3));
        assert_eq!(second.previous_mode, BravoBiasMode::ReadBiased);
        assert_eq!(second.next_mode, BravoBiasMode::WriterRecovery);
        assert_eq!(second.signature, ContentionSignature::WriterStarvationRisk);
        assert!(second.rollback_triggered);

        let third = policy.observe(sample(30, 70, 200, 500, 0));
        assert_eq!(third.next_mode, BravoBiasMode::WriterRecovery);
        assert!(!third.switched);

        let fourth = policy.observe(sample(35, 65, 200, 450, 0));
        assert_eq!(fourth.next_mode, BravoBiasMode::Balanced);
        assert!(fourth.switched);

        let telemetry = policy.snapshot();
        assert_eq!(telemetry.mode, BravoBiasMode::Balanced);
        assert!(telemetry.rollbacks >= 1);
        assert!(telemetry.transitions >= 3);
    }

    #[test]
    fn bravo_writer_recovery_refreshes_on_repeated_starvation_then_exits_cleanly() {
        let mut config = deterministic_config();
        config.writer_recovery_windows = 3;
        let mut policy = BravoContentionState::new(config);

        let _ = policy.observe(sample(85, 15, 100, 400, 0));
        let first_starvation = policy.observe(sample(80, 20, 100, 9_000, 3));
        assert_eq!(first_starvation.previous_mode, BravoBiasMode::ReadBiased);
        assert_eq!(first_starvation.next_mode, BravoBiasMode::WriterRecovery);
        assert!(first_starvation.rollback_triggered);
        assert_eq!(
            first_starvation.signature,
            ContentionSignature::WriterStarvationRisk
        );
        assert_eq!(policy.snapshot().writer_recovery_remaining, 3);

        let second_starvation = policy.observe(sample(75, 25, 100, 8_500, 2));
        assert_eq!(
            second_starvation.previous_mode,
            BravoBiasMode::WriterRecovery
        );
        assert_eq!(second_starvation.next_mode, BravoBiasMode::WriterRecovery);
        assert!(!second_starvation.switched);
        assert!(!second_starvation.rollback_triggered);
        assert_eq!(policy.snapshot().writer_recovery_remaining, 3);

        let _ = policy.observe(sample(45, 55, 150, 450, 0));
        assert_eq!(policy.snapshot().writer_recovery_remaining, 2);
        let _ = policy.observe(sample(45, 55, 150, 450, 0));
        assert_eq!(policy.snapshot().writer_recovery_remaining, 1);

        let exit = policy.observe(sample(45, 55, 150, 450, 0));
        assert_eq!(exit.previous_mode, BravoBiasMode::WriterRecovery);
        assert_eq!(exit.next_mode, BravoBiasMode::Balanced);
        assert!(exit.switched);
        assert!(!exit.rollback_triggered);

        let telemetry = policy.snapshot();
        assert_eq!(telemetry.mode, BravoBiasMode::Balanced);
        assert_eq!(telemetry.writer_recovery_remaining, 0);
        assert_eq!(
            telemetry.last_signature,
            ContentionSignature::MixedContention
        );
        assert_eq!(telemetry.rollbacks, 1);
        assert!(telemetry.transitions >= 3);
    }

    #[test]
    fn bravo_zero_window_config_clamps_to_single_recovery_window() {
        let mut config = deterministic_config();
        config.max_consecutive_read_bias_windows = 0;
        config.writer_recovery_windows = 0;
        let mut policy = BravoContentionState::new(config);

        let first = policy.observe(sample(85, 15, 100, 300, 0));
        assert_eq!(first.next_mode, BravoBiasMode::ReadBiased);
        assert_eq!(policy.snapshot().consecutive_read_bias_windows, 1);

        let second = policy.observe(sample(86, 14, 100, 320, 0));
        assert_eq!(second.previous_mode, BravoBiasMode::ReadBiased);
        assert_eq!(second.next_mode, BravoBiasMode::WriterRecovery);
        assert_eq!(second.signature, ContentionSignature::ReadDominant);
        assert!(!second.rollback_triggered);

        let recovery = policy.snapshot();
        assert_eq!(recovery.writer_recovery_remaining, 1);
        assert_eq!(recovery.consecutive_read_bias_windows, 0);

        let third = policy.observe(sample(45, 55, 120, 450, 0));
        assert_eq!(third.previous_mode, BravoBiasMode::WriterRecovery);
        assert_eq!(third.next_mode, BravoBiasMode::Balanced);
        assert!(third.switched);
        assert_eq!(policy.snapshot().writer_recovery_remaining, 0);
    }

    #[test]
    fn bravo_writer_recovery_starvation_refresh_clamps_to_one_when_config_is_zero() {
        let mut config = deterministic_config();
        config.writer_recovery_windows = 0;
        let mut policy = BravoContentionState::new(config);

        let _ = policy.observe(sample(82, 18, 100, 280, 0));
        let starvation = policy.observe(sample(75, 25, 100, 8_200, 3));
        assert_eq!(starvation.previous_mode, BravoBiasMode::ReadBiased);
        assert_eq!(starvation.next_mode, BravoBiasMode::WriterRecovery);
        assert!(starvation.rollback_triggered);
        assert_eq!(policy.snapshot().writer_recovery_remaining, 1);

        let repeated_starvation = policy.observe(sample(70, 30, 100, 8_600, 2));
        assert_eq!(
            repeated_starvation.previous_mode,
            BravoBiasMode::WriterRecovery
        );
        assert_eq!(repeated_starvation.next_mode, BravoBiasMode::WriterRecovery);
        assert!(!repeated_starvation.rollback_triggered);
        assert_eq!(
            repeated_starvation.signature,
            ContentionSignature::WriterStarvationRisk
        );
        assert_eq!(policy.snapshot().writer_recovery_remaining, 1);

        let exit = policy.observe(sample(48, 52, 140, 420, 0));
        assert_eq!(exit.previous_mode, BravoBiasMode::WriterRecovery);
        assert_eq!(exit.next_mode, BravoBiasMode::Balanced);
        assert_eq!(policy.snapshot().writer_recovery_remaining, 0);
    }

    #[test]
    fn bravo_policy_enforces_writer_fairness_budget() {
        let mut config = deterministic_config();
        config.max_consecutive_read_bias_windows = 2;
        config.writer_recovery_windows = 1;
        let mut policy = BravoContentionState::new(config);

        let _ = policy.observe(sample(80, 20, 100, 250, 0));
        let second = policy.observe(sample(85, 15, 100, 260, 0));
        assert_eq!(second.next_mode, BravoBiasMode::WriterRecovery);
        assert_eq!(second.signature, ContentionSignature::ReadDominant);
        assert!(!second.rollback_triggered);

        let recovery = policy.observe(sample(40, 60, 150, 400, 0));
        assert_eq!(recovery.next_mode, BravoBiasMode::Balanced);

        let telemetry = policy.snapshot();
        assert_eq!(telemetry.mode, BravoBiasMode::Balanced);
        assert_eq!(telemetry.writer_recovery_remaining, 0);
    }

    #[test]
    fn queue_snapshot_exposes_bravo_policy_telemetry() {
        let mut queue: HostcallRequestQueue<u8> =
            HostcallRequestQueue::with_mode(2, 2, HostcallQueueMode::SafeFallback);

        let decision = queue.observe_contention_window(sample(70, 30, 120, 350, 0));
        assert_eq!(decision.next_mode, BravoBiasMode::ReadBiased);

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.bravo_mode, BravoBiasMode::ReadBiased);
        assert_eq!(
            snapshot.bravo_last_signature,
            ContentionSignature::MixedContention
        );
        assert!(snapshot.bravo_transitions >= 1);
    }

    #[test]
    fn queue_spills_to_overflow_with_stable_order() {
        let mut queue = HostcallRequestQueue::with_mode(2, 4, HostcallQueueMode::SafeFallback);
        assert!(matches!(
            queue.push_back(0_u8),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(1_u8),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(2_u8),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.fast_depth, 2);
        assert_eq!(snapshot.overflow_depth, 1);
        assert_eq!(snapshot.total_depth, 3);
        assert_eq!(snapshot.overflow_enqueued_total, 1);

        let drained = queue.drain_all();
        assert_eq!(drained.into_iter().collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn queue_rejects_when_overflow_capacity_reached() {
        let mut queue = HostcallRequestQueue::with_mode(1, 1, HostcallQueueMode::SafeFallback);
        assert!(matches!(
            queue.push_back(0_u8),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(1_u8),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));
        assert!(matches!(
            queue.push_back(2_u8),
            HostcallQueueEnqueueResult::Rejected { .. }
        ));

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.total_depth, 2);
        assert_eq!(snapshot.overflow_depth, 1);
        assert_eq!(snapshot.overflow_rejected_total, 1);
    }

    #[test]
    fn s3fifo_fairness_budget_rejects_noisy_tenant_before_overflow_full() {
        let mut queue = HostcallRequestQueue::with_mode(1, 3, HostcallQueueMode::SafeFallback);

        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.noisy"),
                value: 0
            }),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.noisy"),
                value: 1
            }),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.noisy"),
                value: 2
            }),
            HostcallQueueEnqueueResult::Rejected { .. }
        ));

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.s3fifo_mode, S3FifoMode::Active);
        assert_eq!(snapshot.s3fifo_tenant_budget, 1);
        assert_eq!(snapshot.s3fifo_fairness_rejected_total, 1);
        assert_eq!(snapshot.overflow_rejected_total, 1);
    }

    #[test]
    fn s3fifo_ghost_hits_allow_reentry_after_prior_rejection() {
        let mut queue = HostcallRequestQueue::with_mode(1, 3, HostcallQueueMode::SafeFallback);

        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.ghost"),
                value: 0
            }),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.ghost"),
                value: 1
            }),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.ghost"),
                value: 2
            }),
            HostcallQueueEnqueueResult::Rejected { .. }
        ));

        let drained = queue.drain_all();
        assert_eq!(
            drained
                .into_iter()
                .map(|entry| entry.value)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );

        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.ghost"),
                value: 3
            }),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.ghost"),
                value: 4
            }),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.ghost"),
                value: 5
            }),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));

        let snapshot = queue.snapshot();
        assert!(snapshot.s3fifo_ghost_hits_total >= 1);
        assert_eq!(snapshot.s3fifo_fairness_rejected_total, 1);
    }

    #[test]
    fn s3fifo_falls_back_to_conservative_fifo_when_signal_is_insufficient() {
        let mut queue = HostcallRequestQueue::with_mode(1, 2, HostcallQueueMode::SafeFallback);

        for value in 0..96_u8 {
            let _ = queue.push_back(value);
            let _ = queue.drain_all();
        }

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.s3fifo_mode, S3FifoMode::ConservativeFifo);
        assert_eq!(
            snapshot.s3fifo_fallback_reason,
            Some(S3FifoFallbackReason::SignalQualityInsufficient)
        );
        assert!(snapshot.s3fifo_fallback_transitions >= 1);
    }

    #[test]
    fn s3fifo_fairness_fallback_reason_and_transition_count_stay_stable() {
        let mut queue = HostcallRequestQueue::with_mode(1, 1, HostcallQueueMode::SafeFallback);

        assert!(matches!(
            queue.push_back(0_u8),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(1_u8),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));

        for value in 2_u8..40_u8 {
            let _ = queue.push_back(value);
        }

        let fallback = queue.snapshot();
        assert_eq!(fallback.s3fifo_mode, S3FifoMode::ConservativeFifo);
        assert_eq!(
            fallback.s3fifo_fallback_reason,
            Some(S3FifoFallbackReason::FairnessInstability)
        );
        assert_eq!(fallback.s3fifo_fallback_transitions, 1);
        let fairness_rejections_before = fallback.s3fifo_fairness_rejected_total;

        for value in 40_u8..80_u8 {
            let _ = queue.push_back(value);
            let _ = queue.drain_all();
        }

        let stable = queue.snapshot();
        assert_eq!(stable.s3fifo_mode, S3FifoMode::ConservativeFifo);
        assert_eq!(
            stable.s3fifo_fallback_reason,
            Some(S3FifoFallbackReason::FairnessInstability)
        );
        assert_eq!(stable.s3fifo_fallback_transitions, 1);
        assert_eq!(
            stable.s3fifo_fairness_rejected_total,
            fairness_rejections_before
        );
    }

    #[test]
    fn s3fifo_signal_quality_fallback_reason_does_not_flip_under_later_pressure() {
        let mut queue = HostcallRequestQueue::with_mode(1, 2, HostcallQueueMode::SafeFallback);

        for value in 0..96_u8 {
            let _ = queue.push_back(TenantRequest {
                tenant: None,
                value,
            });
            let _ = queue.drain_all();
        }

        let fallback = queue.snapshot();
        assert_eq!(fallback.s3fifo_mode, S3FifoMode::ConservativeFifo);
        assert_eq!(
            fallback.s3fifo_fallback_reason,
            Some(S3FifoFallbackReason::SignalQualityInsufficient)
        );
        assert_eq!(fallback.s3fifo_fallback_transitions, 1);
        let fairness_rejections_before = fallback.s3fifo_fairness_rejected_total;

        for value in 0_u8..32_u8 {
            let _ = queue.push_back(TenantRequest {
                tenant: Some("ext.noisy"),
                value,
            });
            let _ = queue.push_back(TenantRequest {
                tenant: Some("ext.noisy"),
                value: value.saturating_add(1),
            });
            let _ = queue.drain_all();
        }

        let stable = queue.snapshot();
        assert_eq!(stable.s3fifo_mode, S3FifoMode::ConservativeFifo);
        assert_eq!(
            stable.s3fifo_fallback_reason,
            Some(S3FifoFallbackReason::SignalQualityInsufficient)
        );
        assert_eq!(stable.s3fifo_fallback_transitions, 1);
        assert_eq!(
            stable.s3fifo_fairness_rejected_total,
            fairness_rejections_before
        );
    }

    #[test]
    fn ebr_reclamation_tracks_lag_and_latency() {
        let mut queue = HostcallRequestQueue::with_mode(2, 2, HostcallQueueMode::Ebr);
        let pin = queue.pin_epoch();
        assert!(matches!(
            queue.push_back(1_u8),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(2_u8),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        let drained = queue.drain_all();
        assert_eq!(drained.len(), 2);

        queue.force_reclaim();
        let blocked = queue.snapshot();
        assert_eq!(blocked.reclamation_mode, HostcallQueueMode::Ebr);
        assert_eq!(blocked.retired_backlog, 2);
        assert_eq!(blocked.reclaimed_total, 0);
        assert!(blocked.epoch_lag >= 1);

        drop(pin);
        queue.force_reclaim();
        let reclaimed = queue.snapshot();
        assert_eq!(reclaimed.retired_backlog, 0);
        assert!(reclaimed.reclaimed_total >= 2);
        assert!(reclaimed.reclamation_latency_max_epochs >= 1);
    }

    #[test]
    fn safe_fallback_mode_skips_retirement_tracking() {
        let mut queue = HostcallRequestQueue::with_mode(2, 2, HostcallQueueMode::SafeFallback);
        let _pin = queue.pin_epoch();
        assert!(matches!(
            queue.push_back(1_u8),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        let _ = queue.drain_all();
        queue.force_reclaim();

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.reclamation_mode, HostcallQueueMode::SafeFallback);
        assert_eq!(snapshot.retired_backlog, 0);
        assert_eq!(snapshot.reclaimed_total, 0);
    }

    #[test]
    fn ebr_auto_falls_back_when_retired_backlog_runs_away() {
        let mut queue = HostcallRequestQueue::with_mode(1, 1, HostcallQueueMode::Ebr);
        let _pin = queue.pin_epoch();

        // Keep a pin active while repeatedly retiring nodes so backlog exceeds
        // the safety threshold and forces fallback.
        for value in 0..64_u8 {
            let result = queue.push_back(value);
            assert!(
                !matches!(result, HostcallQueueEnqueueResult::Rejected { .. }),
                "queue should accept one item per cycle"
            );
            let drained = queue.drain_all();
            assert_eq!(drained.len(), 1);
            queue.force_reclaim();
        }

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.reclamation_mode, HostcallQueueMode::SafeFallback);
        assert!(snapshot.fallback_transitions >= 1);
    }

    #[test]
    fn ebr_stress_cycle_keeps_retired_backlog_bounded() {
        let mut queue = HostcallRequestQueue::with_mode(4, 8, HostcallQueueMode::Ebr);

        for value in 0..10_000_u32 {
            let _ = queue.push_back(value);
            let drained = queue.drain_all();
            assert_eq!(drained.len(), 1);
            if value % 64 == 0 {
                queue.force_reclaim();
            }
        }

        queue.force_reclaim();
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.reclamation_mode, HostcallQueueMode::Ebr);
        assert_eq!(snapshot.retired_backlog, 0);
        assert!(snapshot.reclaimed_total >= 10_000);
    }

    // ── Additional public API coverage ──

    #[test]
    fn contention_sample_total_acquires_sums_reads_and_writes() {
        let s = sample(100, 50, 0, 0, 0);
        assert_eq!(s.total_acquires(), 150);

        let zero = ContentionSample::default();
        assert_eq!(zero.total_acquires(), 0);

        let max = ContentionSample {
            read_acquires: u64::MAX,
            write_acquires: 1,
            ..Default::default()
        };
        assert_eq!(max.total_acquires(), u64::MAX, "saturating_add on overflow");
    }

    #[test]
    fn contention_sample_read_ratio_permille_values() {
        // All reads → 1000 permille
        let all_reads = sample(100, 0, 0, 0, 0);
        assert_eq!(all_reads.read_ratio_permille(), 1000);

        // All writes → 0 permille
        let all_writes = sample(0, 100, 0, 0, 0);
        assert_eq!(all_writes.read_ratio_permille(), 0);

        // Balanced → ~500 permille
        let balanced = sample(50, 50, 0, 0, 0);
        assert_eq!(balanced.read_ratio_permille(), 500);

        // Zero total → 0 (no division by zero)
        let zero = ContentionSample::default();
        assert_eq!(zero.read_ratio_permille(), 0);

        // 75% reads → 750 permille
        let three_quarter = sample(75, 25, 0, 0, 0);
        assert_eq!(three_quarter.read_ratio_permille(), 750);
    }

    #[test]
    fn bravo_contention_state_mode_accessor() {
        let state = BravoContentionState::new(deterministic_config());
        assert_eq!(state.mode(), BravoBiasMode::Balanced);

        let mut state2 = BravoContentionState::new(deterministic_config());
        // Feed read-dominant sample to transition to ReadBiased
        let _ = state2.observe(sample(80, 10, 0, 0, 0));
        assert_eq!(state2.mode(), BravoBiasMode::ReadBiased);
    }

    #[test]
    fn s3fifo_config_from_capacities_computes_fields() {
        let config = S3FifoConfig::from_capacities(256, 2048);
        // tenant_budget = overflow/2 = 1024
        assert_eq!(config.tenant_budget, 1024);
        // ghost_capacity = (256+2048)*2 = 4608, above min of 16
        assert_eq!(config.ghost_capacity, 4608);
        assert_eq!(config.min_signal_samples, 32);
        assert_eq!(config.max_signalless_streak, 64);
        assert_eq!(config.unstable_rejection_streak, 16);

        // Small capacities → enforced minimums
        let small = S3FifoConfig::from_capacities(1, 1);
        assert_eq!(small.tenant_budget, 1); // max(1/2, 1) = max(0, 1) = 1
        assert_eq!(small.ghost_capacity, 16); // max((1+1)*2, 16) = max(4, 16) = 16
    }

    #[test]
    fn queue_with_capacities_creates_functional_queue() {
        let mut queue: HostcallRequestQueue<u32> = HostcallRequestQueue::with_capacities(4, 8);
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);

        let result = queue.push_back(42);
        assert!(matches!(
            result,
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(!queue.is_empty());
        assert_eq!(queue.len(), 1);

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.fast_capacity, 4);
        assert_eq!(snapshot.overflow_capacity, 8);
    }

    #[test]
    fn queue_clear_resets_state() {
        let mut queue = HostcallRequestQueue::with_mode(2, 4, HostcallQueueMode::Ebr);
        let _ = queue.push_back(1_u8);
        let _ = queue.push_back(2_u8);
        let _ = queue.push_back(3_u8); // spills to overflow
        assert!(!queue.is_empty());

        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.max_depth_seen, 0);
        assert_eq!(snapshot.overflow_enqueued_total, 0);
        assert_eq!(snapshot.overflow_rejected_total, 0);
    }

    #[test]
    fn queue_clear_resets_s3fifo_fallback_and_counters() {
        let mut queue = HostcallRequestQueue::with_mode(1, 3, HostcallQueueMode::SafeFallback);

        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.reset"),
                value: 0
            }),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.reset"),
                value: 1
            }),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.reset"),
                value: 2
            }),
            HostcallQueueEnqueueResult::Rejected { .. }
        ));

        let _ = queue.drain_all();

        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.reset"),
                value: 3
            }),
            HostcallQueueEnqueueResult::FastPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.reset"),
                value: 4
            }),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));
        assert!(matches!(
            queue.push_back(TenantRequest {
                tenant: Some("ext.reset"),
                value: 5
            }),
            HostcallQueueEnqueueResult::OverflowPath { .. }
        ));

        drive_signal_quality_fallback(&mut queue, 6);

        let before_clear = queue.snapshot();
        assert_eq!(before_clear.s3fifo_mode, S3FifoMode::ConservativeFifo);
        assert_eq!(
            before_clear.s3fifo_fallback_reason,
            Some(S3FifoFallbackReason::SignalQualityInsufficient)
        );
        assert_eq!(before_clear.s3fifo_fallback_transitions, 1);
        assert!(before_clear.s3fifo_ghost_hits_total >= 1);
        assert!(before_clear.s3fifo_fairness_rejected_total >= 1);
        assert!(before_clear.s3fifo_signal_samples >= 1);

        queue.clear();

        let cleared = queue.snapshot();
        assert_eq!(cleared.s3fifo_mode, S3FifoMode::Active);
        assert_eq!(cleared.s3fifo_fallback_reason, None);
        assert_eq!(cleared.s3fifo_ghost_depth, 0);
        assert_eq!(cleared.s3fifo_ghost_hits_total, 0);
        assert_eq!(cleared.s3fifo_fairness_rejected_total, 0);
        assert_eq!(cleared.s3fifo_signal_samples, 0);
        assert_eq!(cleared.s3fifo_signalless_streak, 0);
        assert_eq!(cleared.s3fifo_fallback_transitions, 0);
        assert_eq!(cleared.s3fifo_active_tenants, 0);
    }

    #[test]
    fn queue_clear_allows_s3fifo_fallback_to_retrigger_from_clean_state() {
        let mut queue = HostcallRequestQueue::with_mode(1, 2, HostcallQueueMode::SafeFallback);

        drive_signal_quality_fallback(&mut queue, 0);

        let first = queue.snapshot();
        assert_eq!(first.s3fifo_mode, S3FifoMode::ConservativeFifo);
        assert_eq!(
            first.s3fifo_fallback_reason,
            Some(S3FifoFallbackReason::SignalQualityInsufficient)
        );
        assert_eq!(first.s3fifo_fallback_transitions, 1);

        queue.clear();

        let after_clear = queue.snapshot();
        assert_eq!(after_clear.s3fifo_mode, S3FifoMode::Active);
        assert_eq!(after_clear.s3fifo_fallback_reason, None);
        assert_eq!(after_clear.s3fifo_fallback_transitions, 0);

        drive_signal_quality_fallback(&mut queue, 120);

        let retriggered = queue.snapshot();
        assert_eq!(retriggered.s3fifo_mode, S3FifoMode::ConservativeFifo);
        assert_eq!(
            retriggered.s3fifo_fallback_reason,
            Some(S3FifoFallbackReason::SignalQualityInsufficient)
        );
        assert_eq!(retriggered.s3fifo_fallback_transitions, 1);
    }

    #[test]
    fn queue_reclamation_mode_accessor() {
        let ebr = HostcallRequestQueue::<u8>::with_mode(2, 2, HostcallQueueMode::Ebr);
        assert_eq!(ebr.reclamation_mode(), HostcallQueueMode::Ebr);

        let fallback = HostcallRequestQueue::<u8>::with_mode(2, 2, HostcallQueueMode::SafeFallback);
        assert_eq!(fallback.reclamation_mode(), HostcallQueueMode::SafeFallback);
    }

    #[test]
    fn queue_force_safe_fallback_switches_mode() {
        let mut queue: HostcallRequestQueue<u8> =
            HostcallRequestQueue::with_mode(2, 2, HostcallQueueMode::Ebr);
        assert_eq!(queue.reclamation_mode(), HostcallQueueMode::Ebr);

        queue.force_safe_fallback();
        assert_eq!(queue.reclamation_mode(), HostcallQueueMode::SafeFallback);
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.fallback_transitions, 1);

        // Calling again is idempotent (no extra transition counted)
        queue.force_safe_fallback();
        let snapshot2 = queue.snapshot();
        assert_eq!(snapshot2.fallback_transitions, 1);
    }

    #[test]
    fn queue_default_uses_standard_capacities() {
        let queue: HostcallRequestQueue<u8> = HostcallRequestQueue::default();
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.fast_capacity, HOSTCALL_FAST_RING_CAPACITY);
        assert_eq!(snapshot.overflow_capacity, HOSTCALL_OVERFLOW_CAPACITY);
    }

    // ── Property tests ──

    mod proptest_bravo {
        use super::*;
        use proptest::prelude::*;

        fn arb_sample() -> impl Strategy<Value = ContentionSample> {
            (
                0..10_000u64,
                0..10_000u64,
                0..50_000u64,
                0..50_000u64,
                0..100u64,
            )
                .prop_map(|(reads, writes, r_wait, w_wait, w_timeouts)| {
                    ContentionSample {
                        read_acquires: reads,
                        write_acquires: writes,
                        read_wait_p95_us: r_wait,
                        write_wait_p95_us: w_wait,
                        write_timeouts: w_timeouts,
                    }
                })
        }

        fn arb_config() -> impl Strategy<Value = BravoContentionConfig> {
            (
                1..200u64,
                500..1000u32,
                100..500u32,
                500..999u32,
                1000..20_000u64,
                1..10u64,
                1..10u32,
                1..5u32,
            )
                .prop_map(
                    |(
                        min_acq,
                        rd_ratio,
                        mixed_floor,
                        mixed_ceil,
                        starve_wait,
                        starve_to,
                        max_rb,
                        wr_windows,
                    )| {
                        BravoContentionConfig {
                            min_total_acquires: min_acq,
                            read_dominant_ratio_permille: rd_ratio,
                            mixed_ratio_floor_permille: mixed_floor,
                            mixed_ratio_ceiling_permille: mixed_ceil.max(mixed_floor),
                            writer_starvation_wait_us: starve_wait,
                            writer_starvation_timeouts: starve_to,
                            max_consecutive_read_bias_windows: max_rb,
                            writer_recovery_windows: wr_windows,
                        }
                    },
                )
        }

        proptest! {
            #[test]
            fn classify_is_deterministic(
                sample in arb_sample(),
                cfg in arb_config(),
            ) {
                let s1 = BravoContentionState::classify(sample, cfg);
                let s2 = BravoContentionState::classify(sample, cfg);
                assert_eq!(s1, s2, "same inputs must produce same signature");
            }

            #[test]
            fn read_ratio_permille_bounded_0_to_1000(
                reads in 0..u64::MAX / 2,
                writes in 0..u64::MAX / 2,
            ) {
                let s = ContentionSample {
                    read_acquires: reads,
                    write_acquires: writes,
                    ..Default::default()
                };
                let ratio = s.read_ratio_permille();
                assert!(ratio <= 1000, "ratio was {ratio}, expected <= 1000");
            }

            #[test]
            fn total_acquires_at_least_each_component(
                reads in 0..u64::MAX / 2,
                writes in 0..u64::MAX / 2,
            ) {
                let s = ContentionSample {
                    read_acquires: reads,
                    write_acquires: writes,
                    ..Default::default()
                };
                let total = s.total_acquires();
                assert!(total >= reads, "total must be >= reads");
                assert!(total >= writes, "total must be >= writes");
            }

            #[test]
            fn mode_always_valid_after_observation_sequence(
                cfg in arb_config(),
                samples in prop::collection::vec(arb_sample(), 1..30),
            ) {
                let mut state = BravoContentionState::new(cfg);
                for sample in &samples {
                    let decision = state.observe(*sample);
                    assert!(
                        matches!(
                            decision.next_mode,
                            BravoBiasMode::Balanced
                                | BravoBiasMode::ReadBiased
                                | BravoBiasMode::WriterRecovery
                        ),
                        "mode must be a valid variant"
                    );
                    assert_eq!(decision.switched, decision.previous_mode != decision.next_mode);
                }
            }

            #[test]
            fn counters_monotonically_nondecreasing(
                cfg in arb_config(),
                samples in prop::collection::vec(arb_sample(), 1..30),
            ) {
                let mut state = BravoContentionState::new(cfg);
                let mut prev_transitions = 0u64;
                let mut prev_rollbacks = 0u64;
                let mut prev_windows = 0u64;

                for sample in &samples {
                    let _ = state.observe(*sample);
                    let snap = state.snapshot();
                    assert!(snap.transitions >= prev_transitions);
                    assert!(snap.rollbacks >= prev_rollbacks);
                    assert!(snap.windows_observed >= prev_windows);
                    prev_transitions = snap.transitions;
                    prev_rollbacks = snap.rollbacks;
                    prev_windows = snap.windows_observed;
                }
            }

            #[test]
            fn windows_observed_equals_call_count(
                cfg in arb_config(),
                samples in prop::collection::vec(arb_sample(), 1..30),
            ) {
                let mut state = BravoContentionState::new(cfg);
                for sample in &samples {
                    let _ = state.observe(*sample);
                }
                let snap = state.snapshot();
                assert_eq!(
                    snap.windows_observed,
                    samples.len() as u64,
                    "windows_observed must equal number of observe() calls"
                );
            }

            #[test]
            fn initial_state_is_balanced_with_zero_counters(
                cfg in arb_config(),
            ) {
                let state = BravoContentionState::new(cfg);
                let snap = state.snapshot();
                assert_eq!(snap.mode, BravoBiasMode::Balanced);
                assert_eq!(snap.transitions, 0);
                assert_eq!(snap.rollbacks, 0);
                assert_eq!(snap.windows_observed, 0);
                assert_eq!(snap.consecutive_read_bias_windows, 0);
                assert_eq!(snap.writer_recovery_remaining, 0);
            }
        }
    }

    #[test]
    #[cfg(feature = "loom-tests")]
    fn loom_epoch_pin_blocks_reclamation_until_release() {
        use loom::sync::{Arc, Condvar, Mutex};
        use loom::thread;

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
            drop(gate);
        }

        fn mark_pin_ready(sync: &Arc<(Mutex<PinGate>, Condvar)>) {
            let (lock, cvar) = &**sync;
            let mut gate = lock.lock().expect("lock pin gate");
            gate.pin_ready = true;
            cvar.notify_all();
            while !gate.release_pin {
                gate = cvar.wait(gate).expect("wait for pin release");
            }
            drop(gate);
        }

        fn release_pin(sync: &Arc<(Mutex<PinGate>, Condvar)>) {
            let (lock, cvar) = &**sync;
            let mut gate = lock.lock().expect("lock pin gate");
            gate.release_pin = true;
            drop(gate);
            cvar.notify_all();
        }

        loom::model(|| {
            let queue = Arc::new(Mutex::new(HostcallRequestQueue::with_mode(
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
            let pin_thread = thread::spawn(move || {
                let pin = queue_for_pin
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pin_epoch();
                mark_pin_ready(&pin_gate_for_thread);
                drop(pin);
            });

            let queue_for_worker = Arc::clone(&queue);
            let pin_gate_for_worker = Arc::clone(&pin_gate);
            let worker = thread::spawn(move || {
                wait_until_pin_ready(&pin_gate_for_worker);

                let mut queue = queue_for_worker
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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

            let mut queue = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.force_reclaim();
            let snapshot = queue.snapshot();
            assert_eq!(snapshot.retired_backlog, 0);
            assert!(snapshot.reclaimed_total >= 2);
            drop(queue);
        });
    }

    #[test]
    #[cfg(feature = "loom-tests")]
    fn loom_concurrent_enqueue_dequeue_keeps_values_unique() {
        use loom::sync::{Arc, Mutex};
        use loom::thread;

        loom::model(|| {
            let queue = Arc::new(Mutex::new(HostcallRequestQueue::with_mode(
                2,
                2,
                HostcallQueueMode::SafeFallback,
            )));

            let queue_a = Arc::clone(&queue);
            let producer_a = thread::spawn(move || {
                let mut queue = queue_a
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = queue.push_back(10_u8);
            });

            let queue_b = Arc::clone(&queue);
            let producer_b = thread::spawn(move || {
                let mut queue = queue_b
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = queue.push_back(11_u8);
            });

            producer_a.join().expect("producer_a join");
            producer_b.join().expect("producer_b join");

            let mut queue = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let drained = queue.drain_all();
            drop(queue);
            let mut values = drained.into_iter().collect::<Vec<_>>();
            values.sort_unstable();
            assert_eq!(values, vec![10, 11]);
        });
    }
}
