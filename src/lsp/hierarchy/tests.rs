//! Exercise the model-facing hierarchy workflow over real framed stdio.

use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

const SERVER: &str = r#"
import json, pathlib, sys, time
root = pathlib.Path.cwd()
mode = sys.argv[1]
def span(start=0, end=4):
    return {'start': {'line': 0, 'character': start}, 'end': {'line': 0, 'character': end}}
def item(name, path='source.graphfixture'):
    return {'name': name, 'kind': 12, 'uri': (root / path).as_uri(), 'range': span(0, 20),
            'selectionRange': span(), 'detail': 'signature ' + name, 'tags': [1],
            'data': {'token': name, 'nested': [None, {'opaque': 7}]}, 'x-fixture': 'preserve me'}
items = {'root': item('root'), 'leaf': item('leaf'), 'caller': item('caller', 'caller.graphfixture')}
types = json.loads(json.dumps(items))
for value in types.values():
    value['kind'] = 5
    value['data']['family'] = 'type'
if mode == 'type_external':
    types['caller']['uri'] = 'unresolved-type://library/ExternalBase'
def send(message):
    body = json.dumps(message).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b'\r\n', b'\n'):
            break
        key, value = line.decode('ascii').split(':', 1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    with (root / 'requests.jsonl').open('a', encoding='utf-8') as log:
        log.write(json.dumps(message) + '\n')
    method, params = message.get('method'), message.get('params') or {}
    if method is None:
        (root / 'callback.json').write_text(json.dumps(message), encoding='utf-8')
        continue
    if 'id' not in message:
        continue
    reply = {'jsonrpc': '2.0', 'id': message['id']}
    if method == 'initialize':
        caps = {'textDocumentSync': 0 if mode == 'unversioned' else 1,
                'callHierarchyProvider': False if mode in ('unsupported', 'types_only') else {},
                'typeHierarchyProvider': False if mode == 'type_unsupported' else {}}
        if mode == 'type_bad_capability':
            caps['typeHierarchyProvider'] = 'true'
        if mode == 'calls_only':
            del caps['typeHierarchyProvider']
        assert params['capabilities']['textDocument']['callHierarchy']['dynamicRegistration'] is False
        assert params['capabilities']['textDocument']['typeHierarchy']['dynamicRegistration'] is False
        reply['result'] = {'capabilities': caps}
    elif method == 'textDocument/prepareCallHierarchy':
        if mode == 'hang_prepare':
            (root / 'held').write_text('prepare', encoding='utf-8')
            continue
        if mode == 'shared_timeout':
            time.sleep(0.65)
        reply['result'] = [items['root']]
        if mode == 'ambiguous':
            reply['result'].append(items['leaf'])
        elif mode == 'none':
            reply['result'] = None
        elif mode == 'malformed_item':
            reply['result'].append({'name': 'bad'})
        elif mode == 'drift_prepare':
            (root / 'source.graphfixture').write_text('changed\n', encoding='utf-8')
        elif mode == 'unsolicited':
            send({'jsonrpc':'2.0', 'id':700, 'method':'workspace/applyEdit', 'params':{'edit':{'changes':{
                (root / 'source.graphfixture').as_uri(): [{'range':span(), 'newText':'UNAUTHORIZED'}]
            }}}})
    elif method == 'textDocument/prepareTypeHierarchy':
        reply['result'] = [types['root']]
        if mode == 'type_ambiguous':
            reply['result'].append(types['leaf'])
        elif mode == 'type_none':
            reply['result'] = None
    elif method in ('typeHierarchy/supertypes', 'typeHierarchy/subtypes'):
        selected = params['item']
        assert selected == types[selected['name']], 'type identity was not preserved exactly'
        target = types['caller'] if method == 'typeHierarchy/supertypes' else types['leaf']
        if selected['name'] != 'root':
            target = types['root']
        reply['result'] = [target]
        if mode == 'type_empty':
            reply['result'] = []
        elif mode == 'type_error':
            del reply['result']
            reply['error'] = {'code': -32603, 'message': 'type hierarchy failed'}
        elif mode == 'type_malformed':
            reply['result'].append({'from': items['caller'], 'fromRanges': [span()]})
        elif mode == 'type_many':
            reply['result'] = [target for _ in range(150)]
        elif mode == 'type_drift':
            (root / 'source.graphfixture').write_text('source changed\n', encoding='utf-8')
        elif mode == 'type_hang':
            (root / 'held').write_text('type', encoding='utf-8')
            continue
    elif method in ('callHierarchy/incomingCalls', 'callHierarchy/outgoingCalls'):
        selected = params['item']
        assert selected == items[selected['name']], 'opaque hierarchy item was changed'
        if mode == 'hang_calls':
            (root / 'held').write_text('calls', encoding='utf-8')
            continue
        if mode == 'shared_timeout':
            time.sleep(0.65)
        incoming = method == 'callHierarchy/incomingCalls'
        target = items['caller'] if incoming else items['leaf']
        if selected['name'] != 'root':
            target = items['root']
        edge = {'from' if incoming else 'to': target, 'fromRanges': [span(5, 9)]}
        reply['result'] = [edge]
        if mode == 'empty':
            reply['result'] = None
        elif mode == 'malformed_calls':
            reply['result'].append({'to':items['leaf'], 'fromRanges':[span(9, 5)]})
        elif mode == 'many':
            reply['result'] = [edge for _ in range(150)]
        elif mode == 'many_sites':
            edge['fromRanges'] = [span(5, 9) for _ in range(100)]
        elif mode == 'drift_calls':
            (root / 'source.graphfixture').write_text('changed\n', encoding='utf-8')
        elif mode == 'error':
            del reply['result']
            reply['error'] = {'code':-32603, 'message':'hierarchy fixture failed'}
    else:
        reply['result'] = None
    send(reply)
"#;

fn fixture(root: &Path, mode: &str) -> Option<(LspTool, asupersync::runtime::Runtime)> {
    let python = ["python3", "python"].into_iter().find(|program| {
        Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
            .status().is_ok_and(|status| status.success())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(), "Python required for hierarchy protocol coverage");
        eprintln!("SKIP hierarchy protocol test: Python unavailable");
        return None;
    };
    std::fs::write(root.join("source.graphfixture"), "root leaf caller\n").unwrap();
    std::fs::write(root.join("caller.graphfixture"), "root\n").unwrap();
    let settings = LspServerSettings {
        command: Some(python.into()),
        args: Some(vec!["-I".into(), "-u".into(), "-c".into(), SERVER.into(), mode.into()]),
        extensions: Some(vec![".graphfixture".into()]),
        languages: Some(vec!["plaintext".into()]),
        root_markers: Some(vec![]),
        ..Default::default()
    };
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([("hierarchy-fixture".into(), settings)])),
            ..Default::default()
        }),
        ..Default::default()
    };
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false).worker_threads(1).blocking_threads(1, 8).build().unwrap();
    Some((LspTool::new(&root.canonicalize().unwrap(), Some(&config)), runtime))
}

