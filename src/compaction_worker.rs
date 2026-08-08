//! Background compaction worker with basic quota controls.
//!
//! This keeps LLM compaction off the foreground turn path by running compaction
//! on the existing runtime and applying results on subsequent turns.

use crate::compaction::{self, CompactionPreparation, CompactionResult};
use crate::error::{Error, Result};
use crate::provider::Provider;
use asupersync::runtime::{JoinHandle, RuntimeHandle};
use futures::FutureExt;
use futures::channel::oneshot;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const COMPACTION_ADMISSION_SCHEMA_V1: &str = "pi.compaction.admission.v1";

/// Quota controls that bound background compaction resource usage.
#[derive(Debug, Clone)]
pub struct CompactionQuota {
    /// Minimum elapsed time between compaction starts.
    pub cooldown: Duration,
    /// Maximum wall-clock time to wait for a background compaction result.
    pub timeout: Duration,
    /// Maximum compaction attempts allowed in a single session.
    pub max_attempts_per_session: u32,
}

impl Default for CompactionQuota {
    fn default() -> Self {
        Self {
            cooldown: Duration::from_secs(60),
            timeout: Duration::from_secs(120),
            max_attempts_per_session: 100,
        }
    }
}

/// Optional memory posture supplied by a caller that already has host/cgroup
/// evidence. Unknown memory posture is treated as unavailable, not healthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionMemorySignal {
    pub available_bytes: Option<u64>,
    pub required_headroom_bytes: u64,
    pub pressure: bool,
}

impl CompactionMemorySignal {
    fn is_pressure(self) -> bool {
        self.pressure
            || self
                .available_bytes
                .is_some_and(|available| available < self.required_headroom_bytes)
    }
}

/// Optional provider posture for deciding whether background compaction would
/// amplify an already degraded provider/model path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionProviderSignal {
    pub p95_latency_ms: Option<u64>,
    pub max_p95_latency_ms: Option<u64>,
    pub error_rate_per_mille: Option<u16>,
    pub max_error_rate_per_mille: Option<u16>,
    pub stale: bool,
    pub degraded: bool,
}

impl CompactionProviderSignal {
    fn is_degraded(self) -> bool {
        self.degraded
            || self.stale
            || self
                .p95_latency_ms
                .zip(self.max_p95_latency_ms)
                .is_some_and(|(p95, max)| p95 > max)
            || self
                .error_rate_per_mille
                .zip(self.max_error_rate_per_mille)
                .is_some_and(|(rate, max)| rate > max)
    }
}

/// Optional queue posture from an operator/runtime surface that already tracks
/// compaction demand across sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionQueueSignal {
    pub queued_requests: u32,
    pub max_queued_requests: u32,
    pub saturated: bool,
}

impl CompactionQueueSignal {
    const fn is_saturated(self) -> bool {
        self.saturated
            || (self.max_queued_requests > 0 && self.queued_requests >= self.max_queued_requests)
    }
}

/// Optional admission signals supplied by the caller. The worker never probes
/// live host/provider state itself; callers pass already-sampled, redacted facts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionAdmissionSignals {
    pub memory: Option<CompactionMemorySignal>,
    pub provider: Option<CompactionProviderSignal>,
    pub queue: Option<CompactionQueueSignal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionAdmissionReason {
    Allowed,
    Pending,
    SessionAttemptLimit,
    Cooldown,
    NoPreparation,
    MemoryPressure,
    ProviderDegraded,
    QueueSaturated,
}

impl CompactionAdmissionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Pending => "pending",
            Self::SessionAttemptLimit => "session_attempt_limit",
            Self::Cooldown => "cooldown",
            Self::NoPreparation => "no_preparation",
            Self::MemoryPressure => "memory_pressure",
            Self::ProviderDegraded => "provider_degraded",
            Self::QueueSaturated => "queue_saturated",
        }
    }
}

/// Deterministic explanation for a background compaction admission decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionAdmissionDecision {
    pub schema: &'static str,
    pub allowed: bool,
    pub reason: CompactionAdmissionReason,
    pub tokens_before: Option<u64>,
    pub attempt_count: u32,
    pub max_attempts_per_session: u32,
    pub cooldown_remaining_ms: u64,
    pub signals: CompactionAdmissionSignals,
}

impl CompactionAdmissionDecision {
    pub const fn is_allowed(&self) -> bool {
        self.allowed
    }
}

