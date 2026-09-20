//! Public provider requests and real SSE decoding, not just helper invocation.
//! HTTP fixtures use loopback and explicit fake credentials on every route.

use super::*;
use crate::model::{ThinkingContent, ThinkingLevel, UserMessage};
use crate::provider::BeforeProviderRequestHook;
use crate::providers::vertex::VertexProvider;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug)]
enum Route {
    Developer,
    Cli,
    Vertex,
}

impl Route {
    const ALL: [Self; 3] = [Self::Developer, Self::Cli, Self::Vertex];

    fn provider(self, model: &str, endpoint: &str) -> Box<dyn Provider> {
        match self {
            Self::Developer => Box::new(GeminiProvider::new(model).with_base_url(endpoint)),
            Self::Cli => Box::new(
                GeminiProvider::new(model)
                    .with_google_cli_mode(true)
                    .with_api_name("google-gemini-cli")
                    .with_provider_name("google-gemini-cli")
                    .with_base_url(endpoint),
            ),
            Self::Vertex => Box::new(
                VertexProvider::new(model)
                    .with_project("p")
                    .with_location("us-central1")
                    .with_endpoint_url(endpoint),
            ),
        }
    }

    fn credentials(self, mut options: StreamOptions) -> StreamOptions {
        options.api_key = Some(match self {
            Self::Cli => json!({"token":"test-token","projectId":"p"}).to_string(),
            Self::Developer | Self::Vertex => "test-token".to_string(),
        });
        options
    }

    fn inner(self, body: &Value) -> &Value {
        match self {
            Self::Cli => &body["request"],
            Self::Developer | Self::Vertex => body,
        }
    }
}

fn context() -> Context<'static> {
    Context::owned(
        Some("Use tools when needed.".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("Read the file.".to_string()),
            timestamp: 0,
        })],
        vec![ToolDef {
            name: "read".to_string(),
            description: "Read a file".to_string(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }],
    )
}

fn frames(route: Route, events: &[Value]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for event in events {
        let value = match route {
            Route::Cli => json!({"response": event}),
            Route::Developer | Route::Vertex => event.clone(),
        };
        let _ = write!(out, "data: {value}\n\n");
    }
    out
}

