//! E2E: full agent loop integration tests (bd-2q00).
//!
//! These tests run the real `AgentSession` + `ToolRegistry` loop end-to-end
//! with a deterministic in-process provider script. No network, no fixture
//! playback, and no mock HTTP servers.

mod common;

use async_trait::async_trait;
use common::{TestHarness, run_async};
use futures::Stream;
use pi::agent::{Agent, AgentConfig, AgentEvent, AgentSession, SemanticContextBundleInjection};
use pi::compaction::ResolvedCompactionSettings;
use pi::error::{Error, Result};
use pi::model::{
    AssistantMessage, ContentBlock, Message, StopReason, StreamEvent, TextContent, ToolCall,
    ToolResultMessage, Usage,
};
use pi::provider::{Context, Provider, StreamOptions};
use pi::semantic_workspace_graph::{
    ContextArtifactCacheScope, ContextBundleBudget, ContextBundleRequest, SemanticContextBundle,
    SemanticContextBundlePlanner, SemanticWorkspaceGraphBuilder,
};
use pi::session::Session;
use pi::tools::ToolRegistry;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone)]
enum Scenario {
    SimpleConversation,
    ToolRoundTrip {
        read_path: String,
        expected_fragment: String,
    },
    MultiTool {
        file_path: String,
        content: String,
    },
    BashTool,
    ErrorRecovery,
}

#[derive(Debug)]
struct ScriptedProvider {
    scenario: Scenario,
    stream_calls: AtomicUsize,
}

#[derive(Debug, Default)]
struct CapturedProviderContext {
    system_prompt: Option<String>,
    messages: Vec<Message>,
}

#[derive(Debug, Default)]
struct ContextCaptureProvider {
    calls: Arc<Mutex<Vec<CapturedProviderContext>>>,
}

impl ContextCaptureProvider {
    fn calls(&self) -> Arc<Mutex<Vec<CapturedProviderContext>>> {
        Arc::clone(&self.calls)
    }
}

impl ScriptedProvider {
    const fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            stream_calls: AtomicUsize::new(0),
        }
    }

    fn assistant_message(
        &self,
        stop_reason: StopReason,
        content: Vec<ContentBlock>,
        total_tokens: u64,
    ) -> AssistantMessage {
        AssistantMessage {
            content,
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            usage: Usage {
                total_tokens,
                output: total_tokens,
                ..Usage::default()
            },
            stop_reason,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        }
    }

    fn stream_done(
        &self,
        message: AssistantMessage,
    ) -> Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>> {
        let partial = self.assistant_message(StopReason::Stop, Vec::new(), 0);
        Box::pin(futures::stream::iter(vec![
            Ok(StreamEvent::Start { partial }),
            Ok(StreamEvent::Done {
                reason: message.stop_reason,
                message,
            }),
        ]))
    }

    fn context_tool_results<'a>(context: &'a Context<'a>) -> Vec<&'a ToolResultMessage> {
        context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) => Some(result.as_ref()),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
