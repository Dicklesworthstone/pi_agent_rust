//! Controlled provider streams through the real jobs runtime and eval kernels.
//! No live model, billing or cross-platform acceptance is implied by these fixtures.

use super::*;
use crate::model::{AssistantMessage, Message, StopReason, StreamEvent, ToolCall, UserContent};
use crate::provider::{Context, Provider, StreamOptions};
use crate::tools::{JobsTool, Tool, ToolInvocationContext, ToolInvocationHost};
use futures::{Stream, channel::oneshot};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Reply {
    text: String,
    gate: Option<oneshot::Receiver<()>>,
}

struct ProviderFixture {
    replies: Mutex<VecDeque<Reply>>,
    prompts: Mutex<Vec<String>>,
    active: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
}

struct Active(Arc<AtomicUsize>, Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl Provider for ProviderFixture {
    fn name(&self) -> &str { "fixture" }
    fn api(&self) -> &str { "fixture-stream" }
    fn model_id(&self) -> &str { "background-completion" }

    async fn stream(&self, context: &Context<'_>, options: &StreamOptions)
        -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        assert!(context.tools.is_empty());
        assert_eq!(context.messages.len(), 1);
        assert_eq!(options.api_key.as_deref(), Some("host-only-key"));
        assert!(options.max_tokens.is_some_and(|value| value <= 99));
        let Message::User(message) = &context.messages[0] else { panic!("user prompt"); };
        let UserContent::Text(text) = &message.content else { panic!("text prompt"); };
        self.prompts.lock().unwrap().push(text.clone());
        let reply = self.replies.lock().unwrap().pop_front().expect("one provider call per fixture reply");
        self.active.fetch_add(1, Ordering::SeqCst);
        let lease = Active(self.active.clone(), self.dropped.clone());
        Ok(Box::pin(futures::stream::once(async move {
            let _lease = lease;
            if let Some(gate) = reply.gate {
                gate.await.map_err(|_| Error::api("fixture gate dropped"))?;
            }
            let mut message = AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(reply.text))],
                stop_reason: StopReason::Stop,
                ..Default::default()
            };
            message.usage.input = 11;
            message.usage.output = 3;
            Ok(StreamEvent::Done { reason: StopReason::Stop, message })
        })))
    }
}

struct Host {
    session: String,
    jobs: JobsTool,
    enabled: AtomicBool,
}

#[async_trait::async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(&self, call: ToolCall, expected_session: &str) -> Result<ToolOutput> {
        if expected_session != self.session || !self.enabled.load(Ordering::SeqCst) || call.name != "jobs" {
            return Err(Error::tool("jobs", "fixture host denied current invocation"));
        }
        self.jobs.execute(&call.id, call.arguments, None).await
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    tool: EvalTool,
    host: Host,
    provider: Arc<ProviderFixture>,
}

fn fixture(replies: Vec<Reply>) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(ProviderFixture {
        replies: Mutex::new(replies.into()), prompts: Mutex::new(Vec::new()),
        active: Arc::new(AtomicUsize::new(0)), dropped: Arc::new(AtomicUsize::new(0)),
    });
    let judge = Judge::new(provider.clone(), StreamOptions {
        api_key: Some("host-only-key".into()), max_tokens: Some(99), ..StreamOptions::default()
    });
    let session = format!("completion-{}", uuid::Uuid::new_v4());
    let scope = crate::jobs::JobSessionScope::fixed(&session);
    let mut tool = EvalTool::new(root.path()).with_judge(judge);
    tool.completion_artifacts = root.path().join("artifacts");
    tool.bind_job_session_scope(scope.clone());
    let mut jobs = JobsTool::new();
    jobs.bind_job_session_scope(scope);
    Fixture { _root: root, tool, host: Host { session, jobs, enabled: AtomicBool::new(true) }, provider }
}

