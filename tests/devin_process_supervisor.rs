//! Behavioral contract for the session-owned Devin process supervisor.
//!
//! Every test drives the supervisor through the registered tools so the policy
//! gate, the audit lifecycle, and the process registry are exercised together
//! rather than in isolation.

#![allow(clippy::items_after_statements)]

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use pi::devin::{
    AgentMode, AuditLog, AuditStatus, DevinSessionState, PermissionMode, PolicyAction,
    ProcessStatus, ProcessSupervisor, ScopeAccess, SharedProcessSupervisor, SpawnRequest,
    ToolPolicyEngine, ToolRequest, ToolRequestOrigin,
};
use pi::model::ContentBlock;
use pi::tools::{Tool, ToolOutput, ToolRegistry};
use serde_json::{Value, json};

struct Harness {
    supervisor: SharedProcessSupervisor,
    audit: Arc<AuditLog>,
    policy: Arc<ToolPolicyEngine>,
    registry: ToolRegistry,
}

impl Harness {
    fn new(workspace: &Path, agent_mode: AgentMode, permission_mode: PermissionMode) -> Self {
        Self::with_state(
            |state| {
                state.agent_mode = agent_mode;
                state.permission_mode = permission_mode;
            },
            workspace,
        )
    }

    fn with_state(configure: impl FnOnce(&mut DevinSessionState), workspace: &Path) -> Self {
        let mut state = DevinSessionState::new("session-under-test", workspace);
        configure(&mut state);
        let audit = Arc::new(AuditLog::new(64));
        let policy = Arc::new(
            ToolPolicyEngine::new(Arc::new(RwLock::new(state))).with_audit(Arc::clone(&audit)),
        );
        let supervisor = ProcessSupervisor::shared("session-under-test");
        let mut registry = ToolRegistry::from_tools(Vec::new());
        pi::devin::register_process_tools(
            &mut registry,
            Arc::clone(&supervisor),
            Arc::clone(&policy),
            Some(Arc::clone(&audit)),
        )
        .expect("process tools register on an empty registry");
        Self {
            supervisor,
            audit,
            policy,
            registry,
        }
    }

    async fn call(&self, tool: &str, call_id: &str, arguments: Value) -> ToolOutput {
        self.registry
            .get(tool)
            .unwrap_or_else(|| panic!("`{tool}` must be registered"))
            .execute(call_id, arguments, None)
            .await
            .unwrap_or_else(|err| panic!("`{tool}` returned a hard error: {err}"))
    }

    fn status(&self, call_id: &str) -> AuditStatus {
        self.audit
            .record_for(call_id)
            .unwrap_or_else(|| panic!("no audit record for `{call_id}`"))
            .status
    }