#[allow(clippy::too_many_lines)]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted-provider"
    }

    fn api(&self) -> &str {
        "scripted-api"
    }

    fn model_id(&self) -> &str {
        "scripted-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);

        match &self.scenario {
            Scenario::SimpleConversation => {
                if call_index > 0 {
                    return Err(Error::api(
                        "simple_conversation expected exactly one provider call",
                    ));
                }
                Ok(self.stream_done(self.assistant_message(
                    StopReason::Stop,
                    vec![ContentBlock::Text(TextContent::new(
                        "hello from scripted provider",
                    ))],
                    12,
                )))
            }
            Scenario::ToolRoundTrip {
                read_path,
                expected_fragment,
            } => {
                if call_index == 0 {
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "read-1".to_string(),
                            name: "read".to_string(),
                            arguments: json!({ "path": read_path }),
                            thought_signature: None,
                        })],
                        30,
                    )));
                }
                if call_index == 1 {
                    let results = Self::context_tool_results(context);
                    let Some(result) = results
                        .iter()
                        .rev()
                        .copied()
                        .find(|r| r.tool_call_id == "read-1")
                    else {
                        return Err(Error::api("tool_round_trip expected read-1 tool result"));
                    };
                    let response_content = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    if !response_content.contains(expected_fragment) {
                        return Err(Error::api("tool_round_trip missing expected read output"));
                    }
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new(
                            "package name detected: pi-agent-rust",
                        ))],
                        24,
                    )));
                }
                Err(Error::api(
                    "tool_round_trip received unexpected provider call",
                ))
            }
            Scenario::MultiTool { file_path, content } => {
                // Turn 0: write the file
                if call_index == 0 {
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "write-1".to_string(),
                            name: "write".to_string(),
                            arguments: json!({
                                "path": file_path,
                                "content": content,
                            }),
                            thought_signature: None,
                        })],
                        40,
                    )));
                }
                // Turn 1: verify write succeeded, then read the file back
                if call_index == 1 {
                    let results = Self::context_tool_results(context);
                    let Some(write_result) = results
                        .iter()
                        .rev()
                        .copied()
                        .find(|r| r.tool_call_id == "write-1")
                    else {
                        return Err(Error::api("multi_tool expected write-1 result"));
                    };
                    if write_result.is_error {
                        return Err(Error::api("multi_tool expected successful write result"));
                    }
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "read-1".to_string(),
                            name: "read".to_string(),
                            arguments: json!({ "path": file_path }),
                            thought_signature: None,
                        })],
                        30,
                    )));
                }
                // Turn 2: verify read succeeded and contains expected content
                if call_index == 2 {
                    let results = Self::context_tool_results(context);
                    let Some(read_result) = results
                        .iter()
                        .rev()
                        .copied()
                        .find(|r| r.tool_call_id == "read-1")
                    else {
                        return Err(Error::api("multi_tool expected read-1 result"));
                    };
                    if read_result.is_error {
                        return Err(Error::api("multi_tool expected successful read result"));
                    }
                    let read_text = read_result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    if !read_text.contains(content) {
                        return Err(Error::api(
                            "multi_tool read output missing expected content",
                        ));
                    }
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new("multi-tool complete"))],
                        26,
                    )));
                }
                Err(Error::api("multi_tool received unexpected provider call"))
            }
            Scenario::BashTool => {
                if call_index == 0 {
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "bash-1".to_string(),
                            name: "bash".to_string(),
                            arguments: json!({ "command": "echo hello-agent-loop" }),
                            thought_signature: None,
                        })],
                        32,
                    )));
                }
                if call_index == 1 {
                    let results = Self::context_tool_results(context);
                    let Some(result) = results
                        .iter()
                        .rev()
                        .copied()
                        .find(|r| r.tool_call_id == "bash-1")
                    else {
                        return Err(Error::api("bash_tool_e2e expected bash-1 tool result"));
                    };
                    let text = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    if !text.contains("hello-agent-loop") {
                        return Err(Error::api(
                            "bash_tool_e2e output missing expected echo content",
                        ));
                    }
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new(
                            "bash output verified: hello-agent-loop",
                        ))],
                        20,
                    )));
                }
                Err(Error::api(
                    "bash_tool_e2e received unexpected provider call",
                ))
            }
            Scenario::ErrorRecovery => {
                if call_index == 0 {
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::ToolUse,
                        vec![ContentBlock::ToolCall(ToolCall {
                            id: "bad-1".to_string(),
                            name: "missing_tool".to_string(),
                            arguments: json!({}),
                            thought_signature: None,
                        })],
                        18,
                    )));
                }
                if call_index == 1 {
                    let results = Self::context_tool_results(context);
                    let Some(result) = results
                        .iter()
                        .rev()
                        .copied()
                        .find(|r| r.tool_call_id == "bad-1")
                    else {
                        return Err(Error::api("error_recovery expected bad-1 tool result"));
                    };
                    if !result.is_error {
                        return Err(Error::api(
                            "error_recovery expected tool result marked error",
                        ));
                    }
                    let text = result
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    if !text.contains("not found") {
                        return Err(Error::api("error_recovery expected not found message"));
                    }
                    return Ok(self.stream_done(self.assistant_message(
                        StopReason::Stop,
                        vec![ContentBlock::Text(TextContent::new(
                            "recovered after invalid tool call",
                        ))],
                        16,
                    )));
                }
                Err(Error::api(
                    "error_recovery received unexpected provider call",
                ))
            }
        }
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for ContextCaptureProvider {
    fn name(&self) -> &str {
        "context-capture-provider"
    }

    fn api(&self) -> &str {
        "openai-responses"
    }

    fn model_id(&self) -> &str {
        "context-capture-model"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        {
            let mut calls = match self.calls.lock() {
                Ok(calls) => calls,
                Err(poisoned) => poisoned.into_inner(),
            };
            calls.push(CapturedProviderContext {
                system_prompt: context.system_prompt.as_ref().map(ToString::to_string),
                messages: context.messages.iter().cloned().collect(),
            });
        }

        let message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(
                "context intelligence bundle consumed",
            ))],
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            usage: Usage {
                total_tokens: 31,
                output: 31,
                ..Usage::default()
            },
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        };

        Ok(Box::pin(futures::stream::iter(vec![Ok(
            StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            },
        )])))
    }
}

#[derive(Debug, Default)]
struct EventCapture {
    timeline: Vec<serde_json::Value>,
    turn_starts: BTreeMap<usize, Instant>,
    turn_durations_ms: BTreeMap<usize, u128>,
    tool_starts: usize,
    tool_ends: usize,
}

#[derive(Debug)]
struct RunOutcome {
    message: AssistantMessage,
    capture: EventCapture,
    total_tokens: u64,
}

const fn event_label(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::AgentStart { .. } => "agent_start",
        AgentEvent::AgentEnd { .. } => "agent_end",
        AgentEvent::TurnStart { .. } => "turn_start",
        AgentEvent::TurnEnd { .. } => "turn_end",
        AgentEvent::MessageStart { .. } => "message_start",
        AgentEvent::MessageUpdate { .. } => "message_update",
        AgentEvent::MessageEnd { .. } => "message_end",
        AgentEvent::ToolExecutionStart { .. } => "tool_start",
        AgentEvent::ToolExecutionUpdate { .. } => "tool_update",
        AgentEvent::ToolExecutionEnd { .. } => "tool_end",
        AgentEvent::AutoCompactionStart { .. } => "auto_compaction_start",
        AgentEvent::AutoCompactionEnd { .. } => "auto_compaction_end",
        AgentEvent::AutoRetryStart { .. } => "auto_retry_start",
        AgentEvent::AutoRetryEnd { .. } => "auto_retry_end",
        AgentEvent::FailoverStart { .. } => "failover_start",
        AgentEvent::FailoverEnd { .. } => "failover_end",
        AgentEvent::ExtensionError { .. } => "extension_error",
    }
}

fn assistant_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>()
}

const fn tool_names() -> [&'static str; 7] {
    ["read", "write", "edit", "bash", "grep", "find", "ls"]
}

fn total_assistant_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.usage.total_tokens),
            _ => None,
        })
        .sum()
}

fn write_timeline_artifact(harness: &TestHarness, test_name: &str, capture: &EventCapture) {
    let timeline_path = harness.temp_path(format!("{test_name}.timeline.jsonl"));
    let mut file = std::fs::File::create(&timeline_path).expect("create timeline artifact");
    for entry in &capture.timeline {
        let line = serde_json::to_string(entry).expect("serialize timeline entry");
        let _ = writeln!(file, "{line}");
    }
    harness.record_artifact(format!("{test_name}.timeline.jsonl"), &timeline_path);
}

