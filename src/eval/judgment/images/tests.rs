//! Native image bridge fixtures; no external files or live model requests.

use super::*;
use crate::eval::EvalTool;
use crate::judgment::Judge;
use crate::model::{AssistantMessage, Message, StopReason, StreamEvent, TextContent, UserContent};
use crate::provider::{Context, Provider, StreamOptions};
use crate::tools::{Tool, ToolInvocationHost, ToolOutput};
use futures::Stream;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
const OWNER: &str = "image-judgment-fixture";

fn image() -> ImageContent {
    ImageContent {
        data: PNG.into(),
        mime_type: "image/png".into(),
    }
}

fn embedded() -> Value {
    json!({"type":"image", "data":PNG, "mimeType":"image/png"})
}

fn questions() -> Value {
    json!({"q":{"type":"bool","instructions":"Does the image support the claim?"}})
}

fn single(images: Value) -> Value {
    json!({"state":{"claim":"CANARY-evidence"},"questions":questions(),"images":images})
}

fn batch(images: Value) -> Value {
    json!({"states":[{"index":0},{"index":1}],"questions":questions(),"images":images})
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .expect("native image fixture runtime") // ubs:ignore[rust.ownership.unwrap-expect] -- Tests require the real eval executor.
}

struct Fixture {
    contents: Mutex<Vec<UserContent>>,
    reads: Arc<AtomicUsize>,
    reads_before_dispatch: usize,
}

impl Fixture {
    fn new(reads_before_dispatch: usize) -> Arc<Self> {
        Arc::new(Self {
            contents: Mutex::new(Vec::new()),
            reads: Arc::new(AtomicUsize::new(0)),
            reads_before_dispatch,
        })
    }

    fn judge(self: &Arc<Self>, images: bool) -> Judge {
        Judge::new(
            self.clone(),
            StreamOptions {
                api_key: Some("HOST-ONLY-CREDENTIAL".into()),
                ..StreamOptions::default()
            },
        )
        .with_image_support(images)
    }

    fn contents(&self) -> Vec<UserContent> {
        self.contents // ubs:ignore[rust.async.lock-unwrap] -- A poisoned test recording lock invalidates the observed provider evidence.
            .lock()
            .expect("recorded image requests")
            .clone() // ubs:ignore[rust.ownership.unwrap-expect] -- Poisoned fixture state invalidates provider assertions.
    }
}

#[async_trait::async_trait]
impl Provider for Fixture {
    fn name(&self) -> &str {
        "fixture"
    }
    fn api(&self) -> &str {
        "fixture-stream"
    }
    fn model_id(&self) -> &str {
        "image-judge"
    }

    async fn stream(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        assert!(context.tools.is_empty()); // ubs:ignore[rust.panic.assert-macros] -- The controlled provider must fail if auxiliary judgments acquire tool rights.
        assert_eq!(context.messages.len(), 1);
        assert_eq!(options.api_key.as_deref(), Some("HOST-ONLY-CREDENTIAL"));
        assert_eq!(
            self.reads.load(Ordering::SeqCst),
            self.reads_before_dispatch,
            "all batch reads must finish before the first provider call"
        );
        let Message::User(message) = &context.messages[0] else {
            // ubs:ignore[rust.panic.direct-indexing] -- The preceding assertion establishes exactly one user message.
            return Err(Error::validation("fixture expected one user message"));
        };
        self.contents // ubs:ignore[rust.async.lock-unwrap] -- The controlled provider must fail rather than lose a captured request.
            .lock()
            .expect("recorded image requests")
            .push(message.content.clone()); // ubs:ignore[rust.ownership.unwrap-expect] -- Capture the exact native wire content, not a synthesized projection.
        Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Done {
            reason: StopReason::Stop,
            message: AssistantMessage {
                content: vec![ContentBlock::Text(TextContent::new(r#"{"q":0.8}"#))],
                stop_reason: StopReason::Stop,
                ..Default::default()
            },
        })])))
    }
}

#[derive(Clone, Copy)]
enum ReadMode {
    Good,
    Denied,
    ErrorOutput,
    Empty,
    Multiple,
    LateBad,
}

struct ReadHost {
    reads: Arc<AtomicUsize>,
    mode: ReadMode,
}

