//! Selected code-action review over the real tool and framed child transport.
//! The peer supplies edits, not a substitute implementation of the Rust client.

use super::*;
use crate::config::Config;
use crate::tools::Tool as _;
use std::process::{Command, Stdio};

const BEFORE: &str = "left + right\n";
const AFTER: &str = "extracted()\nfn extracted() { left + right }\n";
const SERVER: &str = r"
import copy, json, pathlib, sys
root = pathlib.Path.cwd()
mode = sys.argv[1]
versions = {}
stats = {'lists': 0, 'resolves': 0, 'commands': 0, 'probe': None}
pending = None

def send(value):
    body = json.dumps(value).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()

def reply(message, result):
    send({'jsonrpc': '2.0', 'id': message['id'], 'result': result})

def edit(uri, version, start, end, text):
    return {'textDocument': {'uri': uri, 'version': version}, 'edits': [{
        'range': {'start': {'line': start[0], 'character': start[1]},
                  'end': {'line': end[0], 'character': end[1]}}, 'newText': text}]}

def workspace(source):
    version = versions.get(source)
    steps = [edit(source, version, (0, 0), (0, 12), 'extracted()')]
    steps[0]['edits'] += edit(source, version, (1, 0), (1, 0),
        'fn extracted() { left + right }\n')['edits']
    sibling = (root / 'sibling.review').as_uri()
    steps += [edit(sibling, None, (0, 0), (0, 8), 'use extracted;')]
    if mode == 'guard': steps = steps[1:]
    if mode == 'stale': steps[0]['textDocument']['version'] = version + 1
    if mode == 'unknown': steps[1]['textDocument']['version'] = 1000
    if mode == 'malformed': return {'changes': 42}
    if mode == 'escape': steps[1]['textDocument']['uri'] = (root.parent / 'outside.review').as_uri()
    if mode == 'oversized': steps[0]['edits'][0]['newText'] = 'x' * 210000
    return {'documentChanges': steps}

def resolved(item):
    result = copy.deepcopy(item)
    result['edit'] = workspace(item['data']['source'])
    if mode in ('command', 'lazy-command'):
        result['command'] = {'title': 'After extraction', 'command': 'test.after'}
    if mode == 'changed': result['title'] = 'A different selection'
    if mode == 'drift': (root / 'source.review').write_text('external\n', encoding='utf-8')
    return result

while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line in (b'\r\n', b'\n'): break
        key, value = line.decode('ascii').split(':', 1)
        headers[key.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    with (root / 'review-requests.jsonl').open('a', encoding='utf-8') as log:
        log.write(json.dumps(message) + '\n')
    method = message.get('method')
    params = message.get('params') or {}
    if not method and message.get('id') == 'edit-probe':
        stats['probe'] = message.get('result')
        reply(pending, resolved(pending['params']))
        pending = None
        continue
    if method == 'exit': sys.exit(0)
    if method == 'textDocument/didOpen':
        doc = params['textDocument']
        versions[doc['uri']] = doc['version']
    if 'id' not in message: continue
    if method == 'initialize':
        reply(message, {'capabilities': {'textDocumentSync': 0 if mode == 'nosync' else 1,
            'codeActionProvider': {'resolveProvider': True}}})
    elif method == 'textDocument/codeAction':
        stats['lists'] += 1
        source = params['textDocument']['uri']
        stats['selection'] = params['range']
        stats['only'] = params['context'].get('only')
        item = {'title': 'Extract selected expression', 'kind': 'refactor.extract.function',
            'data': {'source': source, 'opaque': {'nested': [1, 'kept']}}}
        if mode in ('inline', 'command'): item = resolved(item)
        reply(message, [{'title': 'Other fix', 'kind': 'quickfix', 'command': 'test.unrelated'}, item])
    elif method == 'codeAction/resolve':
        stats['resolves'] += 1
        stats['resolvePayload'] = copy.deepcopy(params)
        if mode == 'error':
            send({'jsonrpc': '2.0', 'id': message['id'], 'error': {'code': -32603, 'message': 'resolution failed'}})
        elif mode == 'hang': pass
        elif mode == 'probe':
            pending = message
            send({'jsonrpc': '2.0', 'id': 'edit-probe', 'method': 'workspace/applyEdit',
                'params': {'edit': workspace(params['data']['source'])}})
        else: reply(message, resolved(params))
    elif method == 'workspace/executeCommand':
        stats['commands'] += 1
        stats['sourceAtCommand'] = (root / 'source.review').read_text(encoding='utf-8')
        reply(message, None)
    elif method == 'test/barrier': reply(message, stats)
    else: reply(message, None)
";

struct Fixture {
    root: PathBuf,
    tool: LspTool,
    runtime: asupersync::runtime::Runtime,
}

impl Fixture {
    fn new(root: &Path, mode: &str) -> Option<Self> {
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
                "Python is required for the selected code-action review tests"
            );
            eprintln!("SKIP code-action review peer: Python unavailable");
            return None;
        };
        std::fs::create_dir_all(root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("source.review"), BEFORE).unwrap();
        std::fs::write(root.join("sibling.review"), "use old;\n").unwrap();
        std::fs::write(root.join(".review-root"), "").unwrap();
        let script = root.join("review_peer.py");
        std::fs::write(&script, SERVER).unwrap();
        let config: Config = serde_json::from_value(json!({"lsp":{"servers":{"review-peer":{
            "command":python,"args":["-I","-u",script.to_str().unwrap(),mode],
            "extensions":[".review"],"languages":["plaintext"],"rootMarkers":[".review-root"]
        }}}}))
        .unwrap();
        let tool = LspTool::new(&root, Some(&config));
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .worker_threads(1)
            .enable_parking(false)
            .build()
            .unwrap();
        Some(Self {
            root,
            tool,
            runtime,
        })
    }

    fn run(&self, input: Value) -> Result<Value> {
        self.runtime
            .block_on(self.tool.execute("review-action", input, None))
            .map(|output| {
                assert!(!output.is_error);
                output.details.unwrap()
            })
    }

    fn list(&self) -> Value {
        self.run(json!({"action":"code_actions","file":"source.review",
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":12}},
            "only":["refactor.extract"],"timeout":3}))
            .unwrap()
    }

    fn preview(&self) -> Value {
        let listing = self.list();
        assert_eq!(listing["count"], 1);
        self.run(json!({"action":"code_actions","actionId":listing["actions"][0]["actionId"],"timeout":3})).unwrap()
    }

    fn approve(&self, preview: &Value) -> Result<Value> {
        self.run(json!({"action":"code_actions","refactorId":preview["refactorId"],"apply":true,"timeout":3}))
    }

    fn text(&self, file: &str) -> String {
        std::fs::read_to_string(self.root.join(file)).unwrap()
    }

    fn barrier(&self) -> Value {
        self.run(
            json!({"action":"request","file":"source.review","method":"test/barrier","timeout":3}),
        )
        .unwrap()["payload"]["result"]
            .clone()
    }
}

