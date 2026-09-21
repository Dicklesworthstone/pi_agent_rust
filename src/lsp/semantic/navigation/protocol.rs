//! Public tool, real child stdio and real files; no native validation claim.
use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

const TEXT: &str = "😀alpha alpha\n";
const SERVER: &str = include_str!("server.py");

fn fixture(root: &Path, mode: &str) -> Option<LspTool> {
    let python = ["python3", "python"].into_iter().find(|program| {
        Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
            .status().is_ok_and(|status| status.success())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(), "Python required for navigation protocol tests");
        eprintln!("SKIP navigation protocol fixture: Python unavailable");
        return None;
    };
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".nav-root"), "").unwrap();
    std::fs::write(root.join("source.navfixture"), TEXT).unwrap();
    std::fs::write(root.join("other.navfixture"), "keep\n").unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([("nav-peer".into(), LspServerSettings {
                command: Some(python.into()),
                args: Some(vec!["-I".into(), "-u".into(), "-c".into(), SERVER.into(), mode.into()]),
                extensions: Some(vec![".navfixture".into()]), languages: Some(vec!["plaintext".into()]),
                root_markers: Some(vec![".nav-root".into()]), ..Default::default()
            })])), ..Default::default()
        }), ..Default::default()
    };
    Some(LspTool::new(&root, Some(&config)))
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap()
}

fn input(action: &str) -> Value {
    json!({"action":action,"file":"source.navfixture","position":{"line":0,"character":2},"timeout":10})
}

fn run(tool: &LspTool, rt: &asupersync::runtime::Runtime, args: Value) -> Result<Value> {
    let out = rt.block_on(tool.execute("navigation", args, None))?;
    assert!(!out.is_error);
    Ok(out.details.unwrap())
}

fn calls(root: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(root.join("requests.jsonl")).unwrap_or_default();
    let end = text.rfind('\n').map_or(0, |at| at + 1);
    text[..end].lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn unchanged(root: &Path) {
    assert_eq!(std::fs::read_to_string(root.join("source.navfixture")).unwrap(), TEXT);
    assert_eq!(std::fs::read_to_string(root.join("other.navfixture")).unwrap(), "keep\n");
    assert!(!root.join("not-created.navfixture").exists());
}

#[test]
fn public_definition_type_and_implementation_negotiate_links_at_the_exact_cursor() {
    for (action, method, capability) in [
        ("definition", "textDocument/definition", "definition"),
        ("type_definition", "textDocument/typeDefinition", "typeDefinition"),
        ("implementation", "textDocument/implementation", "implementation")]
    {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "normal") else { return };
        let out = run(&tool, &runtime(), input(action)).unwrap();
        assert_eq!(out["locations"][0]["range"], super::tests::span(3,8));
        assert_eq!(out["locations"][0]["targetRange"], super::tests::span(0,20));
        assert_eq!(out["locations"][0]["originSelectionRange"], super::tests::span(2,7));
        assert_eq!(out["locations"][0]["character"], 4);
        let log = calls(temp.path());
        let init = log.iter().find(|frame| frame["method"] == "initialize").unwrap();
        assert_eq!(init["params"]["capabilities"]["textDocument"][capability]["linkSupport"], true);
        let call = log.iter().find(|frame| frame["method"] == method).unwrap();
        assert_eq!(call["params"]["position"], json!({"line":0,"character":2}));
        unchanged(temp.path());
    }
}

#[test]
fn public_references_preserves_duplicates_and_declaration_context() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let mut args = input("references"); args["limit"] = json!(1);
    let out = run(&tool, &runtime(), args).unwrap();
    assert_eq!(out["total"], 2); assert_eq!(out["count"], 1);
    assert_eq!(out["truncated"], true); assert_eq!(out["responseComplete"], false);
    let log = calls(temp.path());
    let call = log.iter().find(|frame| frame["method"] == "textDocument/references").unwrap();
    assert_eq!(call["params"]["context"]["includeDeclaration"], true);
    unchanged(temp.path());
}

#[test]
fn legacy_substring_selection_still_resolves_against_the_same_source_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "plain") else { return };
    let out = run(&tool, &runtime(), json!({"action":"definition","file":"source.navfixture","symbol":"alpha#2","timeout":10})).unwrap();
    assert_eq!(out["position"], json!({"line":0,"character":8}));
    assert_eq!(out["count"], 1);
    unchanged(temp.path());
}

#[test]
fn public_hover_keeps_original_content_shapes() {
    for mode in ["normal", "marked", "empty", "null"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let out = run(&tool, &runtime(), input("hover")).unwrap();
        assert_eq!(out["returnedNull"], mode == "null");
        if mode == "marked" { assert_eq!(out["contents"][0]["language"], "rust"); }
        if mode == "normal" { assert_eq!(out["contents"]["kind"], "markdown"); }
        unchanged(temp.path());
    }
}

#[test]
fn public_empty_navigation_reports_are_explicit_and_complete() {
    for mode in ["empty", "null"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let out = run(&tool, &runtime(), input("definition")).unwrap();
        assert_eq!(out["count"], 0); assert_eq!(out["responseComplete"], true);
        unchanged(temp.path());
    }
}

#[test]
fn public_invalid_reports_and_provider_failures_never_become_empty_success() {
    for (mode, action) in [("bad-tail","definition"),("bad-selection","definition"),
        ("many","references"),("bad-hover","hover"),("error","hover")]
    {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let mut args = input(action); args["limit"] = json!(1);
        let error = run(&tool, &runtime(), args).unwrap_err();
        assert!(error.to_string().contains("LSP_"), "{mode}: {error}");
        unchanged(temp.path());
    }
}

