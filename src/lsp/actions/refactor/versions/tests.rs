//! Request-time version identity, including the real ordered apply engine.

use super::*;
use crate::lsp::client::try_path_to_uri;
use serde_json::json;

struct Fixture {
    paths: [PathBuf; 3],
    uris: [String; 3],
    requested: HashMap<PathBuf, DocumentSnapshot>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join("pi-refactor-version-identity");
        let paths = [root.join("a.rs"), root.join("b.rs"), root.join("c.rs")];
        let uris = paths
            .each_ref()
            .map(|path| try_path_to_uri(path).expect("file URI"));
        let requested = HashMap::from([
            (
                paths[0].clone(),
                DocumentSnapshot {
                    version: 7,
                    hash: 17,
                },
            ),
            (
                paths[1].clone(),
                DocumentSnapshot {
                    version: 8,
                    hash: 18,
                },
            ),
        ]);
        Self {
            paths,
            uris,
            requested,
        }
    }

    fn edit(&self, index: usize, version: Value) -> Value {
        json!({"textDocument":{"uri":self.uris[index],"version":version},"edits":[{
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
            "newText":"new"
        }]})
    }

    fn rename(&self, old: usize, new: usize) -> Value {
        json!({"kind":"rename","oldUri":self.uris[old],"newUri":self.uris[new]})
    }

    fn check(&self, changes: Vec<Value>) -> Result<()> {
        validate(
            &json!({"documentChanges":changes}),
            &self.requested,
            &self.requested,
        )
    }
}

#[test]
fn a_move_retains_the_original_document_version() {
    let f = Fixture::new();
    f.check(vec![f.rename(0, 2), f.edit(2, json!(7))])
        .expect("moved identity");
}

#[test]
fn chained_moves_and_interleaved_edits_keep_one_identity() {
    let f = Fixture::new();
    f.check(vec![
        f.edit(0, json!(7)),
        f.rename(0, 2),
        f.edit(2, json!(7)),
        f.rename(2, 0),
        f.edit(0, json!(7)),
    ])
    .expect("identity survives a round trip");
}

#[test]
fn moved_away_path_cannot_reuse_the_old_version() {
    let f = Fixture::new();
    assert!(f.check(vec![f.rename(0, 2), f.edit(0, json!(7))]).is_err());
}

#[test]
fn overwriting_a_destination_does_not_inherit_its_version() {
    let f = Fixture::new();
    let mut rename = f.rename(0, 1);
    rename["options"] = json!({"overwrite":true});
    f.check(vec![rename.clone(), f.edit(1, json!(7))])
        .expect("source version");
    assert!(f.check(vec![rename, f.edit(1, json!(8))]).is_err());
}

#[test]
fn deleting_and_recreating_a_path_does_not_resurrect_its_version() {
    let f = Fixture::new();
    let prefix = vec![
        json!({"kind":"delete","uri":f.uris[0]}),
        json!({"kind":"create","uri":f.uris[0]}),
    ];
    let mut versioned = prefix.clone();
    versioned.push(f.edit(0, json!(7)));
    assert!(f.check(versioned).is_err());
    let mut unversioned = prefix;
    unversioned.push(f.edit(0, Value::Null));
    f.check(unversioned)
        .expect("new files permit unversioned edits");
}

#[test]
fn overwriting_create_invalidates_old_identity_even_with_ignore_if_exists() {
    let f = Fixture::new();
    assert!(
        f.check(vec![
            json!({"kind":"create","uri":f.uris[0],
        "options":{"overwrite":true,"ignoreIfExists":true}}),
            f.edit(0, json!(7))
        ])
        .is_err()
    );
}

#[test]
fn ignored_create_preserves_a_known_existing_document() {
    let f = Fixture::new();
    f.check(vec![
        json!({"kind":"create","uri":f.uris[0],
        "options":{"ignoreIfExists":true}}),
        f.edit(0, json!(7)),
    ])
    .expect("ignored create");
}

#[test]
fn ignored_rename_preserves_both_known_document_identities() {
    let f = Fixture::new();
    let mut rename = f.rename(0, 1);
    rename["options"] = json!({"ignoreIfExists":true});
    f.check(vec![rename, f.edit(0, json!(7)), f.edit(1, json!(8))])
        .expect("ignored move");
}

#[test]
fn overwrite_takes_precedence_over_ignore_if_exists() {
    let f = Fixture::new();
    let mut rename = f.rename(0, 1);
    rename["options"] = json!({"overwrite":true,"ignoreIfExists":true});
    f.check(vec![rename.clone(), f.edit(1, json!(7))])
        .expect("overwriting move");
    assert!(f.check(vec![rename, f.edit(1, json!(8))]).is_err());
}

