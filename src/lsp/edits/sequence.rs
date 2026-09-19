//! Preserve WorkspaceEdit document order without duplicating text payloads.
//!
//! Steps index the public path/edit collections used by workspace scope checks.
//! Validate that every indexed edit is still represented before staging; an
//! embedder mutating a parsed plan cannot leave hidden out-of-scope operations.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::transaction::Transaction;
use super::{FileOp, WorkspaceEditPlan, parse_text_edit_array, plan_error};
use crate::error::Result;
use crate::lsp::client::uri_to_path;

const MAX_STEPS: usize = 4096;
const MAX_TEXT_EDITS: usize = 32768;
const MAX_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug)]
pub(super) enum Step {
    Text {
        path: PathBuf,
        edits: Range<usize>,
    },
    File {
        index: usize,
        ignore_exists: bool,
        ignore_missing: bool,
    },
}

#[derive(Default)]
struct Budget {
    steps: usize,
    edits: usize,
    bytes: usize,
}

impl Budget {
    fn step(&mut self) -> Result<()> {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            return Err(plan_error(
                "LSP_EDIT_LIMIT",
                "workspace edit exceeds 4096 operations",
            ));
        }
        Ok(())
    }

    fn bytes(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|size| *size <= MAX_PAYLOAD_BYTES)
            .ok_or_else(|| plan_error("LSP_EDIT_LIMIT", "workspace edit payload exceeds 32 MiB"))?;
        Ok(())
    }

    fn path(&mut self, raw: &Value, field: &str) -> Result<PathBuf> {
        let uri = raw
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| malformed(format!("missing string {field}")))?;
        self.uri(uri)
    }

    fn uri(&mut self, uri: &str) -> Result<PathBuf> {
        self.bytes(uri.len())?;
        uri_to_path(uri).ok_or_else(|| malformed(format!("unsupported uri {uri:?}")))
    }

    fn text(&mut self, raw: &Value) -> Result<Vec<crate::lsp::text::TextEdit>> {
        let edits = raw
            .as_array()
            .ok_or_else(|| malformed("edits is not an array"))?;
        self.edits = self
            .edits
            .checked_add(edits.len())
            .filter(|count| *count <= MAX_TEXT_EDITS)
            .ok_or_else(|| {
                plan_error("LSP_EDIT_LIMIT", "workspace edit exceeds 32768 text edits")
            })?;
        for edit in edits {
            let edit = edit.get("textEdit").unwrap_or(edit);
            if let Some(text) = edit.get("newText").and_then(Value::as_str) {
                self.bytes(text.len())?;
            }
        }
        parse_text_edit_array(raw)
    }
}

fn malformed(message: impl Into<String>) -> crate::error::Error {
    plan_error("LSP_EDIT_MALFORMED", message)
}

fn option(entry: &Value, name: &str) -> Result<bool> {
    let Some(options) = entry.get("options") else {
        return Ok(false);
    };
    let options = options
        .as_object()
        .ok_or_else(|| malformed("resource options must be an object"))?;
    match options.get(name) {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(malformed(format!("{name} must be a boolean"))),
    }
}

pub(super) fn parse(raw: &Value) -> Result<WorkspaceEditPlan> {
    let mut plan = WorkspaceEditPlan::default();
    if raw.is_null() {
        return Ok(plan);
    }
    let object = raw
        .as_object()
        .ok_or_else(|| malformed("WorkspaceEdit is not an object"))?;
    let mut budget = Budget::default();
    // The LSP prefers documentChanges over changes when both are present.
    // Applying both duplicates edits and changes the meaning of the request.
    if let Some(raw) = object.get("documentChanges") {
        let entries = raw
            .as_array()
            .ok_or_else(|| malformed("documentChanges must be an array"))?;
        let mut sequence = Vec::new();
        for entry in entries {
            budget.step()?;
            sequence.push(parse_step(entry, &mut plan, &mut budget)?);
        }
        plan.sequence = Some(sequence);
    } else if let Some(raw) = object.get("changes") {
        let changes = raw
            .as_object()
            .ok_or_else(|| malformed("changes must be an object"))?;
        for (uri, raw_edits) in changes {
            budget.step()?;
            let path = budget.uri(uri)?;
            let edits = budget.text(raw_edits)?;
            plan.text_edits.entry(path).or_default().extend(edits);
        }
    }
    Ok(plan)
}

