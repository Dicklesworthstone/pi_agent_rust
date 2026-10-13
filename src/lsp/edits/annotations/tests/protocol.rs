//! Real tool dispatch, stdio framing and files; no substitute apply engine.

use crate::config::{Config, LspServerSettings, LspSettings};
use crate::error::Result;
use crate::lsp::LspTool;
use crate::tools::{Tool as _, ToolOutput};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

type Runtime = asupersync::runtime::Runtime;

fn fixture(root: &Path, mode: &str) -> Option<(LspTool, Runtime)> {
    let python = ["python3", "python"].into_iter().find(|program| {
        Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
            .status().is_ok_and(|status| status.success())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(), "Python required for annotation protocol tests");
        eprintln!("SKIP annotation protocol fixture: Python unavailable");
        return None;
    };
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".annotation-root"), "").unwrap();
    std::fs::write(root.join("source.anno"), "old\n").unwrap();
    std::fs::write(root.join("other.anno"), "old\n").unwrap();
    let peer = root.join("peer.py");
    std::fs::write(&peer, include_str!("server.py")).unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([("annotations".into(), LspServerSettings {
                command: Some(python.into()),
                args: Some(vec!["-I".into(), "-u".into(), peer.display().to_string(), mode.into()]),
                extensions: Some(vec![".anno".into()]), languages: Some(vec!["plaintext".into()]),
                root_markers: Some(vec![".annotation-root".into()]),
                ..Default::default()
            })])),
            ..Default::default()
        }),
        ..Default::default()
    };
    Some((LspTool::new(&root, Some(&config)),
        asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap()))
}

fn run(tool: &LspTool, rt: &Runtime, input: Value) -> Result<ToolOutput> {
    rt.block_on(tool.execute("annotations", input, None))
}

fn details(output: ToolOutput) -> Value {
    assert!(!output.is_error, "{output:?}");
    output.details.unwrap()
}

fn rename(apply: bool) -> Value {
    json!({"action":"rename","file":"source.anno","symbol":"old","newName":"new","apply":apply,"timeout":10})
}

fn action(apply: bool) -> Value {
    json!({"action":"code_actions","file":"source.anno","query":"1","apply":apply,"timeout":10})
}

fn approval(action: &str, preview: &Value) -> Value {
    json!({"action":action,"refactorId":preview["refactorId"],"apply":true,"timeout":10})
}

fn records(root: &Path, file: &str) -> Vec<Value> {
    std::fs::read_to_string(root.join(file)).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn count(root: &Path, method: &str) -> usize {
    records(root, "requests.jsonl").iter().filter(|frame| frame["method"] == method).count()
}

fn source(root: &Path) -> String {
    std::fs::read_to_string(root.join("source.anno")).unwrap()
}

#[test]
fn negotiated_rename_annotations_are_reviewed_then_confirmed_exactly_once() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "normal") else { return };
    let preview = details(run(&tool, &rt, rename(false)).unwrap());
    assert_eq!(preview["confirmationRequired"], true);
    assert_eq!(preview["requiredConfirmationIds"], json!(["public-api"]));
    assert_eq!(preview["changeAnnotations"]["public-api"]["label"], "Change public API");
    assert_eq!(source(temp.path()), "old\n");
    let frames = records(temp.path(), "requests.jsonl");
    let caps = &frames.iter().find(|frame| frame["method"] == "initialize").unwrap()["params"]["capabilities"];
    assert_eq!(caps["workspace"]["workspaceEdit"]["changeAnnotationSupport"], json!({"groupsOnLabel":false}));
    assert_eq!(caps["textDocument"]["rename"]["honorsChangeAnnotations"], true);
    assert_eq!(caps["textDocument"]["codeAction"]["honorsChangeAnnotations"], true);
    let inspected = details(run(&tool, &rt, json!({"action":"rename","refactorId":preview["refactorId"]})).unwrap());
    assert_eq!(inspected["changeAnnotations"], preview["changeAnnotations"]);
    let approved = details(run(&tool, &rt, approval("rename", &preview)).unwrap());
    assert_eq!(approved["confirmedAnnotations"], json!(["public-api"]));
    assert_eq!(approved["confirmationRequired"], false);
    assert_eq!(source(temp.path()), "new\n");
    assert_eq!(count(temp.path(), "textDocument/rename"), 1);
    assert!(run(&tool, &rt, approval("rename", &preview)).is_err());
    let next = json!({"action":"rename","file":"source.anno","symbol":"new","newName":"next","apply":true});
    assert!(run(&tool, &rt, next).unwrap_err().to_string().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
}

#[test]
fn fresh_immediate_rename_and_code_action_do_not_imply_confirmation() {
    for input in [rename(true), action(true)] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, rt)) = fixture(temp.path(), "normal") else { return };
        let error = run(&tool, &rt, input).unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFIRMATION_REQUIRED"), "{error}");
        assert_eq!(source(temp.path()), "old\n");
        assert_eq!(count(temp.path(), "workspace/executeCommand"), 0);
    }
}

#[test]
fn lazy_code_action_retains_annotation_data_and_multiple_file_images() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "lazy") else { return };
    let preview = details(run(&tool, &rt, action(false)).unwrap());
    assert_eq!(preview["requiredConfirmationIds"], json!(["public-api"]));
    assert_eq!(preview["workspaceEdit"]["documentChanges"].as_array().unwrap().len(), 2);
    let approved = details(run(&tool, &rt, approval("code_actions", &preview)).unwrap());
    assert_eq!(approved["confirmedAnnotations"], json!(["public-api"]));
    assert_eq!(source(temp.path()), "new\n");
    assert_eq!(std::fs::read_to_string(temp.path().join("other.anno")).unwrap(), "new\n");
    assert_eq!(count(temp.path(), "textDocument/codeAction"), 1);
    assert_eq!(count(temp.path(), "codeAction/resolve"), 1);
}

