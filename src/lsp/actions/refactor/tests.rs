//! Refactor the actual on-disk workspace through the public tool dispatch.

use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::process::{Command, Stdio};

const SERVER: &str = r#"
import json, pathlib, sys
root = pathlib.Path.cwd()
mode = sys.argv[1]
versions = {}
def send(message):
    body = json.dumps(message).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
def edits():
    return [{'range': {'start': {'line': 0, 'character': 0}, 'end': {'line': 0, 'character': 3}}, 'newText': 'renamed'}]
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
        log.write(json.dumps(message) + '\n')
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
        sibling = (root / 'sibling.refactor').as_uri()
        result = {'changes': {source: edits(), sibling: edits()}}
        if mode == 'escape':
            result['changes'][(root.parent / 'outside.refactor').as_uri()] = edits()
        elif mode in ('version', 'stale', 'unknown'):
            uri = sibling if mode == 'unknown' else source
            version = versions[source] + (1 if mode == 'stale' else 0)
            result = {'documentChanges': [{'textDocument': {'uri': uri, 'version': version}, 'edits': edits()}]}
        elif mode == 'source-drift':
            (root / 'source.refactor').write_text('external source\n', encoding='utf-8')
        elif mode == 'sibling-drift':
            (root / 'sibling.refactor').write_text('external sibling\n', encoding='utf-8')
        elif mode == 'resource':
            result = {'documentChanges': [
                {'textDocument': {'uri': sibling, 'version': None}, 'edits': edits()},
                {'kind': 'rename', 'oldUri': source, 'newUri': (root / 'moved.refactor').as_uri()}
            ]}
    send({'jsonrpc': '2.0', 'id': message['id'], 'result': result})
"#;

fn fixture(root: &Path, mode: &str) -> Option<(LspTool, asupersync::runtime::Runtime)> {
    let python = ["python3", "python"].into_iter().find(|program| {
        Command::new(program).arg("--version")
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status().is_ok_and(|status| status.success())
    });
    let Some(python) = python else {
        assert!(std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(), "Python required for refactoring protocol tests");
        eprintln!("SKIP refactoring protocol fixture: Python unavailable");
        return None;
    };
    std::fs::create_dir_all(root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(root.join(".refactor-root"), "").unwrap();
    std::fs::write(root.join("source.refactor"), "old\n").unwrap();
    std::fs::write(root.join("sibling.refactor"), "old\n").unwrap();
    let script = root.join("refactor_peer.py");
    std::fs::write(&script, SERVER).unwrap();
    let config = Config {
        lsp: Some(LspSettings {
            servers: Some(HashMap::from([("refactor-fixture".to_string(), LspServerSettings {
                command: Some(python.to_string()),
                args: Some(vec!["-I".to_string(), "-u".to_string(), script.display().to_string(), mode.to_string()]),
                extensions: Some(vec![".refactor".to_string()]),
                languages: Some(vec!["plaintext".to_string()]),
                root_markers: Some(vec![".refactor-root".to_string()]),
                ..Default::default()
            })])),
            ..Default::default()
        }),
        ..Default::default()
    };
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    Some((LspTool::new(&root, Some(&config)), runtime))
}

fn rename(tool: &LspTool, runtime: &asupersync::runtime::Runtime) -> Result<ToolOutput> {
    runtime.block_on(tool.execute("rename-case", json!({
        "action":"rename","file":"source.refactor","symbol":"old","newName":"renamed","timeout":5
    }), None))
}

fn contents(root: &Path, name: &str) -> String {
    std::fs::read_to_string(root.join(name)).unwrap()
}

#[test]
fn symbol_rename_updates_multiple_files_through_scoped_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let result = rename(&tool, &runtime).unwrap();
    assert!(!result.is_error);
    assert_eq!(contents(temp.path(), "source.refactor"), "renamed\n");
    assert_eq!(contents(temp.path(), "sibling.refactor"), "renamed\n");
    let details = result.details.unwrap();
    assert_eq!(details["filesChanged"].as_array().unwrap().len(), 2);
    assert_eq!(details["atomic"], false);
    assert_eq!(details["rollbackOnError"], true);
}

