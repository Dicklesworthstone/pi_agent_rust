//! Native retained-job fixtures with controlled provider streams and real jobs.

use super::*;
use crate::eval::EvalTool;
use crate::model::{
    AssistantMessage, ImageContent, Message, StopReason, StreamEvent, ToolCall, UserContent,
};
use crate::provider::{Context, Provider, StreamOptions};
use crate::text_completion::AuxiliaryPrivacy;
use crate::tools::{JobsTool, Tool, ToolInvocationContext, ToolInvocationHost};
use futures::Stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as PollContext, Poll, Waker};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

#[derive(Default)]
struct Gate {
    open: AtomicBool,
    waiters: Mutex<Vec<Waker>>,
    dropped: AtomicUsize,
}

impl Gate {
    fn release(&self) {
        self.open.store(true, Ordering::Release);
        let waiters = std::mem::take(
            &mut *self
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for waiter in waiters {
            waiter.wake();
        }
    }
}

struct Reply {
    gate: Arc<Gate>,
    message: Option<AssistantMessage>,
}

impl Stream for Reply {
    type Item = Result<StreamEvent>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut PollContext<'_>) -> Poll<Option<Self::Item>> {
        if !self.gate.open.load(Ordering::Acquire) {
            let mut waiters = self
                .gate
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !waiters.iter().any(|waiter| waiter.will_wake(cx.waker())) {
                waiters.push(cx.waker().clone());
            }
            drop(waiters);
            if !self.gate.open.load(Ordering::Acquire) {
                return Poll::Pending;
            }
        }
        Poll::Ready(self.message.take().map(|message| {
            Ok(StreamEvent::Done {
                reason: StopReason::Stop,
                message,
            })
        }))
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        self.gate.dropped.fetch_add(1, Ordering::AcqRel);
    }
}

struct Fixture {
    gate: Arc<Gate>,
    gates: Mutex<VecDeque<Arc<Gate>>>,
    replies: Mutex<VecDeque<String>>,
    errors: Mutex<VecDeque<Error>>,
    calls: AtomicUsize,
    contents: Mutex<Vec<UserContent>>,
}

impl Fixture {
    fn new(replies: &[&str], open: bool) -> Arc<Self> {
        let gate = Arc::new(Gate::default());
        gate.open.store(open, Ordering::Release);
        Arc::new(Self {
            gate,
            gates: Mutex::new(VecDeque::new()),
            replies: Mutex::new(replies.iter().map(|text| (*text).to_owned()).collect()),
            errors: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
            contents: Mutex::new(Vec::new()),
        })
    }

    fn queue_gates(&self, states: &[bool]) -> Vec<Arc<Gate>> {
        let gates: Vec<_> = states
            .iter()
            .map(|open| {
                let gate = Arc::new(Gate::default());
                gate.open.store(*open, Ordering::Release);
                gate
            })
            .collect();
        self.gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(gates.iter().cloned());
        gates
    }
}

#[async_trait::async_trait]
impl Provider for Fixture {
    fn name(&self) -> &'static str {
        "retained-fixture"
    }
    fn api(&self) -> &'static str {
        "fixture-stream"
    }
    fn model_id(&self) -> &'static str {
        "fixed-model"
    }
    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        assert!(context.tools.is_empty()); // ubs:ignore -- A retained model request must never gain tool execution authority.
        assert_eq!(options.api_key.as_deref(), Some("FIXED-HOST-CREDENTIAL")); // ubs:ignore -- The native binding, not guest arguments, owns credentials.
        let Some(Message::User(user)) = context.messages.first() else {
            return Err(Error::api("fixture expected one user message"));
        };
        self.contents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(user.content.clone());
        self.calls.fetch_add(1, Ordering::AcqRel);
        let queued = self
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        if let Some(error) = queued {
            return Err(error);
        }
        let text = self
            .replies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| r#"{"q":0.8}"#.into());
        let mut message = AssistantMessage {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            stop_reason: StopReason::Stop,
            ..Default::default()
        };
        message.usage.input = 11;
        message.usage.output = 3;
        let gate = self
            .gates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| Arc::clone(&self.gate));
        Ok(Box::pin(Reply {
            gate,
            message: Some(message),
        }))
    }
}

struct Host {
    owner: String,
    jobs: JobsTool,
    denied: AtomicBool,
    reads: AtomicUsize,
    fail_read: Option<usize>,
}

#[async_trait::async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(&self, call: ToolCall, expected_session: &str) -> Result<ToolOutput> {
        if expected_session != self.owner {
            return Err(Error::tool("fixture", "wrong owner"));
        }
        match call.name.as_str() {
            "jobs" if self.denied.load(Ordering::Acquire) => {
                Err(Error::tool("fixture", "PRIVATE-POLICY-DIAGNOSTIC"))
            }
            "jobs" => self.jobs.execute(&call.id, call.arguments, None).await,
            "read" => {
                let index = self.reads.fetch_add(1, Ordering::AcqRel);
                if self.fail_read == Some(index) {
                    return Err(Error::tool("fixture", "PRIVATE-READ-DIAGNOSTIC"));
                }
                Ok(ToolOutput {
                    content: vec![ContentBlock::Image(ImageContent {
                        data: PNG.into(),
                        mime_type: "image/png".into(),
                    })],
                    details: None,
                    is_error: false,
                })
            }
            _ => Err(Error::tool("fixture", "unexpected host call")),
        }
    }
}

