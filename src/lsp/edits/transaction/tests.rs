use super::*;
use crate::lsp::text::{Position, Range};

fn replace_first(text: &str) -> TextEdit {
    TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        },
        new_text: text.to_string(),
    }
}

#[test]
fn every_commit_failure_restores_deleted_overwritten_and_renamed_bytes() {
    // Each case performs real writes, then fails after a different committed
    // target. Include non-UTF-8 preimages: a text-only backup cannot undo these.
    for fail_after in 0..6 {
        let temp = tempfile::tempdir().expect("workspace");
        let root = temp.path();
        let originals: [(&str, &[u8]); 5] = [
            ("a", b"abc"),
            ("b", b"\xff\x00create destination"),
            ("c", b"\xfe\x00rename source"),
            ("d", b"\xfd\x00rename destination"),
            ("e", b"\xfc\x00deleted file"),
        ];
        for (name, bytes) in originals {
            std::fs::write(root.join(name), bytes).expect("original");
        }
        let mut transaction = Transaction::default();
        transaction
            .edit(&root.join("a"), &[replace_first("Z")])
            .unwrap();
        transaction
            .file_op(
                &FileOp::Create {
                    path: root.join("b"),
                    overwrite: true,
                },
                false,
                false,
            )
            .unwrap();
        transaction
            .file_op(
                &FileOp::Rename {
                    old_path: root.join("c"),
                    new_path: root.join("d"),
                    overwrite: true,
                },
                false,
                false,
            )
            .unwrap();
        transaction
            .file_op(
                &FileOp::Delete {
                    path: root.join("e"),
                },
                false,
                false,
            )
            .unwrap();
        transaction
            .file_op(
                &FileOp::Create {
                    path: root.join("new/nested/f"),
                    overwrite: false,
                },
                false,
                false,
            )
            .unwrap();
        let mut observed = 0;
        let error = transaction
            .commit_with(|index, _| {
                observed += 1;
                if index == fail_after {
                    Err(io::Error::other("injected commit failure"))
                } else {
                    Ok(())
                }
            })
            .expect_err("rollback");
        assert_eq!(observed, fail_after + 1);
        assert!(error.to_string().contains("LSP_EDIT_APPLY"), "{error}");
        for (name, bytes) in originals {
            assert_eq!(
                std::fs::read(root.join(name)).unwrap(),
                bytes,
                "{name}, step {fail_after}"
            );
        }
        assert!(
            !root.join("new").exists(),
            "owned parent directories must roll back"
        );
        assert_eq!(std::fs::read_dir(root).unwrap().count(), originals.len());
    }
}

#[test]
fn chained_binary_renames_use_the_staged_source() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    let c = temp.path().join("c");
    std::fs::write(&a, b"\xff\x00payload").unwrap();
    let mut transaction = Transaction::default();
    for (old_path, new_path) in [(&a, &b), (&b, &c)] {
        transaction
            .file_op(
                &FileOp::Rename {
                    old_path: old_path.clone(),
                    new_path: new_path.clone(),
                    overwrite: false,
                },
                false,
                false,
            )
            .unwrap();
    }
    let outcome = transaction.commit().unwrap();
    assert!(!a.exists() && !b.exists());
    assert_eq!(std::fs::read(c).unwrap(), b"\xff\x00payload");
    assert_eq!(outcome.file_ops_applied.len(), 2);
}

#[test]
fn drift_before_commit_is_rejected_without_overwriting_an_external_edit() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    std::fs::write(&path, "original").unwrap();
    let mut transaction = Transaction::default();
    transaction.edit(&path, &[replace_first("Z")]).unwrap();
    std::fs::write(&path, "external edit").unwrap();
    let error = transaction.commit().expect_err("drift");
    assert!(error.to_string().contains("LSP_EDIT_CONFLICT"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "external edit");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
}

#[test]
fn rollback_conflict_retains_the_preimage_and_reports_incomplete_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("source");
    std::fs::write(&path, "original").unwrap();
    let mut transaction = Transaction::default();
    transaction.edit(&path, &[replace_first("Z")]).unwrap();
    let error = transaction
        .commit_with(|_, changed| {
            std::fs::write(changed, "external edit")?;
            Err(io::Error::other("fail after concurrent edit"))
        })
        .expect_err("rollback conflict");
    assert!(error.to_string().contains("LSP_EDIT_ROLLBACK"), "{error}");
    assert!(
        error.to_string().contains("original retained at"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "external edit");
    let backups: Vec<_> = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".pi-lsp-backup-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read(&backups[0]).unwrap(), b"original");
}

