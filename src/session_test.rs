use crate::model::SessionMessage;
use crate::session::Session;
use asupersync::sync::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

#[test]
fn test_session_save_persistence() {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");

    runtime.block_on(async move {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let session_path = temp_dir.path().join("test_session.jsonl");

        // Create a new session
        let mut session = Session::create_with_dir(Some(temp_dir.path().to_path_buf()));
        session.path = Some(session_path.clone());

        // Add some messages
        session.append_message(SessionMessage::User {
            content: crate::model::UserContent::Text("Hello".to_string()),
            timestamp: 0,
        });

        // Save the session
        session.save().await.expect("save session");

        // Check if file exists
        assert!(session_path.exists());

        // Re-open the session
        let (loaded_session, diagnostics) =
            Session::open_with_diagnostics(session_path.to_str().unwrap())
                .await
                .expect("load session");

        assert!(diagnostics.skipped_entries.is_empty());
        assert_eq!(loaded_session.entries.len(), 1);

        if let crate::session::SessionEntry::Message(msg) = &loaded_session.entries[0] {
            if let SessionMessage::User { content, .. } = &msg.message {
                if let crate::model::UserContent::Text(text) = content {
                    assert_eq!(text, "Hello");
                } else {
                    unreachable!("Unexpected content type");
                }
            } else {
                unreachable!("Unexpected message type");
            }
        } else {
            unreachable!("Unexpected entry type");
        }
    });
}
