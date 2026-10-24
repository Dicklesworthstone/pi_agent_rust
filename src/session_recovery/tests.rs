use super::*;

pub(super) fn header(leaf: Option<&str>) -> Value {
    let mut header = Session::in_memory().header;
    header.current_leaf = leaf.map(str::to_string);
    serde_json::to_value(header).unwrap()
}

#[allow(clippy::needless_pass_by_value)] // call sites pass inline json! values
pub(super) fn user(id: &str, parent: Option<&str>, content: Value) -> Value {
    json!({
        "type":"message", "id":id, "parentId":parent,
        "timestamp":"2026-10-13T00:00:00Z",
        "message":{"role":"user", "content":content, "timestamp":0}
    })
}

pub(super) fn transcript(path: &Path, header: &Value, rows: &[Value]) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(header).unwrap();
    bytes.push(b'\n');
    for row in rows {
        serde_json::to_writer(&mut bytes, row).unwrap();
        bytes.push(b'\n');
    }
    std::fs::write(path, &bytes).unwrap();
    bytes
}

fn text(value: &str) -> Value {
    json!({"type":"text", "text":value})
}

#[test]
fn missing_media_sidecar_does_not_hide_text_or_create_any_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("saved.jsonl");
    let payload = json!([
        text("before 界"),
        {"type":"image", "mimeType":"image/png", "data":{
            "$piBlob":format!("sha256:{}", "a".repeat(64)),
            "sizeBytes":70_000, "encoding":"base64"
        }},
        text("after"),
        {"type":"media", "mimeType":"video/mp4", "name":"clip.mp4", "data":"not-base64"}
    ]);
    let original = transcript(&path, &header(None), &[user("one", None, payload)]);
    let report = Session::recover_text(&path).unwrap();
    assert_eq!(report.entries().len(), 1);
    assert_eq!(report.entries()[0].line(), 2);
    let blocks = &report.entries()[0].record()["message"]["content"];
    assert_eq!(blocks[0], text("before 界"));
    assert_eq!(blocks[2], text("after"));
    assert!(
        blocks[1]["text"]
            .as_str()
            .unwrap()
            .contains("attachment not inspected")
    );
    assert_eq!(report.omissions().len(), 2);
    assert_eq!(report.omissions()[0].block_index, 1);
    assert_eq!(report.omissions()[0].storage, "reference_not_inspected");
    assert_eq!(report.omissions()[1].storage, "inline");
    assert_eq!(report.omissions()[1].name.as_deref(), Some("clip.mp4"));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert_eq!(
        report.source_sha256(),
        crate::package_manager::hex_encode(&Sha256::digest(&original))
    );
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["schema"], SCHEMA);
    assert_eq!(json["sourceBytes"], original.len());
    assert!(json.get("activePath").is_none());
}

#[test]
fn corrupt_sidecars_and_malformed_media_references_are_never_inspected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("saved.jsonl");
    // A file in place of the expected directory must not even be traversed.
    let sidecar = dir.path().join("saved.jsonl.blobs");
    std::fs::write(&sidecar, b"retained corrupt sidecar").unwrap();
    let row = user(
        "one",
        None,
        json!([
            text("retained"),
            {"type":"image", "data":{"$piBlob":"../../do-not-open", "sizeBytes":-1}},
            {"type":"media", "data":null}
        ]),
    );
    transcript(&path, &header(None), &[row]);
    let report = recover_session_text(&path).unwrap();
    assert_eq!(report.omissions().len(), 2);
    assert_eq!(report.omissions()[1].storage, "unrecognized_not_inspected");
    assert_eq!(
        std::fs::read(&sidecar).unwrap(),
        b"retained corrupt sidecar"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn branching_and_forward_parent_references_preserve_saved_order_and_selected_leaf() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("branch.jsonl");
    let rows = [
        user("chosen", Some("root"), json!("chosen text")),
        user("root", None, json!("root text")),
        user("sibling", Some("root"), json!("other branch")),
    ];
    transcript(&path, &header(Some("chosen")), &rows);
    let report = recover_session_text(&path).unwrap();
    assert_eq!(
        report
            .entries()
            .iter()
            .map(RecoveredEntry::id)
            .collect::<Vec<_>>(),
        ["chosen", "root", "sibling"]
    );
    assert_eq!(
        report
            .active_path()
            .map(RecoveredEntry::id)
            .collect::<Vec<_>>(),
        ["root", "chosen"]
    );
    assert_eq!(report.selected_leaf(), Some("chosen"));
    assert_eq!(report.entries()[0].parent_id(), Some("root"));
    assert_eq!(report.entries()[2].record(), &rows[2]);
}

