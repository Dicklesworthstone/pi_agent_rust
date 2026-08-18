//! WorkspaceEdit parsing and atomic application.
//!
//! A `WorkspaceEdit` arrives either as `changes: {uri: [TextEdit]}` or as
//! `documentChanges: [...]` mixing `TextDocumentEdit`s with file operations
//! (`CreateFile`/`RenameFile`/`DeleteFile`). Application is all-or-nothing:
//! every file's new content is computed in memory first (positions mapped,
//! overlaps rejected, drift against the request-time hash rejected), then
//! written via temp-file + rename; a mid-apply failure rolls back
//! already-written files from their staged originals (bd-cv653.1.1, same
//! discipline as `ast_edit`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::client::uri_to_path;
use super::text::{TextEdit, apply_text_edits, content_hash_for_drift};

/// One planned file write: original content retained for rollback.
#[derive(Debug)]
struct PlannedWrite {
    path: PathBuf,
    original: String,
    updated: String,
}

/// A file operation from `documentChanges`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    /// Create a file (fails when it exists unless overwrite).
    Create { path: PathBuf, overwrite: bool },
    /// Rename/move a file.
    Rename {
        old_path: PathBuf,
        new_path: PathBuf,
        overwrite: bool,
    },
    /// Delete a file.
    Delete { path: PathBuf },
}

/// A parsed, validated WorkspaceEdit ready for atomic application.
#[derive(Debug, Default)]
pub struct WorkspaceEditPlan {
    /// Text edits per file (uri-decoded paths).
    pub text_edits: HashMap<PathBuf, Vec<TextEdit>>,
    /// File operations in document order.
    pub file_ops: Vec<FileOp>,
}

/// Errors carry a machine-readable taxonomy prefix.
fn plan_error(code: &str, message: impl Into<String>) -> crate::error::Error {
    crate::error::Error::tool("lsp", format!("[{code}] {}", message.into()))
}

/// Parse a raw LSP `WorkspaceEdit` JSON value into a plan.
///
/// # Errors
///
/// Returns `[LSP_EDIT_MALFORMED]` when the payload cannot be interpreted.
pub fn parse_workspace_edit(raw: &Value) -> Result<WorkspaceEditPlan, crate::error::Error> {
    let mut plan = WorkspaceEditPlan::default();
    if raw.is_null() {
        return Ok(plan);
    }
    let Some(obj) = raw.as_object() else {
        return Err(plan_error(
            "LSP_EDIT_MALFORMED",
            "WorkspaceEdit is not an object",
        ));
    };

    if let Some(changes) = obj.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            let path = uri_to_path(uri).ok_or_else(|| {
                plan_error("LSP_EDIT_MALFORMED", format!("unsupported uri {uri:?}"))
            })?;
            let parsed = parse_text_edit_array(edits)?;
            plan.text_edits.entry(path).or_default().extend(parsed);
        }
    }

    if let Some(document_changes) = obj.get("documentChanges").and_then(Value::as_array) {
        for entry in document_changes {
            let kind = entry.get("kind").and_then(Value::as_str);
            match kind {
                None => {
                    // TextDocumentEdit.
                    let uri = entry
                        .get("textDocument")
                        .and_then(|doc| doc.get("uri"))
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            plan_error(
                                "LSP_EDIT_MALFORMED",
                                "TextDocumentEdit missing textDocument.uri",
                            )
                        })?;
                    let path = uri_to_path(uri).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", format!("unsupported uri {uri:?}"))
                    })?;
                    let edits = entry.get("edits").ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", "TextDocumentEdit missing edits")
                    })?;
                    let parsed = parse_text_edit_array(edits)?;
                    plan.text_edits.entry(path).or_default().extend(parsed);
                }
                Some("create") => {
                    let uri = entry.get("uri").and_then(Value::as_str).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", "CreateFile missing uri")
                    })?;
                    let path = uri_to_path(uri).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", format!("unsupported uri {uri:?}"))
                    })?;
                    let overwrite = entry
                        .get("options")
                        .and_then(|o| o.get("overwrite"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    plan.file_ops.push(FileOp::Create { path, overwrite });
                }
                Some("rename") => {
                    let old_uri = entry.get("oldUri").and_then(Value::as_str).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", "RenameFile missing oldUri")
                    })?;
                    let new_uri = entry.get("newUri").and_then(Value::as_str).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", "RenameFile missing newUri")
                    })?;
                    let old_path = uri_to_path(old_uri).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", format!("unsupported uri {old_uri:?}"))
                    })?;
                    let new_path = uri_to_path(new_uri).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", format!("unsupported uri {new_uri:?}"))
                    })?;
                    let overwrite = entry
                        .get("options")
                        .and_then(|o| o.get("overwrite"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    plan.file_ops.push(FileOp::Rename {
                        old_path,
                        new_path,
                        overwrite,
                    });
                }
                Some("delete") => {
                    let uri = entry.get("uri").and_then(Value::as_str).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", "DeleteFile missing uri")
                    })?;
                    let path = uri_to_path(uri).ok_or_else(|| {
                        plan_error("LSP_EDIT_MALFORMED", format!("unsupported uri {uri:?}"))
                    })?;
                    plan.file_ops.push(FileOp::Delete { path });
                }
                Some(other) => {
                    return Err(plan_error(
                        "LSP_EDIT_MALFORMED",
                        format!("unknown documentChanges kind {other:?}"),
                    ));
                }
            }
        }
    }
    Ok(plan)
}

