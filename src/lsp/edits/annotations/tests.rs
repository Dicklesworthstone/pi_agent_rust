//! Annotation contracts exercised through the real parser and apply engines.

use super::*;
use crate::lsp::client::try_path_to_uri;
use crate::lsp::edits::{PreparedEdit, apply_checked, apply_workspace_edit, parse_workspace_edit};
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;

fn text(uri: &str, annotation: Option<&str>) -> Value {
    let mut edit = json!({
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
        "newText":"new"
    });
    if let Some(id) = annotation { edit["annotationId"] = json!(id); }
    json!({"textDocument":{"uri":uri,"version":null},"edits":[edit]})
}

fn workspace(uri: &str) -> Value {
    json!({
        "changeAnnotations":{"public-api":{
            "label":"Change exported API", "description":"Callers must adopt the new name.",
            "needsConfirmation":true
        }},
        "documentChanges":[text(uri, Some("public-api"))]
    })
}

fn file(root: &Path, name: &str) -> (std::path::PathBuf, String) {
    let path = root.canonicalize().unwrap().join(name);
    std::fs::write(&path, "old\n").unwrap();
    let uri = try_path_to_uri(&path).unwrap();
    (path, uri)
}

#[test]
fn text_and_resource_references_preserve_exact_annotation_identity() {
    let mut raw = workspace("file:///source.rs");
    raw["changeAnnotations"]["move"] = json!({"label":"Move module","needsConfirmation":false});
    raw["changeAnnotations"]["unused"] = json!({"label":"Unrelated","needsConfirmation":true});
    raw["documentChanges"].as_array_mut().unwrap().push(json!({
        "kind":"rename","oldUri":"file:///source.rs","newUri":"file:///target.rs","annotationId":"move"
    }));
    let plan = parse_workspace_edit(&raw).unwrap();
    assert_eq!(plan.annotations.required(), vec!["public-api"]);
    let annotations = plan.annotations.value();
    assert_eq!(annotations["public-api"], raw["changeAnnotations"]["public-api"]);
    assert_eq!(annotations["move"], raw["changeAnnotations"]["move"]);
    assert!(annotations.get("unused").is_none());
}

#[test]
fn wrapper_annotations_are_honored_but_conflicting_ids_fail() {
    let original = workspace("file:///source.rs");
    let edit = original["documentChanges"][0]["edits"][0].clone();
    for outer in [false, true] {
        let mut raw = original.clone();
        let mut wrapped = json!({"textEdit":edit});
        if outer { wrapped["annotationId"] = json!("public-api"); }
        raw["documentChanges"][0]["edits"][0] = wrapped;
        assert_eq!(parse_workspace_edit(&raw).unwrap().annotations.required(), vec!["public-api"]);
        raw["documentChanges"][0]["edits"][0]["annotationId"] = json!("different");
        assert!(parse_workspace_edit(&raw).is_err());
    }
}

#[test]
fn undefined_wrong_typed_and_misplaced_references_never_disappear() {
    for value in [Value::Null, json!(true), json!(7), json!(""), json!("missing"), json!("x".repeat(129))] {
        let mut raw = workspace("file:///source.rs");
        raw["documentChanges"][0]["edits"][0]["annotationId"] = value.clone();
        assert!(parse_workspace_edit(&raw).is_err(), "{value}");
        raw["documentChanges"] = json!([{"kind":"delete","uri":"file:///source.rs","annotationId":value}]);
        assert!(parse_workspace_edit(&raw).is_err());
    }
    let mut raw = workspace("file:///source.rs");
    raw.as_object_mut().unwrap().remove("changeAnnotations");
    assert!(parse_workspace_edit(&raw).is_err());
    let mut raw = workspace("file:///source.rs");
    raw["documentChanges"][0]["annotationId"] = json!("public-api");
    assert!(parse_workspace_edit(&raw).is_err());
}