fn run(tool: &LspTool, runtime: &asupersync::runtime::Runtime, input: Value) -> Result<Value> {
    let output = runtime.block_on(tool.execute("hierarchy-test", input, None))?;
    assert!(!output.is_error);
    Ok(output.details.unwrap())
}

fn start(tool: &LspTool, runtime: &asupersync::runtime::Runtime, action: &str) -> Result<Value> {
    run(tool, runtime, json!({"action":action,"file":"source.graphfixture","symbol":"root"}))
}

fn frames(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("requests.jsonl")).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

fn sample_item() -> Value {
    json!({"name":"f","kind":12,"uri":"file:///source.rs",
        "range":{"start":{"line":0,"character":0},"end":{"line":2,"character":0}},
        "selectionRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},
        "data":{"opaque":[null,7]}})
}

#[test]
fn outgoing_handles_reuse_the_exact_item_without_repreparing() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let first = start(&tool, &runtime, "outgoing_calls").unwrap();
    assert_eq!(first["items"][0]["name"], "leaf");
    assert_eq!(first["items"][0]["callSiteUri"], first["source"]["uri"]);
    assert!(first["items"][0].get("data").is_none());
    let id = first["items"][0]["hierarchyId"].clone();
    let second = run(&tool, &runtime, json!({"action":"outgoing_calls","hierarchyId":id})).unwrap();
    assert_eq!(second["items"][0]["name"], "root");
    let log = frames(temp.path());
    assert_eq!(log.iter().filter(|frame| frame["method"] == "textDocument/prepareCallHierarchy").count(), 1);
    assert_eq!(log.iter().filter(|frame| frame["method"] == "callHierarchy/outgoingCalls").count(), 2);
}

