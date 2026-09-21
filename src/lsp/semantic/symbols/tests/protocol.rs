//! Exercise the public tool with real child stdio and observable request logs.
use super::*;
use crate::agent_cx::AgentCx;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new().enable_parking(false)
        .worker_threads(1).blocking_threads(1, 8).build().unwrap()
}

fn fixture(root: &Path, mode: &str) -> Option<LspTool> {
    let python = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|directory| directory.join("python3"))
            .find(|path| path.is_absolute() && path.is_file())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(), "Python required for workspace-symbol protocol tests");
        eprintln!("SKIP workspace-symbol protocol case: Python unavailable");
        return None;
    };
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".symbol-root"), "").unwrap();
    std::fs::write(root.join("anchor.pisymbol"), "original\n").unwrap();
    let script = root.join("symbols.py");
    std::fs::write(&script, include_str!("server.py")).unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([("symbol-fixture".to_string(), LspServerSettings {
                command: Some(python.display().to_string()),
                args: Some(vec!["-I".into(), "-u".into(), script.display().to_string(), mode.into()]),
                extensions: Some(vec![".pisymbol".into()]), languages: Some(vec!["plaintext".into()]),
                root_markers: Some(vec![".symbol-root".into()]), ..Default::default()
            })])), ..Default::default()
        }), ..Default::default()
    };
    Some(LspTool::new(&root, Some(&config)))
}

fn input() -> Value {
    json!({"action":"symbols","file":"anchor.pisymbol","query":"target","resolve":true,"timeout":5})
}

fn run(tool: &LspTool, rt: &asupersync::runtime::Runtime, input: Value) -> Result<Value> {
    let output = rt.block_on(tool.execute("workspace-symbols", input, None))?;
    assert!(!output.is_error);
    Ok(output.details.unwrap()["payload"].clone())
}

fn events(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("symbol-requests.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn calls(root: &Path, method: &str) -> usize {
    events(root).iter().filter(|event| event["method"] == method).count()
}

#[test]
fn workspace_search_resolves_only_missing_ranges_with_exact_opaque_identity() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let out = run(&tool, &runtime(), input()).unwrap();
    assert_eq!(out["count"], 3);
    assert_eq!(out["resolvedCount"], 2);
    assert_eq!(out["locationsComplete"], true);
    assert_eq!(out["responseComplete"], true);
    assert_eq!(out["symbols"][1]["location"]["range"], json!({"start":{"line":1,"character":2},"end":{"line":1,"character":8}}));
    assert_eq!(out["symbols"][2]["location"]["uri"], "https://example.invalid/unopened.pisymbol");
    assert!(!temp.path().join("unopened.pisymbol").exists());
    let log = events(temp.path());
    let init = log.iter().find(|event| event["method"] == "initialize").unwrap();
    assert_eq!(init["params"]["capabilities"]["workspace"]["symbol"]["resolveSupport"]["properties"], json!(["location.range"]));
    let resolve: Vec<_> = log.iter().filter(|event| event["method"] == "workspaceSymbol/resolve").collect();
    assert_eq!(resolve.len(), 2);
    assert_eq!(resolve[0]["params"]["data"], json!({"ticket":1,"literal":"$(not executed)","unicode":"界"}));
    assert!(resolve[0]["params"]["location"].get("range").is_none());
    assert_eq!(calls(temp.path(), "workspace/symbol"), 1);
    assert_eq!(calls(temp.path(), "textDocument/documentSymbol"), 0);
    assert_eq!(calls(temp.path(), "workspace/executeCommand"), 0);
}