    fn records_for(&self, call_id: &str) -> usize {
        self.audit
            .snapshot()
            .iter()
            .filter(|record| record.call_id == call_id)
            .count()
    }
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(value) => Some(value.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn process_id(output: &ToolOutput) -> String {
    output
        .details
        .as_ref()
        .and_then(|details| details.get("process").cloned())
        .and_then(|process| {
            process
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .expect("details must carry the process record")
}

fn process_field(output: &ToolOutput, key: &str) -> Value {
    output
        .details
        .as_ref()
        .and_then(|details| details.get("process"))
        .and_then(|process| process.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[test]
fn all_five_pinned_process_tools_register_on_the_shared_registry() {
    let workspace = tempfile::tempdir().unwrap();
    let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);
    for name in pi::devin::DEVIN_PROCESS_TOOL_NAMES {
        assert!(
            harness.registry.get(name).is_some(),
            "`{name}` must be registered on the shared tool registry"
        );
    }
}

#[test]
fn registering_the_process_tools_twice_is_rejected_instead_of_shadowing() {
    let workspace = tempfile::tempdir().unwrap();
    let mut harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

    // `ToolRegistry::get` returns the first match, so a second registration
    // would leave shadowed duplicates whose pinned schema is never executed.
    let error = pi::devin::register_process_tools(
        &mut harness.registry,
        Arc::clone(&harness.supervisor),
        Arc::clone(&harness.policy),
        Some(Arc::clone(&harness.audit)),
    )
    .expect_err("a duplicate registration must be rejected");
    assert!(
        error.to_string().contains("already registered"),
        "message was: {error}"
    );
    assert_eq!(
        harness
            .registry
            .tools()
            .iter()
            .filter(|tool| tool.name() == "exec")
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// Foreground, background, stdin
// ---------------------------------------------------------------------------

#[test]
fn short_foreground_process_streams_and_reports_its_exit_code() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        let output = harness
            .call(
                "exec",
                "call-fg",
                json!({"command": "echo devin-foreground"}),
            )
            .await;

        assert!(!output.is_error, "unexpected error: {}", text(&output));
        assert!(text(&output).contains("devin-foreground"));
        assert_eq!(process_field(&output, "status"), json!("exited"));
        assert_eq!(process_field(&output, "exitCode"), json!(0));
        assert_eq!(harness.status("call-fg"), AuditStatus::Succeeded);
        assert_eq!(harness.records_for("call-fg"), 1);
    });
}

#[test]
fn nonzero_exit_is_reported_as_a_failed_call_not_a_hard_error() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        let output = harness
            .call("shell_command", "call-exit", json!({"command": "exit 42"}))
            .await;

        assert!(output.is_error);
        assert_eq!(process_field(&output, "exitCode"), json!(42));
        assert_eq!(harness.status("call-exit"), AuditStatus::Failed);
    });
}

#[test]
fn background_process_returns_immediately_and_streams_incremental_output() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        let started = harness
            .call(
                "exec",
                "call-bg",
                json!({
                    "command": "for i in 1 2 3; do echo chunk-$i; sleep 0.2; done",
                    "background": true
                }),
            )
            .await;
        assert!(!started.is_error, "{}", text(&started));
        let id = process_id(&started);
        assert_eq!(process_field(&started, "status"), json!("running"));

        // First read arrives while the process is still producing output.
        let mut collected = String::new();
        for attempt in 0..60 {
            let chunk = harness
                .call(
                    "get_output",
                    &format!("call-out-{attempt}"),
                    json!({"process_id": id}),
                )
                .await;
            let body = text(&chunk);
            if body != "(no new output)" {
                collected.push_str(&body);
            }
            if collected.contains("chunk-3") {
                break;
            }
            // Ambient timer, not `std::thread::sleep`: this loop runs inside a
            // `run_test` future and must not block the executor thread the
            // supervisor's own `cx.time().sleep` depends on.
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(100)).await;
        }

        assert!(
            collected.contains("chunk-1") && collected.contains("chunk-3"),
            "expected incremental output, collected: {collected}"
        );

        // Reads are incremental: nothing is replayed once consumed.
        let drained = harness
            .call("get_output", "call-drain", json!({"process_id": id}))
            .await;
        assert!(!text(&drained).contains("chunk-1"));
    });
}

#[test]
fn write_to_process_feeds_stdin_of_an_interactive_process() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        let started = harness
            .call(
                "exec",
                "call-interactive",
                json!({
                    "command": "while IFS= read -r line; do echo \"echoed:$line\"; done",
                    "background": true,
                    "interactive": true
                }),
            )
            .await;
        let id = process_id(&started);

        let wrote = harness
            .call(
                "write_to_process",
                "call-write",
                json!({"process_id": id, "data": "ping", "append_newline": true}),
            )
            .await;
        assert!(!wrote.is_error, "{}", text(&wrote));
        assert_eq!(harness.status("call-write"), AuditStatus::Succeeded);

        let mut seen = String::new();
        for attempt in 0..60 {
            let chunk = harness
                .call(
                    "get_output",
                    &format!("call-read-{attempt}"),
                    json!({"process_id": id}),
                )
                .await;
            seen.push_str(&text(&chunk));
            if seen.contains("echoed:ping") {
                break;
            }
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(100)).await;
        }
        assert!(seen.contains("echoed:ping"), "collected: {seen}");
    });
}

