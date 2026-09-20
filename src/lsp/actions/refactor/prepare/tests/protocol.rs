//! The real LspTool against a framed child peer; no semantic-server claim.

use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::collections::HashMap;

const SOURCE: &str = "// 😀\nlet alpha = alpha;\n";
const RENAMED: &str = "// 😀\nlet alpha = omega;\n";

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
        std::env::split_paths(&paths)
            .map(|directory| directory.join("python3"))
            .find(|path| path.is_absolute() && path.is_file())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
            "Python is required for rename preparation protocol coverage");
        eprintln!("SKIP rename preparation protocol test: Python unavailable");
        return None;
    };
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".target-root"), "").unwrap();
    std::fs::write(root.join("source.target"), SOURCE).unwrap();
    std::fs::write(root.join("sibling.target"), "alpha\n").unwrap();
    let script = root.join("target_peer.py");
    std::fs::write(&script, include_str!("server.py")).unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([("target-peer".into(), LspServerSettings {
                command: Some(python.display().to_string()),
                args: Some(vec!["-I".into(), "-u".into(), script.display().to_string(), mode.into()]),
                extensions: Some(vec![".target".into()]),
                languages: Some(vec!["plaintext".into()]),
                root_markers: Some(vec![".target-root".into()]),
                ..Default::default()
            })])),
            ..Default::default()
        }),
        ..Default::default()
    };
    Some(LspTool::new(&root, Some(&config)))
}

fn input(action: &str) -> Value {
    let mut raw = json!({"action":action,"file":"source.target",
        "position":{"line":1,"character":14},"timeout":5});
    if action == "rename" { raw["newName"] = json!("omega"); }
    raw
}

fn run(tool: &LspTool, rt: &asupersync::runtime::Runtime, raw: Value) -> Result<ToolOutput> {
    rt.block_on(tool.execute("rename-target-case", raw, None))
}

fn frames(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("requests.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn count(root: &Path, method: &str) -> usize {
    frames(root).iter().filter(|frame| frame["method"] == method).count()
}

fn unchanged(root: &Path) {
    assert_eq!(std::fs::read_to_string(root.join("source.target")).unwrap(), SOURCE);
    assert_eq!(std::fs::read_to_string(root.join("sibling.target")).unwrap(), "alpha\n");
}

#[test]
fn inspection_negotiates_concrete_preparation_without_rename_or_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let rt = runtime();
    for cursor in [14, 17] {
        let mut raw = input("prepare_rename");
        raw["position"]["character"] = json!(cursor);
        let details = run(&tool, &rt, raw).unwrap().details.unwrap();
        assert_eq!(details["canRename"], true);
        assert_eq!(details["target"]["sourceText"], "alpha");
        assert_eq!(details["target"]["placeholder"], "binding");
        assert_eq!(details["target"]["range"]["start"], json!({"line":1,"character":12}));
        assert_eq!(details["position"]["character"], cursor);
        assert_eq!(details["applied"], false);
        assert!(details.get("refactorId").is_none());
        unchanged(temp.path());
    }
    let log = frames(temp.path());
    let caps = &log.iter().find(|frame| frame["method"] == "initialize").unwrap()
        ["params"]["capabilities"]["textDocument"]["rename"];
    assert_eq!(caps["prepareSupport"], true);
    assert!(caps.get("prepareSupportDefaultBehavior").is_none());
    assert_eq!(count(temp.path(), "textDocument/prepareRename"), 2);
    assert_eq!(count(temp.path(), "textDocument/rename"), 0);
}

#[test]
fn bare_preparation_uses_the_exact_source_spelling_as_placeholder() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "bare-range") else { return };
    let details = run(&tool, &runtime(), input("prepare_rename")).unwrap().details.unwrap();
    assert_eq!(details["target"]["placeholder"], "alpha");
    unchanged(temp.path());
}

#[test]
fn server_refusal_is_observed_once_and_blocks_actual_rename_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "refuse") else { return };
    let rt = runtime();
    let details = run(&tool, &rt, input("prepare_rename")).unwrap().details.unwrap();
    assert_eq!(details["canRename"], false);
    assert!(details["target"].is_null());
    let error = run(&tool, &rt, input("rename")).unwrap_err();
    assert!(error.to_string().contains("LSP_RENAME_UNAVAILABLE"), "{error}");
    assert_eq!(count(temp.path(), "textDocument/prepareRename"), 2);
    assert_eq!(count(temp.path(), "textDocument/rename"), 0);
    unchanged(temp.path());
}

