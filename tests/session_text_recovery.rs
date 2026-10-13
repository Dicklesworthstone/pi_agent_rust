//! Exercise recovery as an SDK consumer without internal helpers or credentials.

#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use pi::sdk::Session;
use pi::session_recovery::{RecoveryExport, RecoverySelection};
use serde_json::{Value, json};

#[test]
fn sdk_recovers_and_exports_a_selected_branch_without_media_or_session_mutation() {
    let directory = tempfile::tempdir().expect("temporary workspace");
    let source = directory.path().join("conversation.jsonl");
    let mut header = Session::in_memory().header;
    header.current_leaf = Some("answer".to_string());
    let mut rows = vec![serde_json::to_value(header).unwrap()];
    for (id, parent, content) in [
        ("root", None, json!([
            {"type":"text", "text":"Keep this reasoning even without the image."},
            {"type":"image", "mimeType":"image/png", "data":{
                "$piBlob":format!("sha256:{}", "a".repeat(64)),
                "sizeBytes":100_000, "encoding":"base64"
            }}
        ])),
        ("answer", Some("root"), json!("Selected path with Unicode: 日本語")),
        ("alternate", Some("root"), json!("Independent alternate branch")),
    ] {
        rows.push(json!({
            "type":"message", "id":id, "parentId":parent,
            "timestamp":"2026-10-13T00:00:00Z",
            "message":{"role":"user", "content":content, "timestamp":0}
        }));
    }
    let mut original = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut original, &row).unwrap();
        original.push(b'\n');
    }
    std::fs::write(&source, &original).unwrap();

    let report = Session::recover_text(&source).expect("recover without absent media");
    assert_eq!(report.entries().len(), 3);
    assert_eq!(report.omissions().len(), 1);
    assert_eq!(report.selected_leaf(), Some("answer"));
    assert_eq!(report.active_path().map(|entry| entry.id()).collect::<Vec<_>>(), ["root", "answer"]);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);

    let mut selected = Vec::new();
    report.write_selected_text(RecoverySelection::Active, &mut selected).unwrap();
    let selected = String::from_utf8(selected).unwrap();
    assert!(selected.contains("Keep this reasoning"));
    assert!(selected.contains("日本語"));
    assert!(!selected.contains("Independent alternate branch"));
    assert!(selected.contains("attachment not inspected"));

    let json_path = directory.path().join("recovered.json");
    report.export_file(&json_path, RecoveryExport::Json).unwrap();
    let decoded: Value = serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();
    assert_eq!(decoded["schema"], "pi.session_text_recovery.v1");
    assert_eq!(decoded["entries"][2]["record"]["message"]["content"], "Independent alternate branch");
    assert!(report.export_file(&source, RecoveryExport::Json).is_err());
    assert!(report.export_file(&json_path, RecoveryExport::Json).is_err());
    assert_eq!(std::fs::read(&source).unwrap(), original);
    assert!(!source.with_extension("jsonl.blobs").exists());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
}

