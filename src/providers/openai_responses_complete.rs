//! Bounded complete JSON replies for Responses endpoints without SSE support.
//!
//! Only framing changes: terminal outcomes, tool identities, strict arguments,
//! reasoning replay and billing reuse the streaming provider's production state.

use super::{
    EventStream, MAX_READY_EVENTS_PER_YIELD, MAX_RESPONSES_ITEM_ID_LEN, OpenAIResponsesDonePayload,
    OpenAIResponsesIncompleteDetails, OpenAIResponsesUsage, ReasoningKind, ResponseOutcome,
    ResponseTerminalEvent, StreamState, ToolItemEvent, yield_stream_turn,
};
use crate::error::{Error, Result};
use crate::http::client::Response;
use crate::model::StreamEvent;
use crate::sse::SseEvent;
use futures::{StreamExt as _, stream};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_OUTPUT_ITEMS: usize = 1024;
const MAX_OUTPUT_PARTS: usize = 4096;

type CompleteState = StreamState<stream::Empty<std::io::Result<SseEvent>>>;

pub(super) fn wire_response(
    response: Response,
    model: String,
    api: String,
    provider: String,
    usage_observer: Option<crate::provider::UsageObserver>,
) -> EventStream {
    let mut state = StreamState::new(stream::empty(), model, api, provider);
    state.usage_observer = usage_observer;
    let events = stream::unfold(Some((state, Some(response))), |active| async move {
        let (mut state, response) = active?;
        if let Some(response) = response {
            let body = match response.bytes_limited(MAX_RESPONSE_BYTES).await {
                Ok(body) => body,
                Err(error) => return Some((Err(error), None)),
            };
            if let Err(error) = process_response(&mut state, &body) {
                return Some((Err(error), None));
            }
        }
        if state.ready_work_since_yield == MAX_READY_EVENTS_PER_YIELD {
            yield_stream_turn().await;
            state.ready_work_since_yield = 0;
        }
        let event = state.pending_events.pop_front()?;
        state.ready_work_since_yield += 1;
        let terminal = matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
        Some((Ok(event), (!terminal).then_some((state, None))))
    });
    Box::pin(events.fuse())
}

fn reject(state: &mut CompleteState, message: &str) {
    state.reject_tool_response(format!("Invalid OpenAI Responses JSON: {message}"));
}

