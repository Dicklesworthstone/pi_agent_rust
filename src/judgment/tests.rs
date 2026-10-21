//! Unit-level provider-stream fixtures, not live model or DSR acceptance.
use super::*;
use crate::model::{AssistantMessage, ContentBlock, StopReason, TextContent, ToolCall};
use futures::Stream;
use std::pin::Pin;
use std::sync::Mutex;

fn request() -> JudgmentRequest {
    JudgmentRequest::parse(json!({
        "state":{"change":"adds a regression test"},
        "questions":{
            "safe":{"type":"bool","instructions":"Is this safe?"},
            "kind":{"type":"choice","instructions":"Classify the change", "criteria":{"feature":null,"test":"tests only"}},
            "impact":{"type":"score","instructions":"Rate impact","criteria":["low","medium","high"]}
        }
    })).unwrap()
}
const ANSWER: &str = r#"{"safe":0.9,"kind":{"feature":0.25,"test":0.75},"impact":[0.25,0.5,0.25]}"#;

#[test]
fn derives_typed_answers_from_complete_probabilities() {
    let answers = request().answers(ANSWER).unwrap();
    assert_eq!(answers["safe"], json!({"type":"bool","bool":0.9}));
    assert_eq!(answers["kind"]["choice"], "test");
    assert_eq!(answers["kind"]["probabilities"]["feature"], 0.25);
    assert!((answers["kind"]["confidence"].as_f64().unwrap() - 0.188_721_875_54).abs() < 0.000_001);
    assert_eq!(answers["impact"]["score"], 1.0);
    assert_eq!(answers["impact"]["probabilities"]["1"], 0.5);
}

#[test]
fn rejects_missing_extra_and_duplicate_answers() {
    for answer in [
        r#"{"safe":0.9}"#,
        r#"{"safe":0.9,"kind":{"feature":0.25,"test":0.75},"impact":[0.25,0.5,0.25],"extra":0}"#,
        r#"{"safe":0.1,"safe":0.9,"kind":{"feature":0.25,"test":0.75},"impact":[0.25,0.5,0.25]}"#,
        r#"{"safe":0.9,"kind":{"feature":0.25,"test":0.75,"test":0.75},"impact":[0.25,0.5,0.25]}"#,
        "```json\n{}\n```", "{} trailing", "null",
    ] {
        assert!(request().answers(answer).is_err(), "accepted {answer}");
    }
}

#[test]
fn refuses_probability_coercion_and_partial_distributions() {
    let base: Value = serde_json::from_str(ANSWER).unwrap();
    for probability in [json!(true), json!("0.5"), json!(-0.1), json!(1.1), Value::Null] {
        let mut answer = base.clone();
        answer["safe"] = probability;
        assert!(request().answers(&answer.to_string()).is_err());
    }
    for choice in [json!({"feature":0.1,"test":0.1}), json!({"feature":0.5,"other":0.5}), json!({"feature":1})] {
        let mut answer = base.clone();
        answer["kind"] = choice;
        assert!(request().answers(&answer.to_string()).is_err());
    }
    for score in [json!([0.5,0.5]), json!([1,1,1]), json!({"0":0.5,"1":0.5,"2":0})] {
        let mut answer = base.clone();
        answer["impact"] = score;
        assert!(request().answers(&answer.to_string()).is_err());
    }
}

