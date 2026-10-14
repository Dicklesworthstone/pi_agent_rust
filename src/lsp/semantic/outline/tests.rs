use super::*;
use crate::lsp::text::position_to_offset_exact;
use crate::tools::Tool as _;
use std::time::Duration;

const SOURCE: &str = "class Box {\r\n  run() {}\r\n}\r\nfunction run() {}\r\n";

fn budget() -> Budget {
    Budget::new(Duration::from_secs(5))
}

fn at(line: u32, character: u32) -> Position {
    Position { line, character }
}

fn span(start: Position, end: Position) -> Value {
    json!({"start":start,"end":end})
}

fn symbol(name: &str, start: Position, end: Position) -> Value {
    json!({"name":name,"kind":12,"range":span(start,end),"selectionRange":span(start,end)})
}

fn tree() -> Value {
    let child = symbol("run", at(1, 2), at(1, 5));
    json!([
        {"name":"Box","kind":5,"detail":"class Box","tags":[1],
         "range":span(at(0,0),at(2,1)),"selectionRange":span(at(0,6),at(0,9)),
         "children":[child]},
        symbol("run", at(3,9), at(3,12))
    ])
}

#[test]
fn outline_preserves_server_preorder_duplicates_and_explicit_hierarchy() {
    let raw = tree();
    let outline = parse(&raw, Path::new("unused"), SOURCE, &budget()).unwrap();
    let rows = displayed(&outline, 0..outline.symbols.len(), 100, &budget()).unwrap();
    assert_eq!(outline.shape, "hierarchical");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["name"], "Box");
    assert_eq!(rows[0]["tags"], json!([1]));
    assert_eq!(rows[0]["detail"], "class Box");
    assert_eq!(rows[0]["parentIndex"], Value::Null);
    assert_eq!(rows[1]["index"], 1);
    assert_eq!(rows[1]["parentIndex"], 0);
    assert_eq!(rows[1]["depth"], 1);
    assert_eq!(rows[2]["name"], rows[1]["name"]);
    assert_eq!(rows[2]["depth"], 0);
    assert_eq!(rows[2]["selectionRange"], span(at(3,9),at(3,12)));
}

#[test]
fn flat_container_names_never_invent_parent_or_identifier_ranges() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap().join("not-opened.rs");
    let uri = crate::lsp::client::try_path_to_uri(&path).unwrap();
    let raw = json!([
        {"name":"run","kind":12,"containerName":"Box","deprecated":true,
         "location":{"uri":uri,"range":span(at(1,2),at(1,10))}}
    ]);
    let outline = parse(&raw, &path, SOURCE, &budget()).unwrap();
    let rows = displayed(&outline, 0..outline.symbols.len(), 1, &budget()).unwrap();
    assert_eq!(outline.shape, "flat");
    assert_eq!(rows[0]["containerName"], "Box");
    assert_eq!(rows[0]["deprecated"], true);
    assert_eq!(rows[0]["location"], raw[0]["location"]);
    assert_eq!(rows[0]["selectionRange"], Value::Null);
    assert_eq!(rows[0]["parentIndex"], Value::Null);
    assert!(!path.exists(), "parsing must not open or create a returned URI");
    let other = path.with_file_name("foreign.rs");
    assert!(parse(&raw, &other, SOURCE, &budget()).is_err());
}

#[test]
fn batched_boundaries_match_the_shared_exact_utf16_mapper() {
    for text in ["", "abc", "😀x", "a\rb\n界\r\n😀z\n", "\r\n", "e\u{301}"] {
        let mut all_valid = BTreeSet::new();
        for line in 0..8 {
            for character in 0..12 {
                let p = at(line, character);
                let expected = position_to_offset_exact(text, p).is_some();
                assert_eq!(validate_boundaries(text, &BTreeSet::from([p]), &budget()).is_ok(), expected,
                    "{text:?} at {p:?}");
                if expected { all_valid.insert(p); }
            }
        }
        validate_boundaries(text, &all_valid, &budget()).unwrap();
    }
}