#[allow(clippy::too_many_lines)] // Keep whole-response acceptance and billing together.
fn process_response(state: &mut CompleteState, body: &[u8]) -> Result<()> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(Error::api("OpenAI Responses JSON exceeds the 8 MiB body limit"));
    }
    let response: CompleteResponse = serde_json::from_slice(body).map_err(|error| {
        Error::api(format!(
            "Invalid OpenAI Responses JSON at line {}, column {}",
            error.line(),
            error.column()
        ))
    })?;
    state.ensure_started();
    if let Some(usage) = &response.usage {
        // A billed request stays billed even if its output/status is rejected.
        state.record_usage(usage)?;
    }
    if serde_json::from_slice::<super::super::openai::UniqueToolArgumentKeys>(body).is_err() {
        reject(state, "duplicate or ambiguous JSON object keys");
        return Ok(());
    }
    if response.object.as_deref().is_some_and(|object| object != "response") {
        reject(state, "unexpected response object type");
        return Ok(());
    }
    let event = match response.status.as_deref() {
        Some("completed") => ResponseTerminalEvent::Completed,
        Some("incomplete") => ResponseTerminalEvent::Incomplete,
        Some("failed") => {
            reject(state, "provider returned a failed response");
            return Ok(());
        }
        Some("cancelled") => {
            reject(state, "provider returned a cancelled response");
            return Ok(());
        }
        _ => {
            reject(state, "missing or nonterminal response status");
            return Ok(());
        }
    };
    if response.error.is_some() {
        reject(state, "completion status contradicts the provider error");
        return Ok(());
    }
    let Some(usage) = response.usage else {
        reject(state, "completed response is missing usage");
        return Ok(());
    };
    let terminal = OpenAIResponsesDonePayload {
        status: response.status,
        incomplete_details: response.incomplete_details,
        // Each function-call status is checked below before its completion.
        output: None,
        usage,
    };
    let outcome = match terminal.outcome(event) {
        Ok(outcome) => outcome,
        Err(message) => {
            reject(state, message);
            return Ok(());
        }
    };
    let Some(output) = response.output.as_array() else {
        reject(state, "output must be an array");
        return Ok(());
    };
    if output.len() > MAX_OUTPUT_ITEMS {
        reject(state, "output exceeds 1024 items");
        return Ok(());
    }
    let incomplete = matches!(&outcome, ResponseOutcome::Incomplete(_));
    let mut ids = HashSet::new();
    let mut parts = 0;
    let mut refused = false;
    for (index, raw) in output.iter().enumerate() {
        let item: CompleteItem = match serde_json::from_value(raw.clone()) {
            Ok(item) => item,
            Err(_) => {
                reject(state, "malformed or unsupported output item");
                return Ok(());
            }
        };
        // IDs are copied into every content-part key and replay signature.
        // Bound them before those copies, using the same limit as input replay.
        if item.id().trim().is_empty()
            || item.id().len() > MAX_RESPONSES_ITEM_ID_LEN
            || !ids.insert(item.id().to_string())
        {
            reject(state, "output item identity is empty, oversized or duplicated");
            return Ok(());
        }
        if item.status().is_some_and(|status| {
            status != "completed" && !(incomplete && status == "incomplete")
        }) {
            reject(state, "output item has a nonterminal status");
            return Ok(());
        }
        parts += item.part_count();
        if parts > MAX_OUTPUT_PARTS {
            reject(state, "output exceeds 4096 content parts");
            return Ok(());
        }
        match item {
            CompleteItem::Message { id, role, content, .. } => {
                if role != "assistant" {
                    reject(state, "output message does not have the assistant role");
                    return Ok(());
                }
                for (index, part) in content.into_iter().enumerate() {
                    let text = match part {
                        CompleteContent::OutputText { text } => text,
                        CompleteContent::Refusal { refusal } => {
                            refused = true;
                            refusal
                        }
                    };
                    state.apply_text_snapshot(id.clone(), content_index(index)?, text);
                }
                state.capture_message_item_id(&id);
            }
            CompleteItem::Reasoning { id, summary, content, encrypted_content, .. } => {
                for (index, part) in summary.into_iter().enumerate() {
                    state.apply_reasoning_snapshot(
                        ReasoningKind::Summary,
                        id.clone(),
                        content_index(index)?,
                        part.text,
                    );
                }
                for (index, part) in content.unwrap_or_default().into_iter().enumerate() {
                    state.apply_reasoning_snapshot(
                        ReasoningKind::Text,
                        id.clone(),
                        content_index(index)?,
                        part.text,
                    );
                }
                state.capture_reasoning_item(&id, encrypted_content.as_deref(), "", raw);
            }
            CompleteItem::FunctionCall { id, call_id, name, arguments, status } => {
                if status.as_deref().is_some_and(|status| status != "completed") {
                    reject(state, "function call was not completed; local tool calls withheld");
                    return Ok(());
                }
                if !state.admit_tool_event(
                    &id,
                    Some((&call_id, &name)),
                    Some(content_index(index)?),
                    ToolItemEvent::Done,
                ) {
                    return Ok(());
                }
                state.end_tool_call(&id, &call_id, &name, &arguments);
                if state.finished {
                    return Ok(());
                }
            }
        }
    }
    if refused && state.partial_has_tool_call() {
        reject(state, "refusal combined with function calls; local tool calls withheld");
        return Ok(());
    }
    state.finish(if refused && matches!(&outcome, ResponseOutcome::Completed) {
        ResponseOutcome::Refusal
    } else {
        outcome
    });
    Ok(())
}

fn content_index(index: usize) -> Result<u32> {
    u32::try_from(index).map_err(|_| Error::api("OpenAI Responses content index overflow"))
}

#[derive(Deserialize)]
struct CompleteResponse {
    object: Option<String>,
    status: Option<String>,
    #[serde(default)]
    output: Value,
    usage: Option<OpenAIResponsesUsage>,
    error: Option<Value>,
    incomplete_details: Option<OpenAIResponsesIncompleteDetails>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum CompleteItem {
    #[serde(rename = "message")]
    Message {
        id: String,
        role: String,
        status: Option<String>,
        content: Vec<CompleteContent>,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        id: String,
        status: Option<String>,
        #[serde(default)]
        summary: Vec<SummaryPart>,
        content: Option<Vec<ReasoningPart>>,
        encrypted_content: Option<String>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: String,
        status: Option<String>,
    },
}

impl CompleteItem {
    fn id(&self) -> &str {
        match self {
            Self::Message { id, .. }
            | Self::Reasoning { id, .. }
            | Self::FunctionCall { id, .. } => id,
        }
    }

    fn status(&self) -> Option<&str> {
        match self {
            Self::Message { status, .. }
            | Self::Reasoning { status, .. }
            | Self::FunctionCall { status, .. } => status.as_deref(),
        }
    }