fn parse_text_edit_array(raw: &Value) -> Result<Vec<TextEdit>, crate::error::Error> {
    let Some(edits) = raw.as_array() else {
        return Err(plan_error("LSP_EDIT_MALFORMED", "edits is not an array"));
    };
    let mut out = Vec::with_capacity(edits.len());
    for edit in edits {
        // AnnotatedTextEdit wraps the edit under `textEdit` + annotationId.
        let edit = edit.get("textEdit").unwrap_or(edit);
        let range = edit
            .get("range")
            .ok_or_else(|| plan_error("LSP_EDIT_MALFORMED", "TextEdit missing range"))?;
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| plan_error("LSP_EDIT_MALFORMED", "TextEdit missing newText"))?;
        let range = serde_json::from_value(range.clone())
            .map_err(|err| plan_error("LSP_EDIT_MALFORMED", format!("bad range: {err}")))?;
        out.push(TextEdit {
            range,
            new_text: new_text.to_string(),
        });
    }
    Ok(out)
}

/// Outcome of an atomic apply.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyOutcome {
    /// Files whose text content changed.
    pub files_changed: Vec<PathBuf>,
    /// File operations performed, in order.
    pub file_ops_applied: Vec<String>,
}

/// Apply a parsed WorkspaceEdit atomically.
///
/// Phase 1 (validate + compute): read every target file, apply its text
/// edits in memory. Any failure (unreadable file, out-of-range position,
/// overlap, drift when `expected_hashes` are provided) aborts with zero
/// writes.
///
/// Phase 2 (commit): write each file via temp-file + rename in the same
/// directory, then perform file operations. A mid-commit failure rolls back
/// already-written files from staged originals and un-applies file ops on a
/// best-effort basis.
///
/// # Errors
///
/// Returns `[LSP_EDIT_CONFLICT]` for drift/overlap/range failures and
/// `[LSP_EDIT_APPLY]` for I/O failures during commit (after rollback).
pub fn apply_workspace_edit(
    plan: &WorkspaceEditPlan,
    expected_hashes: Option<&HashMap<PathBuf, u64>>,
) -> Result<ApplyOutcome, crate::error::Error> {
    // ── Phase 1: validate + compute ─────────────────────────────────────
    let mut planned: Vec<PlannedWrite> = Vec::with_capacity(plan.text_edits.len());
    for (path, edits) in &plan.text_edits {
        let original = std::fs::read_to_string(path).map_err(|err| {
            plan_error(
                "LSP_EDIT_CONFLICT",
                format!("cannot read {}: {err}", path.display()),
            )
        })?;
        if let Some(expected) = expected_hashes.and_then(|hashes| hashes.get(path)) {
            let actual = content_hash_for_drift(&original);
            if actual != *expected {
                return Err(plan_error(
                    "LSP_EDIT_CONFLICT",
                    format!(
                        "{} changed on disk since the edit was computed; re-run the request",
                        path.display()
                    ),
                ));
            }
        }
        let updated = apply_text_edits(&original, edits)
            .map_err(|err| plan_error("LSP_EDIT_CONFLICT", format!("{}: {err}", path.display())))?;
        planned.push(PlannedWrite {
            path: path.clone(),
            original,
            updated,
        });
    }

    // Validate file ops before committing anything.
    for op in &plan.file_ops {
        match op {
            FileOp::Create { path, overwrite } => {
                if path.exists() && !overwrite {
                    return Err(plan_error(
                        "LSP_EDIT_CONFLICT",
                        format!("create target exists: {}", path.display()),
                    ));
                }
            }
            FileOp::Rename {
                old_path,
                new_path,
                overwrite,
            } => {
                if !old_path.exists() {
                    return Err(plan_error(
                        "LSP_EDIT_CONFLICT",
                        format!("rename source missing: {}", old_path.display()),
                    ));
                }
                if new_path.exists() && !overwrite {
                    return Err(plan_error(
                        "LSP_EDIT_CONFLICT",
                        format!("rename target exists: {}", new_path.display()),
                    ));
                }
            }
            FileOp::Delete { path } => {
                if !path.exists() {
                    return Err(plan_error(
                        "LSP_EDIT_CONFLICT",
                        format!("delete target missing: {}", path.display()),
                    ));
                }
            }
        }
    }

    // ── Phase 2: commit with rollback ───────────────────────────────────
    let mut written: Vec<PathBuf> = Vec::new();
    let mut ops_done: Vec<FileOp> = Vec::new();
    let commit_result: Result<(), crate::error::Error> = (|| {
        for write in &planned {
            write_file_atomic(&write.path, &write.updated).map_err(|err| {
                plan_error(
                    "LSP_EDIT_APPLY",
                    format!("failed writing {}: {err}", write.path.display()),
                )
            })?;
            written.push(write.path.clone());
        }
        for op in &plan.file_ops {
            apply_file_op(op).map_err(|err| {
                plan_error("LSP_EDIT_APPLY", format!("file operation failed: {err}"))
            })?;
            ops_done.push(op.clone());
        }
        Ok(())
    })();

    if let Err(err) = commit_result {
        // Roll back written files from staged originals.
        for write in &planned {
            if written.contains(&write.path) {
                let _ = write_file_atomic(&write.path, &write.original);
            }
        }
        // Best-effort un-apply file ops in reverse.
        for op in ops_done.iter().rev() {
            undo_file_op(op);
        }
        return Err(err);
    }

    Ok(ApplyOutcome {
        files_changed: planned.iter().map(|w| w.path.clone()).collect(),
        file_ops_applied: ops_done.iter().map(describe_file_op).collect(),
    })
}