#[test]
fn malformed_definitions_fail_even_when_unused() {
    for annotation in [
        Value::Null, json!([]), json!({}), json!({"label":null}), json!({"label":""}),
        json!({"label":"x","needsConfirmation":"true"}),
        json!({"label":"x","needsConfirmation":null}),
        json!({"label":"x","description":false}), json!({"label":"x","description":null}),
        json!({"label":"x".repeat(MAX_LABEL_BYTES + 1)}),
        json!({"label":"x","description":"y".repeat(MAX_DESCRIPTION_BYTES + 1)}),
    ] {
        let raw = json!({"changeAnnotations":{"unused":annotation},"documentChanges":[]});
        assert!(parse_workspace_edit(&raw).is_err());
    }
    for table in [Value::Null, json!(true), json!([])] {
        assert!(parse_workspace_edit(&json!({"changeAnnotations":table})).is_err());
    }
}

#[test]
fn count_and_serialized_byte_limits_apply_before_retaining_annotations() {
    let mut entries = Map::new();
    for index in 0..MAX_ANNOTATIONS {
        entries.insert(index.to_string(), json!({"label":"safe"}));
    }
    assert!(parse_workspace_edit(&json!({"changeAnnotations":entries})).is_ok());
    entries.insert("overflow".into(), json!({"label":"safe"}));
    let error = parse_workspace_edit(&json!({"changeAnnotations":entries})).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_LIMIT"));
    let raw = json!({"changeAnnotations":{"a":{"label":"safe","extension":"x".repeat(MAX_ANNOTATION_BYTES)}}});
    assert!(parse_workspace_edit(&raw).unwrap_err().to_string().contains("LSP_EDIT_LIMIT"));
}

#[test]
fn effective_document_changes_take_precedence_without_inheriting_ignored_confirmation() {
    let raw = json!({
        "changeAnnotations":{"ignored":{"label":"Ignored","needsConfirmation":true}},
        "changes":{"file:///source.rs":[text("file:///source.rs",Some("ignored"))["edits"][0]]},
        "documentChanges":[]
    });
    let plan = parse_workspace_edit(&raw).unwrap();
    assert!(plan.annotations.required().is_empty());
    assert_eq!(plan.annotations.value(), json!({}));
    let mut raw = workspace("file:///source.rs");
    raw["changes"] = json!({"file:///other.rs":[]});
    assert_eq!(parse_workspace_edit(&raw).unwrap().annotations.required(), vec!["public-api"]);
}

#[test]
fn direct_apply_rejects_the_entire_batch_before_any_file_changes() {
    let temp = tempfile::tempdir().unwrap();
    let (a, uri_a) = file(temp.path(), "a.rs");
    let (b, uri_b) = file(temp.path(), "b.rs");
    let mut raw = workspace(&uri_b);
    raw["documentChanges"].as_array_mut().unwrap().insert(0, text(&uri_a, None));
    let plan = parse_workspace_edit(&raw).unwrap();
    let error = apply_workspace_edit(&plan, None).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
    assert_eq!(std::fs::read(&a).unwrap(), b"old\n");
    assert_eq!(std::fs::read(&b).unwrap(), b"old\n");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 2);
}

#[test]
fn changes_form_cannot_bypass_required_annotations() {
    let temp = tempfile::tempdir().unwrap();
    let (path, uri) = file(temp.path(), "a.rs");
    let mut raw = workspace(&uri);
    raw["changes"] = json!({uri.clone():raw["documentChanges"][0]["edits"]});
    raw.as_object_mut().unwrap().remove("documentChanges");
    let error = apply_workspace_edit(&parse_workspace_edit(&raw).unwrap(), None).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
    assert_eq!(std::fs::read(path).unwrap(), b"old\n");
}

#[test]
fn informational_annotations_do_not_block_immediate_application() {
    let temp = tempfile::tempdir().unwrap();
    let (path, uri) = file(temp.path(), "a.rs");
    let mut raw = workspace(&uri);
    raw["changeAnnotations"]["public-api"].as_object_mut().unwrap().remove("needsConfirmation");
    let plan = parse_workspace_edit(&raw).unwrap();
    apply_workspace_edit(&plan, None).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"new\n");
}