fn write_jsonl_artifacts(harness: &TestHarness, test_name: &str) {
    let log_path = harness.temp_path(format!("{test_name}.log.jsonl"));
    harness
        .write_jsonl_logs(&log_path)
        .expect("write jsonl log");
    harness.record_artifact(format!("{test_name}.log.jsonl"), &log_path);

    let normalized_log_path = harness.temp_path(format!("{test_name}.log.normalized.jsonl"));
    harness
        .write_jsonl_logs_normalized(&normalized_log_path)
        .expect("write normalized jsonl log");
    harness.record_artifact(
        format!("{test_name}.log.normalized.jsonl"),
        &normalized_log_path,
    );

    let artifacts_path = harness.temp_path(format!("{test_name}.artifacts.jsonl"));
    harness
        .write_artifact_index_jsonl(&artifacts_path)
        .expect("write artifact index jsonl");
    harness.record_artifact(format!("{test_name}.artifacts.jsonl"), &artifacts_path);
}

fn write_json_value_artifact(
    harness: &TestHarness,
    artifact_name: &str,
    entries: impl IntoIterator<Item = serde_json::Value>,
) -> PathBuf {
    let path = harness.temp_path(artifact_name);
    let mut file = fs::File::create(&path).expect("create jsonl artifact");
    for entry in entries {
        let line = serde_json::to_string(&entry).expect("serialize jsonl artifact entry");
        writeln!(file, "{line}").expect("write jsonl artifact entry");
    }
    harness.record_artifact(artifact_name, &path);
    path
}

fn init_real_git_workspace(root: &Path) {
    let output = Command::new("git")
        .args(["init", "--initial-branch", "main"])
        .current_dir(root)
        .output()
        .expect("run git init");
    assert!(
        output.status.success(),
        "git init failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[allow(clippy::too_many_lines)]
fn write_context_intelligence_workspace(root: &Path) {
    init_real_git_workspace(root);
    fs::create_dir_all(root.join("src/providers")).expect("create source dirs");
    fs::create_dir_all(root.join("tests")).expect("create tests dir");
    fs::create_dir_all(root.join("docs/evidence")).expect("create evidence dir");
    fs::create_dir_all(root.join(".beads")).expect("create beads dir");
    fs::create_dir_all(root.join("logs")).expect("create logs dir");

    fs::write(
        root.join("README.md"),
        "# Context Intelligence Fixture\n\nThe release-facing context bundle cites docs/evidence/context-intelligence-current.json.\nStrict drop-in certification cites docs/evidence/dropin-certification-verdict.json.\nOperator debugging references docs/evidence/auth-secret-transcript.json.\n",
    )
    .expect("write README");
    fs::write(
        root.join("src/agent.rs"),
        "pub struct AgentSession;\n\nimpl AgentSession {\n    pub fn assemble_context_prompt() -> &'static str {\n        \"context intelligence prompt assembly\"\n    }\n}\n",
    )
    .expect("write agent source");
    fs::write(
        root.join("src/session.rs"),
        "pub struct SessionStore;\n\nimpl SessionStore {\n    pub fn persist_context_provenance() -> bool {\n        true\n    }\n}\n",
    )
    .expect("write session source");
    fs::write(
        root.join("src/providers/openai.rs"),
        "pub fn provider_context_window() -> usize {\n    128_000\n}\n",
    )
    .expect("write provider source");
    fs::write(
        root.join("tests/context_intelligence.rs"),
        "#[test]\nfn context_intelligence_no_mock_harness() {\n    assert_eq!(2 + 2, 4);\n}\n",
    )
    .expect("write integration test");
    fs::write(
        root.join("docs/evidence/context-intelligence-current.json"),
        r#"{
  "schema": "pi.context_intelligence.evidence.v1",
  "generated_at": "2026-05-12T00:00:00Z",
  "overall_verdict": "CERTIFIED",
  "claim_surface": "release_facing",
  "validation_command": "cargo test --test context_intelligence context_intelligence_no_mock_harness"
}"#,
    )
    .expect("write current evidence");
    fs::write(
        root.join("docs/evidence/dropin-certification-verdict.json"),
        r#"{
  "schema": "pi.dropin_certification.verdict.v1",
  "generated_at": "2025-01-01T00:00:00Z",
  "overall_verdict": "CERTIFIED",
  "claim_surface": "release_facing"
}"#,
    )
    .expect("write stale evidence");
    fs::write(
        root.join("docs/evidence/auth-secret-transcript.json"),
        r#"{
  "schema": "pi.context_intelligence.secret_transcript.v1",
  "generated_at": "2026-05-12T00:00:00Z",
  "overall_verdict": "CERTIFIED",
  "claim_surface": "operator_evidence",
  "api_key": "sk-test-redacted",
  "messages": [{"role": "user", "content": "private debugging payload"}]
}"#,
    )
    .expect("write unsafe evidence");
    fs::write(
        root.join(".beads/issues.jsonl"),
        concat!(
            "{\"id\":\"bd-context\",\"title\":\"Context intelligence no-mock harness\",",
            "\"status\":\"open\",\"priority\":2,\"issue_type\":\"test\",",
            "\"description\":\"Assemble context intelligence over real source, tests, evidence, sessions, and provider loop\",",
            "\"external_ref\":\"docs/evidence/context-intelligence-current.json\"}\n",
            "{\"id\":\"bd-closed\",\"title\":\"Closed reference context\",",
            "\"status\":\"closed\",\"priority\":3,\"issue_type\":\"task\"}\n"
        ),
    )
    .expect("write beads jsonl");
    fs::write(
        root.join("logs/session-context.log"),
        "{\"event\":\"session_context\",\"request\":\"context intelligence prompt\",\"token\":\"redacted\"}\n",
    )
    .expect("write runtime log");
}

