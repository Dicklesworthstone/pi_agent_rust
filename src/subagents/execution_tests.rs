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

const CAPTURE_ROOT_TRANSPORT: &str = r#"set -eu
printf 'launched\n' >> root-launches
while [ "$#" -gt 0 ]; do
    case "$1" in
        --workspace-roots-file)
            shift
            test -f "$1"
            printf '%s\n' "$1" >> root-manifest-paths
            cat "$1" >> root-manifests
            printf '\n' >> root-manifests
            ;;
        --append-system-prompt)
            shift
            cat "$1" >> root-system-prompts
            printf '\n' >> root-system-prompts
            ;;
    esac
    shift
done
"#;

#[test]
fn workspace_grants_are_frozen_through_queue_retry_and_revival_then_stay_revoked() {
    let script = format!(
        "{CAPTURE_ROOT_TRANSPORT}\nif [ -f retried ]; then\n{}else\ntouch retried\n{}fi\n",
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("retry required", "stop")]),
    );
    let (root, mut tool) = fixture(&script);
    let additional = tempfile::tempdir().expect("additional repository");
    let later = tempfile::tempdir().expect("later root");
    let mut workspace = crate::workspace::WorkspaceHandle::single(root.path());
    workspace.add_root(additional.path());
    tool.workspace = workspace.clone();
    let request: SubagentRequest = serde_json::from_value(json!({
        "agent":"worker","task":"produce typed evidence",
        "outputSchema":{"type":"object","required":["accepted"]},"schemaMode":"strict"
    }))
    .expect("strict request");
    let prepared = tool.prepare_request(&request).expect("capture root grants");
    workspace.add_root(later.path());
    let results = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("native runtime")
        .block_on(prepared.run(None));
    assert!(!results[0].is_error, "{:?}", results[0]);
    assert_eq!(results[0].schema_retries, Some(1));
    let first_id = results[0].hub_id.as_deref().expect("accepted child ID");
    let revived = revive_through_hub(root.path(), first_id);
    assert!(!revived.is_error, "{revived:?}");
    let canonical = crate::extensions::safe_canonicalize(additional.path());
    let expected = json!([canonical]);
    let manifests = std::fs::read_to_string(root.path().join("root-manifests"))
        .expect("original and explicitly revived child manifests");
    assert_eq!(
        manifests.lines().count(),
        2,
        "schema-only correction must not inherit workspace access"
    );
    for line in manifests.lines() {
        let value: Value = serde_json::from_str(line).expect("bounded JSON manifest");
        assert_eq!(value["roots"], expected);
    }
    let prompt = std::fs::read_to_string(root.path().join("root-system-prompts"))
        .expect("actual child instructions");
    assert!(prompt.contains("Delegated workspace access"));
    assert!(!prompt.contains(later.path().to_str().expect("UTF-8 fixture")));
    for path in std::fs::read_to_string(root.path().join("root-manifest-paths"))
        .expect("private manifest lifetimes")
        .lines()
    {
        assert!(
            !Path::new(path).exists(),
            "reaped child retained a manifest"
        );
    }
    assert!(workspace.remove_root(additional.path()));
    workspace.add_root(additional.path());
    let refused = revive_through_hub(root.path(), revived_id(&revived));
    assert!(refused.is_error, "{refused:?}");
    assert!(
        refused
            .details
            .unwrap()
            .to_string()
            .contains("PI_SUBAGENT_WORKSPACE_REVOKED")
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("root-launches"))
            .expect("no replacement process")
            .lines()
            .count(),
        3
    );
}

#[test]
fn revoked_workspace_grants_refuse_launch_or_reap_the_live_child() {
    for boundary in ["starting", "running"] {
        let (root, mut tool) = fixture("sleep 30");
        let additional = tempfile::tempdir().expect("additional root");
        let mut workspace = crate::workspace::WorkspaceHandle::single(root.path());
        workspace.add_root(additional.path());
        tool.workspace = workspace.clone();
        let path = additional.path().to_path_buf();
        let output = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("native runtime")
            .block_on(tool.execute(
                "root-revocation",
                request(),
                Some(Box::new(move |update| {
                    if update.details.as_ref().and_then(|details| {
                        details.pointer("/result/status").and_then(Value::as_str)
                    }) == Some(boundary)
                    {
                        let mut workspace = workspace.clone();
                        workspace.remove_root(&path);
                        workspace.add_root(&path);
                    }
                })),
            ))
            .expect("native failure result");
        assert!(output.is_error, "{boundary}: {output:?}");
        let record = result(&output);
        assert!(
            record["error"]
                .as_str()
                .unwrap()
                .contains("PI_SUBAGENT_WORKSPACE_REVOKED")
        );
        if boundary == "starting" {
            assert!(record["pid"].is_null());
        } else {
            let pid = i32::try_from(record["pid"].as_u64().expect("native process")).unwrap();
            let pid = rustix::process::Pid::from_raw(pid).expect("positive process ID");
            assert!(rustix::process::test_kill_process(pid).is_err());
        }
    }
}

#[test]
fn workflow_and_tan_children_receive_the_same_admitted_workspace() {
    let script = format!(
        "{CAPTURE_ROOT_TRANSPORT}\n{}",
        emit(&[ended("finished", "stop")])
    );
    let (root, mut tool) = fixture(&script);
    let additional = tempfile::tempdir().expect("additional repository");
    let mut workspace = crate::workspace::WorkspaceHandle::single(root.path());
    workspace.add_root(additional.path());
    tool.workspace = workspace;
    let output = run(
        &tool,
        json!({"workflow":[
            {"id":"inspect","call":{"agent":"worker","task":"inspect sibling"}},
            {"id":"follow","needs":["inspect"],"call":{"agent":"worker","task":"use sibling"}}
        ]}),
    );
    assert!(!output.is_error, "{output:?}");
    let tan = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("native runtime")
        .block_on(tool.run_background_tan("inspect sibling independently"))
        .expect("background tan");
    assert!(!tan.is_error, "{tan:?}");
    assert_eq!(
        tan.workspace_roots.unwrap()["inherited"],
        json!([crate::extensions::safe_canonicalize(additional.path())])
    );
    let manifests = std::fs::read_to_string(root.path().join("root-manifests"))
        .expect("workflow and tan manifests");
    assert_eq!(manifests.lines().count(), 3);
    for line in manifests.lines() {
        let manifest: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            manifest["roots"],
            json!([crate::extensions::safe_canonicalize(additional.path())])
        );
    }
}

#[test]
fn borrowed_privacy_callbacks_cannot_authorize_persistent_workspace_disclosures() {
    let (root, mut tool) = fixture("touch unexpectedly-launched");
    let additional = tempfile::tempdir().unwrap();
    let mut workspace = crate::workspace::WorkspaceHandle::single(root.path());
    workspace.add_root(additional.path());
    tool.workspace = workspace;
    let protect = |parts: &[&str]| Ok(parts.iter().map(|part| (*part).to_string()).collect());
    let error = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(tool.execute_with_auxiliary_privacy(
            "borrowed-root-policy",
            request(),
            None,
            &protect,
        ))
        .expect_err("generated child mappings need an owned policy");
    assert!(error.to_string().contains("PI_SUBAGENT_WORKSPACE_PRIVACY"));
    assert!(!root.path().join("unexpectedly-launched").exists());
}

#[test]
fn file_tasks_reach_real_child_stdin_beyond_argv_limits_with_source_provenance() {
    use sha2::{Digest as _, Sha256};

    let script = format!(
        "printf '%s\\n' \"$@\" > arguments\ncat > instructions-used\n{}",
        emit(&[ended("complete", "stop")])
    );
    let (root, tool) = fixture(&script);
    let source = root.path().join("instructions.md");
    let text = format!("Inspect every case.\n{}", "résumé 雪\n".repeat(24 * 1024));
    std::fs::write(&source, &text).unwrap();
    let output = run(&tool, json!({"agent":"worker", "taskFile":source}));
    assert!(!output.is_error, "{output:?}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("instructions-used")).unwrap(),
        format!("Task: {text}")
    );
    let arguments = std::fs::read_to_string(root.path().join("arguments")).unwrap();
    assert!(!arguments.contains("Inspect every case."));
    assert!(!arguments.contains(source.to_str().unwrap()));
    assert_eq!(
        result(&output)["taskFile"],
        source.to_string_lossy().as_ref()
    );
    assert_eq!(
        result(&output)["taskFileSha256"],
        crate::package_manager::hex_encode(&Sha256::digest(text.as_bytes()))
    );
    assert_eq!(result(&output)["task"], source.to_string_lossy().as_ref());
    let visible = crate::agent_hub::registry()
        .lock()
        .expect("hub registry")
        .get(result(&output)["hubId"].as_str().expect("hub ID"))
        .expect("registered file task");
    assert_eq!(visible.task, source.to_string_lossy().as_ref());
}

#[test]
fn inline_tasks_preserve_small_and_large_utf8_in_stdin_without_file_provenance() {
    for text in [
        "--model is task text, not a flag.\r\nKeep trailing spaces.  \n".to_string(),
        format!("Inspect every case.\n{}", "résumé 雪\n".repeat(24 * 1024)),
    ] {
        let script = format!(
            "printf '%s\\n' \"$@\" > arguments\ncat > instructions-used\n{}",
            emit(&[ended("complete", "stop")])
        );
        let (root, tool) = fixture(&script);
        let output = run(&tool, json!({"agent":"worker", "task":text}));
        assert!(!output.is_error, "{output:?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("instructions-used")).unwrap(),
            format!("Task: {text}")
        );
        let arguments = std::fs::read_to_string(root.path().join("arguments")).unwrap();
        assert!(!arguments.contains(&text));
        assert!(!arguments.contains("Task: "));
        assert_eq!(result(&output)["task"], text);
        assert!(result(&output).get("taskFile").is_none());
        assert!(result(&output).get("taskFileSha256").is_none());
    }
}

#[test]
fn large_definition_and_schema_use_a_frozen_prompt_file_for_each_native_retry()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let script = format!(
        r#"set -eu
printf '%s\n' "$@" >> arguments
while [ "$#" -gt 0 ]; do
    case "$1" in
        --append-system-prompt|--system-prompt)
            shift
            test -f "$1"
            printf '%s\n' "$1" >> prompt-paths
            cat "$1" >> system-instructions-used
            ;;
    esac
    shift
done
cat >> task-instructions-used
if [ -f retried ]; then
{}
else
    touch retried
    printf '%s\n' 'changed after admission' > global/agents/worker.md
{}
fi
"#,
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (root, tool) = fixture(&script);
    let body = format!("Full definition.\n{}", "résumé 雪\n".repeat(24 * 1024));
    std::fs::write(
        root.path().join("global/agents/worker.md"),
        format!("---\nname: worker\ndescription: large native instructions\n---\n{body}"),
    )?;
    let schema = json!({"type":"object", "required":["accepted"],
        "description":"schema context ".repeat(3 * 1024)});
    let output = run(
        &tool,
        json!({"agent":"worker", "task":"complete the original assignment",
            "outputSchema":schema, "schemaMode":"strict"}),
    );
    assert!(!output.is_error, "{output:?}");
    assert_eq!(result(&output)["schemaRetries"], 1);
    assert_eq!(result(&output)["data"], json!({"accepted":true}));
    let original_prompt = format!(
        "{}\n\nYour final answer MUST be a single JSON value matching this JSON Schema (no prose, no code fences):\n{schema}",
        body.trim(),
    );
    let correction_prompt = format!(
        "{SCHEMA_CORRECTION_SYSTEM_PROMPT}\n\nYour final answer MUST be a single JSON value matching this JSON Schema (no prose, no code fences):\n{schema}"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("system-instructions-used"))?,
        format!("{original_prompt}{correction_prompt}")
    );
    let arguments = std::fs::read_to_string(root.path().join("arguments"))?;
    assert!(!arguments.contains("Full definition."));
    assert!(!arguments.contains("schema context"));
    assert!(arguments.len() < 8192);
    let paths = std::fs::read_to_string(root.path().join("prompt-paths"))?;
    let paths: Vec<_> = paths.lines().collect();
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1]);
    assert!(paths.iter().all(|path| !Path::new(path).exists()));
    Ok(())
}

