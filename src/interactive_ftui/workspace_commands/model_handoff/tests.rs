//! Native dispatcher/session and controlled-provider tests, not live-model proof.

use super::*;
use crate::agent::{Agent, AgentConfig, AgentSession};
use crate::compaction::ResolvedCompactionSettings;
use crate::model::{AssistantMessage, Message, StopReason, StreamEvent, TextContent, Usage};
use crate::provider::{Context, Provider, StreamOptions};
use crate::sdk::EventListeners;
use crate::tools::ToolRegistry;
use std::collections::VecDeque;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Replies {
    replies: Mutex<VecDeque<Result<String>>>,
    calls: Mutex<Vec<(String, StreamOptions)>>,
    mutate: Mutex<Option<Arc<asupersync::sync::Mutex<Session>>>>,
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for Replies {
    fn name(&self) -> &str { "handoff-fixture" }
    fn api(&self) -> &str { "handoff-fixture" }
    fn model_id(&self) -> &str { "chosen-model" }

    async fn stream(&self, context: &Context<'_>, options: &StreamOptions)
        -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>>
    {
        assert!(context.tools.is_empty());
        assert_eq!(context.messages.len(), 1);
        let Message::User(user) = &context.messages[0] else { panic!("isolated input"); };
        let UserContent::Text(prompt) = &user.content else { panic!("text projection"); };
        self.calls.lock().unwrap().push((prompt.clone(), options.clone()));
        if let Some(store) = self.mutate.lock().unwrap().take() {
            store.try_lock().unwrap().append_message(SessionMessage::User {
                content: UserContent::Text("concurrent source change".into()), timestamp: None,
            });
        }
        let text = self.replies.lock().unwrap().pop_front().expect("no extra provider request")?;
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason: StopReason::Stop,
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                stop_reason: StopReason::Stop,
                usage: Usage { input: 30, output: 20, total_tokens: 50, ..Usage::default() },
                ..AssistantMessage::default()
            },
        })])))
    }
}

fn answer(text: &str, id: usize) -> String {
    let item = json!({"text":text,"sources":[id]});
    json!({"goal":item,"state":item,"decisions":[],"failedApproaches":[],
        "openQuestions":[item],"nextSteps":[item]}).to_string()
}

fn fixture(source: &str, replies: Vec<Result<String>>) -> (AgentSessionHandle, Arc<Replies>) {
    let provider = Arc::new(Replies { replies: Mutex::new(replies.into()), ..Replies::default() });
    let mut stored = Session::in_memory();
    if !source.is_empty() {
        stored.append_message(SessionMessage::User {
            content: UserContent::Text(source.into()), timestamp: None,
        });
    }
    let agent = Agent::new(provider.clone(), ToolRegistry::new(&[], Path::new("."), None),
        AgentConfig {
            stream_options: StreamOptions {
                api_key: Some("fixture-auth-only".into()),
                session_id: Some("parent-session".into()),
                prompt_cache_key: Some("parent-cache".into()),
                max_tokens: Some(512),
                ..StreamOptions::default()
            },
            ..AgentConfig::default()
        });
    let session = AgentSession::new(agent, Arc::new(asupersync::sync::Mutex::new(stored)),
        false, ResolvedCompactionSettings::default());
    (AgentSessionHandle::from_session_with_listeners(session, EventListeners::default()), provider)
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap()
}

fn event(case: &str) {
    eprintln!("{}", json!({"schema":"pi.handoff.model.test.v1","case":case}));
}

fn history(handle: &AgentSessionHandle) -> String {
    serde_json::to_string(&handle.session_store().try_lock().unwrap().entries).unwrap()
}

#[test]
fn default_dispatcher_synthesizes_with_current_model_without_changing_history() {
    event("default_dispatcher");
    let (mut handle, provider) = fixture("Implement pagination; ask me before deploying", vec![Ok(answer("Review pagination; deployment awaits the user", 1))]);
    let before = history(&handle);
    let (sender, receiver) = std::sync::mpsc::channel();
    runtime().block_on(super::super::run(super::super::WorkspaceCommand::Handoff,
        "--model -- remaining verification", &mut handle, Path::new("."), None, None, &sender));
    let crate::interactive::PiMsg::System(output) = receiver.try_recv().unwrap() else { panic!("handoff preview"); };
    assert!(output.contains("Model-assisted handoff"));
    assert!(output.contains("[E1]"));
    assert!(output.contains("not independent verification"));
    assert!(output.contains("handoff-fixture/chosen-model"));
    assert!(output.contains("\"totalTokens\":50"));
    assert!(receiver.try_recv().is_err());
    assert_eq!(history(&handle), before);
    let calls = provider.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let envelope: Value = serde_json::from_str(&calls[0].0).unwrap();
    let evidence: Value = serde_json::from_str(envelope["prompt"].as_str().unwrap()).unwrap();
    assert_eq!(evidence["focus"], "remaining verification");
    assert_eq!(evidence["evidence"][0]["kind"], "user");
    assert_eq!(calls[0].1.api_key.as_deref(), Some("fixture-auth-only"));
    assert_ne!(calls[0].1.session_id.as_deref(), Some("parent-session"));
    assert!(calls[0].1.prompt_cache_key.is_none());
    assert_eq!(calls[0].1.max_tokens, Some(512), "respect the host token ceiling");
}

