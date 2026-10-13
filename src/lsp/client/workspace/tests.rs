use super::*;
use super::super::test_server::Fixture;
use crate::lsp::jsonrpc::{JsonRpcClient, TransportError};
use std::sync::atomic::Ordering;

fn fixture() -> (tempfile::TempDir, Workspace) {
    let temp = tempfile::tempdir().unwrap();
    let uri = super::super::try_path_to_uri(&temp.path().canonicalize().unwrap()).unwrap();
    let workspace = Workspace::new(&uri).unwrap();
    (temp, workspace)
}

fn seed(workspace: &Workspace, value: Value) {
    let settings = Settings::parse(&value).unwrap();
    LspClient::lock(&workspace.state).settings = settings.0;
}

#[test]
fn configuration_preserves_request_order_duplicates_and_json_types() {
    let (_temp, workspace) = fixture();
    let settings = json!({"python":{"analysis":{"enabled":false,"depth":0,"path":"","optional":null}},"list":[1,true]});
    seed(&workspace, settings.clone());
    let reply = workspace.configuration(&json!({"items":[
        {"section":"python.analysis.depth"},{"section":"python.analysis.enabled"},
        {"section":"python.analysis.path"},{"section":"python.analysis.optional"},
        {"section":"missing"},{"section":"python.analysis.depth"},{"section":"list"},
        {},{"section":""},{"section":null}
    ]})).unwrap();
    assert_eq!(reply, json!([0,false,"",null,null,0,[1,true],settings,settings,settings]));
}

#[test]
fn only_nested_object_sections_are_traversed_without_string_coercion() {
    let (_temp, workspace) = fixture();
    seed(&workspace, json!({"python.analysis":"flat key", "python":{"analysis":"nested"},"list":[99]}));
    assert_eq!(workspace.configuration(&json!({"items":[
        {"section":"python.analysis"},{"section":"list.0"},{"section":"python.analysis.more"}
    ]})).unwrap(), json!(["nested",null,null]));
}

