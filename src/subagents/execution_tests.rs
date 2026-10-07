//! Subprocess-level regressions for the native parent/child boundary.

#![cfg(unix)]

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use tempfile::TempDir;

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn emit(events: &[Value]) -> String {
    use std::fmt::Write as _;

    events.iter().fold(String::new(), |mut script, event| {
        let _ = writeln!(script, "printf '%s\\n' {}", quote(&event.to_string()));
        script
    })
}

fn assistant(text: &str, reason: &str) -> Value {
    json!({"role":"assistant","stopReason":reason,"content":[{"type":"text","text":text}]})
}

fn ended(text: &str, reason: &str) -> Value {
    json!({"type":"agent_end","messages":[assistant(text, reason)]})
}

fn fixture(script: &str) -> (TempDir, SubagentTool) {
    let dir = tempfile::tempdir().unwrap();
    let global = dir.path().join("global");
    std::fs::create_dir_all(global.join("agents")).unwrap();
    std::fs::write(
        global.join("agents/worker.md"),
        "---\nname: worker\ndescription: protocol fixture\n---\nComplete the task.",
    )
    .unwrap();
    let child = dir.path().join("child.sh");
    std::fs::write(&child, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o700)).unwrap();
    let tool = SubagentTool::with_paths(dir.path().to_path_buf(), global, child);
    (dir, tool)
}

fn run(tool: &SubagentTool, input: Value) -> ToolOutput {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(tool.execute("protocol-test", input, None))
        .unwrap()
}

fn request() -> Value {
    json!({"agent":"worker","task":"produce a result"})
}

fn result(output: &ToolOutput) -> &Value {
    &output.details.as_ref().unwrap()["results"][0]
}

#[test]
fn zero_exit_without_an_agent_completion_is_a_failed_delegation() {
    let (_dir, tool) = fixture("exit 0");
    let output = run(&tool, request());
    assert!(output.is_error);
    assert_eq!(result(&output)["status"], "failed");
    assert_eq!(result(&output)["exitCode"], 0);
    assert!(
        result(&output)["error"]
            .as_str()
            .unwrap()
            .contains("PI_SUBAGENT_INCOMPLETE")
    );
}

#[test]
fn completed_message_without_agent_end_is_not_success() {
    let (_dir, tool) = fixture(&emit(&[
        json!({"type":"message_end","message":assistant("partial run", "stop")}),
    ]));
    let output = run(&tool, request());
    assert!(output.is_error);
    assert_eq!(result(&output)["status"], "failed");
}

#[test]
fn malformed_stdout_does_not_become_an_ignored_diagnostic() {
    let (_dir, tool) = fixture("printf '%s\\n' 'not-json-secret-content'\nexit 0");
    let output = run(&tool, request());
    assert!(output.is_error);
    let encoded = serde_json::to_string(&output).unwrap();
    assert!(encoded.contains("PI_SUBAGENT_PROTOCOL"));
    assert!(!encoded.contains("not-json-secret-content"));
}

#[test]
fn final_snapshot_replaces_streaming_preview_and_excludes_reasoning() {
    let final_message = json!({"role":"assistant","stopReason":"stop","content":[
        {"type":"text","text":"final "}, {"type":"thinking","thinking":"private-thought"},
        {"type":"text","text":"answer"}
    ]});
    let events = [
        json!({"type":"message_start","message":{"role":"assistant"}}),
        json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"private-thought"}}),
        json!({"type":"message_update","assistantMessageEvent":{"type":"toolcall_delta","delta":"private-arguments"}}),
        json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"preview"}}),
        json!({"type":"message_end","message":final_message}),
        json!({"type":"agent_end","messages":[final_message]}),
    ];
    let (_dir, tool) = fixture(&emit(&events));
    let updates = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&updates);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let output = runtime
        .block_on(tool.execute(
            "preview",
            request(),
            Some(Box::new(move |update| {
                captured
                    .lock()
                    .unwrap()
                    .push(serde_json::to_string(&update).unwrap());
            })),
        ))
        .unwrap();
    assert!(!output.is_error, "{output:?}");
    assert_eq!(result(&output)["output"], "final answer");
    for update in updates.lock().unwrap().iter() {
        assert!(!update.contains("private-thought"));
        assert!(!update.contains("private-arguments"));
    }
}