#[test]
fn old_handoff_path_remains_provider_free() {
    event("local_handoff_unchanged");
    let (mut handle, provider) = fixture("Document the unfinished task", vec![]);
    let before = history(&handle);
    let (sender, receiver) = std::sync::mpsc::channel();
    runtime().block_on(super::super::run(super::super::WorkspaceCommand::Handoff,
        "", &mut handle, Path::new("."), None, None, &sender));
    assert!(matches!(receiver.try_recv().unwrap(), crate::interactive::PiMsg::System(_)));
    assert!(provider.calls.lock().unwrap().is_empty());
    assert_eq!(history(&handle), before);
}

#[test]
fn empty_history_and_invalid_or_delivery_arguments_fail_before_provider_admission() {
    event("preflight");
    let (handle, provider) = fixture("", vec![]);
    runtime().block_on(async {
        assert!(run(&handle, "--model").await.unwrap_err().to_string().contains("HANDOFF_NO_EVIDENCE"));
        for args in ["--modelx", "--model human", "--model agent:other", "--model --", "--model --bad"] {
            assert!(run(&handle, args).await.unwrap_err().to_string().contains("Usage:"));
        }
        assert!(run(&handle, &format!("--model -- {}", "x".repeat(MAX_FOCUS_BYTES + 1))).await.is_err());
    });
    assert!(provider.calls.lock().unwrap().is_empty());
}

#[test]
fn snapshot_is_branch_local_and_records_omissions_instead_of_sending_prefixes() {
    event("branch_and_bounds");
    let mut stored = Session::in_memory();
    let root = stored.append_custom_entry("root".into(), None);
    stored.append_message(SessionMessage::User { content: UserContent::Text("sibling-only-private-text".into()), timestamp: None });
    assert!(stored.navigate_to(&root));
    stored.append_message(SessionMessage::User { content: UserContent::Text("oversized-canary".repeat(MAX_ENTRY_BYTES)), timestamp: None });
    stored.append_message(SessionMessage::User { content: UserContent::Text("recent actionable request".into()), timestamp: None });
    let input = snapshot(&stored, "").unwrap();
    assert!(!input.prompt.contains("sibling-only-private-text"));
    assert!(!input.prompt.contains("oversized-canary"));
    assert!(input.prompt.contains("recent actionable request"));
    assert_eq!(input.sources, [(3, "user")]);
    assert_eq!(input.omitted, 2);
    assert!(input.prompt.len() < MAX_EVIDENCE_BYTES);
}

#[test]
fn media_and_thinking_payloads_never_enter_the_text_evidence() {
    event("nontext_omission");
    let mut stored = Session::in_memory();
    stored.append_message(SessionMessage::User { content: UserContent::Blocks(vec![
        ContentBlock::Text(TextContent::new("Inspect the attached diagram")),
        ContentBlock::Image(crate::model::ImageContent { data: "IMAGE-CANARY".into(), mime_type: "image/png".into() }),
    ]), timestamp: None });
    let input = snapshot(&stored, "").unwrap();
    assert!(!input.prompt.contains("IMAGE-CANARY"));
    assert!(input.prompt.contains("Inspect the attached diagram"));
    assert!(input.prompt.contains("\"nontextOmitted\":true"));
    let blocks = vec![
        ContentBlock::Thinking(crate::model::ThinkingContent {
            thinking: "THINKING-CANARY".into(), thinking_signature: Some("THINKING-SIGNATURE".into()),
        }),
        ContentBlock::ToolCall(crate::model::ToolCall {
            id: "call-1".into(), name: "read".into(), arguments: json!({"path":"src/lib.rs"}),
            thought_signature: Some("CALL-SIGNATURE-CANARY".into()),
        }),
    ];
    let mut evidence = Evidence::default();
    texts(&blocks, &mut evidence).unwrap();
    let projected = encoded(&evidence, MAX_ENTRY_BYTES).unwrap();
    assert!(!projected.contains("THINKING-CANARY"));
    assert!(!projected.contains("SIGNATURE"));
    assert!(projected.contains("src/lib.rs"));
}