#[test]
fn complete_validation_rejects_bad_hidden_members_and_mixed_shapes() {
    for bad in [
        json!({"name":"bad","kind":12,"range":span(at(0,0),at(0,1)),"selectionRange":span(at(0,0),at(0,2))}),
        symbol("bad", at(99,0), at(99,1)),
        symbol("bad", at(0,3), at(0,1)),
        symbol(" ", at(0,0), at(0,1)),
        json!({"name":"bad","kind":27,"range":span(at(0,0),at(0,1)),"selectionRange":span(at(0,0),at(0,1))}),
        json!({"name":"bad","kind":12,"location":{"uri":"untitled:foreign","range":span(at(0,0),at(0,1))}}),
        json!(false),
    ] {
        let raw = json!([symbol("first", at(0,0), at(0,1)), bad]);
        assert!(parse(&raw, Path::new("unused"), SOURCE, &budget()).is_err());
    }
    for extra in [json!({"children":true}), json!({"tags":["1"]}), json!({"deprecated":"yes"}), json!({"detail":4})] {
        let mut bad = symbol("bad", at(0,0), at(0,1));
        bad.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        assert!(parse(&json!([bad]), Path::new("unused"), SOURCE, &budget()).is_err());
    }
}

#[test]
fn all_nested_symbols_and_levels_spend_the_same_limits() {
    let leaf = symbol("nested", at(0,0), at(0,1));
    let mut deep = leaf.clone();
    for _ in 1..MAX_DEPTH {
        let mut next = leaf.clone();
        next["children"] = json!([deep]);
        deep = next;
    }
    let raw = json!([deep]);
    assert_eq!(parse(&raw, Path::new("unused"), SOURCE, &budget()).unwrap().symbols.len(), MAX_DEPTH);
    let mut too_deep = leaf.clone();
    too_deep["children"] = raw;
    assert!(parse(&json!([too_deep]), Path::new("unused"), SOURCE, &budget()).is_err());
    let mut root = leaf.clone();
    root["children"] = json!(vec![leaf.clone(); MAX_SYMBOLS - 1]);
    assert_eq!(parse(&json!([root]), Path::new("unused"), SOURCE, &budget()).unwrap().symbols.len(), MAX_SYMBOLS);
    assert!(parse(&json!(vec![leaf; MAX_SYMBOLS + 1]), Path::new("unused"), SOURCE, &budget()).is_err());
}

#[test]
fn display_caps_keep_whole_structured_symbols_and_full_totals() {
    let mut item = symbol("large", at(0,0), at(0,1));
    item["detail"] = json!("界".repeat(4000));
    let raw = json!(vec![item; 64]);
    let outline = parse(&raw, Path::new("unused"), SOURCE, &budget()).unwrap();
    let rows = displayed(&outline, 0..outline.symbols.len(), MAX_DISPLAY, &budget()).unwrap();
    assert_eq!(outline.symbols.len(), 64);
    assert!(!rows.is_empty() && rows.len() < 64);
    assert!(rows.iter().all(Value::is_object));
    assert_eq!(rows[0]["detail"].as_str().unwrap().len(), 12000);
    assert!(serde_json::to_vec(&rows).unwrap().len() < MAX_PAYLOAD_BYTES - 8192);
    assert_eq!(displayed(&outline, 0..outline.symbols.len(), 1, &budget()).unwrap().len(), 1);
}

#[test]
fn empty_responses_remain_empty_and_cancelled_validation_never_succeeds() {
    for raw in [Value::Null, json!([])] {
        assert!(parse(&raw, Path::new("unused"), SOURCE, &budget()).unwrap().symbols.is_empty());
    }
    let budget = budget();
    budget.owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel outline"));
    assert!(parse(&tree(), Path::new("unused"), SOURCE, &budget).is_err());
    assert!(validate_boundaries(SOURCE, &BTreeSet::new(), &budget).is_err());
}

// Real child stdio, including partial results followed by a terminal response.
// The wire journal and source stay in this test's private temporary directory.
const SERVER: &str = r"
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
def send(value):
    body = json.dumps(value, separators=(',', ':')).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
def point(line, char): return {'line': line, 'character': char}
def span(a, b, c, d): return {'start': point(a,b), 'end': point(c,d)}
def sym(name, line, start, end):
    return {'name':name, 'kind':12, 'range':span(line,start,line,end), 'selectionRange':span(line,start,line,end)}