#[test]
fn review_selection_accepts_cached_and_fresh_choices_without_apply() {
    for raw in [
        json!({"action":"code_actions","actionId":"chosen"}),
        json!({"action":"code_actions","actionId":"chosen","apply":false}),
        json!({"action":"code_actions","file":"source.review","query":"Extract"}),
        json!({"action":"code_actions","file":"source.review","query":"1","apply":false}),
        json!({"action":"code_actions","actionId":"chosen","apply":true}),
    ] {
        let input: LspInput = serde_json::from_value(raw).unwrap();
        validate_action_request(&input).unwrap();
    }
    for raw in [
        json!({"action":"code_actions","actionId":""}),
        json!({"action":"code_actions","actionId":"x".repeat(129)}),
        json!({"action":"code_actions","actionId":"chosen","query":"1"}),
        json!({"action":"code_actions","query":"  "}),
        json!({"action":"code_actions","apply":true}),
    ] {
        let input: LspInput = serde_json::from_value(raw).unwrap();
        assert!(validate_action_request(&input).is_err());
    }
}

#[test]
fn extract_review_and_approval_do_not_repeat_listing_or_resolution() {
    for mode in ["lazy", "inline"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(f) = Fixture::new(temp.path(), mode) else {
            return;
        };
        let preview = f.preview();
        assert_eq!(preview["preview"], true);
        assert_eq!(preview["applied"], false);
        assert_eq!(preview["title"], "Extract selected expression");
        assert_eq!(preview["kind"], "refactor.extract.function");
        assert_eq!(
            preview["workspaceEdit"]["documentChanges"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(f.text("source.review"), BEFORE);
        assert_eq!(f.text("sibling.review"), "use old;\n");
        let inspected = f
            .run(json!({"action":"code_actions","refactorId":preview["refactorId"]}))
            .unwrap();
        assert_eq!(inspected["workspaceEdit"], preview["workspaceEdit"]);
        let applied = f.approve(&preview).unwrap();
        assert_eq!(applied["applied"], true);
        assert_eq!(applied["title"], preview["title"]);
        assert!(applied["executedCommand"].is_null());
        assert_eq!(f.text("source.review"), AFTER);
        assert_eq!(f.text("sibling.review"), "use extracted;\n");
        assert!(f.approve(&preview).is_err());
        let stats = f.barrier();
        assert_eq!(stats["lists"], 1);
        assert_eq!(stats["resolves"], u64::from(mode == "lazy"));
        assert_eq!(stats["commands"], 0);
        if mode == "lazy" {
            assert_eq!(
                stats["resolvePayload"]["data"]["opaque"],
                json!({"nested":[1,"kept"]})
            );
        }
    }
}

#[test]
fn fresh_filtered_query_previews_only_the_selected_action() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "lazy") else {
        return;
    };
    let preview = f
        .run(json!({"action":"code_actions","file":"source.review",
        "only":["refactor.extract"],"query":"1","apply":false,"timeout":3}))
        .unwrap();
    assert_eq!(preview["title"], "Extract selected expression");
    assert_eq!(f.text("source.review"), BEFORE);
    f.approve(&preview).unwrap();
    assert_eq!(f.text("source.review"), AFTER);
    assert_eq!(f.barrier()["only"], json!(["refactor.extract"]));
}

