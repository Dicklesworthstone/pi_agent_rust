//! Public tool requests over real child stdio; no Rust execution claim is
//! implied by testing the Python peer alone.

use super::*;
use crate::config::Config;
use crate::lsp::text::Position;
use crate::tools::Tool as _;
use serde_json::json;
use std::process::{Command, Stdio};

const TEXT: &str = "// 😀\r\ncall(value, flag)\n";

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new().enable_parking(false)
        .worker_threads(1).blocking_threads(1,8).build().unwrap()
}

fn fixture(root: &Path, mode: &str) -> Option<LspTool> {
    let python = ["python3", "python"].into_iter().find(|program| {
        Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
            .status().is_ok_and(|status| status.success())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(), "semantic protocol tests require Python");
        eprintln!("SKIP semantic protocol fixture: Python unavailable");
        return None;
    };
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join("source.pisig"), TEXT).unwrap();
    std::fs::write(root.join(".semantic-root"), "").unwrap();
    let script = root.join("semantic_peer.py");
    std::fs::write(&script, include_str!("server.py")).unwrap();
    let config: Config = serde_json::from_value(json!({"lsp":{"servers":{"semantic-fixture":{
        "command":python,"args":["-I","-u",script.display().to_string(),mode],
        "languages":["plaintext"],"extensions":[".pisig"],"rootMarkers":[".semantic-root"]
    }}}})).unwrap();
    Some(LspTool::new(&root, Some(&config)))
}

fn request() -> Value {
    json!({"action":"signature_help","file":"source.pisig","position":{"line":1,"character":12},"timeout":5})
}

fn events(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("semantic-requests.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[test]
fn signatures_and_active_parameter_reach_the_agent_without_changing_source() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else { return };
    let output = runtime().block_on(tool.execute("signature",request(),None)).unwrap();
    assert!(!output.is_error);
    let payload = output.details.unwrap();
    assert_eq!(payload["action"],"signature_help");
    assert_eq!(payload["count"],2); assert_eq!(payload["activeParameter"],1);
    assert_eq!(payload["signatures"][0]["parameters"][0]["label"],"😀: Text");
    assert_eq!(std::fs::read_to_string(temp.path().join("source.pisig")).unwrap(),TEXT);
    let log = events(temp.path());
    let init = log.iter().find(|event| event["method"]=="initialize").unwrap();
    assert_eq!(init.pointer("/params/capabilities/textDocument/signatureHelp/signatureInformation/parameterInformation/labelOffsetSupport"),Some(&json!(true)));
    let call = log.iter().find(|event| event["method"]=="textDocument/signatureHelp").unwrap();
    assert_eq!(call["params"]["position"],request()["position"]);
    assert_eq!(call["params"]["context"],json!({"triggerKind":1,"isRetrigger":false}));
    assert!(!log.iter().any(|event|event["method"]=="workspace/executeCommand"));
}

#[test]
fn null_and_empty_are_successful_absence_not_provider_errors() {
    for mode in ["null","empty"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(),mode) else { return };
        let payload = runtime().block_on(tool.execute("empty",request(),None)).unwrap().details.unwrap();
        assert_eq!(payload["count"],0); assert!(payload["activeSignature"].is_null());
    }
}

#[test]
fn provider_errors_malformed_reports_and_limits_never_become_empty_success() {
    for (mode, code) in [("error","signature engine failed"),("malformed","LSP_SEMANTIC_MALFORMED"),("oversized","LSP_SEMANTIC_LIMIT"),("unsupported","LSP_UNSUPPORTED"),("encoding","LSP_UNSUPPORTED")] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(),mode) else { return };
        let error = runtime().block_on(tool.execute("error",request(),None)).unwrap_err();
        assert!(error.to_string().contains(code),"{mode}: {error}");
    }
}

#[test]
fn external_source_edits_invalidate_the_semantic_result() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(),"drift") else { return };
    let error = runtime().block_on(tool.execute("drift",request(),None)).unwrap_err();
    assert!(error.to_string().contains("LSP_SEMANTIC_STALE"),"{error}");
    assert_eq!(std::fs::read_to_string(temp.path().join("source.pisig")).unwrap(),"external edit\n");
}

