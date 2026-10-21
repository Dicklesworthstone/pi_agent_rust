//! Native progress, interruption, privacy and retention regressions.

use super::*;
use crate::text_completion::AuxiliaryPrivacy;
use serde_json::{Value, json};

fn privacy() -> AuxiliaryPrivacySnapshot {
    AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::default())
}

fn owner_id() -> String {
    format!("progress-test-{}", uuid::Uuid::new_v4().simple())
}

fn partial(value: Value) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(
            "Confirmed partial result",
        ))],
        details: Some(value),
        is_error: false,
    }
}

fn pending(owner: &str, root: &Path) -> Result<JobSnapshot> {
    spawn_owned_task(
        admit_owned_task(owner)?,
        "progress fixture",
        AgentCx::for_request(),
        Duration::from_secs(30),
        Some(root),
        privacy(),
        futures::future::pending(),
    )
}

fn snapshot_result(snapshot: &JobSnapshot) -> Result<Value> {
    let result = snapshot
        .result
        .as_ref()
        .ok_or_else(|| Error::tool("test", "expected retained native result"))?;
    Ok(serde_json::to_value(result)?)
}

#[test]
fn latest_progress_survives_cancel_without_repeated_artifact_or_notice_writes() -> Result<()> {
    let _guard = super::super::process_test_guard();
    let root = tempfile::tempdir()?;
    let owner = owner_id();
    let job = pending(&owner, root.path())?;
    publish_owned_task_progress(&owner, &job.id, partial(json!({"answer":1})), &privacy())?;
    let first = super::super::wait(&owner, &job.id, Duration::ZERO)?;
    assert_eq!(first.status, "running"); // ubs:ignore -- Progress must not settle the native task.
    assert_eq!(
        snapshot_result(&first)?["details"]["schema"],
        PROGRESS_SCHEMA
    ); // ubs:ignore -- Running results must use the explicit partial wrapper.
    assert!(
        publish_owned_task_progress("foreign", &job.id, partial(json!({"answer":9})), &privacy())
            .is_err()
    ); // ubs:ignore -- A foreign owner cannot replace retained evidence.
    assert!(clear_owned_task_progress("foreign", &job.id).is_err()); // ubs:ignore -- A foreign owner cannot withdraw another session's evidence.
    publish_owned_task_progress(&owner, &job.id, partial(json!({"answer":2})), &privacy())?;
    assert_eq!(std::fs::metadata(&job.artifact_path)?.len(), 0); // ubs:ignore -- Repeated progress is memory-only, not an accumulating artifact.
    assert!(super::super::take_completion_notices(&owner).is_empty()); // ubs:ignore -- Progress polling must not generate duplicate completion notices.
    let stopped = super::super::cancel(&owner, &job.id)?;
    assert_eq!(stopped.status, "killed"); // ubs:ignore -- Interruption stays a failed native lifecycle outcome.
    let result = snapshot_result(&stopped)?;
    assert_eq!(result["isError"], true); // ubs:ignore -- Retained partial evidence must never imply success.
    assert_eq!(result["details"]["schema"], INTERRUPTED_SCHEMA); // ubs:ignore -- Terminal cancellation must not retain a running wrapper.
    assert_eq!(result["details"]["partialResult"]["details"]["answer"], 2); // ubs:ignore -- Keep the latest confirmed observation rather than the first.
    let artifact: ToolOutput = serde_json::from_slice(&std::fs::read(&stopped.artifact_path)?)?;
    assert_eq!(serde_json::to_value(artifact)?, result); // ubs:ignore -- The one final artifact must agree with the native terminal snapshot.
    assert!(
        publish_owned_task_progress(&owner, &job.id, partial(json!({"answer":3})), &privacy())
            .is_err()
    ); // ubs:ignore -- Cancellation fences late publication.
    assert!(clear_owned_task_progress(&owner, &job.id).is_err()); // ubs:ignore -- Settled observations are immutable.
    assert_eq!(
        snapshot_result(&super::super::wait(&owner, &job.id, Duration::ZERO)?)?,
        result
    ); // ubs:ignore -- A late writer cannot alter the permanent terminal result.
    assert_eq!(super::super::take_completion_notices(&owner).len(), 1); // ubs:ignore -- Exactly one final completion notice is delivered.
    Ok(())
}

