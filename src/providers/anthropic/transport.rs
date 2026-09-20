//! Transport adapters sharing the Anthropic message builder and SSE state machine.
//!
//! Vertex selects the model in its URL, puts the API version in the body, and
//! uses Google authorization. It must never enter the first-party OAuth lane.

use super::{AnthropicProvider, StreamState};
use crate::agent_cx::{AgentCx, AgentHttpResponse};
use crate::error::{Error, Result};
use crate::http::client::Response;
use crate::model::{AssistantMessage, ContentBlock, StopReason, StreamEvent};
use crate::provider::{Context, Provider, StreamOptions};
use crate::sse::SseStream;
use futures::StreamExt;
use futures::stream::{self, Stream};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::pin::Pin;

const VERTEX_ANTHROPIC_VERSION: &str = "vertex-2023-10-16";
const MAX_READY_EVENTS_PER_YIELD: usize = 64;

pub(super) type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>;

/// Restore host-owned transport fields after an extension rewrites the body.
/// The endpoint, not a body field, chooses the Vertex model.
fn vertex_request(value: Value) -> std::result::Result<Value, String> {
    let mut value = super::super::validate_streamed_json_rewrite(
        value,
        &[],
        &["messages"],
        &[
            ("stream", Value::Bool(true)),
            ("anthropic_version", json!(VERTEX_ANTHROPIC_VERSION)),
        ],
    )?;
    if value
        .get("max_tokens")
        .and_then(Value::as_u64)
        .is_none_or(|n| n == 0)
    {
        return Err("Vertex Anthropic request requires a positive max_tokens".to_string());
    }
    // The shared validator guarantees an object. Do not remove anything else:
    // tool schemas, thinking, cache markers and future extension fields survive.
    if let Some(object) = value.as_object_mut() {
        object.remove("model");
    }
    Ok(value)
}

