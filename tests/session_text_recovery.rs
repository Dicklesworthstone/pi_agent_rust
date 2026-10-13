//! Exercise recovery as an SDK consumer without internal helpers or credentials.

#![forbid(unsafe_code)]

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
