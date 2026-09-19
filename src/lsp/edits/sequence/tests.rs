use super::*;
use crate::lsp::client::path_to_uri;
use crate::lsp::edits::{apply_workspace_edit, parse_workspace_edit};
use serde_json::json;

fn text(path: &Path, start: u32, end: u32, value: &str) -> Value {
    json!({
        "textDocument": {"uri": path_to_uri(path), "version": null},
        "edits": [{
            "range": {
                "start": {"line": 0, "character": start},
                "end": {"line": 0, "character": end}
            },
            "newText": value
        }]
    })
}

#[allow(clippy::needless_pass_by_value)]
fn apply(steps: Vec<Value>) -> Result<super::super::ApplyOutcome> {
    let plan = parse_workspace_edit(&json!({"documentChanges": steps}))?;
    apply_workspace_edit(&plan, None)
}

#[test]
fn create_edit_rename_edit_is_one_validated_workspace_change() {
    let temp = tempfile::tempdir().unwrap();
    let initial = temp.path().join("new file.txt");
    let final_path = temp.path().join("nested/final file.txt");
    let steps = vec![
        json!({"kind":"create", "uri":path_to_uri(&initial)}),
        text(&initial, 0, 0, "abc"),
        json!({"kind":"rename", "oldUri":path_to_uri(&initial), "newUri":path_to_uri(&final_path)}),
        text(&final_path, 1, 2, "XYZ"),
    ];
    let plan = parse_workspace_edit(&json!({"documentChanges": steps})).unwrap();
    // Scope validation still sees EVERY affected source and destination.
    assert!(plan.text_edits.contains_key(&initial));
    assert!(plan.text_edits.contains_key(&final_path));
    assert_eq!(plan.file_ops.len(), 2);
    let outcome = apply_workspace_edit(&plan, None).unwrap();
    assert!(!initial.exists());
    assert_eq!(std::fs::read(final_path).unwrap(), b"aXYZc");
    assert_eq!(outcome.file_ops_applied.len(), 2);
}

#[test]
fn repeated_document_edits_are_not_merged_into_one_coordinate_space() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    std::fs::write(&path, "abc").unwrap();
    apply(vec![text(&path, 0, 1, "LONG"), text(&path, 4, 5, "Z")]).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"LONGZc");
}

#[test]
fn delete_then_create_edits_a_new_empty_document() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    std::fs::write(&path, "old contents").unwrap();
    apply(vec![
        json!({"kind":"delete", "uri":path_to_uri(&path)}),
        json!({"kind":"create", "uri":path_to_uri(&path)}),
        text(&path, 0, 0, "replacement"),
    ])
    .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"replacement");
}

#[test]
fn a_later_invalid_edit_leaves_all_earlier_resource_operations_unapplied() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    let created = temp.path().join("created");
    std::fs::write(&source, "original source").unwrap();
    std::fs::write(&destination, "original destination").unwrap();
    let error = apply(vec![
        json!({"kind":"create", "uri":path_to_uri(&created)}),
        json!({"kind":"rename", "oldUri":path_to_uri(&source), "newUri":path_to_uri(&destination), "options":{"overwrite":true}}),
        json!({"kind":"delete", "uri":path_to_uri(&destination)}),
        text(&destination, 0, 0, "must not run"),
    ]).expect_err("edit of staged-deleted file");
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"));
    assert!(!created.exists());
    assert_eq!(std::fs::read(source).unwrap(), b"original source");
    assert_eq!(std::fs::read(destination).unwrap(), b"original destination");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn create_ignore_if_exists_and_overwrite_precedence_are_explicit() {
    for overwrite in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source");
        std::fs::write(&path, "old").unwrap();
        let outcome = apply(vec![
            json!({"kind":"create", "uri":path_to_uri(&path), "options":{"overwrite":overwrite, "ignoreIfExists":true}}),
            text(&path, 0, 0, "new"),
        ]).unwrap();
        let expected = if overwrite { "new" } else { "newold" };
        assert_eq!(std::fs::read_to_string(path).unwrap(), expected);
        assert_eq!(outcome.file_ops_applied.len(), usize::from(overwrite));
    }
}

#[test]
fn ignored_rename_does_not_consume_the_source_and_overwrite_wins() {
    for overwrite in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        std::fs::write(&source, "source").unwrap();
        std::fs::write(&destination, "target").unwrap();
        let outcome = apply(vec![
            json!({"kind":"rename", "oldUri":path_to_uri(&source), "newUri":path_to_uri(&destination), "options":{"overwrite":overwrite, "ignoreIfExists":true}}),
            text(if overwrite { &destination } else { &source }, 0, 1, "Z"),
        ]).unwrap();
        if overwrite {
            assert!(!source.exists());
            assert_eq!(std::fs::read(destination).unwrap(), b"Zource");
        } else {
            assert_eq!(std::fs::read(source).unwrap(), b"Zource");
            assert_eq!(std::fs::read(destination).unwrap(), b"target");
        }
        assert_eq!(outcome.file_ops_applied.len(), usize::from(overwrite));
    }
}