impl AnthropicProvider {
    /// Send an Anthropic Messages request over an explicitly selected Vertex
    /// endpoint. Authorization is resolved by VertexProvider; there is no
    /// Anthropic API-key/environment fallback and no Claude OAuth beta headers.
    pub(crate) async fn stream_vertex(
        &self,
        context: &Context<'_>,
        options: &StreamOptions,
        authorization: &str,
    ) -> Result<EventStream> {
        // Hooks and later stream consumers may run under other task contexts.
        // They must not replace the authority or cancellation owner of this call.
        let owner = AgentCx::for_current_or_request();
        let original = vertex_request(serde_json::to_value(self.build_request(context, options))?)
            .map_err(|message| Error::provider(self.name(), message))?;
        let rewritten = super::super::offer_before_provider_request(
            options,
            self.name(),
            "google-vertex",
            self.model_id(),
            &self.base_url,
            &original,
            vertex_request,
        )
        .await;
        let body = rewritten.as_ref().unwrap_or(&original);
        let mut request = self
            .client
            .post(&self.base_url)
            .header("Accept", "text/event-stream");
        if let Some(headers) = self
            .compat
            .as_ref()
            .and_then(|compat| compat.custom_headers.as_ref())
        {
            request = super::super::apply_headers_ignoring_blank_auth_overrides(
                request,
                headers,
                &["authorization"],
            );
        }
        request = super::super::apply_headers_ignoring_blank_auth_overrides(
            request,
            &options.headers,
            &["authorization"],
        );
        // Install the already resolved winner last, including when an empty
        // request header must not erase a non-empty compatibility override.
        let request = request.header("Authorization", authorization).json(body)?;
        let response = Box::pin(owner.http().request(request).send()).await?;
        let status = response.status();
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .unwrap_or_else(|error| format!("<failed to read body: {error}>"));
            return Err(Error::provider(
                self.name(),
                format!("Vertex AI Anthropic API error (HTTP {status}): {body}"),
            ));
        }
        Ok(wire_stream(
            response.bytes_stream(),
            self.model.clone(),
            "google-vertex".to_string(),
            self.provider.clone(),
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ContentKind {
    Text,
    Thinking,
    Tool,
}

struct ContentState {
    kind: ContentKind,
    closed: bool,
    initial_tool_input: Option<Value>,
    saw_tool_delta: bool,
}

/// Check the public event boundary, not just JSON syntax. A terminal marker
/// does not make an unfinished block or invalid tool argument object usable.
/// The underlying parser remains shared with existing fixtures and fuzzing.
#[derive(Default)]
struct StreamLifecycle {
    started: bool,
    /// Top-level deltas follow all content blocks, even when they only update usage.
    message_delta_seen: bool,
    /// A final reason is immutable: later metadata cannot turn a refusal or
    /// truncated response into permission to execute already streamed calls.
    stop_reason: Option<String>,
    blocks: Vec<ContentState>,
    tool_ids: HashSet<String>,
}

impl StreamLifecycle {
    fn open(
        &mut self,
        index: usize,
        kind: ContentKind,
        initial_tool_input: Option<Value>,
    ) -> Result<()> {
        if !self.started || index != self.blocks.len() {
            return Err(protocol_error(
                "content block start has an invalid message or index",
            ));
        }
        self.blocks.push(ContentState {
            kind,
            closed: false,
            initial_tool_input,
            saw_tool_delta: false,
        });
        Ok(())
    }

    fn active(&mut self, index: usize, kind: ContentKind) -> Result<&mut ContentState> {
        self.blocks
            .get_mut(index)
            .filter(|block| !block.closed && block.kind == kind)
            .ok_or_else(|| protocol_error("content event does not match an open block"))
    }

    /// `Ok(None)` from the shared parser is not automatically a harmless frame.
    /// Missing delta payloads, orphan stops, and invalid signatures also take
    /// that path. Inspect only these non-emitting frames, keeping ordinary text
    /// and tool-token deltas on the existing single-decode hot path.
    fn accept_silent(&mut self, raw: &str, partial: &mut AssistantMessage) -> Result<()> {
        let wire: Value = serde_json::from_str(raw)
            .map_err(|_| protocol_error("invalid non-emitting event JSON"))?;
        match wire.get("type").and_then(Value::as_str) {
            Some("ping") => Ok(()),
            Some("message_delta") => {
                if !self.started {
                    return Err(protocol_error("message_delta arrived before message_start"));
                }
                if self.blocks.iter().any(|block| !block.closed) {
                    return Err(protocol_error(
                        "message_delta left unfinished content (unexpected EOF)",
                    ));
                }
                // The shared parser already rejects unsupported stop reasons.
                // Null/omitted reasons are valid metadata updates, but cannot
                // authorize Done until an explicit reason has actually arrived.
                if let Some(reason) = wire["delta"]["stop_reason"].as_str() {
                    if self
                        .stop_reason
                        .as_deref()
                        .is_some_and(|seen| seen != reason)
                    {
                        return Err(protocol_error("conflicting final stop reasons"));
                    }
                    self.stop_reason.get_or_insert_with(|| reason.to_string());
                }
                apply_usage_update(&wire, partial)?;
                self.message_delta_seen = true;
                Ok(())
            }
            Some("content_block_delta")
                if wire["delta"]["type"].as_str() == Some("signature_delta") =>
            {
                if self.message_delta_seen {
                    return Err(protocol_error("signature arrived after message_delta"));
                }
                let index = wire
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| protocol_error("signature has an invalid block index"))?;
                self.active(index, ContentKind::Thinking)?;
                if !matches!(partial.content.get(index), Some(ContentBlock::Thinking(_)))
                    || !wire["delta"]["signature"].is_string()
                {
                    return Err(protocol_error(
                        "signature does not update an open thinking block",
                    ));
                }
                Ok(())
            }
            _ => Err(protocol_error(
                "content event did not produce its required update",
            )),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn accept(
        &mut self,
        event: &mut StreamEvent,
        raw: &str,
        partial: &mut AssistantMessage,
    ) -> Result<()> {
        if self.message_delta_seen
            && !matches!(event, StreamEvent::Done { .. } | StreamEvent::Error { .. })
        {
            return Err(protocol_error("content event arrived after message_delta"));
        }
        match event {
            StreamEvent::Start { .. } => {
                if self.started {
                    return Err(protocol_error("duplicate message_start"));
                }
                self.started = true;
            }
            StreamEvent::TextStart { content_index } => {
                self.open(*content_index, ContentKind::Text, None)?;
            }
            StreamEvent::ThinkingStart { content_index } => {
                self.open(*content_index, ContentKind::Thinking, None)?;
            }
            StreamEvent::ToolCallStart {
                content_index,
                id,
                name,
            } => {
                if id.trim().is_empty()
                    || name.trim().is_empty()
                    || !self.tool_ids.insert(id.clone())
                {
                    return Err(protocol_error(
                        "tool call has an empty or duplicate identity",
                    ));
                }
                // Only tool starts need the original input object. Token deltas
                // are decoded once, by the existing parser, with no extra parse.
                let wire: Value = serde_json::from_str(raw)
                    .map_err(|_| protocol_error("invalid tool start JSON"))?;
                let input = wire
                    .get("content_block")
                    .and_then(|block| block.get("input"))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if !input.is_object() {
                    return Err(protocol_error("initial tool input is not a JSON object"));
                }
                self.open(*content_index, ContentKind::Tool, Some(input))?;
            }
            StreamEvent::TextDelta { content_index, .. } => {
                self.active(*content_index, ContentKind::Text)?;
            }
            StreamEvent::ThinkingDelta { content_index, .. } => {
                self.active(*content_index, ContentKind::Thinking)?;
                if !matches!(
                    partial.content.get(*content_index),
                    Some(ContentBlock::Thinking(_))
                ) {
                    return Err(protocol_error("thinking delta targets opaque content"));
                }
            }
            StreamEvent::ToolCallDelta { content_index, .. } => {
                self.active(*content_index, ContentKind::Tool)?
                    .saw_tool_delta = true;
            }
            StreamEvent::TextEnd { content_index, .. } => {
                self.active(*content_index, ContentKind::Text)?.closed = true;
            }
            StreamEvent::ThinkingEnd { content_index, .. } => {
                self.active(*content_index, ContentKind::Thinking)?.closed = true;
            }
            StreamEvent::ToolCallEnd {
                content_index,
                tool_call,
            } => {
                let block = self.active(*content_index, ContentKind::Tool)?;
                if !block.saw_tool_delta {
                    // A zero-argument call can close without any JSON deltas.
                    // Preserve an initial object rather than treating the empty
                    // accumulator as malformed JSON or inventing null arguments.
                    tool_call.arguments = block
                        .initial_tool_input
                        .take()
                        .ok_or_else(|| protocol_error("tool call lost its initial input"))?;
                }
                if !tool_call.arguments.is_object() {
                    return Err(protocol_error("tool input is not a complete JSON object"));
                }
                let Some(ContentBlock::ToolCall(stored)) = partial.content.get_mut(*content_index)
                else {
                    return Err(protocol_error(
                        "tool call does not match accumulated content",
                    ));
                };
                if stored.id != tool_call.id || stored.name != tool_call.name {
                    return Err(protocol_error(
                        "tool call identity changed during streaming",
                    ));
                }
                stored.arguments.clone_from(&tool_call.arguments);
                block.closed = true;
            }
            StreamEvent::Done { message, .. } => {
                if !self.started {
                    return Err(protocol_error("message_stop arrived before message_start"));
                }
                if self.blocks.iter().any(|block| !block.closed) {
                    return Err(protocol_error(
                        "message_stop left unfinished content (unexpected EOF)",
                    ));
                }
                if self.stop_reason.is_none() {
                    return Err(protocol_error(
                        "message_stop arrived without a final stop reason (unexpected EOF)",
                    ));
                }
                if message.content.len() != self.blocks.len() {
                    return Err(protocol_error(
                        "completed message does not match streamed content",
                    ));
                }
            }
            // Provider errors are valid terminal events even before message_start.
            StreamEvent::Error { .. } => {}
        }
        Ok(())
    }
}

fn protocol_error(message: &str) -> Error {
    Error::api(format!("Anthropic stream protocol error: {message}"))
}

/// A closed client tool block is content, not execution authority. Require the
/// provider's explicit tool_use outcome before giving it to the shared agent,
/// which also accepts calls attached to ordinary Stop/Length/Refusal messages.
/// Text-only refusal, truncation and pause outcomes retain their public meaning.
/// https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons
fn authorize_terminal(event: StreamEvent) -> StreamEvent {
    let StreamEvent::Done {
        reason,
        mut message,
    } = event
    else {
        return event;
    };
    let has_calls = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolCall(_)));
    let failure = match reason {
        StopReason::ToolUse if !has_calls => {
            Some("Anthropic tool_use completion has no client tool calls")
        }
        StopReason::Stop if has_calls => {
            Some("Anthropic ended without tool_use authorization; local tool calls withheld")
        }
        StopReason::Length if has_calls => Some(
            "Anthropic response was truncated before tool_use authorization; local tool calls withheld",
        ),
        StopReason::Refusal if has_calls => {
            Some("Anthropic refused the response; local tool calls withheld")
        }
        StopReason::PauseTurn if has_calls => Some(
            "Anthropic pause_turn cannot authorize client tool calls; local tool calls withheld",
        ),
        _ => None,
    };
    if let Some(failure) = failure {
        // Keep content, stop_details, usage and provider identity for diagnostics.
        // Error is the shared agent's non-execution path; merely changing the
        // outer event variant while retaining the old message reason is unsafe.
        message.stop_reason = StopReason::Error;
        message.error_message = Some(failure.to_string());
        StreamEvent::Error {
            reason: StopReason::Error,
            error: message,
        }
    } else {
        StreamEvent::Done { reason, message }
    }
}

/// Complete the usage update after the shared parser applies output_tokens.
/// Input/cache counts can be corrected in message_delta; like output_tokens,
/// they are cumulative snapshots, not increments. Missing/null optional fields
/// retain the last known count, whereas an explicit zero replaces it.
fn apply_usage_update(wire: &Value, partial: &mut AssistantMessage) -> Result<()> {
    let Some(usage) = wire.get("usage").filter(|usage| !usage.is_null()) else {
        return Ok(());
    };
    let usage = usage
        .as_object()
        .ok_or_else(|| protocol_error("message_delta usage is not an object"))?;
    let counter = |field: &str| -> Result<Option<u64>> {
        match usage.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                protocol_error(&format!(
                    "message_delta usage.{field} is not an unsigned integer"
                ))
            }),
        }
    };
    // Validate every optional counter before changing any of them. Otherwise an
    // invalid late field could leave a partially accepted accounting snapshot.
    let input = counter("input_tokens")?;
    let cache_read = counter("cache_read_input_tokens")?;
    let cache_write = counter("cache_creation_input_tokens")?;
    if let Some(input) = input {
        partial.usage.input = input;
    }
    if let Some(cache_read) = cache_read {
        partial.usage.cache_read = cache_read;
    }
    if let Some(cache_write) = cache_write {
        partial.usage.cache_write = cache_write;
    }
    partial.usage.total_tokens = partial
        .usage
        .input
        .saturating_add(partial.usage.output)
        .saturating_add(partial.usage.cache_read)
        .saturating_add(partial.usage.cache_write);
    Ok(())
}

