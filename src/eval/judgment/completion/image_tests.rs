//! Multimodal completion tests through native admission and both eval kernels.
//! The controlled Provider is a transport boundary, not live vision evidence.

use super::*;
use crate::agent_cx::AgentCx;
use crate::error::Result as NativeResult;
use crate::eval::EvalTool;
use crate::judgment::Judge;
use crate::model::{AssistantMessage, ContentBlock, ImageContent, Message, StopReason, StreamEvent,
    TextContent, ToolCall, Usage, UserContent};
use crate::provider::{Context, Provider, StreamOptions};
use crate::tools::{Tool, ToolInvocationHost, ToolOutput};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
const OWNER: &str = "completion-image-owner";

fn image() -> ImageContent { ImageContent { data: PNG.into(), mime_type: "image/png".into() } }
fn embedded() -> Value { json!({"type":"image","mimeType":"image/png","data":PNG}) }
fn request() -> Value {
    json!({"prompt":"Extract the diagram label", "images":[embedded()], "schema":{
        "type":"object","required":["label"],"additionalProperties":false,
        "properties":{"label":{"type":"string"}}
    }})
}

struct Fixture {
    requests: Mutex<Vec<UserContent>>,
    answer: String,
    pending: bool,
    dropped: Arc<AtomicBool>,
}

impl Fixture {
    fn new(answer: &str) -> Arc<Self> {
        Arc::new(Self { requests: Mutex::new(Vec::new()), answer: answer.into(),
            pending: false, dropped: Arc::new(AtomicBool::new(false)) })
    }

    fn judge(self: &Arc<Self>, vision: bool) -> Judge {
        Judge::new(self.clone(), StreamOptions {
            api_key: Some("HOST-CREDENTIAL".into()), max_tokens: Some(77), ..StreamOptions::default()
        }).with_image_support(vision)
    }
}

struct Pending(Arc<AtomicBool>);
impl futures::Stream for Pending {
    type Item = NativeResult<StreamEvent>;
    fn poll_next(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> { Poll::Pending }
}
impl Drop for Pending { fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); } }

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Provider for Fixture {
    fn name(&self) -> &str { "completion-image-fixture" }
    fn api(&self) -> &str { "controlled" }
    fn model_id(&self) -> &str { "vision" }
    async fn stream(&self, context: &Context<'_>, options: &StreamOptions)
        -> NativeResult<Pin<Box<dyn futures::Stream<Item = NativeResult<StreamEvent>> + Send>>>
    {
        assert!(context.tools.is_empty());
        assert_eq!(context.messages.len(), 1);
        assert_eq!(options.api_key.as_deref(), Some("HOST-CREDENTIAL"));
        assert_eq!(options.max_tokens, Some(77));
        let Message::User(user) = &context.messages[0] else { panic!("user evidence"); };
        self.requests.lock().unwrap().push(user.content.clone());
        if self.pending { return Ok(Box::pin(Pending(Arc::clone(&self.dropped)))); }
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason: StopReason::Stop,
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(&self.answer))],
                stop_reason: StopReason::Stop,
                usage: Usage { input: 20, output: 5, total_tokens: 25, ..Usage::default() },
                ..AssistantMessage::default()
            }
        })])))
    }
}

struct Host { calls: AtomicUsize, deny: bool, bad_second: bool }
#[async_trait::async_trait]
impl ToolInvocationHost for Host {
    async fn call_tool(&self, call: ToolCall, owner: &str) -> NativeResult<ToolOutput> {
        assert_eq!(owner, OWNER);
        assert_eq!(call.name, "read");
        assert!(call.arguments["path"].is_string());
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.deny { return Err(Error::validation("PRIVATE-HOST-DIAGNOSTIC")); }
        let mut content = vec![ContentBlock::Text(TextContent::new("PRIVATE-READ-NOTE"))];
        if !self.bad_second || index == 0 { content.push(ContentBlock::Image(image())); }
        Ok(ToolOutput { content, details: None, is_error: false })
    }
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8).build().unwrap()
}
fn invocation<'a>(provider: &Arc<Fixture>, host: Option<&'a Host>, vision: bool) -> Invocation<'a> {
    let mut call = Invocation::new(Some(provider.judge(vision)), &json!({}), None).unwrap()
        .with_host(host.map(|host| host as &dyn ToolInvocationHost));
    if host.is_some() { call.bind_host_session(OWNER); }
    call
}
fn event(case: &str) { eprintln!("{}", json!({"schema":"pi.completion.images.test.v1","case":case})); }