// ---------------------------------------------------------------------------
// Failure modes
// ---------------------------------------------------------------------------

#[test]
fn unknown_process_id_is_a_clear_error_on_every_process_tool() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        for (index, (tool, arguments)) in [
            ("get_output", json!({"process_id": "proc-404"})),
            (
                "write_to_process",
                json!({"process_id": "proc-404", "data": "x"}),
            ),
            ("kill_shell", json!({"process_id": "proc-404"})),
        ]
        .into_iter()
        .enumerate()
        {
            let call_id = format!("call-unknown-{index}");
            let output = harness.call(tool, &call_id, arguments).await;
            assert!(output.is_error, "`{tool}` must reject an unknown id");
            assert!(
                text(&output).contains("unknown process id `proc-404`"),
                "`{tool}` message was: {}",
                text(&output)
            );
            assert_eq!(harness.status(&call_id), AuditStatus::Failed);
        }
    });
}

#[test]
fn writing_to_a_closed_stdin_reports_why_instead_of_silently_succeeding() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        // Started without `interactive`, so stdin was closed right after spawn.
        let started = harness
            .call(
                "exec",
                "call-noninteractive",
                json!({"command": "sleep 5", "background": true, "interactive": false}),
            )
            .await;
        let id = process_id(&started);

        let refused = harness
            .call(
                "write_to_process",
                "call-closed",
                json!({"process_id": id, "data": "x"}),
            )
            .await;
        assert!(refused.is_error);
        assert!(
            text(&refused).contains("stdin"),
            "message was: {}",
            text(&refused)
        );
        assert_eq!(harness.status("call-closed"), AuditStatus::Failed);

        // Writing after the process exits is also refused, with the status.
        let killed = harness
            .call(
                "kill_shell",
                "call-kill-noninteractive",
                json!({"process_id": id}),
            )
            .await;
        assert!(!killed.is_error, "{}", text(&killed));
        let after_exit = harness
            .call(
                "write_to_process",
                "call-after-exit",
                json!({"process_id": id, "data": "x"}),
            )
            .await;
        assert!(after_exit.is_error);
        assert!(text(&after_exit).contains("is not running"));
    });
}

#[test]
fn foreground_timeout_terminates_the_process_and_records_a_timed_out_call() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        let output = harness
            .call(
                "exec",
                "call-timeout",
                json!({"command": "sleep 30", "timeout_ms": 300}),
            )
            .await;

        assert!(output.is_error);
        assert_eq!(process_field(&output, "status"), json!("timed_out"));
        assert!(text(&output).contains("timed out"));
        assert_eq!(harness.status("call-timeout"), AuditStatus::TimedOut);
        assert_eq!(harness.records_for("call-timeout"), 1);
    });
}

#[cfg(target_os = "linux")]
#[test]
fn ambient_cancellation_stops_the_foreground_process_and_its_children() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);
        let marker = workspace.path().join("cancel-leak.txt");

        let ambient = asupersync::Cx::for_testing();
        let cancel = ambient.clone();
        let _current = asupersync::Cx::set_current(Some(ambient));
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            cancel.set_cancel_requested(true);
        });

        let output = harness
            .call(
                "exec",
                "call-cancel",
                json!({
                    "command": "(sleep 3; echo leaked > cancel-leak.txt) & sleep 10",
                    "timeout_ms": 30000
                }),
            )
            .await;
        canceller.join().expect("cancel thread");

        assert!(output.is_error);
        assert_eq!(process_field(&output, "status"), json!("cancelled"));
        assert_eq!(harness.status("call-cancel"), AuditStatus::Cancelled);

        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !marker.exists(),
            "cancellation must terminate the whole process group"
        );
    });
}