fn context_bundle_source_paths(bundle: &SemanticContextBundle) -> Vec<&str> {
    bundle
        .selected_items
        .iter()
        .map(|item| item.source_path.as_str())
        .collect()
}

fn assert_bundle_has_source(bundle: &SemanticContextBundle, source_path: &str) {
    assert!(
        bundle
            .selected_items
            .iter()
            .any(|item| item.source_path == source_path),
        "expected selected context to include {source_path}; selected={:?}",
        context_bundle_source_paths(bundle)
    );
}

fn run_scenario(
    harness: &TestHarness,
    scenario: Scenario,
    user_prompt: &str,
    max_tool_iterations: usize,
) -> RunOutcome {
    let cwd = harness.temp_dir().to_path_buf();
    let user_prompt = user_prompt.to_string();
    run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new(scenario));
        let tools = ToolRegistry::new(&tool_names(), &cwd, None);
        let config = AgentConfig {
            system_prompt: None,
            max_tool_iterations,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
        };
        let agent = Agent::new(provider, tools, config);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = AgentSession::new(
            agent,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );

        let started_at = Instant::now();
        let capture = Arc::new(Mutex::new(EventCapture::default()));
        let capture_ref = Arc::clone(&capture);
        let message = agent_session
            .run_text(user_prompt, move |event| {
                let elapsed_ms = started_at.elapsed().as_millis();
                let mut guard = match capture_ref.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                match &event {
                    AgentEvent::TurnStart { turn_index, .. } => {
                        guard.turn_starts.insert(*turn_index, Instant::now());
                    }
                    AgentEvent::TurnEnd { turn_index, .. } => {
                        if let Some(start) = guard.turn_starts.remove(turn_index) {
                            guard
                                .turn_durations_ms
                                .insert(*turn_index, start.elapsed().as_millis());
                        }
                    }
                    AgentEvent::ToolExecutionStart { .. } => {
                        guard.tool_starts += 1;
                    }
                    AgentEvent::ToolExecutionEnd { .. } => {
                        guard.tool_ends += 1;
                    }
                    _ => {}
                }
                guard.timeline.push(json!({
                    "event": event_label(&event),
                    "elapsedMs": elapsed_ms,
                }));
                drop(guard);
            })
            .await
            .expect("run agent scenario");

        agent_session
            .persist_session()
            .await
            .expect("persist session");

        let messages = {
            let cx = asupersync::Cx::for_testing();
            let guard = session.lock(&cx).await.expect("lock session");
            guard.to_messages_for_current_path()
        };
        let capture = Arc::try_unwrap(capture)
            .expect("single capture owner")
            .into_inner()
            .expect("extract event capture");
        RunOutcome {
            message,
            capture,
            total_tokens: total_assistant_tokens(&messages),
        }
    })
}