#[test]
fn legacy_anchor_and_document_outline_remain_distinct() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "legacy") else { return };
    let rt = runtime();
    let out = run(&tool, &rt, json!({"action":"symbols","symbol":"anchor.pisymbol","query":""})).unwrap();
    assert_eq!(out["count"], 3);
    assert_eq!(out["resolveSupported"], false);
    assert_eq!(out["locationsComplete"], true);
    let outline = run(&tool, &rt, json!({"action":"symbols","file":"anchor.pisymbol"})).unwrap();
    assert_eq!(outline["symbols"][0]["name"], "document_only");
    assert_eq!(calls(temp.path(), "workspaceSymbol/resolve"), 0);
    assert_eq!(calls(temp.path(), "textDocument/documentSymbol"), 1);
}

#[test]
fn unresolved_locations_are_reported_instead_of_fabricated() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let mut request = input();
    request["resolve"] = json!(false);
    let out = run(&tool, &runtime(), request).unwrap();
    assert_eq!(out["unresolvedIndices"], json!([2,3]));
    assert_eq!(out["locationsComplete"], false);
    assert_eq!(out["responseComplete"], true);
    assert_eq!(calls(temp.path(), "workspaceSymbol/resolve"), 0);
}

#[test]
fn count_and_byte_limits_prevent_unnecessary_resolution() {
    for (limit, expected) in [(1,0),(2,1)] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), "normal") else { return };
        let mut request = input();
        request["limit"] = json!(limit);
        let out = run(&tool, &runtime(), request).unwrap();
        assert_eq!(out["count"], limit);
        assert_eq!(out["total"], 3);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["responseComplete"], false);
        assert_eq!(calls(temp.path(), "workspaceSymbol/resolve"), expected);
    }
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "byte-output") else { return };
    let out = run(&tool, &runtime(), input()).unwrap();
    assert_eq!(out["count"], 3);
    assert_eq!(out["total"], 4);
    assert_eq!(out["truncated"], true);
    assert!(out.to_string().len() < MAX_PAYLOAD_BYTES);
}

#[test]
fn empty_and_null_reports_are_valid_empty_responses() {
    for mode in ["empty","null"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let out = run(&tool, &runtime(), input()).unwrap();
        assert_eq!(out["count"], 0);
        assert_eq!(out["responseComplete"], true);
        assert_eq!(calls(temp.path(), "workspaceSymbol/resolve"), 0);
    }
}

#[test]
fn provider_errors_and_invalid_omitted_items_never_become_empty_success() {
    for (mode, code) in [("search-error","symbol provider failed"), ("resolve-error","symbol provider failed"),
        ("disabled","LSP_UNSUPPORTED"), ("legacy","LSP_UNSUPPORTED"),
        ("invalid-last","LSP_SYMBOL_MALFORMED"), ("oversized-item","LSP_SEMANTIC_LIMIT"),
        ("too-many","LSP_SEMANTIC_LIMIT"), ("encoding","LSP_SYNC_UNSUPPORTED")] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let mut request = input();
        if mode == "invalid-last" { request["limit"] = json!(1); }
        let error = run(&tool, &runtime(), request).unwrap_err();
        assert!(error.to_string().contains(code), "{mode}: {error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("anchor.pisymbol")).unwrap(), "original\n");
    }
}

#[test]
fn resolver_cannot_substitute_name_uri_data_or_omit_the_range() {
    for mode in ["changed","changed-uri","dropped-data","unresolved"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let error = run(&tool, &runtime(), input()).unwrap_err();
        let code = if mode == "unresolved" { "LSP_SYMBOL_MALFORMED" } else { "LSP_SYMBOL_CHANGED" };
        assert!(error.to_string().contains(code), "{mode}: {error}");
        assert_eq!(calls(temp.path(), "workspaceSymbol/resolve"), 1);
    }
}

#[test]
fn changed_anchor_is_rejected_after_either_protocol_phase() {
    for mode in ["drift-search","drift-resolve"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let error = run(&tool, &runtime(), input()).unwrap_err();
        assert!(error.to_string().contains("LSP_SEMANTIC_STALE"), "{error}");
        assert_eq!(std::fs::read_to_string(temp.path().join("anchor.pisymbol")).unwrap(), "external\n");
    }
}

