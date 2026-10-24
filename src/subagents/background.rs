//! Explicit background delegation over the ordinary session-owned jobs runtime.
//!
//! Admission freezes a native request before the handle is published. The job
//! owns the existing ChildRunner future, never an Agent borrow or an eval
//! interpreter. Cancellation therefore retires the same process/worktree leases
//! as foreground delegation; results use the same native output contract.

use super::{
    Deadline, Error, MAX_SUBAGENT_DEPTH, PreparedRequest, RequestMode, Result, SubagentRequest,
    SubagentTool, Value, compile_output_schema, current_subagent_depth, result_output, seance,
    workflow, workspace,
};
use crate::agent_cx::AgentCx;
use crate::model::{ContentBlock, TextContent};
use crate::text_completion::AuxiliaryPrivacySnapshot;
use crate::tools::ToolOutput;
use serde_json::json;
use std::time::Duration;

pub(super) fn requested(input: &Value) -> Result<bool> {
    match input.get("async") {
        None | Some(Value::Bool(false)) => Ok(false),
        Some(Value::Bool(true)) => Ok(true),
        Some(_) => Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_BACKGROUND_INPUT: async must be a boolean",
        )),
    }
}

impl PreparedRequest {
    /// Refuse invalid launches before allocating an asynchronous handle. A
    /// parallel request is admitted as a whole, so an invalid last definition
    /// cannot leave an earlier prefix executing in the background.
    fn validate_background(&self) -> Result<()> {
        let tasks = match &self.mode {
            RequestMode::Single(task) => std::slice::from_ref(task),
            RequestMode::Parallel(tasks) | RequestMode::Chain(tasks) => tasks.as_slice(),
        };
        for task in tasks {
            let agent = self.agents.get(&task.agent).ok_or_else(|| {
                Error::tool(
                    "subagent",
                    format!(
                        "PI_SUBAGENT_BACKGROUND_INPUT: unknown agent {:?}",
                        task.agent
                    ),
                )
            })?;
            if task.task.trim().is_empty() {
                return Err(Error::tool(
                    "subagent",
                    "PI_SUBAGENT_BACKGROUND_INPUT: each task must be nonempty",
                ));
            }
            if let Some(schema) = task.output_schema.as_ref().or(agent.output_schema.as_ref()) {
                compile_output_schema(schema).map_err(|error| {
                    Error::tool("subagent", format!("Invalid outputSchema: {error}"))
                })?;
            }
        }
        self.deadline
            .check()
            .map_err(|error| Error::tool("subagent", error))
    }
}

/// Parse all graph controls before owner lookup, then freeze definitions
/// under the same admission fence as ordinary single/batch delegation.
enum Request {
    Delegation(SubagentRequest),
    Workflow(workflow::Plan),
}

impl Request {
    fn parse(input: Value) -> Result<Self> {
        if let Some(plan) = workflow::Plan::parse(&input)? {
            return Ok(Self::Workflow(plan));
        }
        workspace::validate_request(&input)?;
        let request: SubagentRequest = serde_json::from_value(input)
            .map_err(|error| Error::tool("subagent", format!("Invalid input: {error}")))?;
        request.mode_name()?;
        Ok(Self::Delegation(request))
    }

    fn deadline(&self, tool: &SubagentTool) -> Result<Deadline> {
        match self {
            Self::Delegation(request) => {
                Deadline::for_request(tool.timeout, request.timeout_seconds)
            }
            Self::Workflow(plan) => plan.deadline(tool),
        }
    }

    fn prepare(self, tool: &SubagentTool, deadline: Deadline) -> Result<Prepared> {
        match self {
            Self::Delegation(request) => {
                let mode = request.mode_name()?;
                let prepared = tool.prepare_request_with_deadline(&request, deadline)?;
                prepared.validate_background()?;
                Ok(Prepared::Delegation {
                    request: prepared,
                    mode,
                })
            }
            Self::Workflow(plan) => Ok(Prepared::Workflow(plan.prepare(tool, deadline)?)),
        }
    }
}

enum Prepared {
    Delegation {
        request: PreparedRequest,
        mode: &'static str,
    },
    Workflow(workflow::PreparedWorkflow),
}