def mode(): return (root / 'mode').read_text()
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\r\n', b'\n'): break
        key, value = line.decode('ascii').split(':', 1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    with (root / 'frames.jsonl').open('a') as log: log.write(json.dumps(message) + '\n')
    if 'id' not in message: continue
    reply = {'jsonrpc':'2.0', 'id':message['id']}
    method = message.get('method')
    if method == 'initialize':
        reply['result'] = {'capabilities':{'textDocumentSync':1, 'documentSymbolProvider':mode() != 'unsupported'}}
    elif method == 'textDocument/documentSymbol':
        first = {'name':'Box','kind':5,'range':span(0,0,2,1),'selectionRange':span(0,6,0,9),'children':[sym('run',1,2,5)]}
        second = sym('run',3,9,12)
        active = mode()
        if active == 'bad-tail': second['selectionRange'] = span(3,9,3,999)
        if active == 'flat':
            reply['result'] = [{'name':'run','kind':12,'containerName':'Box','location':{
                'uri':message['params']['textDocument']['uri'], 'range':span(1,2,1,10)}}]
        elif active == 'null': reply['result'] = None
        elif active == 'legacy': reply['result'] = [first, second]
        else:
            token = message['params'].get('partialResultToken')
            send({'jsonrpc':'2.0','method':'$/progress','params':{'token':'unrelated','value':{'kind':'report','message':'indexing'}}})
            send({'jsonrpc':'2.0','method':'$/progress','params':{'token':token,'value':[first]}})
            if active == 'hang': continue
            if active == 'error': reply['error'] = {'code':-32001,'message':'outline failed after progress'}
            else:
                fragment = {'invalid':'shape'} if active == 'malformed-fragment' else [second]
                send({'jsonrpc':'2.0','method':'$/progress','params':{'token':token,'value':fragment}})
                if active == 'drift': (root / 'source.pioutline').write_text('changed\n')
                reply['result'] = [second] if active == 'nonempty-terminal' else None
    else: reply['result'] = None
    send(reply)
";

struct Peer {
    tool: LspTool,
    runtime: asupersync::runtime::Runtime,
    root: tempfile::TempDir,
}

impl Peer {
    fn new(mode: &str) -> Option<Self> {
        use std::process::{Command, Stdio};
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program).arg("--version").stdout(Stdio::null()).stderr(Stdio::null())
                .status().is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(), "Python required for outline protocol tests");
            eprintln!("SKIP document outline peer: Python unavailable");
            return None;
        };
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        std::fs::write(root.path().join("source.pioutline"), SOURCE).unwrap();
        std::fs::write(root.path().join("mode"), mode).unwrap();
        let config: crate::config::Config = serde_json::from_value(json!({"lsp":{
            "enabled":true,"servers":{"outline-fixture":{
                "command":python,"args":["-I","-u","-c",SERVER,canonical],
                "extensions":[".pioutline"],"languages":["plaintext"],"rootMarkers":[]
            }}
        }})).unwrap();
        let tool = LspTool::new(&canonical, Some(&config));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        Some(Self { tool, runtime, root })
    }

    fn input(extra: Value) -> Value {
        let mut input = json!({"action":"symbols","file":"source.pioutline","timeout":5});
        input.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        input
    }

    fn run(&self, extra: Value) -> Result<ToolOutput> {
        self.runtime.block_on(self.tool.execute("outline", Self::input(extra), None))
    }

    fn frames(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.path().join("frames.jsonl")).unwrap_or_default()
            .lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    fn assert_read_only(&self) {
        assert_eq!(std::fs::read(self.root.path().join("source.pioutline")).unwrap(), SOURCE.as_bytes());
        assert!(!self.frames().iter().any(|frame| matches!(frame["method"].as_str(),
            Some("workspace/executeCommand" | "workspace/applyEdit" | "completionItem/resolve"))));
    }
}