#[test]
fn ignore_missing_delete_uses_the_current_staged_state() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    std::fs::write(&path, "old").unwrap();
    let outcome = apply(vec![
        json!({"kind":"delete", "uri":path_to_uri(&path)}),
        json!({"kind":"delete", "uri":path_to_uri(&path), "options":{"ignoreIfNotExists":true}}),
    ])
    .unwrap();
    assert!(!path.exists());
    assert_eq!(outcome.file_ops_applied.len(), 1);
    apply(vec![
        json!({"kind":"delete", "uri":path_to_uri(&path), "options":{"ignoreIfNotExists":true}}),
    ])
    .unwrap();
    assert!(apply(vec![json!({"kind":"delete", "uri":path_to_uri(&path)})]).is_err());
}

#[test]
fn document_changes_take_precedence_instead_of_applying_both_forms() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    std::fs::write(&path, "abc").unwrap();
    let legacy = text(&path, 0, 0, "wrong")["edits"].clone();
    let mut changes = serde_json::Map::new();
    changes.insert(path_to_uri(&path), legacy);
    let raw = json!({"changes":changes, "documentChanges":[text(&path, 0, 0, "right")]});
    apply_workspace_edit(&parse_workspace_edit(&raw).unwrap(), None).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"rightabc");
    let empty = parse_workspace_edit(&json!({"changes":changes, "documentChanges":[]})).unwrap();
    assert!(empty.text_edits.is_empty() && empty.file_ops.is_empty());
}

#[test]
fn malformed_containers_kinds_versions_and_options_are_not_silent_noops() {
    for raw in [
        json!({"changes":42}),
        json!({"changes":null}),
        json!({"documentChanges":{}}),
        json!({"documentChanges":null}),
        json!({"documentChanges":[42]}),
        json!({"documentChanges":[{"kind":false}]}),
        json!({"documentChanges":[{"kind":"create","uri":"file:///tmp/a","options":null}]}),
        json!({"documentChanges":[{"kind":"create","uri":"file:///tmp/a","options":{"overwrite":"yes"}}]}),
        json!({"documentChanges":[{"kind":"rename","oldUri":"file:///tmp/a","newUri":"file:///tmp/b","options":{"ignoreIfExists":1}}]}),
        json!({"documentChanges":[{"kind":"delete","uri":"file:///tmp/a","options":{"recursive":"yes"}}]}),
        json!({"documentChanges":[{"kind":"delete","uri":"file:///tmp/a","options":{"ignoreIfNotExists":null}}]}),
        json!({"documentChanges":[{"textDocument":{"uri":"file:///tmp/a","version":true},"edits":[]}]}),
    ] {
        let error = parse_workspace_edit(&raw).expect_err("malformed request");
        assert!(error.to_string().contains("LSP_EDIT_MALFORMED"), "{error}");
    }
}

#[test]
fn mutated_public_plan_indices_cannot_hide_ordered_operations_from_scope_checks() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    let raw = json!({"documentChanges":[
        {"kind":"create","uri":path_to_uri(&path)}, text(&path, 0, 0, "new")
    ]});
    for modification in 0..3 {
        let mut plan = parse_workspace_edit(&raw).unwrap();
        match modification {
            0 => plan.text_edits.clear(),
            1 => plan.file_ops.clear(),
            _ => plan.file_ops.push(FileOp::Delete { path: path.clone() }),
        }
        let error = apply_workspace_edit(&plan, None).expect_err("inconsistent indices");
        assert!(error.to_string().contains("LSP_EDIT_MALFORMED"));
        assert!(!path.exists());
    }
}

#[test]
fn renamed_text_is_checked_against_the_original_source_hash() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let destination = temp.path().join("destination");
    std::fs::write(&source, "abc").unwrap();
    let raw = json!({"documentChanges":[
        {"kind":"rename", "oldUri":path_to_uri(&source), "newUri":path_to_uri(&destination)},
        text(&destination, 0, 1, "Z")
    ]});
    let plan = parse_workspace_edit(&raw).unwrap();
    let mut hashes = HashMap::new();
    hashes.insert(source.clone(), 0);
    assert!(apply_workspace_edit(&plan, Some(&hashes)).is_err());
    assert_eq!(std::fs::read(&source).unwrap(), b"abc");
    assert!(!destination.exists());
    hashes.insert(
        source.clone(),
        crate::lsp::text::content_hash_for_drift("abc"),
    );
    apply_workspace_edit(&plan, Some(&hashes)).unwrap();
    assert!(!source.exists());
    assert_eq!(std::fs::read(destination).unwrap(), b"Zbc");
}

#[test]
fn operation_and_text_edit_counts_have_finite_admission_limits() {
    let entries = vec![
        json!({"textDocument":{"uri":"file:///tmp/a","version":null},"edits":[]});
        MAX_STEPS + 1
    ];
    let error = parse_workspace_edit(&json!({"documentChanges":entries})).expect_err("step bound");
    assert!(error.to_string().contains("LSP_EDIT_LIMIT"));
    let edit = json!({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"newText":""});
    let error =
        parse_workspace_edit(&json!({"changes":{"file:///tmp/a":vec![edit; MAX_TEXT_EDITS + 1]}}))
            .expect_err("edit bound");
    assert!(error.to_string().contains("LSP_EDIT_LIMIT"));
}
