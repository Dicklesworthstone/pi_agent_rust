// Included inside actions::tests so cases can observe permission lease cleanup.
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}

fn fixture(root: &Path, mode: &str) -> Option<LspTool> {
    let python = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|directory| directory.join("python3"))
            .find(|path| path.is_absolute() && path.is_file())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(), "Python is required for LSP protocol coverage");
        eprintln!("skip: no Python; LSP code-action protocol case was not executed");
        return None;
    };
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(root.join(".pi-lsp-root"), "").unwrap();
    std::fs::write(root.join("source.lspfixture"), "old\n").unwrap();
    std::fs::write(root.join("sibling.lspfixture"), "old\n").unwrap();
    let server = root.join("server.py");
    std::fs::write(&server, include_str!("test_server.py")).unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([("code-action-fixture".to_string(), LspServerSettings {
                command: Some(python.display().to_string()),
                args: Some(vec!["-I".into(), "-u".into(), server.display().to_string(), mode.into()]),
                extensions: Some(vec![".lspfixture".into()]),
                languages: Some(vec!["plaintext".into()]),
                root_markers: Some(vec![".pi-lsp-root".into()]),
                ..Default::default()
            })])),
            ..Default::default()
        }),
        ..Default::default()
    };
    // Canonical, as `hierarchy::tests` and `actions::tests` already build it:
    // the tool canonicalizes each resolved file path (actions.rs:606) but not
    // its own cwd, so a root that still reads `/var/...` while the file reads
    // `/private/var/...` makes `display_path` fail to strip the prefix and
    // report absolute paths. A real cwd comes from `getcwd`, which is already
    // resolved; only a handed-in TempDir path is not.
    Some(LspTool::new(&root.canonicalize().unwrap(), Some(&config)))
}

fn run(tool: &LspTool, runtime: &asupersync::runtime::Runtime, args: Value) -> Result<ToolOutput> {
    runtime.block_on(tool.execute("action-case", args, None))
}

fn methods(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("requests.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["method"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn lazy_action_is_resolved_then_edited_then_commanded_without_relisting() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    let list = run(&tool, &runtime, json!({"action":"code_actions","file":"source.lspfixture"})).unwrap();
    assert!(!methods(temp.path()).iter().any(|method| method == "codeAction/resolve"));
    let id = list.details.unwrap()["actions"][0]["actionId"].clone();
    let output = run(&tool, &runtime, json!({"action":"code_actions","apply":true,"actionId":id})).unwrap();
    assert!(!output.is_error);
    assert_eq!(output.details.unwrap()["applied"], true);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), "fixed\n");
    let calls = methods(temp.path());
    assert_eq!(calls.iter().filter(|method| *method == "textDocument/codeAction").count(), 1);
    assert_eq!(calls.iter().filter(|method| *method == "workspace/executeCommand").count(), 1);
}

#[test]
fn disabled_changed_and_malformed_actions_never_modify_files_or_execute_commands() {
    for mode in ["disabled", "changed", "malformed"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return; };
        assert!(run(&tool, &runtime(), json!({"action":"code_actions","file":"source.lspfixture","apply":true,"query":"1"})).is_err());
        assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), "old\n");
        assert!(!temp.path().join("command-started").exists());
    }
}

#[test]
fn command_failure_reports_partial_edits_and_is_not_retried_as_content_modified() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "error") else { return; };
    let output = run(&tool, &runtime(), json!({"action":"code_actions","file":"source.lspfixture","apply":true,"query":"1"})).unwrap();
    assert!(output.is_error);
    let details = output.details.unwrap();
    assert_eq!(details["partial"], true);
    assert_eq!(details["applied"], false);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.lspfixture")).unwrap(), "fixed\n");
    assert_eq!(methods(temp.path()).iter().filter(|method| *method == "workspace/executeCommand").count(), 1);
}

#[test]
fn selected_command_can_apply_workspace_edits_and_reports_every_changed_file() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "callback") else { return; };
    let output = run(&tool, &runtime(), json!({"action":"code_actions","file":"source.lspfixture","apply":true,"query":"1"})).unwrap();
    assert!(!output.is_error);
    assert_eq!(output.details.unwrap()["serverEditRequests"], 1);
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "fixed\n");
    assert!(lock(&tool.actions.active).is_none());
}

#[test]
fn unsolicited_server_edits_are_denied_on_read_requests() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return; };
    let output = run(&tool, &runtime(), json!({"action":"request","file":"source.lspfixture","method":"test/unsolicited"})).unwrap();
    assert_eq!(output.details.unwrap()["payload"]["result"]["applied"], false);
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
}

#[test]
fn out_of_workspace_server_edit_fails_the_action_even_when_the_command_reports_success() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::write(temp.path().join("outside.lspfixture"), "old\n").unwrap();
    let Some(tool) = fixture(&root, "outside") else { return; };
    let output = run(&tool, &runtime(), json!({"action":"code_actions","file":"source.lspfixture","apply":true,"query":"1"})).unwrap();
    assert!(output.is_error);
    assert_eq!(output.details.unwrap()["partial"], true);
    assert_eq!(std::fs::read_to_string(temp.path().join("outside.lspfixture")).unwrap(), "old\n");
}

#[test]
fn source_drift_expires_a_cached_selection_before_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    let list = run(&tool, &runtime, json!({"action":"code_actions","file":"source.lspfixture"})).unwrap();
    let id = list.details.unwrap()["actions"][0]["actionId"].clone();
    std::fs::write(temp.path().join("source.lspfixture"), "new content\n").unwrap();
    assert!(run(&tool, &runtime, json!({"action":"code_actions","apply":true,"actionId":id})).is_err());
    assert!(!methods(temp.path()).iter().any(|method| method == "codeAction/resolve"));
}

#[test]
fn dropping_a_command_future_revokes_permission_for_late_server_edits() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "stall") else { return; };
    let runtime = runtime();
    runtime.block_on(async {
        let mut request = Box::pin(tool.execute("cancelled", json!({"action":"code_actions","file":"source.lspfixture","apply":true,"query":"1"}), None));
        let owner = AgentCx::for_current_or_request();
        for _ in 0..1000 {
            assert!(futures::poll!(request.as_mut()).is_pending());
            if temp.path().join("command-started").exists() { break; }
            owner.time().sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(temp.path().join("command-started").exists());
        drop(request);
        assert!(lock(&tool.actions.active).is_none());
    });
    let output = run(&tool, &runtime, json!({"action":"request","file":"source.lspfixture","method":"test/lateEdit"})).unwrap();
    assert_eq!(output.details.unwrap()["payload"]["result"]["applied"], false);
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.lspfixture")).unwrap(), "old\n");
}

#[test]
fn resynchronized_documents_do_not_reuse_old_versions() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    run(&tool, &runtime, json!({"action":"code_actions","file":"source.lspfixture"})).unwrap();
    std::fs::write(temp.path().join("source.lspfixture"), "new\n").unwrap();
    run(&tool, &runtime, json!({"action":"code_actions","file":"source.lspfixture"})).unwrap();
    let output = run(&tool, &runtime, json!({"action":"request","file":"source.lspfixture","method":"test/versions"})).unwrap();
    let details = output.details.unwrap();
    let versions = details["payload"]["result"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    assert!(versions[1].as_u64().unwrap() > versions[0].as_u64().unwrap());
}