    fn part_count(&self) -> usize {
        match self {
            Self::Message { content, .. } => content.len(),
            Self::Reasoning { summary, content, .. } => {
                summary.len() + content.as_ref().map_or(0, Vec::len)
            }
            Self::FunctionCall { .. } => 1,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum CompleteContent {
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "refusal")]
    Refusal { refusal: String },
}

#[derive(Deserialize)]
struct SummaryPart {
    #[serde(rename = "type")]
    _kind: SummaryKind,
    text: String,
}

#[derive(Deserialize)]
enum SummaryKind {
    #[serde(rename = "summary_text")]
    SummaryText,
}

#[derive(Deserialize)]
struct ReasoningPart {
    #[serde(rename = "type")]
    _kind: ReasoningTextKind,
    text: String,
}

#[derive(Deserialize)]
enum ReasoningTextKind {
    #[serde(rename = "reasoning_text")]
    ReasoningText,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, ContentBlock, StopReason};
    use serde_json::json;

    fn state() -> CompleteState {
        StreamState::new(
            stream::empty(),
            "configured-model".to_string(),
            "openai-responses".to_string(),
            "configured-provider".to_string(),
        )
    }

    fn reply(output: Value) -> Value {
        json!({
            "object": "response", "status": "completed", "output": output,
            "model": "upstream-alias", "error": null, "incomplete_details": null,
            "usage": {
                "input_tokens": 100, "output_tokens": 20, "total_tokens": 120,
                "input_tokens_details": {"cached_tokens": 25}, "cost": 0.125
            }
        })
    }

    fn text_item() -> Value {
        json!({
            "type": "message", "id": "msg_1", "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "Reading now.", "annotations": []}]
        })
    }

    fn call(id: &str, call_id: &str, arguments: &str) -> Value {
        json!({
            "type": "function_call", "id": id, "call_id": call_id,
            "name": "read", "arguments": arguments, "status": "completed"
        })
    }

    fn terminal(state: &CompleteState) -> &AssistantMessage {
        match state.pending_events.back().expect("terminal event") {
            StreamEvent::Done { reason, message } => {
                assert_eq!(*reason, message.stop_reason);
                message
            }
            StreamEvent::Error { reason, error } => {
                assert_eq!(*reason, StopReason::Error);
                error
            }
            event => panic!("expected terminal event, received {event:?}"),
        }
    }

    fn tool_ends(state: &CompleteState) -> usize {
        state.pending_events.iter().filter(|event| {
            matches!(event, StreamEvent::ToolCallEnd { .. })
        }).count()
    }

    fn assert_rejected_with_usage(state: &CompleteState) {
        let message = terminal(state);
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(tool_ends(state), 0, "no sibling may authorize a local effect");
        assert_eq!(message.usage.input, 75);
        assert_eq!(message.usage.cache_read, 25);
        assert_eq!(message.usage.output, 20);
        assert!((message.usage.cost.total - 0.125).abs() < f64::EPSILON);
    }