#[test]
fn explicit_root_empty_session_and_natural_last_entry_are_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leaf.jsonl");
    let rows = [user("root", None, json!("saved"))];
    transcript(&path, &header(Some("")), &rows);
    let report = recover_session_text(&path).unwrap();
    assert!(report.selected_leaf().is_none());
    assert_eq!(report.active_path().count(), 0);
    assert_eq!(report.entries().len(), 1);
    transcript(&path, &header(None), &rows);
    assert_eq!(
        recover_session_text(&path).unwrap().selected_leaf(),
        Some("root")
    );
    transcript(&path, &header(None), &[]);
    let report = recover_session_text(&path).unwrap();
    assert!(report.entries().is_empty());
    assert!(report.selected_leaf().is_none());
}

#[test]
fn corrupt_graphs_and_unknown_selected_leaves_fail_instead_of_returning_a_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bad.jsonl");
    for rows in [
        vec![user("a", None, json!("one")), user("a", None, json!("two"))],
        vec![user("a", Some("missing"), json!("orphan"))],
        vec![user("a", Some("a"), json!("self cycle"))],
        vec![
            user("good", None, json!("selected")),
            user("a", Some("b"), json!("a")),
            user("b", Some("a"), json!("b")),
        ],
    ] {
        let original = transcript(&path, &header(None), &rows);
        assert!(recover_session_text(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    transcript(
        &path,
        &header(Some("missing")),
        &[user("good", None, json!("saved"))],
    );
    assert!(
        recover_session_text(&path)
            .unwrap_err()
            .to_string()
            .contains("selected leaf")
    );
}

#[test]
fn duplicate_keys_are_rejected_even_in_nested_payloads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("duplicates.jsonl");
    let head = header(None).to_string();
    for row in [
        r#"{"type":"message","id":"one","id":"two"}"#,
        r#"{"type":"message","id":"one","parentId":null,"message":{"role":"user","content":"first","content":"second","timestamp":0},"timestamp":"2026-10-13T00:00:00Z"}"#,
        r#"{"type":"custom","id":"one","customType":"test","data":{"key":1,"\u006bey":2},"timestamp":"2026-10-13T00:00:00Z"}"#,
    ] {
        let original = format!("{head}\n{row}\n");
        std::fs::write(&path, &original).unwrap();
        assert!(recover_session_text(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }
    std::fs::write(&path, r#"{"type":"session","version":3,"version":3}"#).unwrap();
    assert!(recover_session_text(&path).is_err());
}

#[test]
fn media_shaped_objects_outside_native_content_are_not_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("custom.jsonl");
    let row = json!({
        "type":"custom", "id":"custom", "parentId":null,
        "timestamp":"2026-10-13T00:00:00Z", "customType":"historical-example",
        "data":{"type":"image", "data":"this is example source, not an attachment"}
    });
    transcript(&path, &header(None), std::slice::from_ref(&row));
    let report = recover_session_text(&path).unwrap();
    assert_eq!(report.entries()[0].record(), &row);
    assert!(report.omissions().is_empty());
}

#[test]
fn invalid_native_records_and_torn_rows_never_become_successful_partial_reports() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("torn.jsonl");
    let good = transcript(
        &path,
        &header(None),
        &[user("one", None, json!("valid prefix"))],
    );
    for tail in [
        b"{\"type\":".as_slice(),
        b"\xff",
        b"{\"id\":\"two\",\"type\":\"unknown_future_record\"}\n",
    ] {
        let mut original = good.clone();
        original.extend_from_slice(tail);
        std::fs::write(&path, &original).unwrap();
        assert!(recover_session_text(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    let mut complete_without_newline = good;
    assert_eq!(complete_without_newline.pop(), Some(b'\n'));
    std::fs::write(&path, &complete_without_newline).unwrap();
    assert!(recover_session_text(&path).unwrap().unterminated_final_row);
    assert_eq!(std::fs::read(&path).unwrap(), complete_without_newline);
}

#[test]
fn source_row_retained_entry_and_node_limits_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("limits.jsonl");
    transcript(
        &path,
        &header(None),
        &[user("a", None, json!("x".repeat(1024)))],
    );
    for limits in [
        Limits {
            source: 1,
            ..LIMITS
        },
        Limits { row: 16, ..LIMITS },
        Limits {
            retained: 16,
            ..LIMITS
        },
        Limits {
            entries: 0,
            ..LIMITS
        },
        Limits { nodes: 2, ..LIMITS },
    ] {
        assert!(recover(&path, limits, &AgentCx::for_request(), || {}).is_err());
    }
    assert!(recover_session_text(&path).is_ok());
}

#[test]
fn same_length_rewrite_and_same_name_replacement_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("changing.jsonl");
    let original = transcript(&path, &header(None), &[user("a", None, json!("before"))]);
    let changed = String::from_utf8(original.clone())
        .unwrap()
        .replace("before", "after!");
    assert_eq!(changed.len(), original.len());
    let result = recover(&path, LIMITS, &AgentCx::for_request(), || {
        std::fs::write(&path, &changed).unwrap();
    });
    assert!(result.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
    std::fs::write(&path, &original).unwrap();
    let retained = dir.path().join("original.retained");
    let result = recover(&path, LIMITS, &AgentCx::for_request(), || {
        std::fs::rename(&path, &retained).unwrap();
        std::fs::write(&path, &original).unwrap();
    });
    assert!(result.is_err());
    assert_eq!(std::fs::read(&retained).unwrap(), original);
}

#[test]
fn cancelled_and_restricted_owners_cannot_start_reading() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent.jsonl");
    let owner = AgentCx::for_request();
    owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel recovery"));
    assert!(matches!(
        recover(&absent, LIMITS, &owner, || {}),
        Err(Error::Aborted)
    ));
    let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
    let _guard = restricted.set_current_restricted();
    let error = recover_session_text(&absent).unwrap_err();
    assert!(matches!(error, Error::Io(error) if error.kind() == io::ErrorKind::PermissionDenied));
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn cancellation_before_verification_never_returns_the_recovered_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cancelled.jsonl");
    let original = transcript(&path, &header(None), &[user("a", None, json!("retained"))]);
    let owner = AgentCx::for_request();
    let result = recover(&path, LIMITS, &owner, || {
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel second pass"),
        );
    });
    assert!(matches!(result, Err(Error::Aborted)));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn readonly_sources_work_without_chmod_and_final_symlinks_are_refused() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("readonly.jsonl");
    let original = transcript(&path, &header(None), &[user("a", None, json!("saved"))]);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(recover_session_text(&path).is_ok());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o400
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let alias = dir.path().join("alias.jsonl");
    symlink(&path, &alias).unwrap();
    assert!(recover_session_text(&alias).is_err());
    assert!(recover_session_text(dir.path()).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn unsupported_session_versions_and_non_jsonl_sources_fail_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unsupported");
    for version in [json!(1), json!(2), json!(4), Value::Null, json!("3")] {
        let mut head = header(None);
        head["version"] = version;
        transcript(&path, &head, &[]);
        assert!(recover_session_text(&path).is_err());
    }
    std::fs::write(&path, b"SQLite format 3\0").unwrap();
    assert!(recover_session_text(&path).is_err());
}

#[test]
fn strict_json_limits_depth_and_preserves_ordinary_json_values() {
    let value = json!({"unicode":"界", "number":42, "fraction":1.25, "bool":false, "null":null, "array":[1,2]});
    assert_eq!(
        strict_json::parse(&serde_json::to_vec(&value).unwrap(), &mut 100).unwrap(),
        value
    );
    let nested = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    assert!(strict_json::parse(nested.as_bytes(), &mut 1000).is_err());
    assert!(strict_json::parse(b"{} {}", &mut 1000).is_err());
}
