use super::*;

#[allow(clippy::needless_pass_by_value)] // call sites pass inline json! values
fn record(id: &str, parent: Option<&str>, content: Value) -> Value {
    json!({
        "type":"message", "id":id, "parentId":parent,
        "timestamp":"2026-10-13T00:00:00Z",
        "message":{"role":"user", "content":content, "timestamp":0}
    })
}

fn source(root: &Path, records: &[Value], leaf: &str) -> PathBuf {
    let path = root.join("source.jsonl");
    let mut header = serde_json::to_value(Session::in_memory().header).unwrap();
    header["id"] = json!("original-session");
    header["cwd"] = json!(root.to_str().unwrap());
    // The native header key; a misspelled one is silently ignored on load.
    header["leafId"] = json!(leaf);
    header["parentSession"] = json!("old-grandparent.jsonl");
    let mut bytes = serde_json::to_vec(&header).unwrap();
    bytes.push(b'\n');
    for record in records {
        serde_json::to_writer(&mut bytes, record).unwrap();
        bytes.push(b'\n');
    }
    std::fs::write(&path, bytes).unwrap();
    path
}

fn rows(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assistant(id: &str, parent: &str, calls: &[&str]) -> Value {
    let content: Vec<_> = calls
        .iter()
        .map(|call| {
            json!({
                "type":"toolCall", "id":call, "name":"read", "arguments":{"path":"photo.png"}
            })
        })
        .collect();
    json!({
        "type":"message", "id":id, "parentId":parent,
        "timestamp":"2026-10-13T00:00:00Z",
        "message":{
            "role":"assistant", "content":content, "provider":"fixture", "model":"fixture",
            "api":"openai-completions", "stopReason":"toolUse", "timestamp":0,
            "usage":crate::model::Usage::default()
        }
    })
}

fn tool_result(id: &str, parent: &str, call: &str) -> Value {
    json!({
        "type":"message", "id":id, "parentId":parent,
        "timestamp":"2026-10-13T00:00:00Z",
        "message":{
            "role":"toolResult", "toolCallId":call, "toolName":"read", "isError":false,
            "content":[{"type":"image", "mimeType":"image/png", "data":"not-inspected"}],
            "timestamp":0
        }
    })
}

#[test]
fn fork_preserves_all_branches_global_metadata_and_forward_parents_with_new_identity() {
    let directory = tempfile::tempdir().unwrap();
    let global = json!({
        "type":"custom", "id":"global", "timestamp":"2026-10-13T00:00:00Z",
        "customType":"extension-state", "data":{"keep":true}
    });
    let records = [
        record("chosen", Some("root"), json!("chosen 日本語")),
        record("other", Some("root"), json!("other branch")),
        record(
            "root",
            None,
            json!([
                {"type":"text", "text":"original text"},
                {"type":"media", "mimeType":"audio/wav", "data":{"missing":true}}
            ]),
        ),
        global.clone(),
    ];
    let source = source(directory.path(), &records, "chosen");
    let original = std::fs::read(&source).unwrap();
    let report = Session::recover_text(&source).unwrap();
    let output = directory.path().join("recovered.jsonl");
    let id = Session::fork_recovered_text(&source, &output, None).unwrap();
    let copied = rows(&output);
    assert_ne!(id, "original-session");
    assert_eq!(copied[0]["id"], id);
    assert!(copied[0].get("parentSession").is_none());
    assert_eq!(copied[0]["cwd"], directory.path().to_str().unwrap());
    assert_eq!(copied[1]["id"], "root");
    assert_eq!(copied[2], records[0]);
    assert_eq!(copied[3], records[1]);
    assert_eq!(copied[4], global);
    assert_eq!(copied[1]["message"]["content"][0]["text"], "original text");
    assert_eq!(copied[1]["message"]["content"][1]["type"], "text");
    let notice = copied.last().unwrap();
    assert_eq!(
        SessionHeader::deserialize(&copied[0]).unwrap().current_leaf,
        notice["id"].as_str().map(str::to_string)
    );
    assert_eq!(notice["parentId"], "chosen");
    assert_eq!(notice["message"]["customType"], PROVENANCE);
    assert_eq!(
        notice["message"]["details"]["sourceSha256"],
        report.source_sha256()
    );
    assert_eq!(
        notice["message"]["details"]["sourceSessionId"],
        "original-session"
    );
    assert_eq!(notice["message"]["details"]["omittedAttachments"], 1);
    assert!(
        notice["message"]["content"]
            .as_str()
            .unwrap()
            .contains("not newly verified")
    );
    let native: Vec<_> = copied[1..]
        .iter()
        .map(|row| SessionEntry::deserialize(row).unwrap())
        .collect();
    crate::session::validate_session_entry_graph(&native).unwrap();
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert!(!crate::session::attachments::sidecar_path(&source).exists());
    assert!(!crate::session::attachments::sidecar_path(&output).exists());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn explicit_branch_and_root_select_without_discarding_any_history() {
    let directory = tempfile::tempdir().unwrap();
    let source = source(
        directory.path(),
        &[
            record("one", None, json!("one")),
            record("two", Some("one"), json!("two")),
        ],
        "two",
    );
    for (name, selection, expected) in [
        ("saved.jsonl", None, json!("two")),
        ("earlier.jsonl", Some("one"), json!("one")),
        ("root.jsonl", Some(""), Value::Null),
    ] {
        let output = directory.path().join(name);
        Session::fork_recovered_text(&source, &output, selection).unwrap();
        let copied = rows(&output);
        assert_eq!(copied.len(), 4);
        assert_eq!(copied.last().unwrap()["parentId"], expected);
        assert_eq!(copied[1]["id"], "one");
        assert_eq!(copied[2]["id"], "two");
    }
}

#[test]
fn complete_parallel_tools_keep_ids_arguments_results_and_visible_media_omissions() {
    let directory = tempfile::tempdir().unwrap();
    let records = [
        record("start", None, json!("inspect photos")),
        assistant("calls", "start", &["call-a", "call-b"]),
        tool_result("second", "calls", "call-b"),
        tool_result("first", "second", "call-a"),
    ];
    let source = source(directory.path(), &records, "first");
    let output = directory.path().join("copy.jsonl");
    Session::fork_recovered_text(&source, &output, None).unwrap();
    let copied = rows(&output);
    assert_eq!(
        copied[2], records[1],
        "historical tool call is not rewritten or re-executed"
    );
    assert_eq!(copied[3]["message"]["toolCallId"], "call-b");
    assert_eq!(copied[4]["message"]["toolCallId"], "call-a");
    for result in &copied[3..5] {
        assert_eq!(result["message"]["content"][0]["type"], "text");
        assert!(
            result["message"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("not inspected")
        );
    }
}

#[test]
fn unfinished_duplicate_and_unmatched_tool_batches_refuse_without_fabrication() {
    let cases = [
        vec![assistant("calls", "start", &["a"])],
        vec![assistant("calls", "start", &["a", "a"])],
        vec![
            assistant("calls", "start", &["a"]),
            tool_result("wrong", "calls", "b"),
        ],
        vec![tool_result("orphan", "start", "a")],
        vec![
            assistant("calls", "start", &["a"]),
            record("later", Some("calls"), json!("next")),
        ],
    ];
    for tail in cases {
        let directory = tempfile::tempdir().unwrap();
        let mut records = vec![record("start", None, json!("request"))];
        records.extend(tail);
        let leaf = records.last().unwrap()["id"].as_str().unwrap();
        let source = source(directory.path(), &records, leaf);
        let original = std::fs::read(&source).unwrap();
        let output = directory.path().join("copy.jsonl");
        assert!(Session::fork_recovered_text(&source, &output, None).is_err());
        assert!(!output.exists());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        // Choosing a complete ancestor is explicit; the incomplete branch is
        // still retained as history, never repaired with a fictional result.
        Session::fork_recovered_text(&source, &output, Some("start")).unwrap();
        assert_eq!(rows(&output).last().unwrap()["parentId"], "start");
    }
}

#[test]
fn missing_selection_bad_extension_and_malformed_source_publish_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let source = source(directory.path(), &[record("id", None, json!("text"))], "id");
    for (name, selected) in [("copy.jsonl", Some("unknown")), ("wrong.sqlite", None)] {
        let output = directory.path().join(name);
        assert!(Session::fork_recovered_text(&source, &output, selected).is_err());
        assert!(!output.exists());
    }
    let malformed = directory.path().join("malformed.jsonl");
    std::fs::write(&malformed, b"{not json}\n").unwrap();
    let output = directory.path().join("malformed-copy.jsonl");
    assert!(Session::fork_recovered_text(&malformed, &output, None).is_err());
    assert!(!output.exists());
    assert_eq!(std::fs::read(&malformed).unwrap(), b"{not json}\n");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn source_existing_copy_and_hardlink_are_never_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let source = source(directory.path(), &[record("id", None, json!("keep"))], "id");
    let original = std::fs::read(&source).unwrap();
    assert!(Session::fork_recovered_text(&source, &source, None).is_err());
    let alias = directory.path().join("alias.jsonl");
    std::fs::hard_link(&source, &alias).unwrap();
    assert!(Session::fork_recovered_text(&source, &alias, None).is_err());
    let output = directory.path().join("copy.jsonl");
    Session::fork_recovered_text(&source, &output, None).unwrap();
    let copied = std::fs::read(&output).unwrap();
    assert!(Session::fork_recovered_text(&source, &output, Some("")).is_err());
    assert_eq!(std::fs::read(&output).unwrap(), copied);
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert_eq!(std::fs::read(alias).unwrap(), original);
}

#[test]
fn existing_and_late_sidecars_refuse_without_touching_their_contents() {
    for kind in ["blobs", "v2", "lock"] {
        let directory = tempfile::tempdir().unwrap();
        let source = source(directory.path(), &[record("id", None, json!("keep"))], "id");
        let output = directory.path().join("copy.jsonl");
        let sidecar = match kind {
            "blobs" => crate::session::attachments::sidecar_path(&output),
            "v2" => crate::session_store_v2::v2_sidecar_path(&output),
            _ => output.with_extension("jsonl.lock"),
        };
        std::fs::write(&sidecar, b"retain old artifact").unwrap();
        assert!(Session::fork_recovered_text(&source, &output, None).is_err());
        assert!(!output.exists());
        assert_eq!(std::fs::read(sidecar).unwrap(), b"retain old artifact");
    }
    let directory = tempfile::tempdir().unwrap();
    let source = source(directory.path(), &[record("id", None, json!("keep"))], "id");
    let report = Session::recover_text(source).unwrap();
    let owner = AgentCx::for_request();
    let prepared = PreparedCopy::new(&report, None, &owner).unwrap();
    let output = directory.path().join("late.jsonl");
    let sidecar = crate::session::attachments::sidecar_path(&output);
    let result = publish_new_file(&output, &owner, require_unused_sidecars, |file| {
        prepared.write(&report, file, &owner)?;
        std::fs::write(&sidecar, b"late artifact")?;
        Ok(())
    });
    assert!(result.is_err());
    assert!(!output.exists());
    assert_eq!(std::fs::read(sidecar).unwrap(), b"late artifact");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn cancellation_after_preparation_does_not_publish_the_copy() {
    let directory = tempfile::tempdir().unwrap();
    let source = source(directory.path(), &[record("id", None, json!("keep"))], "id");
    let report = Session::recover_text(source).unwrap();
    let owner = AgentCx::for_request();
    let prepared = PreparedCopy::new(&report, None, &owner).unwrap();
    owner.cancel_with(
        asupersync::types::CancelKind::User,
        Some("abandon recovery fork"),
    );
    let output = directory.path().join("copy.jsonl");
    assert!(prepared.publish(&report, &output, &owner).is_err());
    assert!(!output.exists());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn copy_is_private_and_dangling_destination_or_blob_links_are_preserved() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let directory = tempfile::tempdir().unwrap();
    let source = source(directory.path(), &[record("id", None, json!("keep"))], "id");
    let output = directory.path().join("copy.jsonl");
    Session::fork_recovered_text(&source, &output, None).unwrap();
    assert_eq!(
        std::fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let absent = directory.path().join("absent");
    let link = directory.path().join("linked.jsonl");
    symlink(&absent, &link).unwrap();
    assert!(Session::fork_recovered_text(&source, &link, None).is_err());
    let output = directory.path().join("stale.jsonl");
    let sidecar = crate::session::attachments::sidecar_path(&output);
    symlink(&absent, &sidecar).unwrap();
    assert!(Session::fork_recovered_text(&source, &output, None).is_err());
    assert!(!output.exists());
    assert!(!absent.exists());
    assert!(
        std::fs::symlink_metadata(link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        std::fs::symlink_metadata(sidecar)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}