impl Prepared {
    const fn mode(&self) -> &'static str {
        match self {
            Self::Delegation { mode, .. } => mode,
            Self::Workflow(_) => "workflow",
        }
    }

    async fn run(
        self,
        structured_results: bool,
        progress: futures::channel::oneshot::Receiver<workflow::Progress>,
    ) -> Result<ToolOutput> {
        match self {
            Self::Delegation { mut request, mode } => {
                drop(progress);
                request.runner = request.runner.into_background();
                Ok(result_output(
                    &request.run(None).await,
                    mode,
                    structured_results,
                ))
            }
            Self::Workflow(plan) => {
                // Do not start a graph before its progress sink has the actual
                // native job ID. Abandoned admission cannot leave orphan work.
                let progress = progress.await.map_err(|_| {
                    Error::tool(
                        "subagent",
                        "PI_WORKFLOW_PROGRESS_ADMISSION: job handoff did not complete",
                    )
                })?;
                plan.with_progress(progress).run(None).await
            }
        }
    }
}

/// Identity resolvers may wait on a session lock. Waiting is part of the
/// original delegation budget and cannot conceal caller cancellation.
pub(super) async fn resolve_session(
    scope: &crate::jobs::JobSessionScope,
    owner: &AgentCx,
    deadline: Deadline,
) -> Result<String> {
    let mut pending = Box::pin(owner.with_current(scope.session_id()));
    loop {
        owner.checkpoint().map_err(|_| {
            Error::tool(
                "subagent",
                "PI_SUBAGENT_CANCELLED: caller cancelled during owner lookup",
            )
        })?;
        deadline
            .check()
            .map_err(|error| Error::tool("subagent", error))?;
        let time = owner.time();
        let sleep = Box::pin(time.sleep(deadline.remaining().min(Duration::from_millis(20))));
        match futures::future::select(pending, sleep).await {
            futures::future::Either::Left((result, _)) => {
                owner.checkpoint().map_err(|_| {
                    Error::tool(
                        "subagent",
                        "PI_SUBAGENT_CANCELLED: caller cancelled during owner lookup",
                    )
                })?;
                deadline
                    .check()
                    .map_err(|error| Error::tool("subagent", error))?;
                return result;
            }
            futures::future::Either::Right(((), operation)) => pending = operation,
        }
    }
}