#[test]
fn incoming_call_sites_belong_to_the_caller_not_the_selected_callee() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let result = start(&tool, &runtime, "incoming_calls").unwrap();
    assert_eq!(result["items"][0]["name"], "caller");
    assert_eq!(result["items"][0]["callSiteUri"], result["items"][0]["uri"]);
    assert_ne!(result["items"][0]["callSiteUri"], result["source"]["uri"]);
    let id = result["items"][0]["hierarchyId"].clone();
    let next = run(&tool, &runtime, json!({"action":"outgoing_calls","hierarchyId":id})).unwrap();
    assert_eq!(next["source"]["name"], "caller");
}

#[test]
fn ambiguous_preparation_returns_selectable_handles_without_guessing() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "ambiguous") else { return };
    let result = start(&tool, &runtime, "incoming_calls").unwrap();
    assert_eq!(result["selectionRequired"], true);
    assert_eq!(result["count"], 2);
    assert!(!frames(temp.path()).iter().any(|frame| frame["method"] == "callHierarchy/incomingCalls"));
    let next = run(&tool, &runtime, json!({"action":"incoming_calls","hierarchyId":result["items"][1]["hierarchyId"]})).unwrap();
    assert_eq!(next["source"]["name"], "leaf");
}

#[test]
fn source_drift_expires_handles_before_wire_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let result = start(&tool, &runtime, "outgoing_calls").unwrap();
    std::fs::write(temp.path().join("source.graphfixture"), "external change\n").unwrap();
    let error = run(&tool, &runtime, json!({"action":"outgoing_calls","hierarchyId":result["items"][0]["hierarchyId"]})).unwrap_err();
    assert!(error.to_string().contains("LSP_HIERARCHY_EXPIRED"));
    assert_eq!(frames(temp.path()).iter().filter(|frame| frame["method"] == "callHierarchy/outgoingCalls").count(), 1);
}

#[test]
fn source_changes_during_either_stage_never_publish_a_graph() {
    for mode in ["drift_prepare", "drift_calls"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        assert!(start(&tool, &runtime, "outgoing_calls").unwrap_err().to_string().contains("LSP_HIERARCHY_EXPIRED"));
        assert!(lock(&tool.hierarchies.entries).is_empty());
    }
}

#[test]
fn closed_and_reopened_identical_unversioned_sources_retire_handles() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "unversioned") else { return };
    let result = start(&tool, &runtime, "outgoing_calls").unwrap();
    let id = result["items"][0]["hierarchyId"].as_str().unwrap();
    let cached = tool.hierarchies.get(id).unwrap();
    let entry = cached.origin.entry.upgrade().unwrap();
    entry.client.invalidate(&cached.origin.uri);
    entry.client.ensure_synced(&cached.origin.path, "plaintext").unwrap();
    assert!(run(&tool, &runtime, json!({"action":"incoming_calls","hierarchyId":id})).unwrap_err().to_string().contains("LSP_HIERARCHY_EXPIRED"));
}

#[test]
fn server_reload_retires_handles_without_automatically_starting_another_server() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let result = start(&tool, &runtime, "incoming_calls").unwrap();
    run(&tool, &runtime, json!({"action":"reload"})).unwrap();
    assert!(run(&tool, &runtime, json!({"action":"incoming_calls","hierarchyId":result["items"][0]["hierarchyId"]})).unwrap_err().to_string().contains("LSP_HIERARCHY_EXPIRED"));
    assert_eq!(frames(temp.path()).iter().filter(|frame| frame["method"] == "initialize").count(), 1);
}

#[test]
fn unsupported_and_failed_servers_are_not_empty_graphs() {
    for (mode, expected) in [("unsupported", "LSP_HIERARCHY_UNSUPPORTED"), ("error", "hierarchy fixture failed"),
        ("malformed_item", "LSP_HIERARCHY_PROTOCOL"), ("malformed_calls", "LSP_HIERARCHY_PROTOCOL")]
    {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        assert!(start(&tool, &runtime, "outgoing_calls").unwrap_err().to_string().contains(expected));
        assert!(lock(&tool.hierarchies.entries).is_empty());
    }
}