#[test]
#[allow(clippy::too_many_lines)]
fn context_intelligence_no_mock_harness() {
    let test_name = "e2e_agent_loop_context_intelligence_no_mock_harness";
    let harness = TestHarness::new(test_name);
    let root = harness.temp_dir();
    write_context_intelligence_workspace(root);

    let head = fs::read_to_string(root.join(".git/HEAD")).expect("read git HEAD");
    assert!(
        head.contains("refs/heads/main"),
        "fixture should be a real git workspace on main, got {head:?}"
    );

    let reference_time = chrono::DateTime::parse_from_rfc3339("2026-05-13T12:00:00Z")
        .expect("parse reference time")
        .with_timezone(&chrono::Utc);
    let cache_scope = ContextArtifactCacheScope::new(
        "workspace:context-intelligence-e2e",
        "branch:main",
        "session:context-intelligence-e2e",
    );
    let graph = SemanticWorkspaceGraphBuilder::new(root)
        .with_reference_time(reference_time)
        .with_cache_scope(cache_scope)
        .with_cache_ttl_seconds(900)
        .add_expected_path("logs/session-context.log")
        .build()
        .expect("build semantic graph from real workspace");
    assert!(
        graph.nodes.len() >= 16,
        "expected real graph nodes from source/tests/docs/evidence/beads/logs, got {}",
        graph.nodes.len()
    );
    assert!(
        graph
            .input_fingerprints
            .iter()
            .any(|fingerprint| fingerprint.source_path == ".beads/issues.jsonl"),
        "graph should fingerprint real Beads JSONL inputs"
    );
    assert!(
        graph
            .input_fingerprints
            .iter()
            .any(|fingerprint| fingerprint.source_path == "logs/session-context.log"),
        "graph should fingerprint runtime/session log inputs"
    );

    let request = ContextBundleRequest {
        query: Some(
            "context intelligence prompt assembly drop-in certification verdict auth secret transcript"
                .to_string(),
        ),
        bead_id: Some("bd-context".to_string()),
        changed_paths: vec![
            "src/agent.rs".to_string(),
            "src/session.rs".to_string(),
            "tests/context_intelligence.rs".to_string(),
        ],
        failing_command: Some(
            "cargo test --test context_intelligence context_intelligence_no_mock_harness"
                .to_string(),
        ),
        workspace_id: Some("workspace:context-intelligence-e2e".to_string()),
        branch: Some("main".to_string()),
        session_id: Some("session:context-intelligence-e2e".to_string()),
        generated_at_utc: Some("2026-05-13T12:00:00Z".to_string()),
        cache_ttl_seconds: 900,
        budget: ContextBundleBudget {
            max_items: 20,
            max_bytes: 20 * 1024,
        },
    };
    let planner = SemanticContextBundlePlanner::new(&graph);
    let bundle = planner.plan(&request);

    assert_eq!(bundle.schema, "pi.semantic_context_bundle.v1");
    assert!(bundle.selected_items.len() <= bundle.budget.max_items);
    assert!(bundle.estimated_bytes <= bundle.budget.max_bytes);
    assert!(bundle.invalidation_policy.cacheable);
    assert_bundle_has_source(&bundle, "src/agent.rs");
    assert_bundle_has_source(&bundle, "src/session.rs");
    assert_bundle_has_source(&bundle, "tests/context_intelligence.rs");
    assert_bundle_has_source(&bundle, "docs/evidence/context-intelligence-current.json");
    assert!(
        !bundle
            .selected_items
            .iter()
            .any(|item| item.source_path == "docs/evidence/dropin-certification-verdict.json"),
        "stale verdict raw file entries must not be selected: {:?}",
        bundle.selected_items
    );
    assert!(
        bundle.suggested_validation_commands.iter().any(|command| {
            command == "cargo test --test context_intelligence context_intelligence_no_mock_harness"
        }),
        "focused validation command missing from bundle: {:?}",
        bundle.suggested_validation_commands
    );
    assert!(
        bundle.stale_evidence_suppressions.iter().any(|item| {
            item.source_path == "docs/evidence/dropin-certification-verdict.json"
                && item.reason == "suppressed_stale_or_unsafe_evidence"
        }),
        "stale release evidence should be omitted explicitly: {:?}",
        bundle.stale_evidence_suppressions
    );
    assert!(
        bundle.excluded_items.iter().any(|item| {
            item.source_path == "docs/evidence/auth-secret-transcript.json"
                && item.reason.contains("unsafe_to_emit_by_redaction_policy")
        }),
        "unsafe credential transcript should be excluded: {:?}",
        bundle.excluded_items
    );
    assert!(bundle.redaction_summary.suppressed_unsafe_nodes > 0);
    assert!(
        bundle
            .redaction_summary
            .redacted_metadata_keys
            .contains("credential_like")
    );
    assert!(
        bundle
            .redaction_summary
            .redacted_metadata_keys
            .contains("prompt_or_payload")
    );
    assert!(
        bundle
            .redaction_summary
            .sensitive_path_kinds
            .contains("credential_path")
    );

    let replay_graph = SemanticWorkspaceGraphBuilder::new(root)
        .with_reference_time(reference_time)
        .with_cache_scope(ContextArtifactCacheScope::new(
            "workspace:context-intelligence-e2e",
            "branch:main",
            "session:context-intelligence-e2e",
        ))
        .with_cache_ttl_seconds(900)
        .add_expected_path("logs/session-context.log")
        .build()
        .expect("rebuild semantic graph");
    let replay_bundle = SemanticContextBundlePlanner::new(&replay_graph).plan(&request);
    assert_eq!(
        serde_json::to_value(&bundle).expect("serialize bundle"),
        serde_json::to_value(&replay_bundle).expect("serialize replay bundle"),
        "deterministic replay should reproduce the same semantic context bundle"
    );

    let graph_artifact = write_json_value_artifact(
        &harness,
        "context_intelligence.graph_trace.jsonl",
        graph
            .trace
            .iter()
            .map(|entry| serde_json::to_value(entry).expect("serialize graph trace")),
    );
    let planner_artifact = write_json_value_artifact(
        &harness,
        "context_intelligence.planner_decisions.jsonl",
        [
            json!({
                "schema": "pi.context_intelligence.e2e.planner.v1",
                "phase": "selected",
                "items": &bundle.selected_items,
            }),
            json!({
                "schema": "pi.context_intelligence.e2e.planner.v1",
                "phase": "excluded",
                "items": &bundle.excluded_items,
            }),
            json!({
                "schema": "pi.context_intelligence.e2e.planner.v1",
                "phase": "validation",
                "commands": &bundle.suggested_validation_commands,
            }),
        ],
    );
    assert!(
        graph_artifact
            .metadata()
            .expect("stat graph artifact")
            .len()
            > 0,
        "graph JSONL artifact should be durable"
    );
    assert!(
        planner_artifact
            .metadata()
            .expect("stat planner artifact")
            .len()
            > 0,
        "planner JSONL artifact should be durable"
    );

    let provider = ContextCaptureProvider::default();
    let calls = provider.calls();
    let agent = Agent::new(
        Arc::new(provider),
        ToolRegistry::new(&tool_names(), root, None),
        AgentConfig {
            system_prompt: None,
            max_tool_iterations: 1,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
        },
    );
    let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
        Some(root.to_path_buf()),
    )));
    let mut agent_session = AgentSession::new(
        agent,
        Arc::clone(&session),
        true,
        ResolvedCompactionSettings::default(),
    );
    agent_session.set_semantic_context_bundle(Some(
        SemanticContextBundleInjection::enabled(bundle.clone()).with_prompt_budget(8, 4096),
    ));

    let message = run_async(async move {
        let assistant = agent_session
            .run_text(
                "Use the context intelligence bundle to plan the focused fix.".to_string(),
                |_| {},
            )
            .await
            .expect("run context intelligence provider loop");
        agent_session
            .persist_session()
            .await
            .expect("persist context intelligence session");
        assistant
    });
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert!(assistant_text(&message).contains("context intelligence bundle consumed"));

    let calls = match calls.lock() {
        Ok(calls) => calls,
        Err(poisoned) => poisoned.into_inner(),
    };
    let [captured_call] = calls.as_slice() else {
        assert_eq!(calls.len(), 1);
        return;
    };
    assert!(captured_call.system_prompt.is_none());
    let custom_message = captured_call
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Custom(custom) if custom.custom_type == "semantic_context_bundle" => {
                Some(custom)
            }
            _ => None,
        });
    assert!(
        custom_message.is_some(),
        "provider should receive semantic context custom message"
    );
    let Some(custom_message) = custom_message else {
        return;
    };
    assert!(custom_message.display);
    assert!(custom_message.content.len() <= 4096);
    assert!(custom_message.content.contains("Semantic Context Bundle"));
    assert!(custom_message.content.contains("src/agent.rs"));
    assert!(
        custom_message
            .content
            .contains("tests/context_intelligence.rs")
    );
    assert!(
        custom_message
            .content
            .contains("Suppressed or excluded context")
    );
    assert!(
        custom_message.content.contains(
            "cargo test --test context_intelligence context_intelligence_no_mock_harness"
        )
    );
    assert_eq!(
        custom_message
            .details
            .as_ref()
            .and_then(|details| details.pointer("/provider/promptShape"))
            .and_then(serde_json::Value::as_str),
        Some("custom_user_message")
    );
    let prompt_artifact = write_json_value_artifact(
        &harness,
        "context_intelligence.prompt_assembly.jsonl",
        [json!({
            "schema": "pi.context_intelligence.e2e.prompt.v1",
            "provider": "context-capture-provider",
            "message_count": captured_call.messages.len(),
            "custom_type": &custom_message.custom_type,
            "content_bytes": custom_message.content.len(),
            "details": &custom_message.details,
        })],
    );
    assert!(
        prompt_artifact
            .metadata()
            .expect("stat prompt artifact")
            .len()
            > 0,
        "prompt JSONL artifact should be durable"
    );
    drop(calls);

    let session_for_assert = Arc::clone(&session);
    let stored = run_async(async move {
        let cx = asupersync::Cx::for_testing();
        let guard = session_for_assert
            .lock(&cx)
            .await
            .expect("lock persisted session");
        assert!(
            guard.path.as_ref().is_some_and(|path| path.exists()),
            "session path should exist after real persistence: {:?}",
            guard.path
        );
        guard.to_messages_for_current_path()
    });
    assert!(
        stored.iter().any(|message| {
            matches!(
                message,
                Message::Custom(custom)
                    if custom.custom_type == "semantic_context_bundle" && custom.display
            )
        }),
        "semantic context custom message should be persisted in real session: {stored:?}"
    );

    harness.log().info_ctx(
        "summary",
        "context_intelligence_no_mock_harness complete",
        |ctx| {
            ctx.push(("graph_nodes".into(), graph.nodes.len().to_string()));
            ctx.push(("graph_edges".into(), graph.edges.len().to_string()));
            ctx.push((
                "selected_items".into(),
                bundle.selected_items.len().to_string(),
            ));
            ctx.push((
                "excluded_items".into(),
                bundle.excluded_items.len().to_string(),
            ));
            ctx.push((
                "estimated_tokens".into(),
                bundle.estimated_tokens.to_string(),
            ));
        },
    );
    write_jsonl_artifacts(&harness, test_name);
}

