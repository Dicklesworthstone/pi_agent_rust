//! Native tool-to-stdio coverage for source-bound semantic definition reads.

#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use asupersync::runtime::{Runtime, RuntimeBuilder};
use pi::config::Config;
use pi::lsp::LspTool;
use pi::lsp::text::{Position, position_to_offset_exact};
use pi::tools::Tool as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::process::{Command, Stdio};

const SOURCE: &str = "// run is a comment, not a definition\r\nclass Box {\r\n  run() {\r\n    return \"日本😀\";\r\n  }\r\n}\r\nfunction run() { return 2; }\r\n";

const SERVER: &str = r"
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
text = ''
def send(message):
    body = json.dumps(message, ensure_ascii=False, separators=(',', ':')).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
def point(offset):
    before = text[:offset].replace('\r\n','\n').replace('\r','\n')
    line = before.count('\n')
    column = len(before.rsplit('\n',1)[-1].encode('utf-16-le')) // 2
    return {'line':line,'character':column}
def span(start, end): return {'start':point(start),'end':point(end)}
def symbol(name, kind, start, end, selected):
    return {'name':name,'kind':kind,'range':span(start,end),'selectionRange':span(selected,selected+len(name))}
while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\r\n',b'\n'): break
        key, value = line.decode('ascii').split(':',1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    with (root / 'frames.jsonl').open('a') as log:
        log.write(json.dumps(message) + '\n')
    method = message.get('method')
    if method == 'textDocument/didOpen': text = message['params']['textDocument']['text']
    if 'id' not in message: continue
    reply = {'jsonrpc':'2.0','id':message['id']}
    if method == 'initialize':
        reply['result'] = {'capabilities':{'textDocumentSync':1,'documentSymbolProvider':True}}
    elif method == 'textDocument/documentSymbol':
        mode = (root / 'mode').read_text()
        class_start = text.index('class Box')
        function_start = text.index('function run')
        method_start = text.index('  run()') + 2
        method_end = text.index('\r\n  }',method_start) + len('\r\n  }')
        first = symbol('Box',5,class_start,text.index('\r\n}',method_end)+3,class_start+6)
        first['children'] = [symbol('run',6,method_start,method_end,method_start)]
        second = symbol('run',12,function_start,len(text.rstrip('\r\n')),function_start+9)
        if mode == 'flat':
            reply['result'] = [{'name':'run','kind':6,'containerName':'Box','location':{
                'uri':message['params']['textDocument']['uri'],'range':span(method_start,method_start+3)}}]
        else:
            if mode == 'bad-tail': second['selectionRange']['end']['character'] = 999999
            if mode == 'drift':
                (root / 'source.pisource').write_text(text.replace('return 2','return 3'))
            if mode == 'legacy': reply['result'] = [first, second]
            else:
                token = message['params']['partialResultToken']
                for fragment in [[first], [], [second]]:
                    send({'jsonrpc':'2.0','method':'$/progress','params':{'token':token,'value':fragment}})
                reply['result'] = None
    else: reply['result'] = None
    send(reply)
";

struct Peer {
    tool: LspTool,
    runtime: Runtime,
    root: tempfile::TempDir,
}

impl Peer {
    fn new(mode: &str, text: &str) -> Option<Self> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            assert!(
                std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
                "Python required for semantic source protocol tests"
            );
            eprintln!("SKIP semantic source peer: Python unavailable");
            return None;
        };
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        std::fs::write(root.path().join("source.pisource"), text).unwrap();
        std::fs::write(root.path().join("mode"), mode).unwrap();
        let config: Config = serde_json::from_value(json!({"lsp":{
            "enabled":true,"servers":{"source-fixture":{
                "command":python,"args":["-I","-u","-c",SERVER,canonical],
                "extensions":[".pisource"],"languages":["plaintext"],"rootMarkers":[]
            }}
        }}))
        .unwrap();
        Some(Self {
            tool: LspTool::new(&canonical, Some(&config)),
            runtime: RuntimeBuilder::current_thread().build().unwrap(),
            root,
        })
    }

    fn run(&self, extra: Value) -> pi::PiResult<Value> {
        let mut input = json!({"action":"symbols","file":"source.pisource","timeout":5});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let output = self
            .runtime
            .block_on(self.tool.execute("symbol-source", input, None))?;
        assert!(!output.is_error);
        Ok(output.details.unwrap()["payload"].clone())
    }

    fn frames(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.path().join("frames.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn assert_exact_source(row: &Value, original: &str) {
    let source = &row["source"];
    let start: Position = serde_json::from_value(source["range"]["start"].clone()).unwrap();
    let end: Position = serde_json::from_value(source["range"]["end"].clone()).unwrap();
    let first = position_to_offset_exact(original, start).unwrap();
    let last = position_to_offset_exact(original, end).unwrap();
    assert_eq!(source["text"], &original[first..last]);
    assert_eq!(source["byteRange"], json!({"start":first,"end":last}));
}

#[test]
fn selected_definitions_return_implementation_not_comments_or_name_only() {
    for mode in ["stream", "legacy"] {
        let Some(peer) = Peer::new(mode, SOURCE) else {
            return;
        };
        let output = peer.run(json!({"symbol":"run#1"})).unwrap();
        assert_eq!(output["count"], 1);
        assert_eq!(output["matched"], 2);
        assert_eq!(output["sourceIncluded"], true);
        assert_eq!(output["sourceTruncated"], false);
        assert_eq!(
            output["sourceSha256"],
            pi::package_manager::hex_encode(&Sha256::digest(SOURCE.as_bytes()))
        );
        assert_eq!(
            output["symbols"][0]["source"]["text"],
            "run() {\r\n    return \"日本😀\";\r\n  }"
        );
        assert_eq!(output["symbols"][0]["ancestors"][0]["name"], "Box");
        assert_exact_source(&output["symbols"][0], SOURCE);
        assert_eq!(
            std::fs::read(peer.root.path().join("source.pisource")).unwrap(),
            SOURCE.as_bytes()
        );
        let frames = peer.frames();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["method"] == "textDocument/documentSymbol")
                .count(),
            1
        );
        assert!(
            !frames
                .iter()
                .any(|frame| frame["method"] == "workspace/executeCommand")
        );
    }
}