fn describe_file_op(op: &FileOp) -> String {
    match op {
        FileOp::Create { path, .. } => format!("create {}", path.display()),
        FileOp::Rename {
            old_path, new_path, ..
        } => format!("rename {} -> {}", old_path.display(), new_path.display()),
        FileOp::Delete { path } => format!("delete {}", path.display()),
    }
}

fn apply_file_op(op: &FileOp) -> std::io::Result<()> {
    match op {
        FileOp::Create { path, overwrite } => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if *overwrite {
                std::fs::write(path, "")
            } else {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map(|_| ())
            }
        }
        FileOp::Rename {
            old_path, new_path, ..
        } => {
            if let Some(parent) = new_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(old_path, new_path)
        }
        FileOp::Delete { path } => {
            if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            }
        }
    }
}

fn undo_file_op(op: &FileOp) {
    match op {
        FileOp::Create { path, .. } => {
            let _ = std::fs::remove_file(path);
        }
        FileOp::Rename {
            old_path, new_path, ..
        } => {
            let _ = std::fs::rename(new_path, old_path);
        }
        FileOp::Delete { .. } => {
            // Deletion cannot be un-applied without a snapshot; rollback of
            // text writes still proceeded above. Deletes are staged after
            // text writes, so a delete-commit failure can only strand ops
            // that the server itself requested.
        }
    }
}