#[test]
fn stable_ties_and_degenerate_distributions() {
    let answers = request().answers(r#"{"safe":0,"kind":{"test":0.5,"feature":0.5},"impact":[0,0,1]}"#).unwrap();
    assert_eq!(answers["kind"]["choice"], "feature");
    assert_eq!(answers["kind"]["confidence"], 0.0);
    assert_eq!(answers["impact"]["score"], 2.0);
    assert_eq!(answers["impact"]["confidence"], 1.0);
}

#[test]
fn preflight_rejects_unsupported_state_and_rubric_shapes() {
    for state in [Value::Null, json!(0), json!(true), json!(" ")] {
        assert!(JudgmentRequest::parse(json!({"state":state,"questions":{
            "q":{"type":"bool","instructions":"valid"}
        }})).is_err());
    }
    for question in [
        json!({"type":"bool","instructions":""}),
        json!({"type":"bool","instructions":"valid","criteria":{"maybe":"unknown"}}),
        json!({"type":"choice","instructions":"valid","criteria":{"only":null}}),
        json!({"type":"score","instructions":"valid","criteria":["one"]}),
        json!({"type":"score","instructions":"valid","criteria":["one",2]}),
        json!({"type":"bool","instructions":"valid","model":"other"}),
    ] {
        assert!(JudgmentRequest::parse(json!({"state":{},"questions":{"q":question}})).is_err());
    }
    assert!(JudgmentRequest::parse(json!({"state":{},"questions":{}})).is_err());
    assert!(JudgmentRequest::parse(json!({"state":{},"questions":{},"images":[]})).is_err());
}

#[test]
fn preflight_bounds_utf8_shape_and_text_without_echoing_input() {
    let huge = "CANARY秘密".repeat(32 * 1024);
    let error = JudgmentRequest::parse(json!({"state":huge,"questions":{}})).unwrap_err();
    assert!(!error.to_string().contains("CANARY"));
    let mut nested = json!({});
    for _ in 0..33 { nested = json!([nested]); }
    assert!(JudgmentRequest::parse(json!({"state":nested,"questions":{
        "q":{"type":"bool","instructions":"valid"}
    }})).is_err());
    assert!(JudgmentRequest::parse(json!({"state":{},"questions":{
        "q":{"type":"bool","instructions":"x".repeat(4097)}
    }})).is_err());
}

#[test]
fn redaction_may_change_state_but_not_answer_identity() {
    let original = request();
    let mut value: Value = serde_json::from_str(&original.encoded().unwrap()).unwrap();
    value["state"] = json!({"redacted":true});
    assert!(original.same_answer_keys(&JudgmentRequest::parse(value.clone()).unwrap()));
    value["questions"]["safe"]["type"] = json!("choice");
    value["questions"]["safe"]["criteria"] = json!({"yes":null,"no":null});
    assert!(!original.same_answer_keys(&JudgmentRequest::parse(value).unwrap()));
}

struct FixtureProvider {
    events: Mutex<Option<Vec<StreamEvent>>>,
    prompts: Mutex<Vec<String>>,
    contents: Mutex<Vec<UserContent>>,
    tokens: Mutex<Vec<Option<u32>>>,
}
impl FixtureProvider {
    fn new(events: Vec<StreamEvent>) -> Arc<Self> {
        Arc::new(Self { events: Mutex::new(Some(events)), prompts: Mutex::new(Vec::new()),
            contents: Mutex::new(Vec::new()), tokens: Mutex::new(Vec::new()) })
    }
}
#[async_trait::async_trait]
impl Provider for FixtureProvider {
    fn name(&self) -> &str { "fixture" }
    fn api(&self) -> &str { "fixture-stream" }
    fn model_id(&self) -> &str { "typed-judge" }
    async fn stream(&self, context: &Context<'_>, options: &StreamOptions)
        -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>> {
        assert!(context.tools.is_empty());
        assert_eq!(context.messages.len(), 1);
        let Message::User(message) = &context.messages[0] else { panic!("expected user state"); };
        let text = match &message.content {
            UserContent::Text(text) => text,
            UserContent::Blocks(blocks) => match blocks.first() {
                Some(ContentBlock::Text(text)) => &text.text,
                _ => panic!("expected screened text before image evidence"), // ubs:ignore[rust.ownership.panic-macro] -- This controlled provider fixture must observe the documented multimodal wire shape.
            },
        };
        self.prompts.lock().unwrap().push(text.clone());
        self.contents.lock().unwrap().push(message.content.clone()); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- The test fixture must fail on a poisoned recording lock rather than lose the provider-bound evidence.
        self.tokens.lock().unwrap().push(options.max_tokens);
        let events = self.events.lock().unwrap().take().expect("provider was retried");
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}
fn done(answer: &str, reason: StopReason) -> StreamEvent {
    StreamEvent::Done { reason, message: AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(answer))],
        stop_reason: reason,
        ..Default::default()
    }}
}
fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new().build().unwrap()
}

