//! Refactor the actual on-disk workspace through the public tool dispatch.

use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::tools::Tool as _;
use std::process::{Command, Stdio};

const SERVER: &str = r#"
import json, pathlib, sys, urllib.parse, urllib.request
root = pathlib.Path.cwd()
mode = sys.argv[1]
versions = {}
def send(message):
    body = json.dumps(message).encode('utf-8')
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n' % len(body)).encode('ascii') + body)
    sys.stdout.buffer.flush()
def edits():
    return [{'range': {'start': {'line': 0, 'character': 0}, 'end': {'line': 0, 'character': 3}}, 'newText': 'renamed'}]
def native(uri):
    return pathlib.Path(urllib.request.url2pathname(urllib.parse.urlsplit(uri).path))
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
    if method == 'workspace/didRenameFiles':
        moved = params['files'][0]
        target = native(moved['newUri'])
        evidence = {
            'sourceExists': native(moved['oldUri']).exists(),
            'targetExists': target.exists(),
            'targetText': target.read_text(encoding='utf-8') if target.exists() else None,
            'siblingText': (root / 'sibling.refactor').read_text(encoding='utf-8')
        }
        (root / 'notification.json').write_text(json.dumps(evidence), encoding='utf-8')
    if 'id' not in message:
        continue
    result = None
    if method == 'initialize':
        result = {'capabilities': {'textDocumentSync': 1, 'renameProvider': True}}
        if mode.startswith('move-') and mode != 'move-unregistered':
            glob = '**/*.other' if mode == 'move-filtered' else '**/*.refactor'
            registration = {'filters': [{'scheme': 'file', 'pattern': {'glob': glob, 'matches': 'file'}}]}
            result['capabilities']['workspace'] = {'fileOperations': {
                'willRename': registration, 'didRename': registration
            }}
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
    elif method == 'workspace/willRenameFiles':
        source = params['files'][0]['oldUri']
        destination = native(params['files'][0]['newUri'])
        sibling = (root / 'sibling.refactor').as_uri()
        result = {'changes': {source: edits(), sibling: edits()}}
        if mode == 'move-appeared':
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text('external destination\n', encoding='utf-8')
        elif mode == 'move-invalid':
            result['changes'][sibling][0]['range']['end']['line'] = 999
        elif mode == 'move-escape':
            result['changes'][(root.parent / 'outside.refactor').as_uri()] = edits()
        elif mode == 'move-drift':
            (root / 'source.refactor').write_text('external source\n', encoding='utf-8')
        elif mode == 'move-error':
            send({'jsonrpc': '2.0', 'id': message['id'], 'error': {'code': -32603, 'message': 'import preparation failed'}})
            continue
        elif mode == 'move-ordered':
            result = {'documentChanges': [
                {'textDocument': {'uri': source, 'version': versions[source]}, 'edits': edits()},
                {'textDocument': {'uri': sibling, 'version': None}, 'edits': edits()}
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

fn move_file(tool: &LspTool, runtime: &asupersync::runtime::Runtime, destination: &str) -> Result<ToolOutput> {
    runtime.block_on(tool.execute("move-case", json!({
        "action":"rename_file","file":"source.refactor","newFile":destination,"timeout":5
    }), None))
}

fn barrier(tool: &LspTool, runtime: &asupersync::runtime::Runtime, file: &str) {
    runtime.block_on(tool.execute("barrier", json!({
        "action":"request","file":file,"method":"test/barrier","timeout":5
    }), None)).unwrap();
}

fn frames(root: &Path) -> Vec<Value> {
    contents(root, "requests.jsonl").lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[test]
fn move_and_import_updates_finish_before_the_server_is_notified() {
    for mode in ["move-normal", "move-ordered"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        let target = "nested/deeper/renamed.refactor";
        let output = move_file(&tool, &runtime, target).unwrap();
        assert!(!output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["applied"], true);
        assert_eq!(details["willRenameFiles"], true);
        assert_eq!(details["notificationWritten"], true);
        assert_eq!(details["importUpdates"].as_array().unwrap().len(), 2);
        assert!(!temp.path().join("source.refactor").exists());
        assert_eq!(contents(temp.path(), target), "renamed\n");
        assert_eq!(contents(temp.path(), "sibling.refactor"), "renamed\n");
        // The peer processes this request only after prior notifications.
        barrier(&tool, &runtime, target);
        let evidence: Value = serde_json::from_str(&contents(temp.path(), "notification.json")).unwrap();
        assert_eq!(evidence, json!({
            "sourceExists":false,"targetExists":true,
            "targetText":"renamed\n","siblingText":"renamed\n"
        }));
        let frames = frames(temp.path());
        let before = frames.iter().position(|frame| frame["method"] == "workspace/willRenameFiles").unwrap();
        let after = frames.iter().position(|frame| frame["method"] == "workspace/didRenameFiles").unwrap();
        assert!(before < after);
    }
}

#[test]
fn destination_created_while_waiting_leaves_imports_and_source_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "move-appeared") else { return };
    let error = move_file(&tool, &runtime, "destination.refactor").unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
    assert_eq!(contents(temp.path(), "destination.refactor"), "external destination\n");
    assert_eq!(contents(temp.path(), "source.refactor"), "old\n");
    assert_eq!(contents(temp.path(), "sibling.refactor"), "old\n");
    barrier(&tool, &runtime, "source.refactor");
    assert!(!frames(temp.path()).iter().any(|frame| frame["method"] == "workspace/didRenameFiles"));
}

#[test]
fn invalid_import_updates_and_provider_errors_prevent_the_entire_move() {
    for (mode, code) in [("move-invalid", "LSP_EDIT_CONFLICT"), ("move-error", "import preparation failed")] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        let error = move_file(&tool, &runtime, "new/deeper/destination.refactor").unwrap_err();
        assert!(error.to_string().contains(code), "{error}");
        assert_eq!(contents(temp.path(), "source.refactor"), "old\n");
        assert_eq!(contents(temp.path(), "sibling.refactor"), "old\n");
        assert!(!temp.path().join("new").exists());
        barrier(&tool, &runtime, "source.refactor");
        assert!(!temp.path().join("notification.json").exists());
    }
}