#[test]
fn zero_exit_never_overrides_an_unsuccessful_terminal_reason() {
    for reason in [
        "error",
        "aborted",
        "refusal",
        "length",
        "toolUse",
        "pauseTurn",
    ] {
        let (_dir, tool) = fixture(&emit(&[ended("partial result", reason)]));
        let output = run(&tool, request());
        assert!(output.is_error, "{reason}: {output:?}");
        assert_eq!(result(&output)["status"], "failed");
    }
}

#[test]
fn agent_end_error_is_not_hidden_by_a_successful_assistant_snapshot() {
    let mut event = ended("looks complete", "stop");
    event["error"] = json!("secret provider diagnostic");
    let (_dir, tool) = fixture(&emit(&[event]));
    let output = run(&tool, request());
    assert!(output.is_error);
    assert!(
        !serde_json::to_string(&output)
            .unwrap()
            .contains("secret provider diagnostic")
    );
}

#[test]
fn nonzero_process_exit_is_failure_even_after_a_valid_agent_end() {
    let (_dir, tool) = fixture(&format!("{}exit 7\n", emit(&[ended("answer", "stop")])));
    let output = run(&tool, request());
    assert!(output.is_error);
    assert_eq!(result(&output)["exitCode"], 7);
    assert_eq!(result(&output)["status"], "failed");
}

#[test]
fn failed_first_chain_step_never_launches_the_next_assignment() {
    let (_dir, tool) = fixture("printf 'launched\\n' >> launches\nexit 0");
    let output = run(
        &tool,
        json!({"chain":[
            {"agent":"worker","task":"first"}, {"agent":"worker","task":"second"}
        ]}),
    );
    assert!(output.is_error);
    assert_eq!(
        output.details.as_ref().unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("launches")).unwrap(),
        "launched\n"
    );
}

#[test]
fn failed_corrective_retry_is_not_replaced_with_earlier_permissive_success() {
    let script = format!(
        "if [ -f phase ]; then\n exit 9\nelse\n : > phase\n{}fi\n",
        emit(&[ended("not JSON", "stop")])
    );
    let (_dir, tool) = fixture(&script);
    let mut input = request();
    input["outputSchema"] = json!({"type":"object"});
    input["schemaMode"] = json!("permissive");
    let output = run(&tool, input);
    assert!(output.is_error, "retry failure must win: {output:?}");
    assert_eq!(result(&output)["status"], "failed");
    assert_eq!(result(&output)["exitCode"], 9);
    assert_eq!(result(&output)["schemaRetries"], 1);
    assert_eq!(result(&output)["schemaValid"], false);
    assert!(result(&output).get("data").is_none());
}

#[test]
fn public_tool_rejects_a_truncated_answer_instead_of_schema_validating_its_prefix() {
    let (_dir, tool) = fixture(&emit(&[ended(
        &"x".repeat(MAX_CHILD_OUTPUT_BYTES + 1),
        "stop",
    )]));
    let output = run(&tool, request());
    assert!(output.is_error);
    assert!(
        result(&output)["error"]
            .as_str()
            .unwrap()
            .contains("PI_SUBAGENT_OUTPUT_LIMIT")
    );
    assert!(result(&output)["output"].as_str().unwrap().len() <= MAX_CHILD_OUTPUT_BYTES);
}

#[test]
fn dropping_a_running_delegation_settles_its_hub_entry_as_cancelled() {
    let (_dir, tool) = fixture("exec sleep 30");
    let (tx, rx) = futures::channel::oneshot::channel();
    let sender = Mutex::new(Some(tx));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let child_id = runtime.block_on(async {
        let future = Box::pin(tool.execute(
            "cancel",
            request(),
            Some(Box::new(move |update| {
                if update
                    .details
                    .as_ref()
                    .is_some_and(|value| value["result"]["status"] == "running")
                    && let Some(tx) = sender.lock().unwrap().take()
                {
                    let pid = update.details.as_ref().unwrap()["result"]["pid"]
                        .as_u64()
                        .unwrap();
                    let _ = tx.send(pid);
                }
            })),
        ));
        match futures::future::select(future, rx).await {
            futures::future::Either::Right((Ok(pid), pending)) => {
                drop(pending);
                pid
            }
            _ => panic!("child should reach running before completing"),
        }
    });
    let entry = crate::agent_hub::registry()
        .lock()
        .unwrap()
        .roster()
        .into_iter()
        .find(|entry| entry.pid.map(u64::from) == Some(child_id))
        .unwrap();
    assert_eq!(entry.status, crate::agent_hub::ChildStatus::Cancelled);
}

