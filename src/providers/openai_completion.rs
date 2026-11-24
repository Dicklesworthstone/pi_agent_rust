//! Complete JSON replies for Chat Completions endpoints without SSE support.
//!
//! Only transport framing differs. Completed replies go through the streaming
//! provider's existing choice, reasoning, billing and tool-completion checks.

use super::{
    EventStream, MAX_READY_EVENTS_PER_YIELD, OpenAIChoice, OpenAIChunkError, OpenAIDelta,
    OpenAIFunctionDelta, OpenAIStreamChunk, OpenAIToolCallDelta, OpenAIUsage, StopReason,
    StreamEvent, StreamState, openrouter, select_primary_choice, yield_stream_turn,
};
use crate::error::{Error, Result};
use crate::http::client::Response;
use crate::sse::SseStream;
use futures::{StreamExt as _, stream};
use serde::Deserialize;

const MAX_COMPLETION_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_COMPLETION_CHOICES: usize = 256;
const MAX_COMPLETION_TOOL_CALLS: usize = 1024;

type CompletionState = StreamState<stream::Empty<std::io::Result<Vec<u8>>>>;

pub(super) fn wire_response(
    response: Response,
    model: String,
    api: String,
    provider: String,
    audio_input_in_request: bool,
    usage_observer: Option<crate::provider::UsageObserver>,
    openrouter_origin: Option<String>,
) -> EventStream {
    // Create the guard before the first consumer poll: dropping an accepted
    // audio response must fence unknown billing even if its body is never read.
    let mut state = StreamState::new(SseStream::new(stream::empty()), model, api, provider);
    state.audio_input_in_request = audio_input_in_request;
    state.usage_observer = usage_observer;
    state.openrouter_reasoning = openrouter_origin.map(openrouter::ReasoningDetails::new);
    let output = stream::unfold(Some((state, Some(response))), |active| async move {
        let (mut state, response) = active?;
        if let Some(response) = response {
            let body = match response.bytes_limited(MAX_COMPLETION_BODY_BYTES).await {
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
        let active = (!terminal).then_some((state, None));
        Some((Ok(event), active))
    });
    Box::pin(output.fuse())
}

fn process_response(state: &mut CompletionState, body: &[u8]) -> Result<()> {
    let response: CompletionResponse = serde_json::from_slice(body).map_err(|error| {
        // Neither parser diagnostics nor malformed opaque reasoning should
        // echo model output, credentials, or untrusted field values.
        Error::api(format!(
            "Invalid OpenAI completion JSON at line {}, column {}",
            error.line(),
            error.column()
        ))
    })?;
    let selected = if response.choices.len() > MAX_COMPLETION_CHOICES {
        Err(Error::api("OpenAI completion exceeds 256 choices"))
    } else {
        select_primary_choice(response.choices, |choice| choice.index)
            .map_err(|message| Error::api(format!("OpenAI completion choice error: {message}")))
            .and_then(|choice| {
                choice
                    .map(|choice| {
                        let (delta, refused) = choice.message.into_delta()?;
                        Ok((
                            OpenAIChoice {
                                index: choice.index,
                                delta,
                                finish_reason: choice.finish_reason,
                            },
                            refused,
                        ))
                    })
                    .transpose()
            })
    };
    let (choices, refused, validation_error) = match selected {
        Ok(Some((choice, refused))) => (vec![choice], refused, None),
        Ok(None) => (Vec::new(), false, None),
        Err(error) => {
            // The completed request can still be billable when its choice or
            // tool metadata is invalid. Keep its final usage and fail closed
            // before any content or executable ToolCallEnd can be published.
            (Vec::new(), false, Some(error))
        }
    };
    state.process_chunk(
        OpenAIStreamChunk {
            choices,
            usage: response.usage,
            error: response.error,
        },
        true,
    )?;
    if let Some(error) = validation_error {
        state.record_error(error.to_string());
    }
    if refused && state.finalized && state.partial.stop_reason == StopReason::Stop {
        state.partial.stop_reason = StopReason::Refusal;
    }
    state.finish_response();
    Ok(())
}

#[derive(Debug, Deserialize)]
struct CompletionResponse {
    #[serde(default)]
    choices: Vec<CompletionChoice>,
    usage: Option<OpenAIUsage>,
    error: Option<OpenAIChunkError>,
}

#[derive(Debug, Deserialize)]
struct CompletionChoice {
    index: Option<u32>,
    message: CompletionMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CompletionMessage {
    role: Option<String>,
    content: Option<String>,
    refusal: Option<String>,
    reasoning_content: Option<String>,
    reasoning: Option<serde_json::Value>,
    reasoning_text: Option<serde_json::Value>,
    reasoning_details: Option<serde_json::Value>,
    tool_calls: Option<Vec<CompletionToolCall>>,
}

impl CompletionMessage {
    fn into_delta(self) -> Result<(OpenAIDelta, bool)> {
        if self.role.as_deref().is_some_and(|role| role != "assistant") {
            return Err(Error::api(
                "OpenAI completion did not contain an assistant message",
            ));
        }
        let refusal = self.refusal.filter(|text| !text.is_empty());
        let refused = refusal.is_some();
        let tool_calls = self
            .tool_calls
            .map(|calls| {
                if calls.len() > MAX_COMPLETION_TOOL_CALLS {
                    return Err(Error::api("OpenAI completion exceeds 1024 tool calls"));
                }
                if refused && !calls.is_empty() {
                    return Err(Error::api(
                        "OpenAI completion combined a refusal with tool calls",
                    ));
                }
                calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| {
                        if call.kind != "function" {
                            return Err(Error::api(
                                "OpenAI completion returned an unsupported tool-call type",
                            ));
                        }
                        Ok(OpenAIToolCallDelta {
                            // Complete messages omit stream-only call indices;
                            // their array position owns the corresponding call.
                            index: u32::try_from(index).map_err(|_| {
                                Error::api("OpenAI completion tool-call index overflow")
                            })?,
                            id: Some(call.id),
                            function: Some(OpenAIFunctionDelta {
                                name: Some(call.function.name),
                                arguments: Some(call.function.arguments),
                            }),
                        })
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        Ok((
            OpenAIDelta {
                content: self.content.filter(|text| !text.is_empty()).or(refusal),
                reasoning_content: self.reasoning_content,
                reasoning: self.reasoning,
                reasoning_text: self.reasoning_text,
                reasoning_details: self.reasoning_details,
                tool_calls,
            },
            refused,
        ))
    }
}

#[derive(Debug, Deserialize)]
struct CompletionToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: CompletionFunction,
}

#[derive(Debug, Deserialize)]
struct CompletionFunction {
    name: String,
    arguments: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AssistantMessage, ContentBlock};
    use serde_json::{Value, json};

    fn state() -> CompletionState {
        StreamState::new(
            SseStream::new(stream::empty()),
            "test-model".to_string(),
            "openai-completions".to_string(),
            "openai".to_string(),
        )
    }

    fn reply(message: Value, finish_reason: Value) -> Value {
        json!({
            "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
            "usage": {
                "prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120,
                "prompt_tokens_details": {"cached_tokens": 25},
                "cost": 0.125
            }
        })
    }

    fn call(id: &str, arguments: &str) -> Value {
        json!({"id": id, "type": "function", "function": {
            "name": "read", "arguments": arguments
        }})
    }

    fn finish(state: &CompletionState) -> &AssistantMessage {
        match state.pending_events.back().expect("terminal event") {
            StreamEvent::Done { reason, message } => {
                assert_eq!(*reason, message.stop_reason);
                message
            }
            event => panic!("expected Done, received {event:?}"),
        }
    }

    fn tool_ends(state: &CompletionState) -> usize {
        state
            .pending_events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. }))
            .count()
    }

    #[test]
    fn completion_preserves_reasoning_parallel_tools_and_billing() {
        let mut state = state();
        let body = reply(
            json!({
                "role": "assistant",
                "reasoning": "Inspect both files.",
                "content": "Reading now.",
                "tool_calls": [
                    call("a", r#"{"path":"a.rs"}"#),
                    call("b", r#"{"path":"b.rs"}"#)
                ]
            }),
            json!("tool_calls"),
        );
        process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
        assert!(matches!(
            state.pending_events.front(),
            Some(StreamEvent::Start { .. })
        ));
        assert_eq!(tool_ends(&state), 2);
        let message = finish(&state);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert!(matches!(message.content.as_slice(), [
            ContentBlock::Thinking(thinking),
            ContentBlock::Text(text),
            ContentBlock::ToolCall(first),
            ContentBlock::ToolCall(second)
        ] if thinking.thinking == "Inspect both files."
            && text.text == "Reading now."
            && first.id == "a" && first.arguments == json!({"path": "a.rs"})
            && second.id == "b" && second.arguments == json!({"path": "b.rs"})));
        assert_eq!(message.usage.input, 75);
        assert_eq!(message.usage.cache_read, 25);
        assert_eq!(message.usage.output, 20);
        assert_eq!(message.usage.total_tokens, 120);
        assert!((message.usage.cost.total - 0.125).abs() < f64::EPSILON);
        assert!(state.complete_billing_received);
    }

    #[test]
    fn completion_selects_primary_candidate_independent_of_array_order() {
        for primary_first in [false, true] {
            let mut state = state();
            let mut body = reply(
                json!({"role": "assistant", "content": "primary"}),
                json!("stop"),
            );
            let choices = body["choices"].as_array_mut().unwrap();
            choices.push(json!({
                "index": 1,
                "message": {"role": "assistant", "tool_calls": [call("foreign", "{}")]},
                "finish_reason": "tool_calls"
            }));
            if !primary_first {
                choices.reverse();
            }
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert_eq!(tool_ends(&state), 0);
            assert!(matches!(finish(&state).content.as_slice(),
                [ContentBlock::Text(text)] if text.text == "primary"));
        }
    }

    #[test]
    fn completion_rejects_ambiguous_or_missing_primary_and_keeps_usage() {
        for indices in [
            vec![json!(0), json!(0)],
            vec![Value::Null, json!(1)],
            vec![json!(1), json!(2)],
        ] {
            let mut state = state();
            let mut body = reply(json!({"content": "untrusted"}), json!("stop"));
            body["choices"] = Value::Array(
                indices
                    .into_iter()
                    .map(|index| {
                        json!({
                            "index": index,
                            "message": {"tool_calls": [call("untrusted", "{}")]},
                            "finish_reason": "tool_calls"
                        })
                    })
                    .collect(),
            );
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            let message = finish(&state);
            assert_eq!(message.stop_reason, StopReason::Error);
            assert!(message.content.is_empty());
            assert_eq!(tool_ends(&state), 0);
            assert_eq!(message.usage.output, 20);
            assert!((message.usage.cost.total - 0.125).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn completion_never_completes_invalid_or_truncated_tool_batches() {
        let cases = [
            (
                vec![call("a", r#"{"path":"unfinished"#)],
                json!("tool_calls"),
            ),
            (
                vec![call("a", r#"{"path":"a","path":"b"}"#)],
                json!("tool_calls"),
            ),
            (vec![call("a", "[]")], json!("tool_calls")),
            (
                vec![call("same", "{}"), call("same", "{}")],
                json!("tool_calls"),
            ),
            (vec![call("", "{}")], json!("tool_calls")),
            (vec![call("a", "{}")], Value::Null),
            (vec![call("a", "{}")], json!("length")),
            (vec![call("a", "{}")], json!("content_filter")),
            (vec![call("a", "{}")], json!("unknown-finish")),
        ];
        for (calls, reason) in cases {
            let mut state = state();
            let body = reply(json!({"role": "assistant", "tool_calls": calls}), reason);
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            assert_eq!(finish(&state).stop_reason, StopReason::Error, "{body}");
            assert_eq!(tool_ends(&state), 0, "{body}");
        }
    }

    #[test]
    fn completion_message_validation_retains_billing_without_tools() {
        for message in [
            json!({"role": "user", "tool_calls": [call("a", "{}")]}),
            json!({"role": "assistant", "refusal": "Cannot do that.",
                "tool_calls": [call("a", "{}")]}),
            json!({"role": "assistant", "tool_calls": [{
                "id": "a", "type": "custom", "function": {"name": "read", "arguments": "{}"}
            }]}),
        ] {
            let mut state = state();
            let body = reply(message, json!("tool_calls"));
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            let result = finish(&state);
            assert_eq!(result.stop_reason, StopReason::Error);
            assert!(result.content.is_empty());
            assert_eq!(tool_ends(&state), 0);
            assert!((result.usage.cost.total - 0.125).abs() < f64::EPSILON);
            assert!(state.complete_billing_received);
        }
    }

    #[test]
    fn completion_refusal_requires_a_valid_terminal_reason() {
        for reason in [json!("stop"), Value::Null] {
            let mut state = state();
            let body = reply(
                json!({"role": "assistant", "content": null, "refusal": "Cannot do that."}),
                reason.clone(),
            );
            process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
            let message = finish(&state);
            assert_eq!(
                message.stop_reason,
                if reason.is_null() {
                    StopReason::Error
                } else {
                    StopReason::Refusal
                }
            );
            assert!(matches!(message.content.as_slice(),
                [ContentBlock::Text(text)] if text.text == "Cannot do that."));
            assert_eq!(tool_ends(&state), 0);
        }
    }

    #[test]
    fn completion_rejects_malformed_json_without_exposing_wire_values() {
        for body in [
            br#"{"choices":[{"message":{"refusal":{"secret":"opaque-secret"}}}]}"#.as_slice(),
            br#"{"choices":[],"choices":[],"secret":"opaque-secret"}"#.as_slice(),
            br#"{"choices":[{"message":{"content":"opaque-secret"}"#.as_slice(),
            br#"{"choices":[{"message":{"content":"valid"}}]} trailing-opaque-secret"#.as_slice(),
        ] {
            let mut state = state();
            let error = process_response(&mut state, body).unwrap_err().to_string();
            assert!(error.contains("Invalid OpenAI completion JSON"));
            assert!(!error.contains("opaque-secret"));
            assert!(state.pending_events.is_empty());
        }
    }

    #[test]
    fn completion_error_envelope_retains_reported_usage() {
        let mut state = state();
        let body = json!({
            "error": {"message": "Provider rejected the request."},
            "usage": {"prompt_tokens": 3, "completion_tokens": 0, "total_tokens": 3,
                "cost": 0.25}
        });
        process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
        let result = finish(&state);
        assert_eq!(result.stop_reason, StopReason::Error);
        assert_eq!(
            result.error_message.as_deref(),
            Some("Provider rejected the request.")
        );
        assert!((result.usage.cost.total - 0.25).abs() < f64::EPSILON);
        assert_eq!(tool_ends(&state), 0);
    }

    #[test]
    fn completion_preserves_opaque_openrouter_reasoning_for_tool_continuation() {
        let mut state = state();
        state.openrouter_reasoning = Some(openrouter::ReasoningDetails::new(
            "https://openrouter.ai".to_string(),
        ));
        let body = reply(
            json!({
                "role": "assistant",
                "reasoning_details": [
                    {"type": "reasoning.text", "text": "Inspect the file.",
                        "signature": "signed-state"},
                    {"type": "reasoning.encrypted", "data": "opaque-continuation"}
                ],
                "tool_calls": [call("a", "{}")]
            }),
            json!("tool_calls"),
        );
        process_response(&mut state, &serde_json::to_vec(&body).unwrap()).unwrap();
        let result = finish(&state);
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        let Some(ContentBlock::Thinking(thinking)) = result.content.first() else {
            panic!("missing replayable thinking block");
        };
        assert_eq!(thinking.thinking, "Inspect the file.");
        let signature = thinking
            .thinking_signature
            .as_ref()
            .expect("opaque continuation");
        assert!(signature.contains("opaque-continuation"));
        assert!(signature.contains("signed-state"));
        assert!(!thinking.thinking.contains("opaque-continuation"));
        assert_eq!(tool_ends(&state), 1);
    }
}