#[test]
fn simple_conversation() {
    let test_name = "e2e_agent_loop_simple_conversation";
    let harness = TestHarness::new(test_name);

    let outcome = run_scenario(&harness, Scenario::SimpleConversation, "Say hello.", 4);

    assert_eq!(outcome.message.stop_reason, StopReason::Stop);
    assert!(assistant_text(&outcome.message).contains("hello"));
    assert_eq!(outcome.capture.tool_starts, 0);
    assert_eq!(outcome.capture.tool_ends, 0);
    assert!(outcome.total_tokens > 0);

    harness
        .log()
        .info_ctx("summary", "simple_conversation complete", |ctx| {
            ctx.push((
                "turn_count".into(),
                outcome.capture.turn_durations_ms.len().to_string(),
            ));
            ctx.push(("total_tokens".into(), outcome.total_tokens.to_string()));
        });
    write_timeline_artifact(&harness, test_name, &outcome.capture);
    write_jsonl_artifacts(&harness, test_name);
}

#[test]
fn tool_round_trip() {
    let test_name = "e2e_agent_loop_tool_round_trip";
    let harness = TestHarness::new(test_name);

    let fixture = harness.create_file("fixtures/package.txt", "package_name=pi-agent-rust\n");
    let outcome = run_scenario(
        &harness,
        Scenario::ToolRoundTrip {
            read_path: fixture.display().to_string(),
            expected_fragment: "package_name=pi-agent-rust".to_string(),
        },
        "Read the file and report the package name.",
        4,
    );

    assert_eq!(outcome.message.stop_reason, StopReason::Stop);
    assert!(assistant_text(&outcome.message).contains("pi-agent-rust"));
    assert_eq!(outcome.capture.tool_starts, 1);
    assert_eq!(outcome.capture.tool_ends, 1);
    assert!(outcome.total_tokens >= outcome.message.usage.total_tokens);

    harness
        .log()
        .info_ctx("summary", "tool_round_trip complete", |ctx| {
            ctx.push((
                "turn_count".into(),
                outcome.capture.turn_durations_ms.len().to_string(),
            ));
            ctx.push(("total_tokens".into(), outcome.total_tokens.to_string()));
        });
    write_timeline_artifact(&harness, test_name, &outcome.capture);
    write_jsonl_artifacts(&harness, test_name);
}

