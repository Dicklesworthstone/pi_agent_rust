//! Notebook tool integration: the model's read anchors must edit the source
//! view while persistence, conflict detection and undo retain complete JSON.
//! Saved execution data survives code edits; these tools never execute cells.

use super::{EditTool, HashlineEditTool, ReadTool, Tool, ToolOutput};
use crate::model::ContentBlock;
use crate::undo::FileMutationRecorder;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixture(directory: &Path) -> (PathBuf, Value, Vec<u8>) {
    let document = json!({
        "nbformat": 4,
        "nbformat_minor": 5,
        "metadata": {
            "kernelspec": {"name": "python3", "display_name": "Python 3"},
            "hidden_note": "metadata-sentinel"
        },
        "custom_notebook_field": {"preserve": [1, true, "unknown"]},
        "cells": [
            {
                "cell_type": "code",
                "id": "saved-code",
                "metadata": {"tags": ["important"], "custom": {"enabled": true}},
                "source": ["value = 1\n", "print(value)\n"],
                "execution_count": 7,
                "outputs": [{"output_type": "stream", "name": "stdout", "text": ["saved output sentinel\n"]}],
                "custom_cell_field": ["preserve", 42]
            },
            {
                "cell_type": "markdown",
                "id": "saved-markdown",
                "metadata": {"collapsed": false},
                "source": "# Analysis\r\nKeep this exact source representation.",
                "attachments": {"plot.png": {"image/png": "aW1hZ2U="}}
            },
            {
                "cell_type": "raw",
                "id": "saved-raw",
                "metadata": {"format": "text/plain"},
                "source": ["Retained raw content\r\n"]
            }
        ]
    });
    let mut bytes = serde_json::to_vec_pretty(&document).unwrap();
    bytes.push(b'\n');
    // The adapter must recognize notebook extensions without case sensitivity.
    let path = directory.join("analysis.IPYNB");
    std::fs::write(&path, &bytes).unwrap();
    (path, document, bytes)
}

fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn anchor(view: &str, source: &str) -> String {
    view.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(_, line)| *line == source)
        .map(|(tag, _)| tag.to_string())
        .expect("read must expose the requested editable source line")
}