#[test]
fn complete_terminal_answer_is_authoritative_and_has_provenance() {
    let provider = FixtureProvider::new(vec![
        StreamEvent::TextDelta {content_index:0,delta:"incomplete preview".into()},
        done(ANSWER, StopReason::Stop),
    ]);
    let judge = Judge::new(provider.clone(), StreamOptions { max_tokens: Some(90_000), ..Default::default() });
    let result = runtime().block_on(judge.evaluate(request())).unwrap();
    assert_eq!(result["model"], "fixture/typed-judge");
    assert_eq!(result["api"], "fixture-stream");
    assert!(result["usage"].is_object());
    assert_eq!(result["answers"]["safe"]["bool"], 0.9);
    assert_eq!(*provider.tokens.lock().unwrap(), vec![Some(4096)]);
}

#[test]
fn failed_empty_and_unterminated_streams_have_no_verdict() {
    for events in [
        vec![done(ANSWER, StopReason::Error)],
        vec![done(ANSWER, StopReason::Length)],
        vec![done(ANSWER, StopReason::Refusal)],
        vec![done("", StopReason::Stop)],
        vec![StreamEvent::TextDelta { content_index:0, delta:ANSWER.into() }],
        vec![done("not JSON", StopReason::Stop)],
    ] {
        let provider = FixtureProvider::new(events);
        assert!(runtime().block_on(Judge::new(provider.clone(), StreamOptions::default()).evaluate(request())).is_err());
        assert_eq!(provider.prompts.lock().unwrap().len(), 1);
    }
}

#[test]
fn tool_calls_in_a_terminal_response_are_never_executed() {
    let mut message = AssistantMessage::default();
    message.content = vec![ContentBlock::ToolCall(ToolCall {
        id:"forbidden".into(), name:"bash".into(), arguments:json!({"command":"no"}), thought_signature:None,
    })];
    message.stop_reason = StopReason::Stop;
    let provider = FixtureProvider::new(vec![StreamEvent::Done { reason:StopReason::Stop, message }]);
    assert!(runtime().block_on(Judge::new(provider, StreamOptions::default()).evaluate(request())).is_err());
}

#[test]
fn invocation_privacy_blocks_before_provider_admission() {
    let provider = FixtureProvider::new(vec![done(ANSWER, StopReason::Stop)]);
    let judge = Judge::new(provider.clone(), StreamOptions::default());
    let protect = |_parts: &[&str]| -> Result<Vec<String>> { Err(failure("PI_SECRET_BLOCK")) };
    let error = runtime().block_on(judge.evaluate_protected(request(), Some(&protect))).unwrap_err();
    assert!(error.to_string().contains("PI_SECRET_BLOCK"));
    assert!(provider.prompts.lock().unwrap().is_empty());
}

#[test]
fn direct_calls_redact_credentials_before_the_provider() {
    let provider = FixtureProvider::new(vec![done(ANSWER, StopReason::Stop)]);
    let mut value: Value = serde_json::from_str(&request().encoded().unwrap()).unwrap();
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
    value["state"] = json!({"API_KEY":secret,"echo":secret});
    let request = JudgmentRequest::parse(value).unwrap();
    runtime().block_on(Judge::new(provider.clone(), StreamOptions::default()).evaluate(request)).unwrap();
    let prompts = provider.prompts.lock().unwrap();
    assert!(!prompts[0].contains(secret));
    assert!(prompts[0].contains("redacted"));
}