#[async_trait::async_trait]
impl ToolInvocationHost for ReadHost {
    async fn call_tool(
        &self,
        call: crate::model::ToolCall,
        expected_session: &str,
    ) -> Result<ToolOutput> {
        assert_eq!(expected_session, OWNER, "owner comes from the native cell");
        assert_eq!(
            call.name, "read",
            "images must use the enabled native reader"
        );
        assert!(call.arguments["path"].is_string()); // ubs:ignore[rust.panic.assert-macros] -- The fixture checks the actual enabled-reader argument contract.
        let index = self.reads.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, ReadMode::Denied) {
            return Err(Error::validation("PRIVATE-READ-DIAGNOSTIC"));
        }
        let mut content = vec![ContentBlock::Text(TextContent::new("PRIVATE-READ-NOTE"))];
        if !matches!(self.mode, ReadMode::Empty)
            && !(matches!(self.mode, ReadMode::LateBad) && index > 0)
        {
            content.push(ContentBlock::Image(image()));
        }
        if matches!(self.mode, ReadMode::Multiple) {
            content.push(ContentBlock::Image(image()));
        }
        Ok(ToolOutput {
            content,
            details: None,
            is_error: matches!(self.mode, ReadMode::ErrorOutput),
        })
    }
}

fn invocation<'a>(
    provider: &Arc<Fixture>,
    host: Option<&'a ReadHost>,
    supports_images: bool,
) -> Invocation<'a> {
    let mut invocation = Invocation::new(Some(provider.judge(supports_images)), &json!({}), None)
        .expect("valid fixture binding")
        .with_host(host.map(|host| host as &dyn ToolInvocationHost)); // ubs:ignore[rust.ownership.unwrap-expect] -- A controlled binding must construct before admission can be tested.
    if host.is_some() {
        invocation.bind_host_session(OWNER);
    }
    invocation
}