#[test]
fn file_move_waits_for_annotation_approval_before_imports_move_and_notification() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "normal") else { return };
    let mut input = json!({"action":"rename_file","file":"source.anno","newFile":"moved.anno","apply":true,"timeout":10});
    assert!(run(&tool, &rt, input.clone()).unwrap_err().to_string().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
    assert!(!temp.path().join("moved.anno").exists());
    assert!(!temp.path().join("notified.json").exists());
    input["apply"] = json!(false);
    let preview = details(run(&tool, &rt, input).unwrap());
    assert_eq!(source(temp.path()), "old\n");
    assert_eq!(preview["confirmationRequired"], true);
    details(run(&tool, &rt, approval("rename_file", &preview)).unwrap());
    // A protocol barrier ensures the post-commit notification was processed.
    run(&tool, &rt, json!({"action":"request","file":"other.anno","method":"test/barrier"})).unwrap();
    let notification: Value = serde_json::from_str(&std::fs::read_to_string(temp.path().join("notified.json")).unwrap()).unwrap();
    assert_eq!(notification, json!({"afterMove":true,"other":"new\n"}));
    assert_eq!(count(temp.path(), "workspace/willRenameFiles"), 2);
    assert_eq!(count(temp.path(), "workspace/didRenameFiles"), 1);
}

#[test]
fn stale_unopened_sibling_prevents_approved_annotated_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "lazy") else { return };
    let preview = details(run(&tool, &rt, action(false)).unwrap());
    std::fs::write(temp.path().join("other.anno"), "external\n").unwrap();
    assert!(run(&tool, &rt, approval("code_actions", &preview)).is_err());
    assert_eq!(source(temp.path()), "old\n");
    assert_eq!(std::fs::read_to_string(temp.path().join("other.anno")).unwrap(), "external\n");
    assert_eq!(count(temp.path(), "codeAction/resolve"), 1);
}

#[test]
fn same_text_reopen_cannot_reuse_annotation_approval() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "normal") else { return };
    let preview = details(run(&tool, &rt, rename(false)).unwrap());
    let path = temp.path().canonicalize().unwrap().join("source.anno");
    let (uri, entry) = rt.block_on(tool.synced(&path)).unwrap();
    entry.client.invalidate(&uri);
    entry.client.ensure_synced(&path, "plaintext").unwrap();
    assert!(run(&tool, &rt, approval("rename", &preview)).unwrap_err().to_string().contains("LSP_REFACTOR_STALE"));
    assert_eq!(source(temp.path()), "old\n");
}

#[test]
fn restricted_owner_cannot_confirm_but_does_not_destroy_another_owners_plan() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "normal") else { return };
    let preview = details(run(&tool, &rt, rename(false)).unwrap());
    rt.block_on(async {
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let error = tool.execute("restricted", approval("rename", &preview), None).await.unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_PERMISSION"), "{error}");
    });
    assert_eq!(source(temp.path()), "old\n");
    details(run(&tool, &rt, approval("rename", &preview)).unwrap());
    assert_eq!(source(temp.path()), "new\n");
}

#[test]
fn malformed_and_oversized_annotation_previews_never_authorize_a_write() {
    for (mode, code) in [("dangling", "LSP_EDIT_MALFORMED"), ("malformed", "LSP_EDIT_MALFORMED"), ("large", "LSP_EDIT_LIMIT")] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, rt)) = fixture(temp.path(), mode) else { return };
        assert!(run(&tool, &rt, rename(false)).unwrap_err().to_string().contains(code));
        assert_eq!(source(temp.path()), "old\n");
    }
}

#[test]
fn a_selected_command_cannot_confirm_its_own_callback() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "command") else { return };
    let output = run(&tool, &rt, action(true)).unwrap();
    assert!(output.is_error);
    let reply = records(temp.path(), "callbacks.jsonl");
    assert_eq!(reply.len(), 1);
    assert_eq!(reply[0]["result"]["applied"], false);
    assert!(reply[0]["result"]["failureReason"].as_str().unwrap().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
    assert_eq!(source(temp.path()), "old\n");
}

#[test]
fn rejected_annotated_callback_revokes_window_without_undoing_earlier_batch() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "partial") else { return };
    let output = run(&tool, &rt, action(true)).unwrap();
    assert!(output.is_error);
    let report = output.details.unwrap();
    assert_eq!(report["applied"], false);
    assert_eq!(report["partial"], true);
    let replies = records(temp.path(), "callbacks.jsonl");
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[0]["result"]["applied"], true);
    assert_eq!(replies[1]["result"]["applied"], false);
    assert!(replies[1]["result"]["failureReason"].as_str().unwrap().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
    assert_eq!(replies[2]["result"]["applied"], false);
    assert_eq!(source(temp.path()), "new\n", "only the already acknowledged batch persists");
}

#[test]
fn informational_annotations_keep_ordinary_immediate_rename_working() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, rt)) = fixture(temp.path(), "info") else { return };
    assert_eq!(details(run(&tool, &rt, rename(true)).unwrap())["applied"], true);
    assert_eq!(source(temp.path()), "new\n");
}
