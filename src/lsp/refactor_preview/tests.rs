//! Review-time transactions use real file images, never a mock apply engine.

use super::*;
use crate::lsp::edits::{FileEvidence, parse_workspace_edit};
use crate::lsp::text::content_hash_for_drift;
use std::collections::HashMap;
use std::path::Path;

fn uri(path: &Path) -> String {
    try_path_to_uri(path).unwrap()
}

fn replacement(path: &Path, text: &str) -> Value {
    json!({"textDocument":{"uri":uri(path),"version":null},"edits":[{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
        "newText":text
    }]})
}

fn prepare(raw: &Value) -> PreparedEdit {
    PreparedEdit::new(&parse_workspace_edit(raw).unwrap(), &HashMap::new()).unwrap()
}

#[test]
fn prepared_rename_stages_without_writes_and_never_replans() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let old = root.join("old.rs");
    let new = root.join("nested/new.rs");
    std::fs::write(&old, "old\n").unwrap();
    let mut raw = json!({"documentChanges":[replacement(&old,"new"),
        {"kind":"rename","oldUri":uri(&old),"newUri":uri(&new)}]});
    let mut plan = parse_workspace_edit(&raw).unwrap();
    let prepared = PreparedEdit::new(&plan, &HashMap::new()).unwrap();
    assert_eq!(std::fs::read_to_string(&old).unwrap(), "old\n");
    assert!(!root.join("nested").exists());
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    // Mutating the input/parsed plan cannot change the retained final images.
    raw["documentChanges"][0]["edits"][0]["newText"] = json!("wrong");
    plan.text_edits.get_mut(&old).unwrap()[0].new_text = "wrong".into();
    prepared.commit(|| Ok(())).unwrap();
    assert!(!old.exists());
    assert_eq!(std::fs::read_to_string(&new).unwrap(), "new\n");
}

#[test]
fn unopened_sibling_drift_rejects_the_whole_prepared_batch() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source.rs");
    let sibling = root.join("sibling.rs");
    std::fs::write(&source, "old\n").unwrap();
    std::fs::write(&sibling, "old\n").unwrap();
    let prepared = prepare(
        &json!({"documentChanges":[replacement(&source,"new"),replacement(&sibling,"new")]}),
    );
    std::fs::write(&sibling, "external\n").unwrap();
    let error = prepared.commit(|| Ok(())).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"), "{error}");
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "old\n");
    assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "external\n");
}

#[test]
fn recreated_destination_cannot_replace_reviewed_absence() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source.rs");
    let destination = root.join("new.rs");
    std::fs::write(&source, "old\n").unwrap();
    let prepared = prepare(&json!({"documentChanges":[
        {"kind":"rename","oldUri":uri(&source),"newUri":uri(&destination)}]}));
    std::fs::write(&destination, "external\n").unwrap();
    assert!(prepared.commit(|| Ok(())).is_err());
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "old\n");
    assert_eq!(std::fs::read_to_string(&destination).unwrap(), "external\n");
}

#[test]
fn guard_only_source_participates_in_approval_and_summary() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source.rs");
    let target = root.join("target.rs");
    std::fs::write(&source, "source\n").unwrap();
    std::fs::write(&target, "old\n").unwrap();
    let expected = HashMap::from([(
        source.clone(),
        FileEvidence::Text(content_hash_for_drift("source\n")),
    )]);
    let raw = json!({"documentChanges":[replacement(&target,"new")]});
    let prepared = PreparedEdit::new(&parse_workspace_edit(&raw).unwrap(), &expected).unwrap();
    let summary = prepared.summary(&root);
    assert_eq!(
        summary[0],
        json!({"file":"source.rs","beforeBytes":7,"afterBytes":7,"changed":false})
    );
    assert_eq!(summary[1]["changed"], true);
    assert!(prepared.matches_document(&source, "source\n"));
    assert!(!prepared.matches_document(&source, "different\n"));
    std::fs::write(&source, "changed\n").unwrap();
    assert!(prepared.commit(|| Ok(())).is_err());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "old\n");
}

#[test]
fn approval_guard_runs_after_staging_and_blocks_all_writes() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap().join("source.rs");
    std::fs::write(&source, "old\n").unwrap();
    let prepared = prepare(&json!({"documentChanges":[replacement(&source,"new")]}));
    let error = prepared
        .commit(|| Err(tool_err("LSP_CANCELLED", "cancelled at approval")))
        .unwrap_err();
    assert!(error.to_string().contains("LSP_CANCELLED"));
    assert_eq!(std::fs::read_to_string(source).unwrap(), "old\n");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn dropping_a_prepared_create_does_not_create_scratch_or_parent_paths() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let destination = root.join("nested/new.rs");
    let prepared = prepare(&json!({"documentChanges":[{"kind":"create","uri":uri(&destination)}]}));
    let summary = prepared.summary(&root);
    assert!(summary[0]["beforeBytes"].is_null());
    assert_eq!(summary[0]["afterBytes"], 0);
    drop(prepared);
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
}