#[test]
fn escaping_import_edits_do_not_move_the_source_or_change_other_files() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("outside.refactor"), "old\n").unwrap();
    let root = temp.path().join("workspace");
    let Some((tool, runtime)) = fixture(&root, "move-escape") else { return };
    let error = move_file(&tool, &runtime, "destination.refactor").unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_SCOPE"), "{error}");
    assert_eq!(contents(&root, "source.refactor"), "old\n");
    assert_eq!(contents(&root, "sibling.refactor"), "old\n");
    assert_eq!(contents(temp.path(), "outside.refactor"), "old\n");
    assert!(!root.join("destination.refactor").exists());
}

#[test]
fn source_changed_during_import_preparation_is_not_moved_or_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let Some((tool, runtime)) = fixture(temp.path(), "move-drift") else { return };
    let error = move_file(&tool, &runtime, "destination.refactor").unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
    assert_eq!(contents(temp.path(), "source.refactor"), "external source\n");
    assert_eq!(contents(temp.path(), "sibling.refactor"), "old\n");
    assert!(!temp.path().join("destination.refactor").exists());
}

#[test]
fn out_of_workspace_destination_is_rejected_before_will_rename_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let Some((tool, runtime)) = fixture(&root, "move-normal") else { return };
    let destination = temp.path().canonicalize().unwrap().join("outside.refactor");
    let error = move_file(&tool, &runtime, destination.to_str().unwrap()).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_SCOPE"), "{error}");
    assert_eq!(contents(&root, "source.refactor"), "old\n");
    assert!(!destination.exists());
    barrier(&tool, &runtime, "source.refactor");
    assert!(!frames(&root).iter().any(|frame| frame["method"] == "workspace/willRenameFiles"));
}