fn persisted_source(root: &std::path::Path) -> (std::path::PathBuf, Vec<u8>) {
    let source = root.join("original.jsonl");
    let mut header = Session::in_memory().header;
    header.id = "original-session".to_string();
    header.cwd = root.to_str().unwrap().to_string();
    header.current_leaf = Some("answered".to_string());
    let records = [
        serde_json::to_value(header).unwrap(),
        json!({"type":"message", "id":"request", "timestamp":"2026-10-13T00:00:00Z",
            "message":{"role":"user", "content":"Inspect the recording", "timestamp":0}}),
        json!({"type":"message", "id":"calls", "parentId":"request", "timestamp":"2026-10-13T00:00:00Z",
            "message":{"role":"assistant", "content":[
                {"type":"toolCall", "id":"read-call", "name":"read", "arguments":{"path":"photo.png"}}
            ], "api":"openai-completions", "provider":"fixture", "model":"fixture",
                "usage":pi::sdk::Usage::default(), "stopReason":"toolUse", "timestamp":0}}),
        json!({"type":"message", "id":"result", "parentId":"calls", "timestamp":"2026-10-13T00:00:00Z",
            "message":{"role":"toolResult", "toolCallId":"read-call", "toolName":"read",
                "content":[
                    {"type":"text", "text":"Recovered tool output"},
                    {"type":"image", "mimeType":"image/png", "data":{
                        "$piBlob":format!("sha256:{}", "a".repeat(64)),
                        "sizeBytes":100_000, "encoding":"base64"
                    }},
                    {"type":"media", "mimeType":"video/mp4", "data":{
                        "$piBlob":format!("sha256:{}", "b".repeat(64)),
                        "sizeBytes":100_000, "encoding":"base64"
                    }}
                ], "isError":false, "timestamp":0}}),
        json!({"type":"message", "id":"answered", "parentId":"result", "timestamp":"2026-10-13T00:00:00Z",
            "message":{"role":"assistant", "content":[{"type":"text", "text":"Historical answer 日本語"}],
                "api":"openai-completions", "provider":"fixture", "model":"fixture",
                "usage":pi::sdk::Usage::default(), "stopReason":"stop", "timestamp":0}}),
        json!({"type":"message", "id":"alternate", "parentId":"request", "timestamp":"2026-10-13T00:00:00Z",
            "message":{"role":"user", "content":"Other branch preserved", "timestamp":0}}),
    ];
    let mut original = Vec::new();
    for record in records {
        serde_json::to_writer(&mut original, &record).unwrap();
        original.push(b'\n');
    }
    std::fs::write(&source, &original).unwrap();
    (source, original)
}

fn path_ids(session: &Session) -> Vec<String> {
    session.entries_for_current_path().iter().map(|entry| {
        serde_json::to_value(entry).unwrap()["id"].as_str().unwrap().to_string()
    }).collect()
}

#[test]
fn recovered_copy_opens_appends_saves_and_reopens_through_the_native_session_api() {
    let directory = tempfile::tempdir().unwrap();
    let (source, original) = persisted_source(directory.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        assert!(Session::open(source.to_str().unwrap()).await.is_err(),
            "normal loading must still refuse absent media");
        let output = directory.path().join("copy.jsonl");
        let id = Session::fork_recovered_text(&source, &output, None).unwrap();
        let (mut session, diagnostics) = Session::open_with_diagnostics(output.to_str().unwrap()).await.unwrap();
        assert!(diagnostics.skipped_entries.is_empty());
        assert!(diagnostics.orphaned_parent_links.is_empty());
        assert_eq!(session.header.id, id);
        assert_ne!(id, "original-session");
        let notice = session.leaf_id().unwrap().to_string();
        assert_eq!(path_ids(&session), ["request", "calls", "result", "answered", &notice]);
        assert_eq!(session.entries.len(), 6, "all five source records plus provenance");
        assert!(serde_json::to_string(&session.entries).unwrap().contains("Other branch preserved"));
        assert!(!serde_json::to_string(&session.entries).unwrap().contains("$piBlob"));
        let last = serde_json::to_value(session.entries.last().unwrap()).unwrap();
        assert_eq!(last["message"]["details"]["omittedAttachments"], 2);

        // Keep any index publication in this test's temporary namespace.
        session.session_dir = Some(directory.path().to_path_buf());
        session.append_message(pi::session::SessionMessage::User {
            content: pi::sdk::UserContent::Text("Continue using the preserved text".to_string()),
            timestamp: Some(1),
        });
        let continued = session.leaf_id().unwrap().to_string();
        session.save().await.unwrap();
        assert_eq!(session.path.as_ref().unwrap().canonicalize().unwrap(), output.canonicalize().unwrap());
        let (reopened, diagnostics) = Session::open_with_diagnostics(output.to_str().unwrap()).await.unwrap();
        assert!(diagnostics.skipped_entries.is_empty());
        assert!(diagnostics.orphaned_parent_links.is_empty());
        assert_eq!(reopened.header.id, id);
        assert_eq!(reopened.leaf_id(), Some(continued.as_str()));
        assert_eq!(path_ids(&reopened), ["request", "calls", "result", "answered", &notice, &continued]);
        assert_eq!(reopened.entries.len(), 7);
        let appended = serde_json::to_value(reopened.entries.last().unwrap()).unwrap();
        assert_eq!(appended["parentId"], notice);
        assert_eq!(appended["message"]["content"], "Continue using the preserved text");
        assert_eq!(std::fs::read(&source).unwrap(), original);
        assert!(!source.with_extension("jsonl.blobs").exists());
        assert!(!output.with_extension("jsonl.blobs").exists());
        assert!(Session::open(source.to_str().unwrap()).await.is_err());
    });
}