// Keep the raw completion bridge regressions alongside the typed model-job
// suite. Both exercise the same native admission and owned execution runtime.
mod legacy {
    use super::*;

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
        fn name(&self) -> &'static str {
            "fixture"
        }
        fn api(&self) -> &'static str {
            "fixture-stream"
        }
        fn model_id(&self) -> &'static str {
            "background-completion"
        }

        async fn stream(
            &self,
            context: &Context<'_>,
            options: &StreamOptions,
        ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
            assert!(context.tools.is_empty());
            assert_eq!(context.messages.len(), 1);
            assert_eq!(options.api_key.as_deref(), Some("host-only-key"));
            assert!(options.max_tokens.is_some_and(|value| value <= 99));
            let Message::User(message) = &context.messages[0] else {
                panic!("user prompt"); // ubs:ignore -- The controlled provider rejects a fixture request that loses its user message.
            };
            let UserContent::Text(text) = &message.content else {
                panic!("text prompt"); // ubs:ignore -- This legacy text-only fixture must reject accidental image dispatch.
            };
            self.prompts.lock().unwrap().push(text.clone()); // ubs:ignore -- A poisoned fixture recorder invalidates the regression; this guard ends before any await.
            let reply = self // ubs:ignore -- The controlled reply queue is fixture state; poisoning invalidates the test and no guard crosses an await.
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("one provider call per fixture reply");
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
                Ok(StreamEvent::Done {
                    reason: StopReason::Stop,
                    message,
                })
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
            if expected_session != self.session
                || !self.enabled.load(Ordering::SeqCst)
                || call.name != "jobs"
            {
                return Err(Error::tool(
                    "jobs",
                    "fixture host denied current invocation",
                ));
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
            replies: Mutex::new(replies.into()),
            prompts: Mutex::new(Vec::new()),
            active: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicUsize::new(0)),
        });
        let judge = Judge::new(
            provider.clone(),
            StreamOptions {
                api_key: Some("host-only-key".into()),
                max_tokens: Some(99),
                ..StreamOptions::default()
            },
        );
        let session = format!("completion-{}", uuid::Uuid::new_v4());
        let scope = crate::jobs::JobSessionScope::fixed(&session);
        let mut tool = EvalTool::new(root.path()).with_judge(judge);
        tool.completion_artifacts = root.path().join("artifacts");
        tool.bind_job_session_scope(scope.clone());
        let mut jobs = JobsTool::new();
        jobs.bind_job_session_scope(scope);
        Fixture {
            _root: root,
            tool,
            host: Host {
                session,
                jobs,
                enabled: AtomicBool::new(true),
            },
            provider,
        }
    }

    fn ready(text: &str) -> Reply {
        Reply {
            text: text.into(),
            gate: None,
        }
    }
    fn gated(text: &str) -> (oneshot::Sender<()>, Reply) {
        let (tx, rx) = oneshot::channel();
        (
            tx,
            Reply {
                text: text.into(),
                gate: Some(rx),
            },
        )
    }
    fn privacy() -> AuxiliaryPrivacySnapshot {
        AuxiliaryPrivacySnapshot::from_privacy(crate::text_completion::AuxiliaryPrivacy::default())
    }
    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .blocking_threads(1, 8)
            .build()
            .unwrap()
    }
    fn text(output: &ToolOutput) -> String {
        output
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect()
    }
    fn invocation<'a>(
        fixture: &'a Fixture,
        snapshot: &'a crate::eval::judgment::SnapshotPrivacy<'a>,
    ) -> Invocation<'a> {
        let mut invocation = Invocation::new(fixture.tool.judge.clone(), &json!({}), None)
            .unwrap()
            .with_host(Some(&fixture.host))
            .with_snapshot_privacy(Some(snapshot));
        invocation.bind_host_session(&fixture.host.session);
        invocation
    }
    async fn started(provider: &ProviderFixture) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while provider.active.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "native provider was never admitted"
            );
            asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(5)).await;
        }
    }
    async fn launch(invocation: &mut Invocation<'_>, fixture: &Fixture, input: Value) -> String {
        let reply = invocation
            .dispatch(&fixture.tool, "__pi_complete_start", input)
            .await
            .unwrap();
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(reply["schema"], "pi.completion.background.v1");
        assert_eq!(reply["resultSchema"], "pi.completion.v1");
        assert_eq!(reply["budgetSeconds"], 60);
        reply["jobId"].as_str().unwrap().to_owned()
    }
    async fn settled(fixture: &Fixture, id: &str) -> ToolOutput {
        let job = crate::jobs::wait_async(&fixture.host.session, id, Duration::from_secs(5))
            .await
            .unwrap();
        assert_ne!(job.status, "running");
        job.result.expect("settled native result")
    }
    async fn cell(fixture: &Fixture, kernel: &str, code: &str, reset: bool) -> ToolOutput {
        let protect = |parts: &[&str]| {
            crate::text_completion::redact_inputs(
                parts,
                &crate::text_completion::AuxiliaryPrivacy::default(),
            )
        };
        fixture
            .tool
            .execute_with_invocation(
                "cell",
                json!({
                    "kernel": kernel, "code": code, "reset": reset,
                }),
                None,
                ToolInvocationContext {
                    protect: &protect,
                    host: Some(&fixture.host),
                    snapshot_privacy: &privacy,
                },
            )
            .await
            .unwrap()
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
            assert_eq!(call.receipts[0]["operation"], "completion_background");
            assert_eq!(call.receipts[0]["jobId"], id);
            assert_eq!(call.receipts[0]["usageKnown"], false);
            drop(call);
            started(&fixture.provider).await;
            assert_eq!(
                crate::jobs::wait_async(&fixture.host.session, &id, Duration::ZERO)
                    .await
                    .unwrap()
                    .status,
                "running"
            );
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
            assert_eq!(
                crate::jobs::take_completion_notices(&fixture.host.session).len(),
                1
            );
            assert!(crate::jobs::take_completion_notices(&fixture.host.session).is_empty());
        });
    }

    #[test]
    fn both_kernels_keep_job_ids_across_cells_and_reset_without_replaying_generation() {
        let _guard = crate::jobs::process_test_guard();
        for kernel in ["js", "python"] {
            if kernel == "python"
                && !std::process::Command::new("python3")
                    .arg("--version")
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            {
                continue;
            }
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
            let id = launch(
                &mut invocation(&fixture, &privacy),
                &fixture,
                json!({"prompt":"work"}),
            )
            .await;
            started(&fixture.provider).await;
            assert!(
                crate::jobs::cancel_async("foreign-session", &id)
                    .await
                    .is_err()
            );
            assert_eq!(fixture.provider.active.load(Ordering::SeqCst), 1);
            let job = crate::jobs::cancel_async(&fixture.host.session, &id)
                .await
                .unwrap();
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
            let snapshot = || {
                calls.fetch_add(1, Ordering::SeqCst);
                privacy()
            };
            let mut call = invocation(&fixture, &snapshot);
            for input in [
                json!({"prompt":""}),
                json!({"prompt":"x","schema":{"type":17}}),
                json!({"prompt":"x","async":true}),
                json!({"prompt":"x","timeout_secs":900}),
                json!({"prompt":"x","timeout_secs":60}),
                json!({"prompt":"x","images":[]}),
            ] {
                assert!(
                    call.dispatch(&fixture.tool, "__pi_complete_start", input)
                        .await
                        .is_err()
                );
            }
            assert!(call.receipts.is_empty());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let mut standalone =
                Invocation::new(fixture.tool.judge.clone(), &json!({}), None).unwrap();
            assert!(
                standalone
                    .dispatch(&fixture.tool, "__pi_complete_start", json!({"prompt":"x"}))
                    .await
                    .unwrap_err()
                    .contains("CONTEXT")
            );
            let mut unbound = invocation(&fixture, &snapshot);
            unbound.judge = None;
            assert!(
                unbound
                    .dispatch(&fixture.tool, "__pi_complete_start", json!({"prompt":"x"}))
                    .await
                    .unwrap_err()
                    .contains("UNBOUND")
            );
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
            let id = launch(
                &mut call,
                &fixture,
                json!({"prompt":"extract","schema":{"type":"object"}}),
            )
            .await;
            let output = settled(&fixture, &id).await;
            assert!(output.is_error);
            assert!(text(&output).contains("PI_COMPLETION_RESPONSE"));
            assert!(!text(&output).contains("not valid JSON"));
            assert_eq!(call.receipts.len(), 1);
            call.receipts
                .extend((1..MAX_CALLS).map(|_| json!({"status":"failed"})));
            assert!(
                call.dispatch(
                    &fixture.tool,
                    "__pi_complete_start",
                    json!({"prompt":"too many"})
                )
                .await
                .unwrap_err()
                .contains("LIMIT")
            );
            assert!(
                call.complete(&fixture.tool.cwd, json!({"prompt":"also too many"}))
                    .await
                    .unwrap_err()
                    .contains("LIMIT")
            );
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
            scope.bind(Arc::new(|| {
                Box::pin(async { Some("replacement-session".into()) })
            }));
            privacy()
        };
        runtime().block_on(async {
            let mut call = invocation(&fixture, &snapshot);
            let error = call
                .dispatch(&fixture.tool, "__pi_complete_start", json!({"prompt":"x"}))
                .await
                .unwrap_err();
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
        let judge = fixture.tool.judge.clone().unwrap();
        let prepared = Prepared::Completion(judge.prepare_completion(request, None).unwrap());
        let error = runtime()
            .block_on(prepared.run(
                judge,
                Operation::LegacyCompletion,
                0,
                privacy(),
                Instant::now(),
                None,
            ))
            .unwrap_err();
        assert!(error.to_string().contains("PI_COMPLETION_TIMEOUT"));
        assert!(fixture.provider.prompts.lock().unwrap().is_empty());
    }

    #[test]
    fn privacy_snapshot_screens_generation_and_withholds_changed_typed_results() {
        let _guard = crate::jobs::process_test_guard();
        let settings = crate::secrets::SecretsSettings {
            mode: Some("obfuscate".into()),
            extra_patterns: Some(vec!["SESSION-CANARY".into()]),
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
            assert!(
                !serde_json::to_string(&result)
                    .unwrap()
                    .contains("SESSION-CANARY")
            );
            let prompts = fixture.provider.prompts.lock().unwrap(); // ubs:ignore -- Inspect the fixture recorder after settlement; no await follows this assertion guard.
            assert_eq!(prompts.len(), 1);
            assert!(!prompts[0].contains("SESSION-CANARY"));
            drop(prompts);
        });
    }

    #[test]
    fn later_cells_cannot_use_a_retained_id_to_bypass_current_tool_policy() {
        let _guard = crate::jobs::process_test_guard();
        let fixture = fixture(vec![ready("done")]);
        runtime().block_on(async {
            let id = launch(
                &mut invocation(&fixture, &privacy),
                &fixture,
                json!({"prompt":"x"}),
            )
            .await;
            assert!(!settled(&fixture, &id).await.is_error);
            fixture.host.enabled.store(false, Ordering::SeqCst);
            let result = cell(
                &fixture,
                "js",
                &format!(
                    "await tool.result('jobs', {{action:'wait',jobId:{},timeoutMs:0}})",
                    json!(id)
                ),
                false,
            )
            .await;
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
            let first_id = launch(
                &mut invocation(&first, &privacy),
                &first,
                json!({"prompt":"first"}),
            )
            .await;
            let second_id = launch(
                &mut invocation(&second, &privacy),
                &second,
                json!({"prompt":"second"}),
            )
            .await;
            started(&first.provider).await;
            started(&second.provider).await;
            crate::jobs::kill_session(&first.host.session)
                .await
                .unwrap();
            assert_eq!(first.provider.active.load(Ordering::SeqCst), 0);
            assert_eq!(second.provider.active.load(Ordering::SeqCst), 1);
            let first_job = crate::jobs::wait_async(&first.host.session, &first_id, Duration::ZERO)
                .await
                .unwrap();
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
            let error = owner
                .with_current(call.dispatch(
                    &fixture.tool,
                    "__pi_complete_start",
                    json!({"prompt":"x"}),
                ))
                .await
                .unwrap_err();
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
            mode: Some("obfuscate".into()),
            extra_patterns: Some(vec!["FROZEN-CANARY".into()]),
        };
        let policy = Mutex::new(AuxiliaryPrivacySnapshot::from_privacy(
            crate::text_completion::AuxiliaryPrivacy::from_settings(Some(&settings)),
        ));
        let snapshot = || policy.lock().unwrap().clone(); // ubs:ignore -- Fixture snapshot capture is synchronous and must fail if its test mutex is poisoned.
        let (release, reply) = gated("FROZEN-CANARY");
        let fixture = fixture(vec![reply]);
        runtime().block_on(async {
            let id = launch(
                &mut invocation(&fixture, &snapshot),
                &fixture,
                json!({"prompt":"safe prompt"}),
            )
            .await;
            started(&fixture.provider).await;
            *policy.lock().unwrap() = privacy(); // ubs:ignore -- This synchronous fixture policy swap releases its guard before the later provider wait.
            release.send(()).unwrap();
            let output = settled(&fixture, &id).await;
            assert!(output.is_error);
            assert!(text(&output).contains("PRIVACY"));
            for entry in std::fs::read_dir(fixture.tool.completion_artifacts.join("jobs")).unwrap()
            {
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
}

struct Harness {
    _root: tempfile::TempDir,
    tool: EvalTool,
    host: Host,
    provider: Arc<Fixture>,
}

impl Harness {
    fn new(replies: &[&str], open: bool) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let provider = Fixture::new(replies, open);
        let owner = format!("eval-model-job-{}", uuid::Uuid::new_v4().simple());
        let scope = crate::jobs::JobSessionScope::fixed(&owner);
        let mut tool = EvalTool::new(root.path()).with_judge(
            Judge::new(
                provider.clone(),
                StreamOptions {
                    api_key: Some("FIXED-HOST-CREDENTIAL".into()),
                    ..Default::default()
                },
            )
            .with_image_support(true),
        );
        tool.completion_artifacts = root.path().join("artifacts");
        tool.bind_job_session_scope(scope.clone());
        let mut jobs = JobsTool::new();
        jobs.bind_job_session_scope(scope);
        Ok(Self {
            _root: root,
            tool,
            host: Host {
                owner,
                jobs,
                denied: AtomicBool::new(false),
                reads: AtomicUsize::new(0),
                fail_read: None,
            },
            provider,
        })
    }

    fn invocation<'a>(
        &'a self,
        snapshot: &'a super::super::SnapshotPrivacy<'a>,
    ) -> Result<Invocation<'a>> {
        let mut invocation = Invocation::new(self.tool.judge.clone(), &json!({}), None)?
            .with_host(Some(&self.host))
            .with_snapshot_privacy(Some(snapshot));
        invocation.bind_host_session(&self.host.owner);
        Ok(invocation)
    }

    async fn settle(&self, acknowledgment: &Value) -> Result<crate::jobs::JobSnapshot> {
        let id = acknowledgment
            .get("jobId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::tool("fixture", "acknowledgment lacks jobId"))?;
        crate::jobs::wait_async(&self.host.owner, id, Duration::from_secs(10)).await
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.provider.gate.release();
        if let Ok(jobs) = crate::jobs::list(&self.host.owner) {
            for job in jobs {
                let _ = crate::jobs::cancel(&self.host.owner, &job.id);
            }
        }
    }
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("native retained-job fixture runtime") // ubs:ignore -- Native lifecycle regressions require the production runtime.
}

fn privacy() -> AuxiliaryPrivacySnapshot {
    AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::default())
}

fn questions() -> Value {
    json!({"q":{"type":"bool", "instructions":"Is the claim supported?"}})
}

fn request() -> Value {
    json!({"state":"evidence", "questions":questions()})
}

async fn started(harness: &Harness, count: usize) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.provider.calls.load(Ordering::Acquire) < count {
        if Instant::now() >= deadline {
            return Err(Error::tool("fixture", "provider did not start"));
        }
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }
    Ok(())
}

fn bridge_error(error: String) -> Error {
    Error::tool("fixture", error)
}

#[allow(clippy::needless_pass_by_value)] // wraps the owned bridge reply at every call site
fn acknowledgment(text: String) -> Result<Value> {
    Ok(serde_json::from_str(&text)?)
}

fn partial_details(job: &crate::jobs::JobSnapshot) -> Result<Value> {
    job.result
        .as_ref()
        .and_then(|result| result.details.as_ref())
        .and_then(|details| details.pointer("/partialResult/details"))
        .cloned()
        .ok_or_else(|| Error::tool("fixture", "missing native batch progress"))
}

async fn batch_observation(
    harness: &Harness,
    ack: &Value,
    completed: u64,
    running: u64,
) -> Result<crate::jobs::JobSnapshot> {
    let id = ack
        .get("jobId")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::tool("fixture", "missing job id"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let job = crate::jobs::wait_async(&harness.host.owner, id, Duration::ZERO).await?;
        if job.status != "running" {
            return Err(Error::tool(
                "fixture",
                "batch settled before expected progress",
            ));
        }
        if let Ok(partial) = partial_details(&job)
            && partial.get("completed").and_then(Value::as_u64) == Some(completed)
            && partial.get("running").and_then(Value::as_u64) == Some(running)
        {
            return Ok(job);
        }
        if Instant::now() >= deadline {
            return Err(Error::tool(
                "fixture",
                "expected batch progress was never published",
            ));
        }
        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(10)).await;
    }
}