#[test]
fn multi_tool() {
    let test_name = "e2e_agent_loop_multi_tool";
    let harness = TestHarness::new(test_name);

    let multi_path = harness.temp_path("workspace/multi_tool.txt");
    let outcome = run_scenario(
        &harness,
        Scenario::MultiTool {
            file_path: multi_path.display().to_string(),
            content: "alpha-beta-gamma".to_string(),
        },
        "Write then read a file and summarize.",
        8,
    );

    assert_eq!(outcome.message.stop_reason, StopReason::Stop);
    assert!(assistant_text(&outcome.message).contains("multi-tool complete"));
    // Write and read are now in separate turns (parallel execution means
    // dependent tools must be in different turns).
    assert_eq!(outcome.capture.tool_starts, 2);
    assert_eq!(outcome.capture.tool_ends, 2);
    assert!(outcome.total_tokens > 0);

    harness
        .log()
        .info_ctx("summary", "multi_tool complete", |ctx| {
            ctx.push((
                "turn_count".into(),
                outcome.capture.turn_durations_ms.len().to_string(),
            ));
            ctx.push(("total_tokens".into(), outcome.total_tokens.to_string()));
        });
    write_timeline_artifact(&harness, test_name, &outcome.capture);
    write_jsonl_artifacts(&harness, test_name);
}

#[test]
fn bash_tool_e2e() {
    let test_name = "e2e_agent_loop_bash_tool";
    let harness = TestHarness::new(test_name);

    let outcome = run_scenario(
        &harness,
        Scenario::BashTool,
        "Run a bash command and report the output.",
        4,
    );

    assert_eq!(outcome.message.stop_reason, StopReason::Stop);
    assert!(assistant_text(&outcome.message).contains("hello-agent-loop"));
    assert_eq!(outcome.capture.tool_starts, 1);
    assert_eq!(outcome.capture.tool_ends, 1);
    assert!(outcome.total_tokens > 0);

    harness
        .log()
        .info_ctx("summary", "bash_tool_e2e complete", |ctx| {
            ctx.push((
                "turn_count".into(),
                outcome.capture.turn_durations_ms.len().to_string(),
            ));
            ctx.push(("total_tokens".into(), outcome.total_tokens.to_string()));
        });
    write_timeline_artifact(&harness, test_name, &outcome.capture);
    write_jsonl_artifacts(&harness, test_name);
}

#[test]
fn error_recovery() {
    let test_name = "e2e_agent_loop_error_recovery";
    let harness = TestHarness::new(test_name);

    let outcome = run_scenario(
        &harness,
        Scenario::ErrorRecovery,
        "Call an invalid tool and then recover.",
        4,
    );

    assert_eq!(outcome.message.stop_reason, StopReason::Stop);
    assert!(assistant_text(&outcome.message).contains("recovered"));
    assert_eq!(outcome.capture.tool_starts, 1);
    assert_eq!(outcome.capture.tool_ends, 1);
    assert!(outcome.total_tokens > 0);

    harness
        .log()
        .info_ctx("summary", "error_recovery complete", |ctx| {
            ctx.push((
                "turn_count".into(),
                outcome.capture.turn_durations_ms.len().to_string(),
            ));
            ctx.push(("total_tokens".into(), outcome.total_tokens.to_string()));
        });
    write_timeline_artifact(&harness, test_name, &outcome.capture);
    write_jsonl_artifacts(&harness, test_name);
}

// ─── #126: streaming tool-call arguments must reach RPC/ACP partials ────────

/// #126 regression provider: streams a single tool call's arguments in raw
/// fragments, the way OpenAI-compatible backends deliver them. Turn 0 streams
/// `ToolCallStart` → fragmented `ToolCallDelta`s → `ToolCallEnd` → `Done`.
#[derive(Debug)]
struct StreamingToolCallProvider {
    fragments: &'static [&'static str],
    stream_calls: AtomicUsize,
}

impl StreamingToolCallProvider {
    fn full_arguments(&self) -> serde_json::Value {
        serde_json::from_str(&self.fragments.concat()).expect("fragments join into valid JSON")
    }
}

