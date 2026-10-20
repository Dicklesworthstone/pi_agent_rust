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