fn parse_step(entry: &Value, plan: &mut WorkspaceEditPlan, budget: &mut Budget) -> Result<Step> {
    if !entry.is_object() {
        return Err(malformed("documentChanges entries must be objects"));
    }
    let Some(kind) = entry.get("kind") else {
        let document = entry
            .get("textDocument")
            .ok_or_else(|| malformed("TextDocumentEdit missing textDocument"))?;
        // This is wire-type validation, not proof of a matching open-buffer
        // version. Request-time hashes remain the caller's freshness contract.
        if let Some(version) = document.get("version")
            && !version.is_null()
            && version
                .as_i64()
                .and_then(|value| i32::try_from(value).ok())
                .is_none()
        {
            return Err(malformed("textDocument.version must be an integer or null"));
        }
        let path = budget.path(document, "uri")?;
        let raw_edits = entry
            .get("edits")
            .ok_or_else(|| malformed("TextDocumentEdit missing edits"))?;
        let parsed = budget.text(raw_edits)?;
        let target = plan.text_edits.entry(path.clone()).or_default();
        let start = target.len();
        target.extend(parsed);
        return Ok(Step::Text {
            path,
            edits: start..target.len(),
        });
    };
    let kind = kind
        .as_str()
        .ok_or_else(|| malformed("documentChanges kind must be a string"))?;
    let (operation, ignore_exists, ignore_missing) = match kind {
        "create" => (
            FileOp::Create {
                path: budget.path(entry, "uri")?,
                overwrite: option(entry, "overwrite")?,
            },
            option(entry, "ignoreIfExists")?,
            false,
        ),
        "rename" => (
            FileOp::Rename {
                old_path: budget.path(entry, "oldUri")?,
                new_path: budget.path(entry, "newUri")?,
                overwrite: option(entry, "overwrite")?,
            },
            option(entry, "ignoreIfExists")?,
            false,
        ),
        "delete" => {
            // Recursive is well-typed but does not grant directory support:
            // the transaction rejects directories before changing anything.
            let _recursive = option(entry, "recursive")?;
            (
                FileOp::Delete {
                    path: budget.path(entry, "uri")?,
                },
                false,
                option(entry, "ignoreIfNotExists")?,
            )
        }
        other => return Err(malformed(format!("unknown documentChanges kind {other:?}"))),
    };
    let index = plan.file_ops.len();
    plan.file_ops.push(operation);
    Ok(Step::File {
        index,
        ignore_exists,
        ignore_missing,
    })
}

fn validate_sequence(plan: &WorkspaceEditPlan, sequence: &[Step]) -> Result<()> {
    let mut offsets: HashMap<&Path, usize> = HashMap::new();
    let mut files = 0;
    for step in sequence {
        match step {
            Step::Text { path, edits } => {
                let target = plan
                    .text_edits
                    .get(path)
                    .ok_or_else(|| malformed("ordered text target removed from plan"))?;
                let offset = offsets.entry(path.as_path()).or_default();
                if edits.start != *offset || edits.end < edits.start || edits.end > target.len() {
                    return Err(malformed("ordered text ranges no longer match plan"));
                }
                *offset = edits.end;
            }
            Step::File { index, .. } => {
                if *index != files || *index >= plan.file_ops.len() {
                    return Err(malformed("ordered file operations no longer match plan"));
                }
                files += 1;
            }
        }
    }
    if files != plan.file_ops.len()
        || offsets.len() != plan.text_edits.len()
        || plan
            .text_edits
            .iter()
            .any(|(path, edits)| offsets.get(path.as_path()).copied() != Some(edits.len()))
    {
        return Err(malformed(
            "plan contains changes not represented in document order",
        ));
    }
    Ok(())
}

pub(super) fn stage(plan: &WorkspaceEditPlan) -> Result<Transaction> {
    let mut transaction = Transaction::default();
    if let Some(sequence) = &plan.sequence {
        validate_sequence(plan, sequence)?;
        for step in sequence {
            match step {
                Step::Text { path, edits } => {
                    transaction.edit(path, &plan.text_edits[path][edits.clone()])?;
                }
                Step::File {
                    index,
                    ignore_exists,
                    ignore_missing,
                } => {
                    transaction.file_op(&plan.file_ops[*index], *ignore_exists, *ignore_missing)?;
                }
            }
        }
    } else {
        // Unversioned changes and programmatically assembled plans have no
        // interleaving metadata. Keep their deterministic text-then-file order.
        let mut text: Vec<_> = plan.text_edits.iter().collect();
        text.sort_by_key(|(path, _)| *path);
        for (path, edits) in text {
            transaction.edit(path, edits)?;
        }
        for operation in &plan.file_ops {
            transaction.file_op(operation, false, false)?;
        }
    }
    Ok(transaction)
}

#[cfg(test)]
mod tests;
