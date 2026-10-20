//! Owned native work in the ordinary jobs registry. A task's future, privacy
//! projection and cancellation context all outlive the launching tool call;
//! no borrowed Agent, approval callback or eval host crosses this boundary.

use super::{
    ArtifactCleanupOutcome, ArtifactRetentionPolicy, ArtifactSink, CancelDeadline, JobDelivery,
    JobEntry, JobSnapshot, JobSnapshotSource, JobStatus, MAX_ARTIFACT_BYTES, MAX_ARTIFACT_FILES,
    MAX_TOTAL_ARTIFACT_BYTES, MIN_RETAINED_ARTIFACT_FILES, OUTPUT_TAIL_BYTES, TailBuffer,
    acquire_artifact_budget_lock, capture_session_spawn_generation, create_job_artifact,
    enforce_artifact_retention, ensure_session_spawn_generation, lifecycle_lock, now_ms, registry,
    reserve_job_slot, session_closing_error, settle_job_and_enqueue_notice, settle_job_without_notice,
};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::text_completion::AuxiliaryPrivacySnapshot;
use crate::tools::ToolOutput;
use asupersync::sync::Notify;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TASK_SCHEMA: &str = "pi.host_job.v1";
const MAX_TASK_RESULT_BYTES: usize = 1024 * 1024;
const MAX_TASK_LABEL_BYTES: usize = 256;
const TASK_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Retain the owner's shutdown generation across asynchronous preflight. A
/// caller must resolve its live session again after taking this token; if it
/// changed, dropping the token abandons admission without starting any work.
pub(crate) struct TaskAdmission(super::SessionSpawnAttempt);

pub(crate) fn admit_owned_task(owner_session_id: &str) -> Result<TaskAdmission> {
    if owner_session_id.trim().is_empty() {
        return Err(Error::tool("jobs", "PI_JOBS_SESSION_UNAVAILABLE: missing task owner"));
    }
    let admission = TaskAdmission(capture_session_spawn_generation(owner_session_id)?);
    #[cfg(test)]
    super::invoke_after_initial_owner_check_hook(owner_session_id);
    Ok(admission)
}