#[test]
fn unregistered_or_nonmatching_servers_receive_no_file_operation_messages() {
    for mode in ["move-unregistered", "move-filtered"] {
        let temp = tempfile::tempdir().unwrap();
        let Some((tool, runtime)) = fixture(temp.path(), mode) else { return };
        let output = move_file(&tool, &runtime, "destination.refactor").unwrap();
        let details = output.details.unwrap();
        assert_eq!(details["applied"], true);
        assert_eq!(details["willRenameFiles"], false);
        assert_eq!(details["notificationRequested"], false);
        assert!(details["importUpdates"].as_array().unwrap().is_empty());
        assert_eq!(contents(temp.path(), "destination.refactor"), "old\n");
        assert_eq!(contents(temp.path(), "sibling.refactor"), "old\n");
        assert!(!temp.path().join("source.refactor").exists());
        barrier(&tool, &runtime, "destination.refactor");
        assert!(!frames(temp.path()).iter().any(|frame| {
            frame["method"] == "workspace/willRenameFiles" || frame["method"] == "workspace/didRenameFiles"
        }));
    }
}

#[test]
fn appending_a_move_preserves_order_versions_and_change_annotations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let old_uri = try_path_to_uri(&root.join("a.refactor")).unwrap();
    let new_uri = try_path_to_uri(&root.join("b.refactor")).unwrap();
    let original = json!({
        "changes":{"ignored":42},
        "documentChanges":[{"textDocument":{"uri":old_uri,"version":7},"edits":[]}],
        "changeAnnotations":{"move":{"label":"Keep this label"}}
    });
    let combined = append_move(original.clone(), &old_uri, &new_uri).unwrap();
    assert_eq!(combined["documentChanges"][0], original["documentChanges"][0]);
    assert_eq!(combined["changeAnnotations"], original["changeAnnotations"]);
    assert!(combined.get("changes").is_none());
    assert_eq!(combined["documentChanges"][1], json!({
        "kind":"rename","oldUri":old_uri,"newUri":new_uri,"options":{"overwrite":false}
    }));
    assert_eq!(parse_workspace_edit(&combined).unwrap().file_ops.len(), 1);
    for malformed in [json!({"changes":42}), json!({"documentChanges":null}), json!(false)] {
        assert!(append_move(malformed, &old_uri, &new_uri).is_err());
    }
}

#[test]
fn static_file_operation_filters_match_scheme_kind_case_and_native_paths() {
    fn capabilities(filter: Value) -> Value {
        json!({"workspace":{"fileOperations":{"willRename":{"filters":[filter]}}}})
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("My File.REFACTOR");
    let matching = json!({"scheme":"file","pattern":{
        "glob":"**/*.refactor","matches":"file","options":{"ignoreCase":true}
    }});
    assert!(registered_for_file(&capabilities(matching.clone()), "willRename", &path).unwrap());
    assert!(!registered_for_file(&capabilities(matching.clone()), "didRename", &path).unwrap());
    for (field, value) in [("scheme", json!("untitled")), ("matches", json!("folder")), ("ignoreCase", json!(false))] {
        let mut filter = matching.clone();
        match field {
            "scheme" => filter[field] = value,
            "matches" => filter["pattern"][field] = value,
            _ => filter["pattern"]["options"][field] = value,
        }
        assert!(!registered_for_file(&capabilities(filter), "willRename", &path).unwrap());
    }
    for filter in [
        json!({"scheme":true,"pattern":{"glob":"**/*"}}),
        json!({"pattern":{"glob":"["}}),
        json!({"pattern":{"glob":"**/*","matches":true}}),
        json!({"pattern":{"glob":"**/*","options":{"ignoreCase":"yes"}}}),
    ] {
        assert!(registered_for_file(&capabilities(filter), "willRename", &path).is_err());
    }
}

#[cfg(unix)]
#[test]
fn a_requested_symlink_is_not_replaced_by_a_move_of_its_referent() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target.refactor");
    let link = temp.path().join("link.refactor");
    std::fs::write(&target, "keep referent").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let tool = LspTool::new(temp.path(), None);
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    let error = runtime.block_on(tool.execute("symlink", json!({
        "action":"rename_file","file":"link.refactor","newFile":"moved.refactor"
    }), None)).unwrap_err();
    assert!(error.to_string().contains("LSP_FILE_UNREADABLE"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep referent");
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    assert!(!temp.path().join("moved.refactor").exists());
}