#[test]
fn descriptors_are_strict_and_rejected_before_reads_receipts_or_provider_calls() {
    let provider = Fixture::new(0);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::Good,
    };
    let mut invocation = invocation(&provider, Some(&host), true);
    let mut detail = embedded();
    detail["detail"] = json!("high"); // ubs:ignore[rust.panic.direct-indexing] -- embedded() constructs a known object; this mutation introduces one deliberate invalid field.
    let mut mime = embedded();
    mime["mimeType"] = json!("IMAGE/PNG"); // ubs:ignore[rust.panic.direct-indexing] -- Mutate the known fixture object to test refusal of MIME normalization.
    for images in [
        Value::Null,
        json!("screen.png"),
        json!(["https://example.test/image.png"]),
        json!(["data:image/png;base64,private"]),
        json!([" "]),
        json!(["x\n"]),
        json!([detail]),
        json!([mime]),
        json!([embedded(), embedded(), embedded(), embedded(), embedded()]),
    ] {
        assert!(
            // ubs:ignore[rust.panic.assert-macros] -- Every malformed descriptor must fail admission before side effects.
            runtime()
                .block_on(invocation.evaluate(Path::new("."), single(images)))
                .is_err()
        );
    }
    assert!(invocation.receipts.is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Malformed descriptors must fail before judgment reservation.
    assert!(invocation.tool_receipts.is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Malformed descriptors must acquire no read authority.
    assert!(provider.contents().is_empty()); // ubs:ignore[rust.panic.assert-macros] -- No rejected descriptor may reach the controlled provider.
    assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn unsupported_models_refuse_before_path_reads_and_do_not_silently_drop_images() {
    let provider = Fixture::new(0);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::Good,
    };
    let mut invocation = invocation(&provider, Some(&host), false);
    for request in [single(json!(["screen.png"])), single(json!([embedded()]))] {
        let error = runtime()
            .block_on(invocation.evaluate(Path::new("."), request))
            .expect_err("unsupported image binding must refuse");
        assert!(error.contains("PI_JUDGE_IMAGE_UNSUPPORTED")); // ubs:ignore[rust.panic.assert-macros] -- Capability rejection must be explicit rather than a text-only verdict.
    }
    assert!(provider.contents().is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Unsupported bindings must issue no provider request.
    assert!(invocation.tool_receipts.is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Capability checks precede image path reads.
    assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn path_loading_requires_the_current_host_and_its_captured_session() {
    let provider = Fixture::new(0);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::Good,
    };
    for with_host in [false, true] {
        let mut invocation =
            invocation(&provider, if with_host { Some(&host) } else { None }, true);
        invocation.host_session = None;
        let error = runtime()
            .block_on(invocation.evaluate(Path::new("."), single(json!(["screen.png"]))))
            .expect_err("an unowned path must not be read");
        assert!(error.contains("PI_JUDGE_IMAGE_HOST")); // ubs:ignore[rust.panic.assert-macros] -- A host without captured session authority must refuse path evidence.
    }
    assert!(provider.contents().is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Missing path authority must prevent provider admission.
    assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn native_read_errors_and_ambiguous_content_cannot_reach_a_provider() {
    for mode in [
        ReadMode::Denied,
        ReadMode::ErrorOutput,
        ReadMode::Empty,
        ReadMode::Multiple,
    ] {
        let provider = Fixture::new(1);
        let host = ReadHost {
            reads: Arc::clone(&provider.reads),
            mode,
        };
        let mut invocation = invocation(&provider, Some(&host), true);
        let error = runtime()
            .block_on(invocation.evaluate(Path::new("."), single(json!(["screen.png"]))))
            .expect_err("an unsuccessful or ambiguous read cannot supply evidence");
        assert!(error.contains("PI_JUDGE_IMAGE_READ")); // ubs:ignore[rust.panic.assert-macros] -- Reader failures must remain distinct from provider failures.
        assert!(!error.contains("PRIVATE")); // ubs:ignore[rust.panic.assert-macros] -- Reader diagnostics must not leak to the guest.
        assert!(provider.contents().is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Failed or ambiguous reads cannot supply partial evidence.
        assert_eq!(invocation.tool_receipts.len(), 1);
        assert_eq!(invocation.receipts[0]["status"], "failed");
        assert!(
            // ubs:ignore[rust.panic.assert-macros] -- Receipt metadata must omit the private path.
            !serde_json::to_string(&invocation.tool_receipts)
                .expect("receipt JSON")
                .contains("screen.png")
        ); // ubs:ignore[rust.ownership.unwrap-expect] -- Native receipts must remain payload-free.
    }
}

#[test]
fn host_read_budget_is_reserved_for_the_whole_batch_before_the_first_read() {
    let provider = Fixture::new(0);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::Good,
    };
    let mut invocation = invocation(&provider, Some(&host), true);
    invocation.tool_receipts = vec![json!({"tool":"fixture"}); MAX_TOOL_CALLS - 1];
    let error = runtime()
        .block_on(
            invocation.evaluate_batch(Path::new("."), batch(json!({"0":["a.png"],"1":["b.png"]}))),
        )
        .expect_err("two reads cannot consume a one-call remainder");
    assert!(error.contains("PI_JUDGE_IMAGE_CALL_LIMIT")); // ubs:ignore[rust.panic.assert-macros] -- Whole-batch read admission must account for the current cell budget.
    assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
    assert!(provider.contents().is_empty()); // ubs:ignore[rust.panic.assert-macros] -- A read-budget failure must not dispatch any batch item.
    assert_eq!(invocation.receipts.len(), 2);
    assert!(
        // ubs:ignore[rust.panic.assert-macros] -- Reserved slots must remain unstarted when image preparation cannot begin.
        invocation
            .receipts
            .iter()
            .all(|receipt| receipt["status"] == "not_started")
    );
}

#[test]
fn batches_validate_image_keys_and_all_descriptors_before_host_admission() {
    let provider = Fixture::new(0);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::Good,
    };
    let mut invocation = invocation(&provider, Some(&host), true);
    for images in [
        Value::Null,
        json!([]),
        json!({"01":[embedded()]}),
        json!({"2":[embedded()]}),
        json!({"0":["ok.png"],"1":[{"type":"image","mimeType":"image/png","data":"PRIVATE-invalid"}]}),
    ] {
        let error = runtime()
            .block_on(invocation.evaluate_batch(Path::new("."), batch(images)))
            .expect_err("invalid later descriptors must reject the whole batch");
        assert!(!error.contains("PRIVATE")); // ubs:ignore[rust.panic.assert-macros] -- Malformed inline payloads must not be echoed in errors.
    }
    assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
    assert!(provider.contents().is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Invalid image map keys and later descriptors reject the full batch.
    assert!(invocation.receipts.is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Syntactically invalid batches consume no judgment slots.
}

#[test]
fn batch_encoded_image_budget_is_independent_from_text_and_rejects_before_reservation() {
    use base64::Engine as _;

    // A bounded GIF envelope exercises byte accounting, not raster validity.
    let mut bytes = vec![0; 3 * 1024 * 1024];
    bytes[..10].copy_from_slice(b"GIF89a\x01\x00\x01\x00"); // ubs:ignore[rust.panic.direct-indexing] -- The fixture above allocates 3 MiB before writing its ten-byte header.
    let end = bytes.len() - 1;
    bytes[end] = 0x3b; // ubs:ignore[rust.panic.direct-indexing] -- end is the final valid index of the nonempty fixed-size fixture.
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let image = json!({"type":"image","data":data,"mimeType":"image/gif"});
    let provider = Fixture::new(0);
    let mut accepted = invocation(&provider, None, true);
    let request =
        batch(json!({"0":[image.clone(),image.clone()],"1":[image.clone(),image.clone()]}));
    let reply = runtime()
        .block_on(accepted.evaluate_batch(Path::new("."), request))
        .expect("exactly 16 MiB of images must not consume the text allowance"); // ubs:ignore[rust.ownership.unwrap-expect] -- The boundary batch validates separate text and image budgets.
    assert!(reply.contains("pi.eval.judgment-batch.v1")); // ubs:ignore[rust.panic.assert-macros] -- Boundary-size images must produce an ordinary typed batch result.
    assert_eq!(provider.contents().len(), 2);

    let mut rejected = invocation(&provider, None, true);
    let request = json!({"states":[{},{},{}],"questions":questions(),"images":{
        "0":[image.clone(),image.clone()],"1":[image.clone(),image],"2":[embedded()]
    }});
    let error = runtime()
        .block_on(rejected.evaluate_batch(Path::new("."), request))
        .expect_err("even one extra image must exceed the aggregate batch allowance");
    assert!(error.contains("PI_JUDGE_IMAGE_LIMIT")); // ubs:ignore[rust.panic.assert-macros] -- Crossing the aggregate budget must produce an image limit refusal.
    assert!(rejected.receipts.is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Oversized inline batches fail before reserving any attempts.
    assert_eq!(
        provider.contents().len(),
        2,
        "oversized batch admits no provider requests"
    );
}

#[test]
fn a_late_unreadable_batch_image_sends_no_earlier_model_request() {
    let provider = Fixture::new(2);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::LateBad,
    };
    let mut invocation = invocation(&provider, Some(&host), true);
    let error = runtime()
        .block_on(
            invocation.evaluate_batch(Path::new("."), batch(json!({"0":["a.png"],"1":["b.png"]}))),
        )
        .expect_err("all images must be ready before provider dispatch");
    assert!(error.contains("PI_JUDGE_IMAGE_READ")); // ubs:ignore[rust.panic.assert-macros] -- A later missing image must abort batch preparation.
    assert_eq!(provider.reads.load(Ordering::SeqCst), 2);
    assert!(provider.contents().is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Earlier readable items must not be sent before all reads finish.
    assert_eq!(invocation.tool_receipts.len(), 2);
    assert!(
        // ubs:ignore[rust.panic.assert-macros] -- Every reserved item must disclose the failed image-preparation admission.
        invocation
            .receipts
            .iter()
            .all(|receipt| receipt["status"] == "not_started"
                && receipt["admissionError"].is_string())
    );
}

#[test]
fn ordered_images_use_native_blocks_and_never_enter_text_projection_or_receipts() {
    let provider = Fixture::new(1);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::Good,
    };
    let protect = |parts: &[&str]| -> Result<Vec<String>> {
        assert!(parts.iter().all(
            |part| !part.contains(PNG) // ubs:ignore[rust.panic.assert-macros] -- The privacy fixture must reject image payloads and read metadata entering text projection.
            && !part.contains("screen.png")
            && !part.contains("PRIVATE-READ-NOTE")
        ));
        Ok(parts
            .iter()
            .map(|part| part.replace("CANARY-evidence", "screened-evidence"))
            .collect())
    };
    let mut invocation = invocation(&provider, Some(&host), true);
    invocation.protect = Some(&protect);
    let reply = runtime()
        .block_on(invocation.evaluate(Path::new("."), single(json!([embedded(), "screen.png"]))))
        .expect("valid image evidence should reach the provider"); // ubs:ignore[rust.ownership.unwrap-expect] -- This fixture exercises successful policy-checked image delivery.
    assert!(reply.contains("pi.judgment.v1")); // ubs:ignore[rust.panic.assert-macros] -- Image transport preserves the typed verdict schema.
    let contents = provider.contents();
    let UserContent::Blocks(blocks) = &contents[0] else {
        panic!("expected native image blocks"); // ubs:ignore[rust.ownership.panic-macro] -- Text-encoded image bytes would not provide visual evidence.
    };
    assert_eq!(blocks.len(), 3);
    assert!(
        // ubs:ignore[rust.panic.assert-macros] -- Provider text must use the live privacy projection.
        matches!(&blocks[0], ContentBlock::Text(text) if text.text.contains("screened-evidence") && !text.text.contains("CANARY"))
    );
    for block in &blocks[1..] {
        assert!(
            // ubs:ignore[rust.panic.assert-macros] -- Native blocks must preserve exact image bytes and MIME.
            matches!(block, ContentBlock::Image(image) if image.data == PNG && image.mime_type == "image/png")
        );
    }
    let receipts = serde_json::to_string(&(&invocation.receipts, &invocation.tool_receipts))
        .expect("receipt JSON"); // ubs:ignore[rust.ownership.unwrap-expect] -- Inspect both host-owned receipt collections for payload leakage.
    assert!(
        // ubs:ignore[rust.panic.assert-macros] -- Host-owned receipts must not expose images, paths or credentials.
        !receipts.contains(PNG)
            && !receipts.contains("screen.png")
            && !receipts.contains("HOST-ONLY")
    );
}

#[test]
fn batch_images_finish_loading_before_dispatch_and_exact_object_keys_remain_literal() {
    let provider = Fixture::new(2);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::Good,
    };
    let mut invocation = invocation(&provider, Some(&host), true);
    let request = json!({"states":{"01":{},"a/b~0":{}},"questions":questions(),"images":{"01":["a.png"],"a/b~0":["b.png"]}});
    let reply = runtime()
        .block_on(invocation.evaluate_batch(Path::new("."), request))
        .expect("exact object keys may contain numeric-looking or path-like strings"); // ubs:ignore[rust.ownership.unwrap-expect] -- Keys are identities rather than path expressions.
    let reply: Value = serde_json::from_str(&reply).expect("native batch JSON"); // ubs:ignore[rust.ownership.unwrap-expect] -- Inspect ordered native batch outcomes after provider admission.
    assert_eq!(reply["allOk"], true);
    assert_eq!(provider.contents().len(), 2);
    assert_eq!(reply["items"][0]["key"], "01");
    assert_eq!(reply["items"][1]["key"], "a/b~0");
}

#[test]
fn cancelled_image_preparation_never_reads_or_sends_evidence() {
    let provider = Fixture::new(0);
    let host = ReadHost {
        reads: Arc::clone(&provider.reads),
        mode: ReadMode::Good,
    };
    let mut invocation = invocation(&provider, Some(&host), true);
    let owner = AgentCx::for_request();
    owner.set_cancel_requested(true);
    let error = runtime()
        .block_on(owner.with_current(invocation.evaluate(Path::new("."), single(json!(["a.png"])))))
        .expect_err("cancelled image preparation must not acquire authority");
    assert!(error.contains("PI_JUDGE_CANCELLED")); // ubs:ignore[rust.panic.assert-macros] -- Original owner cancellation must stop image preparation.
    assert_eq!(provider.reads.load(Ordering::SeqCst), 0);
    assert!(provider.contents().is_empty()); // ubs:ignore[rust.panic.assert-macros] -- Cancellation before preparation must send no evidence.
}

#[test]
fn both_native_eval_kernels_deliver_embedded_images_to_the_bound_judge() {
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
        let dir = tempfile::tempdir().expect("isolated eval workspace"); // ubs:ignore[rust.ownership.unwrap-expect] -- Each real interpreter needs isolated native state.
        let provider = Fixture::new(0);
        let tool = EvalTool::new(dir.path()).with_judge(provider.judge(true));
        let request = single(json!([embedded()]));
        let code = if kernel == "js" {
            format!("JSON.parse(await tool.call('__pi_judge', {request})).answers.q.bool")
        } else {
            let encoded = serde_json::to_string(&request.to_string()).expect("quoted fixture JSON"); // ubs:ignore[rust.ownership.unwrap-expect] -- Quote the same payload safely for Python source.
            format!(
                "import json\nreply = json.loads(await tool.call('__pi_judge', json.loads({encoded})))\nreply['answers']['q']['bool']"
            )
        };
        let output = runtime()
            .block_on(tool.execute("image-judge", json!({"kernel":kernel,"code":code}), None))
            .expect("native eval image result"); // ubs:ignore[rust.ownership.unwrap-expect] -- This integration exercises the actual QuickJS/Python host bridge.
        assert!(!output.is_error, "{output:?}"); // ubs:ignore[rust.panic.assert-macros] -- The real kernel bridge must complete the controlled image judgment.
        assert!(
            // ubs:ignore[rust.panic.assert-macros] -- Both native interpreters must preserve multimodal provider content.
            matches!(&provider.contents()[0], UserContent::Blocks(blocks) if blocks.len() == 2)
        );
        assert_eq!(
            output.details.expect("native result metadata")["judgments"]["attempts"][0]["status"],
            "completed"
        ); // ubs:ignore[rust.ownership.unwrap-expect] -- Success must still include the host-owned judgment receipt.
    }
}