#[test]
fn native_tool_assembles_hierarchical_and_flat_document_symbols() {
    for mode in ["stream", "legacy", "flat", "null"] {
        let Some(peer) = Peer::new(mode) else { return };
        let output = peer.run(json!({})).unwrap();
        assert!(!output.is_error);
        let details = output.details.unwrap();
        let payload = &details["payload"];
        assert!(payload.is_object());
        assert_eq!(payload["responseComplete"], true);
        assert_eq!(payload["truncated"], false);
        assert_eq!(payload["total"], match mode { "flat" => 1, "null" => 0, _ => 3 });
        if mode == "stream" {
            assert_eq!(payload["symbols"][1]["parentIndex"], 0);
            assert_eq!(payload["symbols"][2]["selectionRange"], span(at(3,9),at(3,12)));
        }
        let frames = peer.frames();
        let requests: Vec<_> = frames.iter().filter(|frame| frame["method"] == "textDocument/documentSymbol").collect();
        assert_eq!(requests.len(), 1);
        assert!(requests[0]["params"]["partialResultToken"].is_string());
        peer.assert_read_only();
    }
}

#[test]
fn native_tool_limits_only_display_and_rejects_invalid_tail_or_failed_stream() {
    let Some(peer) = Peer::new("stream") else { return };
    let output = peer.run(json!({"limit":1})).unwrap().details.unwrap();
    assert_eq!(output["payload"]["total"], 3);
    assert_eq!(output["payload"]["count"], 1);
    assert_eq!(output["truncated"], true);
    for mode in ["bad-tail", "malformed-fragment", "nonempty-terminal", "error"] {
        std::fs::write(peer.root.path().join("mode"), mode).unwrap();
        assert!(peer.run(json!({"limit":1})).is_err(), "{mode}");
        peer.assert_read_only();
    }
    std::fs::write(peer.root.path().join("mode"), "stream").unwrap();
    assert_eq!(peer.run(json!({})).unwrap().details.unwrap()["payload"]["total"], 3);
}

#[test]
fn source_drift_and_unsupported_servers_do_not_return_successful_outlines() {
    let Some(peer) = Peer::new("drift") else { return };
    let error = peer.run(json!({})).unwrap_err();
    assert!(error.to_string().contains("LSP_SEMANTIC_STALE"), "{error}");
    let Some(peer) = Peer::new("unsupported") else { return };
    let error = peer.run(json!({})).unwrap_err();
    assert!(error.to_string().contains("LSP_UNSUPPORTED"), "{error}");
    assert!(!peer.frames().iter().any(|frame| frame["method"] == "textDocument/documentSymbol"));
}

#[test]
fn invalid_outline_selectors_do_not_start_a_server() {
    let Some(peer) = Peer::new("stream") else { return };
    for extra in [json!({"apply":true}), json!({"apply":false}), json!({"limit":0}),
        json!({"resolve":true}), json!({"method":"workspace/executeCommand"}),
        json!({"range":span(at(0,0),at(0,1))})]
    {
        assert!(peer.run(extra).is_err());
    }
    assert!(peer.frames().is_empty());
    peer.assert_read_only();
}

#[test]
fn abandoning_a_streamed_outline_releases_the_workflow_and_discards_fragments() {
    let Some(peer) = Peer::new("hang") else { return };
    peer.runtime.block_on(async {
        let mut pending = Box::pin(peer.tool.execute("abandon", Peer::input(json!({})), None));
        let started = std::time::Instant::now();
        loop {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if peer.frames().iter().any(|frame| frame["method"] == "textDocument/documentSymbol") { break; }
            assert!(started.elapsed() < Duration::from_secs(3), "request never reached peer");
            crate::agent_cx::AgentCx::for_current_or_request().time().sleep(Duration::from_millis(10)).await;
        }
        drop(pending);
    });
    std::fs::write(peer.root.path().join("mode"), "stream").unwrap();
    let output = peer.run(json!({})).unwrap().details.unwrap();
    assert_eq!(output["payload"]["total"], 3, "abandoned fragments must not enter the next outline");
    let frames = peer.frames();
    let requests: Vec<_> = frames.iter().filter(|frame| frame["method"] == "textDocument/documentSymbol").collect();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0]["params"]["partialResultToken"], requests[1]["params"]["partialResultToken"]);
    let cancelled: Vec<_> = frames.iter().filter(|frame| frame["method"] == "$/cancelRequest").collect();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0]["params"]["id"], requests[0]["id"]);
    peer.assert_read_only();
}