#[test]
fn scope_is_component_based_and_never_opens_unseen_targets() {
    let (temp, workspace) = fixture();
    seed(&workspace, json!({"option":42}));
    let inside = workspace.root.join("missing/sub/file.rs");
    let outside = workspace.root.with_file_name(format!("{}-sibling", workspace.root.file_name().unwrap().to_string_lossy()));
    let to_uri = super::super::try_path_to_uri;
    let result = workspace.configuration(&json!({"items":[
        {"section":"option","scopeUri":to_uri(&workspace.root).unwrap()},
        {"section":"option","scopeUri":to_uri(&inside).unwrap()},
        {"section":"option","scopeUri":to_uri(&outside).unwrap()},
        {"section":"option","scopeUri":"https://example.invalid/project"},
        {"section":"option","scopeUri":"untitled:buffer"},
        {"section":"option","scopeUri":null}
    ]})).unwrap();
    assert_eq!(result, json!([42,42,null,null,null,42]));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn malformed_configuration_never_becomes_a_full_settings_disclosure() {
    let (_temp, workspace) = fixture();
    seed(&workspace, json!({"private-to-this-server":"marker"}));
    for params in [
        Value::Null, json!({}), json!({"items":true}), json!({"items":[false]}),
        json!({"items":[{"section":5}]}), json!({"items":[{"scopeUri":5}]}),
        json!({"items":[{"scopeUri":"relative/path"}]}),
        json!({"items":[{"scopeUri":"file:///space here"}]}),
        json!({"items":[{"section":"x".repeat(1025)}]}),
        json!({"items":[{"scopeUri":""}]}),
    ] {
        let error = workspace.configuration(&params).unwrap_err();
        assert_eq!(error.code, -32602);
        assert!(!error.message.contains("marker"));
        assert!(error.data.is_none());
    }
}

#[test]
fn request_count_and_reply_amplification_are_bounded_without_partial_success() {
    let (_temp, workspace) = fixture();
    assert_eq!(workspace.configuration(&json!({"items":[]})).unwrap(), json!([]));
    let items = vec![json!({"section":"missing"}); MAX_ITEMS];
    assert_eq!(workspace.configuration(&json!({"items":items})).unwrap().as_array().unwrap().len(), MAX_ITEMS);
    assert!(workspace.configuration(&json!({"items":vec![json!({});MAX_ITEMS+1]})).is_err());
    seed(&workspace, json!({"large":"x".repeat(32000)}));
    let one = json!({"section":"large"});
    assert_eq!(workspace.configuration(&json!({"items":[one]})).unwrap()[0].as_str().unwrap().len(), 32000);
    assert!(workspace.configuration(&json!({"items":vec![one;9]})).is_err());
}

#[test]
fn settings_serialized_limit_counts_unicode_and_escaping() {
    let exact = json!({"x":"x".repeat(MAX_SETTINGS_BYTES-8)});
    assert_eq!(serde_json::to_vec(&exact).unwrap().len(), MAX_SETTINGS_BYTES);
    Settings::parse(&exact).unwrap();
    assert!(Settings::parse(&json!({"x":"x".repeat(MAX_SETTINGS_BYTES-7)})).is_err());
    assert!(Settings::parse(&json!({"x":"\0".repeat(11000)})).is_err());
    assert!(Settings::parse(&json!({"x":"界".repeat(22000)})).is_err());
    for wrong in [Value::Null, json!([]), json!(true), json!(3)] {
        assert!(Settings::parse(&wrong).is_err());
    }
}

#[test]
fn settings_depth_and_node_limits_precede_recursive_serialization() {
    let mut nested = json!(0);
    for _ in 0..MAX_DEPTH { nested = json!({"x":nested}); }
    Settings::parse(&nested).unwrap();
    assert!(Settings::parse(&json!({"x":nested})).is_err());
    // Root object + array + elements = exactly MAX_NODES.
    Settings::parse(&json!({"x":vec![0;MAX_NODES-2]})).unwrap();
    assert!(Settings::parse(&json!({"x":vec![0;MAX_NODES-1]})).is_err());
}

#[test]
fn workspace_folders_are_exactly_the_connection_root() {
    let (temp, workspace) = fixture();
    for params in [Value::Null,json!({})] {
        let folders = workspace.respond("workspace/workspaceFolders", &params).unwrap().unwrap();
        assert_eq!(folders, workspace.folders);
        assert_eq!(folders.as_array().unwrap().len(), 1);
        assert_eq!(folders[0]["uri"], super::super::try_path_to_uri(&temp.path().canonicalize().unwrap()).unwrap());
    }
    assert!(workspace.respond("workspace/workspaceFolders", &json!({"other":true})).unwrap().is_err());
    assert!(workspace.respond("workspace/applyEdit", &json!({})).is_none());
    assert!(workspace.respond("workspace/executeCommand", &json!({})).is_none());
}

#[test]
fn independent_workspaces_do_not_share_settings_or_read_agent_configuration() {
    let (temp, first) = fixture();
    let (_other, second) = fixture();
    std::fs::write(temp.path().join("settings.json"), r#"{"apiKey":"must-not-export"}"#).unwrap();
    seed(&first, json!({"chosen":"first"}));
    let request = json!({"items":[{}, {"section":"apiKey"}]});
    assert_eq!(first.configuration(&request).unwrap(), json!([{"chosen":"first"},null]));
    assert_eq!(second.configuration(&request).unwrap(), json!([{},null]));
}

#[test]
fn parsed_settings_are_immutable_copies_not_caller_owned_references() {
    let mut raw = json!({"server":{"enabled":false}});
    let settings = Settings::parse(&raw).unwrap();
    raw["server"]["enabled"] = json!(true);
    assert_eq!(settings.value()["server"]["enabled"], false);
}

mod protocol;

fn saturate_writer(peer: &Fixture) {
    // The peer must already have acknowledged stall_reads. Smaller filler
    // frames leave less room than the 64-KiB configuration we will attempt.
    let payload = "x".repeat(32 * 1024);
    for _ in 0..4096 {
        let mut batch = JsonRpcClient::prepare_notifications(vec![(
            "test/outboundPressure",
            json!({"payload":payload}),
        )]);
        match peer.client.rpc.try_admit_notifications(&mut batch) {
            Ok(()) => {}
            Err(TransportError::Backpressure(_)) => return,
            Err(error) => panic!("unexpected pressure admission error: {error:?}"),
        }
    }
    panic!("the stopped peer must exhaust the bounded writer budget");
}

#[test]
#[allow(clippy::too_many_lines)]
fn configuration_backpressure_preserves_state_and_retry_closes_before_updating() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({"textDocumentSync":1})) else {
        return;
    };
    let before = Settings::parse(&json!({"strict":false})).unwrap();
    assert_eq!(peer.client.set_workspace_settings(&before).unwrap(), (true, 1));
    let path = temp.path().join("source.rs");
    std::fs::write(&path, "fn original() {}\n").unwrap();
    let uri = peer.client.ensure_synced(&path, "rust").unwrap();
    let snapshot = peer.client.document_snapshots()[&uri];
    peer.notifications(&[json!({
        "jsonrpc":"2.0","method":"textDocument/publishDiagnostics",
        "params":{"uri":uri,"version":snapshot.version,"diagnostics":[{"message":"keep me"}]}
    })]);
    let diagnostics = peer.client.diagnostics_snapshot();
    assert_eq!(diagnostics[&uri][0]["message"], "keep me");
    peer.client.quiescent.store(true, Ordering::SeqCst);
    let after = Settings::parse(&json!({"x":"x".repeat(MAX_SETTINGS_BYTES - 8)})).unwrap();
    let stalled = peer.stall_reads(temp.path());
    saturate_writer(&peer);

    // Even under pressure an unchanged snapshot consumes no queue capacity.
    assert_eq!(peer.client.set_workspace_settings(&before).unwrap(), (false, 1));
    let error = peer.client.set_workspace_settings(&after).unwrap_err();
    assert!(error.to_string().contains("LSP_BACKPRESSURE"), "{error}");
    assert!(peer.client.is_alive(), "queue pressure is not a broken pipe");
    let (settings, revision) = peer.client.workspace_settings();
    assert_eq!(settings.as_ref(), before.value());
    assert_eq!(revision, 1);
    let retained = peer.client.document_snapshots()[&uri];
    assert_eq!(retained.version, snapshot.version);
    assert_eq!(retained.hash, snapshot.hash);
    assert_eq!(peer.client.diagnostics_snapshot(), diagnostics);
    assert!(peer.client.quiescent.load(Ordering::SeqCst));

    drop(stalled);
    let frames = peer.frames(); // also acknowledges drainage of all filler
    assert!(!frames.iter().any(|frame| frame["method"] == "textDocument/didClose"));
    assert_eq!(frames.iter().filter(|frame| frame["method"] == "workspace/didChangeConfiguration").count(), 1);
    assert_eq!(peer.client.set_workspace_settings(&after).unwrap(), (true, 2));
    assert_eq!(peer.client.open_document_count(), 0);
    assert!(peer.client.diagnostics_snapshot().is_empty());
    assert!(!peer.client.quiescent.load(Ordering::SeqCst));
    assert_eq!(peer.client.workspace_settings().0.as_ref(), after.value());
    peer.client.ensure_synced(&path, "rust").unwrap();
    assert!(peer.client.document_snapshots()[&uri].version > snapshot.version);
    let frames = peer.frames();
    let updates: Vec<_> = frames.iter().enumerate()
        .filter(|(_, frame)| frame["method"] == "workspace/didChangeConfiguration")
        .collect();
    assert_eq!(updates.len(), 2, "the rejected attempt must have no wire effects");
    let update = updates[1].0;
    assert_eq!(frames[update]["params"]["settings"], *after.value());
    assert_eq!(frames[update - 1]["method"], "textDocument/didClose");
    assert_eq!(frames[update - 1]["params"]["textDocument"]["uri"], uri);
    assert_eq!(frames[update + 1]["method"], "textDocument/didOpen");
    assert_eq!(frames.iter().filter(|frame| frame["method"] == "textDocument/didClose").count(), 1);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "fn original() {}\n");
}