#[test]
fn public_byte_limit_retains_only_complete_location_records() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "output-limit") else { return };
    let mut args = input("references"); args["limit"] = json!(1000);
    let out = run(&tool, &runtime(), args).unwrap();
    assert_eq!(out["total"], 40); assert_eq!(out["truncated"], true);
    assert!(out["count"].as_u64().unwrap() > 0);
    assert!(serde_json::to_vec(&out).unwrap().len() <= MAX_PAYLOAD_BYTES);
    for item in out["locations"].as_array().unwrap() {
        assert_eq!(item["uri"].as_str().unwrap().len(), 8010);
        assert_eq!(item["range"], super::tests::span(0,1));
    }
    unchanged(temp.path());
}

#[test]
fn invalid_local_cursors_and_conflicting_selectors_do_not_spawn_a_server() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let rt = runtime();
    for extra in [json!({"position":{"line":0,"character":1}}),json!({"position":{"line":99,"character":0}}),
        json!({"symbol":"alpha"}),json!({"line":1}),json!({"apply":true}),json!({"range":super::tests::span(2,7)})]
    {
        let mut args = input("definition");
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(run(&tool, &rt, args).unwrap_err().to_string().contains("LSP_USAGE"));
    }
    assert!(tool.registry.status().is_empty());
    assert!(calls(temp.path()).is_empty());
}

#[test]
fn public_source_drift_rejects_navigation_and_hover_reports() {
    for action in ["definition", "references", "hover"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "drift") else { return };
        let error = run(&tool, &runtime(), input(action)).unwrap_err();
        assert!(error.to_string().contains("LSP_SEMANTIC_STALE"), "{error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("source.navfixture")).unwrap(), "external source\n");
        assert_eq!(std::fs::read_to_string(temp.path().join("other.navfixture")).unwrap(), "keep\n");
    }
}

#[test]
fn navigation_does_not_authorize_unsolicited_workspace_edits() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "probe") else { return };
    run(&tool, &runtime(), input("definition")).unwrap();
    let probe: Value = serde_json::from_str(&std::fs::read_to_string(temp.path().join("probe.json")).unwrap()).unwrap();
    assert_eq!(probe["result"]["applied"], false);
    assert!(!calls(temp.path()).iter().any(|frame| frame["method"] == "workspace/executeCommand"));
    unchanged(temp.path());
}

#[test]
fn explicit_disabled_and_encoding_mismatch_stop_before_navigation_dispatch() {
    for mode in ["disabled", "encoding"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        assert!(run(&tool, &runtime(), input("definition")).is_err());
        assert!(!calls(temp.path()).iter().any(|frame| frame["method"] == "textDocument/definition"));
        unchanged(temp.path());
    }
}

#[test]
fn restricted_owner_and_startup_timeout_do_not_return_navigation_success() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hang-init") else { return };
    let rt = runtime();
    rt.block_on(async {
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let error = tool.execute("restricted", input("definition"), None).await.unwrap_err();
        assert!(error.to_string().contains("LSP_IO_PERMISSION"), "{error}");
    });
    assert!(calls(temp.path()).is_empty());
    let mut args = input("definition"); args["timeout"] = json!(1);
    assert!(run(&tool, &rt, args).unwrap_err().to_string().contains("LSP_TIMEOUT"));
    unchanged(temp.path());
}

#[test]
fn same_text_reopen_while_waiting_rejects_the_original_response() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hold") else { return };
    runtime().block_on(async {
        let mut pending = Box::pin(tool.execute("reopen", input("definition"), None));
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        let mut posted = false;
        for _ in 0..500 {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if calls(temp.path()).iter().any(|frame| frame["method"] == "textDocument/definition") {
                posted = true; break;
            }
            owner.time().sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(posted);
        let status = tool.registry.status().into_iter().next().unwrap();
        let entry = tool.registry.entry_for_root(&status.name, &status.root).unwrap();
        let path = temp.path().canonicalize().unwrap().join("source.navfixture");
        let uri = crate::lsp::client::try_path_to_uri(&path).unwrap();
        entry.client.invalidate(&uri);
        entry.client.ensure_synced(&path, "plaintext").unwrap();
        entry.client.call_no_wait_notify("test/release", Value::Null).unwrap();
        let error = pending.await.unwrap_err();
        assert!(error.to_string().contains("LSP_SEMANTIC_STALE"), "{error}");
    });
    unchanged(temp.path());
}

#[test]
fn dropping_navigation_cancels_the_pending_id_and_releases_the_workflow_lane() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "hang") else { return };
    runtime().block_on(async {
        let mut pending = Box::pin(tool.execute("drop", input("definition"), None));
        let owner = crate::agent_cx::AgentCx::for_current_or_request();
        let mut posted = false;
        for _ in 0..500 {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if calls(temp.path()).iter().any(|frame| frame["method"] == "textDocument/definition") {
                posted = true; break;
            }
            owner.time().sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(posted); drop(pending);
        tool.execute("barrier", json!({"action":"request","file":"source.navfixture","method":"test/barrier","timeout":5}), None).await.unwrap();
    });
    let log = calls(temp.path());
    let query = log.iter().position(|frame| frame["method"] == "textDocument/definition").unwrap();
    let cancel = log.iter().position(|frame| frame["method"] == "$/cancelRequest").unwrap();
    let barrier = log.iter().position(|frame| frame["method"] == "test/barrier").unwrap();
    assert!(query < cancel && cancel < barrier);
    assert_eq!(log[cancel]["params"]["id"], log[query]["id"]);
    unchanged(temp.path());
}
