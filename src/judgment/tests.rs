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
    tokens: Mutex<Vec<Option<u32>>>,
}
impl FixtureProvider {
    fn new(events: Vec<StreamEvent>) -> Arc<Self> {
        Arc::new(Self { events: Mutex::new(Some(events)), prompts: Mutex::new(Vec::new()), tokens: Mutex::new(Vec::new()) })
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
        let UserContent::Text(text) = &message.content else { panic!("expected text"); };
        self.prompts.lock().unwrap().push(text.clone());
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