fn initialize_git(root: &Path) {
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.name", "Pi Test"],
        vec!["config", "user.email", "pi-test@example.invalid"],
        vec!["config", "commit.gpgSign", "false"],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(root.join("tracked.txt"), "original\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .args(["commit", "--quiet", "-m", "fixture"])
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn unsuccessful_child_protocol_never_applies_worktree_edits() {
    let (_dir, tool) = fixture(&format!(
        "printf 'unsafe edit\\n' > tracked.txt\n{}",
        emit(&[ended("truncated", "length")])
    ));
    initialize_git(&tool.cwd);
    let output = run(
        &tool,
        json!({"tasks":[{"agent":"worker","task":"change file","isolation":"worktree","isoApply":"apply"}]}),
    );
    assert!(output.is_error);
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "original\n"
    );
    assert_eq!(result(&output)["iso"]["applyMode"], "keep");
    assert_eq!(result(&output)["iso"]["applied"], false);
}

#[test]
fn invalid_typed_output_keeps_worktree_edits_even_in_permissive_mode() {
    let (_dir, tool) = fixture(&format!(
        "printf 'unsafe edit\\n' > tracked.txt\n{}",
        emit(&[ended("not JSON", "stop")])
    ));
    initialize_git(&tool.cwd);
    let output = run(
        &tool,
        json!({"tasks":[{
            "agent":"worker","task":"change file","isolation":"worktree","isoApply":"apply",
            "outputSchema":{"type":"object"},"schemaMode":"permissive"
        }]}),
    );
    assert!(
        !output.is_error,
        "permissive schema exhaustion remains an explicit warning: {output:?}"
    );
    assert_eq!(result(&output)["schemaValid"], false);
    assert_eq!(result(&output)["iso"]["applyMode"], "keep");
    assert_eq!(result(&output)["iso"]["applied"], false);
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "original\n"
    );
}

#[test]
fn valid_corrective_retry_applies_only_accepted_edits() {
    // Retry state must not become part of either workspace snapshot.
    let retry_state = tempfile::tempdir().unwrap();
    let marker = quote(retry_state.path().join("attempted").to_str().unwrap());
    let script = format!(
        "if [ -f {marker} ]; then\n\
         test \"$(cat tracked.txt)\" = original || exit 8\n\
         test ! -e rejected.txt || exit 8\n\
         printf 'accepted\\n' > tracked.txt\n\
         {}\
         else\n\
         : > {marker}\n\
         printf 'rejected\\n' > tracked.txt\n\
         printf 'first-only\\n' > rejected.txt\n\
         {}\
         fi\n",
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (_dir, tool) = fixture(&script);
    initialize_git(&tool.cwd);
    let output = run(
        &tool,
        json!({"tasks":[{
            "agent":"worker","task":"produce an accepted change",
            "isolation":"worktree","isoApply":"apply","schemaMode":"strict",
            "outputSchema":{
                "type":"object","required":["accepted"],
                "properties":{"accepted":{"type":"boolean"}}
            }
        }]}),
    );
    assert!(!output.is_error, "{output:?}");
    let result = result(&output);
    assert_eq!(result["task"], "produce an accepted change");
    assert_eq!(result["schemaValid"], true);
    assert_eq!(result["schemaRetries"], 1);
    assert_eq!(result["data"]["accepted"], true);
    assert_eq!(result["iso"]["applied"], true);
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "accepted\n"
    );
    assert!(!tool.cwd.join("rejected.txt").exists());
    let preserved = result["preservedWorktrees"].as_array().unwrap();
    assert_eq!(preserved.len(), 1);
    assert_eq!(preserved[0]["applyMode"], "keep");
    assert_eq!(preserved[0]["applied"], false);
    let path = Path::new(preserved[0]["worktreePath"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(path.join("tracked.txt")).unwrap(),
        "rejected\n"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("rejected.txt")).unwrap(),
        "first-only\n"
    );
}

