use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::super::{MAX_FILE_BYTES, Transaction, evidence::PreparedEdit};
use super::DirectoryMove;
use crate::lsp::client::try_path_to_uri;
use crate::lsp::edits::{parse_workspace_edit, sequence};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    old: PathBuf,
    new: PathBuf,
}

impl Fixture {
    fn new(case: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let old = root.join("old");
        let new = root.join("nested/new");
        std::fs::create_dir_all(old.join("empty/deeper")).unwrap();
        std::fs::write(old.join("lib.rs"), "old\n").unwrap();
        std::fs::write(old.join("data.bin"), [0, 255, 0, 128]).unwrap();
        std::fs::write(root.join("imports.rs"), "old\n").unwrap();
        eprintln!(
            "{}",
            json!({"case":case,"event":"directory_fixture","files":3,"emptyDirectories":2})
        );
        Self {
            _temp: temp,
            root,
            old,
            new,
        }
    }

    fn rename(&self) -> Value {
        json!({"kind":"rename","oldUri":uri(&self.old),"newUri":uri(&self.new)})
    }

    fn plan(&self, imports: bool) -> Value {
        let mut changes = Vec::new();
        if imports {
            changes.push(edit(&self.old.join("lib.rs")));
            changes.push(edit(&self.root.join("imports.rs")));
        }
        changes.push(self.rename());
        json!({"documentChanges":changes})
    }

    fn unchanged(&self) {
        assert_eq!(std::fs::read(self.old.join("lib.rs")).unwrap(), b"old\n");
        assert_eq!(
            std::fs::read(self.root.join("imports.rs")).unwrap(),
            b"old\n"
        );
        assert!(!self.new.exists());
    }
}

fn uri(path: &Path) -> String {
    try_path_to_uri(path).unwrap()
}

fn edit(path: &Path) -> Value {
    json!({"textDocument":{"uri":uri(path),"version":null},"edits":[{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
        "newText":"new"
    }]})
}

fn staged(raw: &Value) -> Transaction {
    sequence::stage(&parse_workspace_edit(raw).unwrap()).unwrap()
}

#[test]
fn moves_the_original_tree_after_imports_and_leaves_no_scratch() {
    let fixture = Fixture::new("directory_imports");
    let outcome = staged(&fixture.plan(true)).commit().unwrap();
    assert!(!fixture.old.exists());
    assert_eq!(std::fs::read(fixture.new.join("lib.rs")).unwrap(), b"new\n");
    assert_eq!(
        std::fs::read(fixture.root.join("imports.rs")).unwrap(),
        b"new\n"
    );
    assert_eq!(
        std::fs::read(fixture.new.join("data.bin")).unwrap(),
        [0, 255, 0, 128]
    );
    assert!(fixture.new.join("empty/deeper").is_dir());
    assert!(outcome.files_changed.contains(&fixture.new.join("lib.rs")));
    assert_eq!(outcome.file_ops_applied.len(), 1);
    for directory in [&fixture.root, &fixture.new] {
        assert!(std::fs::read_dir(directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pi-lsp-")
        }));
    }
}

#[test]
fn prepared_preview_preserves_tree_and_detects_new_entries() {
    let fixture = Fixture::new("directory_preview_drift");
    let plan = parse_workspace_edit(&fixture.plan(true)).unwrap();
    let prepared = PreparedEdit::new(&plan, &HashMap::new()).unwrap();
    fixture.unchanged();
    assert!(!fixture.root.join("nested").exists());
    let summary = prepared.summary(&fixture.root);
    assert!(
        summary
            .iter()
            .any(|item| item["movedTo"] == "nested/new/data.bin")
    );
    assert!(summary.iter().any(|item| item["kind"] == "directoryRename"));
    std::fs::write(fixture.old.join("new-file"), "keep me").unwrap();
    assert!(prepared.commit(|| Ok(())).is_err());
    fixture.unchanged();
    assert_eq!(
        std::fs::read(fixture.old.join("new-file")).unwrap(),
        b"keep me"
    );
}

#[test]
fn unedited_binary_drift_invalidates_review() {
    let fixture = Fixture::new("directory_binary_drift");
    let plan = parse_workspace_edit(&fixture.plan(false)).unwrap();
    let prepared = PreparedEdit::new(&plan, &HashMap::new()).unwrap();
    std::fs::write(fixture.old.join("data.bin"), [1, 2, 3]).unwrap();
    assert!(prepared.verify().is_err());
    fixture.unchanged();
}