#[test]
fn all_three_native_starts_keep_typed_results_and_session_ownership() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(
        &[
            r#"{"count":7}"#,
            r#"{"q":0.8}"#,
            r#"{"q":0.2}"#,
            r#"{"q":0.9}"#,
        ],
        true,
    )?;
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let requests = [
            (Operation::Completion, json!({"prompt":"count", "images":["image.png"], "schema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]}})),
            (Operation::Judgment, request()),
            (Operation::Batch, json!({"states":{"first":"first evidence","second":"second evidence"},"questions":questions(),"concurrency":1})),
        ];
        for (operation, input) in requests {
            let ack = acknowledgment(invocation.start_background(&harness.tool, operation, input).await.map_err(bridge_error)?)?;
            assert_eq!(ack.get("schema"), Some(&json!(ACK_SCHEMA))); // ubs:ignore -- The native acknowledgment must expose the agreed model-job schema.
            let settled = harness.settle(&ack).await?;
            assert_eq!(settled.status, "exited"); // ubs:ignore -- Every valid controlled request must finish successfully.
            assert_eq!(settled.command, operation.label()); // ubs:ignore -- Recovery relies on stable native operation identity.
            let result = settled.result.ok_or_else(|| Error::tool("fixture", "missing retained result"))?;
            let details = result.details.ok_or_else(|| Error::tool("fixture", "missing model details"))?;
            assert_eq!(details.get("operation"), Some(&json!(operation.name()))); // ubs:ignore -- Operation identity must survive retention.
            assert_eq!(details.pointer("/judgments/attempts/0/usageKnown"), Some(&json!(true))); // ubs:ignore -- Completed provider usage must be retained outside the guest.
            assert!(crate::jobs::wait_async("another-session", &settled.id, Duration::ZERO).await.is_err()); // ubs:ignore -- Foreign owners cannot retrieve the retained result.
            if operation == Operation::Completion {
                assert_eq!(details.pointer("/result/data/count"), Some(&json!(7))); // ubs:ignore -- Schema-validated completion data remains typed.
                assert_eq!(details.pointer("/result/imageCount"), Some(&json!(1))); // ubs:ignore -- Owned completion preparation retains the foreground image attachment.
                assert_eq!(harness.host.reads.load(Ordering::Acquire), 1); // ubs:ignore -- The attachment must pass through the launching host's read policy.
                let contents = harness.provider.contents.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                assert!(matches!(contents.first(), Some(UserContent::Blocks(blocks)) if blocks.iter().any(|block| matches!(block, ContentBlock::Image(_))))); // ubs:ignore -- Image evidence must reach the native provider as an image, not encoded prompt text.
                drop(contents);
            }
        }
        assert_eq!(invocation.receipts.len(), 4); // ubs:ignore -- A two-item batch reserves two attempts beside the single calls.
        Ok(())
    })
}