#[test]
fn apply_conflict_marks_result_and_hub_failed() {
    let (_dir, tool) = fixture("");
    initialize_git(&tool.cwd);
    let parent_file = quote(tool.cwd.join("tracked.txt").to_str().unwrap());
    // The absolute parent write represents an independent editor changing the
    // original checkout while the child changes its isolated copy.
    let script = format!(
        "#!/bin/sh\nprintf 'child change\\n' > tracked.txt\n\
         printf 'concurrent parent change\\n' > {parent_file}\n{}",
        emit(&[ended("completed child edit", "stop")]),
    );
    std::fs::write(&tool.child_binary, script).unwrap();
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&statuses);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let output = runtime
        .block_on(tool.execute(
            "conflict",
            json!({"tasks":[{
                "agent":"worker","task":"change file","isolation":"worktree","isoApply":"apply"
            }]}),
            Some(Box::new(move |update| {
                if let Some(status) = update
                    .details
                    .as_ref()
                    .and_then(|value| value["result"]["status"].as_str())
                {
                    captured.lock().unwrap().push(status.to_string());
                }
            })),
        ))
        .unwrap();
    assert!(output.is_error, "{output:?}");
    assert_eq!(result(&output)["status"], "failed");
    assert_eq!(result(&output)["iso"]["applied"], false);
    assert!(
        result(&output)["error"]
            .as_str()
            .unwrap()
            .contains("PI_ISO_CONFLICT")
    );
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "concurrent parent change\n"
    );
    let pid = result(&output)["pid"].as_u64().unwrap();
    let entry = crate::agent_hub::registry()
        .lock()
        .unwrap()
        .roster()
        .into_iter()
        .find(|entry| entry.pid.map(u64::from) == Some(pid))
        .unwrap();
    assert_eq!(entry.status, crate::agent_hub::ChildStatus::Failed);
    let statuses = statuses
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(statuses.last().map(String::as_str), Some("failed"));
    assert!(!statuses.iter().any(|status| status == "completed"));
}

#[test]
fn invalid_isolation_settings_do_not_launch() {
    let (_dir, tool) = fixture(&format!(
        "printf 'launched\\n' > launched\n{}",
        emit(&[ended("answer", "stop")]),
    ));
    for (isolation, apply) in [("worktre", "apply"), ("worktree", "aply")] {
        let output = run(
            &tool,
            json!({"tasks":[{
                "agent":"worker","task":"must not launch","isolation":isolation,"isoApply":apply
            }]}),
        );
        assert!(output.is_error, "{output:?}");
        assert_eq!(result(&output)["status"], "failed");
        assert!(result(&output)["pid"].is_null());
        assert!(!tool.cwd.join("launched").exists());
    }
}

#[test]
fn descendants_holding_pipes_are_stopped_after_root_exit() {
    let (_dir, tool) = fixture(&format!(
        "sleep 30 &\n{}exit 0\n",
        emit(&[ended("answer", "stop")]),
    ));
    // Without process-group cleanup the background sleep retains the pipes,
    // and the explicit pipe-drain deadline makes this a failed delegation.
    let output = run(&tool, request());
    assert!(!output.is_error, "{output:?}");
    assert_eq!(result(&output)["status"], "completed");
    assert_eq!(result(&output)["output"], "answer");
}

#[test]
fn oversized_frame_fails_without_waiting_for_newline() {
    let (_dir, tool) = fixture("cat oversized-frame\nsleep 30");
    std::fs::write(
        tool.cwd.join("oversized-frame"),
        vec![b'x'; protocol::MAX_FRAME_BYTES + 1],
    )
    .unwrap();
    let output = run(&tool, request());
    assert!(output.is_error, "{output:?}");
    assert!(
        result(&output)["error"]
            .as_str()
            .unwrap()
            .contains("PI_SUBAGENT_FRAME_LIMIT")
    );
    assert!(result(&output)["output"].as_str().unwrap().is_empty());
}

#[test]
fn starting_callback_cancellation_prevents_spawn() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let (_dir, tool) = fixture(&format!(
        "printf 'launched\\n' > launched\n{}",
        emit(&[ended("answer", "stop")]),
    ));
    let owner = crate::agent_cx::AgentCx::for_request();
    let cancel_owner = owner.clone();
    let started = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&started);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let output = runtime
        .block_on(async {
            let future = tool.execute(
                "pre-spawn-cancel",
                request(),
                Some(Box::new(move |update| {
                    if update
                        .details
                        .as_ref()
                        .is_some_and(|value| value["result"]["status"] == "starting")
                    {
                        observed.store(true, Ordering::SeqCst);
                        cancel_owner.cancel_with(
                            asupersync::types::CancelKind::User,
                            Some("fixture cancellation"),
                        );
                    }
                })),
            );
            let mut future = std::pin::pin!(future);
            std::future::poll_fn(|task_cx| {
                let _guard = owner.cx().clone().set_current_restricted();
                std::future::Future::poll(future.as_mut(), task_cx)
            })
            .await
        })
        .unwrap();
    assert!(
        started.load(Ordering::SeqCst),
        "the Starting callback must actually run"
    );
    assert!(output.is_error);
    assert_eq!(result(&output)["status"], "cancelled");
    assert!(result(&output)["pid"].is_null());
    assert!(!tool.cwd.join("launched").exists());
}

