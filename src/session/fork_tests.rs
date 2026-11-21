//! Shared native fixtures for the SDK, RPC and classic/FTUI fork entrypoints.

use super::*;
use crate::model::Usage;
use crate::session_workdir::{
    WORKDIR_BINDING_TYPE, attach_session_workdir, require_session_workdir,
};
use serde_json::json;

pub const WORKSPACE_CONTEXT: &str = "FORK_WORKSPACE_CONTEXT: continue in this project.";
pub const MULTIMODAL_TEXT: &str = "Compare the originals.\nKeep the details.";

pub fn multimodal_content() -> Vec<ContentBlock> {
    vec![
        ContentBlock::Image(crate::model::ImageContent {
            data: "aGVsbG8=".to_string(),
            mime_type: "image/png".to_string(),
        }),
        ContentBlock::Text(TextContent {
            text: "Compare the originals.".to_string(),
            text_signature: Some("retained text metadata".to_string()),
        }),
        ContentBlock::Media(crate::model::MediaContent {
            data: "d29ybGQ=".to_string(),
            mime_type: "audio/wav".to_string(),
            name: Some("original.wav".to_string()),
        }),
        ContentBlock::Text(TextContent::new("Keep the details.")),
        ContentBlock::Media(crate::model::MediaContent {
            data: "dmlkZW8=".to_string(),
            mime_type: "video/mp4".to_string(),
            name: Some("original.mp4".to_string()),
        }),
    ]
}

