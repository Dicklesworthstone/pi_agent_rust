//! Persistence regressions for explicit workdir attachment (bd-yutps).
#![recursion_limit = "256"]

use std::path::Path;

use pi::model::{Message, UserContent, UserMessage};
use pi::sdk::{SessionOptions, create_agent_session};
use pi::session::Session;
use pi::session_workdir::{
    WORKDIR_BINDING_TYPE, attach_session_workdir, check_session_workdir, inspect_session_workdir,
    require_session_workdir,
};
use serde_json::json;

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

fn resumed_sdk_options(path: &Path) -> SessionOptions {
    SessionOptions {
        provider: Some("openai".to_string()),
        model: Some("gpt-4o".to_string()),
        api_key: Some("workdir-fixture-key".to_string()),
        session_path: Some(path.to_path_buf()),
        enabled_tools: Some(vec!["read".to_string(), "write".to_string()]),
        ..SessionOptions::default()
    }
}

#[allow(clippy::too_many_lines)]
async fn sdk_resumes_attached_workspace(root: &Path, extension: &str, lazy: bool) {
    let original = root.join("moved-away");
    let replacement = root.join("replacement");
    let additional = root.join("additional-workspace");
    std::fs::create_dir_all(replacement.join(".pi")).expect("replacement workspace");
    std::fs::create_dir(&additional).expect("additional workspace");
    std::fs::write(replacement.join("probe.txt"), "saved workspace content")
        .expect("workspace probe");
    std::fs::write(
        replacement.join("AGENTS.md"),
        "WORKDIR_FIXTURE: use the recovered project context.",
    )
    .expect("project instructions");
    std::fs::write(
        replacement.join(".pi/settings.json"),
        r#"{"secrets":{"mode":"block","extra_patterns":["WORKDIR-[0-9]{6}"]}}"#,
    )
    .expect("project configuration");
    let path = root.join(format!("saved.{extension}"));
    let mut source = Session::in_memory();
    source.header.cwd = original.to_str().expect("UTF-8 original").to_owned();
    source.path = Some(path.clone());
    source.session_dir = Some(root.to_path_buf());
    source.set_additional_roots(&[additional.clone(), root.join("missing-additional-workspace")]);
    attach_session_workdir(&mut source, &replacement).expect("explicit attachment");
    if lazy {
        // The production lazy threshold is 10,000 entries. Put the binding
        // on an omitted branch so relying on the decoded runtime view would
        // incorrectly select the missing original directory.
        for index in 0..10_001 {
            source.append_custom_entry("fixture_archive".to_string(), Some(json!(index)));
        }
    }
    source.reset_leaf();
    source.append_model_message(Message::User(UserMessage {
        content: UserContent::Text("resume this branch".to_string()),
        timestamp: 1,
    }));
    source.save().await.expect("save source session");
    if lazy {
        pi::session::create_v2_sidecar_from_jsonl(&path).expect("create V2 sidecar");
        let partial = Session::open(path.to_str().expect("UTF-8 path"))
            .await
            .expect("lazy native open");
        assert!(
            partial.entries.len() < source.entries.len(),
            "fixture must exercise a partially hydrated runtime"
        );
        assert!(!partial.entries.iter().any(|entry| matches!(
            entry,
            pi::session::SessionEntry::Custom(custom)
                if custom.custom_type == WORKDIR_BINDING_TYPE
        )));
    }
    let process_cwd = std::env::current_dir().expect("process cwd");
    let mut options = resumed_sdk_options(&path);
    options.workspace_trusted = true;
    let mut handle = create_agent_session(options)
        .await
        .expect("resume bound workspace");
    assert_eq!(std::env::current_dir().expect("unchanged cwd"), process_cwd);
    assert!(
        handle
            .session()
            .agent
            .system_prompt()
            .expect("system prompt")
            .contains("WORKDIR_FIXTURE")
    );
    assert!(
        handle
            .session_mut()
            .agent
            .secrets_transform_outbound_text("WORKDIR-123456")
            .is_err(),
        "project configuration must come from the saved workspace"
    );
    let tools = handle.session().agent.shared_tools().snapshot();
    let read = tools
        .get("read")
        .expect("read tool")
        .execute("workdir-read", json!({"path": "probe.txt"}), None)
        .await
        .expect("read in bound workspace");
    assert!(!read.is_error);
    assert!(
        serde_json::to_string(&read)
            .unwrap()
            .contains("saved workspace content")
    );
    let write = tools
        .get("write")
        .expect("write tool")
        .execute(
            "workdir-write",
            json!({"path": "continued.txt", "content": "continued in the attached workspace"}),
            None,
        )
        .await
        .expect("write in bound workspace");
    assert!(!write.is_error);
    assert_eq!(
        std::fs::read_to_string(replacement.join("continued.txt")).expect("bound output"),
        "continued in the attached workspace"
    );
    assert_eq!(
        handle.workspace().expect("restored workspace").additional_roots(),
        vec![additional.canonicalize().expect("canonical additional root")]
    );
    let extra_output = additional.join("continued.txt");
    let write = tools
        .get("write")
        .expect("write tool")
        .execute(
            "workdir-additional-write",
            json!({"path": extra_output, "content": "continued in the restored additional root"}),
            None,
        )
        .await
        .expect("write in restored additional root");
    assert!(!write.is_error);
    handle.remove_workspace_root(&additional).await.expect("revoke root");
    assert!(
        tools
            .get("write")
            .expect("write tool")
            .execute(
                "workdir-revoked-write",
                json!({"path": extra_output, "content": "must not be written"}),
                None,
            )
            .await
            .is_err(),
        "restored tool handles must observe root revocation immediately"
    );
    assert_eq!(
        std::fs::read_to_string(&extra_output).expect("additional output"),
        "continued in the restored additional root"
    );
    assert!(!original.exists());
    let header = handle
        .with_session(|session| session.header.clone())
        .await
        .unwrap();
    assert_eq!(header.id, source.header.id);
    assert_eq!(
        header.cwd, source.header.cwd,
        "original provenance stays intact"
    );
    assert!(handle.shutdown_owned_resources().await.completed_cleanly());
}