#[cfg(target_os = "linux")]
#[test]
fn kill_shell_terminates_the_entire_process_group() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);
        let marker = workspace.path().join("group-leak.txt");

        let started = harness
            .call(
                "exec",
                "call-group",
                json!({
                    "command": "(sleep 3; echo leaked > group-leak.txt) & sleep 30",
                    "background": true
                }),
            )
            .await;
        let id = process_id(&started);
        std::thread::sleep(Duration::from_millis(300));

        let killed = harness
            .call(
                "kill_shell",
                "call-group-kill",
                json!({"process_id": id, "grace_ms": 200}),
            )
            .await;
        assert!(!killed.is_error, "{}", text(&killed));
        assert!(
            matches!(
                process_field(&killed, "status").as_str(),
                Some("killed" | "exited")
            ),
            "unexpected status: {}",
            process_field(&killed, "status")
        );

        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !marker.exists(),
            "kill_shell must terminate the whole process group, not just the shell"
        );
    });
}

#[test]
fn output_beyond_the_in_memory_budget_is_truncated_and_reported() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        // ~1.2 MiB of output, well past both the ring buffer and the artifact
        // spill threshold.
        let output = harness
            .call(
                "exec",
                "call-flood",
                json!({
                    "command": "i=0; while [ $i -lt 12000 ]; do printf '%s\\n' 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'; i=$((i+1)); done",
                    "timeout_ms": 60000
                }),
            )
            .await;

        assert!(!output.is_error, "{}", text(&output));
        let body = text(&output);
        assert!(
            body.contains("[Output truncated:"),
            "expected a truncation notice, got tail: {}",
            &body[body.len().saturating_sub(400)..]
        );
        assert_eq!(
            output
                .details
                .as_ref()
                .and_then(|details| details.get("truncated"))
                .and_then(Value::as_bool),
            Some(true)
        );
        // Large output is referenced through an artifact, never inlined whole.
        let artifact = process_field(&output, "artifactPath");
        assert_ne!(artifact, Value::Null);

        // The artifact is the full record of what the process produced, so it
        // must hold exactly the produced byte count: seeding it from the
        // retained buffers and then also appending the triggering chunk would
        // duplicate a segment at the spill boundary.
        let artifact_path = artifact.as_str().expect("artifactPath must be a string");
        let written = std::fs::metadata(artifact_path)
            .expect("spill artifact must exist")
            .len();
        let produced = process_field(&output, "stdoutBytes")
            .as_u64()
            .expect("stdoutBytes must be reported")
            + process_field(&output, "stderrBytes")
                .as_u64()
                .expect("stderrBytes must be reported");
        assert_eq!(
            written, produced,
            "artifact must contain exactly the produced bytes, with nothing duplicated"
        );
    });
}

// ---------------------------------------------------------------------------
// Session ownership
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn session_stop_cleans_up_every_owned_process() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);
        let marker = workspace.path().join("session-leak.txt");

        for (index, command) in [
            "(sleep 3; echo leaked >> session-leak.txt) & sleep 30",
            "sleep 30",
        ]
        .into_iter()
        .enumerate()
        {
            harness
                .call(
                    "exec",
                    &format!("call-owned-{index}"),
                    json!({"command": command, "background": true}),
                )
                .await;
        }
        std::thread::sleep(Duration::from_millis(300));

        let terminated = harness.supervisor.shutdown().await;
        assert_eq!(terminated.len(), 2);
        assert!(
            terminated.iter().all(|record| record.status.is_terminal()),
            "session stop must leave no running owned process: {terminated:?}"
        );

        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !marker.exists(),
            "session stop must terminate descendants, not just direct children"
        );
    });
}

#[test]
fn detached_processes_are_explicit_and_recorded_on_the_registry_entry() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let supervisor = ProcessSupervisor::shared("session-detached");

        let mut request = SpawnRequest::new("sleep 30", workspace.path(), "exec");
        request.background = true;
        request.detached = true;
        let started = supervisor.start_background(request).unwrap();
        assert!(started.record.detached);

        // Session stop leaves an explicitly detached process alone.
        let terminated = supervisor.shutdown().await;
        assert!(terminated.is_empty());
        assert_eq!(
            supervisor.record(&started.record.id).unwrap().status,
            ProcessStatus::Running
        );

        // The detachment stays visible in the registry for auditing.
        let records = supervisor.records();
        assert_eq!(records.len(), 1);
        assert!(records[0].detached);

        supervisor
            .kill(&started.record.id, Some(Duration::from_millis(200)))
            .await
            .unwrap();
    });
}