#[test]
fn failed_malformed_and_oversized_preparation_never_fall_through_to_rename() {
    for (mode, code) in [
        ("error", "cannot rename this binding"), ("default", "LSP_RENAME_MALFORMED"),
        ("split", "LSP_RENAME_MALFORMED"), ("unrelated", "LSP_RENAME_MALFORMED"),
        ("empty", "LSP_RENAME_MALFORMED"), ("large-target", "LSP_RENAME_LIMIT"),
        ("oversized", "LSP_EDIT_LIMIT"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let error = run(&tool, &runtime(), input("rename")).unwrap_err();
        assert!(error.to_string().contains(code), "{mode}: {error}");
        assert_eq!(count(temp.path(), "textDocument/rename"), 0);
        unchanged(temp.path());
    }
}

#[test]
fn exact_cursor_is_preserved_and_review_approval_never_prepares_or_renames_again() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let rt = runtime();
    let mut raw = input("rename");
    raw["apply"] = json!(false);
    let preview = run(&tool, &rt, raw).unwrap().details.unwrap();
    assert_eq!(preview["prepareRequested"], true);
    assert_eq!(preview["target"]["sourceText"], "alpha");
    unchanged(temp.path());
    let mut selection = json!({"action":"rename","refactorId":preview["refactorId"]});
    let inspected = run(&tool, &rt, selection.clone()).unwrap().details.unwrap();
    assert_eq!(inspected["workspaceEdit"], preview["workspaceEdit"]);
    selection["apply"] = json!(true);
    assert_eq!(run(&tool, &rt, selection.clone()).unwrap().details.unwrap()["applied"], true);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.target")).unwrap(), RENAMED);
    assert_eq!(std::fs::read_to_string(temp.path().join("sibling.target")).unwrap(), "omega\n");
    assert!(run(&tool, &rt, selection).is_err());
    let log = frames(temp.path());
    for method in ["textDocument/prepareRename", "textDocument/rename"] {
        let requests: Vec<_> = log.iter().filter(|frame| frame["method"] == method).collect();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["params"]["position"], json!({"line":1,"character":14}));
    }
    assert_eq!(count(temp.path(), "workspace/executeCommand"), 0);
}

#[test]
fn providers_without_preparation_keep_immediate_rename_but_not_false_inspection_success() {
    for mode in ["plain", "absent"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let rt = runtime();
        let error = run(&tool, &rt, input("prepare_rename")).unwrap_err();
        assert!(error.to_string().contains("LSP_UNSUPPORTED"));
        unchanged(temp.path());
        let details = run(&tool, &rt, input("rename")).unwrap().details.unwrap();
        assert_eq!(details["applied"], true);
        assert_eq!(details["prepareRequested"], false);
        assert!(details["target"].is_null());
        assert_eq!(std::fs::read_to_string(temp.path().join("source.target")).unwrap(), RENAMED);
        assert_eq!(count(temp.path(), "textDocument/prepareRename"), 0);
        assert_eq!(count(temp.path(), "textDocument/rename"), 1);
    }
}

#[test]
fn malformed_disabled_and_incompatible_capabilities_stop_before_preparation() {
    for mode in ["disabled", "bad-capability", "utf8"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        assert!(run(&tool, &runtime(), input("rename")).is_err());
        assert_eq!(count(temp.path(), "textDocument/prepareRename"), 0);
        assert_eq!(count(temp.path(), "textDocument/rename"), 0);
        unchanged(temp.path());
    }
}

#[test]
fn invalid_cursor_and_conflicting_selectors_do_not_start_a_language_server() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let rt = runtime();
    for extra in [json!({"position":{"line":0,"character":4}}),
        json!({"position":{"line":99,"character":0}}), json!({"symbol":"alpha"}),
        json!({"line":2}), json!({"query":"1"}), json!({"newName":"omega"}),
        json!({"apply":false})]
    {
        let mut raw = input("prepare_rename");
        raw.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let error = run(&tool, &rt, raw).unwrap_err();
        assert!(error.to_string().contains("LSP_USAGE"), "{error}");
    }
    assert!(tool.registry.status().is_empty());
    assert!(!temp.path().join("requests.jsonl").exists());
}