/// Launch admitted native work under its original capabilities and budget.
/// The caller must finish input validation, approval and owner selection
/// before calling. The future must own its inputs and clean up on drop.
/// Session shutdown, capacity, retention and notifications are shared with
/// shell jobs. A timeout or cancellation never publishes a success result.
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::significant_drop_tightening)]
pub(crate) fn spawn_owned_task<F>(
    admission: TaskAdmission,
    label: &str,
    owner: AgentCx,
    timeout: Duration,
    artifact_root: Option<&Path>,
    privacy: AuxiliaryPrivacySnapshot,
    future: F,
) -> Result<JobSnapshot>
where
    F: Future<Output = Result<ToolOutput>> + Send + 'static,
{
    // Even rejected, never-polled work is retired under its original owner.
    let work = OwnedWork { owner, future: Some(future) };
    check_owner(&work.owner)?;
    let owner_session_id = admission.0.owner_session_id.as_str();
    if timeout.is_zero() || timeout > Duration::from_hours(24) {
        return Err(Error::tool("jobs", "PI_JOBS_TASK_TIMEOUT: task budget must be positive and at most 24 hours"));
    }
    if label.is_empty() || label.len() > MAX_TASK_LABEL_BYTES || label.chars().any(char::is_control) {
        return Err(Error::tool("jobs", "PI_JOBS_TASK_LABEL: use a non-empty label of at most 256 bytes without control characters"));
    }
    let protected_label = privacy.project_json(&serde_json::Value::String(label.to_string()))?;
    let Some(label) = protected_label.as_str() else {
        return Err(Error::tool("jobs", "PI_JOBS_PRIVACY: task label projection changed shape"));
    };
    let started = Instant::now();
    let _lifecycle = lifecycle_lock().lock()
        .map_err(|_| Error::tool("jobs", "jobs lifecycle lock poisoned"))?;
    ensure_session_spawn_generation(owner_session_id, admission.0.generation())?;
    let (id, sequence, slot) = reserve_job_slot()?;
    let (artifact_path, artifact, artifact_cleanup) = task_artifact(artifact_root, &id)?;
    let artifact = Arc::new(Mutex::new(ArtifactSink::new(artifact, MAX_ARTIFACT_BYTES)));
    let tail = Arc::new(Mutex::new(TailBuffer::new(OUTPUT_TAIL_BYTES)));
    check_owner(&work.owner)?;
    // Shutdown can publish its fence while artifact I/O holds the lifecycle
    // lane. Recheck both generation and cancellation before publication.
    ensure_session_spawn_generation(owner_session_id, admission.0.generation())?;
    let source = {
        let mut reg = registry().lock()
            .map_err(|_| Error::tool("jobs", "jobs registry poisoned"))?;
        let generation = reg.owner_shutdown_generations.get(owner_session_id)
            .copied().unwrap_or_default();
        if reg.closing_owners.contains_key(owner_session_id)
            || generation != admission.0.generation()
        {
            return Err(session_closing_error(owner_session_id));
        }
        let entry = JobEntry {
            schema: TASK_SCHEMA,
            result: None,
            result_bytes: 0,
            owner_session_id: owner_session_id.to_string(),
            delivery: JobDelivery::Background,
            id: id.clone(),
            command: label.to_string(),
            started_at_ms: now_ms(),
            sequence,
            settled_sequence: None,
            status: JobStatus::Running,
            exit_code: None,
            pid: None,
            artifact_path,
            artifact_cleanup,
            tail: Arc::clone(&tail),
            artifact: Arc::clone(&artifact),
            output_complete: false,
            cancel_requested: false,
            // Means live owned work, including a host future with no PID.
            process_live: true,
            settled_snapshot: Arc::new(Mutex::new(None)),
            settled_notify: Arc::new(Notify::new()),
            cancel_deadline: Arc::new(CancelDeadline::new()),
        };
        let source = JobSnapshotSource::from_entry(&entry);
        reg.jobs.insert(id.clone(), entry);
        slot.commit(&mut reg);
        source
    };
    // The caller retains cleanup ownership if the OS refuses a thread.
    let resources = Arc::new(Mutex::new(Some(TaskResources {
        id: id.clone(), artifact, tail, started: false, settled: false,
    })));
    let worker_resources = Arc::clone(&resources);
    let worker = std::thread::Builder::new().name(format!("pi-task-{id}"))
        .spawn(move || {
            let Some(mut resources) = worker_resources.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner).take()
            else { return; };
            resources.started = true;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<TaskOutcome> {
                let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build()
                    .map_err(|_| Error::tool("jobs", "PI_JOBS_TASK_RUNTIME: unable to start task runtime"))?;
                Ok(runtime.block_on(work.run(&resources.id, started, timeout)))
            }));
            let (status, result) = match outcome {
                Ok(Ok(TaskOutcome::Completed(result))) => {
                    let output = result.unwrap_or_else(|error| error_output(&error.to_string()));
                    let status = if output.is_error { JobStatus::Failed } else { JobStatus::Exited };
                    (status, output)
                }
                Ok(Ok(TaskOutcome::Cancelled)) => (JobStatus::Killed, error_output(
                    "PI_JOBS_TASK_CANCELLED: task cancelled; prior side effects may remain",
                )),
                Ok(Ok(TaskOutcome::TimedOut)) => (JobStatus::TimedOut, error_output(
                    "PI_JOBS_TASK_TIMEOUT: task budget exhausted; prior side effects may remain",
                )),
                Ok(Err(_)) => (JobStatus::Failed, error_output("PI_JOBS_TASK_RUNTIME: unable to start task runtime")),
                Err(_) => (JobStatus::Failed, error_output("PI_JOBS_TASK_PANIC: background task failed; prior side effects may remain")),
            };
            resources.finish(status, result, &privacy);
        });
    if let Err(error) = worker {
        let cleanup = resources.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        drop(cleanup);
        registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner).jobs.remove(&id);
        return Err(Error::tool("jobs", format!("PI_JOBS_TASK_SPAWN: {error}")));
    }
    Ok(JobSnapshot::from_source_best_effort(&source))
}

