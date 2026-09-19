use super::*;
use crate::lsp::client::try_path_to_uri;
use crate::lsp::edits::parse_workspace_edit;
use serde_json::{Value, json};

fn text(value: &str) -> FileEvidence {
    FileEvidence::Text(content_hash_for_drift(value))
}

fn edits(path: &std::path::Path, replacement: &str) -> Value {
    json!({"changes":{try_path_to_uri(path).unwrap():[{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
        "newText":replacement
    }]}})
}

fn apply(raw: &Value, expected: &HashMap<PathBuf, FileEvidence>) -> Result<CheckedApply> {
    apply_checked(&parse_workspace_edit(raw)?, expected, || Ok(()))
}

#[test]
fn receipts_follow_move_edit_delete_and_recreation_without_rereading_postimages() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let old = root.join("old.rs");
    let moved = root.join("moved.rs");
    std::fs::write(&old, "old\n").unwrap();
    let old_uri = try_path_to_uri(&old).unwrap();
    let moved_uri = try_path_to_uri(&moved).unwrap();
    let move_edit = json!({"documentChanges":[
        {"kind":"rename","oldUri":old_uri,"newUri":moved_uri},
        {"textDocument":{"uri":moved_uri,"version":null},"edits":edits(&moved,"new")["changes"][&moved_uri]}
    ]});
    let first = apply(&move_edit, &HashMap::from([(old.clone(), text("old\n"))])).unwrap();
    assert_eq!(first.states[&old], FileEvidence::Absent);
    assert_eq!(first.states[&moved], text("new\n"));
    let second = apply(&json!({"documentChanges":[{"kind":"delete","uri":moved_uri}]}), &first.states).unwrap();
    assert_eq!(second.states[&moved], FileEvidence::Absent);
    let third = apply(&json!({"documentChanges":[{"kind":"create","uri":moved_uri}]}), &second.states).unwrap();
    assert_eq!(third.states[&moved], text(""));
    assert!(!old.exists());
    assert_eq!(std::fs::read(&moved).unwrap(), b"");
}

#[test]
fn edited_files_cannot_adopt_external_content_between_batches() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("a.rs");
    std::fs::write(&path, "old\n").unwrap();
    let first = apply(&edits(&path, "new"), &HashMap::new()).unwrap();
    std::fs::write(&path, "external\n").unwrap();
    assert_eq!(first.states[&path], text("new\n"));
    let error = apply(&edits(&path, "bad"), &first.states).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "external\n");
}

#[test]
fn guard_only_source_drift_prevents_sibling_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source.rs");
    let sibling = root.join("sibling.rs");
    std::fs::write(&source, "external\n").unwrap();
    std::fs::write(&sibling, "old\n").unwrap();
    let error = apply(&edits(&sibling, "new"), &HashMap::from([(source.clone(), text("old\n"))])).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"));
    assert_eq!(std::fs::read_to_string(sibling).unwrap(), "old\n");
    assert_eq!(std::fs::read_to_string(source).unwrap(), "external\n");
}

#[test]
fn guard_only_files_are_rechecked_after_staging_before_commit() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let source = root.join("source.rs");
    let sibling = root.join("sibling.rs");
    std::fs::write(&source, "old\n").unwrap();
    std::fs::write(&sibling, "old\n").unwrap();
    let plan = parse_workspace_edit(&edits(&sibling, "new")).unwrap();
    let result = apply_checked(&plan, &HashMap::from([(source.clone(), text("old\n"))]), || {
        std::fs::write(&source, "external\n")?;
        Ok(())
    });
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(sibling).unwrap(), "old\n");
    assert_eq!(std::fs::read_to_string(source).unwrap(), "external\n");
}

#[test]
fn moved_away_paths_cannot_be_externally_recreated_then_overwritten() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("a.rs");
    std::fs::write(&path, "old\n").unwrap();
    let uri = try_path_to_uri(&path).unwrap();
    let deleted = apply(&json!({"documentChanges":[{"kind":"delete","uri":uri}]}), &HashMap::new()).unwrap();
    std::fs::write(&path, "external\n").unwrap();
    let create = json!({"documentChanges":[{"kind":"create","uri":uri,"options":{"overwrite":true}}]});
    assert!(apply(&create, &deleted.states).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "external\n");
}

#[test]
fn missing_is_not_an_empty_or_non_utf8_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("a.bin");
    let empty = WorkspaceEditPlan::default();
    for bytes in [b"".as_slice(), b"\xff\x00".as_slice()] {
        std::fs::write(&path, bytes).unwrap();
        assert!(apply_checked(&empty, &HashMap::from([(path.clone(), FileEvidence::Absent)]), || Ok(())).is_err());
    }
}

#[test]
fn binary_moves_produce_non_lossy_receipts_and_detect_later_drift() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let old = root.join("a.bin");
    let new = root.join("b.bin");
    std::fs::write(&old, b"\xff\x00\x01").unwrap();
    let result = apply(&json!({"documentChanges":[{"kind":"rename",
        "oldUri":try_path_to_uri(&old).unwrap(),"newUri":try_path_to_uri(&new).unwrap()}]}), &HashMap::new()).unwrap();
    assert!(matches!(result.states[&new], FileEvidence::Binary(_)));
    assert_eq!(result.states[&old], FileEvidence::Absent);
    std::fs::write(&new, b"\xfe\x00\x01").unwrap();
    let deletion = json!({"documentChanges":[{"kind":"delete","uri":try_path_to_uri(&new).unwrap()}]});
    assert!(apply(&deletion, &result.states).is_err());
    assert_eq!(std::fs::read(new).unwrap(), b"\xfe\x00\x01");
}

#[test]
fn cancellation_after_staging_never_starts_commit() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("a.rs");
    std::fs::write(&path, "old\n").unwrap();
    let plan = parse_workspace_edit(&edits(&path, "new")).unwrap();
    let result = apply_checked(&plan, &HashMap::new(), || Err(crate::lsp::edits::plan_error("LSP_CANCELLED", "cancelled")));
    assert!(result.unwrap_err().to_string().contains("LSP_CANCELLED"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "old\n");
}

#[test]
fn evidence_path_budget_is_enforced_before_any_write() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let expected = (0..1025).map(|index| (root.join(format!("{index}.rs")), FileEvidence::Absent)).collect();
    let error = apply_checked(&WorkspaceEditPlan::default(), &expected, || Ok(())).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_LIMIT"));
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
}

#[test]
fn checked_batches_reuse_the_existing_rollback_commit_path() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("a.rs");
    std::fs::write(&path, "old\n").unwrap();
    let plan = parse_workspace_edit(&edits(&path, "new")).unwrap();
    let transaction = prepare_checked(&plan, &HashMap::from([(path.clone(), text("old\n"))])).unwrap();
    let error = transaction.commit_with(|_, _| Err(std::io::Error::other("injected commit failure"))).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_APPLY"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), "old\n");
}