#[test]
fn failure_before_move_rolls_back_inside_and_outside_imports() {
    let fixture = Fixture::new("directory_rollback");
    let error = staged(&fixture.plan(true))
        .commit_with(|index, _| {
            if index == 1 {
                Err(io::Error::other("injected edit failure"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
    assert!(error.to_string().contains("original files restored"));
    fixture.unchanged();
    assert!(!fixture.root.join("nested").exists());
}

#[test]
fn destination_appearing_after_edits_prevents_move_and_preserves_foreign_data() {
    let fixture = Fixture::new("directory_destination_race");
    let error = staged(&fixture.plan(true))
        .commit_with(|index, _| {
            if index == 1 {
                std::fs::create_dir(&fixture.new)?;
                std::fs::write(fixture.new.join("foreign"), "keep")?;
            }
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("destination already exists"));
    assert_eq!(std::fs::read(fixture.old.join("lib.rs")).unwrap(), b"old\n");
    assert_eq!(
        std::fs::read(fixture.root.join("imports.rs")).unwrap(),
        b"old\n"
    );
    assert_eq!(std::fs::read(fixture.new.join("foreign")).unwrap(), b"keep");
}

#[test]
fn tree_drift_at_final_move_boundary_rolls_back_imports() {
    let fixture = Fixture::new("directory_late_drift");
    let error = staged(&fixture.plan(true))
        .commit_with(|index, _| {
            if index == 1 {
                std::fs::write(fixture.old.join("late"), "keep")?;
            }
            Ok(())
        })
        .unwrap_err();
    assert!(error.to_string().contains("tree changed"));
    fixture.unchanged();
    assert_eq!(std::fs::read(fixture.old.join("late")).unwrap(), b"keep");
}

#[test]
fn rejects_overlapping_paths_and_nonfinal_or_mixed_resource_operations() {
    let fixture = Fixture::new("directory_shape_rejection");
    assert!(DirectoryMove::capture(&fixture.old, &fixture.old.join("child")).is_err());
    assert!(DirectoryMove::capture(&fixture.old, &fixture.root).is_err());
    for changes in [
        vec![fixture.rename(), edit(&fixture.new.join("lib.rs"))],
        vec![
            json!({"kind":"create","uri":uri(&fixture.root.join("extra"))}),
            fixture.rename(),
        ],
    ] {
        let plan = parse_workspace_edit(&json!({"documentChanges":changes})).unwrap();
        assert!(sequence::stage(&plan).is_err());
    }
    fixture.unchanged();
    assert!(!fixture.root.join("extra").exists());
}

#[test]
fn snapshot_admission_rejects_oversized_or_excessively_deep_trees() {
    let fixture = Fixture::new("directory_limits");
    let huge = std::fs::File::create(fixture.old.join("huge")).unwrap();
    huge.set_len(MAX_FILE_BYTES as u64 + 1).unwrap();
    let plan = parse_workspace_edit(&fixture.plan(false)).unwrap();
    assert!(sequence::stage(&plan).is_err());
    fixture.unchanged();
    let root = fixture.root.join("deep");
    let mut path = root.clone();
    for _ in 0..66 {
        path.push("d");
    }
    std::fs::create_dir_all(path).unwrap();
    assert!(DirectoryMove::capture(&root, &fixture.root.join("deep-new")).is_err());
}

#[cfg(unix)]
#[test]
fn symlinks_and_shared_hardlinks_are_not_moved() {
    let fixture = Fixture::new("directory_links");
    std::os::unix::fs::symlink(fixture.root.join("imports.rs"), fixture.old.join("link")).unwrap();
    let plan = parse_workspace_edit(&fixture.plan(false)).unwrap();
    assert!(sequence::stage(&plan).is_err());
    fixture.unchanged();
    let other = Fixture::new("directory_hardlinks");
    std::fs::hard_link(other.root.join("imports.rs"), other.old.join("linked")).unwrap();
    let plan = parse_workspace_edit(&other.plan(false)).unwrap();
    assert!(sequence::stage(&plan).is_err());
    other.unchanged();
}

#[cfg(unix)]
#[test]
fn directory_permissions_and_identity_survive_native_move() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let fixture = Fixture::new("directory_identity");
    std::fs::set_permissions(
        fixture.old.join("empty"),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();
    let original = std::fs::metadata(&fixture.old).unwrap();
    staged(&fixture.plan(true)).commit().unwrap();
    let moved = std::fs::metadata(&fixture.new).unwrap();
    assert_eq!(original.ino(), moved.ino());
    assert_eq!(original.dev(), moved.dev());
    assert_eq!(
        std::fs::metadata(fixture.new.join("empty")).unwrap().mode() & 0o777,
        0o750
    );
}