// ---------------------------------------------------------------------------
// Policy integration
// ---------------------------------------------------------------------------

#[test]
fn plan_mode_denies_every_process_mutating_tool() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        // Bypass permissions on purpose: plan mode must still win.
        let harness = Harness::new(workspace.path(), AgentMode::Plan, PermissionMode::Bypass);

        for (index, (tool, arguments)) in [
            ("exec", json!({"command": "echo nope"})),
            ("shell_command", json!({"command": "echo nope"})),
            (
                "write_to_process",
                json!({"process_id": "proc-1", "data": "x"}),
            ),
            ("kill_shell", json!({"process_id": "proc-1"})),
        ]
        .into_iter()
        .enumerate()
        {
            let call_id = format!("call-plan-{index}");
            let output = harness.call(tool, &call_id, arguments).await;
            assert!(output.is_error, "plan mode must deny `{tool}`");
            assert!(
                text(&output).contains("plan mode does not permit"),
                "`{tool}` message was: {}",
                text(&output)
            );
            assert_eq!(harness.status(&call_id), AuditStatus::Denied);
            assert_eq!(harness.records_for(&call_id), 1);
        }

        assert!(
            harness.supervisor.records().is_empty(),
            "plan mode must not start any process"
        );
    });
}

#[test]
fn bypass_mode_cannot_run_a_process_outside_the_workspace_or_a_granted_scope() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let granted = tempfile::tempdir().unwrap();

        let granted_root = granted.path().to_path_buf();
        let harness = Harness::with_state(
            |state| {
                state.permission_mode = PermissionMode::Bypass;
                state.grant_scope(granted_root, ScopeAccess::Write);
            },
            workspace.path(),
        );

        let escaped = harness
            .call(
                "exec",
                "call-escape",
                json!({"command": "pwd", "cwd": outside.path().display().to_string()}),
            )
            .await;
        assert!(escaped.is_error, "bypass must not escape containment");
        assert!(
            text(&escaped).contains("outside the allowed workspace and scopes"),
            "message was: {}",
            text(&escaped)
        );
        assert_eq!(harness.status("call-escape"), AuditStatus::Denied);

        let traversal = harness
            .call(
                "exec",
                "call-traversal",
                json!({"command": "pwd", "cwd": "../.."}),
            )
            .await;
        assert!(traversal.is_error, "bypass must not allow path traversal");

        assert!(
            harness.supervisor.records().is_empty(),
            "no contained-violating process may be spawned"
        );

        // The explicitly granted scope still works, so containment is enforced
        // rather than simply blanket-denying non-workspace paths.
        let allowed = harness
            .call(
                "exec",
                "call-granted",
                json!({"command": "pwd", "cwd": granted.path().display().to_string()}),
            )
            .await;
        assert!(!allowed.is_error, "{}", text(&allowed));
        assert_eq!(harness.status("call-granted"), AuditStatus::Succeeded);
    });
}

#[test]
fn autonomous_mode_denies_process_execution_without_an_active_sandbox() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::with_state(
            |state| state.permission_mode = PermissionMode::Autonomous,
            workspace.path(),
        );

        let output = harness
            .call("exec", "call-autonomous", json!({"command": "echo nope"}))
            .await;

        assert!(output.is_error);
        assert!(
            text(&output).contains("requires an active OS sandbox"),
            "message was: {}",
            text(&output)
        );
        assert_eq!(harness.status("call-autonomous"), AuditStatus::Denied);
        assert!(harness.supervisor.records().is_empty());
    });
}

