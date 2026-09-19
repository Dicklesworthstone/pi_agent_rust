//! Exercise version lineage through real tool dispatch and framed child stdio.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::config::{Config, LspServerSettings, LspSettings};
use crate::lsp::LspTool;
use crate::tools::Tool as _;
use serde_json::{Value, json};
use std::collections::HashMap;

const SERVER: &str = r"
import json, pathlib, sys
root = pathlib.Path.cwd()
mode = sys.argv[1]
versions = {}
def send(message):
    body = json.dumps(message).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
def edit(uri, version, text='renamed', end=3):
    return {'textDocument': {'uri': uri, 'version': version}, 'edits': [{
        'range': {'start': {'line': 0, 'character': 0}, 'end': {'line': 0, 'character': end}},
        'newText': text}]}
def move(old, new):
    return {'kind': 'rename', 'oldUri': old, 'newUri': new}
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
    method = message.get('method')
    params = message.get('params') or {}
    with (root / 'requests.jsonl').open('a', encoding='utf-8') as log:
        log.write(json.dumps({'case': mode, 'method': method, 'id': message.get('id')}) + '\n')
    if method == 'exit':
        sys.exit(0)
    if method == 'textDocument/didOpen':
        doc = params['textDocument']
        versions[doc['uri']] = doc['version']
    if 'id' not in message:
        continue
    result = None
    if method == 'initialize':
        result = {'capabilities': {'textDocumentSync': 1, 'renameProvider': True}}
    elif method == 'textDocument/rename':
        source = params['textDocument']['uri']
        version = versions[source]
        moved = (root / 'moved.identity').as_uri()
        steps = [move(source, moved), edit(moved, version)]
        if mode == 'chain':
            final = (root / 'final.identity').as_uri()
            steps += [move(moved, final), edit(final, version, 'complete', 7)]
        elif mode == 'overwrite':
            sibling = (root / 'sibling.identity').as_uri()
            op = move(source, sibling)
            op['options'] = {'overwrite': True}
            steps = [op, edit(sibling, version)]
        elif mode == 'recreate':
            steps = [{'kind': 'delete', 'uri': source}, {'kind': 'create', 'uri': source},
                     edit(source, version, 'replacement\n', 0)]
        elif mode == 'stale':
            steps[1]['textDocument']['version'] += 1
        elif mode == 'drift':
            (root / 'source.identity').write_text('external\n', encoding='utf-8')
        result = {'documentChanges': steps}
    send({'jsonrpc': '2.0', 'id': message['id'], 'result': result})
";

fn fixture(root: &Path, mode: &str) -> Option<(LspTool, asupersync::runtime::Runtime)> {
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
            "Python required for version-identity protocol tests"
        );
        eprintln!("SKIP version-identity protocol fixture: Python unavailable");
        return None;
    };
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".identity-root"), "").unwrap();
    std::fs::write(root.join("source.identity"), "old\n").unwrap();
    std::fs::write(root.join("sibling.identity"), "untouched\n").unwrap();
    let script = root.join("identity_peer.py");
    std::fs::write(&script, SERVER).unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([(
                "identity-fixture".to_string(),
                LspServerSettings {
                    command: Some(python.to_string()),
                    args: Some(vec![
                        "-I".to_string(),
                        "-u".to_string(),
                        script.display().to_string(),
                        mode.to_string(),
                    ]),
                    extensions: Some(vec![".identity".to_string()]),
                    languages: Some(vec!["plaintext".to_string()]),
                    root_markers: Some(vec![".identity-root".to_string()]),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        }),
        ..Default::default()
    };
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .unwrap();
    Some((LspTool::new(&root, Some(&config)), runtime))
}

fn input() -> Value {
    json!({"action":"rename","file":"source.identity","symbol":"old","newName":"renamed","timeout":5})
}

fn assert_request_logged(root: &Path) {
    let log = std::fs::read_to_string(root.join("requests.jsonl")).unwrap();
    let events: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .any(|event| event["method"] == "textDocument/rename")
    );
    assert!(
        !events
            .iter()
            .any(|event| event["method"] == "workspace/executeCommand")
    );
}

#[test]
fn public_rename_moves_then_edits_a_versioned_document() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "move") else {
        return;
    };
    let result = runtime
        .block_on(tool.execute("identity-move", input(), None))
        .unwrap();
    assert!(!result.is_error);
    assert!(!temp.path().join("source.identity").exists());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("moved.identity")).unwrap(),
        "renamed\n"
    );
    assert_eq!(
        std::fs::read_to_string(temp.path().join("sibling.identity")).unwrap(),
        "untouched\n"
    );
    assert_request_logged(temp.path());
}

#[test]
fn public_rename_preserves_identity_across_multiple_moves_and_edits() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "chain") else {
        return;
    };
    let result = runtime
        .block_on(tool.execute("identity-chain", input(), None))
        .unwrap();
    assert!(!result.is_error);
    assert!(!temp.path().join("source.identity").exists());
    assert!(!temp.path().join("moved.identity").exists());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("final.identity")).unwrap(),
        "complete\n"
    );
    assert_request_logged(temp.path());
}

#[test]
fn public_rename_uses_source_version_after_destination_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "overwrite") else {
        return;
    };
    let result = runtime
        .block_on(tool.execute("identity-overwrite", input(), None))
        .unwrap();
    assert!(!result.is_error);
    assert!(!temp.path().join("source.identity").exists());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("sibling.identity")).unwrap(),
        "renamed\n"
    );
    assert_request_logged(temp.path());
}

#[test]
fn public_rename_rejects_recreated_stale_and_drifted_identities_before_writing() {
    for mode in ["recreate", "stale", "drift"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else {
            return;
        };
        let error = runtime
            .block_on(tool.execute("identity-conflict", input(), None))
            .unwrap_err();
        assert!(
            error.to_string().contains("LSP_EDIT_CONFLICT"),
            "{mode}: {error}"
        );
        let expected = if mode == "drift" {
            "external\n"
        } else {
            "old\n"
        };
        assert_eq!(
            std::fs::read_to_string(temp.path().join("source.identity")).unwrap(),
            expected
        );
        assert!(!temp.path().join("moved.identity").exists());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("sibling.identity")).unwrap(),
            "untouched\n"
        );
        assert_request_logged(temp.path());
    }
}