fn source_entry(output: &ToolOutput) -> crate::agent_hub::ChildEntry {
    let value = result(output);
    let pid = value["pid"].as_u64().expect("fixture child must have launched"); // ubs:ignore: required real-process fixture observation
    let task = value["task"].as_str().expect("fixture assignment"); // ubs:ignore: required tool-result shape
    let preview = task.chars().take(500).collect::<String>();
    crate::agent_hub::registry()
        .lock()
        .expect("hub registry") // ubs:ignore: test requires an intact registry
        .roster()
        .into_iter()
        .find(|entry| entry.pid.map(u64::from) == Some(pid) && entry.task == preview)
        .expect("the actual process must have a registered owner") // ubs:ignore: ownership regression oracle
}

fn revive_through_hub(cwd: &Path, id: &str) -> ToolOutput {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("test runtime"); // ubs:ignore: required runtime fixture
    runtime
        .block_on(crate::tools::HubTool::new(cwd).execute(
            "revive-test",
            json!({"op":"agent","action":"revive","name":id}),
            None,
        ))
        .expect("hub domain results use ToolOutput") // ubs:ignore: public tool-result contract
}

fn revived_id(output: &ToolOutput) -> &str {
    output
        .details
        .as_ref()
        .and_then(|details| details["id"].as_str())
        .expect("a dispatched revival must identify its actual child") // ubs:ignore: revival execution oracle
}

#[test]
fn hub_revival_executes_retained_policy_and_full_assignment_without_phantom_children()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let script = format!(
        "printf '%s\\n' \"$PI_SUBAGENT_RUN_ID\" >> \"$PI_CODING_AGENT_DIR/launches\"\n\
         printf '%s\\n' \"$@\" > \"$PI_CODING_AGENT_DIR/$PI_SUBAGENT_RUN_ID.args\"\n\
         printf '%s\\n' \"$PI_SUBAGENT_DEPTH\" > \"$PI_CODING_AGENT_DIR/$PI_SUBAGENT_RUN_ID.depth\"\n\
         pwd > \"$PI_CODING_AGENT_DIR/$PI_SUBAGENT_RUN_ID.cwd\"\n{}",
        emit(&[ended("complete answer", "stop")]),
    );
    let (_directory, tool) = fixture(&script);
    let tool = tool.with_timeout(Duration::from_secs(10));
    std::fs::write(
        tool.global_dir.join("agents/worker.md"),
        "---\nname: worker\ndescription: retained policy\nmodel: fixture/original\nreasoning: low\ntools: read\n---\nOriginal system policy.",
    )?;
    let assignment = format!(
        "revival-{} {}FINAL REQUIREMENT: preserve all data",
        uuid::Uuid::new_v4(),
        "perform required work; ".repeat(60)
    );
    let initial = run(&tool, json!({"agent":"worker","task":assignment}));
    assert!(!initial.is_error, "{initial:?}");
    let source = source_entry(&initial);
    assert_eq!(source.task.chars().count(), 500);
    std::fs::write(
        &source.transcript_path,
        format!(
            "{}\nfirst-history-marker\nkey is sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
            "old unbounded history\n".repeat(2000)
        ),
    )?;
    // Definitions and the hub caller's cwd can change after the first run.
    // Neither is authority to replace the source's tools/model/workspace.
    std::fs::write(
        tool.global_dir.join("agents/worker.md"),
        "---\nname: worker\ndescription: changed policy\nmodel: fixture/escalated\ntools: bash,subagent\n---\nChanged system policy.",
    )?;
    let unrelated = tempfile::tempdir()?;
    let revived = revive_through_hub(unrelated.path(), &source.id);
    assert!(!revived.is_error, "{revived:?}");
    let replacement_id = revived_id(&revived);
    assert_ne!(replacement_id, source.id);
    let arguments = std::fs::read_to_string(
        tool.global_dir.join(format!("{replacement_id}.args")),
    )?;
    assert!(arguments.contains("--model\nfixture/original\n"));
    assert!(arguments.contains("--tools\nread\n"));
    assert!(arguments.contains("--thinking\nlow\n"));
    assert!(arguments.contains("Original system policy."));
    assert!(!arguments.contains("fixture/escalated"));
    assert!(!arguments.contains("Changed system policy."));
    assert!(arguments.contains(&assignment));
    assert!(arguments.contains("first-history-marker"));
    assert!(arguments.contains("<pi-secret:"));
    assert!(!arguments.contains("sk-ant-api03-AAAA"));
    assert!(arguments.len() < assignment.len() + 20 * 1024);
    assert_eq!(arguments.matches("Continuation of a prior run").count(), 1);
    let original_depth = std::fs::read_to_string(
        tool.global_dir.join(format!("{}.depth", source.id)),
    )?;
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join(format!("{replacement_id}.depth")))?,
        original_depth
    );
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join(format!("{replacement_id}.cwd")))?
            .trim(),
        tool.cwd.to_string_lossy()
    );
    let replacement = {
        let registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: ownership regression oracle
        let children = registry
            .roster()
            .into_iter()
            .filter(|entry| entry.revived_from.as_deref() == Some(source.id.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(children.len(), 1, "one revival must create one real child");
        assert_eq!(children[0].id, replacement_id);
        assert_eq!(children[0].status, crate::agent_hub::ChildStatus::Done);
        assert!(registry.native_revival_source(replacement_id).is_ok());
        children[0].clone()
    };
    std::fs::write(&replacement.transcript_path, "second-history-marker\n")?;
    let repeated = revive_through_hub(unrelated.path(), replacement_id);
    assert!(!repeated.is_error, "{repeated:?}");
    let repeated_id = revived_id(&repeated);
    let arguments =
        std::fs::read_to_string(tool.global_dir.join(format!("{repeated_id}.args")))?;
    assert!(arguments.contains(&assignment));
    assert!(arguments.contains("second-history-marker"));
    assert!(!arguments.contains("first-history-marker"));
    assert_eq!(arguments.matches("Continuation of a prior run").count(), 1);
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join("launches"))?,
        format!("{}\n{replacement_id}\n{repeated_id}\n", source.id)
    );
    Ok(())
}