#[test]
fn a_sandbox_status_without_a_named_backend_is_not_treated_as_active() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::with_state(
            |state| {
                state.permission_mode = PermissionMode::Autonomous;
                // Status claimed, but no backend ever activated containment.
                state.sandbox_status = pi::devin::SandboxStatus::Active;
            },
            workspace.path(),
        );

        let output = harness
            .call("exec", "call-fake-sandbox", json!({"command": "echo nope"}))
            .await;
        assert!(output.is_error);
        assert_eq!(harness.status("call-fake-sandbox"), AuditStatus::Denied);
    });
}

#[test]
fn normal_mode_asks_before_running_a_process_and_fails_closed_without_an_approver() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Normal);

        let output = harness
            .call("exec", "call-ask", json!({"command": "echo nope"}))
            .await;
        assert!(output.is_error);
        assert!(
            text(&output).contains("requires approval"),
            "message was: {}",
            text(&output)
        );
        assert_eq!(harness.status("call-ask"), AuditStatus::Denied);
        assert!(harness.supervisor.records().is_empty());
    });
}

#[test]
fn an_approval_recorded_before_dispatch_survives_the_adapter_policy_recheck() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Normal);

        // Exactly what `Agent::execute_tool` does before it dispatches: policy
        // returns `Ask`, an approval surface answers, and the approval is
        // recorded on the call's audit record.
        let decision = harness.policy.evaluate(&ToolRequest {
            call_id: "call-approved".to_string(),
            tool_name: "exec".to_string(),
            arguments: json!({"command": "echo approved"}),
            origin: ToolRequestOrigin::Native,
        });
        assert_eq!(decision.action, PolicyAction::Ask);
        assert!(
            harness
                .audit
                .mark_allowed("call-approved", Some("approval"))
        );

        // The adapter re-evaluates the same call. Re-asking here would convert
        // an approved call into a denial, so the recorded approval must hold.
        let output = harness
            .call("exec", "call-approved", json!({"command": "echo approved"}))
            .await;

        assert!(!output.is_error, "{}", text(&output));
        assert!(text(&output).contains("approved"));
        assert_eq!(harness.status("call-approved"), AuditStatus::Succeeded);
        assert_eq!(harness.records_for("call-approved"), 1);
        assert_eq!(
            harness
                .audit
                .record_for("call-approved")
                .unwrap()
                .approval_source
                .as_deref(),
            Some("approval"),
            "the approver must stay recorded through the re-evaluation"
        );
    });
}

// ---------------------------------------------------------------------------
// Audit hygiene
// ---------------------------------------------------------------------------

#[test]
fn audit_records_never_retain_raw_commands_or_output() {
    asupersync::test_utils::run_test(|| async {
        let workspace = tempfile::tempdir().unwrap();
        let harness = Harness::new(workspace.path(), AgentMode::Normal, PermissionMode::Bypass);

        harness
            .call(
                "exec",
                "call-secret",
                json!({"command": "echo TOKEN=super-secret-value"}),
            )
            .await;

        let serialized =
            serde_json::to_string(&harness.audit.snapshot()).expect("audit serializes");
        assert!(!serialized.contains("super-secret-value"));
        assert!(!serialized.contains("TOKEN="));

        let record = harness.audit.record_for("call-secret").unwrap();
        assert_eq!(record.argument_hash.len(), 64);
        assert_eq!(record.status, AuditStatus::Succeeded);
        assert!(record.ended_at.is_some());
    });
}

#[test]
fn argument_hashes_are_only_comparable_inside_one_audit_log() {
    let arguments = json!({"command": "echo same"});
    let first = AuditLog::new(8);
    let second = AuditLog::new(8);
    assert_eq!(
        first.hash_arguments(&arguments),
        first.hash_arguments(&arguments),
        "hashes must be stable within one log so repeated calls correlate"
    );
    assert_ne!(
        first.hash_arguments(&arguments),
        second.hash_arguments(&arguments),
        "per-log salts must prevent cross-session fingerprinting"
    );
}