#[test]
fn retained_jobs_capture_each_selected_role_before_the_invocation_disappears() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let mut fast = Harness::new(&[r#"{"source":"fast"}"#], false)?;
    let slow = Harness::new(&[r#"{"q":0.2}"#, r#"{"q":0.3}"#], true)?;
    let fast_judge = fast
        .tool
        .judge
        .take()
        .ok_or_else(|| Error::tool("fixture", "missing fast binding"))?;
    let slow_judge = slow
        .tool
        .judge
        .clone()
        .ok_or_else(|| Error::tool("fixture", "missing slow binding"))?;
    let smol = super::super::Selection::role(&json!("smol"))
        .ok_or_else(|| Error::tool("fixture", "missing smol role"))?;
    let deep = super::super::Selection::role(&json!("slow"))
        .ok_or_else(|| Error::tool("fixture", "missing slow role"))?;
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = Invocation::new(None, &json!({"judge_model":{"role":"smol"}}), None)?
            .with_host(Some(&fast.host)).with_snapshot_privacy(Some(&factory));
        invocation.bind_host_session(&fast.host.owner);
        invocation.resolved = vec![(smol, fast_judge), (deep, slow_judge.clone())];
        let first = acknowledgment(invocation.start_background(&fast.tool, Operation::Completion,
            json!({"prompt":"extract","images":["image.png"],"schema":{"type":"object","properties":{"source":{"type":"string"}},"required":["source"]}}))
            .await.map_err(bridge_error)?)?;
        started(&fast, 1).await?;
        let mut input = request();
        input["model"] = json!("slow");
        let second = acknowledgment(invocation.start_background(&fast.tool, Operation::Judgment, input)
            .await.map_err(bridge_error)?)?;
        let third = acknowledgment(invocation.start_background(&fast.tool, Operation::Batch,
            json!({"states":["evidence"],"questions":questions(),"model":"slow"}))
            .await.map_err(bridge_error)?)?;
        assert_eq!(invocation.receipts[0]["selectedModel"]["role"], "smol"); // ubs:ignore -- Launch receipts must identify the selected role without credentials.
        assert_eq!(invocation.receipts[1]["selectedModel"]["role"], "slow"); // ubs:ignore -- Each request owns its selection independently of the cell default.
        assert_eq!(invocation.receipts[2]["selectedModel"]["role"], "slow"); // ubs:ignore -- All batch items inherit that batch's selected role.
        // A later binding change cannot retarget an already admitted native job.
        invocation.resolved[0].1 = slow_judge;
        drop(invocation);
        let second = fast.settle(&second).await?;
        let third = fast.settle(&third).await?;
        assert_eq!(second.status, "exited"); // ubs:ignore -- The slow role can finish while the fast role is gated.
        assert_eq!(third.status, "exited"); // ubs:ignore -- Role-selected batches retain the ordinary job lifecycle.
        assert_eq!(slow.provider.calls.load(Ordering::Acquire), 2); // ubs:ignore -- Both overridden requests must use the distinct native provider binding.
        fast.provider.gate.release();
        let first = fast.settle(&first).await?;
        let details = first.result.and_then(|result| result.details)
            .ok_or_else(|| Error::tool("fixture", "missing completion details"))?;
        assert_eq!(details.pointer("/result/data/source"), Some(&json!("fast"))); // ubs:ignore -- The first job must retain its original structured result after the cell is gone.
        assert_eq!(details.pointer("/result/imageCount"), Some(&json!(1))); // ubs:ignore -- Role routing must preserve native image preparation.
        assert_eq!(fast.host.reads.load(Ordering::Acquire), 1); // ubs:ignore -- Image evidence still enters through the launching Agent's read policy.
        assert_eq!(fast.provider.calls.load(Ordering::Acquire), 1); // ubs:ignore -- A later override cannot cause a repeat request or retarget the original job.
        Ok(())
    })
}

#[test]
fn helper_role_cannot_replace_a_host_binding_or_read_images_before_rejection() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(&[], true)?;
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let error = invocation
            .start_background(
                &harness.tool,
                Operation::Completion,
                json!({"prompt":"work","model":"smol","images":["image.png"]}),
            )
            .await
            .expect_err("host-bound Judge cannot be overridden"); // ubs:ignore -- Guest role selection cannot replace native host authority.
        assert!(error.contains("PI_COMPLETION_BINDING")); // ubs:ignore -- The explicit host binding must reject role replacement before admission.
        assert!(invocation.receipts.is_empty()); // ubs:ignore -- A rejected override must not reserve a provider attempt.
        assert!(invocation.tool_receipts.is_empty()); // ubs:ignore -- No jobs policy or read call is necessary for a rejected override.
        assert_eq!(harness.host.reads.load(Ordering::Acquire), 0); // ubs:ignore -- Failed role admission cannot read image evidence.
        assert!(crate::jobs::list(&harness.host.owner)?.is_empty()); // ubs:ignore -- Rejection must publish no native job.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 0); // ubs:ignore -- Rejection must not contact any provider.
        Ok(())
    })
}