#[async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for StreamingToolCallProvider {
    fn name(&self) -> &str {
        "streaming-tool-call-provider"
    }

    fn api(&self) -> &str {
        "scripted-api"
    }

    fn model_id(&self) -> &str {
        "scripted-model"
    }

    async fn stream(
        &self,
        _context: &Context<'_>,
        _options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        let call_index = self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let empty_partial = AssistantMessage {
            content: Vec::new(),
            api: self.api().to_string(),
            provider: self.name().to_string(),
            model: self.model_id().to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        };
        let tool_call = ToolCall {
            id: "read-stream-1".to_string(),
            name: "read".to_string(),
            arguments: self.full_arguments(),
            thought_signature: None,
        };
        // The agent loop executes the tool call and asks for a follow-up
        // turn; end the run with plain text so the scenario stays single-tool.
        if call_index > 0 {
            let done_message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new("done streaming"))],
                stop_reason: StopReason::Stop,
                stop_details: None,
                ..empty_partial.clone()
            };
            return Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamEvent::Start {
                    partial: empty_partial,
                }),
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message: done_message,
                }),
            ])));
        }

        let final_message = AssistantMessage {
            content: vec![ContentBlock::ToolCall(tool_call.clone())],
            stop_reason: StopReason::ToolUse,
            stop_details: None,
            ..empty_partial.clone()
        };

        let mut events = vec![
            Ok(StreamEvent::Start {
                partial: empty_partial,
            }),
            Ok(StreamEvent::ToolCallStart {
                content_index: 0,
                id: tool_call.id.clone(),
                name: tool_call.name.clone(),
            }),
        ];
        for fragment in self.fragments {
            events.push(Ok(StreamEvent::ToolCallDelta {
                content_index: 0,
                delta: (*fragment).to_string(),
            }));
        }
        events.push(Ok(StreamEvent::ToolCallEnd {
            content_index: 0,
            tool_call,
        }));
        events.push(Ok(StreamEvent::Done {
            reason: StopReason::ToolUse,
            message: final_message,
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// #126 E2E at the RPC/ACP boundary: the fix for #124 updated the `OpenAI`
/// provider's INTERNAL partial, but RPC/ACP clients consume the partial the
/// agent loop rebuilds from `StreamEvent`s — which stayed `Null` on every
/// `ToolCallDelta`. This test drives the REAL `AgentSession` loop with a
/// fragment-streaming provider and serializes every emitted `AgentEvent`
/// exactly as `--mode rpc` does (`serde_json::to_string(&event)`, see
/// `rpc_agent_event_handler`), then asserts the serialized `message_update`
/// partials carry progressively-growing tool-call `arguments` mid-stream —
/// not `Null` until `toolcall_end`.
#[test]
#[allow(clippy::too_many_lines)]
fn rpc_partial_tool_call_arguments_grow_during_stream() {
    // Fragment boundaries chosen to exercise every completion mode:
    // mid-string, mid-object after a complete member, dangling `"key` with no
    // value yet, and the final fully-valid document.
    const FRAGMENTS: &[&str] = &[
        "{\"path\": \"streamed",
        "/demo-fi",
        "le.txt\", \"line",
        "\": 30}",
    ];
    let test_name = "e2e_agent_loop_rpc_partial_tool_call_arguments";
    let harness = TestHarness::new(test_name);
    let cwd = harness.temp_dir().to_path_buf();

    let expected_progression: [serde_json::Value; 4] = [
        json!({"path": "streamed"}),
        json!({"path": "streamed/demo-fi"}),
        json!({"path": "streamed/demo-file.txt"}),
        json!({"path": "streamed/demo-file.txt", "line": 30}),
    ];

    let serialized_events = run_async(async move {
        let provider: Arc<dyn Provider> = Arc::new(StreamingToolCallProvider {
            fragments: FRAGMENTS,
            stream_calls: AtomicUsize::new(0),
        });
        let tools = ToolRegistry::new(&tool_names(), &cwd, None);
        let config = AgentConfig {
            system_prompt: None,
            max_tool_iterations: 2,
            stream_options: StreamOptions {
                api_key: Some("test-key".to_string()),
                ..StreamOptions::default()
            },
            block_images: false,
            fail_closed_hooks: false,
            tool_approval: None,
        };
        let agent = Agent::new(provider, tools, config);
        let session = Arc::new(asupersync::sync::Mutex::new(Session::create_with_dir(
            Some(cwd.clone()),
        )));
        let mut agent_session = AgentSession::new(
            agent,
            Arc::clone(&session),
            true,
            ResolvedCompactionSettings::default(),
        );

        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_ref = Arc::clone(&captured);
        agent_session
            .run_text("stream a large tool call".to_string(), move |event| {
                // Serialize at capture time, exactly like the RPC surface does
                // (`rpc_agent_event_handler`): what lands in this string is
                // byte-for-byte what an RPC client reads off stdout.
                let line = serde_json::to_string(&event).expect("serialize agent event");
                let mut guard = match captured_ref.lock() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.push(line);
            })
            .await
            .expect("run streaming tool-call scenario");

        let guard = match captured.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    });

    let events: Vec<serde_json::Value> = serialized_events
        .iter()
        .map(|line| serde_json::from_str(line).expect("captured event is valid JSON"))
        .collect();

    let tool_call_deltas: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "message_update"
                && event["assistantMessageEvent"]["type"] == "toolcall_delta"
        })
        .collect();
    assert_eq!(
        tool_call_deltas.len(),
        FRAGMENTS.len(),
        "expected one message_update per streamed fragment"
    );

    for (idx, (update, expected)) in tool_call_deltas
        .iter()
        .zip(expected_progression.iter())
        .enumerate()
    {
        for (label, args) in [
            ("message", &update["message"]["content"][0]["arguments"]),
            (
                "assistantMessageEvent.partial",
                &update["assistantMessageEvent"]["partial"]["content"][0]["arguments"],
            ),
        ] {
            assert!(
                !args.is_null(),
                "delta {idx}: {label} arguments must not be Null mid-stream \
                 (the #126 regression: RPC/ACP clients saw Null on every delta)"
            );
            assert_eq!(
                args, expected,
                "delta {idx}: {label} arguments should be the best-effort \
                 completion of the accumulated fragment prefix"
            );
        }

        // #129: the correlation key must be present on EVERY mid-stream
        // partial, not only at toolcall_end — snapshot clients key the
        // growing preview by tool-call id.
        for (label, block) in [
            ("message", &update["message"]["content"][0]),
            (
                "assistantMessageEvent.partial",
                &update["assistantMessageEvent"]["partial"]["content"][0],
            ),
        ] {
            assert_eq!(
                block["id"], "read-stream-1",
                "delta {idx}: {label} tool-call id must be populated mid-stream \
                 (the #129 regression: id stayed \"\" until toolcall_end)"
            );
            assert_eq!(
                block["name"], "read",
                "delta {idx}: {label} tool-call name must be populated mid-stream \
                 (the #129 regression: name stayed \"\" until toolcall_end)"
            );
        }
    }

    // The terminal event still carries the fully-parsed arguments.
    let tool_call_end = events
        .iter()
        .find(|event| {
            event["type"] == "message_update"
                && event["assistantMessageEvent"]["type"] == "toolcall_end"
        })
        .expect("toolcall_end message_update present");
    assert_eq!(
        tool_call_end["message"]["content"][0]["arguments"], expected_progression[3],
        "toolcall_end partial carries the final fully-parsed arguments"
    );

    harness.log().info_ctx(
        "summary",
        "rpc partial tool-call streaming verified",
        |ctx| {
            ctx.push(("delta_updates".into(), tool_call_deltas.len().to_string()));
        },
    );
    write_jsonl_artifacts(&harness, test_name);
}
