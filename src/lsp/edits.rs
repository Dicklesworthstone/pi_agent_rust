//! WorkspaceEdit parsing and rollback-safe application.
//!
//! A `WorkspaceEdit` arrives either as `changes: {uri: [TextEdit]}` or as
//! `documentChanges: [...]` mixing `TextDocumentEdit`s with file operations
//! (`CreateFile`/`RenameFile`/`DeleteFile`). All target images are staged before
//! writing. Reported commit failures restore original bytes and permissions,
//! including deleted files and overwritten rename destinations. Incomplete
//! rollback is explicit and retains recovery files instead of hiding data loss.
//! This is bounded regular-file support, not a multi-file crash-atomic commit.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

use super::text::TextEdit;

mod sequence;
mod transaction;

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
    /// Interleaving indices into the public collections, retained for parsed
    /// documentChanges. Application validates these indices before staging.
    sequence: Option<Vec<sequence::Step>>,
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
    sequence::parse(raw)
}

fn parse_text_edit_array(raw: &Value) -> Result<Vec<TextEdit>, crate::error::Error> {
    let Some(edits) = raw.as_array() else {
        return Err(plan_error("LSP_EDIT_MALFORMED", "edits is not an array"));
    };
    let mut out = Vec::with_capacity(edits.len());
    for edit in edits {
        // Standard AnnotatedTextEdit has range/newText/annotationId directly.
        // Also accept the textEdit wrapper used by existing captured fixtures.
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

/// Apply a parsed WorkspaceEdit with staged validation and rollback.
///
/// Validation stages bounded regular-file images before changing targets.
/// Commit rechecks preimages, preserves permissions and retains sibling
/// backups. On failure it restores originals unless another writer changed
/// a committed target; that conflict is reported with a recovery-file path.
/// Directories, symlinks and file/directory shape changes are rejected before
/// commit. There is no claim of cross-file atomic visibility or crash recovery.
///
/// # Errors
///
/// Returns `[LSP_EDIT_CONFLICT]` for drift/overlap/range failures and
/// `[LSP_EDIT_APPLY]` for commit failures with completed rollback,
/// `[LSP_EDIT_ROLLBACK]` for incomplete rollback, and `[LSP_EDIT_LIMIT]` for
/// admission limits. No filesystem sandbox against hostile concurrent path
/// replacement is implied; callers must still enforce workspace scope.
#[allow(clippy::implicit_hasher)] // concrete RandomState keeps `None` call sites inference-free
pub fn apply_workspace_edit(
    plan: &WorkspaceEditPlan,
    expected_hashes: Option<&HashMap<PathBuf, u64>>,
) -> Result<ApplyOutcome, crate::error::Error> {
    transaction::apply(plan, expected_hashes)
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
        assert_eq!(outcome.files_changed, vec![a]);
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