fn check_owner(owner: &AgentCx) -> Result<()> {
    owner.checkpoint().map_err(|_| Error::tool("jobs", "PI_JOBS_TASK_CANCELLED: task owner cancelled"))?;
    let capabilities = owner.capabilities();
    if !capabilities.spawn || !capabilities.io || !capabilities.time {
        return Err(Error::tool("jobs", "PI_JOBS_TASK_CAPABILITY: background tasks require the owner's spawn, I/O and timer capabilities"));
    }
    Ok(())
}

fn task_artifact(
    root: Option<&Path>, id: &str,
) -> Result<(std::path::PathBuf, std::fs::File, ArtifactCleanupOutcome)> {
    let root = root.map_or_else(
        || crate::config::Config::global_dir().join("tool-output-artifacts"), Path::to_path_buf,
    );
    let directory = root.join("jobs");
    std::fs::create_dir_all(&directory)
        .map_err(|error| Error::tool("jobs", format!("PI_JOBS_TASK_ARTIFACT: {error}")))?;
    let _budget = acquire_artifact_budget_lock(&directory)?;
    let cleanup = enforce_artifact_retention(
        &directory, ArtifactRetentionPolicy::from_environment()?, MAX_TOTAL_ARTIFACT_BYTES,
        MAX_ARTIFACT_FILES, MIN_RETAINED_ARTIFACT_FILES,
    )?;
    let (path, file) = create_job_artifact(&directory, id)
        .map_err(|error| Error::tool("jobs", format!("PI_JOBS_TASK_ARTIFACT: {error}")))?;
    Ok((path, file, cleanup))
}

struct OwnedWork<F> {
    owner: AgentCx,
    future: Option<F>,
}

impl<F> Drop for OwnedWork<F> {
    fn drop(&mut self) {
        let _guard = self.owner.cx().clone().set_current_restricted();
        drop(self.future.take());
    }
}

enum TaskOutcome {
    Completed(Result<ToolOutput>),
    Cancelled,
    TimedOut,
}

impl<F: Future<Output = Result<ToolOutput>>> OwnedWork<F> {
    async fn run(mut self, id: &str, started: Instant, timeout: Duration) -> TaskOutcome {
        let Some(future) = self.future.take() else {
            return TaskOutcome::Completed(Err(Error::tool("jobs", "PI_JOBS_TASK_STATE: missing task future")));
        };
        let mut future = Box::pin(self.owner.with_current(future));
        loop {
            if task_cancelled(id) || self.owner.checkpoint().is_err() {
                return TaskOutcome::Cancelled;
            }
            if started.elapsed() >= timeout {
                return TaskOutcome::TimedOut;
            }
            // The monitor's timer lives in this worker runtime. Only the
            // task future is polled under the captured caller's context, so
            // a cancelled caller cannot park its own cleanup monitor.
            let sleep = Box::pin(asupersync::time::sleep(
                asupersync::time::wall_now(), TASK_POLL_INTERVAL,
            ));
            match futures::future::select(future, sleep).await {
                futures::future::Either::Left((result, _)) => {
                    if task_cancelled(id) || self.owner.checkpoint().is_err() {
                        return TaskOutcome::Cancelled;
                    }
                    return if started.elapsed() >= timeout {
                        TaskOutcome::TimedOut
                    } else {
                        TaskOutcome::Completed(result)
                    };
                }
                futures::future::Either::Right(((), pending)) => future = pending,
            }
        }
    }
}

fn task_cancelled(id: &str) -> bool {
    registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .jobs.get(id).is_none_or(|job| job.cancel_requested)
}

struct TaskResources {
    id: String,
    artifact: Arc<Mutex<ArtifactSink>>,
    tail: Arc<Mutex<TailBuffer>>,
    started: bool,
    settled: bool,
}