#[test]
fn same_text_close_reopen_cannot_reuse_request_time_preparation_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let rt = runtime();
    let args: LspInput = serde_json::from_value(input("rename")).unwrap();
    let request = rt.block_on(tool.rename_request(&args)).unwrap();
    request.entry.client.invalidate(&request.uri);
    request.entry.client.ensure_synced(&request.snapshot.source, "plaintext").unwrap();
    let error = rt.block_on(request.prepare()).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
    assert_eq!(count(temp.path(), "textDocument/prepareRename"), 0);
    unchanged(temp.path());
}

#[test]
fn drift_in_either_server_phase_prevents_all_client_writes() {
    for mode in ["prepare-drift", "rename-drift"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let error = run(&tool, &runtime(), input("rename")).unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("source.target")).unwrap(), "external source\n");
        assert_eq!(std::fs::read_to_string(temp.path().join("sibling.target")).unwrap(), "alpha\n");
        assert_eq!(count(temp.path(), "textDocument/rename"), usize::from(mode == "rename-drift"));
    }
}

#[test]
fn prepare_never_authorizes_unsolicited_server_edits() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "probe") else { return };
    run(&tool, &runtime(), input("prepare_rename")).unwrap();
    let reply: Value = serde_json::from_str(&std::fs::read_to_string(temp.path().join("probe.json")).unwrap()).unwrap();
    assert_eq!(reply["result"]["applied"], false);
    unchanged(temp.path());
    assert_eq!(count(temp.path(), "workspace/executeCommand"), 0);
}

#[test]
fn restricted_callers_fail_before_startup_and_cancellation_blocks_preparation() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let rt = runtime();
    rt.block_on(async {
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let error = tool.execute("restricted", input("prepare_rename"), None).await.unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_PERMISSION"), "{error}");
    });
    assert!(tool.registry.status().is_empty());
    assert!(!temp.path().join("requests.jsonl").exists());
    let args: LspInput = serde_json::from_value(input("rename")).unwrap();
    let mut request = rt.block_on(tool.rename_request(&args)).unwrap();
    request.budget.owner = AgentCx::for_request();
    request.budget.owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel before preparation"));
    let error = rt.block_on(request.prepare()).unwrap_err();
    assert!(error.to_string().contains("LSP_CANCELLED"));
    assert_eq!(count(temp.path(), "textDocument/prepareRename"), 0);
    unchanged(temp.path());
}

#[test]
fn dropping_a_pending_preparation_cancels_it_and_releases_the_tool_lane() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "prepare-stall") else { return };
    let rt = runtime();
    rt.block_on(async {
        let mut pending = Box::pin(tool.execute("drop-prepare", input("rename"), None));
        let owner = AgentCx::for_current_or_request();
        for _ in 0..400 {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if temp.path().join("started").exists() { break; }
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        assert!(temp.path().join("started").exists());
        drop(pending);
    });
    run(&tool, &rt, json!({"action":"request","file":"source.target","method":"test/barrier","timeout":5})).unwrap();
    assert_eq!(count(temp.path(), "textDocument/rename"), 0);
    let log = frames(temp.path());
    let id = &log.iter().find(|frame| frame["method"] == "textDocument/prepareRename").unwrap()["id"];
    assert!(log.iter().any(|frame| frame["method"] == "$/cancelRequest" && frame["params"]["id"] == *id));
    unchanged(temp.path());
}

#[test]
fn server_initialization_uses_the_same_bounded_request_lifetime() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "init-stall") else { return };
    let mut raw = input("rename");
    raw["timeout"] = json!(1);
    let error = run(&tool, &runtime(), raw).unwrap_err();
    assert!(error.to_string().contains("LSP_TIMEOUT"), "{error}");
    assert_eq!(count(temp.path(), "textDocument/prepareRename"), 0);
    assert_eq!(count(temp.path(), "textDocument/rename"), 0);
    unchanged(temp.path());
}

#[test]
fn legacy_symbol_occurrence_selection_still_uses_the_bounded_source_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let rt = runtime();
    let ambiguous = json!({"action":"prepare_rename","file":"source.target","symbol":"alpha"});
    let error = run(&tool, &rt, ambiguous).unwrap_err();
    assert!(error.to_string().contains("LSP_SYMBOL_AMBIGUOUS"));
    assert!(tool.registry.status().is_empty());
    let details = run(&tool, &rt, json!({"action":"prepare_rename","file":"source.target","symbol":"alpha#2","line":2})).unwrap().details.unwrap();
    assert_eq!(details["position"], json!({"line":1,"character":12}));
    assert_eq!(details["target"]["sourceText"], "alpha");
    unchanged(temp.path());
}