#[test]
fn signature_requests_never_authorize_unsolicited_edits() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(),"unsolicited") else { return };
    runtime().block_on(tool.execute("unexpected",request(),None)).unwrap();
    assert_eq!(std::fs::read_to_string(temp.path().join("source.pisig")).unwrap(),TEXT);
    let log = events(temp.path());
    let reply = log.iter().find(|event| event["id"]=="unexpected-edit" && event.get("method").is_none()).unwrap();
    assert_eq!(reply["result"]["applied"],false);
}

#[test]
fn invalid_positions_and_write_selectors_fail_before_server_startup() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(),"normal") else { return };
    let rt = runtime();
    for (key,value) in [("apply",json!(true)),("range",json!({"start":{"line":0,"character":0},"end":{"line":1,"character":0}})),("symbol",json!("call")),("completionId",json!("other")),("limit",json!(0)),("position",json!({"line":0,"character":4})),("position",json!({"line":9,"character":0}))] {
        let mut input = request(); input[key] = value;
        assert!(rt.block_on(tool.execute("invalid",input,None)).is_err(),"{key}");
    }
    assert!(tool.registry.status().is_empty());
    assert!(!temp.path().join("semantic-requests.jsonl").exists());
}

#[test]
fn one_deadline_bounds_initialization_and_signature_waits() {
    for mode in ["hang-initialize","hang"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(),mode) else { return };
        let mut input = request(); input["timeout"] = json!(1);
        let start = Instant::now();
        let error = runtime().block_on(tool.execute("timeout",input,None)).unwrap_err();
        assert!(error.to_string().contains("LSP_TIMEOUT"),"{error}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }
}

#[test]
fn synchronized_incarnation_is_checked_even_when_disk_bytes_are_identical() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(),"normal") else { return };
    let input: LspInput = serde_json::from_value(request()).unwrap();
    let budget = Budget::new(Duration::from_secs(5));
    let source = runtime().block_on(Source::open(&tool,&input,&budget, |_| Ok(()))).unwrap();
    source.entry.client.invalidate(&source.uri);
    source.entry.client.ensure_synced(&source.path,"plaintext").unwrap();
    assert!(source.verify(&budget).unwrap_err().to_string().contains("LSP_SEMANTIC_STALE"));
}

#[test]
fn semantic_budget_preserves_restricted_owner_authority() {
    let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
    let budget = {
        let _guard = restricted.set_current_restricted();
        Budget::new(Duration::from_secs(5))
    };
    assert!(budget.remaining().unwrap_err().to_string().contains("LSP_IO_PERMISSION"));
}

#[test]
fn source_and_output_byte_limits_are_enforced_before_unbounded_work() {
    let temp = tempfile::tempdir().unwrap(); let source = temp.path().join("big");
    let file = std::fs::File::create(&source).unwrap(); file.set_len((MAX_SOURCE_BYTES+1) as u64).unwrap();
    assert!(read_source(&source).unwrap_err().to_string().contains("LSP_SEMANTIC_LIMIT"));
    assert!(bounded_size(&json!({"x":"😀".repeat(100)}),100).is_err());
    assert_eq!(bounded_size(&json!({"x":1}),100).unwrap(),7);
}

#[test]
fn exact_position_shape_is_shared_with_other_native_lsp_operations() {
    let input: LspInput = serde_json::from_value(request()).unwrap();
    assert_eq!(input.position,Some(Position { line:1, character:12 }));
}

#[test]
fn dropping_a_pending_signature_request_cancels_it_and_releases_the_tool_lane() {
    use std::future::Future as _;
    use std::task::Poll;
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(),"hang") else { return };
    let has_method = |method: &str| {
        std::fs::read_to_string(temp.path().join("semantic-requests.jsonl")).unwrap_or_default()
            .lines().filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["method"] == method)
    };
    runtime().block_on(async {
        let owner = AgentCx::for_current_or_request();
        let mut pending = Box::pin(tool.execute("cancel-signature",request(),None));
        let started = Instant::now();
        while !has_method("textDocument/signatureHelp") {
            assert!(started.elapsed() < Duration::from_secs(5), "request was not issued");
            let result = std::future::poll_fn(|cx| Poll::Ready(pending.as_mut().poll(cx))).await;
            assert!(result.is_pending(), "fixture unexpectedly completed: {result:?}");
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        drop(pending);
        let started = Instant::now();
        while !has_method("$/cancelRequest") {
            assert!(started.elapsed() < Duration::from_secs(2), "dropped request was not cancelled");
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        let output = tool.execute("after-drop",json!({"action":"status"}),None).await.unwrap();
        assert!(!output.is_error);
    });
}