fn success() -> Vec<Value> {
    vec![json!({"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]})]
}

fn capture(
    route: Route,
    model: &str,
    context: &Context<'_>,
    options: StreamOptions,
    events: &[Value],
) -> (Value, Vec<Result<StreamEvent>>) {
    let body = frames(route, events);
    let (endpoint, requests) = spawn_test_server(200, "text/event-stream", &body);
    let provider = route.provider(model, &endpoint);
    let options = route.credentials(options);
    let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
    let events = runtime.block_on(async {
        provider
            .stream(context, &options)
            .await
            .expect("start stream")
            .collect()
            .await
    });
    let request = requests
        .recv_timeout(Duration::from_secs(5))
        .expect("captured request");
    assert!(
        request.headers.contains_key("authorization")
            || request.headers.contains_key("x-goog-api-key")
    );
    (
        serde_json::from_str(&request.body).expect("request JSON"),
        events,
    )
}

fn done(events: &[Result<StreamEvent>]) -> &AssistantMessage {
    assert!(events.iter().all(Result::is_ok), "{events:?}");
    let Some(Ok(StreamEvent::Done { message, .. })) = events.last() else {
        panic!("missing terminal message: {events:?}");
    };
    message
}

#[test]
fn explicit_controls_reach_the_wire_on_all_three_google_transports() {
    for route in Route::ALL {
        for (model, level, expected) in [
            (
                "gemini-2.5-flash",
                ThinkingLevel::High,
                json!({"thinkingBudget":4096,"includeThoughts":true}),
            ),
            (
                "gemini-3-flash-preview",
                ThinkingLevel::High,
                json!({"thinkingLevel":"high","includeThoughts":true}),
            ),
            (
                "gemini-3-pro-preview",
                ThinkingLevel::Off,
                json!({"thinkingLevel":"low","includeThoughts":false}),
            ),
        ] {
            let (body, events) = capture(
                route,
                model,
                &context(),
                StreamOptions {
                    max_tokens: Some(8192),
                    thinking_level: Some(level),
                    ..Default::default()
                },
                &success(),
            );
            assert_eq!(
                route.inner(&body)["generationConfig"]["thinkingConfig"],
                expected,
                "{route:?}/{model}"
            );
            assert_eq!(
                route.inner(&body)["generationConfig"]["maxOutputTokens"],
                8192
            );
            assert_eq!(done(&events).model, model);
            if matches!(route, Route::Cli) {
                assert_eq!(body["project"], "projects/p/locations/global");
                assert_eq!(body["model"], model);
                assert!(body.get("generationConfig").is_none());
            }
        }
    }
}

#[test]
fn unspecified_and_unrecognized_models_keep_the_original_generation_config() {
    for route in Route::ALL {
        for (model, level) in [
            ("gemini-3-flash-preview", None),
            ("custom-model", Some(ThinkingLevel::High)),
            (
                "gemini-2.5-flash-native-audio-preview",
                Some(ThinkingLevel::High),
            ),
        ] {
            let (body, events) = capture(
                route,
                model,
                &context(),
                StreamOptions {
                    thinking_level: level,
                    max_tokens: Some(1024),
                    ..Default::default()
                },
                &success(),
            );
            let config = &route.inner(&body)["generationConfig"];
            assert!(config.get("thinkingConfig").is_none(), "{route:?}/{model}");
            assert_eq!(config["maxOutputTokens"], 1024);
            assert_eq!(done(&events).stop_reason, StopReason::Stop);
        }
    }
}

#[test]
fn rewrite_hooks_observe_prepared_controls_and_their_replacement_is_not_overwritten() {
    for route in Route::ALL {
        let observed = Arc::new(AtomicBool::new(false));
        let called = Arc::clone(&observed);
        let options = StreamOptions {
            thinking_level: Some(ThinkingLevel::High),
            before_provider_request: Some(BeforeProviderRequestHook::new(move |event| {
                let called = Arc::clone(&called);
                Box::pin(async move {
                    assert_eq!(
                        event.payload["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                        "high"
                    );
                    assert!(
                        event.payload.get("project").is_none(),
                        "CLI wrapper is host-owned"
                    );
                    called.store(true, Ordering::SeqCst);
                    let mut body = event.payload;
                    body["generationConfig"]["thinkingConfig"]["thinkingLevel"] = json!("low");
                    Some(body)
                })
            })),
            ..Default::default()
        };
        let (body, events) = capture(
            route,
            "gemini-3-flash-preview",
            &context(),
            options,
            &success(),
        );
        assert!(observed.load(Ordering::SeqCst));
        assert_eq!(
            route.inner(&body)["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
        assert_eq!(done(&events).stop_reason, StopReason::Stop);
    }
}

#[test]
fn invalid_rewrites_fall_back_to_prepared_not_unconfigured_requests() {
    for route in Route::ALL {
        let options = StreamOptions {
            thinking_level: Some(ThinkingLevel::High),
            before_provider_request: Some(BeforeProviderRequestHook::new(|_| {
                Box::pin(async { Some(json!({"contents":"not-an-array"})) })
            })),
            ..Default::default()
        };
        let (body, events) = capture(
            route,
            "gemini-3-flash-preview",
            &context(),
            options,
            &success(),
        );
        assert_eq!(
            route.inner(&body)["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        assert_eq!(done(&events).stop_reason, StopReason::Stop);
    }
}

#[test]
fn impossible_thinking_budgets_fail_before_hooks_or_connections() {
    for route in Route::ALL {
        let observed = Arc::new(AtomicBool::new(false));
        let called = Arc::clone(&observed);
        let options = route.credentials(StreamOptions {
            max_tokens: Some(128),
            thinking_level: Some(ThinkingLevel::High),
            before_provider_request: Some(BeforeProviderRequestHook::new(move |_| {
                called.store(true, Ordering::SeqCst);
                Box::pin(async { None })
            })),
            ..Default::default()
        });
        let provider = route.provider("gemini-2.5-pro", "not a valid endpoint");
        let runtime = RuntimeBuilder::current_thread().build().expect("runtime");
        let error = runtime.block_on(async {
            provider
                .stream(&context(), &options)
                .await
                .err()
                .expect("invalid budget")
        });
        assert!(
            error.to_string().contains("max_tokens"),
            "{route:?}: {error}"
        );
        assert!(!observed.load(Ordering::SeqCst));
    }
}

#[test]
fn streamed_thoughts_and_answers_have_distinct_ordered_lifecycles() {
    let body = frames(
        Route::Developer,
        &[
            json!({"candidates":[{"content":{"parts":[{"text":"think ","thought":true}]}}]}),
            json!({"candidates":[{"content":{"parts":[{"text":"more","thought":true},{"text":"answer"}]},"finishReason":"STOP"}]}),
        ],
    );
    let events = collect_stream_items_from_body(&body);
    let message = done(&events);
    assert_eq!(message.content.len(), 2);
    assert!(matches!(&message.content[0], ContentBlock::Thinking(t) if t.thinking == "think more"));
    assert!(matches!(&message.content[1], ContentBlock::Text(t) if t.text == "answer"));
    let sequence: Vec<_> = events
        .iter()
        .map(|event| match event.as_ref().unwrap() {
            StreamEvent::Start { .. } => "start",
            StreamEvent::ThinkingStart { .. } => "thinking_start",
            StreamEvent::ThinkingDelta { .. } => "thinking_delta",
            StreamEvent::ThinkingEnd { .. } => "thinking_end",
            StreamEvent::TextStart { .. } => "text_start",
            StreamEvent::TextDelta { .. } => "text_delta",
            StreamEvent::TextEnd { .. } => "text_end",
            StreamEvent::Done { .. } => "done",
            _ => "unexpected",
        })
        .collect();
    assert_eq!(
        sequence,
        vec![
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_end",
            "done"
        ]
    );
}

#[test]
fn signed_state_round_trips_through_session_encoding_and_actual_followup_requests() {
    let parts = json!([
        {"text":"Use the tool.","thought":true,"thoughtSignature":"dGhpbms="},
        {"text":"Reading.","thoughtSignature":"YW5zd2Vy"},
        {"text":"","thoughtSignature":"ZW1wdHk="},
        {"functionCall":{"name":"read","args":{"path":"a.txt"}},"thoughtSignature":"Y2FsbA=="},
        {"functionCall":{"name":"read","args":{"path":"b.txt"}}}
    ]);
    for route in Route::ALL {
        let initial = context();
        let (_, events) = capture(
            route,
            "gemini-3-flash-preview",
            &initial,
            StreamOptions::default(),
            &[json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]})],
        );
        let message = done(&events);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert!(
            matches!(&events[0], Ok(StreamEvent::Start { partial }) if partial.content.is_empty())
        );
        let stored = serde_json::to_vec(&Message::assistant(message.clone())).unwrap();
        let replay: Message = serde_json::from_slice(&stored).unwrap();
        let mut messages = initial.messages.to_vec();
        messages.push(replay);
        for block in &message.content {
            if let ContentBlock::ToolCall(call) = block {
                messages.push(Message::tool_result(crate::model::ToolResultMessage {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    content: vec![ContentBlock::Text(TextContent::new("file contents"))],
                    details: None,
                    is_error: false,
                    timestamp: 1,
                }));
            }
        }
        let followup = Context::owned(None, messages, initial.tools.to_vec());
        let (body, events) = capture(
            route,
            "gemini-3-flash-preview",
            &followup,
            StreamOptions::default(),
            &success(),
        );
        assert_eq!(
            route.inner(&body)["contents"][1]["parts"],
            parts,
            "{route:?}"
        );
        assert_eq!(
            route.inner(&body)["contents"][2]["parts"][0]["functionResponse"]["name"],
            "read"
        );
        assert_eq!(done(&events).stop_reason, StopReason::Stop);
    }
}

#[test]
fn cumulative_thinking_and_cache_counters_survive_omitted_fields_and_repeated_snapshots() {
    let events = [
        json!({"candidates":[{"content":{"parts":[{"text":"reason","thought":true}]}}],
            "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":2,"thoughtsTokenCount":10,"cachedContentTokenCount":40,"totalTokenCount":112}}),
        json!({"usageMetadata":{"candidatesTokenCount":3}}),
        json!({"usageMetadata":{"candidatesTokenCount":3}}),
        json!({"candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}],
            "usageMetadata":{"thoughtsTokenCount":12,"totalTokenCount":115}}),
    ];
    for route in Route::ALL {
        let (_, output) = capture(
            route,
            "gemini-2.5-flash",
            &context(),
            StreamOptions::default(),
            &events,
        );
        let usage = &done(&output).usage;
        assert_eq!(
            (
                usage.input,
                usage.output,
                usage.cache_read,
                usage.total_tokens
            ),
            (60, 15, 40, 115)
        );
    }
}

#[test]
fn malformed_reasoning_never_falls_through_to_ordinary_text_or_exposes_payloads() {
    for part in [
        json!({"text":"private-thought","thought":"true"}),
        json!({"text":"private-thought","thought":null}),
        json!({"text":"private-thought","thought":true,"thoughtSignature":42}),
        json!({"text":"private-thought","thought":true,"thought_signature":[]}),
        json!({"text":"private-thought","thoughtSignature":"private-signature","thought_signature":"other"}),
        json!({"text":"private-thought","functionCall":{"name":"read","args":{}}}),
    ] {
        let body = frames(
            Route::Developer,
            &[json!({"candidates":[{"content":{"parts":[part]},"finishReason":"STOP"}]})],
        );
        let events = collect_stream_items_from_body(&body);
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::TextDelta { .. }
                | StreamEvent::Done { .. }
                | StreamEvent::ToolCallStart { .. })
        )));
        let error = events.last().unwrap().as_ref().unwrap_err().to_string();
        assert!(error.contains("JSON parse error"));
        assert!(!error.contains("private-"));
    }
}