#[test]
fn javascript_and_python_launches_survive_cell_return_and_kernel_reset() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    for kernel in ["js", "python"] {
        if kernel == "python"
            && !std::process::Command::new("python3")
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            continue;
        }
        let harness = Harness::new(&["retained answer"], false)?;
        runtime().block_on(async {
            let privacy = privacy();
            let protect = |parts: &[&str]| privacy.project_inputs(parts);
            let snapshot = || privacy.clone();
            let code = if kernel == "js" {
                "await tool.call('__pi_model_complete_start', {prompt:'answer'})"
            } else {
                "await tool.call('__pi_model_complete_start', {'prompt':'answer'})"
            };
            let launched = harness
                .tool
                .execute_with_invocation(
                    "launch",
                    json!({"kernel":kernel,"code":code}),
                    None,
                    ToolInvocationContext {
                        protect: &protect,
                        host: Some(&harness.host),
                        snapshot_privacy: &snapshot,
                    },
                )
                .await?;
            assert!(!launched.is_error); // ubs:ignore -- Both public kernel paths must launch the native retained task.
            let id = launched
                .details
                .as_ref()
                .and_then(|details| details.pointer("/judgments/attempts/0/jobId"))
                .and_then(Value::as_str)
                .ok_or_else(|| Error::tool("fixture", "missing native job receipt"))?
                .to_owned();
            started(&harness, 1).await?;
            let running = crate::jobs::wait_async(&harness.host.owner, &id, Duration::ZERO).await?;
            assert_eq!(running.status, "running"); // ubs:ignore -- Cell completion must not wait for or retire the provider stream.
            let reset = harness
                .tool
                .execute(
                    "reset",
                    json!({"kernel":kernel,"code":"1", "reset":true}),
                    None,
                )
                .await?;
            assert!(!reset.is_error); // ubs:ignore -- Kernel reset is independent of the retained native job.
            harness.provider.gate.release();
            let done =
                crate::jobs::wait_async(&harness.host.owner, &id, Duration::from_secs(10)).await?;
            assert_eq!(done.status, "exited"); // ubs:ignore -- Reset must not cancel an accepted model job.
            assert_eq!(
                done.result
                    .and_then(|result| result.details)
                    .and_then(|details| details.pointer("/result/text").cloned()),
                Some(json!("retained answer"))
            ); // ubs:ignore -- The job retains the final provider text across kernel reset.
            Ok::<(), Error>(())
        })?;
    }
    Ok(())
}

#[test]
fn invalid_whole_batch_and_disabled_jobs_never_start_provider_work() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let mut harness = Harness::new(&[], true)?;
    harness.host.fail_read = Some(1);
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let malformed = json!({"states":["first","second"],"questions":questions(),"images":{"0":["first.png"],"2":["invalid-index.png"]}});
        let error = invocation.start_background(&harness.tool, Operation::Batch, malformed).await.expect_err("invalid image index"); // ubs:ignore -- Invalid final descriptors must reject the whole batch before host calls.
        assert!(error.contains("PI_JUDGE_BATCH_INPUT")); // ubs:ignore -- The invalid image key must exercise the complete batch-map validator.
        assert_eq!(harness.host.reads.load(Ordering::Acquire), 0); // ubs:ignore -- No prefix image read may occur for malformed batches.
        let reads = json!({"states":["first","second"],"questions":questions(),"images":{"0":["first.png"],"1":["second.png"]}});
        let error = invocation.start_background(&harness.tool, Operation::Batch, reads).await.expect_err("second image read fails"); // ubs:ignore -- A failed later read must leave every provider request unstarted.
        assert!(error.contains("PI_JUDGE_IMAGE_READ")); // ubs:ignore -- This fixture must fail at image admission, not earlier input parsing.
        assert_eq!(harness.host.reads.load(Ordering::Acquire), 2); // ubs:ignore -- Both admitted reads must occur before the second read refuses the batch.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 0); // ubs:ignore -- Whole-batch attachment admission is atomic with respect to model dispatch.
        harness.host.denied.store(true, Ordering::Release);
        let error = invocation.start_background(&harness.tool, Operation::Completion, json!({"prompt":"work"})).await.expect_err("denied jobs"); // ubs:ignore -- Policy-denied jobs must not be admitted.
        assert!(!error.contains("PRIVATE-POLICY-DIAGNOSTIC")); // ubs:ignore -- Admission errors must not expose policy diagnostics.
        assert!(crate::jobs::list(&harness.host.owner)?.is_empty()); // ubs:ignore -- Failed preflight must not allocate even an idle retained job.
        Ok(())
    })
}

#[test]
fn background_budget_and_snapshot_authority_fail_before_host_side_effects() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(&[], true)?;
    runtime().block_on(async {
        let mut missing = Invocation::new(harness.tool.judge.clone(), &json!({}), None)?
            .with_host(Some(&harness.host));
        missing.bind_host_session(&harness.host.owner);
        assert!(
            missing
                .start_background(&harness.tool, Operation::Judgment, request())
                .await
                .is_err()
        ); // ubs:ignore -- A borrowed host alone cannot mint retained privacy authority.
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        invocation.receipts = vec![json!({"status":"completed"}); 15];
        let input = json!({"states":["first","second"],"questions":questions(),"images":{"0":["first.png"]}});
        let error = invocation.start_background(&harness.tool, Operation::Batch, input).await.expect_err("shared attempt budget exhausted"); // ubs:ignore -- Background batches share the launch cell's remaining attempt budget.
        assert!(error.contains("PI_MODEL_JOB_LIMIT")); // ubs:ignore -- A valid request must reach the shared attempt-budget gate.
        assert_eq!(harness.host.reads.load(Ordering::Acquire), 0); // ubs:ignore -- Exhausted attempt admission cannot read a prefix attachment.
        assert!(invocation.tool_receipts.is_empty()); // ubs:ignore -- Budget refusal precedes the jobs policy host call.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 0); // ubs:ignore -- Refused admission must perform no provider work.
        Ok(())
    })
}

#[test]
fn privacy_is_captured_after_reads_and_all_text_discoveries_protect_results() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(&["opaque-remembered-value"], true)?;
    runtime().block_on(async {
        let snapshot_calls = AtomicUsize::new(0);
        let factory = || {
            snapshot_calls.fetch_add(1, Ordering::AcqRel);
            assert_eq!(harness.host.reads.load(Ordering::Acquire), 2); // ubs:ignore -- Capture must occur after every policy-checked attachment read.
            privacy()
        };
        let mut invocation = harness.invocation(&factory)?;
        let batch = json!({"states":{"early":"opaque-remembered-value", "late":{"api_key":"opaque-remembered-value"}}, "questions":questions(), "images":{"early":["a.png"],"late":["b.png"]},"concurrency":1});
        // Use judgments with the normal fixture verdict; the preparation check
        // records masked text independently of the provider's response validity.
        let ack = acknowledgment(invocation.start_background(&harness.tool, Operation::Batch, batch).await.map_err(bridge_error)?)?;
        let _ = harness.settle(&ack).await?;
        assert_eq!(snapshot_calls.load(Ordering::Acquire), 1); // ubs:ignore -- One frozen privacy projection owns the entire accepted batch.
        let encoded = {
            let contents = harness.provider.contents.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            serde_json::to_string(&*contents)?
        };
        assert!(!encoded.contains("opaque-remembered-value")); // ubs:ignore -- A later secret assignment must also screen an earlier bare value.
        let factory = privacy;
        let mut completion = harness.invocation(&factory)?;
        harness.provider.replies.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push_back("opaque-output-value".into());
        let ack = acknowledgment(completion.start_background(&harness.tool, Operation::Completion,
            json!({"prompt":"api_key=opaque-output-value"})).await.map_err(bridge_error)?)?;
        let done = harness.settle(&ack).await?;
        let encoded = serde_json::to_string(&done)?;
        assert!(!encoded.contains("opaque-output-value")); // ubs:ignore -- The job's output snapshot must remember secrets discovered in launch input.
        Ok(())
    })
}

