//! Reconcile independent server edit sets against one original workspace.
//!
//! Versions belong to their originating connection and must be validated by
//! the coordinator BEFORE calling this function. Output uses null versions;
//! it does not transfer one server's version authority to another connection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Result, check_response_size, inside_root, parse_workspace_edit, tool_err, try_path_to_uri};
use crate::lsp::client::uri_to_path;
use crate::lsp::refactor_preview::MAX_REFACTOR_SERVERS;
use crate::lsp::text::Range;

const MAX_EDITS: usize = 32768;

struct Edit {
    range: Range,
    text: String,
    annotation: Option<(String, Arc<Value>)>,
}

pub(super) struct Merged {
    pub(super) edit: Value,
    pub(super) duplicates: usize,
}

fn conflict(message: &str) -> crate::error::Error {
    tool_err("LSP_RENAME_CONFLICT", message)
}

fn overlap(a: Range, b: Range) -> bool {
    // An empty range is a point: it overlaps a range that contains it,
    // including either endpoint.
    if a.start == a.end {
        (b.start..=b.end).contains(&a.start)
    } else if b.start == b.end {
        (a.start..=a.end).contains(&b.start)
    } else {
        a.start < b.end && b.start < a.end
    }
}

/// Bound the combined wire payload before cloning any individual edit strings.
pub(super) fn admit(raws: &[Value]) -> Result<()> {
    if raws.is_empty() || raws.len() > MAX_REFACTOR_SERVERS {
        return Err(tool_err("LSP_EDIT_LIMIT", "rename requires 1..8 server edit sets"));
    }
    struct Limit(usize);
    impl std::io::Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("combined import edits exceed 2 MiB"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    serde_json::to_writer(&mut Limit(crate::lsp::actions::MAX_ACTION_BYTES), raws)
        .map_err(|_| tool_err("LSP_EDIT_LIMIT", "combined import edits exceed 2 MiB"))
}

pub(super) fn combine(raws: &[Value], root: &Path) -> Result<Merged> {
    admit(raws)?;
    let mut documents: BTreeMap<PathBuf, Vec<Edit>> = BTreeMap::new();
    let mut edit_count = 0_usize;
    for (source, raw) in raws.iter().enumerate() {
        let plan = parse_workspace_edit(raw)?;
        if !plan.file_ops.is_empty() {
            return Err(conflict("directory import edits cannot contain server resource operations"));
        }
        let annotations: BTreeMap<_, _> = raw.get("changeAnnotations")
            .and_then(Value::as_object).into_iter().flat_map(|table| table.iter())
            .enumerate().map(|(index, (id, value))| {
                (id.as_str(), (format!("s{source}-a{index}"), Arc::new(value.clone())))
            }).collect();
        let mut seen = BTreeSet::new();
        let mut document = |uri: &str, edits: &Value| -> Result<()> {
            let path = uri_to_path(uri).ok_or_else(|| conflict("unsupported import-edit URI"))?;
            // Scope admission precedes even path canonicalization. Aliases of
            // one existing file cannot hide overlapping independent edits.
            inside_root(&path, root)?;
            let path = path.canonicalize()?;
            if !seen.insert(path.clone()) {
                return Err(conflict("one server returned sequential or aliased edits for the same file; cannot merge their coordinate spaces"));
            }
            let edits = edits.as_array().ok_or_else(|| conflict("invalid import-edit array"))?;
            edit_count = edit_count.checked_add(edits.len()).filter(|count| *count <= MAX_EDITS)
                .ok_or_else(|| tool_err("LSP_EDIT_LIMIT", "combined import edits exceed 32768 edits"))?;
            if !documents.contains_key(&path) && documents.len() >= 1024 {
                return Err(tool_err("LSP_EDIT_LIMIT", "combined import edits exceed 1024 files"));
            }
            let target = documents.entry(path).or_default();
            for edit in edits {
                let inner = edit.get("textEdit").unwrap_or(edit);
                let range: Range = serde_json::from_value(inner["range"].clone())
                    .map_err(|_| conflict("invalid import-edit range"))?;
                if range.end < range.start {
                    return Err(conflict("reversed import-edit range"));
                }
                let text = inner["newText"].as_str()
                    .ok_or_else(|| conflict("import edit is missing newText"))?.to_string();
                let annotation = edit.get("annotationId").or_else(|| inner.get("annotationId"))
                    .and_then(Value::as_str).map(|id| {
                        annotations.get(id).cloned()
                            .ok_or_else(|| conflict("missing import-edit annotation"))
                    }).transpose()?;
                target.push(Edit { range, text, annotation });
            }
            Ok(())
        };
        // Match WorkspaceEdit's normative documentChanges precedence exactly.
        if let Some(changes) = raw.get("documentChanges").and_then(Value::as_array) {
            for change in changes {
                let uri = change["textDocument"]["uri"].as_str()
                    .ok_or_else(|| conflict("missing import-edit document URI"))?;
                document(uri, &change["edits"])?;
            }
        } else if let Some(changes) = raw.get("changes").and_then(Value::as_object) {
            for (uri, edits) in changes { document(uri, edits)?; }
        }
    }
    let mut changes = Vec::with_capacity(documents.len());
    let mut annotations = serde_json::Map::new();
    let mut duplicates = 0;
    for (path, mut edits) in documents {
        edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
        let mut kept: Vec<Edit> = Vec::with_capacity(edits.len());
        for edit in edits {
            if let Some(previous) = kept.last() {
                let same_annotation = previous.annotation.as_ref().map(|(_, value)| value)
                    == edit.annotation.as_ref().map(|(_, value)| value);
                if previous.range == edit.range && previous.text == edit.text && same_annotation {
                    duplicates += 1;
                    continue;
                }
                if overlap(previous.range, edit.range) {
                    return Err(conflict("language servers returned overlapping or differently annotated import edits; no server was preferred and nothing was applied"));
                }
            }
            kept.push(edit);
        }
        let mut output = Vec::with_capacity(kept.len());
        for edit in kept {
            let mut value = json!({"range":edit.range,"newText":edit.text});
            if let Some((id, annotation)) = edit.annotation {
                value["annotationId"] = json!(id);
                annotations.entry(id).or_insert_with(|| (*annotation).clone());
            }
            output.push(value);
        }
        changes.push(json!({"textDocument":{"uri":try_path_to_uri(&path)?,"version":null},"edits":output}));
    }
    let edit = json!({"documentChanges":changes,"changeAnnotations":annotations});
    check_response_size(&edit)?;
    // Reapply native aggregate operation/annotation limits after namespacing.
    parse_workspace_edit(&edit)?;
    Ok(Merged { edit, duplicates })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::edits::PreparedEdit;
    use std::collections::HashMap;

    fn edit(start: u32, end: u32, text: &str) -> Value {
        json!({"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":end}},"newText":text})
    }

    fn workspace() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let path = root.join("source");
        std::fs::write(&path, "abcdef\n").unwrap();
        let uri = try_path_to_uri(&path).unwrap();
        (temp, root, uri)
    }

    fn wire(uri: &str, edits: Vec<Value>) -> Value {
        json!({"changes":{uri:edits}})
    }

    #[test]
    fn independent_import_edits_apply_to_original_coordinates() {
        let (_temp, root, uri) = workspace();
        let merged = combine(&[wire(&uri, vec![edit(0, 1, "long")]), wire(&uri, vec![edit(4, 6, "Z")])], &root).unwrap();
        let prepared = PreparedEdit::new(&parse_workspace_edit(&merged.edit).unwrap(), &HashMap::new()).unwrap();
        assert_eq!(std::fs::read(root.join("source")).unwrap(), b"abcdef\n");
        prepared.commit(|| Ok(())).unwrap();
        assert_eq!(std::fs::read(root.join("source")).unwrap(), b"longbcdZ\n");
        eprintln!("{}", json!({"case":"merge_original_coordinates","event":"verified","sources":2}));
    }

    #[test]
    fn identical_edits_deduplicate_but_conflicts_never_choose_a_server() {
        let (_temp, root, uri) = workspace();
        let a = wire(&uri, vec![edit(1, 3, "X")]);
        let merged = combine(&[a.clone(), a.clone()], &root).unwrap();
        assert_eq!(merged.duplicates, 1);
        assert_eq!(merged.edit["documentChanges"][0]["edits"].as_array().unwrap().len(), 1);
        for b in [edit(1, 3, "Y"), edit(2, 4, "Z"), edit(1, 1, "insert"), edit(3, 3, "insert")] {
            assert!(combine(&[a.clone(), wire(&uri, vec![b])], &root).err().unwrap().to_string().contains("LSP_RENAME_CONFLICT"));
        }
        assert_eq!(std::fs::read(root.join("source")).unwrap(), b"abcdef\n");
        eprintln!("{}", json!({"case":"merge_conflicts","event":"verified","silentPreference":false}));
    }

    #[test]
    fn colliding_annotation_ids_retain_both_confirmation_requirements() {
        let (_temp, root, uri) = workspace();
        let mut raws = Vec::new();
        for (start, label) in [(0, "first server"), (4, "second server")] {
            let mut change = edit(start, start + 1, "X");
            change["annotationId"] = json!("same-id");
            let mut raw = wire(&uri, vec![change]);
            raw["changeAnnotations"] = json!({"same-id":{"label":label,"needsConfirmation":true}});
            raws.push(raw);
        }
        let merged = combine(&raws, &root).unwrap();
        let prepared = PreparedEdit::new(&parse_workspace_edit(&merged.edit).unwrap(), &HashMap::new()).unwrap();
        assert_eq!(prepared.required_annotations().len(), 2);
        assert!(prepared.commit(|| Ok(())).is_err());
        let prepared = PreparedEdit::new(&parse_workspace_edit(&merged.edit).unwrap(), &HashMap::new()).unwrap();
        prepared.commit_reviewed(|| Ok(())).unwrap();
        assert_eq!(std::fs::read(root.join("source")).unwrap(), b"XbcdXf\n");
        eprintln!("{}", json!({"case":"merge_annotations","event":"verified","confirmations":2}));
    }

    #[test]
    fn duplicate_text_cannot_erase_a_distinct_annotation() {
        let (_temp, root, uri) = workspace();
        let plain = wire(&uri, vec![edit(0, 1, "X")]);
        let mut annotated = plain.clone();
        annotated["changes"][&uri][0]["annotationId"] = json!("review");
        annotated["changeAnnotations"] = json!({"review":{"label":"must review","needsConfirmation":true}});
        assert!(combine(&[plain, annotated], &root).is_err());
        eprintln!("{}", json!({"case":"merge_annotation_conflict","event":"verified"}));
    }

    #[test]
    fn connection_versions_are_not_exported_as_cross_server_authority() {
        let (_temp, root, uri) = workspace();
        let raw = json!({"documentChanges":[{"textDocument":{"uri":uri,"version":17},"edits":[edit(0, 1, "X")]}],
            "changes":{"ignored-invalid-uri":[edit(0, 1, "wrong")]}});
        let merged = combine(&[raw], &root).unwrap();
        assert!(merged.edit["documentChanges"][0]["textDocument"]["version"].is_null());
        assert_eq!(merged.edit["documentChanges"].as_array().unwrap().len(), 1);
        eprintln!("{}", json!({"case":"merge_version_scope","event":"verified"}));
    }

    #[test]
    fn sequential_coordinate_spaces_and_resource_operations_are_rejected() {
        let (_temp, root, uri) = workspace();
        let change = json!({"textDocument":{"uri":uri,"version":null},"edits":[edit(0, 1, "X")]});
        assert!(combine(&[json!({"documentChanges":[change.clone(),change]})], &root).is_err());
        assert!(combine(&[json!({"documentChanges":[{"kind":"delete","uri":uri}]})], &root).is_err());
        assert_eq!(std::fs::read(root.join("source")).unwrap(), b"abcdef\n");
        eprintln!("{}", json!({"case":"merge_unsupported_operations","event":"verified"}));
    }

    #[test]
    fn aggregate_limits_apply_before_allocating_or_applying_merged_edits() {
        let (_temp, root, uri) = workspace();
        assert!(combine(&[], &root).is_err());
        assert!(combine(&vec![Value::Null; MAX_REFACTOR_SERVERS + 1], &root).is_err());
        let large = wire(&uri, vec![edit(0, 1, &"x".repeat(1024 * 1024))]);
        assert!(combine(&[large.clone(), large], &root).err().unwrap().to_string().contains("LSP_EDIT_LIMIT"));
        assert_eq!(std::fs::read(root.join("source")).unwrap(), b"abcdef\n");
        eprintln!("{}", json!({"case":"merge_limits","event":"verified"}));
    }
}