#[test]
fn rejected_typed_or_oversized_progress_withdraws_the_prior_observation() -> Result<()> {
    let _guard = super::super::process_test_guard();
    let root = tempfile::tempdir()?;
    let owner = owner_id();
    let job = pending(&owner, root.path())?;
    for rejected in [
        json!({"api_key":1_234_567_890_123_456_u64}),
        json!({"large":"x".repeat(MAX_TASK_RESULT_BYTES)}),
    ] {
        publish_owned_task_progress(
            &owner,
            &job.id,
            partial(json!({"answer":"earlier evidence"})),
            &privacy(),
        )?;
        assert!(
            publish_owned_task_progress(&owner, &job.id, partial(rejected), &privacy()).is_err()
        ); // ubs:ignore -- Numeric secret projection and over-budget output must both refuse publication.
        assert!(
            super::super::wait(&owner, &job.id, Duration::ZERO)?
                .result
                .is_none()
        ); // ubs:ignore -- A rejected new aggregate invalidates any older retained observation synchronously.
    }
    let stopped = super::super::cancel(&owner, &job.id)?;
    assert!(snapshot_result(&stopped)?["details"].is_null()); // ubs:ignore -- Cancellation cannot resurrect evidence withdrawn by privacy or bounds checks.
    assert!(!std::fs::read_to_string(&stopped.artifact_path)?.contains("earlier evidence")); // ubs:ignore -- Withdrawn evidence must not reach the terminal artifact.
    Ok(())
}

#[test]
fn generic_failure_discards_partial_results_instead_of_claiming_interruption() -> Result<()> {
    let _guard = super::super::process_test_guard();
    let root = tempfile::tempdir()?;
    let owner = owner_id();
    let (release, ready) = futures::channel::oneshot::channel::<()>();
    let job = spawn_owned_task(
        admit_owned_task(&owner)?,
        "failing progress fixture",
        AgentCx::for_request(),
        Duration::from_secs(30),
        Some(root.path()),
        privacy(),
        async move {
            ready
                .await
                .map_err(|_| Error::tool("test", "fixture gate closed"))?;
            Err(Error::tool("test", "controlled task failure"))
        },
    )?;
    publish_owned_task_progress(
        &owner,
        &job.id,
        partial(json!({"answer":"untrusted old evidence"})),
        &privacy(),
    )?;
    release
        .send(())
        .map_err(|()| Error::tool("test", "fixture task stopped early"))?;
    let stopped = super::super::wait(&owner, &job.id, Duration::from_secs(10))?;
    assert_eq!(stopped.status, "failed"); // ubs:ignore -- Ordinary failure must remain distinct from cancellation and timeout.
    assert!(snapshot_result(&stopped)?["details"].is_null()); // ubs:ignore -- Failed tasks do not preserve potentially invalid old aggregates.
    assert!(!std::fs::read_to_string(&stopped.artifact_path)?.contains("untrusted old evidence")); // ubs:ignore -- A failed final publication cannot persist stale progress.
    Ok(())
}

#[test]
fn absolute_deadline_cannot_be_renewed_and_timeout_keeps_confirmed_progress() -> Result<()> {
    let _guard = super::super::process_test_guard();
    let root = tempfile::tempdir()?;
    let owner = owner_id();
    let deadline = Instant::now() + Duration::from_secs(1);
    let admission = admit_owned_task(&owner)?
        .with_deadline(deadline)
        .with_deadline(deadline + Duration::from_secs(20));
    let job = spawn_owned_task(
        admission,
        "deadline progress fixture",
        AgentCx::for_request(),
        Duration::from_secs(30),
        Some(root.path()),
        privacy(),
        futures::future::pending(),
    )?;
    publish_owned_task_progress(&owner, &job.id, partial(json!({"answer":42})), &privacy())?;
    let stopped = super::super::wait(&owner, &job.id, Duration::from_secs(10))?;
    assert_eq!(stopped.status, "timedOut"); // ubs:ignore -- The original absolute deadline beats both a later bound and the launch duration.
    let result = snapshot_result(&stopped)?;
    assert_eq!(result["details"]["schema"], INTERRUPTED_SCHEMA); // ubs:ignore -- Deadline exhaustion preserves only an explicit interrupted observation.
    assert_eq!(result["details"]["partialResult"]["details"]["answer"], 42); // ubs:ignore -- Completed evidence survives timeout without claiming the remaining work completed.
    Ok(())
}