#[test]
fn hub_revival_refuses_synthetic_registration_without_spending_a_child_entry() {
    let name = format!("synthetic-{}", uuid::Uuid::new_v4());
    let source = {
        let mut registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: required registry fixture
        let source = registry.register(&name, "not a native launch").expect("synthetic entry"); // ubs:ignore: synthetic-registration regression fixture
        registry.settle(&source.id, crate::agent_hub::ChildStatus::Done);
        source
    };
    let output = revive_through_hub(Path::new("."), &source.id);
    assert!(output.is_error);
    assert!(serde_json::to_string(&output).expect("tool JSON").contains("PI_HUB_REVIVAL_UNAVAILABLE")); // ubs:ignore: named refusal oracle
    let registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: no-phantom-child regression oracle
    assert_eq!(registry.roster().iter().filter(|entry| entry.name == name).count(), 1);
}

#[test]
fn hub_revival_checks_current_authority_before_starting_a_replacement()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (_directory, tool) = fixture(&emit(&[ended("complete", "stop")]));
    let initial = run(&tool, request());
    assert!(!initial.is_error);
    let source = source_entry(&initial);
    let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build()?;
    let output = runtime.block_on(async {
        let tool = crate::tools::HubTool::new(&tool.cwd);
        let mut future = std::pin::pin!(tool.execute(
            "restricted-revival",
            json!({"op":"agent","action":"revive","name":source.id}),
            None,
        ));
        std::future::poll_fn(|cx| {
            let _guard = restricted.clone().set_current_restricted();
            std::future::Future::poll(future.as_mut(), cx)
        })
        .await
    })?;
    assert!(output.is_error);
    assert!(serde_json::to_string(&output)?.contains("PI_SUBAGENT_PERMISSION"));
    let registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: no-phantom-child regression oracle
    assert!(!registry.roster().iter().any(|entry| {
        entry.revived_from.as_deref() == Some(source.id.as_str())
    }));
    assert!(registry.native_revival_source(&source.id).is_ok());
    Ok(())
}

