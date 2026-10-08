//! Persistence regressions for explicit workdir attachment (bd-yutps).
#![recursion_limit = "256"]

use std::path::Path;

use pi::session::Session;
use pi::session_workdir::{
    WORKDIR_BINDING_TYPE, attach_session_workdir, check_session_workdir, inspect_session_workdir,
    require_session_workdir,
};

fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    let reactor = asupersync::runtime::reactor::create_reactor().expect("reactor");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .expect("runtime");
    runtime.block_on(Box::pin(future))
}

async fn round_trip(root: &Path, extension: &str) {
    let original = root.join("original");
    let replacement = root.join("replacement");
    std::fs::create_dir(&original).expect("original directory");
    let path = root.join(format!("session.{extension}"));
    let mut session = Session::in_memory();
    session.header.cwd = original.to_str().expect("UTF-8 original").to_owned();
    session.session_dir = Some(root.to_path_buf());
    session.path = Some(path.clone());
    session.save().await.expect("initial save");
    let original_id = session.header.id.clone();
    let original_parent = session.header.parent_session.clone();
    std::fs::rename(&original, &replacement).expect("move workspace");

    assert!(require_session_workdir(&session).is_err());
    assert!(attach_session_workdir(&mut session, &replacement).expect("explicit attach"));
    session.save().await.expect("save attachment");
    drop(session);

    let mut reopened = Session::open(path.to_str().expect("UTF-8 path"))
        .await
        .expect("reopen");
    check_session_workdir(&reopened, &replacement).expect("binding survives persistence");
    assert_eq!(reopened.header.id, original_id);
    assert_eq!(reopened.header.parent_session, original_parent);
    assert_eq!(
        reopened.header.cwd,
        original.to_str().expect("UTF-8 original")
    );
    assert_eq!(reopened.path.as_deref(), Some(path.as_path()));
    let state = inspect_session_workdir(&reopened).expect("inspect");
    assert_eq!(state.original_cwd, original);
    assert_eq!(
        state.bound_cwd,
        replacement.canonicalize().expect("canonical replacement")
    );
    assert!(!attach_session_workdir(&mut reopened, &replacement).expect("idempotent attach"));
    assert_eq!(reopened.entries.iter().filter(|entry| matches!(
        entry,
        pi::session::SessionEntry::Custom(custom) if custom.custom_type == WORKDIR_BINDING_TYPE
    )).count(), 1);
}

#[test]
fn jsonl_attachment_survives_save_and_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(round_trip(root.path(), "jsonl"));
}

#[cfg(feature = "sqlite-sessions")]
#[test]
fn sqlite_attachment_survives_save_and_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(round_trip(root.path(), "sqlite"));
}

#[test]
fn rejected_target_leaves_persisted_jsonl_unchanged() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let path = root.path().join("unchanged.jsonl");
        let mut session = Session::in_memory();
        session.header.cwd = root.path().to_str().expect("UTF-8 workdir").to_owned();
        session.session_dir = Some(root.path().to_path_buf());
        session.path = Some(path.clone());
        session.save().await.expect("initial save");
        let before = std::fs::read(&path).expect("original bytes");
        assert!(attach_session_workdir(&mut session, &root.path().join("not-found")).is_err());
        assert_eq!(std::fs::read(&path).expect("after bytes"), before);
        assert!(session.entries.is_empty());
    });
}