fn duration_millis_saturating(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

type CompactionOutcome = Result<CompactionResult>;

struct PendingCompaction {
    join: JoinHandle<CompactionOutcome>,
    abort_tx: Option<oneshot::Sender<()>>,
    started_at: Instant,
}

impl PendingCompaction {
    fn is_finished(&self) -> bool {
        self.join.is_finished()
    }

    fn abort(&mut self) {
        if let Some(abort_tx) = self.abort_tx.take()
            && abort_tx.send(()).is_err()
        {
            tracing::debug!("abort signal receiver was already dropped");
        }
    }
}

/// Per-session background compaction state.
pub(crate) struct CompactionWorkerState {
    pending: Option<PendingCompaction>,
    last_start: Option<Instant>,
    attempt_count: u32,
    quota: CompactionQuota,
}

impl CompactionWorkerState {
    pub const fn new(quota: CompactionQuota) -> Self {
        Self {
            pending: None,
            last_start: None,
            attempt_count: 0,
            quota,
        }
    }

    /// Whether a new background compaction is allowed to start now.
    pub fn can_start(&self) -> bool {
        self.quota_block_reason().is_none()
    }

    pub fn admission_decision(
        &self,
        preparation: Option<&CompactionPreparation>,
        signals: &CompactionAdmissionSignals,
    ) -> CompactionAdmissionDecision {
        let cooldown_remaining = self.cooldown_remaining();
        let tokens_before = preparation.map(|prep| prep.tokens_before);
        let reason = self
            .quota_block_reason()
            .unwrap_or_else(|| Self::signal_block_reason(preparation, signals));

        CompactionAdmissionDecision {
            schema: COMPACTION_ADMISSION_SCHEMA_V1,
            allowed: reason == CompactionAdmissionReason::Allowed,
            reason,
            tokens_before,
            attempt_count: self.attempt_count,
            max_attempts_per_session: self.quota.max_attempts_per_session,
            cooldown_remaining_ms: duration_millis_saturating(cooldown_remaining),
            signals: *signals,
        }
    }

    fn signal_block_reason(
        preparation: Option<&CompactionPreparation>,
        signals: &CompactionAdmissionSignals,
    ) -> CompactionAdmissionReason {
        if preparation.is_none() {
            CompactionAdmissionReason::NoPreparation
        } else if signals
            .memory
            .is_some_and(CompactionMemorySignal::is_pressure)
        {
            CompactionAdmissionReason::MemoryPressure
        } else if signals
            .provider
            .is_some_and(CompactionProviderSignal::is_degraded)
        {
            CompactionAdmissionReason::ProviderDegraded
        } else if signals
            .queue
            .is_some_and(CompactionQueueSignal::is_saturated)
        {
            CompactionAdmissionReason::QueueSaturated
        } else {
            CompactionAdmissionReason::Allowed
        }
    }

    fn quota_block_reason(&self) -> Option<CompactionAdmissionReason> {
        if self.pending.is_some() {
            Some(CompactionAdmissionReason::Pending)
        } else if self.attempt_count >= self.quota.max_attempts_per_session {
            Some(CompactionAdmissionReason::SessionAttemptLimit)
        } else if !self.cooldown_remaining().is_zero() {
            Some(CompactionAdmissionReason::Cooldown)
        } else {
            None
        }
    }

    fn cooldown_remaining(&self) -> Duration {
        self.last_start.map_or(Duration::ZERO, |last| {
            self.quota.cooldown.saturating_sub(last.elapsed())
        })
    }

    /// Non-blocking check for a completed compaction result.
    pub async fn try_recv(&mut self) -> Option<CompactionOutcome> {
        // Check timeout first (read-only borrow, then drop before mutation).
        let timed_out = self
            .pending
            .as_ref()
            .is_some_and(|p| p.started_at.elapsed() > self.quota.timeout);

        if timed_out {
            if let Some(mut pending) = self.pending.take() {
                pending.abort();
            }
            return Some(Err(Error::session(
                "Background compaction timed out".to_string(),
            )));
        }

        if !self
            .pending
            .as_ref()
            .is_some_and(PendingCompaction::is_finished)
        {
            return None;
        }

        let pending = self.pending.take()?;
        let outcome = pending.join.await;

        // Reset the per-session attempt counter on a SUCCESSFUL compaction so
        // that a burst of transient failures (e.g. oversized-prompt rejections
        // during the very endgame that the failsafe local-truncation summary
        // covers) cannot permanently trip `SessionAttemptLimit`. Without this
        // reset, the monotonic counter is the mechanism by which a long
        // session ends up with `can_start() == false` forever — see
        // `pi-usage/diagnosis/stuck-pending-deadlock-root-cause`.
        if outcome.is_ok() {
            self.note_success();
        }

        Some(outcome)
    }

    /// Mark the most recent compaction as successful and restore full quota.
    ///
    /// Called automatically by `try_recv` when the awaited outcome is `Ok`.
    /// Resets `attempt_count` to zero (so `SessionAttemptLimit` cannot latch
    /// permanently) and clears the last-start timestamp so the cooldown clock
    /// starts fresh only from the next `start()`.
    pub fn note_success(&mut self) {
        self.attempt_count = 0;
        self.last_start = None;
    }

    /// Abort and drop any in-flight background compaction, freeing the worker
    /// slot immediately. Used by the catastrophic-oversize failsafe in
    /// `Agent::maybe_compact` so a stuck `pending` task (which would otherwise
    /// keep `can_start() == false`) cannot block forced synchronous recovery.
    pub fn abort_pending(&mut self) {
        if let Some(mut pending) = self.pending.take() {
            pending.abort();
        }
    }

    /// Spawn a background compaction on the provided runtime.
    pub fn start(
        &mut self,
        runtime_handle: &RuntimeHandle,
        preparation: CompactionPreparation,
        provider: Arc<dyn Provider>,
        api_key: String,
        custom_instructions: Option<String>,
    ) {
        debug_assert!(
            self.can_start(),
            "start() called while can_start() is false"
        );

        let (abort_tx, abort_rx) = oneshot::channel();
        let now = Instant::now();
        let join = runtime_handle.spawn(async move {
            run_compaction_task(
                preparation,
                provider,
                api_key,
                custom_instructions,
                abort_rx,
            )
            .await
        });

        self.pending = Some(PendingCompaction {
            join,
            abort_tx: Some(abort_tx),
            started_at: now,
        });
        self.last_start = Some(now);
        self.attempt_count = self.attempt_count.saturating_add(1);
    }
}

pub fn compaction_admission_evidence(
    decisions: &[CompactionAdmissionDecision],
    foreground_p95_ms: u64,
    foreground_p99_ms: u64,
) -> Value {
    let mut rejected_by_reason: BTreeMap<&'static str, usize> = BTreeMap::new();
    for decision in decisions {
        if !decision.allowed {
            *rejected_by_reason
                .entry(decision.reason.as_str())
                .or_default() += 1;
        }
    }
    let admitted = decisions.iter().filter(|decision| decision.allowed).count();
    json!({
        "schema": COMPACTION_ADMISSION_SCHEMA_V1,
        "decisionCount": decisions.len(),
        "admittedCount": admitted,
        "rejectedCount": decisions.len().saturating_sub(admitted),
        "rejectedByReason": rejected_by_reason,
        "foregroundImpact": {
            "source": "deterministic_fixture",
            "p95Ms": foreground_p95_ms,
            "p99Ms": foreground_p99_ms,
        },
        "decisions": decisions,
    })
}

impl Drop for CompactionWorkerState {
    fn drop(&mut self) {
        if let Some(mut pending) = self.pending.take() {
            pending.abort();
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
async fn run_compaction_task(
    preparation: CompactionPreparation,
    provider: Arc<dyn Provider>,
    api_key: String,
    custom_instructions: Option<String>,
    abort_rx: oneshot::Receiver<()>,
) -> CompactionOutcome {
    let abort_fut = async move {
        if abort_rx.await.is_err() {
            tracing::debug!("abort signal sender was dropped before sending abort");
        }
        Err(Error::session("Background compaction aborted".to_string()))
    }
    .fuse();
    let compaction_fut = std::panic::AssertUnwindSafe(compaction::compact(
        preparation,
        provider,
        &api_key,
        custom_instructions.as_deref(),
    ))
    .catch_unwind()
    .fuse();

    futures::pin_mut!(abort_fut, compaction_fut);

    match futures::future::select(abort_fut, compaction_fut).await {
        futures::future::Either::Left((abort_result, _)) => abort_result,
        futures::future::Either::Right((Ok(result), _)) => result,
        futures::future::Either::Right((Err(_), _)) => Err(Error::session(
            "Background compaction worker panicked".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn make_worker(quota: CompactionQuota) -> CompactionWorkerState {
        CompactionWorkerState::new(quota)
    }

    fn default_worker() -> CompactionWorkerState {
        make_worker(CompactionQuota::default())
    }

    fn compaction_admission_preparation(tokens_before: u64) -> CompactionPreparation {
        CompactionPreparation {
            first_kept_entry_id: "entry-kept".to_string(),
            messages_to_summarize: Vec::new(),
            turn_prefix_messages: Vec::new(),
            is_split_turn: false,
            tokens_before,
            previous_summary: None,
            file_ops: compaction::FileOperations::default(),
            settings: compaction::ResolvedCompactionSettings::default(),
        }
    }

    fn compaction_admission_decision(
        worker: &CompactionWorkerState,
        signals: CompactionAdmissionSignals,
    ) -> CompactionAdmissionDecision {
        let prep = compaction_admission_preparation(150_000);
        worker.admission_decision(Some(&prep), &signals)
    }

    fn run_async<T, F>(make_future: impl FnOnce(RuntimeHandle) -> F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build test runtime");
        let runtime_handle = runtime.handle();
        runtime.block_on(make_future(runtime_handle))
    }

    fn inject_pending(worker: &mut CompactionWorkerState, pending: PendingCompaction) {
        worker.pending = Some(pending);
        worker.last_start = Some(Instant::now());
        worker.attempt_count += 1;
    }

    async fn ready_pending_with_handle(
        runtime_handle: RuntimeHandle,
        outcome: CompactionOutcome,
    ) -> PendingCompaction {
        let join = runtime_handle.spawn(async move { outcome });
        PendingCompaction {
            join,
            abort_tx: None,
            started_at: Instant::now(),
        }
    }

    async fn parked_pending_with_handle(
        runtime_handle: RuntimeHandle,
        aborted: Option<Arc<AtomicBool>>,
    ) -> PendingCompaction {
        let (abort_tx, abort_rx) = oneshot::channel();
        let join = runtime_handle.spawn(async move {
            if abort_rx.await.is_err() {
                tracing::debug!("abort signal sender was dropped before sending abort");
            }
            if let Some(flag) = aborted {
                flag.store(true, Ordering::SeqCst);
            }
            Err(Error::session("Background compaction aborted".to_string()))
        });
        PendingCompaction {
            join,
            abort_tx: Some(abort_tx),
            started_at: Instant::now(),
        }
    }

    #[test]
    fn fresh_worker_can_start() {
        let w = default_worker();
        assert!(w.can_start());
    }

    #[test]
    fn cannot_start_while_pending() {
        run_async(|runtime_handle| async move {
            let mut w = default_worker();
            let pending = parked_pending_with_handle(runtime_handle, None).await;
            inject_pending(&mut w, pending);
            assert!(!w.can_start());
        });
    }

    #[test]
    fn cannot_start_during_cooldown() {
        let mut w = make_worker(CompactionQuota {
            cooldown: Duration::from_secs(3600),
            ..CompactionQuota::default()
        });
        w.last_start = Some(Instant::now());
        w.attempt_count = 1;
        assert!(!w.can_start());
    }

    #[test]
    fn can_start_after_cooldown() {
        let mut w = make_worker(CompactionQuota {
            cooldown: Duration::from_millis(0),
            ..CompactionQuota::default()
        });
        w.last_start = Some(
            Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        );
        w.attempt_count = 1;
        assert!(w.can_start());
    }

    #[test]
    fn max_attempts_blocks_start() {
        let mut w = make_worker(CompactionQuota {
            max_attempts_per_session: 2,
            cooldown: Duration::from_millis(0),
            ..CompactionQuota::default()
        });
        w.attempt_count = 2;
        assert!(!w.can_start());
    }

    #[test]
    fn compaction_admission_reports_session_attempt_limit() {
        let mut w = make_worker(CompactionQuota {
            max_attempts_per_session: 2,
            cooldown: Duration::from_millis(0),
            ..CompactionQuota::default()
        });
        w.attempt_count = 2;

        let decision = compaction_admission_decision(&w, CompactionAdmissionSignals::default());
        assert!(!decision.is_allowed());
        assert_eq!(
            decision.reason,
            CompactionAdmissionReason::SessionAttemptLimit
        );
    }

    #[test]
    fn compaction_admission_reports_cooldown() {
        let mut w = make_worker(CompactionQuota {
            cooldown: Duration::from_secs(3600),
            ..CompactionQuota::default()
        });
        w.last_start = Some(Instant::now());
        w.attempt_count = 1;

        let decision = compaction_admission_decision(&w, CompactionAdmissionSignals::default());
        assert!(!decision.is_allowed());
        assert_eq!(decision.reason, CompactionAdmissionReason::Cooldown);
        assert!(decision.cooldown_remaining_ms > 0);
    }

    #[test]
    fn compaction_admission_reports_no_preparation_when_transcript_below_threshold() {
        let w = default_worker();
        let decision = w.admission_decision(None, &CompactionAdmissionSignals::default());
        assert!(!decision.allowed);
        assert_eq!(decision.reason, CompactionAdmissionReason::NoPreparation);
        assert_eq!(decision.tokens_before, None);
    }

    #[test]
    fn compaction_admission_allows_prepared_workload_without_degraded_signals() {
        let w = default_worker();
        let prep = compaction_admission_preparation(180_000);
        let decision = w.admission_decision(Some(&prep), &CompactionAdmissionSignals::default());
        assert!(decision.allowed);
        assert_eq!(decision.reason, CompactionAdmissionReason::Allowed);
        assert_eq!(decision.tokens_before, Some(180_000));
    }

    #[test]
    fn compaction_admission_reports_memory_pressure() {
        let w = default_worker();
        let decision = compaction_admission_decision(
            &w,
            CompactionAdmissionSignals {
                memory: Some(CompactionMemorySignal {
                    available_bytes: Some(256 * 1024 * 1024),
                    required_headroom_bytes: 512 * 1024 * 1024,
                    pressure: false,
                }),
                ..CompactionAdmissionSignals::default()
            },
        );
        assert!(!decision.allowed);
        assert_eq!(decision.reason, CompactionAdmissionReason::MemoryPressure);
    }

    #[test]
    fn compaction_admission_reports_provider_degraded_for_stale_or_slow_metrics() {
        let w = default_worker();
        let stale = compaction_admission_decision(
            &w,
            CompactionAdmissionSignals {
                provider: Some(CompactionProviderSignal {
                    p95_latency_ms: None,
                    max_p95_latency_ms: Some(5_000),
                    error_rate_per_mille: None,
                    max_error_rate_per_mille: Some(50),
                    stale: true,
                    degraded: false,
                }),
                ..CompactionAdmissionSignals::default()
            },
        );
        assert_eq!(stale.reason, CompactionAdmissionReason::ProviderDegraded);

        let slow = compaction_admission_decision(
            &w,
            CompactionAdmissionSignals {
                provider: Some(CompactionProviderSignal {
                    p95_latency_ms: Some(8_000),
                    max_p95_latency_ms: Some(5_000),
                    error_rate_per_mille: Some(10),
                    max_error_rate_per_mille: Some(50),
                    stale: false,
                    degraded: false,
                }),
                ..CompactionAdmissionSignals::default()
            },
        );
        assert_eq!(slow.reason, CompactionAdmissionReason::ProviderDegraded);

        let erroring = compaction_admission_decision(
            &w,
            CompactionAdmissionSignals {
                provider: Some(CompactionProviderSignal {
                    p95_latency_ms: Some(100),
                    max_p95_latency_ms: Some(5_000),
                    error_rate_per_mille: Some(75),
                    max_error_rate_per_mille: Some(50),
                    stale: false,
                    degraded: false,
                }),
                ..CompactionAdmissionSignals::default()
            },
        );
        assert_eq!(erroring.reason, CompactionAdmissionReason::ProviderDegraded);
    }

    #[test]
    fn compaction_admission_reports_queue_saturated() {
        let w = default_worker();
        let decision = compaction_admission_decision(
            &w,
            CompactionAdmissionSignals {
                queue: Some(CompactionQueueSignal {
                    queued_requests: 4,
                    max_queued_requests: 4,
                    saturated: false,
                }),
                ..CompactionAdmissionSignals::default()
            },
        );
        assert!(!decision.allowed);
        assert_eq!(decision.reason, CompactionAdmissionReason::QueueSaturated);
    }

    #[test]
    fn compaction_admission_priority_prefers_existing_work_and_quotas() {
        run_async(|runtime_handle| async move {
            let mut w = default_worker();
            let pending = parked_pending_with_handle(runtime_handle, None).await;
            inject_pending(&mut w, pending);

            let decision = compaction_admission_decision(
                &w,
                CompactionAdmissionSignals {
                    memory: Some(CompactionMemorySignal {
                        available_bytes: Some(1),
                        required_headroom_bytes: 2,
                        pressure: true,
                    }),
                    provider: Some(CompactionProviderSignal {
                        p95_latency_ms: Some(10_000),
                        max_p95_latency_ms: Some(1),
                        error_rate_per_mille: Some(1_000),
                        max_error_rate_per_mille: Some(1),
                        stale: true,
                        degraded: true,
                    }),
                    queue: Some(CompactionQueueSignal {
                        queued_requests: 10,
                        max_queued_requests: 1,
                        saturated: true,
                    }),
                },
            );
            assert_eq!(decision.reason, CompactionAdmissionReason::Pending);
        });
    }

    #[test]
    fn compaction_admission_multi_session_fixture_bounds_started_compactions() {
        let mut decisions = Vec::new();
        let mut queued_requests = 0_u32;
        for _ in 0..5 {
            let w = default_worker();
            let decision = compaction_admission_decision(
                &w,
                CompactionAdmissionSignals {
                    queue: Some(CompactionQueueSignal {
                        queued_requests,
                        max_queued_requests: 2,
                        saturated: false,
                    }),
                    ..CompactionAdmissionSignals::default()
                },
            );
            if decision.allowed {
                queued_requests = queued_requests.saturating_add(1);
            }
            decisions.push(decision);
        }

        let evidence = compaction_admission_evidence(&decisions, 4, 7);
        assert_eq!(evidence["schema"], COMPACTION_ADMISSION_SCHEMA_V1);
        assert_eq!(evidence["admittedCount"], 2);
        assert_eq!(evidence["rejectedCount"], 3);
        assert_eq!(evidence["rejectedByReason"]["queue_saturated"], 3);
        assert_eq!(evidence["foregroundImpact"]["p95Ms"], 4);
        assert_eq!(evidence["foregroundImpact"]["p99Ms"], 7);
    }

    #[test]
    fn try_recv_none_when_no_pending() {
        run_async(|_runtime_handle| async move {
            let mut w = default_worker();
            assert!(w.try_recv().await.is_none());
        });
    }

    #[test]
    fn try_recv_none_when_not_ready() {
        run_async(|runtime_handle| async move {
            let mut w = default_worker();
            let pending = parked_pending_with_handle(runtime_handle, None).await;
            inject_pending(&mut w, pending);
            // Nothing completed yet.
            assert!(w.try_recv().await.is_none());
            // Pending should still be there.
            assert!(w.pending.is_some());
        });
    }

    #[test]
    fn dropping_worker_aborts_pending_task() {
        run_async(|runtime_handle| async move {
            let aborted = Arc::new(AtomicBool::new(false));
            let mut w = default_worker();
            let pending =
                parked_pending_with_handle(runtime_handle, Some(Arc::clone(&aborted))).await;
            inject_pending(&mut w, pending);

            drop(w);
            asupersync::time::sleep(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(50),
            )
            .await;

            assert!(
                aborted.load(Ordering::SeqCst),
                "dropping the worker should abort the pending task"
            );
        });
    }

    #[test]
    fn try_recv_timeout() {
        run_async(|runtime_handle| async move {
            let aborted = Arc::new(AtomicBool::new(false));
            let mut w = make_worker(CompactionQuota {
                timeout: Duration::from_millis(0),
                ..CompactionQuota::default()
            });
            let mut pending =
                parked_pending_with_handle(runtime_handle, Some(Arc::clone(&aborted))).await;
            pending.started_at = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now);
            inject_pending(&mut w, pending);

            let outcome = w.try_recv().await.expect("should return timeout error");
            assert!(outcome.is_err());
            let err_msg = outcome.unwrap_err().to_string();
            assert!(err_msg.contains("timed out"), "got: {err_msg}");

            asupersync::time::sleep(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(50),
            )
            .await;
            assert!(
                aborted.load(Ordering::SeqCst),
                "timing out the worker should abort the pending task"
            );
        });
    }

    #[test]
    fn try_recv_success() {
        run_async(|runtime_handle| async move {
            let mut w = default_worker();

            // Simulate a successful compaction result.
            let result = CompactionResult {
                summary: "test summary".to_string(),
                first_kept_entry_id: "entry-1".to_string(),
                tokens_before: 1000,
                details: compaction::CompactionDetails {
                    read_files: vec![],
                    modified_files: vec![],
                },
            };
            let pending = ready_pending_with_handle(runtime_handle, Ok(result)).await;
            inject_pending(&mut w, pending);
            asupersync::time::sleep(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(50),
            )
            .await;

            let outcome = w.try_recv().await.expect("should have result");
            let result = outcome.expect("should be Ok");
            assert_eq!(result.summary, "test summary");
            assert!(w.pending.is_none());
        });
    }

    // ─── Stuck-pending deadlock regression tests ─────────────────────
    //
    // These mirror the production failure proven on sessions b058fa72
    // (549k tokens, 0 compactions) and 1e796d52 (573k tokens, 0 compactions):
    // a failed summarization LLM call left the worker's `pending` slot stuck,
    // `attempt_count` climbed to `max_attempts_per_session`, and `can_start()`
    // returned false forever. The fix (Part B) resets attempt_count on success
    // and exposes `abort_pending()` so the Part C force-failsafe path can
    // always recover from the >=2x-window catastrophic state.

    #[test]
    fn note_success_resets_attempt_count_and_last_start() {
        // Part B unit proof: a single success restores full quota even after a
        // long run of failures that otherwise would hit SessionAttemptLimit.
        let mut w = default_worker();
        w.attempt_count = 100; // exactly at the default SessionAttemptLimit
        w.last_start = Some(Instant::now());
        assert!(
            !w.can_start(),
            "at the limit, can_start() must be false before note_success"
        );

        w.note_success();
        assert_eq!(w.attempt_count, 0, "attempt_count reset to 0");
        assert!(w.last_start.is_none(), "last_start cleared so cooldown clock restarts fresh");
        assert!(w.can_start(), "after a success, can_start() must be true again");
    }

    #[test]
    fn try_recv_resets_attempt_count_on_success() {
        // Wire proof: try_recv() must call note_success() when the awaited
        // outcome is Ok, directly unbreaking the SessionAttemptLimit latch
        // that poisoned production sessions.
        run_async(|runtime_handle| async move {
            let mut w = default_worker();
            // Fill the quota to the permanent-block state.
            w.attempt_count = 100;
            assert!(!w.can_start(), "precondition: at SessionAttemptLimit");

            let result = CompactionResult {
                summary: "recovery summary".to_string(),
                first_kept_entry_id: "kept".to_string(),
                tokens_before: 1_000_000,
                details: compaction::CompactionDetails {
                    read_files: vec![],
                    modified_files: vec![],
                },
            };
            let pending = ready_pending_with_handle(runtime_handle, Ok(result)).await;
            inject_pending(&mut w, pending);
            asupersync::time::sleep(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(50),
            )
            .await;

            let outcome = w.try_recv().await.expect("should have result");
            assert!(outcome.is_ok(), "consumed the Ok outcome");
            assert_eq!(w.attempt_count, 0, "try_recv reset attempt_count via note_success");
            assert!(w.can_start(), "quota restored by a single success");
        });
    }

    #[test]
    fn try_recv_does_not_reset_attempt_count_on_failure() {
        // Counter-proof: failed compactions must NOT reset the quota — that
        // would defeat the SessionAttemptLimit safety valve. Only success resets.
        run_async(|runtime_handle| async move {
            let mut w = default_worker();
            w.attempt_count = 99;
            let pending = ready_pending_with_handle(
                runtime_handle,
                Err(Error::session("simulated provider 500")),
            )
            .await;
            inject_pending(&mut w, pending);
            asupersync::time::sleep(
                asupersync::time::wall_now(),
                std::time::Duration::from_millis(50),
            )
            .await;

            let outcome = w.try_recv().await.expect("should have outcome");
            assert!(outcome.is_err(), "consumed the Err outcome");
            assert_eq!(
                w.attempt_count, 100,
                "failed attempt incremented, NOT reset; SessionAttemptLimit now latches"
            );
            assert!(!w.can_start(), "failures must keep the quota blocked");
        });
    }

    #[test]
    fn abort_pending_frees_worker_slot_immediately() {
        // Part C unit proof: the force-failsafe path in maybe_compact needs a
        // way to drop a stuck pending task so it can synchronously apply a
        // local_compact regardless of quota. abort_pending() is that escape.
        run_async(|runtime_handle| async move {
            let mut w = default_worker();
            let parked = parked_pending_with_handle(runtime_handle, None).await;
            inject_pending(&mut w, parked);
            assert!(w.pending.is_some(), "precondition: pending is Some");
            assert!(!w.can_start(), "pending keeps can_start() false");

            w.abort_pending();
            assert!(w.pending.is_none(), "abort_pending dropped the task");
            // Cooldown may still elapse (last_start was set), but pending is
            // definitely gone — that's what the force path needs.
        });
    }

    #[test]
    fn catastrophic_oversize_admission_denied_then_local_compact_recovers() {
        // End-to-end-ish proof for the bug's exact production scenario:
        //   context (tokens_before) = 2x window  -> oversized
        //   worker.attempt_count = max           -> SessionAttemptLimit latch
        //   => admission.allowed is false
        //   => without the fix, this is the permanent-spiral state.
        // The recovery contract: abort_pending() + compaction::local_compact()
        // produce a real CompactionResult with the failsafe summary, regardless
        // of the denial reason.
        run_async(|runtime_handle| async move {
            let mut w = make_worker(CompactionQuota {
                cooldown: std::time::Duration::from_secs(60),
                timeout: std::time::Duration::from_secs(120),
                max_attempts_per_session: 100,
            });
            w.attempt_count = 100; // SessionAttemptLimit latched
            let parked = parked_pending_with_handle(runtime_handle, None).await;
            inject_pending(&mut w, parked); // AND pending is stuck (Pending reason)

            assert!(!w.can_start(), "stuck pending + limit = full starvation");

            // Mirror what `agent::maybe_compact` does in the force-failsafe branch.
            w.abort_pending();

            let settings = compaction::ResolvedCompactionSettings {
                enabled: true,
                context_window_tokens: 128_000, // production deepseek-chat window
                reserve_tokens: 8_192,         // production-applied value
                keep_recent_tokens: 4_096,
            };
            // tokens_before = 186_582 (proven peak of session 6667aec1).
            let prep = compaction::CompactionPreparation {
                first_kept_entry_id: "kept".to_string(),
                messages_to_summarize: vec![compaction::SessionMessage::User {
                    content: compaction::UserContent::Text(
                        "very long conversation body that broke pi".to_string(),
                    ),
                    timestamp: Some(0),
                }],
                turn_prefix_messages: Vec::new(),
                is_split_turn: false,
                tokens_before: 186_582,
                previous_summary: Some("prior compaction summary".to_string()),
                file_ops: compaction::FileOperations::default(),
                settings,
            };

            // admission is STILL denied even after abort (the 60s cooldown ticks):
            let decision = w.admission_decision(Some(&prep), &CompactionAdmissionSignals::default());
            assert!(
                !decision.allowed,
                "admission should still be denied by cooldown/quota; \
                 this is precisely why the force path is needed"
            );

            // The force-failsafe runs `local_compact` IGNORING the denial:
            let result = compaction::local_compact(prep);
            assert!(
                result.summary.contains("failsafe local truncation"),
                "recovery produced a deterministic failsafe summary"
            );
            assert!(
                result.summary.contains("prior compaction summary"),
                "previous compaction summary preserved across forced recovery"
            );
            assert_eq!(result.tokens_before, 186_582);
            assert_eq!(result.first_kept_entry_id, "kept");
            // local_compact did not touch the provider, so HTTP500 cannot block it.
        });
    }

    #[test]
    fn config_matrix_compaction_threshold_invariants_hold() {
        // Validates the claim that NO (reserve, keep_recent, window) combination
        // changes the deadlock behavior: the trigger threshold is a pure
        // function of (window, reserve), independent of keep_recent — the lock
        // itself occurs in the admission/worker, untouched by config.
        let cases = [
            // (window, reserve, keep_recent, expected_trigger)
            (128_000u32, 4_096u32, 8_192u32, 123_904u64),
            (128_000, 8_192, 4_096, 119_808),
            (128_000, 12_000, 2_000, 116_000),
            (32_768, 4_096, 2_000, 28_672),
            (200_000, 10_000, 8_000, 190_000),
        ];
        for (window, reserve, keep_recent, expected_trigger) in cases {
            let settings = compaction::ResolvedCompactionSettings {
                enabled: true,
                context_window_tokens: window,
                reserve_tokens: reserve,
                keep_recent_tokens: keep_recent,
            };
            // Just below trigger: should_compact false.
            assert!(
                !compaction::prepare_compaction_threshold_check(
                    expected_trigger.saturating_sub(1),
                    window,
                    &settings,
                ),
                "case w={window} r={reserve} k={keep_recent}: just-below must NOT trigger"
            );
            // At/above trigger: should_compact true.
            assert!(
                compaction::prepare_compaction_threshold_check(
                    expected_trigger,
                    window,
                    &settings,
                ),
                "case w={window} r={reserve} k={keep_recent}: at-trigger must trigger"
            );
            // Crucially: keep_recent does NOT affect the trigger. Confirm by
            // flipping keep_recent and verifying the same expected_trigger.
            let other = compaction::ResolvedCompactionSettings {
                keep_recent_tokens: keep_recent.wrapping_add(99),
                ..settings.clone()
            };
            assert!(
                compaction::prepare_compaction_threshold_check(expected_trigger, window, &other),
                "keep_recent must not affect the trigger threshold (config-independence)"
            );
        }
    }
}