fn filter(extra: Value) -> Filter {
    let input: LspInput = serde_json::from_value(Peer::input(extra)).unwrap();
    Filter::parse(&input).unwrap()
}

#[test]
fn semantic_name_filter_preserves_ancestors_and_ordinal_identity() {
    let raw = tree();
    let outline = parse(&raw, Path::new("unused"), SOURCE, &budget()).unwrap();
    let matching = select(&outline, &filter(json!({"symbol":"run"})), &budget()).unwrap();
    assert_eq!(matching.matched, 2);
    assert_eq!(matching.indices, vec![1, 2]);
    let rows = displayed(&outline, matching.indices.iter().copied(), 100, &budget()).unwrap();
    assert_eq!(rows[0]["index"], 1);
    assert_eq!(rows[0]["parentIndex"], 0);
    assert_eq!(rows[0]["ancestors"][0]["index"], 0);
    assert_eq!(rows[0]["ancestors"][0]["name"], "Box");
    assert_eq!(rows[0]["ancestors"][0]["selectionRange"], span(at(0,6), at(0,9)));
    assert_eq!(rows[1]["ancestors"], json!([]));
    let second = select(&outline, &filter(json!({"symbol":"run#2"})), &budget()).unwrap();
    assert_eq!(second.matched, 2);
    assert_eq!(second.indices, vec![2]);
    assert!(select(&outline, &filter(json!({"symbol":"run#3"})), &budget()).is_err());
    assert!(select(&outline, &filter(json!({"symbol":"Run"})), &budget()).unwrap().indices.is_empty());
}

#[test]
fn line_filter_respects_half_open_endpoints_and_keeps_enclosing_definitions() {
    let raw = tree();
    let outline = parse(&raw, Path::new("unused"), SOURCE, &budget()).unwrap();
    for (line, expected) in [(1, vec![0]), (2, vec![0, 1]), (3, vec![0]), (4, vec![2]), (5, vec![])] {
        let filter = filter(json!({"line":line}));
        filter.validate_source(SOURCE).unwrap();
        assert_eq!(select(&outline, &filter, &budget()).unwrap().indices, expected);
    }
    let end_at_next_line = Range { start:at(0,0), end:at(1,0) };
    assert!(intersects_line(end_at_next_line, 0));
    assert!(!intersects_line(end_at_next_line, 1));
    assert!(intersects_line(Range { start:at(1,0), end:at(1,0) }, 1));
    assert!(!intersects_line(Range { start:at(1,0), end:at(1,0) }, 0));
    assert!(filter(json!({"line":6})).validate_source(SOURCE).is_err());
}

#[test]
fn line_and_name_intersect_before_numbered_definition_selection() {
    let raw = tree();
    let outline = parse(&raw, Path::new("unused"), SOURCE, &budget()).unwrap();
    let matching = select(&outline, &filter(json!({"symbol":"run#1","line":4})), &budget()).unwrap();
    assert_eq!(matching.matched, 1);
    assert_eq!(matching.indices, vec![2]);
    assert!(select(&outline, &filter(json!({"symbol":"run#2","line":4})), &budget()).is_err());
    assert!(select(&outline, &filter(json!({"symbol":"Box","line":4})), &budget()).unwrap().indices.is_empty());
}

