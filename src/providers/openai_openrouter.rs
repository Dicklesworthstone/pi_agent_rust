//! OpenRouter's structured reasoning continuation for Chat Completions.
//!
//! The gateway's text, summary, and encrypted reasoning records must survive
//! tool turns and session serialization. Display text is separate from opaque
//! replay metadata; native provider signatures never enter this envelope.
//! Contract: https://openrouter.ai/docs/guides/best-practices/reasoning-tokens

use crate::model::{AssistantMessage, ContentBlock, StopReason};
use serde::Serialize;
use serde_json::Value;

const REPLAY_KIND: &str = "pi.openrouter.reasoning_details.v1";
const MAX_REASONING_BYTES: usize = 8 * 1024 * 1024;
const MAX_REASONING_ITEMS: usize = 1024;
const MAX_ORIGIN_BYTES: usize = 2048;
const MAX_REPLAY_BYTES: usize = MAX_REASONING_BYTES + MAX_ORIGIN_BYTES + 256;

/// URL origins exclude credentials, paths, and query secrets. An explicit
/// OpenRouter proxy is supported, but replay never crosses transport origins.
pub(super) fn origin(endpoint: &str) -> Option<String> {
    let endpoint = url::Url::parse(endpoint).ok()?;
    if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
        return None;
    }
    let origin = endpoint.origin().ascii_serialization();
    (origin.len() <= MAX_ORIGIN_BYTES).then_some(origin)
}

/// Bounded, incremental assembly. Counting incoming JSON bytes also bounds a
/// stream that repeatedly sends empty fragments or redundant metadata.
pub(super) struct ReasoningDetails {
    origin: String,
    items: Vec<Value>,
    received_bytes: usize,
    received: bool,
    failed: bool,
}

impl ReasoningDetails {
    pub(super) fn new(origin: String) -> Self {
        Self {
            origin,
            items: Vec::new(),
            received_bytes: 0,
            received: false,
            failed: false,
        }
    }

    pub(super) fn extend(&mut self, delta: Value) -> Result<String, &'static str> {
        if self.failed {
            return Err("OpenRouter reasoning details are already invalid");
        }
        let result = self.extend_inner(delta);
        if result.is_err() {
            self.failed = true;
            self.items.clear();
        }
        result
    }

    fn extend_inner(&mut self, delta: Value) -> Result<String, &'static str> {
        let Value::Array(details) = delta else {
            return Err("OpenRouter reasoning_details must be an array");
        };
        if details.len() > MAX_REASONING_ITEMS {
            return Err("OpenRouter reasoning details exceed the item limit");
        }
        for detail in &details {
            validate_detail(detail)?;
        }
        let bytes = serde_json::to_vec(&details)
            .map_err(|_| "OpenRouter reasoning details could not be encoded")?
            .len();
        self.received_bytes = self
            .received_bytes
            .checked_add(bytes)
            .filter(|bytes| *bytes <= MAX_REASONING_BYTES)
            .ok_or("OpenRouter reasoning details exceed the byte limit")?;
        self.received = true;

        let mut visible = String::new();
        for detail in details {
            match detail.get("type").and_then(Value::as_str) {
                Some("reasoning.text") => {
                    if let Some(text) = detail.get("text").and_then(Value::as_str) {
                        visible.push_str(text);
                    }
                }
                Some("reasoning.summary") => {
                    if let Some(summary) = detail.get("summary").and_then(Value::as_str) {
                        visible.push_str(summary);
                    }
                }
                // Ciphertext and signatures are never display content.
                _ => {}
            }
            if let Some(previous) = self.items.last_mut()
                && can_merge(previous, &detail)
            {
                merge_fragment(previous, detail);
            } else {
                if self.items.len() == MAX_REASONING_ITEMS {
                    return Err("OpenRouter reasoning details exceed the item limit");
                }
                self.items.push(detail);
            }
        }
        Ok(visible)
    }

    /// Encode once at finalization instead of copying the accumulated
    /// reasoning into every streamed snapshot.
    pub(super) fn signature(&self) -> Option<String> {
        if !self.received || self.failed {
            return None;
        }
        #[derive(Serialize)]
        struct Envelope<'a> {
            #[serde(rename = "type")]
            kind: &'static str,
            origin: &'a str,
            details: &'a [Value],
        }
        serde_json::to_string(&Envelope {
            kind: REPLAY_KIND,
            origin: &self.origin,
            details: &self.items,
        })
        .ok()
    }
}