#[test]
fn false_thought_flags_and_signature_aliases_decode_without_losing_native_state() {
    for signature_key in ["thoughtSignature", "thought_signature"] {
        for thought in [false, true] {
            let mut input = json!({"text":"body","thought":thought});
            input[signature_key] = json!("c2ln");
            let decoded: GeminiPart = serde_json::from_value(input).unwrap();
            let output = serde_json::to_value(decoded).unwrap();
            assert_eq!(output["text"], "body");
            assert_eq!(output["thoughtSignature"], "c2ln");
            assert_eq!(
                output.get("thought").and_then(Value::as_bool),
                thought.then_some(true)
            );
            assert!(output.get("thought_signature").is_none());
        }
    }
}

#[test]
fn signature_only_tail_parts_survive_and_every_content_block_closes_once() {
    let body = frames(
        Route::Developer,
        &[
            json!({"candidates":[{"content":{"parts":[{"text":"answer"}]},"finishReason":"STOP"}]}),
            json!({"candidates":[{"content":{"parts":[{"thoughtSignature":"dGFpbA=="}]}}]}),
            json!({"candidates":[{"finishReason":"STOP"}]}),
        ],
    );
    let events = collect_stream_items_from_body(&body);
    let message = done(&events);
    assert_eq!(message.content.len(), 2);
    assert!(matches!(&message.content[1], ContentBlock::Text(t)
        if t.text.is_empty() && t.text_signature.as_deref() == Some("dGFpbA==")));
    let ends: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Ok(StreamEvent::TextEnd { content_index, .. }) => Some(*content_index),
            _ => None,
        })
        .collect();
    assert_eq!(ends, vec![0, 1]);
}

