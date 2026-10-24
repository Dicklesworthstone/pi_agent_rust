//! Retain approval requirements instead of erasing AnnotatedTextEdit metadata.
//!
//! Only references in the operative WorkspaceEdit form select annotations.
//! The entire table is validated and bounded, including unreferenced entries.
//! Labels are descriptive data, never commands, selectors or filesystem paths.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::plan_error;
use crate::error::Result;

const MAX_ANNOTATIONS: usize = 128;
const MAX_ID_BYTES: usize = 128;
const MAX_LABEL_BYTES: usize = 1024;
const MAX_DESCRIPTION_BYTES: usize = 8192;
const MAX_ANNOTATION_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub(super) struct Annotations(BTreeMap<String, Value>);

impl Annotations {
    pub(super) fn value(&self) -> Value {
        serde_json::json!(self.0)
    }

    pub(super) fn required(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, value)| value["needsConfirmation"] == true)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub(super) fn require_no_confirmation(&self) -> Result<()> {
        if self
            .0
            .values()
            .any(|value| value["needsConfirmation"] == true)
        {
            return Err(plan_error(
                "LSP_EDIT_CONFIRMATION_REQUIRED",
                "edit requires change-annotation confirmation; request a preview and approve its refactorId",
            ));
        }
        Ok(())
    }
}

fn malformed(message: &str) -> crate::error::Error {
    plan_error("LSP_EDIT_MALFORMED", message)
}

fn id(value: &Value) -> Result<&str> {
    value
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
        .ok_or_else(|| malformed("annotationId must be a nonempty string of at most 128 bytes"))
}

fn table(raw: &Value) -> Result<Option<&Map<String, Value>>> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("change annotation byte limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let Some(value) = raw.get("changeAnnotations") else {
        return Ok(None);
    };
    let table = value
        .as_object()
        .ok_or_else(|| malformed("changeAnnotations must be an object"))?;
    if table.len() > MAX_ANNOTATIONS {
        return Err(plan_error(
            "LSP_EDIT_LIMIT",
            "workspace edit exceeds 128 change annotations",
        ));
    }
    // Count serialized bytes before cloning, including escaping and any
    // extension metadata. Large unused entries cannot evade admission.
    serde_json::to_writer(&mut Counter(MAX_ANNOTATION_BYTES), table)
        .map_err(|_| plan_error("LSP_EDIT_LIMIT", "changeAnnotations exceeds 64 KiB"))?;
    for (key, annotation) in table {
        if key.is_empty() || key.len() > MAX_ID_BYTES {
            return Err(malformed(
                "change annotation identifiers must be 1..128 bytes",
            ));
        }
        let annotation = annotation
            .as_object()
            .ok_or_else(|| malformed("change annotation must be an object"))?;
        if annotation
            .get("label")
            .and_then(Value::as_str)
            .is_none_or(|label| label.is_empty() || label.len() > MAX_LABEL_BYTES)
        {
            return Err(malformed("change annotation label must be 1..1024 bytes"));
        }
        if annotation
            .get("needsConfirmation")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(malformed("needsConfirmation must be boolean"));
        }
        if annotation.get("description").is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|text| text.len() > MAX_DESCRIPTION_BYTES)
        }) {
            return Err(malformed(
                "annotation description must be a string of at most 8192 bytes",
            ));
        }
    }
    Ok(Some(table))
}

fn reference(
    value: Option<&Value>,
    table: Option<&Map<String, Value>>,
    used: &mut Annotations,
) -> Result<()> {
    let Some(value) = value else { return Ok(()) };
    let id = id(value)?;
    let annotation = table
        .and_then(|table| table.get(id))
        .ok_or_else(|| malformed("annotationId has no changeAnnotations definition"))?;
    // Clone each selected annotation at most once even for many occurrences.
    if !used.0.contains_key(id) {
        used.0.insert(id.to_string(), annotation.clone());
    }
    Ok(())
}

fn text_references(
    edits: &Value,
    table: Option<&Map<String, Value>>,
    used: &mut Annotations,
) -> Result<()> {
    let edits = edits
        .as_array()
        .ok_or_else(|| malformed("edits must be an array"))?;
    for edit in edits {
        let outer = edit.get("annotationId");
        let inner = edit
            .get("textEdit")
            .and_then(|inner| inner.get("annotationId"));
        if outer.zip(inner).is_some_and(|(a, b)| a != b) {
            return Err(malformed(
                "textEdit wrapper has conflicting annotation identifiers",
            ));
        }
        reference(outer.or(inner), table, used)?;
    }
    Ok(())
}

pub(super) fn parse(raw: &Value) -> Result<Annotations> {
    let table = table(raw)?;
    let mut used = Annotations::default();
    // Exactly match sequence::parse precedence. Ignored legacy changes must
    // neither manufacture confirmation nor authorize the effective changes.
    if let Some(changes) = raw.get("documentChanges") {
        let changes = changes
            .as_array()
            .ok_or_else(|| malformed("documentChanges must be an array"))?;
        for change in changes {
            if change.get("kind").is_some() {
                reference(change.get("annotationId"), table, &mut used)?;
            } else {
                if change.get("annotationId").is_some() {
                    return Err(malformed(
                        "TextDocumentEdit annotations belong on individual edits",
                    ));
                }
                text_references(&change["edits"], table, &mut used)?;
            }
        }
    } else if let Some(changes) = raw.get("changes") {
        let changes = changes
            .as_object()
            .ok_or_else(|| malformed("changes must be an object"))?;
        for edits in changes.values() {
            text_references(edits, table, &mut used)?;
        }
    }
    Ok(used)
}

#[cfg(test)]
mod tests;