#[test]
fn newly_appearing_destination_is_not_overwritten_and_prior_changes_roll_back() {
    let temp = tempfile::tempdir().unwrap();
    let a = temp.path().join("a");
    let z = temp.path().join("z");
    std::fs::write(&a, "original").unwrap();
    let mut transaction = Transaction::default();
    transaction.edit(&a, &[replace_first("Z")]).unwrap();
    transaction
        .file_op(
            &FileOp::Create {
                path: z.clone(),
                overwrite: false,
            },
            false,
            false,
        )
        .unwrap();
    let error = transaction
        .commit_with(|index, _| {
            if index == 0 {
                std::fs::write(&z, "external new file")?;
            }
            Ok(())
        })
        .expect_err("destination appeared");
    assert!(error.to_string().contains("LSP_EDIT_APPLY"), "{error}");
    assert_eq!(std::fs::read(a).unwrap(), b"original");
    assert_eq!(std::fs::read(z).unwrap(), b"external new file");
}

#[cfg(unix)]
#[test]
fn executable_permissions_survive_text_updates_renames_and_rollback() {
    use std::os::unix::fs::PermissionsExt;

    for fail in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::fs::write(&a, "abc").unwrap();
        std::fs::set_permissions(&a, Permissions::from_mode(0o751)).unwrap();
        let mut transaction = Transaction::default();
        transaction.edit(&a, &[replace_first("Z")]).unwrap();
        transaction
            .file_op(
                &FileOp::Rename {
                    old_path: a.clone(),
                    new_path: b.clone(),
                    overwrite: false,
                },
                false,
                false,
            )
            .unwrap();
        let result = transaction.commit_with(|index, _| {
            if fail && index == 1 {
                Err(io::Error::other("after deleting source"))
            } else {
                Ok(())
            }
        });
        let survivor = if fail {
            assert!(result.is_err());
            assert!(!b.exists());
            assert_eq!(std::fs::read(&a).unwrap(), b"abc");
            a
        } else {
            result.unwrap();
            assert!(!a.exists());
            assert_eq!(std::fs::read(&b).unwrap(), b"Zbc");
            b
        };
        assert_eq!(
            std::fs::metadata(survivor).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }
}

#[test]
fn directories_and_overlapping_file_shapes_are_rejected_before_commit() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("directory")).unwrap();
    std::fs::write(temp.path().join("directory/child"), "keep").unwrap();
    let mut transaction = Transaction::default();
    assert!(
        transaction
            .file_op(
                &FileOp::Delete {
                    path: temp.path().join("directory"),
                },
                false,
                false
            )
            .is_err()
    );
    assert_eq!(
        std::fs::read(temp.path().join("directory/child")).unwrap(),
        b"keep"
    );
    transaction
        .file_op(
            &FileOp::Create {
                path: temp.path().join("new"),
                overwrite: false,
            },
            false,
            false,
        )
        .unwrap();
    assert!(
        transaction
            .file_op(
                &FileOp::Create {
                    path: temp.path().join("new/child"),
                    overwrite: false,
                },
                false,
                false
            )
            .is_err()
    );
    assert!(!temp.path().join("new").exists());
}

#[cfg(unix)]
#[test]
fn final_symlinks_are_never_followed_for_write_or_delete() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("target");
    let link = temp.path().join("link");
    std::fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let mut transaction = Transaction::default();
    assert!(transaction.edit(&link, &[replace_first("Z")]).is_err());
    assert!(
        transaction
            .file_op(&FileOp::Delete { path: link }, false, false)
            .is_err()
    );
    assert_eq!(std::fs::read(target).unwrap(), b"keep");
}

#[test]
fn sparse_oversized_preimages_and_total_allocation_overflow_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("large");
    File::create(&path)
        .unwrap()
        .set_len(MAX_FILE_BYTES as u64 + 1)
        .unwrap();
    let mut transaction = Transaction::default();
    assert!(
        transaction
            .file_op(&FileOp::Delete { path: path.clone() }, false, false)
            .is_err()
    );
    assert_eq!(
        std::fs::metadata(path).unwrap().len(),
        MAX_FILE_BYTES as u64 + 1
    );
    transaction.spend(MAX_TRANSACTION_BYTES).unwrap();
    assert!(transaction.spend(1).is_err());
    let mut transaction = Transaction::default();
    assert!(transaction.spend(usize::MAX).is_err());
}

#[cfg(unix)]
#[test]
fn shared_hard_links_are_not_silently_split_by_atomic_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let alias = temp.path().join("alias");
    std::fs::write(&source, "shared").unwrap();
    std::fs::hard_link(&source, &alias).unwrap();
    let mut transaction = Transaction::default();
    let error = transaction
        .edit(&source, &[replace_first("Z")])
        .expect_err("shared inode");
    assert!(error.to_string().contains("shared hard links"));
    assert_eq!(std::fs::read(source).unwrap(), b"shared");
    assert_eq!(std::fs::read(alias).unwrap(), b"shared");
}