#[test]
fn command_backed_actions_are_never_presented_as_complete_frozen_edits() {
    for mode in ["command", "lazy-command"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(f) = Fixture::new(temp.path(), mode) else {
            return;
        };
        let listing = f.list();
        let error = f
            .run(json!({"action":"code_actions","actionId":listing["actions"][0]["actionId"]}))
            .unwrap_err();
        assert!(
            error.to_string().contains("LSP_ACTION_NOT_PREVIEWABLE"),
            "{error}"
        );
        assert_eq!(f.text("source.review"), BEFORE);
        assert_eq!(f.text("sibling.review"), "use old;\n");
        assert!(lock(&f.tool.actions.active).is_none());
        assert_eq!(f.barrier()["commands"], 0);
        // Existing explicit direct application still applies edits before command.
        let listing = f.list();
        let applied = f.run(json!({"action":"code_actions","actionId":listing["actions"][0]["actionId"],"apply":true})).unwrap();
        assert_eq!(applied["executedCommand"], "test.after");
        let stats = f.barrier();
        assert_eq!(stats["commands"], 1);
        assert_eq!(stats["sourceAtCommand"], AFTER);
    }
}

#[test]
fn unchanged_bytes_cannot_reuse_a_closed_source_before_preview_resolution() {
    for mode in ["lazy", "nosync"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(f) = Fixture::new(temp.path(), mode) else {
            return;
        };
        let listing = f.list();
        let path = f.root.join("source.review");
        let (uri, entry) = f.runtime.block_on(f.tool.synced(&path)).unwrap();
        entry.client.invalidate(&uri);
        entry.client.ensure_synced(&path, "plaintext").unwrap();
        let error = f
            .run(json!({"action":"code_actions","actionId":listing["actions"][0]["actionId"]}))
            .unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
        assert_eq!(f.text("source.review"), BEFORE);
        assert_eq!(f.barrier()["resolves"], 0);
    }
}

#[test]
fn reviewed_extract_rejects_unopened_sibling_and_guard_only_source_drift() {
    for (mode, changed) in [("lazy", "sibling.review"), ("guard", "source.review")] {
        let temp = tempfile::tempdir().unwrap();
        let Some(f) = Fixture::new(temp.path(), mode) else {
            return;
        };
        let preview = f.preview();
        std::fs::write(f.root.join(changed), "external\n").unwrap();
        let error = f.approve(&preview).unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
        assert_eq!(f.text(changed), "external\n");
        let other = if changed == "source.review" {
            "sibling.review"
        } else {
            "source.review"
        };
        assert_eq!(
            f.text(other),
            if other == "source.review" {
                BEFORE
            } else {
                "use old;\n"
            }
        );
        assert!(f.approve(&preview).is_err());
        assert_eq!(f.barrier()["resolves"], 1);
    }
}

#[test]
fn preview_resolution_does_not_authorize_unsolicited_edits() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "probe") else {
        return;
    };
    let preview = f.preview();
    assert_eq!(f.barrier()["probe"]["applied"], false);
    assert_eq!(f.text("source.review"), BEFORE);
    assert_eq!(f.text("sibling.review"), "use old;\n");
    assert!(lock(&f.tool.actions.active).is_none());
    f.approve(&preview).unwrap();
    assert_eq!(f.text("source.review"), AFTER);
}

#[test]
fn wrong_action_and_overriding_review_selectors_preserve_the_valid_plan() {
    let temp = tempfile::tempdir().unwrap();
    let Some(f) = Fixture::new(temp.path(), "lazy") else {
        return;
    };
    let preview = f.preview();
    for raw in [
        json!({"action":"rename","refactorId":preview["refactorId"],"apply":true}),
        json!({"action":"code_actions","refactorId":preview["refactorId"],"query":"Other","apply":true}),
        json!({"action":"code_actions","refactorId":preview["refactorId"],"actionId":"other","apply":true}),
    ] {
        assert!(f.run(raw).unwrap_err().to_string().contains("LSP_USAGE"));
    }
    f.approve(&preview).unwrap();
    assert_eq!(f.text("source.review"), AFTER);
}