fn ready(text: &str) -> Reply { Reply { text: text.into(), gate: None } }
fn gated(text: &str) -> (oneshot::Sender<()>, Reply) {
    let (tx, rx) = oneshot::channel();
    (tx, Reply { text: text.into(), gate: Some(rx) })
}
fn privacy() -> AuxiliaryPrivacySnapshot {
    AuxiliaryPrivacySnapshot::from_privacy(crate::text_completion::AuxiliaryPrivacy::default())
}
fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8).build().unwrap()
}
fn text(output: &ToolOutput) -> String {
    output.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()), _ => None,
    }).collect()
}
fn invocation<'a>(fixture: &'a Fixture, snapshot: &'a super::super::PrivacySnapshot<'a>) -> Invocation<'a> {
    let mut invocation = Invocation::new(fixture.tool.judge.clone(), &json!({}), None).unwrap()
        .with_host(Some(&fixture.host)).with_privacy_snapshot(Some(snapshot));
    invocation.bind_host_session(&fixture.host.session);
    invocation
}
async fn started(provider: &ProviderFixture) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while provider.active.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "native provider was never admitted");
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
    }
}
async fn launch(invocation: &mut Invocation<'_>, fixture: &Fixture, input: Value) -> String {
    let reply = invocation.start_completion(&fixture.tool, input).await.unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["schema"], "pi.completion.background.v1");
    reply["jobId"].as_str().unwrap().to_owned()
}
async fn settled(fixture: &Fixture, id: &str) -> ToolOutput {
    let job = crate::jobs::wait_async(&fixture.host.session, id, Duration::from_secs(5)).await.unwrap();
    assert_ne!(job.status, "running");
    job.result.expect("settled native result")
}
async fn cell(fixture: &Fixture, kernel: &str, code: &str, reset: bool) -> ToolOutput {
    let protect = |parts: &[&str]| crate::text_completion::redact_inputs(
        parts, &crate::text_completion::AuxiliaryPrivacy::default(),
    );
    fixture.tool.execute_with_invocation("cell", json!({
        "kernel": kernel, "code": code, "reset": reset,
    }), None, ToolInvocationContext {
        protect: &protect, host: Some(&fixture.host), snapshot_privacy: &privacy,
    }).await.unwrap()
}