#[test]
fn source_and_response_secrets_are_screened_before_display() {
    event("privacy");
    let canary = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
    let (handle, provider) = fixture(&format!("github_token = {canary}\nRotate the credential"), vec![Ok(answer(canary, 1))]);
    let output = runtime().block_on(run(&handle, "--model")).unwrap();
    assert!(!provider.calls.lock().unwrap()[0].0.contains(canary));
    assert!(!output.contains(canary));
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
}

#[test]
fn bad_output_and_provider_failure_never_publish_a_partial_brief_or_retry() {
    event("failed_generation");
    for reply in [Ok(answer("invented citation", 999)), Ok("```json\n{}\n```".into()),
        Ok("{\"goal\":null,\"goal\":null}".into()), Err(Error::api("provider unavailable"))]
    {
        let (handle, provider) = fixture("Only one source", vec![reply]);
        let before = history(&handle);
        assert!(runtime().block_on(run(&handle, "--model")).is_err());
        assert_eq!(provider.calls.lock().unwrap().len(), 1);
        assert_eq!(history(&handle), before);
    }
}

#[test]
fn source_change_during_generation_refuses_a_stale_preview() {
    event("stale_source");
    let (handle, provider) = fixture("Original request", vec![Ok(answer("old summary", 1))]);
    *provider.mutate.lock().unwrap() = Some(handle.session_store());
    let error = runtime().block_on(run(&handle, "--model")).unwrap_err();
    assert!(error.to_string().contains("HANDOFF_STALE"));
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
    assert!(history(&handle).contains("concurrent source change"));
}

fn export_command(path: &Path) -> String {
    format!("--model --export {}", serde_json::to_string(path.to_str().unwrap()).unwrap())
}

#[test]
fn export_grammar_preserves_quoted_paths_and_rejects_ambiguous_options() {
    event("bundle_grammar");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("handoff 雪 with spaces.json");
    let args = format!("{} -- review tests before deployment", export_command(&path));
    let selected = bundle::Options::parse(&args).unwrap();
    assert_eq!(selected.export.as_ref(), Some(&path));
    assert_eq!(selected.focus, "review tests before deployment");
    assert!(bundle::Options::parse("--model").unwrap().export.is_none());
    for args in ["--model --export", "--model --export relative.json",
        "--model --export null", "--model --export \"unterminated",
        "--model --export \"bad\u{0000}path\"", "--model --extra"]
    {
        assert!(bundle::Options::parse(args).is_err());
    }
    for tail in ["extra", "--", "--export elsewhere", "x"] {
        let args = format!("{} {tail}", export_command(&path));
        assert!(bundle::Options::parse(&args).is_err());
    }
    let adjacent = format!("{}x", export_command(&path));
    assert!(bundle::Options::parse(&adjacent).is_err());
}