/// Write file content atomically via temp-file + rename in the same
/// directory (same-filesystem rename is atomic on POSIX).
fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temp, content.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::text::{Position, Range};

    fn edit(line: u32, start: u32, end: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn parse_changes_form() {
        let raw = serde_json::json!({
            "changes": {
                "file:///tmp/a.rs": [
                    { "range": { "start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3} }, "newText": "XX" }
                ]
            }
        });
        let plan = parse_workspace_edit(&raw).expect("parse");
        assert_eq!(plan.text_edits.len(), 1);
        let edits = &plan.text_edits[&PathBuf::from("/tmp/a.rs")];
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].new_text, "XX");
    }

    #[test]
    fn parse_document_changes_with_file_ops() {
        let raw = serde_json::json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": "file:///tmp/a.rs", "version": 1 },
                    "edits": [
                        { "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} }, "newText": "Z" }
                    ]
                },
                { "kind": "rename", "oldUri": "file:///tmp/old.rs", "newUri": "file:///tmp/new.rs" },
                { "kind": "create", "uri": "file:///tmp/created.rs", "options": { "overwrite": true } },
                { "kind": "delete", "uri": "file:///tmp/gone.rs" }
            ]
        });
        let plan = parse_workspace_edit(&raw).expect("parse");
        assert_eq!(plan.text_edits.len(), 1);
        assert_eq!(plan.file_ops.len(), 3);
        assert!(matches!(plan.file_ops[0], FileOp::Rename { .. }));
        assert!(matches!(
            plan.file_ops[1],
            FileOp::Create {
                overwrite: true,
                ..
            }
        ));
        assert!(matches!(plan.file_ops[2], FileOp::Delete { .. }));
    }

    #[test]
    fn parse_annotated_text_edits() {
        let raw = serde_json::json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": "file:///tmp/a.rs", "version": 1 },
                    "edits": [
                        {
                            "textEdit": { "range": { "start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1} }, "newText": "Z" },
                            "annotationId": "note-1"
                        }
                    ]
                }
            ]
        });
        let plan = parse_workspace_edit(&raw).expect("parse");
        assert_eq!(
            plan.text_edits[&PathBuf::from("/tmp/a.rs")][0].new_text,
            "Z"
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_workspace_edit(&serde_json::json!(42)).is_err());
        let bad = serde_json::json!({ "changes": { "file:///tmp/a.rs": { "not": "array" } } });
        assert!(parse_workspace_edit(&bad).is_err());
    }

    #[test]
    fn apply_is_atomic_on_overlap() {
        let temp = tempfile::tempdir().expect("tempdir");
        let a = temp.path().join("a.txt");
        let b = temp.path().join("b.txt");
        std::fs::write(&a, "abcdef\n").expect("a");
        std::fs::write(&b, "012345\n").expect("b");

        let mut plan = WorkspaceEditPlan::default();
        plan.text_edits.insert(a.clone(), vec![edit(0, 0, 2, "XX")]);
        // Overlapping edits in b: apply must fail with zero writes.
        plan.text_edits
            .insert(b.clone(), vec![edit(0, 1, 4, "Y"), edit(0, 2, 5, "Z")]);
        let err = apply_workspace_edit(&plan, None).expect_err("overlap fails");
        assert!(err.to_string().contains("LSP_EDIT_CONFLICT"), "{err}");
        assert_eq!(std::fs::read_to_string(&a).expect("a"), "abcdef\n");
        assert_eq!(std::fs::read_to_string(&b).expect("b"), "012345\n");
    }

    #[test]
    fn apply_detects_drift() {
        let temp = tempfile::tempdir().expect("tempdir");
        let a = temp.path().join("a.txt");
        std::fs::write(&a, "abcdef\n").expect("a");
        let mut plan = WorkspaceEditPlan::default();
        plan.text_edits.insert(a.clone(), vec![edit(0, 0, 2, "XX")]);
        let mut hashes = HashMap::new();
        hashes.insert(a.clone(), 0xdead_beef_u64); // wrong hash => drift
        let err = apply_workspace_edit(&plan, Some(&hashes)).expect_err("drift fails");
        assert!(err.to_string().contains("changed on disk"), "{err}");
        assert_eq!(std::fs::read_to_string(&a).expect("a"), "abcdef\n");
    }

    #[test]
    fn apply_writes_and_runs_file_ops() {
        let temp = tempfile::tempdir().expect("tempdir");
        let a = temp.path().join("a.txt");
        let old = temp.path().join("old.txt");
        let new = temp.path().join("renamed/new.txt");
        std::fs::write(&a, "abcdef\n").expect("a");
        std::fs::write(&old, "payload\n").expect("old");

        let mut plan = WorkspaceEditPlan::default();
        plan.text_edits.insert(a.clone(), vec![edit(0, 0, 2, "XX")]);
        plan.file_ops.push(FileOp::Rename {
            old_path: old.clone(),
            new_path: new.clone(),
            overwrite: false,
        });
        let outcome = apply_workspace_edit(&plan, None).expect("apply");
        assert_eq!(std::fs::read_to_string(&a).expect("a"), "XXcdef\n");
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(&new).expect("new"), "payload\n");
        assert_eq!(outcome.files_changed, vec![a.clone()]);
        assert_eq!(outcome.file_ops_applied.len(), 1);
    }

    #[test]
    fn apply_validates_file_ops_before_writes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let a = temp.path().join("a.txt");
        std::fs::write(&a, "abcdef\n").expect("a");
        let mut plan = WorkspaceEditPlan::default();
        plan.text_edits.insert(a.clone(), vec![edit(0, 0, 2, "XX")]);
        plan.file_ops.push(FileOp::Rename {
            old_path: temp.path().join("missing.txt"),
            new_path: temp.path().join("new.txt"),
            overwrite: false,
        });
        let err = apply_workspace_edit(&plan, None).expect_err("missing source fails");
        assert!(err.to_string().contains("rename source missing"), "{err}");
        // Text write never happened.
        assert_eq!(std::fs::read_to_string(&a).expect("a"), "abcdef\n");
    }

    #[test]
    fn create_op_refuses_existing_without_overwrite() {
        let temp = tempfile::tempdir().expect("tempdir");
        let a = temp.path().join("a.txt");
        std::fs::write(&a, "here\n").expect("a");
        let mut plan = WorkspaceEditPlan::default();
        plan.file_ops.push(FileOp::Create {
            path: a.clone(),
            overwrite: false,
        });
        let err = apply_workspace_edit(&plan, None).expect_err("conflict");
        assert!(err.to_string().contains("create target exists"), "{err}");
        assert_eq!(std::fs::read_to_string(&a).expect("a"), "here\n");
    }
}