#[test]
fn null_preparation_and_null_calls_are_explicit_successful_empty_results() {
    for mode in ["none", "empty"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        let result = start(&tool, &runtime, "outgoing_calls").unwrap();
        assert_eq!(result["count"], 0);
        assert_eq!(result["truncated"], false);
        assert_eq!(result["selectionRequired"], false);
        assert_eq!(result["source"].is_null(), mode == "none");
    }
}

#[test]
fn item_and_site_limits_preserve_whole_ranges_and_advertise_truncation() {
    for mode in ["many", "many_sites"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        let result = start(&tool, &runtime, "outgoing_calls").unwrap();
        assert!(result.to_string().len() <= super::super::MAX_PAYLOAD_BYTES);
        if mode == "many" {
            assert_eq!(result["count"], 100);
            assert_eq!(result["total"], 150);
            assert_eq!(result["truncated"], true);
        } else {
            assert_eq!(result["items"][0]["callSiteCount"], 100);
            assert_eq!(result["items"][0]["fromRanges"].as_array().unwrap().len(), MAX_SHOWN_CALL_SITES);
            assert_eq!(result["items"][0]["callSitesTruncated"], true);
        }
        for row in result["items"].as_array().unwrap() {
            assert!(tool.hierarchies.get(row["hierarchyId"].as_str().unwrap()).is_ok());
        }
    }
}

#[test]
fn malformed_ranges_selections_and_items_are_rejected_before_caching() {
    let valid = sample_item();
    assert!(Item::parse(&valid).is_ok());
    for (field, value) in [("kind", json!(99)), ("name", json!("")), ("uri", json!("relative")),
        ("detail", json!(false)), ("tags", json!(["bad"])), ("selectionRange", json!({"start":{"line":3,"character":0},"end":{"line":3,"character":1}}))]
    {
        let mut raw = valid.clone();
        raw[field] = value;
        assert!(Item::parse(&raw).is_err(), "field {field}");
    }
    let mut oversized = valid;
    oversized["data"] = json!("x".repeat(MAX_ITEM_BYTES));
    assert!(Item::parse(&oversized).is_err());
    assert!(response_items(&json!({})).is_err());
    assert!(response_items(&json!(vec![Value::Null; MAX_RESPONSE_ITEMS + 1])).is_err());
}

#[test]
fn prepare_positions_use_the_synchronized_unicode_and_crlf_source() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    std::fs::write(temp.path().join("source.graphfixture"), "😀root\r\nleaf").unwrap();
    start(&tool, &runtime, "outgoing_calls").unwrap();
    let log = frames(temp.path());
    let prepare = log.iter().find(|frame| frame["method"] == "textDocument/prepareCallHierarchy").unwrap();
    assert_eq!(prepare["params"]["position"], json!({"line":0,"character":2}));
}

#[test]
fn unsolicited_workspace_edits_remain_denied_during_hierarchy_queries() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "unsolicited") else { return };
    start(&tool, &runtime, "outgoing_calls").unwrap();
    let reply: Value = serde_json::from_str(&std::fs::read_to_string(temp.path().join("callback.json")).unwrap()).unwrap();
    assert_eq!(reply["result"]["applied"], false);
    assert_eq!(std::fs::read_to_string(temp.path().join("source.graphfixture")).unwrap(), "root leaf caller\n");
}

#[test]
fn preparation_and_expansion_share_one_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "shared_timeout") else { return };
    let error = run(&tool, &runtime, json!({"action":"outgoing_calls","file":"source.graphfixture","symbol":"root","timeout":1})).unwrap_err();
    assert!(error.to_string().contains("LSP_TIMEOUT"));
    assert!(lock(&tool.hierarchies.entries).is_empty());
}

#[test]
fn dropping_each_request_stage_cancels_it_without_leaking_handles() {
    for mode in ["hang_prepare", "hang_calls"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        runtime.block_on(async {
            let mut request = Box::pin(tool.execute("cancelled", json!({"action":"outgoing_calls","file":"source.graphfixture","symbol":"root"}), None));
            let owner = AgentCx::for_current_or_request();
            let start = Instant::now();
            loop {
                assert!(futures::poll!(request.as_mut()).is_pending());
                if temp.path().join("held").exists() { break; }
                assert!(start.elapsed() < Duration::from_secs(10), "fixture did not reach the held stage");
                owner.time().sleep(Duration::from_millis(10)).await;
            }
            drop(request);
        });
        run(&tool, &runtime, json!({"action":"request","file":"source.graphfixture","method":"test/barrier"})).unwrap();
        assert!(frames(temp.path()).iter().any(|frame| frame["method"] == "$/cancelRequest"));
        assert!(lock(&tool.hierarchies.entries).is_empty());
    }
}