#[test]
fn explicit_root_and_alternate_branch_resume_with_their_own_selected_paths() {
    let directory = tempfile::tempdir().unwrap();
    let (source, original) = persisted_source(directory.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    for (name, selection, expected) in [
        ("root.jsonl", "", Vec::<String>::new()),
        ("alternate.jsonl", "alternate", vec!["request".to_string(), "alternate".to_string()]),
    ] {
        let output = directory.path().join(name);
        let id = Session::fork_recovered_text(&source, &output, Some(selection)).unwrap();
        runtime.block_on(async {
            let mut session = Session::open(output.to_str().unwrap()).await.unwrap();
            assert_eq!(session.header.id, id);
            assert_eq!(session.entries.len(), 6, "selection does not discard other branches");
            let mut expected_path = expected;
            let notice = session.leaf_id().unwrap().to_string();
            expected_path.push(notice.clone());
            assert_eq!(path_ids(&session), expected_path);
            session.session_dir = Some(directory.path().to_path_buf());
            session.append_message(pi::session::SessionMessage::User {
                content: pi::sdk::UserContent::Text("New path".to_string()),
                timestamp: Some(2),
            });
            expected_path.push(session.leaf_id().unwrap().to_string());
            session.save().await.unwrap();
            let session = Session::open(output.to_str().unwrap()).await.unwrap();
            assert_eq!(path_ids(&session), expected_path);
            assert_eq!(session.entries.len(), 7);
            let last = serde_json::to_value(session.entries.last().unwrap()).unwrap();
            assert_eq!(last["parentId"], notice);
        });
    }
    assert_eq!(std::fs::read(source).unwrap(), original);
}

#[test]
fn recovered_copy_never_repairs_or_reuses_corrupt_original_attachments() {
    let directory = tempfile::tempdir().unwrap();
    let (source, original) = persisted_source(directory.path());
    let sidecar = source.with_extension("jsonl.blobs");
    std::fs::create_dir(&sidecar).unwrap();
    let corrupt = sidecar.join("a".repeat(64));
    std::fs::write(&corrupt, b"corrupt attachment bytes").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&corrupt, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let output = directory.path().join("copy.jsonl");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
    runtime.block_on(async {
        assert!(Session::open(source.to_str().unwrap()).await.is_err());
        let id = Session::fork_recovered_text(&source, &output, None).unwrap();
        let session = Session::open(output.to_str().unwrap()).await.unwrap();
        assert_eq!(session.header.id, id);
        assert_eq!(session.entries.len(), 6);
        assert!(Session::open(source.to_str().unwrap()).await.is_err());
    });
    assert_eq!(std::fs::read(source).unwrap(), original);
    assert_eq!(std::fs::read(corrupt).unwrap(), b"corrupt attachment bytes");
    assert_eq!(std::fs::read_dir(sidecar).unwrap().count(), 1);
    assert!(!output.with_extension("jsonl.blobs").exists());
}