#[test]
fn corrupted_or_timed_out_streams_never_become_success_after_a_finish_marker() {
    for cloud in [false, true] {
        for kind in [
            std::io::ErrorKind::WriteZero,
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::ConnectionReset,
        ] {
            let route = if cloud { Route::Cli } else { Route::Developer };
            let bytes = frames(route, &success()).into_bytes();
            let source = stream::iter(vec![
                Ok(bytes),
                Err(std::io::Error::new(kind, "fixture failure")),
                Ok(frames(route, &success()).into_bytes()),
            ]);
            let state = StreamState::new(
                SseStream::new(source),
                "gemini-test".into(),
                "google-generative-ai".into(),
                "google".into(),
            );
            let events: Vec<_> = futures::executor::block_on(state.into_stream(cloud).collect());
            assert_eq!(events.iter().filter(|event| event.is_err()).count(), 1);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(StreamEvent::Done { .. })))
            );
            assert!(events.last().unwrap().is_err());
        }
    }
}

#[test]
fn truncated_reasoning_is_a_partial_stream_not_a_completed_answer() {
    let body = frames(
        Route::Developer,
        &[json!({"candidates":[{"content":{"parts":[{"text":"unfinished","thought":true}]}}]})],
    );
    let events = collect_stream_items_from_body(&body);
    assert!(events.iter().any(|event| matches!(event, Ok(StreamEvent::ThinkingDelta { delta, .. }) if delta == "unfinished")));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::Done { .. })))
    );
    assert!(
        events
            .last()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("unexpected EOF")
    );
}