#[test]
fn filter_is_bounded_literal_matching_and_does_not_validate_only_selected_nodes() {
    for raw in ["", " ", "#1", "   #2"] {
        let input: LspInput = serde_json::from_value(Peer::input(json!({"symbol":raw}))).unwrap();
        // A leading # has no nonempty name and is an ordinary literal under
        // the shared selector grammar; it does not mean select any symbol.
        if raw == "#1" {
            assert_eq!(Filter::parse(&input).unwrap().name.as_deref(), Some("#1"));
        } else {
            assert!(Filter::parse(&input).is_err());
        }
    }
    for extra in [json!({"symbol":"界".repeat(342)}), json!({"line":0}), json!({"after":"cursor"}),
        json!({"snippetValues":{}}), json!({"only":[]}), json!({"refactorId":"old"})]
    {
        let input: LspInput = serde_json::from_value(Peer::input(extra)).unwrap();
        assert!(Filter::parse(&input).is_err());
    }
    let raw = json!([
        symbol("a.*", at(0,0), at(0,1)), symbol("ab", at(0,0), at(0,1)),
        symbol("日本語", at(0,0), at(0,1))
    ]);
    let outline = parse(&raw, Path::new("unused"), SOURCE, &budget()).unwrap();
    assert_eq!(select(&outline, &filter(json!({"symbol":"a.*"})), &budget()).unwrap().indices, vec![0]);
    assert_eq!(select(&outline, &filter(json!({"symbol":"日本"})), &budget()).unwrap().indices, vec![2]);
    let bad_tail = json!([raw[0], symbol("not-matched", at(99,0), at(99,1))]);
    assert!(parse(&bad_tail, Path::new("unused"), SOURCE, &budget()).is_err());
}

#[test]
fn native_definition_search_ignores_source_hits_and_reports_filter_totals() {
    let Some(peer) = Peer::new("stream") else { return };
    // These textual matches are not definitions and must never affect #N.
    let original = format!("{SOURCE}// run run run\r\n");
    std::fs::write(peer.root.path().join("source.pioutline"), &original).unwrap();
    let limited = peer.run(json!({"symbol":"run","limit":1})).unwrap().details.unwrap();
    assert_eq!(limited["payload"]["total"], 3);
    assert_eq!(limited["payload"]["matched"], 2);
    assert_eq!(limited["payload"]["selectedCount"], 2);
    assert_eq!(limited["payload"]["count"], 1);
    assert_eq!(limited["truncated"], true);
    assert_eq!(limited["payload"]["symbols"][0]["ancestors"][0]["name"], "Box");
    let second = peer.run(json!({"symbol":"run#2","limit":1})).unwrap().details.unwrap();
    assert_eq!(second["payload"]["matched"], 2);
    assert_eq!(second["payload"]["selectedCount"], 1);
    assert_eq!(second["truncated"], false, "intentional filtering is not truncation");
    assert_eq!(second["payload"]["symbols"][0]["index"], 2);
    assert_eq!(second["payload"]["symbols"][0]["selectionRange"], span(at(3,9),at(3,12)));
    let empty = peer.run(json!({"symbol":"run","line":5})).unwrap().details.unwrap();
    assert_eq!(empty["payload"]["total"], 3);
    assert_eq!(empty["payload"]["matched"], 0);
    assert_eq!(empty["truncated"], false);
    assert_eq!(std::fs::read_to_string(peer.root.path().join("source.pioutline")).unwrap(), original);
    assert!(!peer.frames().iter().any(|frame| matches!(frame["method"].as_str(),
        Some("workspace/symbol" | "workspace/executeCommand" | "completionItem/resolve"))));
}

#[test]
fn native_enclosing_line_search_disambiguates_names_and_rejects_bad_hidden_tail() {
    let Some(peer) = Peer::new("stream") else { return };
    let enclosing = peer.run(json!({"line":2})).unwrap().details.unwrap();
    assert_eq!(enclosing["payload"]["matched"], 2);
    assert_eq!(enclosing["payload"]["symbols"][0]["name"], "Box");
    assert_eq!(enclosing["payload"]["symbols"][1]["name"], "run");
    let intersected = peer.run(json!({"symbol":"run#1","line":4})).unwrap().details.unwrap();
    assert_eq!(intersected["payload"]["matched"], 1);
    assert_eq!(intersected["payload"]["symbols"][0]["index"], 2);
    assert!(peer.run(json!({"symbol":"run#2","line":4})).is_err());
    std::fs::write(peer.root.path().join("mode"), "bad-tail").unwrap();
    assert!(peer.run(json!({"symbol":"Box","limit":1})).is_err(), "a filtered-out invalid method must still fail the outline");
    peer.assert_read_only();
    let Some(peer) = Peer::new("stream") else { return };
    assert!(peer.run(json!({"line":6})).is_err());
    assert!(peer.frames().is_empty(), "reject nonexistent lines before server startup");
}