#[test]
fn staged_edits_retain_annotations_and_only_reviewed_commit_accepts_them() {
    let temp = tempfile::tempdir().unwrap();
    let (path, uri) = file(temp.path(), "a.rs");
    let mut raw = workspace(&uri);
    let plan = parse_workspace_edit(&raw).unwrap();
    let pending = PreparedEdit::new(&plan, &HashMap::new()).unwrap();
    assert_eq!(pending.required_annotations(), vec!["public-api"]);
    assert!(pending.commit(|| Ok(())).unwrap_err().to_string().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
    assert_eq!(std::fs::read(&path).unwrap(), b"old\n");
    let reviewed = PreparedEdit::new(&plan, &HashMap::new()).unwrap();
    raw["changeAnnotations"]["public-api"]["needsConfirmation"] = json!(false);
    raw["documentChanges"][0]["edits"][0]["newText"] = json!("wrong");
    assert_eq!(reviewed.required_annotations(), vec!["public-api"]);
    reviewed.commit_reviewed(|| Ok(())).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"new\n");
}

#[test]
fn command_batch_confirmation_is_not_implied_by_an_open_edit_window() {
    let temp = tempfile::tempdir().unwrap();
    let (path, uri) = file(temp.path(), "a.rs");
    let plan = parse_workspace_edit(&workspace(&uri)).unwrap();
    let guard_ran = std::cell::Cell::new(false);
    let error = apply_checked(&plan, &HashMap::new(), || {
        guard_ran.set(true);
        Ok(())
    }).unwrap_err();
    assert!(error.to_string().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
    assert!(!guard_ran.get(), "no commit admission or receipt for an unreviewed batch");
    assert_eq!(std::fs::read(path).unwrap(), b"old\n");
}

#[test]
fn annotation_approval_does_not_bypass_source_or_owner_checks() {
    let temp = tempfile::tempdir().unwrap();
    let (path, uri) = file(temp.path(), "a.rs");
    let plan = parse_workspace_edit(&workspace(&uri)).unwrap();
    let reviewed = PreparedEdit::new(&plan, &HashMap::new()).unwrap();
    let error = reviewed.commit_reviewed(|| Err(plan_error("LSP_CANCELLED", "cancelled"))).unwrap_err();
    assert!(error.to_string().contains("LSP_CANCELLED"));
    assert_eq!(std::fs::read(&path).unwrap(), b"old\n");
    let reviewed = PreparedEdit::new(&plan, &HashMap::new()).unwrap();
    std::fs::write(&path, "external\n").unwrap();
    assert!(reviewed.commit_reviewed(|| Ok(())).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"external\n");
}

#[test]
fn resource_operations_keep_the_confirmation_boundary() {
    for kind in ["create", "rename", "delete"] {
        let temp = tempfile::tempdir().unwrap();
        let (path, uri) = file(temp.path(), "a.rs");
        let target = path.with_file_name("b.rs");
        let target_uri = try_path_to_uri(&target).unwrap();
        let mut operation = json!({"kind":kind,"annotationId":"public-api"});
        if kind == "rename" {
            operation["oldUri"] = json!(uri);
            operation["newUri"] = json!(target_uri);
        } else {
            operation["uri"] = json!(if kind == "create" { target_uri } else { uri });
        }
        let mut raw = workspace("file:///unused.rs");
        raw["documentChanges"] = json!([operation]);
        let plan = parse_workspace_edit(&raw).unwrap();
        assert!(apply_workspace_edit(&plan, None).unwrap_err().to_string().contains("LSP_EDIT_CONFIRMATION_REQUIRED"));
        assert!(path.exists());
        assert!(!target.exists());
        PreparedEdit::new(&plan, &HashMap::new()).unwrap().commit_reviewed(|| Ok(())).unwrap();
        assert_eq!(path.exists(), kind == "create");
        assert_eq!(target.exists(), kind != "delete");
    }
}