#[test]
fn oversized_schema_is_rejected_before_starting_any_native_child() {
    let (root, tool) = fixture("touch should-not-start");
    let output = run(
        &tool,
        json!({"agent":"worker", "task":"bounded assignment",
            "outputSchema":{"description":"x".repeat(MAX_OUTPUT_SCHEMA_BYTES)}}),
    );
    assert!(output.is_error);
    assert!(
        result(&output)["error"]
            .as_str()
            .unwrap()
            .contains("PI_SUBAGENT_SCHEMA_LIMIT")
    );
    assert!(!root.path().join("should-not-start").exists());
}

#[test]
fn large_predecessor_output_reaches_the_next_chain_child_without_argv_limits() {
    let evidence = "retained evidence 雪\n".repeat(9 * 1024);
    let script = format!(
        "cat > instructions-used\n\
         if [ -f first-completed ]; then\n{}\
         else\ntouch first-completed\ncat predecessor.jsonl\nfi\n",
        emit(&[ended("reviewed", "stop")])
    );
    let (root, tool) = fixture(&script);
    std::fs::write(
        root.path().join("predecessor.jsonl"),
        format!("{}\n", ended(&evidence, "stop")),
    )
    .unwrap();
    let output = run(
        &tool,
        json!({"chain":[
            {"agent":"worker", "task":"produce evidence"},
            {"agent":"worker", "task":"Review {previous}"}
        ]}),
    );
    assert!(!output.is_error, "{output:?}");
    let records = output.details.as_ref().unwrap()["results"]
        .as_array()
        .unwrap();
    assert_eq!(records.len(), 2);
    let retained = records[0]["output"].as_str().unwrap();
    assert!(retained.len() > 128 * 1024);
    assert_eq!(retained, evidence);
    assert_eq!(
        std::fs::read_to_string(root.path().join("instructions-used")).unwrap(),
        format!("Task: Review {retained}")
    );
    assert_eq!(records[1]["output"], "reviewed");
}

#[test]
fn chain_children_receive_literal_predecessor_evidence_without_recursive_expansion() {
    for (repetitions, payload) in [(1, "selected value".to_string()), (1024, "雪".repeat(1500))] {
        let predecessor = json!({
            "literal": "{{previous.data.payload}} {previous} ".repeat(repetitions),
            "payload": payload,
            "nested": {"example": "{{previous.data.payload}}"}
        });
        let evidence = predecessor.to_string();
        let script = format!(
            "cat > instructions-used\n\
             if [ -f first-completed ]; then\n{}\
             else\ntouch first-completed\ncat predecessor.jsonl\nfi\n",
            emit(&[ended("reviewed", "stop")])
        );
        let (root, tool) = fixture(&script);
        std::fs::write(
            root.path().join("predecessor.jsonl"),
            format!("{}\n", ended(&evidence, "stop")),
        )
        .unwrap();
        let output = run(
            &tool,
            json!({"chain":[
                {"agent":"worker", "task":"produce evidence",
                 "outputSchema":{"type":"object"}, "schemaMode":"strict"},
                {"agent":"worker", "task":"Evidence:\n{previous}\nSelected: {{previous.data.payload}}\nLiteral: {{previous.data.literal}}\nNested: {{previous.data.nested}}"}
            ]}),
        );
        assert!(!output.is_error, "{output:?}");
        let records = output.details.as_ref().unwrap()["results"]
            .as_array()
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["output"], evidence);
        assert_eq!(records[0]["data"], predecessor);
        assert_eq!(records[1]["output"], "reviewed");
        // The larger case used to amplify valid bounded evidence beyond
        // 4 MiB and refuse the second child; the smaller case corrupted it.
        assert_eq!(
            std::fs::read_to_string(root.path().join("instructions-used")).unwrap(),
            format!(
                "Task: Evidence:\n{evidence}\nSelected: {payload}\nLiteral: {}\nNested: {}",
                predecessor["literal"].as_str().unwrap(),
                predecessor["nested"]
            )
        );
    }
}

#[test]
fn oversized_inline_chain_expansion_preserves_prior_results_without_a_second_launch() {
    let predecessor = json!({"value":"x".repeat(16 * 1024)}).to_string();
    for marker in ["{previous}", "{{previous.data.value}}"] {
        let script = format!(
            "printf 'launched\\n' >> launches\n{}",
            emit(&[ended(&predecessor, "stop")])
        );
        let (root, tool) = fixture(&script);
        let output = run(
            &tool,
            json!({"chain":[
                {"agent":"worker", "task":"produce typed evidence",
                 "outputSchema":{"type":"object"}, "schemaMode":"strict"},
                {"agent":"worker", "task":marker.repeat(257)}
            ]}),
        );
        assert!(output.is_error, "{output:?}");
        let records = output.details.as_ref().unwrap()["results"]
            .as_array()
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["status"], "completed");
        assert_eq!(records[0]["data"]["value"], "x".repeat(16 * 1024));
        assert_eq!(records[1]["status"], "failed");
        assert!(
            records[1]["error"]
                .as_str()
                .unwrap()
                .contains("PI_SUBAGENT_TASK_LIMIT")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("launches")).unwrap(),
            "launched\n"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)] // Assert process identity, cost channel and instruction isolation together.