fn validate_detail(detail: &Value) -> Result<(), &'static str> {
    let object = detail
        .as_object()
        .ok_or("OpenRouter reasoning detail must be an object")?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or("OpenRouter reasoning detail has no type")?;
    match kind {
        "reasoning.text" => {
            for field in ["text", "signature"] {
                if object
                    .get(field)
                    .is_some_and(|value| !value.is_null() && !value.is_string())
                {
                    return Err("OpenRouter reasoning text or signature has an invalid type");
                }
            }
        }
        "reasoning.summary" => {
            if !object.get("summary").is_some_and(Value::is_string) {
                return Err("OpenRouter reasoning summary must be a string");
            }
        }
        "reasoning.encrypted" => {
            if !object.get("data").is_some_and(Value::is_string) {
                return Err("OpenRouter encrypted reasoning data must be a string");
            }
        }
        _ => return Err("OpenRouter returned an unsupported reasoning detail type"),
    }
    for field in ["id", "format"] {
        if object
            .get(field)
            .is_some_and(|value| !value.is_null() && !value.is_string())
        {
            return Err("OpenRouter reasoning detail identity has an invalid type");
        }
    }
    if object
        .get("index")
        .is_some_and(|value| !value.is_null() && value.as_u64().is_none())
    {
        return Err("OpenRouter reasoning detail index must be a nonnegative integer");
    }
    Ok(())
}

/// Fragment assembly follows logical block order, not a global index map.
/// Gateways may reuse index 0 for several blocks. A signature ends its text
/// block, explicit identity changes start a new block, and ciphertext remains
/// a discrete record even when adjacent records have the same index.
fn can_merge(previous: &Value, next: &Value) -> bool {
    let Some(previous) = previous.as_object() else {
        return false;
    };
    let Some(next) = next.as_object() else {
        return false;
    };
    let kind = previous.get("type").and_then(Value::as_str);
    if kind != next.get("type").and_then(Value::as_str) {
        return false;
    }
    let payload = match kind {
        Some("reasoning.text") => {
            if previous
                .get("signature")
                .and_then(Value::as_str)
                .is_some_and(|signature| !signature.is_empty())
            {
                return false;
            }
            "text"
        }
        Some("reasoning.summary") => "summary",
        _ => return false,
    };
    next.iter().all(|(key, value)| {
        key == payload
            || (payload == "text" && key == "signature")
            || value.is_null()
            || previous
                .get(key)
                .is_none_or(|prior| prior.is_null() || prior == value)
    })
}

fn merge_fragment(previous: &mut Value, next: Value) {
    let (Some(previous), Value::Object(next)) = (previous.as_object_mut(), next) else {
        return;
    };
    let payload = if previous.get("type").and_then(Value::as_str) == Some("reasoning.text") {
        "text"
    } else {
        "summary"
    };
    for (key, value) in next {
        if key == payload {
            if let Value::String(fragment) = value {
                let target = previous
                    .entry(key)
                    .or_insert_with(|| Value::String(String::new()));
                if let Value::String(text) = target {
                    text.push_str(&fragment);
                } else {
                    *target = Value::String(fragment);
                }
            }
        } else if previous.get(&key).is_none_or(|prior| {
            !value.is_null()
                && (prior.is_null()
                    || (key == "signature" && prior.as_str().is_some_and(str::is_empty)))
        }) {
            previous.insert(key, value);
        }
    }
}