#[test]
fn unknown_conditional_destination_never_grants_version_evidence() {
    let f = Fixture::new();
    let mut rename = f.rename(0, 2);
    rename["options"] = json!({"ignoreIfExists":true});
    for target in [0, 2] {
        assert!(
            f.check(vec![rename.clone(), f.edit(target, json!(7))])
                .is_err()
        );
    }
}

#[test]
fn previously_deleted_destination_makes_conditional_move_unambiguous() {
    let f = Fixture::new();
    let mut rename = f.rename(0, 1);
    rename["options"] = json!({"ignoreIfExists":true});
    f.check(vec![
        json!({"kind":"delete","uri":f.uris[1]}),
        rename,
        f.edit(1, json!(7)),
    ])
    .expect("deleted destination is absent in this transaction");
}

#[test]
fn moving_an_unknown_document_cannot_borrow_a_known_destination_version() {
    let f = Fixture::new();
    let mut rename = f.rename(2, 1);
    rename["options"] = json!({"overwrite":true});
    assert!(f.check(vec![rename, f.edit(1, json!(8))]).is_err());
}

#[test]
fn a_moved_document_still_checks_the_original_live_snapshot() {
    let f = Fixture::new();
    let raw = json!({"documentChanges":[f.rename(0, 2),f.edit(2, json!(7))]});
    for replacement in [
        None,
        Some(DocumentSnapshot {
            version: 9,
            hash: 17,
        }),
        Some(DocumentSnapshot {
            version: 7,
            hash: 99,
        }),
    ] {
        let mut current = f.requested.clone();
        match replacement {
            Some(snapshot) => {
                current.insert(f.paths[0].clone(), snapshot);
            }
            None => {
                current.remove(&f.paths[0]);
            }
        }
        assert!(validate(&raw, &f.requested, &current).is_err());
    }
}

#[test]
fn invalid_or_unknown_versions_still_fail_closed() {
    let f = Fixture::new();
    for version in [
        json!(0),
        json!(-1),
        json!(2147483648_u64),
        json!(7.5),
        json!("7"),
    ] {
        assert!(f.check(vec![f.rename(0, 2), f.edit(2, version)]).is_err());
    }
    assert!(f.check(vec![f.edit(2, json!(7))]).is_err());
}

#[test]
fn identity_mapping_does_not_mutate_request_evidence() {
    let f = Fixture::new();
    f.check(vec![f.rename(0, 2), f.edit(2, json!(7))])
        .expect("rename");
    assert_eq!(f.requested.len(), 2);
    assert_eq!(f.requested[&f.paths[0]].version, 7);
    assert!(!f.requested.contains_key(&f.paths[2]));
    f.check(vec![f.edit(0, json!(7))])
        .expect("next response uses its own identity map");
}

#[test]
fn actual_ordered_transaction_moves_then_applies_a_versioned_edit() {
    let temp = tempfile::tempdir().expect("workspace");
    let root = temp.path().canonicalize().expect("canonical workspace");
    let old = root.join("old.rs");
    let new = root.join("new.rs");
    std::fs::write(&old, "old\n").expect("source");
    let hash = crate::lsp::actions::file_hash(&old).expect("request hash");
    let requested = HashMap::from([(old.clone(), DocumentSnapshot { version: 7, hash })]);
    let raw = json!({"documentChanges":[
        {"kind":"rename","oldUri":try_path_to_uri(&old).unwrap(),"newUri":try_path_to_uri(&new).unwrap()},
        {"textDocument":{"uri":try_path_to_uri(&new).unwrap(),"version":7},"edits":[{
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"new"
        }]}
    ]});
    let plan = crate::lsp::edits::parse_workspace_edit(&raw).expect("ordered plan");
    validate(&raw, &requested, &requested).expect("moved version");
    crate::lsp::edits::apply_workspace_edit(&plan, Some(&HashMap::from([(old.clone(), hash)])))
        .expect("apply real transaction");
    assert!(!old.exists());
    assert_eq!(std::fs::read_to_string(new).unwrap(), "new\n");
}

#[test]
fn recreated_document_version_is_rejected_before_transaction_changes_bytes() {
    let temp = tempfile::tempdir().expect("workspace");
    let source = temp.path().canonicalize().unwrap().join("source.rs");
    std::fs::write(&source, "old\n").unwrap();
    let hash = crate::lsp::actions::file_hash(&source).expect("request hash");
    let requested = HashMap::from([(source.clone(), DocumentSnapshot { version: 7, hash })]);
    let uri = try_path_to_uri(&source).unwrap();
    let raw = json!({"documentChanges":[
        {"kind":"delete","uri":uri}, {"kind":"create","uri":uri},
        {"textDocument":{"uri":uri,"version":7},"edits":[]}
    ]});
    crate::lsp::edits::parse_workspace_edit(&raw).expect("structurally valid edit");
    assert!(validate(&raw, &requested, &requested).is_err());
    assert_eq!(std::fs::read_to_string(source).unwrap(), "old\n");
}

mod protocol;