#[test]
fn mixed_batch_failure_keeps_ordered_peer_results_and_receipts() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(
        &[r#"{"q":0.9}"#, "invalid model result", r#"{"q":0.1}"#],
        true,
    )?;
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let ack = acknowledgment(
            invocation
                .start_background(
                    &harness.tool,
                    Operation::Batch,
                    json!({"states":["first","second","third"],"questions":questions(),"concurrency":1}),
                )
                .await
                .map_err(bridge_error)?,
        )?;
        let done = harness.settle(&ack).await?;
        assert_eq!(done.status, "failed"); // ubs:ignore -- Aggregate status cannot claim all items succeeded.
        let details = done
            .result
            .and_then(|result| result.details)
            .ok_or_else(|| Error::tool("fixture", "missing batch details"))?;
        assert_eq!(details.pointer("/result/succeeded"), Some(&json!(2))); // ubs:ignore -- Successful peer results survive an ordinary item failure.
        assert_eq!(details.pointer("/result/items/1/key"), Some(&json!(1))); // ubs:ignore -- Stable array indices identify results in original order.
        assert_eq!(details.pointer("/result/items/1/ok"), Some(&json!(false))); // ubs:ignore -- The failed item retains explicit failure evidence.
        assert_eq!(
            details.pointer("/judgments/attempts/2/usageKnown"),
            Some(&json!(true))
        ); // ubs:ignore -- Later successful provider usage remains visible.
        assert!(!details.to_string().contains("invalid model result")); // ubs:ignore -- Provider diagnostics cannot leak through public errors.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 3); // ubs:ignore -- No failed judgment may be retried implicitly.
        Ok(())
    })
}

#[test]
fn retries_run_after_untouched_peers_and_keep_every_attempt_receipt() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(&["invalid answer", r#"{"q":0.2}"#, r#"{"q":0.8}"#], true)?;
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let ack = acknowledgment(invocation.start_background(&harness.tool, Operation::Batch,
            json!({"states":["first","second"],"questions":questions(),"concurrency":1,"retries":1}))
            .await.map_err(bridge_error)?)?;
        assert_eq!(invocation.receipts.len(), 4); // ubs:ignore -- Reserve the worst case before detaching, even if fewer calls are made.
        let done = harness.settle(&ack).await?;
        assert_eq!(done.status, "exited"); // ubs:ignore -- A recovered typed answer makes this complete batch successful.
        let details = done.result.and_then(|result| result.details).ok_or_else(stopped)?;
        assert_eq!(details["result"]["allOk"], true); // ubs:ignore -- Every original item has a real successful verdict.
        assert_eq!(details["result"]["items"][0]["verdict"]["answers"]["q"]["bool"], 0.8); // ubs:ignore -- The third response belongs to the retry of the first item.
        assert_eq!(details["result"]["items"][1]["verdict"]["answers"]["q"]["bool"], 0.2); // ubs:ignore -- The untouched peer dispatches before the retry and is never repeated.
        let attempts = details["judgments"]["attempts"].as_array().ok_or_else(stopped)?;
        assert_eq!(attempts.len(), 3); // ubs:ignore -- Unused reservation slots are not fabricated provider attempts.
        assert_eq!(attempts[0]["status"], "failed"); // ubs:ignore -- Recovery does not erase the initial failure.
        assert_eq!(attempts[0]["retryScheduled"], true); // ubs:ignore -- Failed receipts explain why the item stayed unsettled.
        assert_eq!(attempts[2]["item"], 0); // ubs:ignore -- Retry receipts retain the original item identity.
        assert_eq!(attempts[2]["itemAttempt"], 2); // ubs:ignore -- Each actual attempt has its own ordinal.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 3); // ubs:ignore -- The successful peer must never run again.
        Ok(())
    })
}

#[test]
fn minimum_success_is_separate_from_all_items_succeeding() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    for (minimum, expected) in [(0, "exited"), (1, "exited"), (2, "failed")] {
        let harness = Harness::new(&[r#"{"q":0.7}"#, "invalid answer"], true)?;
        runtime().block_on(async {
            let factory = privacy;
            let mut invocation = harness.invocation(&factory)?;
            let ack = acknowledgment(invocation.start_background(&harness.tool, Operation::Batch,
                json!({"states":["first","second"],"questions":questions(),"concurrency":1,"min_ok":minimum}))
                .await.map_err(bridge_error)?)?;
            let done = harness.settle(&ack).await?;
            assert_eq!(done.status, expected); // ubs:ignore -- Only the explicitly requested threshold changes aggregate success.
            let details = done.result.and_then(|result| result.details).ok_or_else(stopped)?;
            assert_eq!(details["result"]["allOk"], false); // ubs:ignore -- Meeting a threshold cannot turn a failed item into success.
            assert_eq!(details["result"]["meetsMinimum"], minimum <= 1); // ubs:ignore -- The threshold is truthful and independent of allOk.
            assert_eq!(details["result"]["failed"], 1); // ubs:ignore -- Threshold success preserves failed evidence.
            assert_eq!(harness.provider.calls.load(Ordering::Acquire), 2); // ubs:ignore -- Reaching minOk never skips remaining original items.
            Ok::<(), Error>(())
        })?;
    }
    Ok(())
}

#[test]
fn retry_reservation_and_option_validation_precede_all_host_work() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    for (name, value) in [
        ("retries", json!(4)),
        ("retries", json!(-1)),
        ("retries", json!(true)),
        ("retries", json!(1.5)),
        ("min_ok", json!(2)),
        ("min_ok", json!(-1)),
        ("min_ok", json!(false)),
    ] {
        let mut input = json!({"states":["first"],"questions":questions()});
        input[name] = value;
        assert!(Request::parse(Operation::Batch, input).is_err()); // ubs:ignore -- Retry controls are bounded integers, never coerced.
    }
    let harness = Harness::new(&[], true)?;
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let result = invocation
            .start_background(
                &harness.tool,
                Operation::Batch,
                json!({"states":vec!["state"; 9],"questions":questions(),"retries":1}),
            )
            .await;
        assert!(result.is_err_and(|error| error.contains("PI_MODEL_JOB_LIMIT"))); // ubs:ignore -- All 18 possible attempts must fit before admission.
        assert!(invocation.receipts.is_empty() && invocation.tool_receipts.is_empty()); // ubs:ignore -- Rejected reservation performs neither reads nor jobs admission.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 0); // ubs:ignore -- An over-budget batch never reaches a provider.
        Ok(())
    })
}