#[test]
fn search_never_authorizes_unsolicited_workspace_edits() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "probe") else { return };
    run(&tool, &runtime(), input()).unwrap();
    let log = events(temp.path());
    let response = log.iter().find(|event| event["id"] == "unapproved-symbol-edit").unwrap();
    assert_eq!(response["result"]["applied"], false);
    assert_eq!(std::fs::read_to_string(temp.path().join("anchor.pisymbol")).unwrap(), "original\n");
}

#[test]
fn invalid_workspace_selectors_are_rejected_before_spawning() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let rt = runtime();
    for extra in [json!({"symbol":"other.pisymbol"}),json!({"apply":true}),json!({"limit":0}),
        json!({"query":"x".repeat(1025)}),json!({"line":1}),json!({"newName":"bad"})] {
        let mut request = input();
        request.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(run(&tool, &rt, request).unwrap_err().to_string().contains("LSP_USAGE"));
    }
    assert!(tool.registry.status().is_empty());
    assert!(!temp.path().join("symbol-requests.jsonl").exists());
}

#[test]
fn startup_search_and_resolve_share_a_bounded_request_lifetime() {
    for mode in ["hang-initialize","stall-search","stall-resolve","shared-budget"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else { return };
        let mut request = input();
        request["timeout"] = json!(1);
        let started = Instant::now();
        let error = run(&tool, &runtime(), request).unwrap_err();
        assert!(error.to_string().contains("LSP_TIMEOUT"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}

#[test]
fn dropped_resolver_cancels_the_request_and_releases_the_tool_lane() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "stall-resolve") else { return };
    let rt = runtime();
    rt.block_on(async {
        let mut future = Box::pin(tool.execute("drop", input(), None));
        let owner = AgentCx::for_current_or_request();
        let started = Instant::now();
        while !temp.path().join("workspace-resolve-started").exists() {
            assert!(futures::poll!(future.as_mut()).is_pending());
            assert!(started.elapsed() < Duration::from_secs(5));
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        drop(future);
        let status = tool.execute("after-drop", json!({"action":"status"}), None).await.unwrap();
        assert!(!status.is_error);
        while calls(temp.path(), "$/cancelRequest") == 0 {
            assert!(started.elapsed() < Duration::from_secs(5));
            owner.time().sleep(Duration::from_millis(10)).await;
        }
    });
}

#[test]
fn identical_close_reopen_during_resolution_cannot_refresh_old_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "stall-resolve") else { return };
    runtime().block_on(async {
        let mut future = Box::pin(tool.execute("resync", input(), None));
        let owner = AgentCx::for_current_or_request();
        let started = Instant::now();
        while !temp.path().join("workspace-resolve-started").exists() {
            assert!(futures::poll!(future.as_mut()).is_pending());
            assert!(started.elapsed() < Duration::from_secs(5));
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        let root = temp.path().canonicalize().unwrap();
        let entry = tool.registry.entry_for_root("symbol-fixture", &root).unwrap();
        let path = root.join("anchor.pisymbol");
        let uri = crate::lsp::client::try_path_to_uri(&path).unwrap();
        entry.client.invalidate(&uri);
        entry.client.ensure_synced(&path, "plaintext").unwrap();
        entry.client.call_no_wait_notify("test/release", json!({})).unwrap();
        let error = future.await.unwrap_err();
        assert!(error.to_string().contains("LSP_SEMANTIC_STALE"), "{error}");
    });
}

#[test]
fn restricted_owner_cannot_open_a_workspace_anchor() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
    let budget = {
        let _guard = restricted.set_current_restricted();
        Budget::new(Duration::from_secs(5))
    };
    let error = runtime().block_on(Source::open_file(&tool, "anchor.pisymbol", &budget, |_| Ok(()))).err().unwrap();
    assert!(error.to_string().contains("LSP_IO_PERMISSION"));
    assert!(tool.registry.status().is_empty());
    assert!(!temp.path().join("symbol-requests.jsonl").exists());
}