#[test]
fn failed_hub_revival_spawn_settles_its_single_registered_replacement()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let (_directory, tool) = fixture(&emit(&[ended("complete", "stop")]));
    let initial = run(&tool, request());
    assert!(!initial.is_error);
    let source = source_entry(&initial);
    std::fs::set_permissions(&tool.child_binary, std::fs::Permissions::from_mode(0o600))?;
    let output = revive_through_hub(&tool.cwd, &source.id);
    assert!(output.is_error);
    let replacement_id = revived_id(&output);
    let registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: failed-spawn ownership oracle
    let replacements = registry
        .roster()
        .into_iter()
        .filter(|entry| entry.revived_from.as_deref() == Some(source.id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(replacements.len(), 1);
    assert_eq!(replacements[0].id, replacement_id);
    assert_eq!(replacements[0].status, crate::agent_hub::ChildStatus::Failed);
    assert!(replacements[0].pid.is_none());
    assert!(registry.native_revival_source(replacement_id).is_ok());
    Ok(())
}

#[test]
fn active_and_killed_revival_owners_block_replacement_until_drop_reaps_the_child()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let script = format!(
        "if [ -f \"$PI_CODING_AGENT_DIR/block\" ]; then exec sleep 30; fi\n{}",
        emit(&[ended("source complete", "stop")]),
    );
    let (_directory, tool) = fixture(&script);
    let tool = tool.with_timeout(Duration::from_secs(10));
    let initial = run(&tool, request());
    assert!(!initial.is_error);
    let source = source_entry(&initial);
    let middle = revive_through_hub(&tool.cwd, &source.id);
    assert!(!middle.is_error, "{middle:?}");
    let middle_id = revived_id(&middle).to_string();
    std::fs::write(tool.global_dir.join("block"), "block")?;
    let (sender, receiver) = futures::channel::oneshot::channel();
    let sender = Mutex::new(Some(sender));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build()?;
    let (replacement_id, pid) = runtime.block_on(async {
        let hub = crate::tools::HubTool::new(&tool.cwd);
        let future = Box::pin(hub.execute(
            "running-revival",
            json!({"op":"agent","action":"revive","name":middle_id}),
            Some(Box::new(move |update| {
                if let Some(value) = update.details.as_ref()
                    && value["result"]["status"] == "running"
                    && let Some(pid) = value["result"]["pid"].as_u64()
                    && let Some(sender) = sender.lock().expect("progress sender").take() // ubs:ignore: required running-child observation
                {
                    let _ = sender.send(pid);
                }
            })),
        ));
        let (pid, pending) = match futures::future::select(future, receiver).await {
            futures::future::Either::Right((Ok(pid), pending)) => (pid, pending),
            _ => panic!("revival must launch before it can be cancelled"), // ubs:ignore: real-process progress oracle
        };
        let replacement = crate::agent_hub::registry()
            .lock()
            .expect("hub registry") // ubs:ignore: required running-child ownership
            .roster()
            .into_iter()
            .find(|entry| {
                entry.pid.map(u64::from) == Some(pid)
                    && entry.revived_from.as_deref() == Some(middle_id.as_str())
            })
            .expect("revival must own its single registered process"); // ubs:ignore: no-phantom-child regression oracle
        // Both the direct parent and its ancestor share this live lineage.
        // Checking only immediate revived_from edges would admit the latter.
        for id in [&source.id, &middle_id] {
            let duplicate = hub
                .execute(
                    "duplicate-revival",
                    json!({"op":"agent","action":"revive","name":id}),
                    None,
                )
                .await
                .expect("hub refusal result"); // ubs:ignore: public domain-refusal contract
            assert!(duplicate.is_error);
        }
        crate::agent_hub::registry()
            .lock()
            .expect("hub registry") // ubs:ignore: required operator-kill fixture
            .mark_killed(&replacement.id);
        let premature = hub
            .execute(
                "premature-revival",
                json!({"op":"agent","action":"revive","name":replacement.id}),
                None,
            )
            .await
            .expect("hub refusal result"); // ubs:ignore: public domain-refusal contract
        assert!(premature.is_error);
        assert!(
            serde_json::to_string(&premature)
                .expect("tool JSON") // ubs:ignore: named cleanup-refusal oracle
                .contains("cleanup is still active")
        );
        // The real pending driver owns the process, pipes, and hub lease.
        // Dropping it must reap before the terminal run becomes revivable.
        drop(pending);
        (replacement.id, pid)
    });
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    assert!(!alive.success(), "dropped revival root was not reaped");
    let registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: cleanup ownership oracle
    let recovered = registry.native_revival_source(&replacement_id)?;
    assert_eq!(recovered.entry.status, crate::agent_hub::ChildStatus::Killed);
    assert_eq!(
        registry
            .roster()
            .iter()
            .filter(|entry| entry.revived_from.as_deref() == Some(source.id.as_str()))
            .count(),
        1
    );
    assert_eq!(
        registry
            .roster()
            .iter()
            .filter(|entry| entry.revived_from.as_deref() == Some(middle_id.as_str()))
            .count(),
        1
    );
    assert!(!registry.roster().iter().any(|entry| {
        entry.revived_from.as_deref() == Some(replacement_id.as_str())
    }));
    Ok(())
}