#[test]
fn provider_retries_use_typed_failure_before_public_sanitization() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    for (error, retry) in [
        (Error::api("HTTP 503: unavailable PRIVATE-DIAGNOSTIC"), true),
        (Error::api("connection reset PRIVATE-DIAGNOSTIC"), true),
        (
            Error::Auth("503 overloaded PRIVATE-DIAGNOSTIC".into()),
            false,
        ),
        (
            Error::tool("policy", "503 overloaded PRIVATE-DIAGNOSTIC"),
            false,
        ),
        (Error::api("HTTP 403: rate limit PRIVATE-DIAGNOSTIC"), false),
        (
            Error::api("HTTP 400: usage limit field invalid PRIVATE-DIAGNOSTIC"),
            false,
        ),
        (Error::api("HTTP 504: overloaded PRIVATE-DIAGNOSTIC"), false),
        (
            Error::api("policy denied: 503 overloaded PRIVATE-DIAGNOSTIC"),
            false,
        ),
        (
            Error::api("request cancelled: 503 PRIVATE-DIAGNOSTIC"),
            false,
        ),
        (Error::Aborted, false),
    ] {
        let harness = Harness::new(&[], true)?;
        harness
            .provider
            .errors
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(error);
        runtime().block_on(async {
            let factory = privacy;
            let mut invocation = harness.invocation(&factory)?;
            let ack = acknowledgment(
                invocation
                    .start_background(
                        &harness.tool,
                        Operation::Batch,
                        json!({"states":["first"],"questions":questions(),"retries":1}),
                    )
                    .await
                    .map_err(bridge_error)?,
            )?;
            let done = harness.settle(&ack).await?;
            assert_eq!(done.status, if retry { "exited" } else { "failed" }); // ubs:ignore -- Terminal errors stay terminal despite retry-looking words.
            assert_eq!(
                harness.provider.calls.load(Ordering::Acquire),
                if retry { 2 } else { 1 }
            ); // ubs:ignore -- Only classified provider transients spend another reserved attempt.
            assert!(!serde_json::to_string(&done)?.contains("PRIVATE-DIAGNOSTIC")); // ubs:ignore -- Classifying a raw error must not expose its contents.
            Ok::<(), Error>(())
        })?;
    }
    Ok(())
}

#[test]
fn cancelling_a_retry_keeps_the_item_unsettled_and_drops_the_stream() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(&["invalid answer", r#"{"q":0.2}"#, r#"{"q":0.8}"#], false)?;
    let gates = harness.provider.queue_gates(&[true, true, false]);
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let ack = acknowledgment(invocation.start_background(&harness.tool, Operation::Batch,
            json!({"states":["first","second"],"questions":questions(),"concurrency":1,"retries":2}))
            .await.map_err(bridge_error)?)?;
        started(&harness, 3).await?;
        let running = batch_observation(&harness, &ack, 1, 1).await?;
        let partial = partial_details(&running)?;
        assert_eq!(partial["failed"], 0); // ubs:ignore -- An intermediate failed attempt is not a settled failed item.
        assert_eq!(partial["items"][0]["status"], "running"); // ubs:ignore -- The retried item remains unavailable to drains.
        assert_eq!(partial["judgments"]["attempts"][0]["status"], "failed"); // ubs:ignore -- The failed attempt remains inspectable even while its item is running.
        let cancelled = crate::jobs::cancel_async(&harness.host.owner, ack["jobId"].as_str().ok_or_else(stopped)?).await?;
        assert_eq!(cancelled.status, "killed"); // ubs:ignore -- The owner cancels the original job including every retry.
        assert_eq!(partial_details(&cancelled)?, partial); // ubs:ignore -- Cancellation preserves exactly the last screened incomplete observation.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 3); // ubs:ignore -- Cancellation cannot dispatch the unused final retry.
        assert_eq!(gates[2].dropped.load(Ordering::Acquire), 1); // ubs:ignore -- Native cancellation retires the retried stream before returning.
        Ok(())
    })
}

#[test]
fn cancellation_drops_active_streams_and_never_dispatches_queued_items() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(&[], false)?;
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let ack = acknowledgment(
            invocation
                .start_background(
                    &harness.tool,
                    Operation::Batch,
                    json!({"states":["first","second","third","fourth"],"questions":questions(),"concurrency":2}),
                )
                .await
                .map_err(bridge_error)?,
        )?;
        started(&harness, 2).await?;
        let id = ack
            .get("jobId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::tool("fixture", "missing id"))?;
        let cancelled = crate::jobs::cancel_async(&harness.host.owner, id).await?;
        assert_eq!(cancelled.status, "killed"); // ubs:ignore -- Cancellation must settle the native carrier as killed.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 2); // ubs:ignore -- Queued requests must not start after cancellation.
        assert_eq!(harness.provider.gate.dropped.load(Ordering::Acquire), 2); // ubs:ignore -- Both retained provider streams must be dropped before settled cancellation.
        Ok(())
    })
}

#[test]
fn cancellation_retains_finished_items_errors_and_pending_identity() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(
        &[r#"{"q":0.9}"#, "invalid model result", r#"{"q":0.1}"#],
        false,
    )?;
    let gates = harness.provider.queue_gates(&[true, true, false]);
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let ack = acknowledgment(invocation.start_background(&harness.tool, Operation::Batch, json!({
            "states":["first","second","third","fourth"], "questions":questions(), "concurrency":1,
        })).await.map_err(bridge_error)?)?;
        let running = batch_observation(&harness, &ack, 2, 1).await?;
        started(&harness, 3).await?;
        let partial = partial_details(&running)?;
        assert_eq!(running.result.as_ref().and_then(|result| result.details.as_ref()).and_then(|details| details.get("schema")), Some(&json!("pi.host_job.progress.v1"))); // ubs:ignore -- Running observations use the explicit nonterminal native wrapper.
        assert_eq!(partial["schema"], PROGRESS_SCHEMA); // ubs:ignore -- Typed progress has an independent schema, never the final result schema.
        assert_eq!(partial["complete"], false); // ubs:ignore -- An observation with pending work cannot claim a completed batch.
        assert_eq!((partial["total"].as_u64(), partial["succeeded"].as_u64(), partial["failed"].as_u64(), partial["queued"].as_u64()), (Some(4), Some(1), Some(1), Some(1))); // ubs:ignore -- Failed items count as finished without converting pending items into results.
        assert_eq!(partial.pointer("/items/0/verdict/answers/q/bool"), Some(&json!(0.9))); // ubs:ignore -- A completed peer's typed verdict must be available before the batch settles.
        assert_eq!(partial.pointer("/items/1/status"), Some(&json!("failed"))); // ubs:ignore -- Per-item failures remain inspectable alongside successful peers.
        assert!(partial.pointer("/items/1/error").and_then(Value::as_str).is_some_and(|error| error.contains("PI_JUDGE_RESPONSE"))); // ubs:ignore -- Failed progress exposes only the native sanitized error.
        for (index, status) in [(2, "running"), (3, "queued")] {
            let item = &partial["items"][index];
            assert_eq!(item["key"], index); // ubs:ignore -- Pending items retain their original typed array identity.
            assert_eq!(item["status"], status); // ubs:ignore -- Running and undispatched work remain distinguishable.
            assert!(item.get("verdict").is_none() && item.get("error").is_none()); // ubs:ignore -- Pending rows cannot fabricate completed or failed evidence.
        }
        assert_eq!(partial.pointer("/judgments/attempts").and_then(Value::as_array).map(Vec::len), Some(4)); // ubs:ignore -- Every reserved attempt has a native receipt, including pending items.
        assert_eq!(partial.pointer("/judgments/attempts/0/usageKnown"), Some(&json!(true))); // ubs:ignore -- Finished usage survives interruption.
        assert_eq!(partial.pointer("/judgments/attempts/2/usageKnown"), Some(&json!(false))); // ubs:ignore -- A running provider's unknown usage must not be fabricated.
        let id = ack["jobId"].as_str().ok_or_else(stopped)?;
        let cancelled = crate::jobs::cancel_async(&harness.host.owner, id).await?;
        assert_eq!(cancelled.status, "killed"); // ubs:ignore -- Cancellation remains authoritative despite retained peer results.
        assert!(cancelled.result.as_ref().is_some_and(|result| result.is_error)); // ubs:ignore -- An interrupted partial is a terminal error, never a successful aggregate.
        assert_eq!(cancelled.result.as_ref().and_then(|result| result.details.as_ref()).and_then(|details| details.get("schema")), Some(&json!("pi.host_job.interrupted.v1"))); // ubs:ignore -- Interrupted work has an explicit wrapper distinct from both running and final results.
        assert_eq!(partial_details(&cancelled)?, partial); // ubs:ignore -- Cancellation keeps exactly the last accepted observation without inventing results.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 3); // ubs:ignore -- The queued fourth request must never reach the provider after cancellation.
        assert_eq!(gates[2].dropped.load(Ordering::Acquire), 1); // ubs:ignore -- The active provider stream must be retired before cancelled completion is visible.
        assert!(!serde_json::to_string(&cancelled)?.contains("invalid model result")); // ubs:ignore -- Raw rejected provider text must not enter retained progress.
        Ok(())
    })
}