#[test]
fn dispatcher_exports_a_complete_branch_local_bundle_without_mutating_history() {
    event("bundle_dispatcher");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("continuation 雪.json");
    let (mut handle, provider) = fixture("Implement pagination without deploying", vec![Ok(answer("Review the pagination tests", 1))]);
    let source_id = {
        let store = handle.session_store();
        let mut session = store.try_lock().unwrap();
        let root = session.leaf_id().unwrap().to_owned();
        session.append_message(SessionMessage::User {
            content: UserContent::Text("sibling-private-canary".into()), timestamp: None,
        });
        assert!(session.navigate_to(&root));
        session.append_message(SessionMessage::User {
            content: UserContent::Text("selected branch still needs tests".into()), timestamp: None,
        });
        session.header.id.clone()
    };
    let before = history(&handle);
    let (sender, receiver) = std::sync::mpsc::channel();
    runtime().block_on(super::super::run(super::super::WorkspaceCommand::Handoff,
        &export_command(&path), &mut handle, directory.path(), None, None, &sender));
    let crate::interactive::PiMsg::System(output) = receiver.try_recv().unwrap() else { panic!("saved handoff"); };
    assert!(output.contains("Saved handoff bundle:"));
    assert!(!output.contains("Preview only"));
    assert_eq!(history(&handle), before);
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() <= bundle::MAX_BUNDLE_BYTES);
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["schema"], "pi.handoff.bundle.v1");
    assert_eq!(value["executionAuthorized"], false);
    assert_eq!(value["brief"]["goal"]["sources"], json!([1]));
    assert_eq!(value["source"]["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(value["coverage"]["includedEntries"], 2);
    assert_eq!(value["auxiliaryUsage"]["totalTokens"], 50);
    assert_eq!(value["sourceSnapshotDigest"].as_str().unwrap().len(), 71);
    assert!(value["markdown"].as_str().unwrap().contains("[E1]"));
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("selected branch still needs tests"));
    assert!(!text.contains("sibling-private-canary"));
    assert!(!text.contains(&source_id));
    assert!(!text.contains("fixture-auth-only"));
    assert!(!text.contains("parent-cache"));
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn exported_evidence_and_generated_text_share_the_privacy_projection() {
    event("bundle_privacy");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private.json");
    let canary = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
    let (handle, provider) = fixture(&format!("github_token = {canary}\nRotate it"), vec![Ok(answer(canary, 1))]);
    let output = runtime().block_on(run(&handle, &export_command(&path))).unwrap();
    let published = std::fs::read_to_string(path).unwrap();
    assert!(!published.contains(canary));
    assert!(!output.contains(canary));
    assert!(!provider.calls.lock().unwrap()[0].0.contains(canary));
    let bundle: Value = serde_json::from_str(&published).unwrap();
    assert!(bundle["source"]["evidence"][0]["text"][0].as_str().unwrap().contains("Rotate it"));
}

#[test]
fn occupied_or_missing_parent_destination_is_refused_before_model_work() {
    event("bundle_preflight");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("occupied.json");
    std::fs::write(&path, "keep existing user data").unwrap();
    let (handle, provider) = fixture("source", vec![]);
    runtime().block_on(async {
        let error = run(&handle, &export_command(&path)).await.unwrap_err();
        assert!(error.to_string().contains("HANDOFF_EXPORT_EXISTS"));
        assert!(run(&handle, &export_command(&directory.path().join("missing").join("out.json"))).await.is_err());
        assert!(run(&handle, &export_command(directory.path())).await.is_err());
    });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "keep existing user data");
    assert!(provider.calls.lock().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn failed_or_stale_generation_publishes_no_bundle_or_temporary_file() {
    event("bundle_failure_cleanup");
    for (reply, stale) in [
        (Ok(answer("unknown source", 999)), false),
        (Err(Error::api("provider unavailable")), false),
        (Ok(answer("stale summary", 1)), true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("never-published.json");
        let (handle, provider) = fixture("source", vec![reply]);
        if stale { *provider.mutate.lock().unwrap() = Some(handle.session_store()); }
        assert!(runtime().block_on(run(&handle, &export_command(&path))).is_err());
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        assert_eq!(provider.calls.lock().unwrap().len(), 1);
    }
}

#[test]
fn publication_never_replaces_a_destination_created_after_preflight() {
    event("bundle_publication_race");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("raced.json");
    runtime().block_on(async {
        let staged = bundle::Destination::prepare(path.clone()).unwrap()
            .stage(br#"{"complete":true}"#).unwrap();
        std::fs::write(&path, "concurrent user output").unwrap();
        assert!(staged.publish().is_err());
    });
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "concurrent user output");
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn export_size_limit_and_changed_privacy_identity_fail_before_publication() {
    event("bundle_limits");
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("oversized.json");
    runtime().block_on(async {
        let destination = bundle::Destination::prepare(path.clone()).unwrap();
        assert!(destination.stage(&vec![b'x'; bundle::MAX_BUNDLE_BYTES + 1]).is_err());
    });
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    let mut stored = Session::in_memory();
    stored.append_message(SessionMessage::User { content: UserContent::Text("source".into()), timestamp: None });
    let input = snapshot(&stored, "").unwrap();
    let mut changed: Value = serde_json::from_str(&input.prompt).unwrap();
    changed["evidence"][0]["id"] = json!(999);
    assert!(screened_source(&changed.to_string(), &input).is_err());
    let mut changed: Value = serde_json::from_str(&input.prompt).unwrap();
    changed["omittedEntries"] = json!(99);
    assert!(screened_source(&changed.to_string(), &input).is_err());
}

#[cfg(unix)]
#[test]
fn export_refuses_existing_and_dangling_destination_symlinks() {
    event("bundle_symlink");
    let directory = tempfile::tempdir().unwrap();
    let real = directory.path().join("real.json");
    std::fs::write(&real, "preserve target").unwrap();
    for (name, target) in [("existing", real.clone()), ("dangling", directory.path().join("missing"))] {
        let link = directory.path().join(name);
        std::os::unix::fs::symlink(target, &link).unwrap();
        let (handle, provider) = fixture("source", vec![]);
        assert!(runtime().block_on(run(&handle, &export_command(&link))).is_err());
        assert!(provider.calls.lock().unwrap().is_empty());
        assert!(std::fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }
    assert_eq!(std::fs::read_to_string(&real).unwrap(), "preserve target");
}