mod image_evidence {
    use super::*;
    use crate::model::ImageContent;
    use base64::Engine as _;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    fn image() -> ImageContent {
        ImageContent { data: PNG.to_string(), mime_type: "image/png".to_string() }
    }

    // An envelope fixture, deliberately not a claim that every payload is a
    // decodable raster. It exercises byte/canvas admission without decoding.
    fn gif(bytes: usize, width: u16, height: u16) -> ImageContent {
        let mut data = vec![0; bytes.max(14)];
        data[..6].copy_from_slice(b"GIF89a");
        data[6..8].copy_from_slice(&width.to_le_bytes());
        data[8..10].copy_from_slice(&height.to_le_bytes());
        let end = data.len() - 1;
        data[end] = 0x3b;
        ImageContent {
            data: base64::engine::general_purpose::STANDARD.encode(data),
            mime_type: "image/gif".to_string(),
        }
    }

    #[test]
    fn images_remain_separate_from_text_contract_and_native_blocks_preserve_order() {
        // This image alone exceeds the complete text privacy allowance, but
        // stays admissible as separately bounded opaque image evidence.
        let second = gif(crate::text_completion::MAX_INPUT_BYTES, 2, 3);
        let expected = [image(), second];
        let original = request();
        let text = original.encoded().unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- A validated fixture request must encode before attachment.
        let request = original.with_images(expected.to_vec()).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Controlled PNG/GIF envelopes must be admitted.
        assert!(request.has_images()); // ubs:ignore[rust.panic.assert-macros] -- The host must detect the image capability requirement.
        assert_eq!(request.image_bytes(), expected.iter().map(|image| image.data.len()).sum::<usize>()); // ubs:ignore[rust.panic.assert-macros] -- Batch charging uses the exact encoded attachment sizes.
        assert_eq!(request.encoded().unwrap(), text); // ubs:ignore[rust.panic.assert-macros,rust.panic.unwrap,rust.ownership.unwrap-expect] -- Attaching images must not enlarge or contaminate the text privacy envelope.
        let provider = FixtureProvider::new(vec![done(ANSWER, StopReason::Stop)]);
        let judge = Judge::new(provider.clone(), StreamOptions::default()).with_image_support(true);
        let verdict = runtime().block_on(judge.evaluate(request)).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- A terminal fixture verdict is required to inspect transmitted evidence.
        assert_eq!(verdict["answers"]["safe"]["bool"], 0.9); // ubs:ignore[rust.panic.assert-macros] -- Images preserve the existing typed verdict contract.
        let contents = provider.contents.lock().unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- This isolated fixture cannot continue without its captured provider request.
        let UserContent::Blocks(blocks) = &contents[0] else {
            panic!("images were not sent as native content blocks"); // ubs:ignore[rust.ownership.panic-macro] -- Serialized base64 in a text message would not provide vision evidence.
        };
        assert_eq!(blocks.len(), 3); // ubs:ignore[rust.panic.assert-macros] -- Exactly one screened text block precedes the two evidence images.
        assert!(matches!(&blocks[0], ContentBlock::Text(text) if !text.text.contains(PNG))); // ubs:ignore[rust.panic.assert-macros] -- Image payloads must never be rendered into prompt text.
        for (block, original) in blocks[1..].iter().zip(expected) {
            assert!(matches!(block, ContentBlock::Image(image) if image.data == original.data && image.mime_type == original.mime_type)); // ubs:ignore[rust.panic.assert-macros] -- Admission must preserve exact bytes, MIME and ordering without transcoding.
        }
    }