#[test]
fn original_timeout_retains_the_last_batch_observation_without_starting_queued_work() -> Result<()>
{
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(&[r#"{"q":0.25}"#, r#"{"q":0.75}"#], false)?;
    let gates = harness.provider.queue_gates(&[true, false]);
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let ack = acknowledgment(invocation.start_background(&harness.tool, Operation::Batch, json!({
            "states":["first","second","third"], "questions":questions(), "concurrency":1, "timeout_secs":2,
        })).await.map_err(bridge_error)?)?;
        let running = batch_observation(&harness, &ack, 1, 1).await?;
        started(&harness, 2).await?;
        let partial = partial_details(&running)?;
        let timed_out = harness.settle(&ack).await?;
        assert_eq!(timed_out.status, "timedOut"); // ubs:ignore -- The original absolute deadline must settle as native timeout even when the inner guard wins its poll.
        assert_eq!(partial_details(&timed_out)?, partial); // ubs:ignore -- Timeout retains the last screened evidence without treating pending work as failed judgments.
        assert_eq!(partial["complete"], false); // ubs:ignore -- Retained evidence from a timeout remains explicitly incomplete.
        assert_eq!(harness.provider.calls.load(Ordering::Acquire), 2); // ubs:ignore -- Observation and deadline expiry cannot dispatch the queued third item.
        assert_eq!(gates[1].dropped.load(Ordering::Acquire), 1); // ubs:ignore -- Timeout drops the active stream before publishing the interrupted result.
        Ok(())
    })
}

#[test]
fn progress_and_final_privacy_failures_withdraw_earlier_batch_snapshots() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    for (pattern, second) in [
        (r"^0[.]875$", r#"{"q":0.875}"#),
        (r"^pi[.]eval[.]model-job[.]result[.]v1$", r#"{"q":0.75}"#),
    ] {
        let harness = Harness::new(&[r#"{"q":0.25}"#, second], false)?;
        let gates = harness.provider.queue_gates(&[true, false]);
        let snapshot = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(
            Some(&crate::secrets::SecretsSettings {
                mode: Some("obfuscate".into()),
                extra_patterns: Some(vec![pattern.into()]),
            }),
        ));
        runtime().block_on(async {
            let factory = || snapshot.clone();
            let mut invocation = harness.invocation(&factory)?;
            let ack = acknowledgment(
                invocation
                    .start_background(
                        &harness.tool,
                        Operation::Batch,
                        json!({
                            "states":["first","second"], "questions":questions(), "concurrency":1,
                        }),
                    )
                    .await
                    .map_err(bridge_error)?,
            )?;
            let running = batch_observation(&harness, &ack, 1, 1).await?;
            assert_eq!(
                partial_details(&running)?.pointer("/items/0/verdict/answers/q/bool"),
                Some(&json!(0.25))
            ); // ubs:ignore -- The fixture must first publish real completed evidence before testing its withdrawal.
            gates[1].release();
            let failed = harness.settle(&ack).await?;
            assert_eq!(failed.status, "failed"); // ubs:ignore -- Changing typed evidence or final native identity is a task failure.
            let encoded = serde_json::to_string(&failed)?;
            assert!(encoded.contains("PI_MODEL_JOB_RESULT_PRIVACY")); // ubs:ignore -- Privacy refusal must remain explicit after withdrawing prior progress.
            assert!(!encoded.contains("partialResult")); // ubs:ignore -- Neither progress-projection nor final-projection failure may retain an older partial.
            assert!(!encoded.contains("0.875")); // ubs:ignore -- A rejected secret-bearing probability cannot enter retained output.
            assert_eq!(harness.provider.calls.load(Ordering::Acquire), 2); // ubs:ignore -- Privacy refusal must not retry or fabricate additional model work.
            Ok::<(), Error>(())
        })?;
    }
    Ok(())
}

#[test]
fn privacy_never_turns_typed_success_into_invalid_retained_data() -> Result<()> {
    let _serial = crate::jobs::process_test_guard();
    let harness = Harness::new(&[r#"{"api_key":1234567890123456}"#], true)?;
    runtime().block_on(async {
        let factory = privacy;
        let mut invocation = harness.invocation(&factory)?;
        let ack = acknowledgment(invocation.start_background(&harness.tool, Operation::Completion, json!({
            "prompt":"return the requested integer", "schema":{"type":"object", "required":["api_key"],
                "properties":{"api_key":{"type":"integer"}}}
        })).await.map_err(bridge_error)?)?;
        let done = harness.settle(&ack).await?;
        assert_eq!(done.status, "failed"); // ubs:ignore -- Typed data changed by output privacy cannot remain a successful job.
        let encoded = serde_json::to_string(&done)?;
        assert!(encoded.contains("PI_MODEL_JOB_RESULT_PRIVACY")); // ubs:ignore -- Withholding typed output must be explicit.
        assert!(!encoded.contains("1234567890123456")); // ubs:ignore -- The provider's secret-bearing integer cannot reach artifacts or results.
        assert!(!encoded.contains("schemaValid")); // ubs:ignore -- No schema-valid claim may survive a type-changing privacy projection.
        Ok(())
    })
}

#[test]
fn timeout_controls_are_strict_and_unknown_bridge_names_do_not_dispatch() {
    for timeout in [
        Value::Null,
        json!(0),
        json!(601),
        json!(1.5),
        json!("60"),
        json!(true),
    ] {
        assert!(
            Request::parse(
                Operation::Completion,
                json!({"prompt":"x", "timeout_secs":timeout})
            )
            .is_err()
        ); // ubs:ignore -- Launch deadlines must never silently coerce invalid controls.
    }
    assert!(
        Request::parse(
            Operation::LegacyCompletion,
            json!({"prompt":"x", "images":[]})
        )
        .is_err()
    ); // ubs:ignore -- The raw completion bridge preserves its text-only contract.
    assert!(Request::parse(Operation::Completion, json!({"prompt":"x", "images":[]})).is_ok()); // ubs:ignore -- Typed completion jobs share the supported image descriptor path.
    assert_eq!(
        Operation::for_bridge("__pi_complete_start"),
        Some(Operation::LegacyCompletion)
    ); // ubs:ignore -- Raw completion callers keep the ordinary retained completion result.
    assert_eq!(
        Operation::for_bridge("__pi_model_complete_start"),
        Some(Operation::Completion)
    ); // ubs:ignore -- Typed handles must use their distinct bridge and retained operation identity.
    assert_eq!(Operation::for_bridge("__pi_complete"), None); // ubs:ignore -- Existing foreground calls must retain their original ownership semantics.
    assert_eq!(
        Operation::for_bridge("__pi_judge_batch_start"),
        Some(Operation::Batch)
    ); // ubs:ignore -- Only explicit start operations detach provider work.
}