#[test]
fn a_blocked_prompt_cannot_dispatch_an_adjoining_tool_candidate() {
    let body = frames(
        Route::Developer,
        &[json!({"promptFeedback":{"blockReason":"SAFETY"},
            "candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{}}}]},"finishReason":"STOP"}]})],
    );
    let events = collect_stream_items_from_body(&body);
    assert_eq!(done(&events).stop_reason, StopReason::Error);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(StreamEvent::ToolCallStart { .. })))
    );
}

#[test]
fn foreign_and_claude_vertex_signatures_are_not_relabelled_as_gemini_state() {
    for (api, model) in [
        ("anthropic-messages", "claude-sonnet-4-6"),
        ("openai-responses", "gpt-test"),
        ("google-vertex", "claude-sonnet-4-6"),
    ] {
        let message = Message::assistant(AssistantMessage {
            api: api.into(),
            model: model.into(),
            content: vec![
                ContentBlock::Thinking(ThinkingContent {
                    thinking: "private foreign thought".into(),
                    thinking_signature: Some("foreign-thought-signature".into()),
                }),
                ContentBlock::Text(TextContent {
                    text: "visible answer".into(),
                    text_signature: Some("foreign-text-signature".into()),
                }),
                ContentBlock::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: json!({}),
                    thought_signature: Some("foreign-call-signature".into()),
                }),
            ],
            ..AssistantMessage::default()
        });
        let wire = serde_json::to_value(convert_message_to_gemini(&message)).unwrap();
        assert_eq!(wire[0]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(wire[0]["parts"][0], json!({"text":"visible answer"}));
        assert!(wire[0]["parts"][1].get("thoughtSignature").is_none());
        assert!(!wire.to_string().contains("foreign"));
    }
}

#[test]
fn unicode_and_signed_part_boundaries_are_invariant_at_every_transport_split() {
    let parts = json!([
        {"text":"考える 🦀","thought":true,"thoughtSignature":"dGhpbms="},
        {"text":"回答","thoughtSignature":"YW5zd2Vy"},
        {"text":"","thoughtSignature":"ZW1wdHk="}
    ]);
    let body = frames(
        Route::Developer,
        &[json!({"candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]})],
    )
    .into_bytes();
    for split in 0..=body.len() {
        let source = stream::iter(vec![Ok(body[..split].to_vec()), Ok(body[split..].to_vec())]);
        let state = StreamState::new(
            SseStream::new(source),
            "gemini-test".into(),
            "google-generative-ai".into(),
            "google".into(),
        );
        let events: Vec<_> = futures::executor::block_on(state.into_stream(false).collect());
        let message = done(&events);
        let wire = serde_json::to_value(convert_message_to_gemini(&Message::assistant(
            message.clone(),
        )))
        .unwrap();
        assert_eq!(wire[0]["parts"], parts, "byte split {split}");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    Ok(StreamEvent::ThinkingEnd { .. } | StreamEvent::TextEnd { .. })
                ))
                .count(),
            3
        );
    }
}

#[test]
fn zero_argument_calls_are_not_dropped_and_non_object_arguments_fail() {
    let body = frames(
        Route::Developer,
        &[
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"status"}}]},"finishReason":"STOP"}]}),
        ],
    );
    let events = collect_stream_items_from_body(&body);
    let message = done(&events);
    assert!(
        matches!(&message.content[0], ContentBlock::ToolCall(call) if call.arguments == json!({}))
    );
    for args in [
        Value::Null,
        json!([]),
        json!("private-arguments"),
        json!(42),
    ] {
        assert!(
            serde_json::from_value::<GeminiPart>(
                json!({"functionCall":{"name":"status","args":args}})
            )
            .is_err()
        );
    }
}