pub struct WorkspaceFixture {
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
pub async fn workspace_fixture(root: &Path, lazy: bool) -> WorkspaceFixture {
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
fn fork_from_a_reopened_multimodal_message_returns_exact_unsent_content() {
    let root = tempfile::tempdir().unwrap();
    run_async(async {
        let mut source = Session::create_with_dir(Some(root.path().to_path_buf()));
        source.append_message(user("retained context"));
        let selected = source.append_message(SessionMessage::User {
            content: UserContent::Blocks(multimodal_content()),
            timestamp: Some(2),
        });
        source.append_message(assistant("answer to omit"));
        source.save().await.unwrap();
        let path = source.path.as_ref().unwrap();
        let original_bytes = std::fs::read(path).unwrap();
        let reopened = Session::open(path.to_str().unwrap()).await.unwrap();
        let plan = reopened.plan_fork_from_user_message(&selected).unwrap();
        assert_eq!(plan.selected_text, MULTIMODAL_TEXT);
        assert_eq!(
            serde_json::to_value(plan.selected_content.as_ref().unwrap()).unwrap(),
            serde_json::to_value(multimodal_content()).unwrap(),
        );
        let mut fork = Session::in_memory();
        fork.init_from_fork_plan(plan);
        let context = fork.to_messages_for_current_path();
        assert_eq!(context.len(), 1, "the selected prompt is still unsent");
        assert!(
            serde_json::to_string(&context)
                .unwrap()
                .contains("retained context")
        );
        assert_eq!(std::fs::read(path).unwrap(), original_bytes);
        assert!(
            reopened
                .plan_fork_current_context()
                .unwrap()
                .selected_content
                .is_none()
        );
    });
}

#[test]
fn fork_rejects_assistant_only_blocks_without_mutating_the_source() {
    for content in [
        Vec::new(),
        vec![ContentBlock::Thinking(crate::model::ThinkingContent {
            thinking: "not a user attachment".to_string(),
            thinking_signature: None,
        })],
    ] {
        let mut source = Session::in_memory();
        let id = source.append_message(SessionMessage::User {
            content: UserContent::Blocks(content),
            timestamp: Some(1),
        });
        let before = serde_json::to_value(&source.entries).unwrap();
        assert!(source.plan_fork_from_user_message(&id).is_err());
        assert_eq!(serde_json::to_value(&source.entries).unwrap(), before);
        assert_eq!(source.leaf_id(), Some(id.as_str()));
    }
}

#[test]
fn editing_fork_prose_keeps_native_order_and_removes_old_text_metadata() {
    let original = multimodal_content();
    let unchanged = edit_fork_content(&original, MULTIMODAL_TEXT, MULTIMODAL_TEXT);
    assert_eq!(
        serde_json::to_value(&unchanged).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    let edited = edit_fork_content(&original, MULTIMODAL_TEXT, "Revise @missing-file.wav");
    let expected = vec![
        original[0].clone(),
        ContentBlock::Text(TextContent::new("Revise @missing-file.wav")),
        original[2].clone(),
        original[4].clone(),
    ];
    assert_eq!(
        serde_json::to_value(&edited).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
    let media_only = edit_fork_content(&original, MULTIMODAL_TEXT, "");
    assert_eq!(media_only.len(), 3);
    assert!(
        media_only
            .iter()
            .all(|block| !matches!(block, ContentBlock::Text(_)))
    );
    let added = edit_fork_content(&media_only, "", "New text");
    assert!(matches!(&added[0], ContentBlock::Text(text) if text.text == "New text"));
    assert_eq!(
        serde_json::to_value(&added[1..]).unwrap(),
        serde_json::to_value(&media_only).unwrap()
    );
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
fn current_context_forks_keep_the_active_leaf_and_do_not_change_the_source() {
    for lazy in [false, true] {
        let root = tempfile::tempdir().expect("tempdir");
        run_async(async {
            let mut fixture = workspace_fixture(root.path(), lazy).await;
            for empty in [false, true] {
                if empty {
                    fixture.source.reset_leaf();
                }
                let source_view = serde_json::to_value((
                    &fixture.source.header,
                    &fixture.source.entries,
                    &fixture.source.leaf_id,
                    fixture.source.v2_partial_hydration,
                ))
                .expect("source snapshot");
                let selected_leaf = fixture.source.leaf_id().map(str::to_owned);
                let expected = serde_json::to_value(fixture.source.to_messages_for_current_path())
                    .expect("selected context");
                let plan = fixture
                    .source
                    .plan_fork_current_context()
                    .expect("plan fork");
                assert!(plan.selected_text.is_empty());
                assert_eq!(plan.leaf_id, selected_leaf);
                let mut fork = Session::create_with_dir(Some(root.path().join("forks")));
                fork.init_from_fork_plan(plan);
                fixture.assert_workspace(&fork);
                assert_eq!(
                    serde_json::to_value(fork.to_messages_for_current_path()).unwrap(),
                    expected
                );
                if !empty {
                    assert_eq!(expected.as_array().unwrap().len(), 4);
                    let text = expected.to_string();
                    assert!(text.contains("revise this request"));
                    assert!(text.contains("discard this answer"));
                    assert!(!text.contains("omitted branch question"));
                }
                assert_eq!(
                    serde_json::to_value((
                        &fixture.source.header,
                        &fixture.source.entries,
                        &fixture.source.leaf_id,
                        fixture.source.v2_partial_hydration,
                    ))
                    .unwrap(),
                    source_view
                );

                fork.save().await.expect("save current-context fork");
                let mut reopened = Session::open(fork.path.as_ref().unwrap().to_str().unwrap())
                    .await
                    .expect("reopen current-context fork");
                fixture.assert_workspace(&reopened);
                assert_eq!(reopened.leaf_id(), selected_leaf.as_deref());
                assert_eq!(
                    serde_json::to_value(reopened.to_messages_for_current_path()).unwrap(),
                    expected
                );
                let continuation = reopened.append_message(user("independent fork continuation"));
                assert_eq!(
                    reopened.get_entry(&continuation).unwrap().base().parent_id,
                    selected_leaf
                );
                reopened.save().await.expect("save fork continuation");
                assert_eq!(
                    std::fs::read(&fixture.source_file).unwrap(),
                    fixture.source_bytes
                );
            }
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
        assert!(
            fixture
                .source
                .get_entry(&fixture.omitted_target_id)
                .is_none()
        );
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