pub(super) async fn start(
    tool: &SubagentTool,
    mut input: Value,
    privacy: AuxiliaryPrivacySnapshot,
) -> Result<ToolOutput> {
    let owner = AgentCx::for_current_or_request();
    owner.checkpoint().map_err(|_| {
        Error::tool(
            "subagent",
            "PI_SUBAGENT_CANCELLED: caller cancelled before background admission",
        )
    })?;
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.spawn || !capabilities.time {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_PERMISSION: background delegation requires I/O, spawn and timer capabilities",
        ));
    }
    if current_subagent_depth() >= MAX_SUBAGENT_DEPTH {
        return Err(Error::tool(
            "subagent",
            format!("Refusing nested subagent depth above {MAX_SUBAGENT_DEPTH}"),
        ));
    }
    if seance::is_request(&input)? {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_BACKGROUND_INPUT: seance remains foreground; async supports single, tasks, chain or workflow delegation",
        ));
    }
    if input
        .get("timeoutSeconds")
        .is_some_and(|value| value.as_u64().is_none())
    {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_INVALID_TIMEOUT: timeoutSeconds must be an integer from 1 to 86400",
        ));
    }
    if let Some(fields) = input.as_object_mut() {
        fields.remove("async");
    }
    let request = Request::parse(input)?;
    let deadline = request.deadline(tool)?;
    // Sample now BEFORE remaining(): this bound may shorten by the sampling
    // interval, never extend the delegation's monotonic deadline.
    let job_deadline = std::time::Instant::now() + deadline.remaining();
    let session_id = resolve_session(&tool.job_session_scope, &owner, deadline).await?;
    let admission = crate::jobs::admit_owned_task(&session_id)?.with_deadline(job_deadline);
    let prepared = request.prepare(tool, deadline)?;
    let mode = prepared.mode();
    if resolve_session(&tool.job_session_scope, &owner, deadline).await? != session_id {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_SESSION_CHANGED: owner changed before background admission",
        ));
    }
    owner.checkpoint().map_err(|_| {
        Error::tool(
            "subagent",
            "PI_SUBAGENT_CANCELLED: caller cancelled before background admission",
        )
    })?;
    let timeout = deadline.remaining();
    let structured_results = tool.structured_results;
    let artifacts = tool.global_dir.join("tool-output-artifacts");
    let progress_privacy = (mode == "workflow").then(|| privacy.clone());
    let (publish, progress) = futures::channel::oneshot::channel();
    let job = crate::jobs::spawn_owned_task(
        admission,
        &format!("subagent {mode}"),
        owner,
        timeout,
        Some(&artifacts),
        privacy,
        prepared.run(structured_results, progress),
    )?;
    if let Some(privacy) = progress_privacy {
        // No await after job publication. If cancellation already retired the
        // receiver, the existing job ID still identifies its terminal outcome;
        // never submit the graph again to obtain another acknowledgment.
        let _ = publish.send(workflow::Progress::new(session_id, job.id.clone(), privacy));
    }
    let progress_notice = if mode == "workflow" {
        " Settled-wave progress is in job.result.details.partialResult; cancellation or timeout preserves the last screened checkpoint. Pending nodes there have unresolved outcomes, not guaranteed absence of side effects."
    } else {
        ""
    };
    Ok(ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(format!(
            "Background subagent {mode} started as {}. Use jobs wait/cancel with this jobId. Its settled result contains the ordinary native subagent results; completion is also delivered to the owning session.{progress_notice}",
            job.id,
        )))],
        details: Some(json!({
            "schema": "pi.subagent.background.v1",
            "mode": mode,
            "jobId": job.id,
            "job": job,
        })),
        is_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_control_is_explicit_and_never_coerces() {
        assert!(!requested(&json!({})).unwrap()); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Missing async must select foreground in this parser regression.
        assert!(!requested(&json!({"async": false})).unwrap()); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Explicit false must remain foreground.
        assert!(requested(&json!({"async": true})).unwrap()); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Only explicit true admits background execution.
        for value in [Value::Null, json!(1), json!("true"), json!([]), json!({})] {
            assert!(requested(&json!({"async": value})).is_err()); // ubs:ignore[rust.panic.assert-macros] -- Malformed async controls must be rejected by this parser regression.
        }
    }

    #[cfg(unix)]
    mod native {
        use super::*;
        use crate::tools::{Tool, ToolModelScope};
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::{Duration, Instant};

        fn fixture(script: &str) -> (tempfile::TempDir, SubagentTool, String) {
            let root = tempfile::tempdir().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Failure to create the isolated fixture invalidates the test.
            let global = root.path().join("global");
            std::fs::create_dir_all(global.join("agents")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Native discovery requires this fixture directory.
            std::fs::write(
                // ubs:ignore[rust.ownership.unwrap-expect] -- The authored agent definition is required test input.
                global.join("agents/worker.md"),
                "---\nname: worker\ndescription: background fixture\n---\nOriginal definition.\n",
            )
            .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- A missing authored definition invalidates native discovery coverage.
            let binary = root.path().join("child.sh");
            std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The real child-process regression requires its controlled executable.
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- A non-executable fixture cannot test native process lifecycle.
            let owner = format!("subagent-background-{}", uuid::Uuid::new_v4());
            let mut tool = SubagentTool::with_paths(root.path().to_path_buf(), global, binary)
                .with_timeout(Duration::from_secs(20));
            tool.bind_job_session_scope(crate::jobs::JobSessionScope::fixed(&owner));
            (root, tool, owner)
        }

        fn end(text: &str) -> String {
            let event = json!({"type":"agent_end","messages":[{
                "role":"assistant","stopReason":"stop",
                "content":[{"type":"text","text":text}]
            }]})
            .to_string();
            format!("printf '%s\\n' '{}'", event.replace('\'', "'\"'\"'"))
        }

        fn request() -> Value {
            json!({"agent":"worker","task":"finish","async":true})
        }

        async fn await_marker(path: &std::path::Path) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "child did not reach its gated fixture"
                ); // ubs:ignore[rust.panic.assert-macros] -- A stuck native fixture must fail within the test deadline.
                asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10))
                    .await;
            }
        }

        #[test]
        fn background_returns_before_completion_and_wait_preserves_typed_results() {
            let _guard = crate::jobs::process_test_guard();
            let script = format!(
                "touch launched\nwhile [ ! -f release ]; do sleep 0.02; done\n{}",
                end(r#"{"count":7}"#),
            );
            let (root, tool, owner) = fixture(&script);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The native lifecycle regression requires a functioning runtime.
            runtime.block_on(async {
                let output = tool
                    .execute(
                        "launch",
                        json!({ // ubs:ignore[rust.ownership.unwrap-expect] -- This valid native launch must produce its acknowledgment.
                            "agent":"worker","task":"typed result","async":true,
                            "outputSchema":{"type":"object","required":["count"],
                                "properties":{"count":{"type":"integer"}}},
                            "schemaMode":"strict",
                        }),
                        None,
                    )
                    .await
                    .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Failure to admit the controlled child is a regression.
                assert!(!output.is_error); // ubs:ignore[rust.panic.assert-macros] -- The valid native launch must succeed before its child is released.
                let details = output.details.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- A native background acknowledgment must carry its descriptor.
                assert_eq!(details["schema"], "pi.subagent.background.v1"); // ubs:ignore[rust.panic.assert-macros] -- Assert the public background acknowledgment schema.
                let id = details["jobId"].as_str().unwrap(); // ubs:ignore[rust.panic.direct-indexing,rust.ownership.unwrap-expect] -- This assertion requires the native acknowledgment's job identifier.
                await_marker(&root.path().join("launched")).await;
                let pending = crate::jobs::wait_async(&owner, id, Duration::ZERO)
                    .await
                    .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The owner must be able to poll its admitted native job.
                assert_eq!(pending.status, "running"); // ubs:ignore[rust.panic.assert-macros] -- The unreleased real child must still be running.
                assert!(pending.result.is_none()); // ubs:ignore[rust.panic.assert-macros] -- Running jobs must not expose a terminal result.
                assert!(crate::jobs::take_completion_notices(&owner).is_empty()); // ubs:ignore[rust.panic.assert-macros] -- An unfinished child must not publish completion.
                std::fs::write(root.path().join("release"), "continue").unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Release the controlled child only after its running-state assertions.
                let settled = crate::jobs::wait_async(&owner, id, Duration::from_secs(10))
                    .await
                    .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The released child must settle through the real jobs path.
                assert_eq!(settled.status, "exited", "{settled:?}"); // ubs:ignore[rust.panic.assert-macros] -- Releasing the child must yield successful native settlement.
                let native = settled.result.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- A settled native task must retain its complete output.
                assert!(!native.is_error); // ubs:ignore[rust.panic.assert-macros] -- The valid typed child output must remain successful.
                let result = &native.details.as_ref().unwrap()["results"][0]; // ubs:ignore[rust.panic.direct-indexing,rust.ownership.unwrap-expect] -- The single launched child must retain exactly its structured native result.
                assert_eq!(result["data"]["count"], 7); // ubs:ignore[rust.panic.assert-macros] -- Preserve the child fixture's typed data through job retention.
                assert_eq!(result["schemaValid"], true); // ubs:ignore[rust.panic.assert-macros] -- Strict output schema validation must survive background rendering.
                assert!(result["hubId"].as_str().is_some()); // ubs:ignore[rust.panic.assert-macros] -- Native background results must retain their hub reference.
                let notice = crate::jobs::take_completion_notices(&owner);
                assert_eq!(notice.len(), 1); // ubs:ignore[rust.panic.assert-macros] -- One task must publish exactly one completion notice.
                assert!(crate::jobs::take_completion_notices(&owner).is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Draining notices must not redeliver completion.
            });
        }

        #[test]
        fn background_cancel_reaps_native_child_before_terminal_publication() {
            let _guard = crate::jobs::process_test_guard();
            let (root, tool, owner) = fixture("printf '%s\\n' \"$$\" > child.pid\nexec sleep 30");
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Native cancellation coverage requires an executing runtime.
            runtime.block_on(async {
                let output = tool.execute("launch", request(), None).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The child must be admitted before its cancellation is tested.
                let id = output.details.as_ref().unwrap()["jobId"].as_str().unwrap(); // ubs:ignore[rust.panic.direct-indexing,rust.ownership.unwrap-expect] -- The cancellation regression requires the admitted job's native identifier.
                await_marker(&root.path().join("child.pid")).await;
                let pid = std::fs::read_to_string(root.path().join("child.pid")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Read the real child PID written by the gated fixture.
                let foreign = crate::jobs::cancel_async("another-session", id)
                    .await
                    .unwrap_err(); // ubs:ignore[rust.ownership.unwrap-expect] -- Foreign cancellation must fail without affecting the child.
                assert!(foreign.to_string().contains("PI_JOBS_UNKNOWN_ID")); // ubs:ignore[rust.panic.assert-macros] -- A foreign session must not cancel the owned child.
                let settled = crate::jobs::cancel_async(&owner, id).await.unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Cancellation by the actual owner must complete.
                assert_eq!(settled.status, "killed", "{settled:?}"); // ubs:ignore[rust.panic.assert-macros] -- Native cancellation must publish a killed terminal state.
                assert!(settled.result.as_ref().unwrap().is_error); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Cancellation must retain an explicit unsuccessful result.
                let status = std::process::Command::new("kill") // ubs:ignore[rust.ownership.unwrap-expect] -- The OS liveness probe independently verifies child cleanup.
                    .args(["-0", pid.trim()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Failure to execute the liveness probe invalidates this Unix fixture.
                assert!(
                    !status.success(),
                    "cancel returned while its child was alive"
                ); // ubs:ignore[rust.panic.assert-macros] -- The real OS child must be reaped before cancellation returns.
            });
        }

        #[test]
        fn invalid_background_batch_never_starts_a_valid_prefix() {
            let _guard = crate::jobs::process_test_guard();
            let (root, tool, owner) = fixture("touch launched");
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Admission validation must execute through the native async entry point.
            runtime.block_on(async {
                for input in [
                    json!({"tasks":[{"agent":"worker","task":"valid"},{"agent":"missing","task":"bad"}],"async":true}),
                    json!({"agent":"worker","task":"","async":true}),
                    json!({"workflow":[],"async":true}),
                    json!({"agent":"seance","task":"history","sourceSession":"missing.jsonl","async":true}),
                    json!({"agent":"worker","task":"bad timeout","timeoutSeconds":null,"async":true}),
                    json!({"agent":"worker","task":"bad workspace","isolation":null,"async":true}),
                ] {
                    assert!(tool.execute("invalid", input, None).await.is_err()); // ubs:ignore[rust.panic.assert-macros] -- Every malformed request must fail before dispatch.
                }
                assert!(crate::jobs::list(&owner).unwrap().is_empty()); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Invalid input must not allocate a job.
                assert!(!root.path().join("launched").exists()); // ubs:ignore[rust.panic.assert-macros] -- A valid prefix of an invalid batch must never launch.
            });
        }

        #[test]
        fn prepared_background_request_keeps_definition_and_model_snapshot() {
            let _guard = crate::jobs::process_test_guard();
            let script = format!(
                "printf '%s\\n' \"$@\" > arguments\n{}",
                end("frozen launch")
            );
            let (root, mut tool, _owner) = fixture(&script);
            let model = ToolModelScope::default();
            model.set("provider-before", "model-before");
            tool.bind_model_scope(model.clone());
            let request: SubagentRequest = serde_json::from_value(request()).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- This literal request is the valid snapshot regression input.
            let prepared = tool.prepare_request(&request).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Preparation must capture the authored model and definition.
            prepared.validate_background().unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The controlled request must pass the same background preflight.
            model.set("provider-after", "model-after");
            std::fs::write(tool.global_dir.join("agents/worker.md"), // ubs:ignore[rust.ownership.unwrap-expect] -- Deliberately replace the fixture definition after preparation.
                "---\nname: worker\ndescription: changed\nmodel: changed-model\n---\nChanged definition.\n",
            ).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The changed definition is required to exercise snapshot retention.
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Execute the prepared child through the native runner.
            let results = runtime.block_on(prepared.run(None));
            assert!(!results[0].is_error, "{:?}", results[0]); // ubs:ignore[rust.panic.assert-macros] -- The single prepared child must execute with its frozen definition.
            let argv = std::fs::read_to_string(root.path().join("arguments")).unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The real child records argv as independent snapshot evidence.
            assert!(argv.contains("model-before") && argv.contains("provider-before")); // ubs:ignore[rust.panic.assert-macros] -- Child argv must use the model captured during preparation.
            assert!(argv.contains("Original definition.")); // ubs:ignore[rust.panic.assert-macros] -- Child argv must retain the prepared prompt definition.
            assert!(!argv.contains("model-after") && !argv.contains("Changed definition.")); // ubs:ignore[rust.panic.assert-macros] -- Later settings and file changes must not alter an admitted request.
        }

        #[test]
        fn session_switch_during_preflight_refuses_background_publication() {
            let _guard = crate::jobs::process_test_guard();
            let (root, mut tool, owner) = fixture("touch launched");
            let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = count.clone();
            let original = owner.clone();
            let scope = crate::jobs::JobSessionScope::fixed(&owner);
            scope.bind(std::sync::Arc::new(move || {
                let id = if observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    original.clone()
                } else {
                    "replacement-owner".to_string()
                };
                Box::pin(async move { Some(id) })
            }));
            tool.bind_job_session_scope(scope);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- Session-change refusal must run through native admission.
            let error = runtime
                .block_on(tool.execute("switch", request(), None))
                .unwrap_err(); // ubs:ignore[rust.ownership.unwrap-expect] -- Ownership replacement is intentionally expected to fail.
            assert!(error.to_string().contains("PI_SUBAGENT_SESSION_CHANGED")); // ubs:ignore[rust.panic.assert-macros] -- Session replacement during preflight must fail explicitly.
            assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2); // ubs:ignore[rust.panic.assert-macros] -- Ownership must be checked both before and after preparation.
            assert!(crate::jobs::list(&owner).unwrap().is_empty()); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Session replacement must not publish a ghost job.
            assert!(!root.path().join("launched").exists()); // ubs:ignore[rust.panic.assert-macros] -- Rejected ownership must prevent real child launch.
        }

        #[test]
        fn a_pending_owner_lookup_observes_cancellation_without_a_job() {
            let _guard = crate::jobs::process_test_guard();
            let (root, mut tool, session_id) = fixture("touch launched");
            let owner = AgentCx::for_request();
            let cancel = owner.clone();
            let scope = crate::jobs::JobSessionScope::fixed(&session_id);
            scope.bind(std::sync::Arc::new(move || {
                let cancel = cancel.clone();
                Box::pin(async move {
                    cancel.set_cancel_requested(true);
                    futures::future::pending::<Option<String>>().await
                })
            }));
            tool.bind_job_session_scope(scope);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap(); // ubs:ignore[rust.ownership.unwrap-expect] -- The blocked-resolver fixture requires native cancellation polling.
            let error = runtime
                .block_on(owner.with_current(tool.execute("lookup", request(), None))) // ubs:ignore[rust.ownership.unwrap-expect] -- The cancelled resolver must fail rather than allocate a job.
                .unwrap_err(); // ubs:ignore[rust.ownership.unwrap-expect] -- Cancellation is the expected outcome of this regression.
            assert!(error.to_string().contains("PI_SUBAGENT_CANCELLED")); // ubs:ignore[rust.panic.assert-macros] -- Cancellation must interrupt a pending owner lookup.
            assert!(crate::jobs::list(&session_id).unwrap().is_empty()); // ubs:ignore[rust.panic.assert-macros,rust.ownership.unwrap-expect] -- Cancelled admission must leave no retained job.
            assert!(!root.path().join("launched").exists()); // ubs:ignore[rust.panic.assert-macros] -- A cancelled lookup must not reach real child dispatch.
        }
    }
}