#[test]
fn cache_evicts_old_handles_without_evicting_the_just_returned_batch() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "many") else { return };
    let first = start(&tool, &runtime, "outgoing_calls").unwrap();
    start(&tool, &runtime, "outgoing_calls").unwrap();
    let latest = start(&tool, &runtime, "outgoing_calls").unwrap();
    assert!(tool.hierarchies.get(first["source"]["hierarchyId"].as_str().unwrap()).is_err());
    for item in latest["items"].as_array().unwrap() {
        assert!(tool.hierarchies.get(item["hierarchyId"].as_str().unwrap()).is_ok());
    }
    assert!(lock(&tool.hierarchies.entries).len() <= MAX_CACHE_ITEMS);
}

#[test]
fn expired_handles_do_not_renew_their_lifetime_on_lookup() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let result = start(&tool, &runtime, "outgoing_calls").unwrap();
    let id = result["items"][0]["hierarchyId"].as_str().unwrap();
    {
        let mut entries = lock(&tool.hierarchies.entries);
        let cached = entries.iter_mut().find(|cached| cached.id == id).unwrap();
        let old = &cached.origin;
        cached.origin = Arc::new(Origin {
            entry: old.entry.clone(), path: old.path.clone(), uri: old.uri.clone(),
            text: Arc::clone(&old.text), created: Instant::now() - HANDLE_TTL,
            family: old.family,
        });
    }
    assert!(tool.hierarchies.get(id).is_err());
    assert!(tool.hierarchies.get(id).is_err());
}

#[test]
fn caller_without_io_authority_cannot_reuse_a_live_handle() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let result = start(&tool, &runtime, "outgoing_calls").unwrap();
    let cached = tool.hierarchies.get(result["items"][0]["hierarchyId"].as_str().unwrap()).unwrap();
    let entry = cached.origin.entry.upgrade().unwrap();
    let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
    let owner = {
        let _guard = restricted.set_current_restricted();
        AgentCx::for_current_or_request()
    };
    assert!(cached.origin.check(&entry, &owner).unwrap_err().to_string().contains("LSP_HIERARCHY_AUTHORITY"));
}

#[test]
fn supertypes_and_subtypes_follow_exact_type_items_without_repreparing() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let first = start(&tool, &runtime, "supertypes").unwrap();
    assert_eq!(first["hierarchyKind"], "type");
    assert_eq!(first["items"][0]["name"], "caller");
    assert_eq!(first["items"][0]["kind"], 5);
    assert!(first["items"][0].get("fromRanges").is_none());
    assert!(first["items"][0].get("callSiteUri").is_none());
    assert!(first["items"][0].get("data").is_none());
    let second = run(&tool, &runtime, json!({"action":"subtypes","hierarchyId":first["items"][0]["hierarchyId"]})).unwrap();
    assert_eq!(second["items"][0]["name"], "root");
    let log = frames(temp.path());
    assert_eq!(log.iter().filter(|frame| frame["method"] == "textDocument/prepareTypeHierarchy").count(), 1);
    assert_eq!(log.iter().filter(|frame| frame["method"] == "typeHierarchy/subtypes").count(), 1);
}

#[test]
fn call_and_type_handles_are_not_interchangeable_in_either_direction() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let calls = start(&tool, &runtime, "incoming_calls").unwrap();
    let types = start(&tool, &runtime, "supertypes").unwrap();
    let before = frames(temp.path()).len();
    for (result, action) in [(&calls, "subtypes"), (&types, "outgoing_calls")] {
        let error = run(&tool, &runtime, json!({"action":action,"hierarchyId":result["source"]["hierarchyId"]})).unwrap_err();
        assert!(error.to_string().contains("another hierarchy kind"));
    }
    assert_eq!(frames(temp.path()).len(), before, "family mismatch must fail before dispatch");
}