#[test]
fn symbol_rename_cannot_write_outside_the_server_workspace() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("outside.refactor"), "old\n").unwrap();
    let root = temp.path().join("workspace");
    let Some((tool, runtime)) = fixture(&root, "escape") else { return };
    let error = rename(&tool, &runtime).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_SCOPE"), "{error}");
    assert_eq!(contents(&root, "source.refactor"), "old\n");
    assert_eq!(contents(&root, "sibling.refactor"), "old\n");
    assert_eq!(contents(temp.path(), "outside.refactor"), "old\n");
}

#[test]
fn symbol_rename_accepts_the_matching_document_version() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "version") else { return };
    rename(&tool, &runtime).unwrap();
    assert_eq!(contents(temp.path(), "source.refactor"), "renamed\n");
    assert_eq!(contents(temp.path(), "sibling.refactor"), "old\n");
}

#[test]
fn stale_and_unknown_versioned_renames_leave_the_workspace_unchanged() {
    for mode in ["stale", "unknown"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        let error = rename(&tool, &runtime).unwrap_err();
        assert!(error.to_string().contains("requested document version"), "{error}");
        assert_eq!(contents(temp.path(), "source.refactor"), "old\n");
        assert_eq!(contents(temp.path(), "sibling.refactor"), "old\n");
    }
}

#[test]
fn source_drift_during_rename_preserves_the_external_edit() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "source-drift") else { return };
    assert!(rename(&tool, &runtime).unwrap_err().to_string().contains("LSP_EDIT_CONFLICT"));
    assert_eq!(contents(temp.path(), "source.refactor"), "external source\n");
    assert_eq!(contents(temp.path(), "sibling.refactor"), "old\n");
}

#[test]
fn previously_synchronized_sibling_drift_is_checked_before_any_write() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "sibling-drift") else { return };
    runtime.block_on(tool.synced(&tool.cwd.join("sibling.refactor"))).unwrap();
    assert!(rename(&tool, &runtime).unwrap_err().to_string().contains("LSP_EDIT_CONFLICT"));
    assert_eq!(contents(temp.path(), "source.refactor"), "old\n");
    assert_eq!(contents(temp.path(), "sibling.refactor"), "external sibling\n");
}

#[test]
fn resource_operations_invalidate_all_open_document_state() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "resource") else { return };
    let (_, entry) = runtime.block_on(tool.synced(&tool.cwd.join("sibling.refactor"))).unwrap();
    let result = rename(&tool, &runtime).unwrap();
    assert_eq!(result.details.unwrap()["fileOps"].as_array().unwrap().len(), 1);
    assert!(!temp.path().join("source.refactor").exists());
    assert_eq!(contents(temp.path(), "moved.refactor"), "old\n");
    assert_eq!(entry.client.open_document_count(), 0);
}

#[test]
fn cancelled_refactor_does_not_apply_an_already_available_edit() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "normal") else { return };
    let path = tool.cwd.join("source.refactor");
    let (uri, entry) = runtime.block_on(tool.synced(&path)).unwrap();
    let snapshot = RefactorSnapshot::capture(&entry, &path, file_hash(&path).unwrap()).unwrap();
    let owner = AgentCx::for_request();
    owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel before applying"));
    let raw = json!({"documentChanges":[{
        "textDocument":{"uri":uri,"version":null},
        "edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"renamed"}]
    }]});
    let error = tool.apply_refactor(&entry, &raw, &snapshot, &owner).unwrap_err();
    assert!(error.to_string().contains("LSP_CANCELLED"));
    assert_eq!(contents(temp.path(), "source.refactor"), "old\n");
}

#[test]
fn oversized_rename_responses_are_rejected_before_copying_the_plan() {
    let raw = json!({"changes":{},"unexpected":"x".repeat(MAX_ACTION_BYTES + 1)});
    assert!(check_response_size(&raw).unwrap_err().to_string().contains("LSP_EDIT_LIMIT"));
}