#[test]
fn sdk_resume_uses_saved_workspace_for_configuration_and_real_tools() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(sdk_resumes_attached_workspace(root.path(), "jsonl", false));
}

#[test]
fn sdk_resume_finds_a_workdir_binding_outside_the_lazy_active_path() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(sdk_resumes_attached_workspace(root.path(), "jsonl", true));
}

#[test]
fn explicit_reattachment_hydrates_the_latest_binding_outside_the_active_path() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let original = root.path().join("original");
        let first = root.path().join("first-attachment");
        let second = root.path().join("second-attachment");
        std::fs::create_dir(&first).expect("first workspace");
        std::fs::create_dir(&second).expect("second workspace");
        let path = root.path().join("lazy.jsonl");
        let mut source = Session::in_memory();
        source.header.cwd = original.display().to_string();
        source.path = Some(path.clone());
        source.session_dir = Some(root.path().to_path_buf());
        attach_session_workdir(&mut source, &first).expect("first attachment");
        for index in 0..10_001 {
            source.append_custom_entry("fixture_archive".to_string(), Some(json!(index)));
        }
        source.reset_leaf();
        source.append_model_message(Message::User(UserMessage {
            content: UserContent::Text("selected branch".to_string()),
            timestamp: 1,
        }));
        let selected_leaf = source.leaf_id.clone();
        source.save().await.expect("save source");
        pi::session::create_v2_sidecar_from_jsonl(&path).expect("V2 sidecar");
        let mut partial = Session::open(path.to_str().expect("UTF-8 path"))
            .await
            .expect("open partial source");
        assert!(partial.entries.len() < source.entries.len());
        assert!(attach_session_workdir(&mut partial, &second).expect("reattach lazy session"));
        assert_eq!(partial.entries.len(), source.entries.len() + 1);
        let pi::session::SessionEntry::Custom(binding) = partial.entries.last().unwrap() else {
            unreachable!("attachment must be a custom entry");
        };
        assert_eq!(binding.base.parent_id, selected_leaf);
        assert_eq!(binding.custom_type, WORKDIR_BINDING_TYPE);
        assert_eq!(
            binding.data.as_ref().expect("binding data")["previousCwd"],
            first.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(partial.header.cwd, source.header.cwd);
        partial.save().await.expect("persist latest attachment");
        let saved = pi::session_workdir::inspect_saved_session_workdir(&path)
            .await
            .expect("saved attachment metadata");
        assert_eq!(
            saved.resolve_runtime_cwd(None).expect("resolve reattachment"),
            second.canonicalize().unwrap()
        );
    });
}

#[cfg(feature = "sqlite-sessions")]
#[test]
fn sdk_resume_uses_saved_sqlite_workspace_for_real_tools() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(sdk_resumes_attached_workspace(root.path(), "sqlite", false));
}