fn registered_resources(owner: &str, root: &Path) -> TaskResources {
    let id = format!("progress-resource-{}", uuid::Uuid::new_v4().simple());
    let mut entry = super::super::tests::synthetic_entry(&id, root.join("artifact.json"), 0, true);
    entry.schema = TASK_SCHEMA;
    entry.owner_session_id = owner.to_owned();
    entry.pid = None;
    let resources = TaskResources {
        id: id.clone(),
        artifact: Arc::clone(&entry.artifact),
        tail: Arc::clone(&entry.tail),
        started: true,
        settled: false,
    };
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .jobs
        .insert(id, entry);
    resources
}

#[test]
fn privacy_withdrawal_wins_even_after_cancellation_and_session_closing() -> Result<()> {
    let _guard = super::super::process_test_guard();
    let root = tempfile::tempdir()?;
    let owner = owner_id();
    let mut resources = registered_resources(&owner, root.path());
    publish_owned_task_progress(
        &owner,
        &resources.id,
        partial(json!({"answer":"withdrawn"})),
        &privacy(),
    )?;
    let shutdown = super::super::SessionShutdownAttempt::begin(&owner)?;
    super::super::request_cancel(&owner, &resources.id)?;
    assert!(
        publish_owned_task_progress(
            &owner,
            &resources.id,
            partial(json!({"answer":"late"})),
            &privacy()
        )
        .is_err()
    ); // ubs:ignore -- Closing and cancellation refuse new evidence before terminal settlement.
    clear_owned_task_progress(&owner, &resources.id)?;
    resources.finish(
        JobStatus::Failed,
        error_output("privacy refusal"),
        &privacy(),
    );
    let stopped = super::super::wait(&owner, &resources.id, Duration::ZERO)?;
    assert_eq!(stopped.status, "killed"); // ubs:ignore -- The real cancellation fence wins the terminal-state race.
    assert!(snapshot_result(&stopped)?["details"].is_null()); // ubs:ignore -- Winning cancellation must not revive synchronously withdrawn evidence.
    shutdown.finish_success()?;
    Ok(())
}

#[test]
fn emergency_resource_cleanup_cannot_settle_a_running_progress_result() -> Result<()> {
    let _guard = super::super::process_test_guard();
    let root = tempfile::tempdir()?;
    let owner = owner_id();
    let resources = registered_resources(&owner, root.path());
    let id = resources.id.clone();
    publish_owned_task_progress(
        &owner,
        &id,
        partial(json!({"answer":"incomplete publication"})),
        &privacy(),
    )?;
    drop(resources);
    let stopped = super::super::wait(&owner, &id, Duration::ZERO)?;
    assert_eq!(stopped.status, "failed"); // ubs:ignore -- The emergency cleanup path reports failure.
    assert!(!stopped.output_complete); // ubs:ignore -- A failed publication cannot claim a complete artifact.
    let result = snapshot_result(&stopped)?;
    assert_eq!(result["isError"], true); // ubs:ignore -- A running success-shaped progress wrapper must never become the terminal result.
    assert!(result["details"].is_null()); // ubs:ignore -- Emergency cleanup removes partial evidence before freezing the snapshot.
    assert!(clear_owned_task_progress(&owner, &id).is_err()); // ubs:ignore -- Cleanup also fences late progress withdrawal and publication.
    Ok(())
}

#[test]
fn live_progress_counts_toward_retention_without_evicting_live_work() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut reg = super::super::JobRegistry::default();
    for index in 0..32_u64 {
        let id = format!("retained-{index:02}");
        let mut entry =
            super::super::tests::synthetic_entry(&id, root.path().join(&id), index, false);
        entry.status = JobStatus::Exited;
        entry.settled_sequence = Some(index);
        entry.result_bytes = MAX_TASK_RESULT_BYTES;
        reg.jobs.insert(id, entry);
    }
    let mut live = super::super::tests::synthetic_entry("live", root.path().join("live"), 32, true);
    live.schema = TASK_SCHEMA;
    live.result_bytes = MAX_TASK_RESULT_BYTES;
    reg.jobs.insert("live".into(), live);
    super::super::prune_settled_jobs(&mut reg);
    assert_eq!(reg.jobs.len(), 32); // ubs:ignore -- Live progress must count against the shared 32 MiB budget.
    assert!(!reg.jobs.contains_key("retained-00")); // ubs:ignore -- The oldest settled entry supplies the reclaimed capacity.
    assert!(reg.jobs.contains_key("retained-31") && reg.jobs.contains_key("live")); // ubs:ignore -- Recent history and live work survive eviction pressure.
    Ok(())
}