#[test]
fn prepared_binary_resource_moves_preserve_exact_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source.bin");
    let destination = root.join("new.bin");
    let bytes = [0, 255, 128, 10];
    std::fs::write(&source, bytes).unwrap();
    let prepared = prepare(
        &json!({"documentChanges":[{"kind":"rename","oldUri":uri(&source),"newUri":uri(&destination)}]}),
    );
    assert!(!prepared.matches_document(&source, ""));
    prepared.commit(|| Ok(())).unwrap();
    assert_eq!(std::fs::read(destination).unwrap(), bytes);
    assert!(!source.exists());
}

#[test]
fn prepared_delete_does_not_adopt_changed_original_content() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().canonicalize().unwrap().join("source.rs");
    std::fs::write(&source, "old\n").unwrap();
    let prepared = prepare(&json!({"documentChanges":[{"kind":"delete","uri":uri(&source)}]}));
    std::fs::write(&source, "must survive\n").unwrap();
    assert!(prepared.commit(|| Ok(())).is_err());
    assert_eq!(std::fs::read_to_string(source).unwrap(), "must survive\n");
}

#[test]
#[cfg(unix)]
fn prepared_approval_checks_permissions_and_parent_routes() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let directory = root.join("original");
    std::fs::create_dir(&directory).unwrap();
    let source = directory.join("source.rs");
    std::fs::write(&source, "old\n").unwrap();
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600)).unwrap();
    let raw = json!({"documentChanges":[replacement(&source,"new")]});
    let prepared = prepare(&raw);
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(prepared.commit(|| Ok(())).is_err());
    let prepared = prepare(&raw);
    let moved = root.join("moved");
    std::fs::rename(&directory, &moved).unwrap();
    symlink(&moved, &directory).unwrap();
    assert!(prepared.commit(|| Ok(())).is_err());
    assert_eq!(
        std::fs::read_to_string(moved.join("source.rs")).unwrap(),
        "old\n"
    );
}

#[test]
fn prepared_transactions_keep_existing_file_and_count_limits() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("large.rs");
    std::fs::File::create(&source)
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();
    let raw = json!({"documentChanges":[{"kind":"delete","uri":uri(&source)}]});
    assert!(PreparedEdit::new(&parse_workspace_edit(&raw).unwrap(), &HashMap::new()).is_err());
    let changes: Vec<_> = (0..1025)
        .map(|i| json!({"kind":"create","uri":uri(&root.join(format!("file-{i}.rs")))}))
        .collect();
    let plan = parse_workspace_edit(&json!({"documentChanges":changes})).unwrap();
    let error = PreparedEdit::new(&plan, &HashMap::new()).err().unwrap();
    assert!(error.to_string().contains("LSP_EDIT_LIMIT"), "{error}");
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 1);
}

#[test]
fn selected_refactor_rejects_all_overriding_selectors() {
    for (key, value) in [
        ("file", json!("other.rs")),
        ("newName", json!("other")),
        ("newFile", json!("other.rs")),
        ("symbol", json!("other")),
        ("line", json!(1)),
        ("query", json!("other")),
        ("limit", json!(1)),
        ("only", json!(["refactor"])),
        ("after", json!("a.rs")),
        ("actionId", json!("other")),
        ("completionId", json!("other")),
        ("snippetValues", json!({})),
        ("hierarchyId", json!("other")),
        ("resolve", json!(false)),
        ("formatOptions", json!({})),
        ("method", json!("workspace/executeCommand")),
        ("payload", json!({})),
        ("position", json!({"line":0,"character":0})),
        (
            "range",
            json!({"start":{"line":0,"character":0},"end":{"line":0,"character":1}}),
        ),
    ] {
        for action in ["rename", "rename_file", "code_actions", "format"] {
            let mut raw = json!({"action":action,"refactorId":"test"});
            raw[key] = value.clone();
            let input: LspInput = serde_json::from_value(raw).unwrap();
            assert!(validate_selection(&input).is_err(), "{action}: {key}");
        }
    }
    for raw in [
        json!({"action":"diagnostics","refactorId":"test"}),
        json!({"action":"rename","refactorId":""}),
        json!({"action":"rename","refactorId":"x".repeat(129)}),
    ] {
        assert!(validate_selection(&serde_json::from_value(raw).unwrap()).is_err());
    }
    for action in ["rename", "rename_file", "code_actions", "format"] {
        let input: LspInput = serde_json::from_value(
            json!({"action":action,"refactorId":"test","apply":true,"timeout":10}),
        )
        .unwrap();
        validate_selection(&input).unwrap();
    }
}

#[test]
fn oversized_previews_fail_instead_of_truncating_approval_evidence() {
    let payload = json!({"workspaceEdit":{"newText":"界".repeat(MAX_PAYLOAD_BYTES)}});
    let error = output(payload).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_LIMIT"));
    let payload = json!({"workspaceEdit":{"newText":"reviewed"}});
    let result = output(payload.clone()).unwrap();
    assert_eq!(result.details, Some(payload));
}

mod protocol;