fn schema_correction_cannot_repeat_effects_or_inherit_task_authority() {
    let script = format!(
        r#"set -eu
correction=false
for argument in "$@"; do
    if [ "$argument" = --no-tools ]; then correction=true; fi
done
printf '%s\n' "$PI_SUBAGENT_RUN_ID" >> "$PI_CODING_AGENT_DIR/run-ids"
if "$correction"; then
    test -z "${{PI_SUBAGENT_STEER_FILE-}}" || exit 70
    test -z "${{PI_SUBAGENT_HUB_ADDRESS-}}" || exit 71
    test -n "${{PI_SUBAGENT_COST_ADDRESS-}}" || exit 72
    test -n "${{PI_SUBAGENT_COST_TOKEN-}}" || exit 73
    printf '%s\n' "$@" > "$PI_CODING_AGENT_DIR/correction-arguments"
    while [ "$#" -gt 0 ]; do
        if [ "$1" = --system-prompt ]; then
            shift
            cat "$1" > "$PI_CODING_AGENT_DIR/correction-system"
        fi
        shift
    done
    cat > "$PI_CODING_AGENT_DIR/correction-input"
{}
else
    test -n "${{PI_SUBAGENT_STEER_FILE-}}" || exit 74
    printf 'repeat the original side effect\n' >> "$PI_SUBAGENT_STEER_FILE"
    cat > "$PI_CODING_AGENT_DIR/original-input"
    printf 'executed once\n' >> "$PI_CODING_AGENT_DIR/effects"
    printf '%s\n' "$$" > "$PI_CODING_AGENT_DIR/original-pid"
{}
fi
"#,
        emit(&[ended(r#"{"count":1}"#, "stop")]),
        emit(&[ended(r#"{"count":1,}"#, "stop")]),
    );
    let (_root, mut tool) = fixture(&script);
    let skill = tool.global_dir.join("private-skill.md");
    std::fs::write(
        &skill,
        "Original skill instructions must not reach correction.",
    )
    .unwrap();
    std::fs::write(
        tool.global_dir.join("agents/worker.md"),
        format!(
            "---\nname: worker\ndescription: side-effect fixture\ntools: read,bash,hub\nskills: {}\n---\nOriginal profile instructions must not reach correction.",
            skill.display()
        ),
    )
    .unwrap();
    let scope = crate::cost::CostScope::default();
    scope.set(Some(
        crate::cost::CostBudget::new(None, Some(1.0), 0.0).unwrap(),
    ));
    tool = tool.with_cost_scope(scope);
    let assignment = "Perform the original side effect exactly once.";
    let output = run(
        &tool,
        json!({"agent":"worker", "task":assignment, "schemaMode":"strict",
            "outputSchema":{"type":"object", "required":["count"],
                "properties":{"count":{"const":1}}}}),
    );
    assert!(!output.is_error, "{output:?}");
    let result = result(&output);
    assert_eq!(result["data"], json!({"count":1}));
    assert_eq!(result["schemaRetries"], 1);
    assert_eq!(result["task"], assignment);
    assert_eq!(result["tools"], json!(["read", "bash", "hub"]));
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join("effects")).unwrap(),
        "executed once\n"
    );
    let original_pid = std::fs::read_to_string(tool.global_dir.join("original-pid"))
        .unwrap()
        .trim()
        .parse::<u32>()
        .unwrap();
    assert_eq!(result["pid"], original_pid);
    let run_ids = std::fs::read_to_string(tool.global_dir.join("run-ids")).unwrap();
    let run_ids: Vec<_> = run_ids.lines().collect();
    assert_eq!(run_ids.len(), 2);
    assert_eq!(run_ids[0], run_ids[1]);
    assert_eq!(result["hubId"], run_ids[0]);
    let arguments = std::fs::read_to_string(tool.global_dir.join("correction-arguments")).unwrap();
    let arguments: Vec<_> = arguments.lines().collect();
    for required in [
        "--no-tools",
        "--no-implicit-tools",
        "--no-mcp",
        "--no-extensions",
        "--no-skills",
        "--no-context-files",
        "--no-prompt-templates",
        "--no-themes",
        "--no-migrations",
        "--hide-cwd-in-prompt",
        "--system-prompt",
    ] {
        assert!(arguments.contains(&required), "missing {required}");
    }
    for forbidden in ["--skill", "--workspace-roots-file", "--append-system-prompt"] {
        assert!(!arguments.contains(&forbidden), "leaked {forbidden}");
    }
    let correction_system =
        std::fs::read_to_string(tool.global_dir.join("correction-system")).unwrap();
    assert!(correction_system.starts_with(SCHEMA_CORRECTION_SYSTEM_PROMPT));
    assert!(!correction_system.contains("Original profile instructions"));
    assert!(!correction_system.contains("Original skill instructions"));
    let correction_input = std::fs::read_to_string(tool.global_dir.join("correction-input")).unwrap();
    let correction: Value =
        serde_json::from_str(correction_input.strip_prefix("Task: ").unwrap()).unwrap();
    assert_eq!(correction["previousOutput"], r#"{"count":1,}"#);
    assert!(!correction_input.contains(assignment));
    assert!(!correction_input.contains("repeat the original side effect"));
    let source = crate::agent_hub::registry()
        .lock()
        .unwrap()
        .native_revival_source(run_ids[0])
        .unwrap();
    assert_eq!(source.launch.task.task, assignment);
    assert!(source.entry.revived_from.is_none());
}

#[test]
fn large_inline_schema_retry_uses_new_stdin_and_retains_the_original_assignment() {
    let script = format!(
        "printf '\\n---attempt---\\n' >> instructions-used\ncat >> instructions-used\n\
         if [ -f retried ]; then\n{}\
         else\ntouch retried\n{}\
         fi\n",
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (root, tool) = fixture(&script);
    let original = format!(
        "Inspect the whole assignment.\n{}",
        "résumé 雪\n".repeat(24 * 1024)
    );
    let output = run(
        &tool,
        json!({"agent":"worker", "task":original,
            "outputSchema":{"type":"object", "required":["accepted"]}, "schemaMode":"strict"}),
    );
    assert!(!output.is_error, "{output:?}");
    assert_eq!(result(&output)["schemaRetries"], 1);
    assert_eq!(result(&output)["task"], original);
    let instructions = std::fs::read_to_string(root.path().join("instructions-used")).unwrap();
    assert_eq!(instructions.matches("---attempt---").count(), 2);
    assert_eq!(instructions.matches(original.as_str()).count(), 1);
    let correction = instructions.rsplit("---attempt---\nTask: ").next().unwrap();
    let correction: Value = serde_json::from_str(correction).unwrap();
    assert_eq!(correction["previousOutput"], "not JSON");
    assert!(correction["validationErrors"].is_array());
}

#[test]
fn a_maximal_assignment_does_not_consume_the_answer_correction_budget() {
    let script = format!(
        "printf 'launched\\n' >> launches\n\
         if [ -f attempted ]; then\n{}else\n: > attempted\n{}fi\n",
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")])
    );
    let (root, tool) = fixture(&script);
    let original = "x".repeat(task_file::MAX_BYTES);
    let output = run(
        &tool,
        json!({"agent":"worker", "task":original,
            "outputSchema":{"type":"object"}, "schemaMode":"strict"}),
    );
    assert!(!output.is_error, "{output:?}");
    assert_eq!(result(&output)["schemaRetries"], 1);
    assert_eq!(result(&output)["schemaValid"], true);
    assert_eq!(result(&output)["data"], json!({"accepted":true}));
    assert_eq!(result(&output)["task"], original);
    assert_eq!(
        std::fs::read_to_string(root.path().join("launches")).unwrap(),
        "launched\nlaunched\n"
    );
}

#[test]
fn amplified_schema_failures_still_launch_one_bounded_corrective_child() {
    let script = r"set -eu
printf 'launched\n' >> launches
if [ -f attempted ]; then
    cat > corrective-task
    cat corrected-events.jsonl
else
    cat > original-task
    printf 'attempted\n' > attempted
    cat invalid-events.jsonl
fi
";
    let (root, tool) = fixture(script);
    let invalid = json!(vec!["rejected-value-".repeat(14); 1200]).to_string();
    assert!(invalid.len() < MAX_CHILD_OUTPUT_BYTES);
    std::fs::write(
        root.path().join("invalid-events.jsonl"),
        format!("{}\n", ended(&invalid, "stop")),
    )
    .unwrap();
    std::fs::write(
        root.path().join("corrected-events.jsonl"),
        format!("{}\n", ended("[7]", "stop")),
    )
    .unwrap();
    // Every item fails all 64 branches. The former error collection exceeded
    // the 4 MiB task limit, refusing the retry despite a bounded child answer.
    let schema = json!({"type":"array", "items":{"allOf":vec![json!({"type":"integer"});64]}});
    let assignment = "Return the requested structured results.";
    let output = run(
        &tool,
        json!({"agent":"worker", "task":assignment,
            "outputSchema":schema, "schemaMode":"strict"}),
    );
    assert!(!output.is_error, "{output:?}");
    assert_eq!(result(&output)["schemaRetries"], 1);
    assert_eq!(result(&output)["schemaValid"], true);
    assert_eq!(result(&output)["data"], json!([7]));
    assert_eq!(result(&output)["task"], assignment);
    assert_eq!(
        std::fs::read_to_string(root.path().join("launches")).unwrap(),
        "launched\nlaunched\n"
    );
    let correction = std::fs::read_to_string(root.path().join("corrective-task")).unwrap();
    let correction: Value = serde_json::from_str(correction.strip_prefix("Task: ").unwrap()).unwrap();
    assert_eq!(correction["previousOutput"], invalid);
    let errors = correction["validationErrors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    let diagnostic = errors[0].as_str().unwrap();
    assert!(diagnostic.len() <= MAX_OUTPUT_VALIDATION_BYTES);
    assert!(diagnostic.contains("PI_SUBAGENT_SCHEMA_OUTPUT"));
    assert!(diagnostic.contains(OUTPUT_VALIDATION_TRUNCATED));
    assert!(!diagnostic.contains("rejected-value-"));
    assert!(!correction.to_string().contains(assignment));
}

#[test]
fn workflow_schema_retry_keeps_its_smaller_complete_task_budget() {
    let script = format!(
        "printf 'launched\\n' >> launches\n{}",
        emit(&[ended(&"x".repeat(64 * 1024), "stop")])
    );
    let (root, tool) = fixture(&script);
    let output = run(
        &tool,
        json!({"workflow":[{"id":"bounded", "call":{
            "agent":"worker", "task":"return the bounded answer",
            "outputSchema":{"type":"object"}, "schemaMode":"strict"
        }}]}),
    );
    assert!(output.is_error, "{output:?}");
    let result = &output.details.as_ref().unwrap()["nodes"][0]["result"];
    assert_eq!(result["schemaRetries"], 0);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("PI_SUBAGENT_TASK_LIMIT")
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("launches")).unwrap(),
        "launched\n"
    );
}

#[test]
fn invalid_later_file_tasks_launch_no_valid_prefix_in_any_delegation_mode() {
    let (root, tool) = fixture("touch launched");
    let bad = root.path().join("bad.md");
    std::fs::write(&bad, [0xff]).unwrap();
    let first = json!({"agent":"worker", "task":"valid first task"});
    let later = json!({"agent":"worker", "taskFile":bad});
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    for input in [
        later.clone(),
        json!({"tasks":[first, later], "concurrency":1}),
        json!({"chain":[first, later]}),
        json!({"workflow":[
            {"id":"first", "call":first},
            {"id":"later", "needs":["first"], "call":later}
        ]}),
    ] {
        assert!(
            runtime
                .block_on(tool.execute("invalid-file", input, None))
                .is_err()
        );
        assert!(!root.path().join("launched").exists());
    }
    std::fs::write(&bad, "x".repeat(64 * 1024 + 1)).unwrap();
    let oversized_graph = json!({"workflow":[
        {"id":"first", "call":first},
        {"id":"later", "needs":["first"], "call":later}
    ]});
    assert!(
        runtime
            .block_on(tool.execute("oversized-file", oversized_graph, None))
            .is_err()
    );
    assert!(!root.path().join("launched").exists());
}

#[test]
fn queued_chain_file_snapshots_keep_original_bytes_and_render_previous_results() {
    use sha2::{Digest as _, Sha256};

    let script = format!(
        "cat > instructions-used\n{}",
        emit(&[ended("accepted", "stop")])
    );
    let (root, tool) = fixture(&script);
    let source = root.path().join("review.md");
    let text = "Review {previous}.";
    std::fs::write(&source, text).unwrap();
    let request: SubagentRequest = serde_json::from_value(json!({"chain":[
        {"agent":"worker", "task":"produce evidence"},
        {"agent":"worker", "taskFile":source}
    ]}))
    .unwrap();
    let prepared = tool.prepare_request(&request).unwrap();
    std::fs::write(&source, "replacement must not run").unwrap();
    let results = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(prepared.run(None));
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| !result.is_error));
    assert_eq!(
        std::fs::read_to_string(root.path().join("instructions-used")).unwrap(),
        "Task: Review accepted."
    );
    let digest = crate::package_manager::hex_encode(&Sha256::digest(text.as_bytes()));
    assert_eq!(
        results[1].task_file_sha256.as_deref(),
        Some(digest.as_str())
    );
}

#[test]
fn oversized_raw_and_typed_file_chain_expansion_fails_before_that_child_launches() {
    use sha2::{Digest as _, Sha256};

    let predecessor = json!({"value":"x".repeat(16 * 1024)}).to_string();
    for marker in ["{previous}", "{{previous.data.value}}"] {
        let script = format!(
            "printf 'launched\\n' >> launches\n{}",
            emit(&[ended(&predecessor, "stop")])
        );
        let (root, tool) = fixture(&script);
        let source = root.path().join("expand.md");
        let instructions = marker.repeat(257);
        std::fs::write(&source, &instructions).expect("bounded authored instructions");
        let output = run(
            &tool,
            json!({"chain":[
                {"agent":"worker", "task":"produce bounded typed evidence",
                 "outputSchema":{"type":"object", "required":["value"]}, "schemaMode":"strict"},
                {"agent":"worker", "taskFile":source}
            ]}),
        );
        assert!(output.is_error, "{output:?}");
        let records = &output.details.as_ref().expect("chain details")["results"];
        assert_eq!(records.as_array().expect("chain results").len(), 2);
        assert_eq!(records[0]["status"], "completed");
        assert_eq!(records[1]["status"], "failed");
        assert!(
            records[1]["error"]
                .as_str()
                .expect("expansion failure")
                .contains("PI_SUBAGENT_TASK_LIMIT")
        );
        assert_eq!(records[1]["taskFile"], source.to_string_lossy().as_ref());
        assert_eq!(
            records[1]["taskFileSha256"],
            crate::package_manager::hex_encode(&Sha256::digest(instructions.as_bytes()))
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("launches")).expect("real launches"),
            "launched\n"
        );
    }
}

#[test]
fn workflow_file_instructions_keep_typed_predecessor_data_and_source_digest() {
    use sha2::{Digest as _, Sha256};

    let script = format!(
        "cat > instructions-used\n{}",
        emit(&[ended(r#"{"verdict":"accepted"}"#, "stop")])
    );
    let (root, tool) = fixture(&script);
    let source = root.path().join("join.md");
    let text = "Review the accepted predecessor evidence.";
    std::fs::write(&source, text).unwrap();
    let output = run(
        &tool,
        json!({"workflow":[
            {"id":"produce", "call":{"agent":"worker", "task":"produce typed evidence", "outputSchema":{"type":"object"}}},
            {"id":"join", "needs":["produce"], "inputs":{"evidence":{"from":"produce","pointer":""}},
             "call":{"agent":"worker", "taskFile":source}}
        ]}),
    );
    assert!(!output.is_error, "{output:?}");
    let instructions = std::fs::read_to_string(root.path().join("instructions-used")).unwrap();
    assert!(instructions.starts_with(&format!("Task: {text}")));
    assert!(instructions.contains("<workflow-inputs>"));
    assert!(instructions.contains(r#""evidence":{"verdict":"accepted"}"#));
    assert_eq!(
        output.details.as_ref().unwrap()["nodes"][1]["result"]["task"],
        source.to_string_lossy().as_ref()
    );
    assert_eq!(
        output.details.as_ref().unwrap()["nodes"][1]["result"]["taskFileSha256"],
        crate::package_manager::hex_encode(&Sha256::digest(text.as_bytes()))
    );
}

#[test]
fn revoking_task_file_root_after_preparation_prevents_child_launch() {
    let (root, tool) = fixture("touch launched");
    let outside = tempfile::tempdir().expect("additional root");
    let source = outside.path().join("instructions.md");
    std::fs::write(&source, "Inspect the admitted workspace.").expect("task file");
    let mut roots = crate::workspace::WorkspaceHandle::single(root.path());
    roots.add_root(outside.path());
    let tool = tool.with_workspace(roots.clone());
    let request: SubagentRequest =
        serde_json::from_value(json!({"agent":"worker", "taskFile":source})).expect("file request");
    let prepared = tool.prepare_request(&request).expect("admitted file");
    assert!(roots.remove_root(outside.path()));
    let results = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("native runtime")
        .block_on(prepared.run(None));
    assert_eq!(results.len(), 1);
    assert!(results[0].is_error);
    assert!(!root.path().join("launched").exists());
}

#[test]
fn task_file_snapshot_survives_schema_retry_and_explicit_hub_revival() {
    use sha2::{Digest as _, Sha256};

    let script = format!(
        "printf '\\n---attempt---\\n' >> instructions-used\ncat >> instructions-used\n\
         if [ -f retried ]; then\n{}\
         else\ntouch retried\n{}\
         fi\n",
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (root, tool) = fixture(&script);
    let source = root.path().join("instructions.md");
    let original = format!("Inspect the frozen assignment {}.", uuid::Uuid::new_v4());
    std::fs::write(&source, &original).expect("task file");
    let request: SubagentRequest = serde_json::from_value(json!({
        "agent":"worker", "taskFile":source,
        "outputSchema":{"type":"object", "required":["accepted"]}, "schemaMode":"strict"
    }))
    .expect("strict file request");
    let prepared = tool.prepare_request(&request).expect("snapshot");
    std::fs::write(&source, "replacement must not run").expect("replace source");
    let results = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("native runtime")
        .block_on(prepared.run(None));
    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error, "{:?}", results[0]);
    assert_eq!(results[0].schema_retries, Some(1));
    let digest = crate::package_manager::hex_encode(&Sha256::digest(original.as_bytes()));
    assert_eq!(
        results[0].task_file_sha256.as_deref(),
        Some(digest.as_str())
    );
    let revived = revive_through_hub(&tool.cwd, results[0].hub_id.as_deref().expect("hub ID"));
    assert!(!revived.is_error, "{revived:?}");
    let instructions = std::fs::read_to_string(root.path().join("instructions-used"))
        .expect("all native child instructions");
    assert_eq!(instructions.matches("---attempt---").count(), 3);
    assert_eq!(instructions.matches(original.as_str()).count(), 2);
    assert!(!instructions.contains("replacement must not run"));
}

fn result(output: &ToolOutput) -> &Value {
    &output.details.as_ref().unwrap()["results"][0]
}

fn record_model_script(body: &str) -> String {
    format!(
        "provider=\nmodel=\nthinking=\nfallback=\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             --provider) shift; provider=\"$1\" ;;\n\
             --model) shift; model=\"$1\" ;;\n\
             --thinking) shift; thinking=\"$1\" ;;\n\
             --subagent-fallback-policy) shift; fallback=\"$1\" ;;\n\
           esac\n\
           shift\n\
         done\n\
         printf '%s|%s\\n' \"$provider\" \"$model\" >> \"$PI_CODING_AGENT_DIR/models-used\"\n\
         printf '%s\\n' \"$thinking\" >> \"$PI_CODING_AGENT_DIR/model-thinking\"\n\
         if [ -n \"$fallback\" ]; then printf '%s\\n' \"$fallback\" >> \"$PI_CODING_AGENT_DIR/fallback-used\"; fi\n\
         {body}"
    )
}

fn recorded_models(tool: &SubagentTool) -> Vec<String> {
    std::fs::read_to_string(tool.global_dir.join("models-used"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn explicit_model_catalog(tool: &SubagentTool) {
    std::fs::write(tool.global_dir.join("models.json"), json!({"providers": {
        "selection-local": {
            "baseUrl":"http://127.0.0.1:1/v1", "api":"openai-completions", "authHeader":false,
            "models":[{"id":"selection-fast", "reasoning":false}, {"id":"selection-careful", "reasoning":true}]
        }
    }}).to_string()).unwrap();
}

fn fallback_config() -> Config {
    Config {
        retry: Some(crate::config::RetrySettings {
            fallback_chains: Some(std::collections::HashMap::from([
                (
                    "task".to_string(),
                    vec!["selection-local/selection-careful:high".to_string()],
                ),
                ("default".to_string(), vec![]),
            ])),
            ..Default::default()
        }),
        disabled_providers: Some(vec!["excluded-provider".to_string()]),
        ..Default::default()
    }
}

fn explicit_role_config() -> Config {
    Config {
        model_roles: Some(crate::config::ModelRoleSettings {
            smol: Some("selection-local/selection-fast".to_string()),
            slow: Some("selection-local/selection-careful:high".to_string()),
            ..Default::default()
        }),
        retry: Some(crate::config::RetrySettings {
            fallback_chains: Some(std::collections::HashMap::from([
                (
                    "smol".to_string(),
                    vec!["selection-local/selection-careful:high".to_string()],
                ),
                ("slow".to_string(), vec![]),
                ("default".to_string(), vec![]),
            ])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn record_fallback_script(body: &str) -> String {
    format!(
        "while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             --subagent-fallback-policy) shift; printf '%s\\n' \"$1\" >> \"$PI_CODING_AGENT_DIR/fallback-used\" ;;\n\
           esac\n\
           shift\n\
         done\n{body}"
    )
}

fn recorded_fallbacks(tool: &SubagentTool) -> Vec<Value> {
    std::fs::read_to_string(tool.global_dir.join("fallback-used"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn task_role_provenance_is_carried_in_all_modes_without_model_id_inference() {
    let role = json!({"agent":"worker", "task":"role-owned work"});
    let explicit =
        json!({"agent":"worker", "task":"explicit work", "model":"selection-local/selection-fast"});
    for input in [
        role.clone(),
        json!({"tasks":[role, explicit], "concurrency":1}),
        json!({"chain":[role, explicit]}),
        json!({"workflow":[
            {"id":"role", "call":role},
            {"id":"explicit", "needs":["role"], "call":explicit}
        ]}),
    ] {
        let (_dir, tool) = fixture(&record_fallback_script(&emit(&[ended("complete", "stop")])));
        explicit_model_catalog(&tool);
        let tool = tool
            .with_model_config(Some(fallback_config()))
            .with_role_model_selection(Some((
                crate::models::ModelRole::Task,
                "selection-local/selection-fast".to_string(),
            )));
        let output = run(&tool, input.clone());
        assert!(!output.is_error, "{output:?}");
        let policies = recorded_fallbacks(&tool);
        assert_eq!(policies[0]["role"], "task");
        assert_eq!(
            policies[0]["disabled_providers"],
            json!(["excluded-provider"])
        );
        if input.get("agent").is_none() {
            assert_eq!(policies.len(), 2);
            assert_eq!(
                policies[1]["role"], "default",
                "same model does not imply same role"
            );
        }
        std::fs::write(tool.global_dir.join("agents/worker.md"),
            "---\nname: worker\ndescription: pinned\nmodel: selection-local/selection-fast\n---\nComplete the task.").unwrap();
        assert!(!run(&tool, request()).is_error);
        assert_eq!(recorded_fallbacks(&tool).last().unwrap()["role"], "default");
    }
}

#[test]
fn admitted_fallback_snapshot_survives_configuration_changes_schema_retry_and_revival() {
    let body = format!(
        "if [ -f \"$PI_CODING_AGENT_DIR/retried\" ]; then\n{}\
         else\n: > \"$PI_CODING_AGENT_DIR/retried\"\n{}\
         fi\n",
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (_dir, mut tool) = fixture(&record_fallback_script(&body));
    tool = tool
        .with_model_config(Some(fallback_config()))
        .with_role_model_selection(Some((
            crate::models::ModelRole::Task,
            "selection-local/selection-fast".to_string(),
        )));
    let request: SubagentRequest = serde_json::from_value(json!({
        "chain":[
            {"agent":"worker", "task":format!("frozen-policy-{}", uuid::Uuid::new_v4()),
             "outputSchema":{"type":"object", "required":["accepted"]}, "schemaMode":"strict"},
            {"agent":"worker", "task":"follow-up"}
        ]
    }))
    .unwrap();
    let prepared = tool.prepare_request(&request).unwrap();
    tool.model_config
        .as_mut()
        .unwrap()
        .retry
        .as_mut()
        .unwrap()
        .fallback_chains
        .as_mut()
        .unwrap()
        .insert("task".to_string(), vec![]);
    let results = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(prepared.run(None));
    assert!(results.iter().all(|result| !result.is_error));
    assert_eq!(results[0].schema_retries, Some(1));
    let policies = recorded_fallbacks(&tool);
    assert_eq!(policies.len(), 3);
    assert!(policies.iter().all(|policy| policy == &policies[0]));
    assert_eq!(
        policies[0]["retry"]["fallback_chains"]["task"],
        json!(["selection-local/selection-careful:high"])
    );
    let revived = revive_through_hub(&tool.cwd, results[0].hub_id.as_deref().unwrap());
    assert!(!revived.is_error, "{revived:?}");
    assert_eq!(recorded_fallbacks(&tool)[3], policies[0]);
    assert!(!run(&tool, json!({"agent":"worker", "task":"new policy"})).is_error);
    assert_eq!(
        recorded_fallbacks(&tool)[4]["retry"]["fallback_chains"]["task"],
        json!([])
    );
}

#[test]
fn explicit_models_override_definition_role_and_parent_in_every_one_shot_mode() {
    let first =
        json!({"agent":"worker", "task":"inspect", "model":"selection-local/selection-fast"});
    let second = json!({"agent":"worker", "task":"review {previous}", "model":"selection-local/selection-careful:high"});
    for input in [
        first.clone(),
        json!({"tasks":[first, second], "concurrency":1}),
        json!({"chain":[first, second]}),
        json!({"workflow":[
            {"id":"inspect", "call":first},
            {"id":"review", "needs":["inspect"], "call":second}
        ]}),
    ] {
        let (_dir, tool) = fixture(&record_model_script(&emit(&[ended("complete", "stop")])));
        explicit_model_catalog(&tool);
        std::fs::write(tool.global_dir.join("agents/worker.md"),
            "---\nname: worker\ndescription: pinned fixture\nmodel: definition/model\n---\nComplete the task.").unwrap();
        let tool = tool
            .with_model_config(Some(Config::default()))
            .with_role_model_spec(Some("role/model".to_string()))
            .with_parent_model(Some(ToolModelSelection {
                provider: "parent".to_string(),
                model: "parent-model".to_string(),
            }));
        let expected = if input.get("agent").is_some() {
            vec!["selection-local|selection-fast"]
        } else {
            vec![
                "selection-local|selection-fast",
                "selection-local|selection-careful",
            ]
        };
        let output = run(&tool, input);
        assert!(!output.is_error, "{output:?}");
        assert_eq!(recorded_models(&tool), expected);
    }
}

#[test]
fn explicit_roles_route_every_mode_with_their_own_thinking_and_fallback_chain() {
    let first = json!({"agent":"worker", "task":"inspect", "model":{"role":"smol"}});
    let second = json!({"agent":"worker", "task":"review {previous}", "model":{"role":"slow"}});
    for input in [
        first.clone(),
        json!({"tasks":[first, second], "concurrency":1}),
        json!({"chain":[first, second]}),
        json!({"workflow":[
            {"id":"inspect", "call":first},
            {"id":"review", "needs":["inspect"], "call":second}
        ]}),
    ] {
        let (_dir, tool) = fixture(&record_model_script(&emit(&[ended("complete", "stop")])));
        explicit_model_catalog(&tool);
        std::fs::write(tool.global_dir.join("agents/worker.md"),
            "---\nname: worker\ndescription: pinned fixture\nmodel: definition/model\nreasoning: low\n---\nComplete the task.").unwrap();
        let tool = tool
            .with_model_config(Some(explicit_role_config()))
            .with_role_model_selection(Some((
                crate::models::ModelRole::Task,
                "role/model".to_string(),
            )))
            .with_parent_model(Some(ToolModelSelection {
                provider: "parent".to_string(),
                model: "parent-model".to_string(),
            }));
        let single = input.get("agent").is_some();
        let output = run(&tool, input);
        assert!(!output.is_error, "{output:?}");
        assert_eq!(recorded_models(&tool)[0], "selection-local|selection-fast");
        let efforts = std::fs::read_to_string(tool.global_dir.join("model-thinking")).unwrap();
        assert_eq!(efforts, if single { "low\n" } else { "low\nhigh\n" });
        let policies = recorded_fallbacks(&tool);
        assert_eq!(policies[0]["role"], "smol");
        assert_eq!(
            policies[0]["retry"]["fallback_chains"]["smol"],
            json!(["selection-local/selection-careful:high"])
        );
        assert_eq!(policies.len(), if single { 1 } else { 2 });
        if !single {
            assert_eq!(
                recorded_models(&tool)[1],
                "selection-local|selection-careful"
            );
            assert_eq!(policies[1]["role"], "slow");
        }
    }
}

#[test]
fn invalid_later_model_rejects_entire_request_before_process_or_isolation() {
    for key in ["tasks", "chain", "workflow"] {
        let (_dir, tool) = fixture(&record_model_script(&emit(&[ended("complete", "stop")])));
        explicit_model_catalog(&tool);
        let tool = tool.with_model_config(Some(Config::default()));
        let first = json!({"agent":"worker", "task":"must not run", "model":"selection-fast", "isolation":"worktree"});
        let second = json!({"agent":"worker", "task":"invalid destination", "model":"missing-provider/private-selector-sentinel"});
        let input = if key == "workflow" {
            json!({"workflow":[{"id":"first", "call":first}, {"id":"second", "needs":["first"], "call":second}]})
        } else {
            let mut input = json!({});
            input[key] = json!([first, second]);
            input
        };
        let error = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
            .block_on(tool.execute("invalid-model", input, None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("PI_SUBAGENT_MODEL"), "{error}");
        assert!(!error.contains("private-selector-sentinel"), "{error}");
        assert!(!tool.global_dir.join("models-used").exists());
    }
}

#[test]
fn missing_later_role_rejects_the_batch_before_a_child_worktree_or_job() {
    let _guard = crate::jobs::process_test_guard();
    for key in ["tasks", "chain", "workflow"] {
        for background in [false, true] {
            let (_dir, tool) = fixture(&record_model_script(&emit(&[ended("complete", "stop")])));
            explicit_model_catalog(&tool);
            let mut tool = tool.with_model_config(Some(explicit_role_config()));
            let owner = format!("missing-role-{}", uuid::Uuid::new_v4());
            tool.bind_job_session_scope(crate::jobs::JobSessionScope::fixed(&owner));
            let first = json!({"agent":"worker", "task":"must not run",
                "model":{"role":"smol"}, "isolation":"worktree"});
            let second = json!({"agent":"worker", "task":"unconfigured role",
                "model":{"role":"advisor"}});
            let mut input = if key == "workflow" {
                json!({"workflow":[{"id":"first", "call":first},
                    {"id":"second", "needs":["first"], "call":second}]})
            } else {
                let mut input = json!({});
                input[key] = json!([first, second]);
                input
            };
            input["async"] = json!(background);
            let error = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap()
                .block_on(tool.execute("missing-role", input, None))
                .unwrap_err()
                .to_string();
            assert!(error.contains("PI_SUBAGENT_MODEL"), "{error}");
            assert!(!tool.global_dir.join("models-used").exists());
            assert!(!tool.global_dir.join("tool-output-artifacts").exists());
            assert!(crate::jobs::list(&owner).unwrap().is_empty());
        }
    }
}

#[test]
fn seance_refuses_role_objects_before_reading_the_saved_session() {
    let (_dir, tool) = fixture(&record_model_script(&emit(&[ended(
        "must not run",
        "stop",
    )])));
    let error = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(tool.execute(
            "seance-role",
            json!({
                "agent":"seance", "sourceSession":"missing.jsonl", "task":"inspect history",
                "model":{"role":"smol"}
            }),
            None,
        ))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("seance model must be an exact-model string"),
        "{error}"
    );
    assert!(!tool.global_dir.join("models-used").exists());
}

#[test]
fn prepared_model_identity_survives_catalog_and_parent_changes() {
    let (_dir, tool) = fixture(&record_model_script(&emit(&[ended("complete", "stop")])));
    explicit_model_catalog(&tool);
    let scope = ToolModelScope::default();
    scope.set("first-parent", "first-model");
    let tool = tool
        .with_model_config(Some(Config::default()))
        .with_model_scope(scope.clone());
    let request: SubagentRequest = serde_json::from_value(json!({
        "chain":[{"agent":"worker", "task":"inspect", "model":"selection-fast"},
                 {"agent":"worker", "task":"review", "model":"selection-careful:high"}]
    }))
    .unwrap();
    let prepared = tool.prepare_request(&request).unwrap();
    scope.set("changed-parent", "changed-model");
    std::fs::write(tool.global_dir.join("models.json"), "{\"providers\":{}}").unwrap();
    let results = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(prepared.run(None));
    assert!(results.iter().all(|result| !result.is_error));
    assert_eq!(
        recorded_models(&tool),
        vec![
            "selection-local|selection-fast",
            "selection-local|selection-careful"
        ]
    );
}

#[test]
fn prepared_role_identity_and_policy_survive_settings_changes_retry_and_revival() {
    let body = format!(
        "if [ -f \"$PI_CODING_AGENT_DIR/retried\" ]; then\n{}\
         else\n: > \"$PI_CODING_AGENT_DIR/retried\"\n{}\
         fi\n",
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (_dir, tool) = fixture(&record_model_script(&body));
    explicit_model_catalog(&tool);
    let mut tool = tool.with_model_config(Some(explicit_role_config()));
    let request: SubagentRequest = serde_json::from_value(json!({
        "chain":[
            {"agent":"worker", "task":format!("frozen-role-{}", uuid::Uuid::new_v4()),
             "model":{"role":"slow"}, "outputSchema":{"type":"object", "required":["accepted"]},
             "schemaMode":"strict"},
            {"agent":"worker", "task":"follow-up", "model":{"role":"smol"}}
        ]
    }))
    .unwrap();
    let prepared = tool.prepare_request(&request).unwrap();
    tool.model_config = Some(Config::default());
    std::fs::write(tool.global_dir.join("models.json"), "{\"providers\":{}}").unwrap();
    let results = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap()
        .block_on(prepared.run(None));
    assert!(results.iter().all(|result| !result.is_error));
    assert_eq!(results[0].schema_retries, Some(1));
    assert_eq!(
        recorded_models(&tool),
        [
            "selection-local|selection-careful",
            "selection-local|selection-careful",
            "selection-local|selection-fast",
        ]
    );
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join("model-thinking")).unwrap(),
        "high\nhigh\n\n"
    );
    let policies = recorded_fallbacks(&tool);
    assert_eq!(policies[0], policies[1]);
    assert_eq!(policies[0]["role"], "slow");
    assert_eq!(policies[2]["role"], "smol");
    let revived = revive_through_hub(&tool.cwd, results[0].hub_id.as_deref().unwrap());
    assert!(!revived.is_error, "{revived:?}");
    assert_eq!(
        recorded_models(&tool)[3],
        "selection-local|selection-careful"
    );
    assert_eq!(recorded_fallbacks(&tool)[3], policies[0]);
}

#[test]
fn queued_children_schema_retries_and_revival_keep_their_original_cost_account() {
    for mode in ["tasks", "chain"] {
        let body = format!(
            "test -n \"$PI_SUBAGENT_COST_ADDRESS\" || exit 70\n\
             test -n \"$PI_SUBAGENT_COST_TOKEN\" || exit 71\n\
             test -z \"$PI_SUBAGENT_HUB_ADDRESS\" || exit 72\n\
             if [ -f \"$PI_CODING_AGENT_DIR/retried\" ]; then\n{}\
             else\n: > \"$PI_CODING_AGENT_DIR/retried\"\n{}\
             fi\n",
            emit(&[ended(r#"{"accepted":true}"#, "stop")]),
            emit(&[ended("not JSON", "stop")]),
        );
        let (_dir, tool) = fixture(&record_model_script(&body));
        std::fs::write(
            tool.global_dir.join("agents/worker.md"),
            "---\nname: worker\ndescription: cost-only fixture\ntools: read\n---\nFinish.",
        )
        .unwrap();
        let original = crate::cost::CostBudget::new(None, Some(1.0), 0.0).unwrap();
        let replacement = crate::cost::CostBudget::new(None, Some(1.0), 1.0).unwrap();
        let scope = crate::cost::CostScope::default();
        scope.set(Some(original.clone()));
        let tool = tool.with_cost_scope(scope.clone());
        let mut input = json!({"concurrency":1});
        input[mode] = json!([
            {"agent":"worker", "task":"cost captured before queue",
             "outputSchema":{"type":"object","required":["accepted"]}, "schemaMode":"strict"},
            {"agent":"worker", "task":"follow up on {previous}"}
        ]);
        let parsed: SubagentRequest = serde_json::from_value(input).unwrap();
        let prepared = tool.prepare_request(&parsed).unwrap();
        scope.set(Some(replacement));
        let results = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
            .block_on(prepared.run(None));
        assert!(
            results.iter().all(|result| !result.is_error),
            "{mode}: {results:?}"
        );
        assert_eq!(results[0].schema_retries, Some(1));
        assert_eq!(recorded_models(&tool).len(), 3);
        let source_id = results[0].hub_id.as_deref().unwrap();
        let source = crate::agent_hub::registry()
            .lock()
            .unwrap()
            .native_revival_source(source_id)
            .unwrap();
        assert!(
            source
                .launch
                .cost_budget
                .as_ref()
                .unwrap()
                .same_tree(&original)
        );

        // New work uses the current session, while revival keeps its source.
        let refused = run(&tool, request());
        assert!(refused.is_error);
        assert!(result(&refused)["pid"].is_null());
        assert_eq!(recorded_models(&tool).len(), 3);
        let revived = revive_through_hub(&tool.cwd, source_id);
        assert!(!revived.is_error, "{revived:?}");
        assert_eq!(recorded_models(&tool).len(), 4);

        // Spending the original allowance prevents another revival before
        // registration or spawn, even though the live scope was replaced.
        original.record(1.0).unwrap();
        let refused = revive_through_hub(&tool.cwd, source_id);
        assert!(refused.is_error);
        assert_eq!(recorded_models(&tool).len(), 4);
    }
}

#[test]
fn subagent_launch_model_precedence_keeps_resolved_parent_ids_exact() {
    let (_dir, tool) = fixture(&record_model_script(&emit(&[ended("complete", "stop")])));
    let scope = ToolModelScope::default();
    scope.set("host-router", "vendor/resolved-model:literal");
    let tool = tool.with_model_scope(scope);
    assert!(!run(&tool, request()).is_error);

    let tool = tool.with_role_model_spec(Some("role-provider/task-model:low".to_string()));
    assert!(!run(&tool, request()).is_error);
    std::fs::write(
        tool.global_dir.join("agents/worker.md"),
        "---\nname: worker\ndescription: pinned\nmodel: pinned-provider/pinned-model\n---\nFinish.\n",
    )
    .unwrap();
    assert!(!run(&tool, request()).is_error);
    assert_eq!(
        recorded_models(&tool),
        [
            "host-router|vendor/resolved-model:literal",
            "|role-provider/task-model:low",
            "|pinned-provider/pinned-model",
        ]
    );
}

#[test]
#[allow(clippy::significant_drop_tightening)] // Registry guard scope is deliberate.
fn request_model_snapshot_survives_queue_chain_retry_and_revival() {
    for mode in ["tasks", "chain"] {
        let body = format!(
            "if [ -f \"$PI_CODING_AGENT_DIR/retried\" ]; then\n{}\
             else\n\
               : > \"$PI_CODING_AGENT_DIR/retried\"\n{}\
             fi\n",
            emit(&[ended(r#"{"accepted":true}"#, "stop")]),
            emit(&[ended("not JSON", "stop")]),
        );
        let (_dir, tool) = fixture(&record_model_script(&body));
        let scope = ToolModelScope::default();
        scope.set("initial", "vendor/parent:literal");
        let selected = scope.current().unwrap();
        let tool = tool.with_model_scope(scope.clone());
        let mut input = json!({"concurrency":1});
        input[mode] = json!([
            {"agent":"worker","task":format!("captured-{}", uuid::Uuid::new_v4()),
             "outputSchema":{"type":"object","required":["accepted"]},"schemaMode":"strict"},
            {"agent":"worker","task":"follow up on {previous}"}
        ]);
        let changed = scope.clone();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let output = runtime
            .block_on(tool.execute(
                "capture-model-once",
                input,
                Some(Box::new(move |update| {
                    if update.details.as_ref().unwrap()["result"]["status"] == "starting" {
                        changed.set("changed", "vendor/next-model");
                    }
                })),
            ))
            .unwrap();
        assert!(!output.is_error, "{mode}: {output:?}");
        assert_eq!(result(&output)["schemaRetries"], 1);
        assert_eq!(result(&output)["schemaValid"], true);
        assert_eq!(
            recorded_models(&tool),
            vec!["initial|vendor/parent:literal".to_string(); 3],
            "queued work, chain steps and corrective retries must share the initial selection"
        );
        let source = source_entry(&output);
        assert!(!run(&tool, request()).is_error);
        assert_eq!(recorded_models(&tool)[3], "changed|vendor/next-model");

        scope.set("third", "another/model");
        let revived = revive_through_hub(&tool.cwd, &source.id);
        assert!(!revived.is_error, "{revived:?}");
        assert_eq!(recorded_models(&tool)[4], "initial|vendor/parent:literal");
        let hub = crate::agent_hub::registry().lock().unwrap();
        let replacement = hub.native_revival_source(revived_id(&revived)).unwrap();
        assert_eq!(replacement.launch.parent_model, Some(selected));
    }
}

#[test]
fn background_tan_keeps_parent_snapshot_captured_before_dispatch() {
    let (_dir, tool) = fixture(&record_model_script(&emit(&[ended("complete", "stop")])));
    let scope = ToolModelScope::default();
    scope.set("launch-provider", "vendor/background-model:literal");
    let tool = tool
        .with_model_scope(scope.clone())
        .with_parent_model(scope.current());
    scope.set("later-provider", "different-model");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let completion = runtime
        .block_on(tool.run_background_tan("background assignment"))
        .unwrap();
    assert!(!completion.is_error);
    let tool = tool.with_role_model_spec(Some("role-provider/background-role".to_string()));
    let completion = runtime
        .block_on(tool.run_background_tan("role assignment"))
        .unwrap();
    assert!(!completion.is_error);
    assert_eq!(
        recorded_models(&tool),
        [
            "launch-provider|vendor/background-model:literal",
            "|role-provider/background-role",
        ]
    );
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
    for isolated in [false, true] {
        let script = format!(
            "if [ -f phase ]; then\n exit 9\nelse\n : > phase\n\
             printf 'original effect\\n' > tracked.txt\n{}fi\n",
            emit(&[ended("not JSON", "stop")])
        );
        let (_dir, tool) = fixture(&script);
        let mut input = request();
        input["outputSchema"] = json!({"type":"object"});
        input["schemaMode"] = json!("permissive");
        if isolated {
            initialize_git(&tool.cwd);
            input["isolation"] = json!("worktree");
            input["isoApply"] = json!("apply");
        }
        let output = run(&tool, input);
        assert!(output.is_error, "retry failure must win: {output:?}");
        let result = result(&output);
        assert_eq!(result["status"], "failed");
        assert_eq!(
            result["exitCode"], 0,
            "original process metadata is retained"
        );
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("Child exited with code 9")
        );
        assert_eq!(result["output"], "not JSON");
        assert_eq!(result["schemaRetries"], 1);
        assert_eq!(result["schemaValid"], false);
        assert!(result.get("data").is_none());
        assert!(result.get("preservedWorktrees").is_none());
        if isolated {
            assert_eq!(result["iso"]["applyMode"], "keep");
            assert_eq!(result["iso"]["applied"], false);
            assert_eq!(
                std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
                "original\n"
            );
            let retained = Path::new(result["iso"]["worktreePath"].as_str().unwrap());
            assert_eq!(
                std::fs::read_to_string(retained.join("tracked.txt")).unwrap(),
                "original effect\n"
            );
        }
    }
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
fn isolated_serial_task_preserves_the_parents_package_directory() {
    let script = format!(
        "test \"$(cat tracked.txt)\" = 'package original' || exit 8\n\
         printf 'package changed\\n' > tracked.txt\n{}",
        emit(&[ended("package edit accepted", "stop")]),
    );
    let (_dir, tool) = fixture(&script);
    initialize_git(&tool.cwd);
    let package = tool.cwd.join("packages/api");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("tracked.txt"), "package original\n").unwrap();
    let nested = SubagentTool::with_paths(
        package.clone(),
        tool.global_dir.clone(),
        tool.child_binary.clone(),
    );
    let output = run(
        &nested,
        json!({"chain":[
            {"agent":"worker","task":"edit this package","isolation":"worktree"}
        ]}),
    );
    assert!(!output.is_error, "{output:?}");
    let value = result(&output);
    assert_eq!(value["iso"]["applied"], true);
    assert!(Path::new(value["cwd"].as_str().unwrap()).ends_with("packages/api"));
    assert_eq!(
        std::fs::read_to_string(package.join("tracked.txt")).unwrap(),
        "package changed\n"
    );
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "original\n",
        "a package-relative edit must not target the root file"
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Drive queueing, symlink retargeting and schema retry through real children.
fn queued_isolated_retry_keeps_the_captured_package_when_a_symlink_changes() {
    let script = format!(
        "correction=false\n\
         for argument in \"$@\"; do\n\
           if [ \"$argument\" = --no-tools ]; then correction=true; fi\n\
         done\n\
         assignment=$(cat)\n\
         case \"$assignment\" in\n\
         'Task: retarget source alias')\n\
           mv selected selected-before\n\
           ln -s packages/other selected\n{}\
           exit 0 ;;\n\
         esac\n\
         test \"$(cat tracked.txt)\" = 'api original' || exit 8\n\
         if \"$correction\"; then\n\
           test \"$(cat accepted.txt)\" = 'accepted in api' || exit 9\n{}\
         else\n\
           test ! -e accepted.txt || exit 10\n\
           printf 'executed\\n' >> \"$PI_CODING_AGENT_DIR/package-effects\"\n\
           printf 'accepted in api\\n' > accepted.txt\n{}\
         fi\n",
        emit(&[ended("alias changed", "stop")]),
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (_dir, tool) = fixture(&script);
    initialize_git(&tool.cwd);
    let package = tool.cwd.join("packages/api");
    let other = tool.cwd.join("packages/other");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(package.join("tracked.txt"), "api original\n").unwrap();
    std::fs::write(other.join("tracked.txt"), "other original\n").unwrap();
    std::os::unix::fs::symlink("packages/api", tool.cwd.join("selected")).unwrap();
    let output = run(
        &tool,
        json!({"concurrency":1,"tasks":[
            {"agent":"worker","task":"retarget source alias"},
            {"agent":"worker","task":"edit captured package","cwd":"selected",
             "isolation":"worktree","schemaMode":"strict",
             "outputSchema":{"type":"object","required":["accepted"]}}
        ]}),
    );
    assert!(!output.is_error, "{output:?}");
    let value = &output.details.as_ref().unwrap()["results"][1];
    assert_eq!(value["schemaRetries"], 1);
    assert_eq!(value["schemaValid"], true);
    assert_eq!(value["data"]["accepted"], true);
    assert_eq!(value["iso"]["applied"], true);
    assert!(Path::new(value["cwd"].as_str().unwrap()).ends_with("packages/api"));
    assert_eq!(
        std::fs::read_to_string(package.join("accepted.txt")).unwrap(),
        "accepted in api\n"
    );
    assert!(!other.join("accepted.txt").exists());
    assert_eq!(
        std::fs::read_to_string(other.join("tracked.txt")).unwrap(),
        "other original\n"
    );
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "original\n"
    );
    assert_eq!(
        std::fs::read_link(tool.cwd.join("selected")).unwrap(),
        Path::new("packages/other")
    );
    assert!(value.get("preservedWorktrees").is_none());
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join("package-effects")).unwrap(),
        "executed\n"
    );
}

#[test]
fn an_isolated_task_rejects_a_working_directory_absent_from_its_snapshot() {
    let script = format!(
        ": > \"$PI_CODING_AGENT_DIR/launched\"\n{}",
        emit(&[ended("must not launch at the root", "stop")]),
    );
    let (_dir, tool) = fixture(&script);
    initialize_git(&tool.cwd);
    std::fs::create_dir_all(tool.cwd.join("packages/empty")).unwrap();
    let output = run(
        &tool,
        json!({"chain":[
            {"agent":"worker","task":"empty package","cwd":"packages/empty",
             "isolation":"worktree"}
        ]}),
    );
    assert!(output.is_error, "{output:?}");
    let value = result(&output);
    assert!(value["error"].as_str().unwrap().contains("PI_ISO_CWD"));
    assert_eq!(value["iso"]["applied"], false);
    assert_eq!(value["iso"]["applyMode"], "keep");
    assert!(!tool.global_dir.join("launched").exists());
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "original\n"
    );
}

#[test]
#[allow(clippy::too_many_lines)] // Force completion order through the real concurrency queue.
fn parallel_worktree_writeback_follows_task_order_and_preserves_the_dirty_baseline() {
    let control = tempfile::tempdir().unwrap();
    let release = control.path().join("release-first");
    let script = format!(
        "assignment=$(cat)\n\
         test \"$(cat tracked.txt)\" = 'parent dirty' || exit 8\n\
         test \"$(cat untracked.txt)\" = 'carry me' || exit 9\n\
         case \"$assignment\" in\n\
         'Task: first')\n\
           while [ ! -f {} ]; do sleep 0.01; done\n\
           printf 'first wins\\n' > tracked.txt ;;\n\
         'Task: second') printf 'later patch\\n' > tracked.txt ;;\n\
         'Task: queued') printf 'queued saw original dirty state\\n' > queued.txt ;;\n\
         *) exit 10 ;;\n\
         esac\n{}",
        quote(release.to_str().unwrap()),
        emit(&[ended("accepted child output", "stop")]),
    );
    let (_dir, tool) = fixture(&script);
    let tool = tool.with_timeout(Duration::from_secs(30));
    initialize_git(&tool.cwd);
    std::fs::write(tool.cwd.join("tracked.txt"), "parent staged\n").unwrap();
    assert!(
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(&tool.cwd)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(tool.cwd.join("tracked.txt"), "parent dirty\n").unwrap();
    std::fs::write(tool.cwd.join("untracked.txt"), "carry me\n").unwrap();
    let parent_file = tool.cwd.join("tracked.txt");
    let completions = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&completions);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let output = runtime
        .block_on(tool.execute(
            "ordered-apply",
            json!({"concurrency":2,"tasks":[
                {"agent":"worker","task":"first","isolation":"worktree"},
                {"agent":"worker","task":"second","isolation":"worktree"},
                {"agent":"worker","task":"queued","isolation":"worktree"}
            ]}),
            Some(Box::new(move |update| {
                let value = &update.details.as_ref().unwrap()["result"];
                if value["task"] == "queued" && value["status"] == "starting" {
                    // The second task must have fully exited to free this
                    // launch slot; the first cannot exit until we release it.
                    assert_eq!(
                        std::fs::read_to_string(&parent_file).unwrap(),
                        "parent dirty\n",
                        "a faster sibling applied before the batch finished"
                    );
                    std::fs::write(&release, b"ready").unwrap();
                }
                if value["status"] == "completed" {
                    observed
                        .lock()
                        .unwrap()
                        .push(value["task"].as_str().unwrap().to_string());
                }
            })),
        ))
        .unwrap();
    let values = output.details.as_ref().unwrap()["results"]
        .as_array()
        .unwrap();
    assert!(
        output.is_error,
        "the later conflicting patch must be rejected"
    );
    assert_eq!(values[0]["task"], "first");
    assert_eq!(values[0]["iso"]["applied"], true);
    assert_eq!(values[1]["task"], "second");
    assert_eq!(values[1]["status"], "failed");
    assert_eq!(values[1]["iso"]["applied"], false);
    assert!(
        values[1]["error"]
            .as_str()
            .unwrap()
            .contains("PI_ISO_CONFLICT")
    );
    assert_eq!(values[2]["task"], "queued");
    assert_eq!(values[2]["iso"]["applied"], true);
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "first wins\n"
    );
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("queued.txt")).unwrap(),
        "queued saw original dirty state\n"
    );
    let staged = Command::new("git")
        .args(["show", ":tracked.txt"])
        .current_dir(&tool.cwd)
        .output()
        .unwrap();
    assert!(staged.status.success());
    assert_eq!(staged.stdout, b"parent staged\n");
    let kept = Path::new(values[1]["iso"]["worktreePath"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(kept.join("tracked.txt")).unwrap(),
        "later patch\n"
    );
    assert_eq!(
        *completions.lock().unwrap(),
        vec!["first".to_string(), "queued".to_string()]
    );
}

#[test]
fn queued_isolated_schema_retry_uses_the_snapshot_before_nonisolated_siblings() {
    let control = tempfile::tempdir().unwrap();
    let marker = quote(control.path().join("attempted").to_str().unwrap());
    let script = format!(
        "correction=false\n\
         for argument in \"$@\"; do\n\
           if [ \"$argument\" = --no-tools ]; then correction=true; fi\n\
         done\n\
         assignment=$(cat)\n\
         case \"$assignment\" in\n\
         'Task: shared writer')\n\
           printf 'shared parent edit\\n' > tracked.txt\n\
           printf 'sibling-only\\n' > sibling.txt\n{}\
           exit 0 ;;\n\
         esac\n\
         test \"$(cat tracked.txt)\" = original || exit 8\n\
         test ! -e sibling.txt || exit 9\n\
         if \"$correction\"; then\n\
           test \"$(cat accepted.txt)\" = 'original attempt' || exit 10\n{}\
         else\n\
           printf 'executed\\n' >> {marker}\n\
           printf 'original attempt\\n' > accepted.txt\n{}\
         fi\n",
        emit(&[ended("shared edit finished", "stop")]),
        emit(&[ended(r#"{"accepted":true}"#, "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (_dir, tool) = fixture(&script);
    initialize_git(&tool.cwd);
    let output = run(
        &tool,
        json!({"concurrency":1,"tasks":[
            {"agent":"worker","task":"shared writer"},
            {"agent":"worker","task":"isolated writer","isolation":"worktree",
             "outputSchema":{"type":"object","required":["accepted"]},"schemaMode":"strict"}
        ]}),
    );
    assert!(!output.is_error, "{output:?}");
    let value = &output.details.as_ref().unwrap()["results"][1];
    assert_eq!(value["schemaRetries"], 1);
    assert_eq!(value["schemaValid"], true);
    assert_eq!(value["data"]["accepted"], true);
    assert_eq!(value["iso"]["applied"], true);
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt")).unwrap(),
        "shared parent edit\n"
    );
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("accepted.txt")).unwrap(),
        "original attempt\n"
    );
    assert!(tool.cwd.join("sibling.txt").exists());
    assert!(value.get("preservedWorktrees").is_none());
    assert_eq!(
        std::fs::read_to_string(control.path().join("attempted")).unwrap(),
        "executed\n"
    );
}

struct PendingParallelFixture {
    _workspace: TempDir,
    _control: TempDir,
    tool: SubagentTool,
    request: Value,
    ready_task: String,
    waiting_task: String,
    release: PathBuf,
}

fn pending_parallel_fixture() -> PendingParallelFixture {
    let control = tempfile::tempdir().unwrap();
    let release = control.path().join("release");
    let ready_task = format!("ready-{}", uuid::Uuid::new_v4());
    let waiting_task = format!("waiting-{}", uuid::Uuid::new_v4());
    let script = format!(
        "assignment=$(cat)\n\
         if [ \"$assignment\" = {} ]; then\n\
           printf 'ready patch\\n' > tracked.txt\n\
         else\n\
           while [ ! -f {} ]; do sleep 0.01; done\n\
           printf 'waiting patch\\n' > tracked.txt\n\
         fi\n{}",
        quote(&format!("Task: {ready_task}")),
        quote(release.to_str().unwrap()),
        emit(&[ended("accepted output", "stop")]),
    );
    let (workspace, tool) = fixture(&script);
    initialize_git(&tool.cwd);
    let request = json!({"concurrency":1,"tasks":[
        {"agent":"worker","task":ready_task,"isolation":"worktree"},
        {"agent":"worker","task":waiting_task,"isolation":"worktree"}
    ]});
    PendingParallelFixture {
        _workspace: workspace,
        _control: control,
        tool: tool.with_timeout(Duration::from_secs(30)),
        request,
        ready_task,
        waiting_task,
        release,
    }
}

/// Notify only after a completed child has freed the sole execution slot and
/// its successor begins. The public kill operation must see history without
/// any remaining authority to signal the old PID or revive pending writeback.
// Registry guard scope is deliberate.
#[allow(
    clippy::significant_drop_tightening,
    clippy::significant_drop_in_scrutinee
)]
fn pending_writeback_callback(
    fixture: &PendingParallelFixture,
    sender: futures::channel::oneshot::Sender<String>,
) -> Box<dyn Fn(ToolUpdate) + Send + Sync> {
    let ready_task = fixture.ready_task.clone();
    let waiting_task = fixture.waiting_task.clone();
    let sender = Mutex::new(Some(sender));
    Box::new(move |update| {
        let value = &update.details.as_ref().unwrap()["result"];
        if value["task"] != waiting_task || value["status"] != "starting" {
            return;
        }
        let hub = crate::agent_hub::registry().lock().unwrap();
        let entry = hub
            .roster()
            .into_iter()
            .find(|entry| entry.task == ready_task)
            .unwrap();
        assert_eq!(entry.status, crate::agent_hub::ChildStatus::Running);
        assert!(entry.pid.is_some());
        assert_eq!(hub.control_pid(&entry.id), None);
        assert!(hub.native_revival_source(&entry.id).is_err());
        if let Some(sender) = sender.lock().unwrap().take() {
            let _ = sender.send(entry.id);
        }
    })
}

#[test]
#[allow(clippy::significant_drop_tightening)] // Registry guard scope is deliberate.
fn hub_kill_rejects_reaped_pending_writeback_without_cancelling_its_sibling() {
    let fixture = pending_parallel_fixture();
    let (sender, receiver) = futures::channel::oneshot::channel();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let (output, ready_id) = runtime.block_on(async {
        let run = Box::pin(fixture.tool.execute(
            "kill-pending-writeback",
            fixture.request.clone(),
            Some(pending_writeback_callback(&fixture, sender)),
        ));
        let futures::future::Either::Right((Ok(ready_id), pending)) =
            futures::future::select(run, receiver).await
        else {
            panic!("a completed child must wait for the second child")
        };
        let killed = crate::tools::HubTool::new(&fixture.tool.cwd)
            .execute(
                "kill-reaped-child",
                json!({"op":"agent","action":"kill","name":ready_id}),
                None,
            )
            .await
            .unwrap();
        assert!(!killed.is_error, "{killed:?}");
        std::fs::write(&fixture.release, b"continue").unwrap();
        (pending.await.unwrap(), ready_id)
    });
    assert!(output.is_error);
    let values = &output.details.as_ref().unwrap()["results"];
    assert_eq!(values[0]["status"], "cancelled");
    assert_eq!(values[0]["iso"]["applyMode"], "keep");
    assert_eq!(values[0]["iso"]["applied"], false);
    let kept = Path::new(values[0]["iso"]["worktreePath"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(kept.join("tracked.txt")).unwrap(),
        "ready patch\n"
    );
    assert_eq!(values[1]["status"], "completed");
    assert_eq!(values[1]["iso"]["applied"], true);
    assert_eq!(
        std::fs::read_to_string(fixture.tool.cwd.join("tracked.txt")).unwrap(),
        "waiting patch\n"
    );
    let hub = crate::agent_hub::registry().lock().unwrap();
    assert_eq!(
        hub.get(&ready_id).unwrap().status,
        crate::agent_hub::ChildStatus::Killed
    );
    assert_eq!(hub.control_pid(&ready_id), None);
}

#[test]
fn parent_cancellation_rejects_already_completed_parallel_writeback() {
    let fixture = pending_parallel_fixture();
    let owner = crate::agent_cx::AgentCx::for_request();
    let (sender, receiver) = futures::channel::oneshot::channel();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let output = runtime.block_on(owner.with_current(async {
        let run = Box::pin(fixture.tool.execute(
            "cancel-pending-writeback",
            fixture.request.clone(),
            Some(pending_writeback_callback(&fixture, sender)),
        ));
        let futures::future::Either::Right((Ok(_), pending)) =
            futures::future::select(run, receiver).await
        else {
            panic!("a completed child must wait for the second child")
        };
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel pending apply"),
        );
        pending.await.unwrap()
    }));
    assert!(output.is_error);
    let values = &output.details.as_ref().unwrap()["results"];
    for index in [0, 1] {
        assert_eq!(values[index]["status"], "cancelled");
        assert_eq!(values[index]["iso"]["applyMode"], "keep");
        assert_eq!(values[index]["iso"]["applied"], false);
    }
    assert_eq!(
        std::fs::read_to_string(fixture.tool.cwd.join("tracked.txt")).unwrap(),
        "original\n"
    );
}

#[test]
#[allow(clippy::significant_drop_tightening)] // Registry guard scope is deliberate.
fn dropping_parallel_execution_preserves_completed_work_and_cancels_its_lease() {
    let fixture = pending_parallel_fixture();
    let (sender, receiver) = futures::channel::oneshot::channel();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    let ready_id = runtime.block_on(async {
        let run = Box::pin(fixture.tool.execute(
            "drop-pending-writeback",
            fixture.request.clone(),
            Some(pending_writeback_callback(&fixture, sender)),
        ));
        match futures::future::select(run, receiver).await {
            futures::future::Either::Right((Ok(id), pending)) => {
                drop(pending);
                id
            }
            _ => panic!("a completed child must wait for the second child"),
        }
    });
    assert_eq!(
        std::fs::read_to_string(fixture.tool.cwd.join("tracked.txt")).unwrap(),
        "original\n"
    );
    let worktrees = crate::worktree_iso::list_mine(&fixture.tool.cwd).unwrap();
    assert!(worktrees.iter().any(|worktree| {
        std::fs::read_to_string(Path::new(&worktree.path).join("tracked.txt"))
            .is_ok_and(|text| text == "ready patch\n")
    }));
    let hub = crate::agent_hub::registry().lock().unwrap();
    assert_eq!(
        hub.get(&ready_id).unwrap().status,
        crate::agent_hub::ChildStatus::Cancelled
    );
    assert_eq!(hub.control_pid(&ready_id), None);
    let waiting = hub
        .roster()
        .into_iter()
        .find(|entry| entry.task == fixture.waiting_task)
        .unwrap();
    assert_eq!(waiting.status, crate::agent_hub::ChildStatus::Cancelled);
    assert_eq!(hub.control_pid(&waiting.id), None);
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
fn valid_schema_correction_applies_only_the_original_attempts_edits() {
    // Count external effects independently of the isolated workspace.
    let retry_state = tempfile::tempdir().unwrap();
    let marker = quote(retry_state.path().join("attempted").to_str().unwrap());
    let script = format!(
        "correction=false\n\
         for argument in \"$@\"; do\n\
           if [ \"$argument\" = --no-tools ]; then correction=true; fi\n\
         done\n\
         if \"$correction\"; then\n\
         test \"$(cat tracked.txt)\" = 'original execution' || exit 8\n\
         test \"$(cat accepted.txt)\" = first-only || exit 9\n\
         {}\
         else\n\
         printf 'executed\\n' >> {marker}\n\
         printf 'original execution\\n' > tracked.txt\n\
         printf 'first-only\\n' > accepted.txt\n\
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
        "original execution\n"
    );
    assert!(result.get("preservedWorktrees").is_none());
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("accepted.txt")).unwrap(),
        "first-only\n"
    );
    assert_eq!(
        std::fs::read_to_string(retry_state.path().join("attempted")).unwrap(),
        "executed\n"
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
        // `workspace::validate_request` admits workspace controls before
        // discovery or launch, so a typo is a tool error, not a failed task.
        let error = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap()
            .block_on(tool.execute(
                "protocol-test",
                json!({"tasks":[{
                    "agent":"worker","task":"must not launch","isolation":isolation,"isoApply":apply
                }]}),
                None,
            ))
            .expect_err("invalid workspace controls must be refused");
        assert!(
            error.to_string().contains("PI_SUBAGENT_INVALID_WORKSPACE"),
            "{error}"
        );
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
    let pid = value["pid"]
        .as_u64()
        .expect("fixture child must have launched"); // ubs:ignore: required real-process fixture observation
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
#[allow(clippy::significant_drop_tightening)] // Registry guard scope is deliberate.
fn hub_revival_executes_retained_policy_and_full_assignment_without_phantom_children()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let script = format!(
        "printf '%s\\n' \"$PI_SUBAGENT_RUN_ID\" >> \"$PI_CODING_AGENT_DIR/launches\"\n\
         printf '%s\\n' \"$@\" > \"$PI_CODING_AGENT_DIR/$PI_SUBAGENT_RUN_ID.args\"\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             --append-system-prompt|--system-prompt)\n\
               shift\n\
               cat \"$1\" > \"$PI_CODING_AGENT_DIR/$PI_SUBAGENT_RUN_ID.prompt\"\n\
               ;;\n\
           esac\n\
           shift\n\
         done\n\
         cat > \"$PI_CODING_AGENT_DIR/$PI_SUBAGENT_RUN_ID.task\"\n\
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
    let arguments =
        std::fs::read_to_string(tool.global_dir.join(format!("{replacement_id}.args")))?;
    let system_prompt =
        std::fs::read_to_string(tool.global_dir.join(format!("{replacement_id}.prompt")))?;
    assert!(arguments.contains("--model\nfixture/original\n"));
    assert!(arguments.contains("--tools\nread\n"));
    assert!(arguments.contains("--thinking\nlow\n"));
    assert!(system_prompt.contains("Original system policy."));
    assert!(!arguments.contains("Original system policy."));
    assert!(!arguments.contains("fixture/escalated"));
    assert!(!arguments.contains("Changed system policy."));
    assert!(!system_prompt.contains("Changed system policy."));
    assert!(!arguments.contains(&assignment));
    let instructions =
        std::fs::read_to_string(tool.global_dir.join(format!("{replacement_id}.task")))?;
    assert!(instructions.contains(&assignment));
    assert!(instructions.contains("first-history-marker"));
    assert!(instructions.contains("<pi-secret:"));
    assert!(!instructions.contains("sk-ant-api03-AAAA"));
    assert!(instructions.len() < assignment.len() + 20 * 1024);
    assert_eq!(
        instructions.matches("Continuation of a prior run").count(),
        1
    );
    let original_depth =
        std::fs::read_to_string(tool.global_dir.join(format!("{}.depth", source.id)))?;
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join(format!("{replacement_id}.depth")))?,
        original_depth
    );
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join(format!("{replacement_id}.cwd")))?.trim(),
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
    let instructions =
        std::fs::read_to_string(tool.global_dir.join(format!("{repeated_id}.task")))?;
    assert!(instructions.contains(&assignment));
    assert!(instructions.contains("second-history-marker"));
    assert!(!instructions.contains("first-history-marker"));
    assert_eq!(
        instructions.matches("Continuation of a prior run").count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join("launches"))?,
        format!("{}\n{replacement_id}\n{repeated_id}\n", source.id)
    );
    Ok(())
}

#[test]
#[allow(clippy::significant_drop_tightening)] // Registry guard scope is deliberate.
fn hub_revival_refuses_synthetic_registration_without_spending_a_child_entry() {
    let name = format!("synthetic-{}", uuid::Uuid::new_v4());
    let source = {
        let mut registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: required registry fixture
        let source = registry
            .register(&name, "not a native launch")
            .expect("synthetic entry"); // ubs:ignore: synthetic-registration regression fixture
        registry.settle(&source.id, crate::agent_hub::ChildStatus::Done);
        source
    };
    let output = revive_through_hub(Path::new("."), &source.id);
    assert!(output.is_error);
    assert!(
        serde_json::to_string(&output)
            .expect("tool JSON")
            .contains("PI_HUB_REVIVAL_UNAVAILABLE")
    ); // ubs:ignore: named refusal oracle
    let registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: no-phantom-child regression oracle
    assert_eq!(
        registry
            .roster()
            .iter()
            .filter(|entry| entry.name == name)
            .count(),
        1
    );
}

#[test]
#[allow(clippy::significant_drop_tightening)] // Registry guard scope is deliberate.
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
    assert!(
        !registry
            .roster()
            .iter()
            .any(|entry| { entry.revived_from.as_deref() == Some(source.id.as_str()) })
    );
    assert!(registry.native_revival_source(&source.id).is_ok());
    Ok(())
}

#[test]
#[allow(clippy::significant_drop_tightening)] // Registry guard scope is deliberate.
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
    assert_eq!(
        replacements[0].status,
        crate::agent_hub::ChildStatus::Failed
    );
    assert!(replacements[0].pid.is_none());
    assert!(registry.native_revival_source(replacement_id).is_ok());
    Ok(())
}

#[test]
// Registry guard scope is deliberate; the real-child lifecycle stays in one test.
#[allow(clippy::significant_drop_tightening, clippy::too_many_lines)]
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
                    && let Some(sender) = sender.lock().expect("progress sender").take()
                // ubs:ignore: required running-child observation
                {
                    let _ = sender.send(pid);
                }
            })),
        ));
        let futures::future::Either::Right((Ok(pid), pending)) =
            futures::future::select(future, receiver).await
        else {
            panic!("revival must launch before it can be cancelled"); // ubs:ignore: real-process progress oracle
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
    assert_eq!(
        recovered.entry.status,
        crate::agent_hub::ChildStatus::Killed
    );
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
    assert!(
        !registry
            .roster()
            .iter()
            .any(|entry| { entry.revived_from.as_deref() == Some(replacement_id.as_str()) })
    );
    Ok(())
}

#[test]
#[allow(clippy::significant_drop_tightening)] // Registry guard scope is deliberate.
fn hub_revival_keeps_schema_isolation_and_authored_task_across_corrective_retry()
-> std::result::Result<(), Box<dyn std::error::Error>> {
    let script = format!(
        "correction=false\n\
         for argument in \"$@\"; do\n\
           if [ \"$argument\" = --no-tools ]; then correction=true; fi\n\
         done\n\
         if [ ! -f \"$PI_CODING_AGENT_DIR/source-finished\" ]; then\n\
           : > \"$PI_CODING_AGENT_DIR/source-finished\"\n{}\
         elif \"$correction\"; then\n{}\
         else\n\
           printf 'executed\\n' >> \"$PI_CODING_AGENT_DIR/revival-effects\"\n\
           printf 'accepted\\n' > tracked.txt\n{}fi\n",
        emit(&[ended("{\"accepted\":true}", "stop")]),
        emit(&[ended("{\"accepted\":true}", "stop")]),
        emit(&[ended("not JSON", "stop")]),
    );
    let (_directory, tool) = fixture(&script);
    initialize_git(&tool.cwd);
    let assignment = format!(
        "retained-schema-{}: complete the authored change",
        uuid::Uuid::new_v4()
    );
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
    assert_eq!(
        std::fs::read_to_string(tool.cwd.join("tracked.txt"))?,
        "accepted\n"
    );
    assert!(result.get("preservedWorktrees").is_none());
    assert_eq!(
        std::fs::read_to_string(tool.global_dir.join("revival-effects"))?,
        "executed\n"
    );
    let retry_id = revived_id(&output);
    let retry_source = {
        let registry = crate::agent_hub::registry().lock().expect("hub registry"); // ubs:ignore: native replay-policy oracle
        let retry = registry.native_revival_source(retry_id)?;
        assert_eq!(retry.entry.revived_from.as_deref(), Some(source.id.as_str()));
        assert_eq!(
            registry.get(&source.id).expect("original source").status,
            crate::agent_hub::ChildStatus::Done
        );
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