#[test]
fn unfiltered_outline_stays_small_and_line_selection_reads_containing_code() {
    let Some(peer) = Peer::new("stream", SOURCE) else {
        return;
    };
    let outline = peer.run(json!({})).unwrap();
    assert_eq!(outline["count"], 3);
    assert_eq!(outline["sourceIncluded"], false);
    assert!(outline["sourceSha256"].is_null());
    assert!(
        outline["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("source").is_none())
    );
    let selected = peer.run(json!({"symbol":"run","line":4})).unwrap();
    assert_eq!(selected["count"], 1);
    assert_eq!(selected["truncated"], false);
    assert_exact_source(&selected["symbols"][0], SOURCE);
    assert!(
        selected["symbols"][0]["source"]["text"]
            .as_str()
            .unwrap()
            .contains("日本😀")
    );
}

#[test]
fn large_bodies_are_explicitly_clipped_without_losing_exact_range_or_counts() {
    let text = SOURCE.replace("日本😀", &"😀界".repeat(6000));
    let Some(peer) = Peer::new("stream", &text) else {
        return;
    };
    let output = peer.run(json!({"symbol":"run#1"})).unwrap();
    assert_eq!(output["count"], 1);
    assert_eq!(output["truncated"], false, "no symbol was omitted");
    assert_eq!(output["sourceTruncated"], true, "code omission is separate");
    let source = &output["symbols"][0]["source"];
    let bytes = source["text"].as_str().unwrap().len();
    assert!(bytes <= 16 * 1024);
    assert!(source["omittedBytes"].as_u64().unwrap() > 0);
    assert_eq!(
        u64::try_from(bytes).unwrap() + source["omittedBytes"].as_u64().unwrap(),
        source["totalBytes"].as_u64().unwrap()
    );
    assert_exact_source(&output["symbols"][0], &text);
}

#[test]
fn flat_ranges_remain_literal_and_invalid_or_changed_sources_produce_no_excerpt() {
    let Some(peer) = Peer::new("flat", SOURCE) else {
        return;
    };
    let output = peer.run(json!({"symbol":"run"})).unwrap();
    assert_eq!(output["symbols"][0]["source"]["text"], "run");
    assert_eq!(
        output["symbols"][0]["source"]["extent"],
        "symbol_location_range"
    );
    assert_exact_source(&output["symbols"][0], SOURCE);
    for mode in ["bad-tail", "drift"] {
        let Some(peer) = Peer::new(mode, SOURCE) else {
            return;
        };
        assert!(
            peer.run(json!({"symbol":"Box","limit":1})).is_err(),
            "{mode} cannot return partial source success"
        );
    }
}

#[test]
fn line_focused_read_reaches_the_end_of_a_large_method_and_its_containing_class() {
    let text = SOURCE.replace(
        "    return",
        &format!("{}    return", "    padding();\r\n".repeat(3000)),
    );
    let focus = text.find("    return").unwrap();
    let line = pi::lsp::text::offset_to_position(&text, focus)
        .unwrap()
        .line
        + 1;
    let Some(peer) = Peer::new("stream", &text) else {
        return;
    };
    let output = peer.run(json!({"line":line})).unwrap();
    assert_eq!(
        output["count"], 2,
        "the class and its method both contain the line"
    );
    assert_eq!(output["sourceTruncated"], true);
    assert_eq!(output["truncated"], false);
    for row in output["symbols"].as_array().unwrap() {
        let source = &row["source"];
        assert!(
            source["text"]
                .as_str()
                .unwrap()
                .contains("return \"日本😀\";")
        );
        assert_eq!(source["mode"], "line_context");
        assert_eq!(source["focusLine"], line);
        assert!(source["omittedBeforeBytes"].as_u64().unwrap() > 0);
        assert_eq!(source["omittedAfterBytes"], 0);
        assert_exact_source(row, &text);
    }
    assert_eq!(
        std::fs::read(peer.root.path().join("source.pisource")).unwrap(),
        text.as_bytes()
    );
    assert_eq!(
        peer.frames()
            .iter()
            .filter(|frame| frame["method"] == "textDocument/documentSymbol")
            .count(),
        1
    );
}

#[test]
fn repeated_line_reads_keep_exact_context_and_preserve_source_fingerprints() {
    let padding = "    padding();\r\n".repeat(4000);
    let text = SOURCE.replace("    return", &format!("{padding}    return"));
    let Some(peer) = Peer::new("legacy", &text) else {
        return;
    };
    let early = peer.run(json!({"symbol":"run","line":10})).unwrap();
    let late = peer.run(json!({"symbol":"run","line":3900})).unwrap();
    assert_eq!(early["count"], 1);
    assert_eq!(late["count"], 1);
    assert_eq!(early["sourceSha256"], late["sourceSha256"]);
    for output in [&early, &late] {
        let source = &output["symbols"][0]["source"];
        assert_exact_source(&output["symbols"][0], &text);
        assert!(source["text"].as_str().unwrap().len() <= 16 * 1024);
        assert_eq!(
            source["omittedBeforeBytes"].as_u64().unwrap()
                + source["omittedAfterBytes"].as_u64().unwrap(),
            source["omittedBytes"].as_u64().unwrap()
        );
    }
    assert!(
        early["symbols"][0]["source"]["omittedAfterBytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        late["symbols"][0]["source"]["omittedBeforeBytes"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_ne!(
        early["symbols"][0]["source"]["byteRange"],
        late["symbols"][0]["source"]["byteRange"]
    );
}