    #[test]
    fn complete_response_preserves_reasoning_tools_replay_and_billing() {
        let reasoning = json!({
            "type": "reasoning", "id": "rs_1", "status": "completed",
            "summary": [{"type": "summary_text", "text": "Inspect both files."}],
            "content": [{"type": "reasoning_text", "text": "Check their dependencies."}],
            "encrypted_content": "opaque-continuation"
        });
        let body = reply(json!([
            reasoning.clone(), text_item(),
            call("fc_1", "call_1", r#"{"path":"a.rs"}"#),
            call("fc_2", "call_2", r#"{"path":"b.rs"}"#)
        ]));
        let mut state = state();
        process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(matches!(state.pending_events.front(), Some(StreamEvent::Start { .. })));
        assert_eq!(tool_ends(&state), 2);
        let message = terminal(&state);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(message.model, "configured-model");
        assert_eq!(message.provider, "configured-provider");
        assert_eq!(message.api, "openai-responses");
        assert!(matches!(message.content.as_slice(), [
            ContentBlock::Thinking(summary), ContentBlock::Thinking(detail),
            ContentBlock::Text(text), ContentBlock::ToolCall(first), ContentBlock::ToolCall(second)
        ] if summary.thinking == "Inspect both files."
            && detail.thinking == "Check their dependencies."
            && text.text == "Reading now." && text.text_signature.as_deref() == Some("msg_1")
            && first.id == "call_1" && first.arguments == json!({"path":"a.rs"})
            && first.thought_signature.as_deref() == Some("fc_1")
            && second.id == "call_2" && second.arguments == json!({"path":"b.rs"})));
        let ContentBlock::Thinking(thinking) = &message.content[0] else {
            panic!("reasoning replay block");
        };
        assert_eq!(
            serde_json::from_str::<Value>(thinking.thinking_signature.as_deref().unwrap()).unwrap(),
            reasoning
        );
        assert_eq!(message.usage.input, 75);
        assert_eq!(message.usage.cache_read, 25);
        assert_eq!(message.usage.output, 20);
        assert_eq!(message.usage.total_tokens, 120);
        assert!((message.usage.cost.total - 0.125).abs() < f64::EPSILON);
    }

    #[test]
    fn complete_response_keeps_summary_only_and_opaque_only_reasoning() {
        for (item, expected) in [
            (json!({"type":"reasoning", "id":"rs_1", "summary":[
                {"type":"summary_text", "text":"A useful summary."}
            ]}), "A useful summary."),
            (json!({"type":"reasoning", "id":"rs_1", "summary":[],
                "encrypted_content":"opaque-only"}), ""),
        ] {
            let mut state = state();
            let body = reply(json!([item.clone(), text_item()]));
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            let message = terminal(&state);
            assert_eq!(message.stop_reason, StopReason::Stop);
            let ContentBlock::Thinking(thinking) = &message.content[0] else {
                panic!("summary or opaque state must survive JSON framing");
            };
            assert_eq!(thinking.thinking, expected);
            assert_eq!(
                serde_json::from_str::<Value>(thinking.thinking_signature.as_deref().unwrap()).unwrap(),
                item
            );
        }
    }

    #[test]
    fn complete_response_rejects_whole_tool_batch_after_any_invalid_sibling() {
        for invalid in [
            call("fc_2", "call_2", r#"{"path":"unfinished"#),
            call("fc_2", "call_2", r#"{"path":"a","path":"b"}"#),
            call("fc_2", "call_2", "[]"),
            call("fc_2", "call_1", "{}"),
            call("fc_1", "call_2", "{}"),
            call("", "call_2", "{}"),
            call("fc_2", "", "{}"),
            json!({"type":"function_call", "id":"fc_2", "call_id":"call_2",
                "name":"", "arguments":"{}"}),
            json!({"type":"function_call", "id":"fc_2", "call_id":"call_2",
                "name":"read", "arguments":{}, "status":"completed"}),
            json!({"type":"function_call", "id":"fc_2", "call_id":"call_2",
                "name":"read", "status":"completed"}),
            json!({"type":"function_call", "id":"fc_2", "call_id":"call_2",
                "name":"read", "arguments":"{}", "status":"in_progress"}),
            json!({"type":"message", "id":"msg_2", "role":"user", "content":[]}),
            json!({"type":"message", "id":"msg_2", "role":"assistant",
                "content":[{"type":"output_text", "text":42}]}),
            json!({"type":"reasoning", "id":"rs_2",
                "summary":[{"type":"unknown", "text":"untrusted"}]}),
            json!({"type":"unknown", "id":"unknown_2"}),
        ] {
            let mut state = state();
            let body = reply(json!([call("fc_1", "call_1", "{}"), invalid]));
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert_rejected_with_usage(&state);
        }
    }

    #[test]
    fn complete_response_terminal_metadata_never_authorizes_unfinished_tools() {
        for status in ["incomplete", "failed", "cancelled", "queued", "in_progress", "unknown"] {
            let mut state = state();
            let mut body = reply(json!([call("fc_1", "call_1", "{}")]));
            body["status"] = json!(status);
            if status == "incomplete" {
                body["incomplete_details"] = json!({"reason":"max_output_tokens"});
            }
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert_rejected_with_usage(&state);
        }
        for (key, value) in [
            ("status", Value::Null),
            ("error", json!({"message":"upstream failed"})),
            ("incomplete_details", json!({"reason":"max_output_tokens"})),
            ("object", json!("chat.completion")),
            ("output", json!({"type":"function_call"})),
        ] {
            let mut state = state();
            let mut body = reply(json!([call("fc_1", "call_1", "{}")]));
            body[key] = value;
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert_rejected_with_usage(&state);
        }
    }

    #[test]
    fn complete_response_distinguishes_text_length_refusal_and_failed_status() {
        let refusal = json!({
            "type":"message", "id":"msg_1", "role":"assistant", "status":"completed",
            "content":[{"type":"refusal", "refusal":"Cannot do that."}]
        });
        let mut state = state();
        let body = reply(json!([refusal.clone()]));
        process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(terminal(&state).stop_reason, StopReason::Refusal);
        assert!(matches!(terminal(&state).content.as_slice(),
            [ContentBlock::Text(text)] if text.text == "Cannot do that."));

        let mut refused_tools = self::state();
        let body = reply(json!([call("fc_1", "call_1", "{}"), refusal]));
        process_response(&mut refused_tools, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert_rejected_with_usage(&refused_tools);

        for (reason, expected) in [
            ("max_output_tokens", StopReason::Length),
            ("max_messages", StopReason::Length),
            ("content_filter", StopReason::Error),
            ("unknown", StopReason::Error),
        ] {
            let mut truncated = self::state();
            let mut body = reply(json!([text_item()]));
            body["status"] = json!("incomplete");
            body["incomplete_details"] = json!({"reason":reason});
            body["output"][0]["status"] = json!("incomplete");
            process_response(&mut truncated, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert_eq!(terminal(&truncated).stop_reason, expected);
            assert_eq!(tool_ends(&truncated), 0);
        }
        for status in ["failed", "cancelled"] {
            let mut failed = self::state();
            let body = json!({"status":status, "output":[], "usage":null});
            process_response(&mut failed, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert_eq!(terminal(&failed).stop_reason, StopReason::Error);
        }
        let mut missing_usage = self::state();
        let mut body = reply(json!([call("fc_1", "call_1", "{}")]));
        body["usage"] = Value::Null;
        process_response(&mut missing_usage, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert_eq!(terminal(&missing_usage).stop_reason, StopReason::Error);
        assert_eq!(tool_ends(&missing_usage), 0);
    }

    #[test]
    fn complete_response_rejects_ambiguous_keys_without_losing_known_usage() {
        let body = br#"{
            "status":"completed", "output":[{
                "type":"function_call", "id":"fc_1", "call_id":"call_1",
                "name":"read", "arguments":"{}",
                "metadata":{"mode":1,"mo\u0064e":2}
            }],
            "usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,
                "input_tokens_details":{"cached_tokens":25},"cost":0.125}
        }"#;
        let mut state = state();
        process_response(&mut state, body).unwrap();
        assert_rejected_with_usage(&state);
    }

    #[test]
    fn complete_response_bounds_output_and_keeps_parser_errors_private() {
        for body in [
            br#"{"status":"completed","output":["opaque-secret"#.as_slice(),
            br#"{"status":"completed","status":"failed","secret":"opaque-secret"}"#.as_slice(),
            br#"{"status":42,"secret":"opaque-secret"}"#.as_slice(),
            br#"{"status":"completed"} trailing-opaque-secret"#.as_slice(),
        ] {
            let mut state = state();
            let error = process_response(&mut state, body).unwrap_err().to_string();
            assert!(error.contains("Invalid OpenAI Responses JSON"));
            assert!(!error.contains("opaque-secret"));
            assert!(state.pending_events.is_empty());
        }
        let mut oversized = state();
        let error = process_response(&mut oversized, &vec![b' '; MAX_RESPONSE_BYTES + 1])
            .unwrap_err().to_string();
        assert!(error.contains("8 MiB"));
        assert!(oversized.pending_events.is_empty());

        let too_many_items = reply(json!(vec![text_item(); MAX_OUTPUT_ITEMS + 1]));
        let mut too_many_parts = reply(json!([text_item()]));
        too_many_parts["output"][0]["content"] = json!(vec![
            json!({"type":"output_text", "text":"x"}); MAX_OUTPUT_PARTS + 1
        ]);
        for body in [too_many_items, too_many_parts] {
            let mut state = state();
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert_rejected_with_usage(&state);
        }
    }

    #[test]
    fn complete_response_bounds_item_ids_before_content_part_expansion() {
        for mut item in [
            text_item(),
            json!({"type":"reasoning", "id":"rs_1", "summary":[
                {"type":"summary_text", "text":"summary"}
            ]}),
        ] {
            item["id"] = json!("x".repeat(MAX_RESPONSES_ITEM_ID_LEN));
            let mut accepted = state();
            process_response(&mut accepted, &serde_json::to_vec(&reply(json!([item.clone()]))).unwrap())
                .unwrap();
            assert_eq!(terminal(&accepted).stop_reason, StopReason::Stop);

            item["id"] = json!("x".repeat(MAX_RESPONSES_ITEM_ID_LEN + 1));
            let mut rejected = state();
            process_response(&mut rejected, &serde_json::to_vec(&reply(json!([item]))).unwrap())
                .unwrap();
            assert_rejected_with_usage(&rejected);
            assert_eq!(rejected.pending_events.len(), 2, "reject before cloning per-part IDs");
            assert!(terminal(&rejected).content.is_empty());
        }
    }
}