    #[test]
    fn image_support_is_explicit_and_revocation_refuses_before_provider_admission() {
        let provider = FixtureProvider::new(vec![done(ANSWER, StopReason::Stop)]);
        let judge = Judge::new(provider.clone(), StreamOptions::default());
        assert!(judge.check_image_support(false).is_ok()); // ubs:ignore[rust.panic.assert-macros] -- A text request never requires image capability.
        for binding in [judge.clone(), judge.with_image_support(true).with_image_support(false)] {
            let request = request().with_images(vec![image()]).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Valid image evidence isolates the capability rejection.
            let error = runtime().block_on(binding.evaluate(request)).unwrap_err(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- An unsupported binding must fail without silently dropping evidence.
            assert!(error.to_string().contains("PI_JUDGE_IMAGE_UNSUPPORTED")); // ubs:ignore[rust.panic.assert-macros] -- Callers need a named capability refusal rather than a text-only verdict.
        }
        assert!(provider.contents.lock().unwrap().is_empty()); // ubs:ignore[rust.panic.assert-macros,rust.panic.unwrap,rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- Unsupported requests must consume no provider call; poisoned evidence must fail the test.
    }

    #[test]
    fn empty_images_preserve_text_only_binding_and_wire_shape() {
        let request = request().with_images(vec![image()]).unwrap().with_images(Vec::new()).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Replacing attachments with an empty validated collection must remove image authority requirements.
        assert!(!request.has_images()); // ubs:ignore[rust.panic.assert-macros] -- Empty collections remain ordinary text judgments.
        assert_eq!(request.image_bytes(), 0); // ubs:ignore[rust.panic.assert-macros] -- Removed evidence must not keep charging the batch image budget.
        let provider = FixtureProvider::new(vec![done(ANSWER, StopReason::Stop)]);
        runtime().block_on(Judge::new(provider.clone(), StreamOptions::default()).evaluate(request)).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- The default text binding must remain usable after empty-image admission.
        assert!(matches!(&provider.contents.lock().unwrap()[0], UserContent::Text(_))); // ubs:ignore[rust.panic.assert-macros,rust.panic.unwrap,rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- No-image calls retain the existing wire representation; losing the capture must fail this assertion.
    }

    #[test]
    fn text_privacy_still_blocks_or_redacts_without_receiving_image_bytes() {
        let provider = FixtureProvider::new(vec![done(ANSWER, StopReason::Stop)]);
        let judge = Judge::new(provider.clone(), StreamOptions::default()).with_image_support(true);
        let protect = |parts: &[&str]| -> Result<Vec<String>> {
            assert!(parts.iter().all(|part| !part.contains(PNG))); // ubs:ignore[rust.panic.assert-macros] -- Pixel evidence is opaque and excluded from the text privacy scan.
            Err(failure("PI_SECRET_BLOCK"))
        };
        let blocked = request().with_images(vec![image()]).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Valid evidence ensures the privacy callback is the only refusal.
        let error = runtime().block_on(judge.evaluate_protected(blocked, Some(&protect))).unwrap_err(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Block mode must refuse before sending either text or images.
        assert!(error.to_string().contains("PI_SECRET_BLOCK")); // ubs:ignore[rust.panic.assert-macros] -- The existing text privacy refusal remains authoritative.
        assert!(provider.contents.lock().unwrap().is_empty()); // ubs:ignore[rust.panic.assert-macros,rust.panic.unwrap,rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- A blocked request cannot leak an image; poisoned capture state cannot be accepted as evidence of no call.
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
        let mut value: Value = serde_json::from_str(&request().encoded().unwrap()).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Controlled typed input is extended with a known credential-shaped fixture.
        value["state"] = json!({"API_KEY":secret,"echo":secret});
        let redacted = JudgmentRequest::parse(value).unwrap().with_images(vec![image()]).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Known-secret text and valid image evidence must reach the ordinary privacy projection.
        runtime().block_on(judge.evaluate(redacted)).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Redaction, unlike block mode, should preserve usable evidence.
        let prompts = provider.prompts.lock().unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect,rust.async.lock-unwrap] -- Inspect exactly what the provider received; poison must invalidate the test.
        assert!(!prompts[0].contains(secret)); // ubs:ignore[rust.panic.assert-macros] -- Adding image support cannot weaken existing credential redaction.
        assert!(prompts[0].contains("redacted")); // ubs:ignore[rust.panic.assert-macros] -- The admitted text must carry irreversible redaction.
    }

    #[test]
    fn image_admission_rejects_malformed_metadata_base64_and_unbounded_canvas() {
        let mut cases = vec![
            ImageContent { data: "CANARY-private-image-data".to_string(), mime_type: "image/png".to_string() },
            ImageContent { data: String::new(), mime_type: "image/png".to_string() },
            ImageContent { data: PNG.trim_end_matches('=').to_string(), mime_type: "image/png".to_string() },
            ImageContent { data: format!("{PNG}\n"), mime_type: "image/png".to_string() },
            ImageContent { data: PNG.replace("ggg==", "ggh=="), mime_type: "image/png".to_string() },
            gif(14, 0, 1), gif(14, 16_385, 1), gif(14, 4097, 4096),
        ];
        for mime in ["image/jpeg", "image/svg+xml", "IMAGE/PNG", "image/png; charset=utf-8", "image/png\u{1b}"] {
            cases.push(ImageContent { data: PNG.to_string(), mime_type: mime.to_string() });
        }
        for image in cases {
            let error = validate_image(&image).unwrap_err(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Every case violates a different input envelope invariant.
            assert!(error.to_string().contains("PI_JUDGE_IMAGE_INPUT")); // ubs:ignore[rust.panic.assert-macros] -- Image errors remain named and independent of provider details.
            assert!(!error.to_string().contains("CANARY")); // ubs:ignore[rust.panic.assert-macros] -- Rejections cannot echo raw image input.
        }
    }

    #[test]
    fn image_count_decoded_and_aggregate_encoded_allowances_are_independent() {
        let boundary_image = gif(MAX_JUDGMENT_IMAGE_BYTES, 1, 1);
        assert_eq!(validate_image(&boundary_image).unwrap(), MAX_JUDGMENT_IMAGE_BYTES / 3 * 4); // ubs:ignore[rust.panic.assert-macros,rust.panic.unwrap,rust.ownership.unwrap-expect] -- Exact decoded allowance is admitted without changing bytes.
        let oversized = gif(MAX_JUDGMENT_IMAGE_BYTES + 1, 1, 1);
        assert!(validate_image(&oversized).unwrap_err().to_string().contains("PI_JUDGE_IMAGE_LIMIT")); // ubs:ignore[rust.panic.assert-macros,rust.panic.unwrap,rust.ownership.unwrap-expect] -- A single additional decoded byte must exceed the per-image allowance.
        assert!(request().with_images(vec![boundary_image.clone(), boundary_image]).unwrap_err().to_string().contains("PI_JUDGE_IMAGE_LIMIT")); // ubs:ignore[rust.panic.assert-macros,rust.panic.unwrap,rust.ownership.unwrap-expect] -- Two individually admissible images cannot exceed the aggregate request allowance.
        assert!(request().with_images(vec![image(); MAX_JUDGMENT_IMAGES + 1]).is_err()); // ubs:ignore[rust.panic.assert-macros] -- Count admission is independent of byte size.
        let half = gif(3 * 1024 * 1024, 1, 1);
        let request = request().with_images(vec![half.clone(), half]).unwrap(); // ubs:ignore[rust.panic.unwrap,rust.ownership.unwrap-expect] -- Two 4 MiB encoded envelopes exactly fill the aggregate allowance.
        assert_eq!(request.image_bytes(), MAX_JUDGMENT_IMAGE_ENCODED_BYTES); // ubs:ignore[rust.panic.assert-macros] -- Aggregate charging is based on encoded bytes, not decoded bytes or image count.
    }
}