#[test]
fn sdk_resume_rejects_other_or_missing_workspaces_before_project_startup() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let bound = root.path().join("bound");
        let unrelated = root.path().join("unrelated");
        std::fs::create_dir(&bound).expect("bound workspace");
        std::fs::create_dir_all(unrelated.join(".pi")).expect("unrelated workspace");
        // A cwd check after config loading would return a parse error instead
        // of the named mismatch, or could start project extensions first.
        std::fs::write(unrelated.join(".pi/settings.json"), "{invalid config")
            .expect("malformed unrelated config");
        let path = root.path().join("saved.jsonl");
        let mut source = Session::in_memory();
        source.header.cwd = bound.to_str().expect("UTF-8 bound").to_owned();
        source.path = Some(path.clone());
        source.session_dir = Some(root.path().to_path_buf());
        source.save().await.expect("save source");
        let original_bytes = std::fs::read(&path).expect("source bytes");
        for use_workspace_handle in [false, true] {
            let mut options = resumed_sdk_options(&path);
            options.workspace_trusted = true;
            if use_workspace_handle {
                options.workspace = Some(pi::workspace::WorkspaceHandle::single(&unrelated));
            } else {
                options.working_directory = Some(unrelated.clone());
            }
            let error = create_agent_session(options)
                .await
                .err()
                .expect("reject mismatch");
            assert!(
                error.to_string().contains("PI_SESSION_WORKDIR_MISMATCH"),
                "{error}"
            );
            assert_eq!(
                std::fs::read(&path).expect("source unchanged"),
                original_bytes
            );
        }
        std::fs::rename(&bound, root.path().join("moved-bound")).expect("move workspace");
        let error = create_agent_session(resumed_sdk_options(&path))
            .await
            .err()
            .expect("reject missing workspace");
        assert!(
            error.to_string().contains("PI_SESSION_WORKDIR_UNAVAILABLE"),
            "{error}"
        );
        assert_eq!(std::fs::read(&path).expect("source unchanged"), original_bytes);
    });
}

#[test]
fn sdk_new_explicit_session_file_records_the_requested_workspace() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let path = root.path().join("new.jsonl");
        let mut options = resumed_sdk_options(&path);
        options.working_directory = Some(workspace.clone());
        let handle = create_agent_session(options)
            .await
            .expect("create explicit session");
        assert_eq!(
            handle
                .with_session(|session| session.header.cwd.clone())
                .await
                .unwrap(),
            workspace.to_str().expect("UTF-8 workspace")
        );
        assert!(handle.shutdown_owned_resources().await.completed_cleanly());
        let saved = Session::open(path.to_str().expect("UTF-8 path"))
            .await
            .expect("reopen new session");
        check_session_workdir(&saved, &workspace).expect("new session bound to SDK cwd");
    });
}

#[test]
fn sdk_explicit_workspace_roots_survive_save_and_resume_without_a_handle() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let primary = root.path().join("primary");
        let additional = root.path().join("additional");
        std::fs::create_dir(&primary).expect("primary workspace");
        std::fs::create_dir(&additional).expect("additional workspace");
        let probe = additional.join("probe.txt");
        std::fs::write(&probe, "persisted additional workspace").expect("additional probe");
        let path = root.path().join("explicit-roots.jsonl");
        let mut workspace = pi::workspace::WorkspaceHandle::single(&primary);
        workspace.add_root(&additional);
        let mut options = resumed_sdk_options(&path);
        options.working_directory = Some(primary.clone());
        options.workspace = Some(workspace);
        let handle = create_agent_session(options).await.expect("new SDK session");
        {
            let cx = pi::agent_cx::AgentCx::for_request();
            let mut stored = handle.session().session.lock(cx.cx()).await.expect("session lock");
            stored.save().await.expect("save explicit workspace roots");
        }
        drop(handle);

        let resumed = create_agent_session(resumed_sdk_options(&path))
            .await
            .expect("resume with only the saved path");
        assert_eq!(
            resumed.workspace().expect("restored handle").additional_roots(),
            vec![additional.canonicalize().expect("canonical additional root")]
        );
        let tools = resumed.session().agent.shared_tools().snapshot();
        let read = tools
            .get("read")
            .expect("read tool")
            .execute("read-restored-explicit-root", json!({"path": probe}), None)
            .await
            .expect("read restored additional workspace");
        assert!(!read.is_error);
        assert!(
            serde_json::to_string(&read)
                .expect("read output")
                .contains("persisted additional workspace")
        );
    });
}