/// Replay only the exact provider/API/model that produced the records. The
/// caller additionally checks that its destination uses the OpenRouter
/// dialect. Malformed or foreign signatures do not become replay payloads.
pub(super) fn replay(
    assistant: &AssistantMessage,
    provider: &str,
    model: &str,
    origin: &str,
) -> Option<Vec<Value>> {
    if assistant.api != "openai-completions"
        || assistant.provider != provider
        || assistant.model != model
        || !matches!(
            assistant.stop_reason,
            StopReason::Stop | StopReason::ToolUse
        )
    {
        return None;
    }
    let mut replay = None;
    for block in &assistant.content {
        let ContentBlock::Thinking(thinking) = block else {
            continue;
        };
        let Some(signature) = thinking.thinking_signature.as_deref() else {
            continue;
        };
        if signature.len() > MAX_REPLAY_BYTES {
            return None;
        }
        let Ok(Value::Object(mut envelope)) = serde_json::from_str::<Value>(signature) else {
            continue;
        };
        if envelope.get("type").and_then(Value::as_str) != Some(REPLAY_KIND) {
            continue;
        }
        if envelope.get("origin").and_then(Value::as_str) != Some(origin) {
            return None;
        }
        // One response has one envelope. Repeating it on parallel tool calls
        // or multiple thinking blocks must not duplicate signed input items.
        if replay.is_some() {
            return None;
        }
        let Some(Value::Array(details)) = envelope.remove("details") else {
            return None;
        };
        if details.len() > MAX_REASONING_ITEMS
            || details
                .iter()
                .any(|detail| validate_detail(detail).is_err())
        {
            return None;
        }
        replay = Some(details);
    }
    replay
}

#[cfg(test)]
mod tests {
    use super::super::{OpenAIProvider, wire_stream};
    use super::*;
    use crate::model::{
        Message, StreamEvent, TextContent, ThinkingContent, ThinkingLevel, ToolResultMessage,
    };
    use crate::provider::{Context, StreamOptions, ToolDef};
    use futures::StreamExt as _;
    use serde_json::json;
    use std::fmt::Write as _;

    const ORIGIN: &str = "https://openrouter.ai";
    const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";
    const MODEL: &str = "anthropic/claude-test";

    fn provider() -> OpenAIProvider {
        OpenAIProvider::new(MODEL)
            .with_provider_name("openrouter")
            .with_base_url(ENDPOINT)
            .with_reasoning(true)
    }

    fn choice(delta: Value, finish: Option<&str>) -> Value {
        let mut frame = json!({"choices": [{"index": 0, "finish_reason": finish}]});
        frame["choices"][0]["delta"] = delta;
        frame
    }

    fn tool_delta() -> Value {
        json!({"tool_calls": [
            {"index": 0, "id": "read-a", "function": {
                "name": "read", "arguments": "{\"path\":\"a.rs\"}"}},
            {"index": 3, "id": "read-b", "function": {
                "name": "read", "arguments": "{\"path\":\"b.rs\"}"}}
        ]})
    }

    fn wire_bytes(frames: impl IntoIterator<Item = Value>) -> Vec<u8> {
        let mut bytes = String::new();
        for frame in frames {
            write!(bytes, "data: {frame}\n\n").unwrap();
        }
        bytes.push_str("data: [DONE]\n\n");
        bytes.into_bytes()
    }

    fn raw_events(bytes: &[u8], capture: bool) -> Vec<crate::error::Result<StreamEvent>> {
        // Split UTF-8, SSE delimiters and JSON strings across transport chunks.
        let chunks = bytes
            .chunks(37)
            .map(|chunk| Ok(chunk.to_vec()))
            .collect::<Vec<std::io::Result<Vec<u8>>>>();
        let output = wire_stream(
            futures::stream::iter(chunks),
            MODEL.to_string(),
            "openai-completions".to_string(),
            if capture { "openrouter" } else { "openai" }.to_string(),
            false,
            None,
            capture.then(|| ORIGIN.to_string()),
        );
        futures::executor::block_on(output.collect())
    }