#[test]
fn hub_revival_keeps_schema_isolation_and_authored_task_across_corrective_retry()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let script = format!(
        "if [ ! -f \"$PI_CODING_AGENT_DIR/source-finished\" ]; then\n\
           : > \"$PI_CODING_AGENT_DIR/source-finished\"\n{}\
         elif [ ! -f \"$PI_CODING_AGENT_DIR/correction\" ]; then\n\
           : > \"$PI_CODING_AGENT_DIR/correction\"\n\
           printf 'rejected\\n' > tracked.txt\n{}\
         else\n\
           printf 'accepted\\n' > tracked.txt\n{}fi\n",
        emit(&[ended("{\"accepted\":true}", "stop")]),
        emit(&[ended("not JSON", "stop")]),
        emit(&[ended("{\"accepted\":true}", "stop")]),
    );
    let (_directory, tool) = fixture(&script);
    initialize_git(&tool.cwd);
    let assignment = format!("retained-schema-{}: complete the authored change", uuid::Uuid::new_v4());
    let initial = run(
        &tool,
        json!({"tasks":[{
            "agent":"worker","task":assignment,"isolation":"worktree","isoApply":"apply",
            "schemaMode":"strict","outputSchema":{
                "type":"object","required":["accepted"],
                "properties":{"accepted":{"type":"boolean"}}
            }
        }]}),
    );
    assert!(!initial.is_error, "{initial:?}");
    let source = source_entry(&initial);
    let output = revive_through_hub(&tool.cwd, &source.id);
    assert!(!output.is_error, "{output:?}");
    let details = output.details.as_ref().expect("revival details"); // ubs:ignore: required actual execution result
    let result = &details["result"];
    assert_eq!(result["schemaValid"], true);
    assert_eq!(result["schemaRetries"], 1);
    assert_eq!(result["data"]["accepted"], true);
    assert_eq!(result["iso"]["applied"], true);
    assert_eq!(result["task"], assignment);
    assert_eq!(std::fs::read_to_string(tool.cwd.join("tracked.txt"))?, "accepted\n");
    let preserved = result["preservedWorktrees"].as_array().expect("rejected attempt preserved"); // ubs:ignore: schema-rejection disposition oracle
    assert_eq!(preserved.len(), 1);
    assert_eq!(preserved[0]["applied"], false);
    let rejected = preserved[0]["worktreePath"].as_str().expect("preserved worktree path"); // ubs:ignore: real isolation evidence
    assert_eq!(std::fs::read_to_string(Path::new(rejected).join("tracked.txt"))?, "rejected\n");
    let retry_id = revived_id(&output);
    let retry_source = {
        let registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: native replay-policy oracle
        let retry = registry.native_revival_source(retry_id)?;
        let first_id = retry.entry.revived_from.as_deref().expect("corrective lineage"); // ubs:ignore: one-attempt lineage oracle
        let first = registry.get(first_id).expect("first revival attempt"); // ubs:ignore: actual attempt registry evidence
        assert_eq!(first.revived_from.as_deref(), Some(source.id.as_str()));
        assert_eq!(first.status, crate::agent_hub::ChildStatus::Failed);
        assert_eq!(retry.entry.status, crate::agent_hub::ChildStatus::Done);
        assert_eq!(retry.launch.task.task, assignment);
        assert_eq!(retry.launch.task.isolation.as_deref(), Some("worktree"));
        assert_eq!(retry.launch.task.iso_apply.as_deref(), Some("apply"));
        retry
    };
    let prompt = retry_source.continuation_prompt()?;
    assert!(prompt.starts_with(&assignment));
    assert!(!prompt.contains("Your previous output failed schema validation"));
    assert_eq!(prompt.matches("Continuation of a prior run").count(), 1);
    Ok(())
}
