//! Shared native fixtures for the SDK, RPC and classic/FTUI fork entrypoints.

use super::*;
use crate::model::Usage;
use crate::session_workdir::{
    WORKDIR_BINDING_TYPE, attach_session_workdir, require_session_workdir,
};
use serde_json::json;

pub(crate) const WORKSPACE_CONTEXT: &str = "FORK_WORKSPACE_CONTEXT: continue in this project.";

pub(crate) struct WorkspaceFixture {
    pub source: Session,
    pub target_id: String,
    pub root_target_id: String,
    pub omitted_target_id: String,
    pub fork_leaf_id: String,
    pub source_file: PathBuf,
    pub source_bytes: Vec<u8>,
    pub original_cwd: PathBuf,
    pub bound_cwd: PathBuf,
    pub additional_root: PathBuf,
    latest_binding: Value,
}

impl WorkspaceFixture {
    pub fn assert_workspace(&self, fork: &Session) {
        assert_ne!(fork.header.id, self.source.header.id);
        assert_eq!(fork.header.cwd, self.original_cwd.display().to_string());
        assert_eq!(fork.additional_roots(), vec![self.additional_root.clone()]);
        assert_eq!(
            fork.header.parent_session.as_deref(),
            self.source_file.to_str()
        );
        assert_eq!(
            require_session_workdir(fork).expect("fork retains the latest attachment"),
            self.bound_cwd
        );
        let latest = fork.entries.iter().rev().find_map(|entry| match entry {
            SessionEntry::Custom(custom) if custom.custom_type == WORKDIR_BINDING_TYPE => {
                custom.data.as_ref()
            }
            _ => None,
        });
        assert_eq!(latest, Some(&self.latest_binding));
    }

    pub fn assert_selected_context(&self, fork: &Session) {
        assert_eq!(fork.leaf_id(), Some(self.fork_leaf_id.as_str()));
        let messages = fork.to_messages_for_current_path();
        assert_eq!(messages.len(), 2);
        let context = serde_json::to_string(&messages).expect("fork context");
        assert!(context.contains("keep this question"));
        assert!(context.contains("keep this answer"));
        assert!(!context.contains("revise this request"));
        assert!(!context.contains("discard this answer"));
        assert!(!context.contains("omitted branch question"));
    }
}

fn user(text: &str) -> SessionMessage {
    SessionMessage::User {
        content: UserContent::Text(text.to_string()),
        timestamp: Some(1),
    }
}

fn assistant(text: &str) -> SessionMessage {
    SessionMessage::Assistant {
        message: AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            api: "anthropic-messages".to_string(),
            provider: "anthropic".to_string(),
            model: "test-model".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 1,
        },
    }
}

/// The active conversation contains an older attachment, while the latest
/// binding and another user message live on an omitted branch. Large fixtures
/// exercise the real automatic V2 threshold without changing process globals.
pub(crate) async fn workspace_fixture(root: &Path, lazy: bool) -> WorkspaceFixture {
    let root = root.canonicalize().expect("canonical fixture root");
    let original_cwd = root.join("moved-original-workspace");
    let previous_cwd = root.join("previous-workspace");
    let bound_cwd = root.join("bound-workspace");
    let additional_root = root.join("additional-workspace");
    for directory in [&previous_cwd, &bound_cwd, &additional_root] {
        std::fs::create_dir(directory).expect("workspace directory");
    }
    std::fs::write(bound_cwd.join("AGENTS.md"), WORKSPACE_CONTEXT)
        .expect("bound workspace context");
    let source_file = root.join("source.jsonl");
    let mut source = Session::create_with_dir(Some(root.join("sessions")));
    source.header.cwd = original_cwd.display().to_string();
    source.header.provider = Some("anthropic".to_string());
    source.header.model_id = Some("test-model".to_string());
    source.header.thinking_level = Some("off".to_string());
    source.header.parent_session = Some(root.join("grandparent.jsonl").display().to_string());
    source.set_additional_roots(std::slice::from_ref(&additional_root));
    source.path = Some(source_file.clone());
    let root_target_id = source.append_message(user("keep this question"));
    attach_session_workdir(&mut source, &previous_cwd).expect("earlier attachment");
    let fork_leaf_id = source.append_message(assistant("keep this answer"));
    let target_id = source.append_message(user("revise this request"));
    let active_tip = source.append_message(assistant("discard this answer"));
    source.reset_leaf();
    attach_session_workdir(&mut source, &bound_cwd).expect("latest off-branch attachment");
    let latest_binding = match source.entries.last().expect("attachment entry") {
        SessionEntry::Custom(custom) => custom.data.clone().expect("attachment data"),
        _ => unreachable!("attachment must be a custom entry"),
    };
    let omitted_target_id = source.append_message(user("omitted branch question"));
    if lazy {
        for index in 0..10_001 {
            source.append_custom_entry("archive_fixture".to_string(), Some(json!(index)));
        }
    }
    assert!(source.navigate_to(&active_tip));
    source.save().await.expect("persist native source");
    let full_entry_count = source.entries.len();
    if lazy {
        create_v2_sidecar_from_jsonl(&source_file).expect("create V2 sidecar");
    }
    let mut source = Session::open(source_file.to_str().expect("UTF-8 source path"))
        .await
        .expect("open source");
    source.session_dir = Some(root.join("sessions"));
    if lazy {
        assert!(source.v2_partial_hydration);
        assert!(source.entries.len() < full_entry_count);
        assert!(source.get_entry(&omitted_target_id).is_none());
    }
    let source_bytes = std::fs::read(&source_file).expect("source bytes");
    WorkspaceFixture {
        source,
        target_id,
        root_target_id,
        omitted_target_id,
        fork_leaf_id,
        source_file,
        source_bytes,
        original_cwd,
        bound_cwd,
        additional_root,
        latest_binding,
    }
}

fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(Box::pin(future))
}

#[test]
fn fork_preserves_workspace_provenance_without_navigating_or_rewriting_its_source() {
    for lazy in [false, true] {
        let root = tempfile::tempdir().expect("tempdir");
        run_async(async {
            let fixture = workspace_fixture(root.path(), lazy).await;
            let source_view = serde_json::to_value((
                &fixture.source.header,
                &fixture.source.entries,
                &fixture.source.leaf_id,
            ))
            .expect("source snapshot");
            let plan = fixture
                .source
                .plan_fork_from_user_message(&fixture.target_id)
                .expect("plan fork");
            assert_eq!(plan.selected_text, "revise this request");
            let mut fork = Session::create_with_dir(Some(root.path().join("forks")));
            fork.init_from_fork_plan(plan);
            fixture.assert_workspace(&fork);
            fixture.assert_selected_context(&fork);
            assert_eq!(
                serde_json::to_value((
                    &fixture.source.header,
                    &fixture.source.entries,
                    &fixture.source.leaf_id,
                ))
                .unwrap(),
                source_view
            );
            fork.save().await.expect("persist fork");
            let mut reopened = Session::open(fork.path.as_ref().unwrap().to_str().unwrap())
                .await
                .expect("reopen independent fork");
            fixture.assert_workspace(&reopened);
            fixture.assert_selected_context(&reopened);
            let continuation = reopened.append_message(user("fork continuation"));
            assert_eq!(
                reopened
                    .get_entry(&continuation)
                    .unwrap()
                    .base()
                    .parent_id
                    .as_deref(),
                Some(fixture.fork_leaf_id.as_str())
            );
            reopened
                .save()
                .await
                .expect("persist independent continuation");
            assert_eq!(
                std::fs::read(&fixture.source_file).unwrap(),
                fixture.source_bytes
            );
        });
    }
}

#[test]
fn fork_from_root_keeps_empty_conversation_and_current_workspace_after_reopen() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let fixture = workspace_fixture(root.path(), false).await;
        let plan = fixture
            .source
            .plan_fork_from_user_message(&fixture.root_target_id)
            .unwrap();
        let mut fork = Session::create_with_dir(Some(root.path().join("forks")));
        fork.init_from_fork_plan(plan);
        fixture.assert_workspace(&fork);
        assert!(fork.leaf_id().is_none());
        assert!(fork.to_messages_for_current_path().is_empty());
        fork.save().await.expect("save root fork");
        let reopened = Session::open(fork.path.as_ref().unwrap().to_str().unwrap())
            .await
            .unwrap();
        fixture.assert_workspace(&reopened);
        assert!(reopened.leaf_id().is_none());
        assert!(reopened.to_messages_for_current_path().is_empty());
    });
}

#[test]
fn lazy_fork_can_target_an_omitted_branch_without_duplicating_its_latest_binding() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let fixture = workspace_fixture(root.path(), true).await;
        let plan = fixture
            .source
            .plan_fork_from_user_message(&fixture.omitted_target_id)
            .unwrap();
        assert_eq!(plan.selected_text, "omitted branch question");
        let mut fork = Session::in_memory();
        fork.init_from_fork_plan(plan);
        fixture.assert_workspace(&fork);
        assert_eq!(
            fork.entries.len(),
            1,
            "the latest binding is already in the copied ancestry"
        );
        assert!(fork.to_messages_for_current_path().is_empty());
        assert!(fixture.source.get_entry(&fixture.omitted_target_id).is_none());
    });
}

#[test]
fn fork_rejects_an_invalid_latest_attachment_instead_of_reviving_the_old_branch_binding() {
    let root = tempfile::tempdir().expect("tempdir");
    run_async(async {
        let mut fixture = workspace_fixture(root.path(), false).await;
        for custom_type in [WORKDIR_BINDING_TYPE, "pi.session.workdir.v2"] {
            fixture
                .source
                .append_custom_entry(custom_type.to_string(), Some(json!({"cwd": "/"})));
            let error = fixture
                .source
                .plan_fork_from_user_message(&fixture.target_id)
                .unwrap_err();
            assert!(
                error.to_string().contains("PI_SESSION_WORKDIR_BINDING_"),
                "{error}"
            );
        }
    });
}