    fn events(frames: impl IntoIterator<Item = Value>) -> Vec<StreamEvent> {
        raw_events(&wire_bytes(frames), true)
            .into_iter()
            .collect::<crate::error::Result<Vec<_>>>()
            .expect("valid SSE transport")
    }

    fn done(events: &[StreamEvent]) -> &AssistantMessage {
        let Some(StreamEvent::Done { reason, message }) = events.last() else {
            panic!("missing terminal response");
        };
        assert_eq!(*reason, message.stop_reason);
        message
    }

    fn thinking_text(message: &AssistantMessage) -> String {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Thinking(thinking) => Some(thinking.thinking.as_str()),
                _ => None,
            })
            .collect()
    }

    fn captured(details: Value) -> AssistantMessage {
        let mut capture = ReasoningDetails::new(ORIGIN.to_string());
        capture.extend(details).unwrap();
        AssistantMessage {
            api: "openai-completions".to_string(),
            provider: "openrouter".to_string(),
            model: MODEL.to_string(),
            content: vec![ContentBlock::Thinking(ThinkingContent {
                thinking: "display text".to_string(),
                thinking_signature: capture.signature(),
            })],
            ..AssistantMessage::default()
        }
    }

    fn signature_mut(message: &mut AssistantMessage) -> &mut Option<String> {
        let ContentBlock::Thinking(thinking) = &mut message.content[0] else {
            panic!("expected reasoning");
        };
        &mut thinking.thinking_signature
    }

    #[test]
    fn signed_stream_survives_session_roundtrip_and_parallel_tool_results() {
        let signature = "opaque-signature/\u{03b1}\n\0";
        let encrypted = json!({
            "type": "reasoning.encrypted", "id": "cipher-a", "index": 0,
            "format": "openai-responses-v1", "data": "CIPHERTEXT/+==",
            "provider_extension": {"keep": true}
        });
        let summary = json!({"type": "reasoning.summary", "summary": "Then read.", "index": 1});
        let output = events([
            choice(
                json!({
                    "reasoning": "Inspect ",
                    "reasoning_details": [{
                        "type": "reasoning.text", "id": "thought-a", "index": 0,
                        "format": "anthropic-claude-v1", "text": "Inspect ", "signature": null
                    }]
                }),
                None,
            ),
            choice(
                json!({"reasoning_details": [{
                    "type": "reasoning.text", "text": "inputs"
                }]}),
                None,
            ),
            choice(
                json!({"reasoning_details": [
                    {"type": "reasoning.text", "signature": signature},
                    encrypted, summary
                ]}),
                None,
            ),
            choice(tool_delta(), Some("tool_calls")),
        ]);
        let message = done(&output);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(thinking_text(message), "Inspect inputsThen read.");
        assert_eq!(
            output
                .iter()
                .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
                .count(),
            2
        );
        let visible = output
            .iter()
            .filter_map(|event| match event {
                StreamEvent::ThinkingDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(visible, "Inspect inputsThen read.");
        assert!(!visible.contains(signature));
        assert!(!visible.contains("CIPHERTEXT"));

        // Use the session's actual serialized Message shape, then continue with
        // both results and thinking explicitly disabled on this request.
        let persisted = serde_json::to_vec(&Message::assistant(message.clone())).unwrap();
        let restored: Message = serde_json::from_slice(&persisted).unwrap();
        let mut history = vec![restored];
        for id in ["read-a", "read-b"] {
            history.push(Message::tool_result(ToolResultMessage {
                tool_call_id: id.to_string(),
                tool_name: "read".to_string(),
                content: vec![ContentBlock::Text(TextContent::new("file contents"))],
                details: None,
                is_error: false,
                timestamp: 0,
            }));
        }
        let context = Context::owned(
            Some("system".to_string()),
            history,
            vec![ToolDef {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({"type": "object"}),
            }],
        );
        let request = provider()
            .build_request_json(
                &context,
                &StreamOptions {
                    thinking_level: Some(ThinkingLevel::Off),
                    ..StreamOptions::default()
                },
            )
            .unwrap();
        let expected = json!([
            {"type": "reasoning.text", "id": "thought-a", "index": 0,
             "format": "anthropic-claude-v1", "text": "Inspect inputs", "signature": signature},
            encrypted, summary
        ]);
        assert_eq!(request["messages"][1]["reasoning_details"], expected);
        assert_eq!(request["messages"][1]["tool_calls"][0]["id"], "read-a");
        assert_eq!(request["messages"][1]["tool_calls"][1]["id"], "read-b");
        assert_eq!(request["messages"][2]["tool_call_id"], "read-a");
        assert_eq!(request["messages"][3]["tool_call_id"], "read-b");
        for index in [0, 2, 3] {
            assert!(
                request["messages"][index]
                    .get("reasoning_details")
                    .is_none()
            );
        }
        assert!(request["messages"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn fragment_merging_preserves_signed_boundaries_identity_and_encrypted_order() {
        let mut capture = ReasoningDetails::new(ORIGIN.to_string());
        for delta in [
            json!([{"type": "reasoning.text", "index": 0, "text": "first"}]),
            json!([{"type": "reasoning.text", "signature": "sig-first", "provider_extension": null}]),
            json!([{"type": "reasoning.text", "index": 0, "text": "second", "signature": "sig-second"}]),
            json!([{"type": "reasoning.summary", "index": 0, "id": "summary-a", "summary": "sum"}]),
            json!([{"type": "reasoning.summary", "summary": "mary"}]),
            json!([{"type": "reasoning.summary", "index": 0, "id": "summary-b", "summary": "new"}]),
            json!([{"type": "reasoning.encrypted", "index": 0, "data": "cipher-one"}]),
            json!([{"type": "reasoning.encrypted", "index": 0, "data": "cipher-two"}]),
            json!([{"type": "reasoning.text", "index": 1, "text": "third"}]),
            json!([{"type": "reasoning.text", "index": 2, "text": "fourth"}]),
        ] {
            capture.extend(delta).unwrap();
        }
        let envelope: Value = serde_json::from_str(&capture.signature().unwrap()).unwrap();
        assert_eq!(
            envelope["details"],
            json!([
                {"type": "reasoning.text", "index": 0, "text": "first", "signature": "sig-first", "provider_extension": null},
                {"type": "reasoning.text", "index": 0, "text": "second", "signature": "sig-second"},
                {"type": "reasoning.summary", "index": 0, "id": "summary-a", "summary": "summary"},
                {"type": "reasoning.summary", "index": 0, "id": "summary-b", "summary": "new"},
                {"type": "reasoning.encrypted", "index": 0, "data": "cipher-one"},
                {"type": "reasoning.encrypted", "index": 0, "data": "cipher-two"},
                {"type": "reasoning.text", "index": 1, "text": "third"},
                {"type": "reasoning.text", "index": 2, "text": "fourth"}
            ])
        );
    }

    #[test]
    fn metadata_only_and_explicit_empty_arrays_survive_without_displaying_secrets() {
        for details in [
            json!([]),
            json!([{"type": "reasoning.encrypted", "data": "secret-cipher"}]),
            json!([{"type": "reasoning.text", "signature": "secret-signature"}]),
        ] {
            let output = events([
                choice(json!({"reasoning_details": details}), None),
                choice(tool_delta(), Some("tool_calls")),
            ]);
            let message = done(&output);
            assert_eq!(message.stop_reason, StopReason::ToolUse);
            assert!(thinking_text(message).is_empty());
            assert!(
                !output
                    .iter()
                    .any(|event| matches!(event, StreamEvent::ThinkingDelta { .. }))
            );
            let context =
                Context::owned(None, vec![Message::assistant(message.clone())], Vec::new());
            let request = provider()
                .build_request_json(&context, &StreamOptions::default())
                .unwrap();
            assert_eq!(request["messages"][0]["reasoning_details"], details);
            assert_eq!(request["messages"][0]["content"], json!(""));
        }
        for delta in [
            json!({"content": "plain reply"}),
            json!({"content": "plain reply", "reasoning_details": null}),
        ] {
            let output = events([choice(delta, Some("stop"))]);
            assert!(replay(done(&output), "openrouter", MODEL, ORIGIN).is_none());
        }
    }

    #[test]
    fn replay_is_fenced_by_transport_provider_api_model_and_completion() {
        let details = json!([{"type": "reasoning.encrypted", "data": "opaque-state"}]);
        let original = captured(details.clone());
        assert_eq!(
            replay(&original, "openrouter", MODEL, ORIGIN),
            details.as_array().cloned()
        );
        assert!(replay(&original, "another-provider", MODEL, ORIGIN).is_none());
        assert!(replay(&original, "openrouter", "another-model", ORIGIN).is_none());
        assert!(replay(&original, "openrouter", MODEL, "https://proxy.example").is_none());
        let mut foreign_api = original.clone();
        foreign_api.api = "anthropic-messages".to_string();
        assert!(replay(&foreign_api, "openrouter", MODEL, ORIGIN).is_none());
        for reason in [
            StopReason::Error,
            StopReason::Aborted,
            StopReason::Length,
            StopReason::Refusal,
        ] {
            let mut failed = original.clone();
            failed.stop_reason = reason;
            assert!(replay(&failed, "openrouter", MODEL, ORIGIN).is_none());
        }

        let context = Context::owned(None, vec![Message::assistant(original)], Vec::new());
        for endpoint in [
            "https://proxy.example/api/v1/chat/completions",
            "http://openrouter.ai/api/v1/chat/completions",
            "https://openrouter.ai:8443/api/v1/chat/completions",
        ] {
            let request = provider()
                .with_base_url(endpoint)
                .build_request_json(&context, &StreamOptions::default())
                .unwrap();
            assert!(request["messages"][0].get("reasoning_details").is_none());
        }
        assert_eq!(
            origin("https://user:password@openrouter.ai:443/path?key=secret"),
            Some(ORIGIN.to_string())
        );
        assert_eq!(origin("data:text/plain,secret"), None);
    }

    #[test]
    fn gateway_detection_uses_the_host_and_custom_routes_keep_their_own_origin() {
        for endpoint in [
            "https://openrouter.ai.evil.example/chat/completions",
            "https://evil.example/openrouter.ai",
            "https://evil.example/?target=openrouter.ai",
            "https://openrouter.ai@evil.example/chat/completions",
        ] {
            assert!(
                !OpenAIProvider::new(MODEL)
                    .with_base_url(endpoint)
                    .is_openrouter()
            );
        }
        assert!(
            OpenAIProvider::new(MODEL)
                .with_base_url(ENDPOINT)
                .is_openrouter()
        );
        let proxy = "https://proxy.example";
        let mut capture = ReasoningDetails::new(proxy.to_string());
        capture.extend(json!([])).unwrap();
        let mut message = captured(json!([]));
        *signature_mut(&mut message) = capture.signature();
        let context = Context::owned(None, vec![Message::assistant(message)], Vec::new());
        let request = provider()
            .with_base_url(format!("{proxy}/v1/chat/completions"))
            .build_request_json(&context, &StreamOptions::default())
            .unwrap();
        assert_eq!(request["messages"][0]["reasoning_details"], json!([]));
    }

    #[test]
    fn malformed_details_poison_the_whole_tool_batch_without_echoing_payloads() {
        for invalid in [
            json!({"secret": "DO-NOT-ECHO"}),
            json!([null]),
            json!([{"type": "DO-NOT-ECHO", "data": "secret"}]),
            json!([{"type": "reasoning.text", "signature": {"secret": "DO-NOT-ECHO"}}]),
            json!([{"type": "reasoning.summary", "summary": ["DO-NOT-ECHO"]}]),
            json!([{"type": "reasoning.encrypted", "data": {"secret": "DO-NOT-ECHO"}}]),
            json!([{"type": "reasoning.encrypted", "data": "DO-NOT-ECHO", "index": -1}]),
            json!([{"type": "reasoning.text", "text": "DO-NOT-ECHO", "id": 42}]),
            json!([{"type": "reasoning.text", "text": "DO-NOT-ECHO", "format": []}]),
        ] {
            let output = events([
                choice(tool_delta(), None),
                choice(json!({"reasoning_details": invalid}), None),
                choice(json!({"reasoning_details": []}), Some("tool_calls")),
            ]);
            let message = done(&output);
            assert_eq!(message.stop_reason, StopReason::Error);
            assert!(
                !output
                    .iter()
                    .any(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
            );
            assert!(
                !message
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains("DO-NOT-ECHO")
            );
            assert!(replay(message, "openrouter", MODEL, ORIGIN).is_none());
        }
    }

    #[test]
    fn reasoning_limits_apply_across_deltas_and_failure_cannot_be_reset() {
        let fragment = json!([{
            "type": "reasoning.text",
            "text": "x".repeat(MAX_REASONING_BYTES / 16)
        }]);
        let mut capture = ReasoningDetails::new(ORIGIN.to_string());
        for _ in 0..15 {
            capture.extend(fragment.clone()).unwrap();
        }
        assert!(capture.extend(fragment).is_err());
        assert!(capture.extend(json!([])).is_err());
        assert!(capture.signature().is_none());

        let mut capture = ReasoningDetails::new(ORIGIN.to_string());
        for _ in 0..MAX_REASONING_ITEMS {
            capture
                .extend(json!([{"type": "reasoning.encrypted", "data": "opaque"}]))
                .unwrap();
        }
        assert!(
            capture
                .extend(json!([{"type": "reasoning.encrypted", "data": "overflow"}]))
                .is_err()
        );
        assert!(capture.signature().is_none());

        // A single oversize array must also prevent completed local calls.
        let too_many =
            vec![json!({"type": "reasoning.encrypted", "data": "opaque"}); MAX_REASONING_ITEMS + 1];
        let output = events([
            choice(tool_delta(), None),
            choice(json!({"reasoning_details": too_many}), Some("tool_calls")),
        ]);
        assert_eq!(done(&output).stop_reason, StopReason::Error);
        assert!(
            !output
                .iter()
                .any(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
        );
    }

    #[test]
    fn foreign_malformed_and_duplicated_persisted_signatures_are_not_replayed() {
        let original = captured(json!([{"type": "reasoning.encrypted", "data": "opaque"}]));
        let mut duplicate = original.clone();
        duplicate.content.extend(original.content.clone());
        assert!(replay(&duplicate, "openrouter", MODEL, ORIGIN).is_none());
        for signature in [
            "native-anthropic-signature".to_string(),
            r#"{"type":"pi.openrouter.reasoning_details.v1","origin":"https://openrouter.ai","details":{}}"#.to_string(),
            json!({"type": REPLAY_KIND, "origin": ORIGIN, "details": [{"type": "reasoning.encrypted", "data": 42}]}).to_string(),
            "x".repeat(MAX_REPLAY_BYTES + 1),
        ] {
            let mut message = original.clone();
            *signature_mut(&mut message) = Some(signature);
            assert!(replay(&message, "openrouter", MODEL, ORIGIN).is_none());
        }
    }

    #[test]
    fn late_reasoning_invalidates_completion_and_bad_json_never_echoes_opaque_data() {
        let output = events([
            choice(tool_delta(), Some("tool_calls")),
            choice(
                json!({"reasoning_details": [
                    {"type": "reasoning.encrypted", "data": "late-cipher"}
                ]}),
                None,
            ),
        ]);
        assert_eq!(done(&output).stop_reason, StopReason::Error);
        assert!(replay(done(&output), "openrouter", MODEL, ORIGIN).is_none());

        // Serde normally includes the invalid typed field's value in its error.
        let bytes = wire_bytes([choice(
            json!({
                "reasoning_details": [{"type": "reasoning.encrypted", "data": "PRIVATE-CIPHER"}],
                "tool_calls": [{"index": "PRIVATE-INDEX"}]
            }),
            None,
        )]);
        let output = raw_events(&bytes, true);
        let error = output.last().unwrap().as_ref().unwrap_err().to_string();
        assert!(error.contains("Invalid OpenAI stream JSON"));
        assert!(!error.contains("PRIVATE"));
    }

    #[test]
    fn final_signature_after_answer_does_not_reopen_or_duplicate_visible_thinking() {
        let same_frame = events([choice(
            json!({
                "reasoning": "first thought",
                "reasoning_details": [{
                    "type": "reasoning.text", "text": "first thought", "signature": "first-signature"
                }],
                "content": "first answer"
            }),
            Some("stop"),
        )]);
        assert!(matches!(
            done(&same_frame).content.as_slice(),
            [ContentBlock::Thinking(thinking), ContentBlock::Text(text)]
                if thinking.thinking == "first thought" && text.text == "first answer"
        ));

        for early_details in [false, true] {
            let opening = if early_details {
                json!({"reasoning_details": [{
                    "type": "reasoning.text", "text": "original thought", "index": 0
                }]})
            } else {
                json!({"reasoning": "original thought"})
            };
            let mut final_detail = json!({
                "type": "reasoning.text", "signature": "late-signature", "index": 0
            });
            if !early_details {
                final_detail["text"] = json!("original thought");
            }
            let output = events([
                choice(opening, None),
                choice(json!({"content": "answer"}), None),
                choice(
                    json!({
                        "reasoning": "original thought",
                        "reasoning_details": [final_detail]
                    }),
                    Some("stop"),
                ),
            ]);
            let message = done(&output);
            assert_eq!(message.stop_reason, StopReason::Stop);
            assert_eq!(thinking_text(message), "original thought");
            assert!(matches!(
                message.content.as_slice(),
                [ContentBlock::Thinking(_), ContentBlock::Text(text)] if text.text == "answer"
            ));
            assert_eq!(
                output
                    .iter()
                    .filter(|event| matches!(event, StreamEvent::ThinkingStart { .. }))
                    .count(),
                1
            );
            assert_eq!(
                replay(message, "openrouter", MODEL, ORIGIN),
                Some(vec![json!({
                    "type": "reasoning.text", "text": "original thought",
                    "signature": "late-signature", "index": 0
                })])
            );
        }
    }

    #[test]
    fn native_dialects_ignore_gateway_metadata_and_keep_plain_reasoning() {
        let bytes = wire_bytes([
            choice(
                json!({"reasoning_content": "native thought", "reasoning_details": {"bad": true}}),
                None,
            ),
            choice(json!({"content": "answer"}), Some("stop")),
        ]);
        let output = raw_events(&bytes, false)
            .into_iter()
            .collect::<crate::error::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(done(&output).stop_reason, StopReason::Stop);
        assert_eq!(thinking_text(done(&output)), "native thought");
        assert!(done(&output).content.iter().all(|block| match block {
            ContentBlock::Thinking(thinking) => thinking.thinking_signature.is_none(),
            _ => true,
        }));

        let mut native = captured(json!([]));
        native.provider = "deepseek".to_string();
        native.model = "deepseek-reasoner".to_string();
        *signature_mut(&mut native) = None;
        let context = Context::owned(
            None,
            vec![Message::assistant(native)],
            vec![ToolDef {
                name: "read".to_string(),
                description: "Read".to_string(),
                parameters: json!({"type": "object"}),
            }],
        );
        let request = OpenAIProvider::new("deepseek-reasoner")
            .with_provider_name("deepseek")
            .with_base_url("https://api.deepseek.com/v1/chat/completions")
            .with_reasoning(true)
            .build_request_json(&context, &StreamOptions::default())
            .unwrap();
        assert_eq!(request["messages"][0]["reasoning_content"], "display text");
        assert!(request["messages"][0].get("reasoning_details").is_none());
    }
}