#[test]
fn acknowledgment_precedes_generation_and_owned_work_outlives_its_invocation() {
    let _guard = crate::jobs::process_test_guard();
    let (release, reply) = gated("  complete text  \n");
    let fixture = fixture(vec![reply]);
    runtime().block_on(async {
        let mut call = invocation(&fixture, &privacy);
        let id = launch(&mut call, &fixture, json!({"prompt":"produce text"})).await;
        assert_eq!(call.receipts[0]["status"], "background");
        assert_eq!(call.receipts[0]["jobId"], id);
        assert_eq!(call.receipts[0]["usageKnown"], false);
        drop(call);
        started(&fixture.provider).await;
        assert_eq!(crate::jobs::wait_async(&fixture.host.session, &id, Duration::ZERO).await.unwrap().status, "running");
        release.send(()).unwrap();
        let result = settled(&fixture, &id).await;
        assert!(!result.is_error, "{}", text(&result));
        assert_eq!(text(&result), "  complete text  \n");
        let details = result.details.unwrap();
        assert_eq!(details["schema"], "pi.completion.v1");
        assert_eq!(details["usage"]["input"], 11);
        assert!(!details.to_string().contains("host-only-key"));
        assert_eq!(fixture.provider.prompts.lock().unwrap().len(), 1);
        assert_eq!(fixture.provider.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(crate::jobs::take_completion_notices(&fixture.host.session).len(), 1);
        assert!(crate::jobs::take_completion_notices(&fixture.host.session).is_empty());
    });
}

#[test]
fn both_kernels_keep_job_ids_across_cells_and_reset_without_replaying_generation() {
    let _guard = crate::jobs::process_test_guard();
    for kernel in ["js", "python"] {
        if kernel == "python" && !std::process::Command::new("python3").arg("--version")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().is_ok_and(|status| status.success()) { continue; }
        let (release, reply) = gated(r#"{"value":7}"#);
        let fixture = fixture(vec![reply]);
        runtime().block_on(async {
            let code = if kernel == "js" {
                "globalThis.completionJob = JSON.parse(await tool.call('__pi_complete_start', {prompt:'extract',schema:{type:'object',required:['value'],properties:{value:{type:'integer'}}}})).jobId; 'launched'"
            } else {
                "import json\ncompletion_job = json.loads(await tool.call('__pi_complete_start', {'prompt':'extract','schema':{'type':'object','required':['value'],'properties':{'value':{'type':'integer'}}}}))['jobId']\n'launched'"
            };
            let launched = cell(&fixture, kernel, code, false).await;
            assert!(!launched.is_error, "{}", text(&launched));
            let id = launched.details.as_ref().unwrap()["judgments"]["attempts"][0]["jobId"].as_str().unwrap().to_string();
            started(&fixture.provider).await;
            let retained = cell(&fixture, kernel, if kernel == "js" { "completionJob" } else { "completion_job" }, false).await;
            assert!(text(&retained).contains(&id));
            // Reset loses the language variable, not the already admitted job.
            let reset = cell(&fixture, kernel, "42", true).await;
            assert!(!reset.is_error);
            assert_eq!(fixture.provider.active.load(Ordering::SeqCst), 1);
            release.send(()).unwrap();
            let code = if kernel == "js" {
                format!("globalThis.result = await tool.result('jobs',{{action:'wait',jobId:{},timeoutMs:5000}}); result.details.result.details.data.value", json!(id))
            } else {
                format!("result = await tool.result('jobs', {{'action':'wait','jobId':{},'timeoutMs':5000}})\nresult['details']['result']['details']['data']['value']", json!(id))
            };
            let result = cell(&fixture, kernel, &code, false).await;
            assert!(!result.is_error, "{}", text(&result));
            assert_eq!(text(&result).trim(), "7");
            assert_eq!(fixture.provider.prompts.lock().unwrap().len(), 1);
        });
    }
}

#[test]
fn cancellation_is_owner_scoped_and_retires_the_active_provider_stream() {
    let _guard = crate::jobs::process_test_guard();
    let (_release, reply) = gated("never returned");
    let fixture = fixture(vec![reply]);
    runtime().block_on(async {
        let id = launch(&mut invocation(&fixture, &privacy), &fixture, json!({"prompt":"work"})).await;
        started(&fixture.provider).await;
        assert!(crate::jobs::cancel_async("foreign-session", &id).await.is_err());
        assert_eq!(fixture.provider.active.load(Ordering::SeqCst), 1);
        let job = crate::jobs::cancel_async(&fixture.host.session, &id).await.unwrap();
        assert_eq!(job.status, "killed");
        assert!(job.result.unwrap().is_error);
        assert_eq!(fixture.provider.active.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.provider.dropped.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn invalid_input_unbound_calls_and_missing_owned_context_never_allocate_jobs() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = fixture(vec![]);
    runtime().block_on(async {
        let calls = AtomicUsize::new(0);
        let snapshot = || { calls.fetch_add(1, Ordering::SeqCst); privacy() };
        let mut call = invocation(&fixture, &snapshot);
        for input in [json!({"prompt":""}), json!({"prompt":"x","schema":{"type":17}}),
            json!({"prompt":"x","async":true}), json!({"prompt":"x","timeout_secs":900})] {
            assert!(call.start_completion(&fixture.tool, input).await.is_err());
        }
        assert!(call.receipts.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut standalone = Invocation::new(fixture.tool.judge.clone(), &json!({}), None).unwrap();
        assert!(standalone.start_completion(&fixture.tool, json!({"prompt":"x"})).await.unwrap_err().contains("CONTEXT"));
        let mut unbound = invocation(&fixture, &snapshot);
        unbound.judge = None;
        assert!(unbound.start_completion(&fixture.tool, json!({"prompt":"x"})).await.unwrap_err().contains("UNBOUND"));
        assert!(crate::jobs::list(&fixture.host.session).unwrap().is_empty());
        assert!(fixture.provider.prompts.lock().unwrap().is_empty());
    });
}

#[test]
fn completions_share_the_existing_cell_attempt_limit_without_reclaiming_failed_jobs() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = fixture(vec![ready("not valid JSON")]);
    runtime().block_on(async {
        let mut call = invocation(&fixture, &privacy);
        let id = launch(&mut call, &fixture, json!({"prompt":"extract","schema":{"type":"object"}})).await;
        let output = settled(&fixture, &id).await;
        assert!(output.is_error);
        assert!(text(&output).contains("PI_COMPLETION_RESPONSE"));
        assert!(!text(&output).contains("not valid JSON"));
        assert_eq!(call.receipts.len(), 1);
        call.receipts.extend((1..MAX_CALLS).map(|_| json!({"status":"failed"})));
        assert!(call.start_completion(&fixture.tool, json!({"prompt":"too many"})).await.unwrap_err().contains("LIMIT"));
        assert!(call.complete(&fixture.tool.cwd, json!({"prompt":"also too many"})).await.unwrap_err().contains("LIMIT"));
        assert_eq!(fixture.provider.prompts.lock().unwrap().len(), 1);
        assert_eq!(call.receipts.len(), MAX_CALLS);
    });
}

#[test]
fn session_change_inside_snapshot_factory_prevents_publication_and_provider_admission() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = fixture(vec![]);
    let scope = fixture.tool.session_scope.clone();
    let snapshot = || {
        scope.bind(Arc::new(|| Box::pin(async { Some("replacement-session".into()) })));
        privacy()
    };
    runtime().block_on(async {
        let mut call = invocation(&fixture, &snapshot);
        let error = call.start_completion(&fixture.tool, json!({"prompt":"x"})).await.unwrap_err();
        assert!(error.contains("SESSION_CHANGED"));
        assert_eq!(call.receipts[0]["status"], "failed");
        assert!(crate::jobs::list(&fixture.host.session).unwrap().is_empty());
        assert!(fixture.provider.prompts.lock().unwrap().is_empty());
    });
}

#[test]
fn original_admission_deadline_cannot_be_restarted_when_owned_work_is_polled() {
    let fixture = fixture(vec![]);
    let request = CompletionRequest::parse(json!({"prompt":"x"})).unwrap();
    let error = runtime().block_on(complete_owned(
        fixture.tool.judge.clone().unwrap(), request, privacy(), Instant::now(),
    )).unwrap_err();
    assert!(error.to_string().contains("PI_COMPLETION_TIMEOUT"));
    assert!(fixture.provider.prompts.lock().unwrap().is_empty());
}

#[test]
fn privacy_snapshot_screens_generation_and_withholds_changed_typed_results() {
    let _guard = crate::jobs::process_test_guard();
    let settings = crate::secrets::SecretsSettings {
        mode: Some("obfuscate".into()), extra_patterns: Some(vec!["SESSION-CANARY".into()]),
        ..Default::default()
    };
    let captured = AuxiliaryPrivacySnapshot::from_privacy(
        crate::text_completion::AuxiliaryPrivacy::from_settings(Some(&settings)),
    );
    let snapshot = || captured.clone();
    let fixture = fixture(vec![ready(r#"{"value":"SESSION-CANARY"}"#)]);
    runtime().block_on(async {
        let id = launch(&mut invocation(&fixture, &snapshot), &fixture, json!({
            "prompt":"Summarize SESSION-CANARY without exposing it", "schema":{"type":"object"},
        })).await;
        let result = settled(&fixture, &id).await;
        assert!(result.is_error);
        assert!(text(&result).contains("PI_COMPLETION_PRIVACY"));
        assert!(!serde_json::to_string(&result).unwrap().contains("SESSION-CANARY"));
        let prompts = fixture.provider.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 1);
        assert!(!prompts[0].contains("SESSION-CANARY"));
    });
}

#[test]
fn later_cells_cannot_use_a_retained_id_to_bypass_current_tool_policy() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = fixture(vec![ready("done")]);
    runtime().block_on(async {
        let id = launch(&mut invocation(&fixture, &privacy), &fixture, json!({"prompt":"x"})).await;
        assert!(!settled(&fixture, &id).await.is_error);
        fixture.host.enabled.store(false, Ordering::SeqCst);
        let result = cell(&fixture, "js", &format!(
            "await tool.result('jobs', {{action:'wait',jobId:{},timeoutMs:0}})", json!(id)
        ), false).await;
        assert!(result.is_error);
        assert!(text(&result).contains("denied"));
    });
}

#[test]
fn session_shutdown_cancels_only_its_completion_jobs() {
    let _guard = crate::jobs::process_test_guard();
    let (_first_release, first_reply) = gated("first");
    let (second_release, second_reply) = gated("second");
    let first = fixture(vec![first_reply]);
    let second = fixture(vec![second_reply]);
    runtime().block_on(async {
        let first_id = launch(&mut invocation(&first, &privacy), &first, json!({"prompt":"first"})).await;
        let second_id = launch(&mut invocation(&second, &privacy), &second, json!({"prompt":"second"})).await;
        started(&first.provider).await;
        started(&second.provider).await;
        crate::jobs::kill_session(&first.host.session).await.unwrap();
        assert_eq!(first.provider.active.load(Ordering::SeqCst), 0);
        assert_eq!(second.provider.active.load(Ordering::SeqCst), 1);
        let first_job = crate::jobs::wait_async(&first.host.session, &first_id, Duration::ZERO).await.unwrap();
        assert_eq!(first_job.status, "killed");
        second_release.send(()).unwrap();
        assert_eq!(text(&settled(&second, &second_id).await), "second");
    });
}

#[test]
fn cancelled_owner_never_allocates_an_attempt_or_publishes_work() {
    let _guard = crate::jobs::process_test_guard();
    let fixture = fixture(vec![]);
    let owner = AgentCx::for_request();
    owner.set_cancel_requested(true);
    runtime().block_on(async {
        let mut call = invocation(&fixture, &privacy);
        let error = owner.with_current(call.start_completion(&fixture.tool, json!({"prompt":"x"})))
            .await.unwrap_err();
        assert!(error.contains("CANCELLED"));
        assert!(call.receipts.is_empty());
        assert!(crate::jobs::list(&fixture.host.session).unwrap().is_empty());
        assert!(fixture.provider.prompts.lock().unwrap().is_empty());
    });
}

#[test]
fn launched_work_keeps_its_privacy_snapshot_after_later_policy_changes() {
    let _guard = crate::jobs::process_test_guard();
    let settings = crate::secrets::SecretsSettings {
        mode: Some("obfuscate".into()), extra_patterns: Some(vec!["FROZEN-CANARY".into()]),
        ..Default::default()
    };
    let policy = Mutex::new(AuxiliaryPrivacySnapshot::from_privacy(
        crate::text_completion::AuxiliaryPrivacy::from_settings(Some(&settings)),
    ));
    let snapshot = || policy.lock().unwrap().clone();
    let (release, reply) = gated("FROZEN-CANARY");
    let fixture = fixture(vec![reply]);
    runtime().block_on(async {
        let id = launch(&mut invocation(&fixture, &snapshot), &fixture, json!({"prompt":"safe prompt"})).await;
        started(&fixture.provider).await;
        *policy.lock().unwrap() = privacy();
        release.send(()).unwrap();
        let output = settled(&fixture, &id).await;
        assert!(output.is_error);
        assert!(text(&output).contains("PRIVACY"));
        for entry in std::fs::read_dir(fixture.tool.completion_artifacts.join("jobs")).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let bytes = std::fs::read(path).unwrap();
                let text = String::from_utf8_lossy(&bytes);
                assert!(!text.contains("FROZEN-CANARY"));
                assert!(!text.contains("host-only-key"));
            }
        }
    });
}