#[test]
fn native_completion_preserves_images_and_schema_validated_data() {
    event("native_schema_completion");
    let provider = Fixture::new(r#"{"label":"diagram"}"#);
    let mut value = request();
    value.as_object_mut().unwrap().remove("images");
    let request = CompletionRequest::parse(value).unwrap().with_images(vec![image(), image()]).unwrap();
    let reply = runtime().block_on(provider.judge(true).complete(request)).unwrap();
    assert_eq!(reply["data"]["label"], "diagram");
    assert_eq!(reply["schemaValid"], true);
    assert_eq!(reply["imageCount"], 2);
    assert_eq!(reply["usage"]["totalTokens"], 25);
    let requests = provider.requests.lock().unwrap();
    let UserContent::Blocks(blocks) = &requests[0] else { panic!("multimodal context"); };
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[0], ContentBlock::Text(text) if text.text.contains("schema") && !text.text.contains(PNG)));
    for block in &blocks[1..] { assert!(matches!(block, ContentBlock::Image(image) if image.data == PNG)); }
    assert!(!reply.to_string().contains(PNG));
}

#[test]
fn all_descriptor_and_schema_failures_precede_reads_and_attempt_reservation() {
    event("whole_request_preflight");
    let provider = Fixture::new("unused");
    let host = Host { calls: AtomicUsize::new(0), deny: false, bad_second: false };
    let mut call = invocation(&provider, Some(&host), true);
    runtime().block_on(async {
        let mut bad_schema = request();
        bad_schema["schema"] = json!({"type":"not-a-json-type"});
        bad_schema["images"] = json!(["valid.png"]);
        assert!(call.complete(Path::new("."), bad_schema).await.unwrap_err().contains("PI_COMPLETION_SCHEMA"));
        for bad in [Value::Null, json!(["ok.png", {"type":"image","data":"PRIVATE-CANARY","mimeType":"image/png"}]),
            json!(["https://example.invalid/image.png"]), json!(vec![embedded(); 5])]
        {
            let error = call.complete(Path::new("."), json!({"prompt":"read diagram","images":bad})).await.unwrap_err();
            assert!(error.contains("PI_COMPLETION_IMAGE_"));
            assert!(!error.contains("PRIVATE-CANARY"));
        }
        assert!(call.receipts.is_empty());
    });
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn unsupported_bindings_and_missing_host_never_read_paths_or_dispatch() {
    event("capability_before_io");
    let provider = Fixture::new("unused");
    let host = Host { calls: AtomicUsize::new(0), deny: false, bad_second: false };
    runtime().block_on(async {
        let mut unsupported = invocation(&provider, Some(&host), false);
        let error = unsupported.complete(Path::new("."), json!({"prompt":"inspect","images":["screen.png"]})).await.unwrap_err();
        assert!(error.contains("PI_COMPLETION_IMAGE_UNSUPPORTED"));
        assert_eq!(unsupported.receipts[0]["status"], "failed");
        let mut standalone = invocation(&provider, None, true);
        let error = standalone.complete(Path::new("."), json!({"prompt":"inspect","images":["screen.png"]})).await.unwrap_err();
        assert!(error.contains("PI_COMPLETION_IMAGE_HOST"));
    });
    assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    assert!(provider.requests.lock().unwrap().is_empty());
}

#[test]
fn host_reads_share_native_receipts_and_preserve_text_privacy() {
    event("host_and_privacy");
    let provider = Fixture::new("answer");
    let host = Host { calls: AtomicUsize::new(0), deny: false, bad_second: false };
    let protect = |parts: &[&str]| -> NativeResult<Vec<String>> {
        assert!(parts.iter().all(|part| !part.contains(PNG) && !part.contains("screen.png") && !part.contains("PRIVATE-READ-NOTE")));
        Ok(parts.iter().map(|part| part.replace("TEXT-CANARY", "screened")).collect())
    };
    let mut call = invocation(&provider, Some(&host), true);
    call.protect = Some(&protect);
    let reply = runtime().block_on(call.complete(Path::new("."), json!({"prompt":"TEXT-CANARY","images":[embedded(),"screen.png"]}))).unwrap();
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert_eq!(call.tool_receipts.len(), 1);
    assert_eq!(call.tool_receipts[0]["tool"], "read");
    assert_eq!(call.receipts[0]["status"], "completed");
    let requests = provider.requests.lock().unwrap();
    let UserContent::Blocks(blocks) = &requests[0] else { panic!("native images"); };
    assert_eq!(blocks.len(), 3);
    assert!(matches!(&blocks[0], ContentBlock::Text(text) if text.text == "screened"));
    assert!(!reply.contains(PNG));
    let receipt = serde_json::to_string(&(&call.receipts, &call.tool_receipts)).unwrap();
    assert!(!receipt.contains(PNG) && !receipt.contains("screen.png") && !receipt.contains("HOST-CREDENTIAL"));
}

#[test]
fn late_image_read_failure_never_sends_a_partial_completion() {
    event("read_failure");
    for deny in [false, true] {
        let provider = Fixture::new("unused");
        let host = Host { calls: AtomicUsize::new(0), deny, bad_second: true };
        let mut call = invocation(&provider, Some(&host), true);
        let error = runtime().block_on(call.complete(Path::new("."), json!({"prompt":"inspect both","images":["a.png","b.png"]}))).unwrap_err();
        assert!(error.contains("PI_COMPLETION_IMAGE_READ"));
        assert!(!error.contains("PRIVATE"));
        assert!(provider.requests.lock().unwrap().is_empty());
        assert_eq!(host.calls.load(Ordering::SeqCst), if deny { 1 } else { 2 });
        assert_eq!(call.receipts[0]["status"], "failed");
        assert_eq!(call.receipts[0]["usageKnown"], false);
    }
}

#[test]
fn image_schema_failure_is_not_repaired_and_stays_in_native_receipts() {
    event("schema_refusal");
    let provider = Fixture::new(r#"{"label":4}"#);
    let mut call = invocation(&provider, None, true);
    let error = runtime().block_on(call.complete(Path::new("."), request())).unwrap_err();
    assert!(error.contains("PI_COMPLETION_RESPONSE"));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert_eq!(call.receipts[0]["status"], "failed");
    assert_eq!(call.receipts[0]["usage"], Value::Null);
}

#[test]
fn image_completion_keeps_owner_cancellation_and_drops_pending_streams() {
    event("owned_cancellation");
    let provider = Arc::new(Fixture { requests: Mutex::new(Vec::new()), answer: String::new(),
        pending: true, dropped: Arc::new(AtomicBool::new(false)) });
    let host = Host { calls: AtomicUsize::new(0), deny: false, bad_second: false };
    runtime().block_on(async {
        let owner = AgentCx::for_request();
        owner.set_cancel_requested(true);
        let mut cancelled = invocation(&provider, Some(&host), true);
        let error = owner.with_current(cancelled.complete(Path::new("."), json!({"prompt":"inspect","images":["a.png"]}))).await.unwrap_err();
        assert!(error.contains("PI_COMPLETION_CANCELLED"));
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
        let mut active = invocation(&provider, None, true);
        let mut future = Box::pin(active.complete(Path::new("."), request()));
        assert!(futures::poll!(future.as_mut()).is_pending());
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        drop(future);
        assert!(provider.dropped.load(Ordering::SeqCst));
        assert_eq!(active.receipts[0]["status"], "incomplete");
    });
}

#[test]
fn public_eval_kernels_deliver_images_with_schema_data_and_shared_receipts() {
    for kernel in ["js", "python"] {
        if kernel == "python" && !std::process::Command::new("python3").arg("--version")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().is_ok_and(|status| status.success())
        {
            assert!(std::env::var_os("PI_EVAL_REQUIRE_PYTHON").is_none(), "Python lane is required");
            event("python_explicit_skip");
            continue;
        }
        event(kernel);
        let provider = Fixture::new(r#"{"label":"diagram"}"#);
        let dir = tempfile::tempdir().unwrap();
        let tool = EvalTool::new(dir.path()).with_judge(provider.judge(true));
        let input = request();
        let code = if kernel == "js" {
            format!("JSON.parse(await tool.call('__pi_complete', {input})).data.label")
        } else {
            let literal = serde_json::to_string(&input.to_string()).unwrap();
            format!("import json\nreply = json.loads(await tool.call('__pi_complete', json.loads({literal})))\nreply['data']['label']")
        };
        let output = runtime().block_on(tool.execute("completion-image", json!({"kernel":kernel,"code":code}), None)).unwrap();
        assert!(!output.is_error, "{output:?}");
        let details = output.details.unwrap();
        assert_eq!(details["judgments"]["attempts"][0]["operation"], "completion");
        assert_eq!(details["judgments"]["attempts"][0]["status"], "completed");
        assert_eq!(details["judgments"]["attempts"][0]["usage"]["input"], 20);
        assert!(!details.to_string().contains(PNG));
        let calls = provider.requests.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(matches!(&calls[0], UserContent::Blocks(blocks) if blocks.len() == 2));
    }
}