#[test]
#[allow(clippy::too_many_lines)]
fn native_notebook_read_hashline_edit_and_undo_preserve_saved_state() {
    asupersync::test_utils::run_test(|| async {
        let directory = tempfile::tempdir().unwrap();
        let (path, original, original_bytes) = fixture(directory.path());
        let read = ReadTool::new(directory.path());
        let plain = read
            .execute("plain", json!({"path": path}), None)
            .await
            .unwrap();
        let plain_text = text(&plain);
        assert!(plain_text.contains("→# %% [code] cell:0"));
        assert!(plain_text.contains("→# %% [markdown] cell:1"));
        assert!(!plain_text.contains("saved output sentinel"));
        assert!(!plain_text.contains("metadata-sentinel"));
        assert_eq!(plain.details.as_ref().unwrap()["editable"], true);
        assert_eq!(
            plain.details.as_ref().unwrap()["extractor"],
            "notebook-cells"
        );

        let hashed = read
            .execute("anchors", json!({"path": path, "hashline": true}), None)
            .await
            .unwrap();
        let tag = anchor(&text(&hashed), "value = 1");
        let window = read
            .execute(
                "window",
                json!({
                    "path": path, "hashline": true, "offset": 2, "limit": 1
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(anchor(&text(&window), "value = 1"), tag);
        assert_eq!(window.details.as_ref().unwrap()["outputLines"], 1);
        assert_eq!(window.details.as_ref().unwrap()["nextOffset"], 3);

        let recorder = Arc::new(FileMutationRecorder::default());
        let hashline = HashlineEditTool::new(directory.path())
            .with_mutation_recorder(Some(Arc::clone(&recorder)));
        let output = hashline
            .execute(
                "change-code",
                json!({
                    "path": path,
                    "edits": [{"op": "replace", "pos": tag, "lines": "value = 2"}]
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        let diff = output.details.as_ref().unwrap()["diff"].as_str().unwrap();
        assert!(diff.contains("value = 1") && diff.contains("value = 2"));
        assert!(
            !diff.contains("execution_count"),
            "diff addresses editable source"
        );
        let after_hashline = std::fs::read(&path).unwrap();
        let changed: Value = serde_json::from_slice(&after_hashline).unwrap();
        assert_eq!(changed["metadata"], original["metadata"]);
        assert_eq!(
            changed["custom_notebook_field"],
            original["custom_notebook_field"]
        );
        for field in [
            "id",
            "metadata",
            "custom_cell_field",
            "outputs",
            "execution_count",
        ] {
            assert_eq!(
                changed["cells"][0][field], original["cells"][0][field],
                "{field}"
            );
        }
        assert_eq!(
            changed["cells"][0]["source"],
            json!(["value = 2\n", "print(value)\n"])
        );
        assert_eq!(changed["cells"][1], original["cells"][1]);
        assert_eq!(changed["cells"][2], original["cells"][2]);

        // Changing code does not execute it or refresh its saved result.
        // Converting that cell to markdown does remove execution-only fields.
        let edit =
            EditTool::new(directory.path()).with_mutation_recorder(Some(Arc::clone(&recorder)));
        edit.execute(
            "convert-cell",
            json!({
                "path": path,
                "oldText": "# %% [code] cell:0",
                "newText": "# %% [markdown] cell:0"
            }),
            None,
        )
        .await
        .unwrap();
        let converted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(converted["cells"][0]["id"], "saved-code");
        assert_eq!(
            converted["cells"][0]["metadata"],
            original["cells"][0]["metadata"]
        );
        assert_eq!(converted["cells"][0]["cell_type"], "markdown");
        assert!(converted["cells"][0].get("outputs").is_none());
        assert!(converted["cells"][0].get("execution_count").is_none());
        assert_eq!(recorder.stats().undo_depth, 2);
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), after_hashline);
        assert_eq!(recorder.undo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        assert_eq!(recorder.redo(1, false).applied.len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), after_hashline);
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn raw_notebook_reads_and_both_edit_lanes_operate_on_json() {
    asupersync::test_utils::run_test(|| async {
        let directory = tempfile::tempdir().unwrap();
        let (path, _, original_bytes) = fixture(directory.path());
        let read = ReadTool::new(directory.path());
        let before_view = text(
            &read
                .execute("before", json!({"path": path}), None)
                .await
                .unwrap(),
        );
        let edit = EditTool::new(directory.path());
        edit.execute(
            "not-visible",
            json!({
                "path": path, "oldText": "metadata-sentinel", "newText": "raw-metadata"
            }),
            None,
        )
        .await
        .expect_err("default edits cannot silently address hidden JSON");
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        edit.execute(
            "raw-edit",
            json!({
                "path": path, "oldText": "metadata-sentinel", "newText": "raw-metadata", "raw": true
            }),
            None,
        )
        .await
        .unwrap();

        // raw=true wins over rendered=true, including real JSON hash anchors.
        let raw = read
            .execute(
                "raw-anchors",
                json!({
                    "path": path, "raw": true, "rendered": true, "hashline": true
                }),
                None,
            )
            .await
            .unwrap();
        let raw_text = text(&raw);
        assert!(raw_text.contains("\"nbformat\""));
        assert!(raw_text.contains("saved output sentinel"));
        assert!(raw_text.contains("raw-metadata"));
        assert!(!raw_text.contains("# %% [code] cell:0"));
        let (tag, line) = raw_text
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(_, line)| line.trim() == "\"execution_count\": 7,")
            .unwrap();
        HashlineEditTool::new(directory.path())
            .execute(
                "raw-hashline",
                json!({
                    "path": path, "raw": true,
                    "edits": [{"op": "replace", "pos": tag, "lines": line.replace(": 7", ": 8")}]
                }),
                None,
            )
            .await
            .unwrap();
        let changed: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(changed["metadata"]["hidden_note"], "raw-metadata");
        assert_eq!(changed["cells"][0]["execution_count"], 8);
        let after_view = text(
            &read
                .execute("after", json!({"path": path}), None)
                .await
                .unwrap(),
        );
        assert_eq!(
            after_view, before_view,
            "raw metadata changes do not alter cell source"
        );
    });
}

#[test]
fn invalid_notebook_markers_refuse_both_edit_lanes_without_writing() {
    asupersync::test_utils::run_test(|| async {
        let directory = tempfile::tempdir().unwrap();
        let (path, _, original_bytes) = fixture(directory.path());
        let recorder = Arc::new(FileMutationRecorder::default());
        let read = ReadTool::new(directory.path());
        let view = read
            .execute("read", json!({"path": path, "hashline": true}), None)
            .await
            .unwrap();
        let marker = anchor(&text(&view), "# %% [code] cell:0");
        let edit =
            EditTool::new(directory.path()).with_mutation_recorder(Some(Arc::clone(&recorder)));
        let error = edit
            .execute(
                "invalid-edit",
                json!({
                    "path": path,
                    "oldText": "# %% [code] cell:0",
                    "newText": "# %% [unsupported] cell:0"
                }),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("NOTEBOOK_EDIT"), "{error}");
        let hashline = HashlineEditTool::new(directory.path())
            .with_mutation_recorder(Some(Arc::clone(&recorder)));
        let error = hashline
            .execute(
                "invalid-hashline",
                json!({
                    "path": path,
                    "edits": [{"op": "replace", "pos": marker, "lines": "# %% [code] cell:invalid"}]
                }),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("NOTEBOOK_EDIT"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        assert_eq!(recorder.stats().undo_depth, 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    });
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn notebook_aliases_use_the_same_admitted_file_type_for_read_and_edit() {
    use std::os::unix::fs::symlink;

    asupersync::test_utils::run_test(|| async {
        let directory = tempfile::tempdir().unwrap();
        let (target, original, _) = fixture(directory.path());
        let alias = directory.path().join("alias.txt");
        symlink(&target, &alias).unwrap();
        let read = ReadTool::new(directory.path());
        let view = read
            .execute("alias-read", json!({"path": alias, "hashline": true}), None)
            .await
            .unwrap();
        assert!(text(&view).contains("# %% [code] cell:0"));
        let tag = anchor(&text(&view), "value = 1");
        HashlineEditTool::new(directory.path())
            .execute(
                "alias-hashline",
                json!({
                    "path": alias,
                    "edits": [{"op": "replace", "pos": tag, "lines": "value = 2"}]
                }),
                None,
            )
            .await
            .unwrap();
        EditTool::new(directory.path())
            .execute(
                "alias-edit",
                json!({
                    "path": alias, "oldText": "value = 2", "newText": "value = 3"
                }),
                None,
            )
            .await
            .unwrap();
        let changed: Value = serde_json::from_slice(&std::fs::read(&target).unwrap()).unwrap();
        assert_eq!(
            changed["cells"][0]["source"],
            json!(["value = 3\n", "print(value)\n"])
        );
        assert_eq!(
            changed["cells"][0]["outputs"],
            original["cells"][0]["outputs"]
        );
        assert!(
            std::fs::symlink_metadata(&alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // A notebook-looking alias must not reinterpret an ordinary target.
        let plain = directory.path().join("ordinary.txt");
        let notebook_alias = directory.path().join("ordinary.ipynb");
        std::fs::write(&plain, "plain first\nplain second\n").unwrap();
        symlink(&plain, &notebook_alias).unwrap();
        let view = read
            .execute(
                "plain-read",
                json!({
                    "path": notebook_alias, "hashline": true
                }),
                None,
            )
            .await
            .unwrap();
        let tag = anchor(&text(&view), "plain first");
        HashlineEditTool::new(directory.path())
            .execute(
                "plain-hashline",
                json!({
                    "path": notebook_alias,
                    "edits": [{"op": "replace", "pos": tag, "lines": "changed first"}]
                }),
                None,
            )
            .await
            .unwrap();
        EditTool::new(directory.path())
            .execute(
                "plain-edit",
                json!({
                    "path": notebook_alias, "oldText": "plain second", "newText": "changed second"
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&plain).unwrap(),
            "changed first\nchanged second\n"
        );
        assert!(
            std::fs::symlink_metadata(notebook_alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    });
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
#[test]
fn notebook_edits_detect_concurrent_changes_to_hidden_saved_outputs() {
    use std::os::unix::fs::MetadataExt as _;

    for hashline in [false, true] {
        asupersync::test_utils::run_test(|| async {
            let directory = tempfile::tempdir().unwrap();
            let (path, mut concurrent, _) = fixture(directory.path());
            let read = ReadTool::new(directory.path());
            let view = read
                .execute("read", json!({"path": path, "hashline": true}), None)
                .await
                .unwrap();
            let tag = anchor(&text(&view), "value = 1");
            let inode = std::fs::metadata(&path).unwrap().ino();
            concurrent["cells"][0]["outputs"][0]["text"] =
                json!(["new output from another writer\n"]);
            let concurrent_bytes = serde_json::to_vec_pretty(&concurrent).unwrap();
            let hook_path = path.clone();
            let hook_bytes = concurrent_bytes.clone();
            let hook = move || {
                std::fs::write(&hook_path, &hook_bytes).unwrap();
                assert_eq!(std::fs::metadata(&hook_path).unwrap().ino(), inode);
            };
            let recorder = Arc::new(FileMutationRecorder::default());
            let tool: Box<dyn Tool> = if hashline {
                Box::new(
                    HashlineEditTool::with_before_persist_hook(directory.path(), hook)
                        .with_mutation_recorder(Some(Arc::clone(&recorder))),
                )
            } else {
                Box::new(
                    EditTool::with_before_persist_hook(directory.path(), hook)
                        .with_mutation_recorder(Some(Arc::clone(&recorder))),
                )
            };
            let input = if hashline {
                json!({"path": path, "edits": [{"op": "replace", "pos": tag, "lines": "value = 2"}]})
            } else {
                json!({"path": path, "oldText": "value = 1", "newText": "value = 2"})
            };
            let error = tool
                .execute("concurrent-edit", input, None)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("changed since it was read"),
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), concurrent_bytes);
            assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
            assert_eq!(recorder.stats().undo_depth, 0);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        });
    }
}