#[test]
fn configuration_retires_the_full_working_set_in_one_ordered_batch() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({"textDocumentSync":1})) else {
        return;
    };
    let mut uris = Vec::new();
    for index in 0..128 {
        let path = temp.path().join(format!("source-{index:03}.rs"));
        std::fs::write(&path, "fn source() {}\n").unwrap();
        uris.push(peer.client.ensure_synced(&path, "rust").unwrap());
    }
    assert_eq!(peer.client.open_document_count(), 128);
    let settings = Settings::parse(&json!({"strict":true})).unwrap();
    assert_eq!(peer.client.set_workspace_settings(&settings).unwrap(), (true, 1));
    assert_eq!(peer.client.open_document_count(), 0);
    let frames = peer.frames();
    let update = frames.iter().position(|frame| frame["method"] == "workspace/didChangeConfiguration").unwrap();
    let closes = &frames[update - uris.len()..update];
    uris.sort_unstable();
    for (close, uri) in closes.iter().zip(uris) {
        assert_eq!(close["method"], "textDocument/didClose");
        assert_eq!(close["params"]["textDocument"]["uri"], uri);
    }
    assert_eq!(frames.iter().filter(|frame| frame["method"] == "textDocument/didClose").count(), 128);
}

#[test]
fn concurrent_configurations_publish_in_the_same_order_as_the_wire() {
    let temp = tempfile::tempdir().unwrap();
    let Some(peer) = Fixture::connect(temp.path(), json!({"textDocumentSync":1})) else {
        return;
    };
    let path = temp.path().join("source.rs");
    std::fs::write(&path, "fn source() {}\n").unwrap();
    peer.client.ensure_synced(&path, "rust").unwrap();
    let first = Settings::parse(&json!({"choice":"first"})).unwrap();
    let second = Settings::parse(&json!({"choice":"second"})).unwrap();
    let barrier = std::sync::Barrier::new(2);
    let mut revisions = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            peer.client.set_workspace_settings(&first).unwrap().1
        });
        let right = scope.spawn(|| {
            barrier.wait();
            peer.client.set_workspace_settings(&second).unwrap().1
        });
        [left.join().unwrap(), right.join().unwrap()]
    });
    revisions.sort_unstable();
    assert_eq!(revisions, [1, 2]);
    let frames = peer.frames();
    let updates: Vec<_> = frames.iter()
        .filter(|frame| frame["method"] == "workspace/didChangeConfiguration")
        .collect();
    assert_eq!(updates.len(), 2);
    let (settings, revision) = peer.client.workspace_settings();
    assert_eq!(revision, 2);
    assert_eq!(&updates[1]["params"]["settings"], settings.as_ref());
    assert_eq!(frames.iter().filter(|frame| frame["method"] == "textDocument/didClose").count(), 1);
}