#[test]
fn type_capabilities_are_negotiated_independently_of_call_capabilities() {
    for mode in ["types_only", "calls_only", "type_unsupported", "type_bad_capability"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        if mode == "types_only" {
            assert_eq!(start(&tool, &runtime, "subtypes").unwrap()["items"][0]["name"], "leaf");
            assert!(start(&tool, &runtime, "outgoing_calls").is_err());
        } else {
            assert!(start(&tool, &runtime, "subtypes").is_err());
            assert!(!frames(temp.path()).iter().any(|frame| frame["method"] == "textDocument/prepareTypeHierarchy"));
        }
    }
}

#[test]
fn ambiguous_types_return_choices_without_querying_a_guessed_root() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "type_ambiguous") else { return };
    let result = start(&tool, &runtime, "subtypes").unwrap();
    assert_eq!(result["selectionRequired"], true);
    assert_eq!(result["hierarchyKind"], "type");
    assert_eq!(result["count"], 2);
    assert!(!frames(temp.path()).iter().any(|frame| frame["method"] == "typeHierarchy/subtypes"));
    let next = run(&tool, &runtime, json!({"action":"subtypes","hierarchyId":result["items"][1]["hierarchyId"]})).unwrap();
    assert_eq!(next["source"]["name"], "leaf");
}

#[test]
fn unknown_and_leaf_types_are_empty_results_not_failures() {
    for mode in ["type_none", "type_empty"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        let result = start(&tool, &runtime, "supertypes").unwrap();
        assert_eq!(result["count"], 0);
        assert_eq!(result["total"], 0);
        assert_eq!(result["truncated"], false);
    }
}

#[test]
fn type_errors_malformed_tails_and_drift_do_not_publish_partial_results() {
    for (mode, expected) in [("type_error", "type hierarchy failed"),
        ("type_malformed", "LSP_HIERARCHY_PROTOCOL"), ("type_drift", "LSP_HIERARCHY_EXPIRED")]
    {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        assert!(start(&tool, &runtime, "subtypes").unwrap_err().to_string().contains(expected));
        assert!(lock(&tool.hierarchies.entries).is_empty());
    }
}

#[test]
fn external_type_resources_are_opaque_labels_and_can_be_followed_without_opening_them() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "type_external") else { return };
    let result = start(&tool, &runtime, "supertypes").unwrap();
    assert_eq!(result["items"][0]["uri"], "unresolved-type://library/ExternalBase");
    let next = run(&tool, &runtime, json!({"action":"subtypes","hierarchyId":result["items"][0]["hierarchyId"]})).unwrap();
    assert_eq!(next["source"]["uri"], "unresolved-type://library/ExternalBase");
    assert_eq!(next["items"][0]["name"], "root");
}

#[test]
fn type_output_limits_return_only_complete_reusable_items() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "type_many") else { return };
    let result = run(&tool, &runtime, json!({"action":"subtypes","file":"source.graphfixture","symbol":"root","limit":2})).unwrap();
    assert_eq!(result["count"], 2);
    assert_eq!(result["total"], 150);
    assert_eq!(result["truncated"], true);
    for row in result["items"].as_array().unwrap() {
        assert!(tool.hierarchies.get(row["hierarchyId"].as_str().unwrap()).is_ok());
    }
}

#[test]
fn dropping_type_expansion_cancels_the_pending_protocol_request() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "type_hang") else { return };
    runtime.block_on(async {
        let mut request = Box::pin(tool.execute("cancel-types", json!({"action":"supertypes","file":"source.graphfixture","symbol":"root"}), None));
        let owner = AgentCx::for_current_or_request();
        let start = Instant::now();
        loop {
            assert!(futures::poll!(request.as_mut()).is_pending());
            if temp.path().join("held").exists() { break; }
            assert!(start.elapsed() < Duration::from_secs(10));
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        drop(request);
    });
    run(&tool, &runtime, json!({"action":"request","file":"source.graphfixture","method":"test/barrier"})).unwrap();
    let log = frames(temp.path());
    let request = log.iter().find(|frame| frame["method"] == "typeHierarchy/supertypes").unwrap();
    assert!(log.iter().any(|frame| frame["method"] == "$/cancelRequest" && frame["params"]["id"] == request["id"]));
    assert!(lock(&tool.hierarchies.entries).is_empty());
}