/// Bind the already-dispatched native Anthropic response to its current owner.
/// This scopes body reads and cleanup, not the request dispatch that preceded it.
/// Vertex uses the same driver with a response scoped before dispatch instead.
pub(super) fn response_stream(
    response: Response,
    model: String,
    api: String,
    provider: String,
) -> EventStream {
    let response = AgentHttpResponse::from_response(AgentCx::for_current_or_request(), response);
    wire_stream(response.bytes_stream(), model, api, provider)
}

/// Metadata and heartbeat frames do not yield a public event. Even if the
/// transport is continuously ready, periodically return control to the executor
/// so abort/select futures and other sessions can be polled. No timer or task
/// is detached from the consumer's stream lifetime.
async fn yield_stream_turn() {
    let mut yielded = false;
    std::future::poll_fn(|task| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            task.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

fn wire_stream<S>(source: S, model: String, api: String, provider: String) -> EventStream
where
    S: Stream<Item = std::io::Result<Vec<u8>>> + Unpin + Send + 'static,
{
    let state = StreamState::new(SseStream::new(source), model, api, provider);
    let output = stream::unfold(
        Some((state, StreamLifecycle::default())),
        |active| async move {
            let (mut state, mut lifecycle) = active?;
            let mut ready_events = 0;
            loop {
                if ready_events == MAX_READY_EVENTS_PER_YIELD {
                    yield_stream_turn().await;
                    ready_events = 0;
                }
                ready_events += 1;
                match state.event_source.next().await {
                    Some(Ok(msg)) => {
                        state.transient_error_count = 0;
                        if msg.event == "ping" {
                            continue;
                        }
                        match state.process_event(&msg.data) {
                            Ok(Some(mut event)) => {
                                if let Err(error) =
                                    lifecycle.accept(&mut event, &msg.data, &mut state.partial)
                                {
                                    return Some((Err(error), None));
                                }
                                let event = authorize_terminal(event);
                                state.done = matches!(
                                    &event,
                                    StreamEvent::Done { .. } | StreamEvent::Error { .. }
                                );
                                // then_some drops the entire state on a terminal
                                // event BEFORE returning it. Retaining the outer
                                // stream must not retain a socket, owner waiter,
                                // buffered tail or partial tool accumulators.
                                let active = (!state.done).then_some((state, lifecycle));
                                return Some((Ok(event), active));
                            }
                            Ok(None) => {
                                if let Err(error) =
                                    lifecycle.accept_silent(&msg.data, &mut state.partial)
                                {
                                    return Some((Err(error), None));
                                }
                            }
                            Err(error) => {
                                return Some((Err(error), None));
                            }
                        }
                    }
                    Some(Err(error)) => {
                        const MAX_CONSECUTIVE_TRANSIENT_ERRORS: usize = 5;
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WriteZero
                                | std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                        ) {
                            state.transient_error_count += 1;
                            if state.transient_error_count <= MAX_CONSECUTIVE_TRANSIENT_ERRORS {
                                tracing::warn!(
                                    kind = ?error.kind(),
                                    count = state.transient_error_count,
                                    "Transient error in SSE stream, continuing"
                                );
                                continue;
                            }
                        }
                        return Some((Err(Error::sse(&error)), None));
                    }
                    None => {
                        return Some((
                            Err(Error::api(
                                "Anthropic stream ended before message_stop (unexpected EOF)",
                            )),
                            None,
                        ));
                    }
                }
            }
        },
    );
    Box::pin(output.fuse())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::StopReason;
    use asupersync::runtime::RuntimeBuilder;
    use futures::FutureExt as _;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context as TaskContext, Poll, Wake, Waker};

    #[test]
    fn vertex_wire_format_keeps_messages_tools_thinking_and_cache() {
        let original = json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 16000,
            "stream": false,
            "anthropic_version": "wrong-version",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hello", "cache_control": {"type": "ephemeral"}}]}],
            "tools": [{"name": "read", "input_schema": {"type": "object"}}],
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "metadata": {"user_id": "test"}
        });
        let body = vertex_request(original.clone()).unwrap();
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], VERTEX_ANTHROPIC_VERSION);
        assert_eq!(body["stream"], true);
        for key in [
            "messages",
            "tools",
            "thinking",
            "output_config",
            "metadata",
            "max_tokens",
        ] {
            assert_eq!(body[key], original[key], "{key}");
        }
    }

    #[test]
    fn malformed_vertex_rewrites_are_rejected_for_fail_open_fallback() {
        for body in [
            json!(null),
            json!([]),
            json!({"messages": "wrong", "max_tokens": 1}),
            json!({"messages": [], "max_tokens": 0}),
            json!({"messages": [], "max_tokens": -1}),
            json!({"messages": [], "max_tokens": "100"}),
            json!({"messages": []}),
        ] {
            assert!(vertex_request(body).is_err());
        }
    }

    fn collect_wire(events: impl IntoIterator<Item = Value>) -> Vec<Result<StreamEvent>> {
        collect_wire_for_api(events, "anthropic-messages", "anthropic")
    }

    fn collect_wire_for_api(
        events: impl IntoIterator<Item = Value>,
        api: &str,
        provider: &str,
    ) -> Vec<Result<StreamEvent>> {
        let chunks: Vec<_> = events
            .into_iter()
            .map(|event| Ok::<_, std::io::Error>(format!("data: {event}\n\n").into_bytes()))
            .collect();
        RuntimeBuilder::current_thread()
            .build()
            .expect("runtime")
            .block_on(
                wire_stream(
                    stream::iter(chunks),
                    "claude-test".to_string(),
                    api.to_string(),
                    provider.to_string(),
                )
                .collect(),
            )
    }

    fn start() -> Value {
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}})
    }

    fn finish() -> [Value; 2] {
        [
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ]
    }

    fn tool_start(index: usize, id: &str, input: &Value) -> Value {
        json!({"type": "content_block_start", "index": index, "content_block": {
            "type": "tool_use", "id": id, "name": "read", "input": input
        }})
    }

    fn tool_delta(index: usize, json: &str) -> Value {
        json!({"type": "content_block_delta", "index": index, "delta": {
            "type": "input_json_delta", "partial_json": json
        }})
    }

    fn stop(index: usize) -> Value {
        json!({"type": "content_block_stop", "index": index})
    }

    fn assert_terminal_error(events: &[Result<StreamEvent>]) {
        assert_eq!(
            events.iter().filter(|event| event.is_err()).count(),
            1,
            "{events:?}"
        );
        assert!(events.last().is_some_and(Result::is_err), "{events:?}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Done { .. }))),
            "{events:?}"
        );
    }

    #[test]
    fn zero_argument_calls_are_objects_in_events_and_final_message() {
        let events = collect_wire(
            [start(), tool_start(0, "call-a", &json!({})), stop(0)]
                .into_iter()
                .chain(finish()),
        );
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let call = events
            .iter()
            .find_map(|event| match event {
                Ok(StreamEvent::ToolCallEnd { tool_call, .. }) => Some(tool_call),
                _ => None,
            })
            .expect("completed tool");
        assert_eq!(call.arguments, json!({}));
        let Some(Ok(StreamEvent::Done { reason, message })) = events.last() else {
            panic!("expected Done");
        };
        assert_eq!(*reason, StopReason::ToolUse);
        let ContentBlock::ToolCall(stored) = &message.content[0] else {
            panic!("expected stored call");
        };
        assert_eq!(stored.arguments, call.arguments);
    }

    #[test]
    fn initial_input_is_preserved_when_no_argument_deltas_arrive() {
        let input = json!({"path": "initial.txt"});
        let events = collect_wire(
            [start(), tool_start(0, "call-a", &input), stop(0)]
                .into_iter()
                .chain(finish()),
        );
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let Some(Ok(StreamEvent::Done { message, .. })) = events.last() else {
            panic!("expected Done");
        };
        let ContentBlock::ToolCall(stored) = &message.content[0] else {
            panic!("expected stored call");
        };
        assert_eq!(stored.arguments, input);
    }

    #[test]
    fn interleaved_calls_keep_initial_and_streamed_inputs_separate() {
        let events = collect_wire(
            [
                start(),
                tool_start(0, "call-a", &json!({"path": "initial.txt"})),
                tool_start(1, "call-b", &json!({})),
                tool_delta(1, "{\"path\":"),
                stop(0),
                tool_delta(1, "\"streamed.txt\"}"),
                stop(1),
            ]
            .into_iter()
            .chain(finish()),
        );
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let calls: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Ok(StreamEvent::ToolCallEnd { tool_call, .. }) => Some(tool_call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call-a");
        assert_eq!(calls[0].arguments, json!({"path": "initial.txt"}));
        assert_eq!(calls[1].id, "call-b");
        assert_eq!(calls[1].arguments, json!({"path": "streamed.txt"}));
    }

    #[test]
    fn incomplete_or_non_object_arguments_never_emit_completed_calls() {
        for data in ["{\"path\":", "{bad}", "null", "[]", "42", "\"text\"", ""] {
            let events = collect_wire(
                [
                    start(),
                    tool_start(0, "call-a", &json!({})),
                    tool_delta(0, data),
                    stop(0),
                ]
                .into_iter()
                .chain(finish()),
            );
            assert_terminal_error(&events);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(StreamEvent::ToolCallEnd { .. })))
            );
        }
    }

    #[test]
    fn malformed_initial_tool_input_is_not_replaced_with_empty_arguments() {
        for input in [json!(null), json!([]), json!("not-an-object")] {
            let events = collect_wire(
                [start(), tool_start(0, "call-a", &input), stop(0)]
                    .into_iter()
                    .chain(finish()),
            );
            assert_terminal_error(&events);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(StreamEvent::ToolCallEnd { .. })))
            );
        }
    }

    #[test]
    fn message_stop_cannot_complete_unclosed_text_thinking_or_tool_blocks() {
        for kind in ["text", "thinking", "tool_use"] {
            let events = collect_wire(
                [
                    start(),
                    json!({"type": "content_block_start", "index": 0, "content_block": {
                        "type": kind, "id": "call-a", "name": "read", "input": {}
                    }}),
                ]
                .into_iter()
                .chain(finish()),
            );
            assert_terminal_error(&events);
            assert!(
                events
                    .last()
                    .unwrap()
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("unfinished content")
            );
        }
    }

    #[test]
    fn wrong_delta_kind_and_deltas_after_block_close_are_terminal() {
        for wrong_kind in [true, false] {
            let mut input = vec![
                start(),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            ];
            if wrong_kind {
                input.push(json!({"type": "content_block_delta", "index": 0, "delta": {
                    "type": "thinking_delta", "thinking": "wrong block"
                }}));
            } else {
                input.push(stop(0));
                input.push(json!({"type": "content_block_delta", "index": 0, "delta": {
                    "type": "text_delta", "text": "late text"
                }}));
            }
            input.extend(finish());
            assert_terminal_error(&collect_wire(input));
        }
    }

    #[test]
    fn sparse_and_duplicate_block_indices_are_rejected_without_padding() {
        for index in [0, 2, u32::MAX] {
            let events = collect_wire([
                start(),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
                json!({"type": "content_block_start", "index": index, "content_block": {"type": "text"}}),
            ].into_iter().chain(finish()));
            assert_terminal_error(&events);
        }
    }

    #[test]
    fn duplicate_tool_ids_and_empty_tool_identities_are_rejected() {
        for id in ["", "  ", "call-a"] {
            let events = collect_wire(
                [
                    start(),
                    tool_start(0, "call-a", &json!({})),
                    stop(0),
                    tool_start(1, id, &json!({})),
                    stop(1),
                ]
                .into_iter()
                .chain(finish()),
            );
            assert_terminal_error(&events);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Ok(StreamEvent::ToolCallEnd { .. })))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn message_and_content_events_require_a_single_message_start() {
        for input in [
            vec![json!({"type": "message_stop"})],
            vec![start(), start(), json!({"type": "message_stop"})],
            vec![
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "orphan"}}),
            ],
        ] {
            assert_terminal_error(&collect_wire(input));
        }
    }

    #[test]
    fn signature_only_and_redacted_thinking_blocks_remain_valid() {
        let events = collect_wire([
            start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "c2ln"}}),
            stop(0),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "b3BhcXVl"}}),
            stop(1),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ]);
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let Some(Ok(StreamEvent::Done { message, .. })) = events.last() else {
            panic!("expected Done");
        };
        let ContentBlock::Thinking(thinking) = &message.content[0] else {
            panic!("expected thinking");
        };
        assert!(thinking.thinking.is_empty());
        assert_eq!(thinking.thinking_signature.as_deref(), Some("c2ln"));
        let ContentBlock::RedactedThinking(redacted) = &message.content[1] else {
            panic!("expected opaque thinking");
        };
        assert_eq!(redacted.data, "b3BhcXVl");
    }

    #[test]
    fn provider_errors_before_message_start_remain_terminal_provider_errors() {
        let events = collect_wire([
            json!({"type": "error", "error": {"message": "overloaded"}}),
            start(),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(events.len(), 1);
        let Ok(StreamEvent::Error { error, .. }) = &events[0] else {
            panic!("expected provider Error");
        };
        assert_eq!(error.error_message.as_deref(), Some("overloaded"));
    }

    #[test]
    fn missing_or_null_delta_payloads_never_become_completed_content() {
        for (kind, delta_kind, field) in [
            ("text", "text_delta", "text"),
            ("thinking", "thinking_delta", "thinking"),
            ("tool_use", "input_json_delta", "partial_json"),
        ] {
            for payload in [None, Some(Value::Null)] {
                let mut delta = json!({"type": delta_kind});
                if let Some(payload) = payload {
                    delta
                        .as_object_mut()
                        .unwrap()
                        .insert(field.to_string(), payload);
                }
                let events = collect_wire(
                    [
                        start(),
                        json!({"type": "content_block_start", "index": 0, "content_block": {
                            "type": kind, "id": "call-a", "name": "read", "input": {}
                        }}),
                        json!({"type": "content_block_delta", "index": 0, "delta": delta}),
                        stop(0),
                    ]
                    .into_iter()
                    .chain(finish()),
                );
                assert_terminal_error(&events);
                assert!(
                    !events.iter().any(|event| matches!(
                        event,
                        Ok(StreamEvent::TextEnd { .. }
                            | StreamEvent::ThinkingEnd { .. }
                            | StreamEvent::ToolCallEnd { .. })
                    )),
                    "{kind}: {events:?}"
                );
            }
        }
    }

    #[test]
    fn orphan_and_duplicate_tool_stops_are_not_silently_discarded() {
        for input in [
            vec![start(), stop(0)],
            vec![start(), tool_start(0, "call-a", &json!({})), stop(9)],
            vec![
                start(),
                tool_start(0, "call-a", &json!({})),
                stop(0),
                stop(0),
            ],
        ] {
            assert_terminal_error(&collect_wire(input.into_iter().chain(finish())));
        }
    }

    #[test]
    fn signatures_cannot_target_non_thinking_or_closed_blocks() {
        for kind in ["text", "tool_use", "redacted_thinking", "thinking"] {
            let mut input = vec![
                start(),
                json!({"type": "content_block_start", "index": 0, "content_block": {
                    "type": kind, "id": "call-a", "name": "read", "input": {}, "data": "opaque"
                }}),
            ];
            if kind == "thinking" {
                input.push(stop(0));
            }
            input.push(json!({"type": "content_block_delta", "index": 0, "delta": {
                "type": "signature_delta", "signature": "signature"
            }}));
            input.extend(finish());
            assert_terminal_error(&collect_wire(input));
        }
    }

    #[test]
    fn missing_signatures_and_orphan_signatures_are_terminal() {
        for delta in [
            json!({"type": "signature_delta"}),
            json!({"type": "signature_delta", "signature": null}),
        ] {
            let events = collect_wire([
                start(),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking"}}),
                json!({"type": "content_block_delta", "index": 0, "delta": delta}),
                stop(0),
            ].into_iter().chain(finish()));
            assert_terminal_error(&events);
        }
        assert_terminal_error(&collect_wire(
            [
                start(),
                json!({"type": "content_block_delta", "index": 9, "delta": {
                    "type": "signature_delta", "signature": "orphan"
                }}),
            ]
            .into_iter()
            .chain(finish()),
        ));
    }

    #[test]
    fn redacted_thinking_cannot_silently_discard_thinking_text() {
        assert_terminal_error(&collect_wire(
            [
                start(),
                json!({"type": "content_block_start", "index": 0, "content_block": {
                    "type": "redacted_thinking", "data": "opaque"
                }}),
                json!({"type": "content_block_delta", "index": 0, "delta": {
                    "type": "thinking_delta", "thinking": "must not disappear"
                }}),
                stop(0),
            ]
            .into_iter()
            .chain(finish()),
        ));
    }

    #[test]
    fn message_stop_requires_an_explicit_completion_reason() {
        for mut input in [
            vec![start()],
            vec![start(), json!({"type": "message_delta", "delta": {}})],
            vec![
                start(),
                json!({"type": "message_delta", "delta": {"stop_reason": null}}),
            ],
        ] {
            input.push(json!({"type": "message_stop"}));
            let events = collect_wire(input);
            assert_terminal_error(&events);
            assert!(
                events
                    .last()
                    .unwrap()
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("without a final stop reason")
            );
        }
    }

    #[test]
    fn top_level_deltas_require_a_started_message_and_finish_content() {
        assert_terminal_error(&collect_wire(finish()));
        for reason in [Value::Null, json!("end_turn")] {
            let events = collect_wire(
                [
                    start(),
                    json!({"type": "message_delta", "delta": {"stop_reason": reason}}),
                    tool_start(0, "too-late", &json!({})),
                    stop(0),
                ]
                .into_iter()
                .chain(finish()),
            );
            assert_terminal_error(&events);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(StreamEvent::ToolCallStart { .. })))
            );
        }
    }

    #[test]
    fn empty_text_deltas_and_multiple_metadata_updates_remain_valid() {
        let events = collect_wire([
            json!({"type": "ping"}),
            start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "retained"}}),
            stop(0),
            json!({"type": "message_delta", "delta": {}, "usage": {"output_tokens": 1}}),
            json!({"type": "ping"}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ]);
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(StreamEvent::Done { .. })))
                .count(),
            1
        );
        let Some(Ok(StreamEvent::Done { reason, message })) = events.last() else {
            panic!("expected Done");
        };
        assert_eq!(*reason, StopReason::Stop);
        assert_eq!(message.usage.output, 2);
        let ContentBlock::Text(text) = &message.content[0] else {
            panic!("expected text");
        };
        assert_eq!(text.text, "retained");
    }

    #[test]
    fn native_and_vertex_streams_keep_cumulative_usage_and_tool_content() {
        for (api, provider) in [
            ("anthropic-messages", "anthropic"),
            ("google-vertex", "google-vertex"),
        ] {
            let events = collect_wire_for_api(
                [
                    json!({"type": "message_start", "message": {"usage": {
                        "input_tokens": 10,
                        "cache_read_input_tokens": 20,
                        "cache_creation_input_tokens": 30
                    }}}),
                    tool_start(0, "call-a", &json!({})),
                    tool_delta(0, "{\"path\":\"kept.txt\"}"),
                    stop(0),
                    json!({"type": "message_delta", "delta": {}, "usage": {
                        "input_tokens": 100,
                        "cache_read_input_tokens": 200,
                        "cache_creation_input_tokens": 300,
                        "output_tokens": 4
                    }}),
                    json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {
                        "input_tokens": 110,
                        "output_tokens": 5
                    }}),
                    json!({"type": "message_stop"}),
                ],
                api,
                provider,
            );
            assert!(events.iter().all(Result::is_ok), "{api}: {events:?}");
            let Some(Ok(StreamEvent::Done { reason, message })) = events.last() else {
                panic!("expected Done for {api}");
            };
            assert_eq!(*reason, StopReason::ToolUse);
            assert_eq!(message.api, api);
            assert_eq!(message.provider, provider);
            assert_eq!(message.usage.input, 110);
            assert_eq!(message.usage.output, 5);
            assert_eq!(message.usage.cache_read, 200);
            assert_eq!(message.usage.cache_write, 300);
            assert_eq!(message.usage.total_tokens, 615);
            let ContentBlock::ToolCall(call) = &message.content[0] else {
                panic!("expected retained tool call");
            };
            assert_eq!(call.id, "call-a");
            assert_eq!(call.arguments, json!({"path": "kept.txt"}));
        }
    }

    #[test]
    fn omitted_and_null_usage_fields_preserve_previous_counts() {
        let events = collect_wire([
            json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 20,
                "cache_creation_input_tokens": 30
            }}}),
            json!({"type": "message_delta", "delta": {}, "usage": {
                "output_tokens": 7,
                "input_tokens": null,
                "cache_read_input_tokens": null,
                "cache_creation_input_tokens": null
            }}),
            json!({"type": "message_delta", "delta": {}, "usage": {"output_tokens": 7}}),
            json!({"type": "message_delta", "delta": {}, "usage": null}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ]);
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let Some(Ok(StreamEvent::Done { message, .. })) = events.last() else {
            panic!("expected Done");
        };
        assert_eq!(message.usage.input, 10);
        assert_eq!(message.usage.cache_read, 20);
        assert_eq!(message.usage.cache_write, 30);
        assert_eq!(message.usage.output, 7);
        assert_eq!(message.usage.total_tokens, 67);
    }

    #[test]
    fn explicit_zero_resets_only_the_supplied_usage_counter() {
        for (field, expected) in [
            ("input_tokens", (0, 20, 30, 57)),
            ("cache_read_input_tokens", (10, 0, 30, 47)),
            ("cache_creation_input_tokens", (10, 20, 0, 37)),
        ] {
            let mut usage = json!({
                "output_tokens": 7,
                "input_tokens": null,
                "cache_read_input_tokens": null,
                "cache_creation_input_tokens": null
            });
            usage
                .as_object_mut()
                .unwrap()
                .insert(field.to_string(), json!(0));
            let events = collect_wire([
                json!({"type": "message_start", "message": {"usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 30
                }}}),
                json!({"type": "message_delta", "delta": {}, "usage": usage}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}}),
                json!({"type": "message_stop"}),
            ]);
            assert!(events.iter().all(Result::is_ok), "{field}: {events:?}");
            let Some(Ok(StreamEvent::Done { message, .. })) = events.last() else {
                panic!("expected Done for {field}");
            };
            assert_eq!(
                (
                    message.usage.input,
                    message.usage.cache_read,
                    message.usage.cache_write,
                    message.usage.total_tokens,
                ),
                expected,
                "{field}"
            );
        }
    }

    #[test]
    fn late_usage_corrections_saturate_total_tokens() {
        let events = collect_wire([
            start(),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {
                "input_tokens": u64::MAX,
                "cache_read_input_tokens": u64::MAX,
                "cache_creation_input_tokens": 1,
                "output_tokens": 1
            }}),
            json!({"type": "message_stop"}),
        ]);
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let Some(Ok(StreamEvent::Done { message, .. })) = events.last() else {
            panic!("expected Done");
        };
        assert_eq!(message.usage.input, u64::MAX);
        assert_eq!(message.usage.cache_read, u64::MAX);
        assert_eq!(message.usage.cache_write, 1);
        assert_eq!(message.usage.total_tokens, u64::MAX);
    }

    #[test]
    fn malformed_optional_usage_counters_never_publish_done() {
        for field in [
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            for invalid in [json!(-1), json!(1.5), json!("7"), json!([]), json!({})] {
                let mut usage = json!({"output_tokens": 1});
                usage
                    .as_object_mut()
                    .unwrap()
                    .insert(field.to_string(), invalid);
                let events = collect_wire([
                    start(),
                    json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": usage}),
                    json!({"type": "message_stop"}),
                ]);
                assert_terminal_error(&events);
                assert!(
                    events
                        .last()
                        .unwrap()
                        .as_ref()
                        .unwrap_err()
                        .to_string()
                        .contains(field),
                    "{field}: {events:?}"
                );
            }
        }
    }

    #[test]
    fn invalid_late_counter_does_not_partially_apply_optional_usage() {
        let mut partial = AssistantMessage::default();
        partial.usage.input = 7;
        partial.usage.cache_read = 11;
        partial.usage.cache_write = 13;
        partial.usage.output = 5;
        partial.usage.total_tokens = 36;
        let wire = json!({"usage": {
            "input_tokens": 100,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": "invalid"
        }});
        assert!(apply_usage_update(&wire, &mut partial).is_err());
        assert_eq!(partial.usage.input, 7);
        assert_eq!(partial.usage.cache_read, 11);
        assert_eq!(partial.usage.cache_write, 13);
        assert_eq!(partial.usage.output, 5);
        assert_eq!(partial.usage.total_tokens, 36);
    }

    #[test]
    fn provider_error_retains_preceding_usage_corrections() {
        let events = collect_wire([
            start(),
            json!({"type": "message_delta", "delta": {}, "usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 20,
                "cache_creation_input_tokens": 30,
                "output_tokens": 7
            }}),
            json!({"type": "error", "error": {"message": "overloaded"}}),
            json!({"type": "message_stop"}),
        ]);
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let Some(Ok(StreamEvent::Error { error, .. })) = events.last() else {
            panic!("expected terminal provider Error");
        };
        assert_eq!(error.error_message.as_deref(), Some("overloaded"));
        assert_eq!(error.usage.input, 10);
        assert_eq!(error.usage.cache_read, 20);
        assert_eq!(error.usage.cache_write, 30);
        assert_eq!(error.usage.output, 7);
        assert_eq!(error.usage.total_tokens, 67);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Done { .. })))
        );
    }

    fn final_delta(reason: &str) -> Value {
        json!({"type": "message_delta", "delta": {
            "stop_reason": reason,
            "stop_details": if reason == "refusal" {
                json!({"type": "refusal", "category": "fixture", "explanation": "retained"})
            } else {
                Value::Null
            }
        }, "usage": {
            "input_tokens": 10, "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 30, "output_tokens": 7
        }})
    }

    fn terminal_failure(events: &[Result<StreamEvent>]) -> &AssistantMessage {
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Done { .. })))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Ok(StreamEvent::Error { .. })))
                .count(),
            1
        );
        let Some(Ok(StreamEvent::Error { reason, error })) = events.last() else {
            panic!("expected terminal Error: {events:?}");
        };
        assert_eq!(*reason, StopReason::Error);
        assert_eq!(error.stop_reason, StopReason::Error);
        error
    }

    #[test]
    fn native_and_vertex_client_calls_require_explicit_terminal_authorization() {
        for (api, provider) in [
            ("anthropic-messages", "anthropic"),
            ("google-vertex", "google-vertex"),
        ] {
            for reason in [
                "end_turn",
                "stop_sequence",
                "max_tokens",
                "model_context_window_exceeded",
                "refusal",
                "pause_turn",
            ] {
                let events = collect_wire_for_api(
                    [
                        start(),
                        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
                        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "retained"}}),
                        stop(0),
                        tool_start(1, "call-a", &json!({"path": "must-not-run.txt"})),
                        stop(1),
                        final_delta(reason),
                        json!({"type": "message_stop"}),
                    ],
                    api,
                    provider,
                );
                // ToolCallEnd is still a content event, not permission to run.
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, Ok(StreamEvent::ToolCallEnd { .. })))
                        .count(),
                    1
                );
                let error = terminal_failure(&events);
                assert!(
                    error
                        .error_message
                        .as_deref()
                        .unwrap()
                        .contains("local tool calls withheld")
                );
                assert_eq!(error.api, api);
                assert_eq!(error.provider, provider);
                assert_eq!(error.model, "claude-test");
                assert_eq!(error.usage.total_tokens, 67);
                assert_eq!(error.content.len(), 2);
                let ContentBlock::Text(text) = &error.content[0] else {
                    panic!("expected retained text");
                };
                assert_eq!(text.text, "retained");
                let ContentBlock::ToolCall(call) = &error.content[1] else {
                    panic!("expected diagnostic tool content");
                };
                assert_eq!(call.id, "call-a");
                assert_eq!(call.arguments, json!({"path": "must-not-run.txt"}));
                if reason == "refusal" {
                    let details = error.stop_details.as_ref().expect("refusal details");
                    assert_eq!(details.kind, "refusal");
                    assert_eq!(details.category.as_deref(), Some("fixture"));
                    assert_eq!(details.explanation.as_deref(), Some("retained"));
                }
            }
        }
    }

    #[test]
    fn text_only_outcomes_keep_refusal_pause_and_truncation_semantics() {
        for (reason, expected) in [
            ("end_turn", StopReason::Stop),
            ("stop_sequence", StopReason::Stop),
            ("max_tokens", StopReason::Length),
            ("model_context_window_exceeded", StopReason::Length),
            ("refusal", StopReason::Refusal),
            ("pause_turn", StopReason::PauseTurn),
        ] {
            let events = collect_wire([
                start(),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "retained"}}),
                stop(0),
                final_delta(reason),
                json!({"type": "message_stop"}),
            ]);
            assert!(events.iter().all(Result::is_ok), "{reason}: {events:?}");
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(StreamEvent::Error { .. })))
            );
            let Some(Ok(StreamEvent::Done { reason, message })) = events.last() else {
                panic!("expected a coherent Done");
            };
            assert_eq!(*reason, expected);
            assert_eq!(message.stop_reason, expected);
            assert!(message.error_message.is_none());
            assert_eq!(message.usage.total_tokens, 67);
            if expected == StopReason::Refusal {
                assert_eq!(
                    message.stop_details.as_ref().unwrap().category.as_deref(),
                    Some("fixture")
                );
            }
        }
    }

    #[test]
    fn tool_use_cannot_complete_without_client_calls() {
        for with_text in [false, true] {
            let mut input = vec![start()];
            if with_text {
                input.extend([
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
                    stop(0),
                ]);
            }
            input.extend([final_delta("tool_use"), json!({"type": "message_stop"})]);
            let events = collect_wire(input);
            let error = terminal_failure(&events);
            assert!(
                error
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains("no client tool calls")
            );
            assert_eq!(error.usage.total_tokens, 67);
        }
    }

    #[test]
    fn conflicting_stop_reasons_never_reauthorize_a_streamed_call() {
        let reasons = [
            "tool_use",
            "end_turn",
            "stop_sequence",
            "max_tokens",
            "model_context_window_exceeded",
            "refusal",
            "pause_turn",
        ];
        for first in reasons {
            for second in reasons {
                if first == second {
                    continue;
                }
                let events = collect_wire([
                    start(),
                    tool_start(0, "call-a", &json!({})),
                    stop(0),
                    final_delta(first),
                    final_delta(second),
                    json!({"type": "message_stop"}),
                ]);
                assert_terminal_error(&events);
                assert!(
                    events
                        .last()
                        .unwrap()
                        .as_ref()
                        .unwrap_err()
                        .to_string()
                        .contains("conflicting final stop reasons"),
                    "{first} -> {second}"
                );
            }
        }
    }

    #[test]
    fn repeated_or_null_metadata_cannot_erase_valid_tool_authorization() {
        let events = collect_wire([
            start(),
            tool_start(0, "call-a", &json!({})),
            stop(0),
            final_delta("tool_use"),
            json!({"type": "message_delta", "delta": {"stop_reason": null}, "usage": {"output_tokens": 8}}),
            final_delta("tool_use"),
            json!({"type": "message_delta", "delta": {}, "usage": {"output_tokens": 9}}),
            json!({"type": "message_stop"}),
        ]);
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let Some(Ok(StreamEvent::Done { reason, message })) = events.last() else {
            panic!("expected authorized completion");
        };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.usage.output, 9);
        assert_eq!(message.usage.total_tokens, 69);
        assert_eq!(message.content.len(), 1);
    }

    #[test]
    fn terminal_authorization_survives_every_transport_split() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        for reason in ["refusal", "tool_use"] {
            let bytes = wire_bytes([
                start(),
                tool_start(0, "call-a", &json!({})),
                tool_delta(0, "{\"path\":\"héllo.txt\"}"),
                stop(0),
                final_delta(reason),
                json!({"type": "message_stop"}),
            ]);
            for split in 0..=bytes.len() {
                let chunks = [Ok(bytes[..split].to_vec()), Ok(bytes[split..].to_vec())];
                let events: Vec<_> = runtime.block_on(
                    wire_stream(
                        stream::iter(chunks),
                        "claude-test".into(),
                        "anthropic-messages".into(),
                        "anthropic".into(),
                    )
                    .collect(),
                );
                if reason == "refusal" {
                    terminal_failure(&events);
                } else {
                    assert!(
                        events.iter().all(Result::is_ok),
                        "split {split}: {events:?}"
                    );
                    assert!(matches!(
                        events.last(),
                        Some(Ok(StreamEvent::Done {
                            reason: StopReason::ToolUse,
                            ..
                        }))
                    ));
                }
            }
        }
    }

    struct ObservedSource {
        chunks: VecDeque<std::io::Result<Vec<u8>>>,
        pending_at_end: bool,
        dropped: Arc<AtomicBool>,
    }

    impl Stream for ObservedSource {
        type Item = std::io::Result<Vec<u8>>;

        fn poll_next(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
            let this = self.get_mut();
            if let Some(chunk) = this.chunks.pop_front() {
                Poll::Ready(Some(chunk))
            } else if this.pending_at_end {
                Poll::Pending
            } else {
                Poll::Ready(None)
            }
        }
    }

    impl Drop for ObservedSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn observed_wire(
        chunks: impl IntoIterator<Item = std::io::Result<Vec<u8>>>,
        pending_at_end: bool,
    ) -> (EventStream, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let source = ObservedSource {
            chunks: chunks.into_iter().collect(),
            pending_at_end,
            dropped: Arc::clone(&dropped),
        };
        (
            wire_stream(
                source,
                "claude-test".into(),
                "anthropic-messages".into(),
                "anthropic".into(),
            ),
            dropped,
        )
    }

    fn wire_bytes(events: impl IntoIterator<Item = Value>) -> Vec<u8> {
        use std::fmt::Write;
        let mut out = String::new();
        for event in events {
            let _ = write!(out, "data: {event}\n\n");
        }
        out.into_bytes()
    }

    fn assert_retired(output: &mut EventStream, dropped: &AtomicBool) {
        // This assertion MUST precede another poll: a consumer may retain the
        // terminal stream indefinitely without ever asking it for EOF.
        assert!(
            dropped.load(Ordering::SeqCst),
            "source retained after terminal event"
        );
        for _ in 0..3 {
            assert!(
                output
                    .next()
                    .now_or_never()
                    .expect("fused stream must be ready")
                    .is_none()
            );
        }
    }

    #[test]
    fn successful_terminal_releases_source_without_an_extra_poll() {
        let bytes = wire_bytes([
            start(),
            final_delta("end_turn"),
            json!({"type": "message_stop"}),
        ]);
        let (mut output, dropped) = observed_wire([Ok(bytes)], true);
        assert!(matches!(
            output.next().now_or_never().unwrap(),
            Some(Ok(StreamEvent::Start { .. }))
        ));
        assert!(!dropped.load(Ordering::SeqCst));
        assert!(matches!(
            output.next().now_or_never().unwrap(),
            Some(Ok(StreamEvent::Done { .. }))
        ));
        assert_retired(&mut output, &dropped);
    }

    #[test]
    fn rejected_tool_terminal_releases_source_before_error_is_observed() {
        let bytes = wire_bytes([
            start(),
            tool_start(0, "call-a", &json!({})),
            stop(0),
            final_delta("refusal"),
            json!({"type": "message_stop"}),
            // A queued tail cannot keep the source alive or reauthorize calls.
            final_delta("tool_use"),
            json!({"type": "message_stop"}),
        ]);
        let (mut output, dropped) = observed_wire([Ok(bytes)], true);
        for _ in 0..3 {
            let event = output.next().now_or_never().unwrap().unwrap().unwrap();
            assert!(!matches!(
                event,
                StreamEvent::Done { .. } | StreamEvent::Error { .. }
            ));
        }
        let event = output.next().now_or_never().unwrap().unwrap().unwrap();
        let StreamEvent::Error { reason, error } = event else {
            panic!("expected withheld call");
        };
        assert_eq!(reason, StopReason::Error);
        assert_eq!(error.usage.total_tokens, 67);
        assert_retired(&mut output, &dropped);
    }

    #[test]
    fn protocol_failures_release_source_before_yielding_the_error() {
        for bytes in [
            b"data: {invalid json}\n\n".to_vec(),
            wire_bytes([start(), stop(9)]),
            wire_bytes([start(), start()]),
            wire_bytes([start(), final_delta("refusal"), final_delta("tool_use")]),
            wire_bytes([
                start(),
                tool_start(0, "call-a", &json!({})),
                json!({"type": "content_block_delta", "index": 0, "delta": {
                    "type": "input_json_delta", "partial_json": null
                }}),
            ]),
        ] {
            let (mut output, dropped) = observed_wire([Ok(bytes)], true);
            let mut terminal_seen = false;
            for _ in 0..8 {
                let event = output
                    .next()
                    .now_or_never()
                    .expect("error must not wait for EOF")
                    .unwrap();
                if event.is_err() {
                    terminal_seen = true;
                    break;
                }
            }
            assert!(terminal_seen);
            assert_retired(&mut output, &dropped);
        }
    }

    #[test]
    fn provider_error_releases_source_before_yielding_the_error() {
        let bytes = wire_bytes([
            json!({"type": "error", "error": {"message": "overloaded"}}),
            start(),
        ]);
        let (mut output, dropped) = observed_wire([Ok(bytes)], true);
        assert!(matches!(
            output.next().now_or_never().unwrap(),
            Some(Ok(StreamEvent::Error { .. }))
        ));
        assert_retired(&mut output, &dropped);
    }

    #[test]
    fn transport_errors_and_premature_eof_release_source_and_fuse() {
        for kind in [
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::Interrupted,
        ] {
            let (mut output, dropped) =
                observed_wire([Err(std::io::Error::new(kind, "fixture"))], true);
            assert!(output.next().now_or_never().unwrap().unwrap().is_err());
            assert_retired(&mut output, &dropped);
        }
        for started in [false, true] {
            let chunks = if started {
                vec![Ok(wire_bytes([start()]))]
            } else {
                Vec::new()
            };
            let (mut output, dropped) = observed_wire(chunks, false);
            if started {
                assert!(matches!(
                    output.next().now_or_never().unwrap(),
                    Some(Ok(StreamEvent::Start { .. }))
                ));
            }
            assert!(output.next().now_or_never().unwrap().unwrap().is_err());
            assert_retired(&mut output, &dropped);
        }
    }

    #[test]
    fn dropping_a_pending_stream_releases_its_source() {
        let (mut output, dropped) = observed_wire([], true);
        assert!(output.next().now_or_never().is_none());
        assert!(!dropped.load(Ordering::SeqCst));
        drop(output);
        assert!(dropped.load(Ordering::SeqCst));
    }

    struct WakeFlag(AtomicBool);

    impl Wake for WakeFlag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn silent_burst(kind: &str) -> Vec<u8> {
        let mut bytes = wire_bytes([start()]);
        for _ in 0..MAX_READY_EVENTS_PER_YIELD * 2 {
            let frame = match kind {
                "header" => b"event: ping\ndata: {}\n\n".to_vec(),
                "body" => wire_bytes([json!({"type": "ping"})]),
                "metadata" => wire_bytes([
                    json!({"type": "message_delta", "delta": {}, "usage": {"output_tokens": 1}}),
                ]),
                _ => panic!("unexpected fixture kind"),
            };
            bytes.extend(frame);
        }
        bytes.extend(wire_bytes([
            final_delta("end_turn"),
            json!({"type": "message_stop"}),
        ]));
        bytes
    }

    #[test]
    fn heartbeat_and_metadata_bursts_yield_and_wake_the_consumer() {
        for kind in ["header", "body", "metadata"] {
            let (mut output, dropped) = observed_wire([Ok(silent_burst(kind))], true);
            let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
            let waker = Waker::from(Arc::clone(&flag));
            let mut task = TaskContext::from_waker(&waker);
            assert!(matches!(
                output.as_mut().poll_next(&mut task),
                Poll::Ready(Some(Ok(StreamEvent::Start { .. })))
            ));
            assert!(
                output.as_mut().poll_next(&mut task).is_pending(),
                "{kind} must yield before completing the burst"
            );
            assert!(
                flag.0.load(Ordering::SeqCst),
                "{kind} must schedule another poll"
            );
            assert!(!dropped.load(Ordering::SeqCst));
            // A finite burst must still finish, with no timer or network EOF.
            let mut done = false;
            for _ in 0..4 {
                if let Poll::Ready(Some(Ok(StreamEvent::Done { message, .. }))) =
                    output.as_mut().poll_next(&mut task)
                {
                    assert_eq!(message.usage.total_tokens, 67);
                    done = true;
                    break;
                }
            }
            assert!(done, "{kind} did not resume after yielding");
            assert_retired(&mut output, &dropped);
        }
    }

    #[test]
    fn abort_can_win_while_ready_metadata_would_otherwise_monopolize_polling() {
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let (mut output, dropped) = observed_wire([Ok(silent_burst("metadata"))], true);
        runtime.block_on(async {
            assert!(matches!(
                output.next().await,
                Some(Ok(StreamEvent::Start { .. }))
            ));
            let (abort, signal) = crate::agent::AbortHandle::new();
            // Poll the response first. Without cooperative yielding it returns
            // Done before the abort branch ever gets a chance to run.
            let next = Box::pin(output.next());
            let cancel = Box::pin(async move {
                abort.abort();
                signal.wait().await;
            });
            match futures::future::select(next, cancel).await {
                futures::future::Either::Right(((), pending)) => drop(pending),
                futures::future::Either::Left(_) => panic!("metadata burst starved cancellation"),
            }
        });
        drop(output);
        assert!(dropped.load(Ordering::SeqCst));
    }
}