impl TaskResources {
    fn finish(&mut self, mut status: JobStatus, mut result: ToolOutput, privacy: &AuxiliaryPrivacySnapshot) {
        {
            let mut reg = registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(job) = reg.jobs.get_mut(&self.id) {
                // Completion/cancellation linearizes before any result is
                // projected or persisted. No late cancellation can change a
                // completed task's artifact, notice or structured result.
                if job.cancel_requested && !matches!(status, JobStatus::Killed | JobStatus::TimedOut) {
                    status = JobStatus::Killed;
                    result = error_output("PI_JOBS_TASK_CANCELLED: task cancelled during result publication; prior side effects may remain");
                }
                job.process_live = false;
            }
        }
        let result = match protect_result(&result, privacy) {
            Ok(result) => result,
            Err(_) => {
                if matches!(status, JobStatus::Exited | JobStatus::Failed) { status = JobStatus::Failed; }
                error_output("PI_JOBS_TASK_RESULT_WITHHELD: result exceeded the 1 MiB budget or failed privacy projection; no raw output was retained")
            }
        };
        let encoded = serde_json::to_vec(&result).unwrap_or_default();
        {
            let mut artifact = self.artifact.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            artifact.write(&encoded);
            artifact.seal();
        }
        {
            let mut tail = self.tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for block in &result.content {
                if let ContentBlock::Text(text) = block {
                    tail.push(text.text.as_bytes());
                    tail.push(b"\n");
                }
            }
            tail.seal();
        }
        {
            let mut reg = registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(job) = reg.jobs.get_mut(&self.id) {
                job.result = Some(result);
                job.result_bytes = encoded.len();
            }
        }
        settle_job_and_enqueue_notice(&self.id, status, None, true);
        self.settled = true;
    }
}

impl Drop for TaskResources {
    fn drop(&mut self) {
        self.artifact.lock().unwrap_or_else(std::sync::PoisonError::into_inner).seal();
        self.tail.lock().unwrap_or_else(std::sync::PoisonError::into_inner).seal();
        if !self.settled {
            if self.started {
                settle_job_and_enqueue_notice(&self.id, JobStatus::Failed, None, false);
            } else {
                settle_job_without_notice(&self.id, JobStatus::Failed, None, false);
            }
        }
    }
}

fn error_output(message: &str) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(message))],
        details: None,
        is_error: true,
    }
}

/// Serialization stops at the bound before allocating a JSON value or copying
/// an oversized child result. A partial projection is never published.
struct BoundedJson(Vec<u8>);

impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_TASK_RESULT_BYTES.saturating_sub(self.0.len()) {
            return Err(std::io::Error::other("PI_JOBS_TASK_RESULT_LIMIT"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn protect_result(result: &ToolOutput, privacy: &AuxiliaryPrivacySnapshot) -> Result<ToolOutput> {
    let mut encoded = BoundedJson(Vec::new());
    serde_json::to_writer(&mut encoded, result)
        .map_err(|_| Error::tool("jobs", "PI_JOBS_TASK_RESULT_LIMIT: result exceeds 1 MiB"))?;
    let value = serde_json::from_slice(&encoded.0)?;
    let projected = privacy.project_json(&value)?;
    encoded.0.clear();
    serde_json::to_writer(&mut encoded, &projected)
        .map_err(|_| Error::tool("jobs", "PI_JOBS_TASK_RESULT_LIMIT: projected result exceeds 1 MiB"))?;
    serde_json::from_value(projected).map_err(|error| Error::tool("jobs", format!("PI_JOBS_PRIVACY: invalid result projection: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_completion::AuxiliaryPrivacy;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Poll};

    #[allow(clippy::too_many_arguments)]
    fn spawn_owned_task<F>(
        owner_session_id: &str,
        label: &str,
        owner: AgentCx,
        timeout: Duration,
        artifact_root: Option<&Path>,
        privacy: AuxiliaryPrivacySnapshot,
        future: F,
    ) -> Result<JobSnapshot>
    where F: Future<Output = Result<ToolOutput>> + Send + 'static {
        super::spawn_owned_task(admit_owned_task(owner_session_id)?, label, owner, timeout, artifact_root, privacy, future)
    }

    fn privacy() -> AuxiliaryPrivacySnapshot {
        AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(None))
    }

    fn output(text: &str) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(json!({"answer": 42, "items": [1, 2, 3]})),
            is_error: false,
        }
    }

    fn owner_id() -> String { format!("task-test-{}", uuid::Uuid::new_v4().simple()) }

    fn settle(owner: &str, id: &str) -> JobSnapshot {
        let snapshot = super::super::wait(owner, id, Duration::from_secs(10)).expect("job settles"); // ubs:ignore test helper requires bounded native settlement
        assert_ne!(snapshot.status, "running", "background task must terminate"); // ubs:ignore test assertion rejects an unfinished task
        snapshot
    }

    struct PendingDrop {
        dropped: Arc<AtomicBool>,
        restricted_drop: Option<Arc<AtomicBool>>,
    }

    impl Future for PendingDrop {
        type Output = Result<ToolOutput>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> { Poll::Pending }
    }

    impl Drop for PendingDrop {
        fn drop(&mut self) {
            if let Some(observed) = &self.restricted_drop {
                observed.store(asupersync::Cx::current().is_some_and(|cx| !cx.capabilities().io), Ordering::Release);
            }
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[test]
    fn task_returns_before_completion_and_retains_structured_result_for_its_owner() {
        let _guard = super::super::process_test_guard();
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore isolated artifact directory is required fixture setup
        let owner = owner_id();
        let (release, ready) = futures::channel::oneshot::channel::<()>();
        let started = spawn_owned_task(&owner, "test task", AgentCx::for_request(), Duration::from_secs(30), Some(root.path()), privacy(), async move { // ubs:ignore fixture launch must succeed before testing retained results
            ready.await.map_err(|_| Error::tool("test", "gate disconnected"))?;
            Ok(output("native result"))
        }).expect("start task"); // ubs:ignore fixture launch failure invalidates this result-retention regression
        assert_eq!(started.status, "running"); // ubs:ignore assert immediate admission before the gated future completes
        assert_eq!(started.schema, TASK_SCHEMA); // ubs:ignore assert native task schema rather than shell-job schema
        assert!(started.pid.is_none()); // ubs:ignore native futures must not claim a process ID
        assert!(super::super::wait("foreign-owner", &started.id, Duration::ZERO).is_err()); // ubs:ignore assert cross-session wait is denied
        assert!(super::super::cancel("foreign-owner", &started.id).is_err()); // ubs:ignore assert cross-session cancellation is denied
        assert!(super::super::list("foreign-owner").expect("foreign list").is_empty()); // ubs:ignore foreign-owner listing must succeed without exposing the task
        release.send(()).expect("release task"); // ubs:ignore a live fixture receiver is required to release the gated task
        let settled = settle(&owner, &started.id);
        assert_eq!(settled.status, "exited"); // ubs:ignore assert successful terminal publication
        let result = settled.result.as_ref().expect("native result retained"); // ubs:ignore retained native result is the behavior under test
        assert!(!result.is_error); // ubs:ignore assert the successful future stays successful
        assert_eq!(result.details, Some(json!({"answer":42,"items":[1,2,3]}))); // ubs:ignore assert structured details survive background execution
        let artifact: ToolOutput = serde_json::from_slice(&std::fs::read(&settled.artifact_path).expect("artifact")).expect("complete native JSON"); // ubs:ignore fixture artifact must contain complete native JSON
        assert_eq!(artifact.details, result.details); // ubs:ignore assert artifact and retained-result consistency
        assert_eq!(super::super::take_completion_notices(&owner).len(), 1); // ubs:ignore assert exactly one owner notice
        assert!(super::super::take_completion_notices("foreign-owner").is_empty()); // ubs:ignore assert completion does not leak to another session
    }

    #[test]
    fn cancelling_a_quiet_task_drops_its_future_before_terminal_publication() {
        let _guard = super::super::process_test_guard();
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore cancellation fixture requires isolated artifacts
        let owner = owner_id();
        let dropped = Arc::new(AtomicBool::new(false));
        let started = spawn_owned_task(&owner, "quiet task", AgentCx::for_request(), Duration::from_secs(30), Some(root.path()), privacy(), PendingDrop { dropped: Arc::clone(&dropped), restricted_drop: None }).expect("start task"); // ubs:ignore pending-future fixture must launch before cancellation
        let settled = super::super::cancel(&owner, &started.id).expect("cancel task"); // ubs:ignore native cancellation must complete in this regression
        assert_eq!(settled.status, "killed"); // ubs:ignore assert cancellation terminal state
        assert!(dropped.load(Ordering::Acquire), "future cleanup precedes completed cancellation"); // ubs:ignore assert cleanup happens before cancellation is observable
        assert!(settled.result.as_ref().expect("cancellation result").is_error); // ubs:ignore assert cancellation retains an error result
        let artifact: ToolOutput = serde_json::from_slice(&std::fs::read(&settled.artifact_path).expect("artifact")).expect("cancelled JSON"); // ubs:ignore cancellation artifact must remain valid complete JSON
        assert!(artifact.is_error); // ubs:ignore assert cancellation artifact agrees with terminal state
        assert!(settled.output_tail.contains("CANCELLED")); // ubs:ignore assert tail reports cancellation
    }

    #[test]
    fn ready_after_deadline_cannot_be_published_as_success() {
        let _guard = super::super::process_test_guard();
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore timeout fixture requires isolated artifacts
        let owner = owner_id();
        let started = spawn_owned_task(&owner, "late task", AgentCx::for_request(), Duration::from_millis(50), Some(root.path()), privacy(), async { // ubs:ignore fixture must launch before exercising a late-ready outcome
            std::thread::sleep(Duration::from_millis(100));
            Ok(output("late success must be discarded"))
        }).expect("start task"); // ubs:ignore late-ready fixture launch is required setup
        let settled = settle(&owner, &started.id);
        assert_eq!(settled.status, "timedOut"); // ubs:ignore assert the deadline wins over late readiness
        assert!(settled.result.as_ref().expect("timeout result").is_error); // ubs:ignore timeout must publish an error result
        let artifact = std::fs::read_to_string(&settled.artifact_path).expect("artifact"); // ubs:ignore read the fixture artifact to verify rejected success is absent
        assert!(!artifact.contains("late success")); // ubs:ignore assert no late success reaches durable output
        assert!(!settled.output_tail.contains("late success")); // ubs:ignore assert no late success reaches the tail
    }

    #[test]
    fn session_shutdown_cancels_owned_tasks_and_preserves_other_sessions() {
        let _guard = super::super::process_test_guard();
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore shutdown fixture requires isolated artifacts
        let first_owner = owner_id();
        let second_owner = owner_id();
        let first = spawn_owned_task(&first_owner, "first task", AgentCx::for_request(), Duration::from_secs(30), Some(root.path()), privacy(), futures::future::pending()).expect("first task"); // ubs:ignore first owner must have a live fixture task
        let second = spawn_owned_task(&second_owner, "second task", AgentCx::for_request(), Duration::from_secs(30), Some(root.path()), privacy(), futures::future::pending()).expect("second task"); // ubs:ignore second owner must have a live isolation-control task
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().expect("runtime"); // ubs:ignore native shutdown fixture requires its runtime
        runtime.block_on(super::super::kill_session(&first_owner)).expect("owner shutdown"); // ubs:ignore owner shutdown must complete for the isolation assertion
        assert_eq!(settle(&first_owner, &first.id).status, "killed"); // ubs:ignore assert shutdown retires its own task
        assert_eq!(super::super::wait(&second_owner, &second.id, Duration::ZERO).expect("other owner").status, "running"); // ubs:ignore assert shutdown preserves the other owner's task
        super::super::cancel(&second_owner, &second.id).expect("second cleanup"); // ubs:ignore cleanup must retire the control task
    }

    #[test]
    fn original_owner_cancellation_is_observed_by_background_monitor() {
        let _guard = super::super::process_test_guard();
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore owner-cancellation fixture requires isolated artifacts
        let owner = owner_id();
        let context = AgentCx::for_request();
        let started = spawn_owned_task(&owner, "owner-cancel task", context.clone(), Duration::from_secs(30), Some(root.path()), privacy(), futures::future::pending()).expect("start task"); // ubs:ignore original-context cancellation requires a live fixture task
        context.set_cancel_requested(true);
        assert_eq!(settle(&owner, &started.id).status, "killed"); // ubs:ignore assert the monitor observes original-owner cancellation
    }

    #[test]
    fn shutdown_generation_fences_a_task_waiting_to_enter_admission() {
        let _guard = super::super::process_test_guard();
        struct RestoreHooks;
        impl Drop for RestoreHooks {
            fn drop(&mut self) {
                *crate::jobs::spawn_background_test_hooks().lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = crate::jobs::SpawnBackgroundTestHooks::default();
            }
        }
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore admission-race fixture requires isolated artifacts
        let owner = owner_id();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = Arc::new(Mutex::new(release_rx));
        *crate::jobs::spawn_background_test_hooks().lock().expect("hooks") = crate::jobs::SpawnBackgroundTestHooks { // ubs:ignore serialized test hook installation must succeed
            owner_session_id: Some(owner.clone()),
            after_initial_owner_check: Some(Arc::new(move || {
                entered_tx.send(()).expect("admission reached"); // ubs:ignore test must observe the paused admission seam
                release.lock().expect("release gate").recv_timeout(Duration::from_secs(10)).expect("release old admission"); // ubs:ignore bounded test gate must release the paused launch
            })),
            ..crate::jobs::SpawnBackgroundTestHooks::default()
        };
        let restore = RestoreHooks;
        let polled = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&polled);
        let task_owner = owner.clone();
        let artifact_root = root.path().to_path_buf();
        let starter = std::thread::spawn(move || {
            spawn_owned_task(&task_owner, "queued task", AgentCx::for_request(), Duration::from_secs(30), Some(&artifact_root), privacy(), async move {
                observed.store(true, Ordering::Release);
                Ok(output("stale task must not run"))
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(10)).expect("old admission paused"); // ubs:ignore test requires a proven paused admission before shutdown
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().expect("runtime"); // ubs:ignore generation-race fixture requires its shutdown runtime
        runtime.block_on(super::super::kill_session(&owner)).expect("shutdown before publication"); // ubs:ignore fixture must complete shutdown while admission is paused
        release_tx.send(()).expect("resume old admission"); // ubs:ignore release the old admission so its rejection can be observed
        let error = starter.join().expect("starter joined").expect_err("stale generation refused"); // ubs:ignore joined fixture launch must reject its stale generation
        assert!(error.to_string().contains("PI_JOBS_SESSION_CLOSING")); // ubs:ignore assert the named shutdown-generation error
        assert!(!polled.load(Ordering::Acquire)); // ubs:ignore assert stale work was never polled
        assert!(super::super::list(&owner).expect("owner list").is_empty()); // ubs:ignore assert refused admission leaves no registry entry
        drop(restore);
        let replacement = spawn_owned_task(&owner, "fresh task", AgentCx::for_request(), Duration::from_secs(30), Some(root.path()), privacy(), async { Ok(output("fresh work")) }).expect("fresh admission after quiescence"); // ubs:ignore fixture verifies a new generation can be admitted after shutdown
        assert_eq!(settle(&owner, &replacement.id).status, "exited"); // ubs:ignore assert fresh work completes after the stale generation is refused
    }

    #[test]
    fn rejected_work_is_never_polled_and_drops_under_restricted_original_owner() {
        let _guard = super::super::process_test_guard();
        let context = {
            let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
            let _scope = restricted.set_current_restricted();
            AgentCx::for_current_or_request()
        };
        let dropped = Arc::new(AtomicBool::new(false));
        let restricted_drop = Arc::new(AtomicBool::new(false));
        let owner = owner_id();
        let result = spawn_owned_task(&owner, "restricted task", context, Duration::from_secs(30), None, privacy(), PendingDrop { dropped: Arc::clone(&dropped), restricted_drop: Some(Arc::clone(&restricted_drop)) });
        assert!(result.expect_err("capability denied").to_string().contains("PI_JOBS_TASK_CAPABILITY")); // ubs:ignore assert admission refuses the restricted owner
        assert!(dropped.load(Ordering::Acquire)); // ubs:ignore assert rejected never-polled work is retired
        assert!(restricted_drop.load(Ordering::Acquire)); // ubs:ignore assert retirement uses the original restricted capabilities
        assert!(super::super::list(&owner).expect("owner list").is_empty()); // ubs:ignore assert rejected work never enters the registry
    }

    #[test]
    fn task_panic_settles_as_failure_without_exporting_panic_payload() {
        let _guard = super::super::process_test_guard();
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore panic fixture requires isolated artifacts
        let owner = owner_id();
        let started = spawn_owned_task(&owner, "panic task", AgentCx::for_request(), Duration::from_secs(30), Some(root.path()), privacy(), async { // ubs:ignore fixture must launch before triggering the deliberate panic
            panic!("private panic payload"); // ubs:ignore intentional panic fixture verifies panic retirement and result privacy
        }).expect("start task"); // ubs:ignore panic fixture launch is required setup
        let settled = settle(&owner, &started.id);
        assert_eq!(settled.status, "failed"); // ubs:ignore assert panic settles as failure
        assert!(settled.output_tail.contains("PI_JOBS_TASK_PANIC")); // ubs:ignore assert a public named panic error is retained
        assert!(!serde_json::to_string(&settled).expect("snapshot").contains("private panic payload")); // ubs:ignore serialized fixture snapshot must exclude the private panic payload
    }

    #[test]
    fn known_secrets_are_projected_across_content_details_and_artifact() {
        let _guard = super::super::process_test_guard();
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore privacy fixture requires isolated artifacts
        let owner = owner_id();
        let secret = "opaque-private-value-from-earlier-turn";
        let mut vault = crate::secrets::SecretVault::default();
        crate::secrets::obfuscate(secret, &mut vault, &[regex::Regex::new(secret).expect("pattern")]); // ubs:ignore fixed test pattern must seed the remembered-secret fixture
        let reversible = vault.mask(secret);
        let projection = AuxiliaryPrivacySnapshot::with_vault(AuxiliaryPrivacy::from_settings(None), &vault);
        let started = spawn_owned_task(&owner, "private task", AgentCx::for_request(), Duration::from_secs(30), Some(root.path()), projection, async move { // ubs:ignore privacy fixture must launch with its known-secret snapshot
            Ok(ToolOutput { content: vec![ContentBlock::Text(TextContent::new(secret))], details: Some(json!({"echo":secret})), is_error:false })
        }).expect("start task"); // ubs:ignore privacy fixture launch is required setup
        let settled = settle(&owner, &started.id);
        assert_eq!(settled.status, "exited"); // ubs:ignore assert redaction preserves a valid successful result
        for text in [serde_json::to_string(&settled).expect("snapshot"), std::fs::read_to_string(&settled.artifact_path).expect("artifact")] { // ubs:ignore inspect both complete fixture publication surfaces
            assert!(!text.contains(secret)); // ubs:ignore assert remembered secret is absent from durable and structured output
            assert!(!text.contains(&reversible), "restorable vault identifiers cannot escape"); // ubs:ignore assert reversible vault tokens cannot escape
            assert!(text.contains("<pi-secret:redacted>")); // ubs:ignore assert non-restorable redaction is present
        }
        for notice in super::super::take_completion_notices(&owner) {
            assert!(!serde_json::to_string(&notice).expect("notice").contains(secret)); // ubs:ignore assert serialized completion notices also protect the secret
        }
    }

    #[test]
    fn oversized_results_are_withheld_without_persisting_a_partial_payload() {
        let _guard = super::super::process_test_guard();
        let root = tempfile::tempdir().expect("artifact root"); // ubs:ignore size-limit fixture requires isolated artifacts
        let owner = owner_id();
        let started = spawn_owned_task(&owner, "large task", AgentCx::for_request(), Duration::from_secs(30), Some(root.path()), privacy(), async { // ubs:ignore oversize-result fixture must reach the carrier
            Ok(output(&"raw-payload-marker".repeat(MAX_TASK_RESULT_BYTES / 8)))
        }).expect("start task"); // ubs:ignore oversize fixture launch is required setup
        let settled = settle(&owner, &started.id);
        assert_eq!(settled.status, "failed"); // ubs:ignore assert an oversized result cannot be published as success
        assert!(settled.output_tail.contains("PI_JOBS_TASK_RESULT_WITHHELD")); // ubs:ignore assert the named size refusal reaches the tail
        assert!(!std::fs::read_to_string(&settled.artifact_path).expect("artifact").contains("raw-payload-marker")); // ubs:ignore assert no raw prefix is persisted when the result exceeds its bound
        assert!(settled.result.expect("withheld result").is_error); // ubs:ignore assert a withheld result remains an explicit native error
    }
}
