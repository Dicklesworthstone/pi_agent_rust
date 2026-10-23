//! Terminal and fragmented-stream regressions for DeepSeek text tool recovery.

use super::{
    AssistantMessage, ContentBlock, SseStream, StopReason, StreamEvent, StreamState,
};
use serde_json::{Value, json};

type TestState = StreamState<futures::stream::Empty<std::result::Result<Vec<u8>, std::io::Error>>>;

fn state(provider: &str, model: &str) -> TestState {
    StreamState::new(
        SseStream::new(futures::stream::empty()),
        model.to_string(),
        "openai-completions".to_string(),
        provider.to_string(),
    )
}

fn deepseek_state() -> TestState {
    state("fireworks", "deepseek-v4.1-flash")
}

fn feed(state: &mut TestState, delta: &Value, finish: Option<&str>) {
    state
        .process_event(
            &json!({"choices": [{"delta": delta, "finish_reason": finish}]}).to_string(),
        )
        .expect("valid stream frame");
}

fn feed_characters(state: &mut TestState, text: &str) {
    for character in text.chars() {
        feed(state, &json!({"content": character.to_string()}), None);
    }
}

fn finish(mut state: TestState) -> (Vec<StreamEvent>, AssistantMessage) {
    state.finish_response();
    let events = state.pending_events.drain(..).collect::<Vec<_>>();
    let message = events
        .iter()
        .find_map(|event| match event {
            StreamEvent::Done { reason, message } => {
                assert_eq!(*reason, message.stop_reason);
                Some(message.clone())
            }
            _ => None,
        })
        .expect("one terminal message");
    assert_eq!(
        events.iter().filter(|event| matches!(event, StreamEvent::Done { .. })).count(),
        1
    );
    (events, message)
}

fn visible(message: &AssistantMessage) -> String {
    message.content.iter().filter_map(|block| match block {
        ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }).collect()
}

fn calls(message: &AssistantMessage) -> Vec<&crate::model::ToolCall> {
    message.content.iter().filter_map(|block| match block {
        ContentBlock::ToolCall(call) => Some(call),
        _ => None,
    }).collect()
}

fn template_call(name: &str, arguments: &Value) -> String {
    format!(
        "<｜tool_calls_begin｜><｜tool_call_begin｜>{name}<｜tool_sep｜>{arguments}<｜tool_calls_end｜>"
    )
}

fn dsml_call(bar: &str) -> String {
    format!(
        "<{bar}DSML{bar}tool_calls>\n\
         <{bar}DSML{bar}invoke name=\"bash\">\n\
         <{bar}DSML{bar}parameter name=\"command\" string=\"true\">  echo &lt;ok&gt;\n</{bar}DSML{bar}parameter>\n\
         <{bar}DSML{bar}parameter name=\"timeout\" string=\"false\">15</{bar}DSML{bar}parameter>\n\
         <{bar}DSML{bar}parameter name=\"options\" string=\"false\">{{\"enabled\":true,\"items\":[1,null]}}</{bar}DSML{bar}parameter>\n\
         </{bar}DSML{bar}invoke>\n</{bar}DSML{bar}tool_calls>"
    )
}

#[test]
fn recovers_both_dsml_spellings_with_typed_parameters_and_ordered_events() {
    for bar in ["｜", "|"] {
        let mut state = deepseek_state();
        let input = format!("Before\n{}\nAfter", dsml_call(bar));
        feed_characters(&mut state, &input);
        // Neither raw markup nor speculative calls reach any partial snapshot.
        assert!(!state.pending_events.iter().any(|event| matches!(
            event, StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. }
        )));
        assert!(state.pending_events.iter().all(|event| match event {
            StreamEvent::TextDelta { delta, .. } => !delta.contains("DSML"),
            _ => true,
        }));
        feed(&mut state, &json!({}), Some("stop"));
        let (events, message) = finish(state);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(visible(&message), "Before\n\nAfter");
        assert!(matches!(message.content.as_slice(), [
            ContentBlock::Text(_), ContentBlock::ToolCall(_), ContentBlock::Text(_)
        ]));
        let calls = calls(&message);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "bash");
        assert!(calls[0].id.starts_with("call_"));
        assert_eq!(calls[0].arguments, json!({
            "command": "  echo &lt;ok&gt;\n",
            "timeout": 15,
            "options": {"enabled": true, "items": [1, null]}
        }));
        let start = events.iter().position(|event| matches!(event, StreamEvent::ToolCallStart { .. })).unwrap();
        let delta = events.iter().position(|event| matches!(event, StreamEvent::ToolCallDelta { .. })).unwrap();
        let end = events.iter().position(|event| matches!(event, StreamEvent::ToolCallEnd { .. })).unwrap();
        assert!(start < delta && delta < end && end < events.len() - 1);
        assert_eq!(events.iter().filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. })).count(), 1);
    }
}

#[test]
fn recovers_canonical_respelled_and_legacy_templates_at_every_split() {
    let arguments = json!({"path": "src/lib.rs"});
    let inputs = [
        format!("<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>read<｜tool▁sep｜>{arguments}<｜tool▁call▁end｜><｜tool▁calls▁end｜>"),
        format!("<｜tool_calls_begin｜><tool_call_begin｜>read<｜tool_sep｜>{arguments}<｜tool_calls_end｜><｜tool_calls_end｜>"),
        format!("<｜tool▁calls_begin｜><｜tool_call_begin>read<tool_sep｜>{arguments}<tool_calls_end｜>"),
        format!("<｜tool▁call▁begin｜>function<｜tool▁sep｜>read\n```json\n{arguments}\n```<｜tool▁call▁end｜>"),
    ];
    for input in inputs {
        for split in input.char_indices().map(|(index, _)| index).chain([input.len()]) {
            let mut state = deepseek_state();
            feed(&mut state, &json!({"content": &input[..split]}), None);
            feed(&mut state, &json!({"content": &input[split..]}), Some("stop"));
            let (_, message) = finish(state);
            assert_eq!(message.stop_reason, StopReason::ToolUse, "split {split}");
            assert_eq!(visible(&message), "");
            let calls = calls(&message);
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "read");
            assert_eq!(calls[0].arguments, arguments);
        }
    }
}

#[test]
fn consecutive_calls_have_unique_ids_and_ignore_markers_inside_json_strings() {
    let arguments = json!({"content": "x <｜tool▁calls▁end｜> <｜tool_call_begin｜> <tool_call_end｜> y"});
    let input = format!(
        "<｜tool_calls_begin｜><｜tool_call_begin｜>write<｜tool_sep｜>{arguments}\
         <｜tool_call_begin｜>read<｜tool_sep｜>{{\"path\":\"a\"}}<｜tool_calls_end｜>Done."
    );
    let mut state = deepseek_state();
    feed_characters(&mut state, &input);
    feed(&mut state, &json!({}), Some("stop"));
    let (_, message) = finish(state);
    let calls = calls(&message);
    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].id, calls[1].id);
    assert_eq!(calls[0].arguments, arguments);
    assert_eq!(calls[1].arguments, json!({"path": "a"}));
    assert_eq!(visible(&message), "Done.");
}

#[test]
fn quoted_examples_and_other_model_templates_remain_ordinary_prose() {
    let prose = concat!(
        "Quoted `<｜tool_calls_begin｜>` and ``<tool_call_begin｜>``.\n",
        "```text\n<｜tool▁call▁begin｜>bash<｜tool▁sep｜>{}\n```\n",
        "~~~xml\n<|DSML|tool_calls>\n~~~\n",
        "Kimi: <|tool_call_begin|>functions.read:0<|tool_call_end|>.\n",
        "A <tool_call_begin> element and if a < b.\n",
    );
    let mut state = deepseek_state();
    feed_characters(&mut state, &format!("{prose}{}", template_call("read", &json!({"path":"a"}))));
    feed(&mut state, &json!({}), Some("stop"));
    let (_, message) = finish(state);
    assert_eq!(visible(&message), prose);
    assert_eq!(calls(&message).len(), 1);
}

#[test]
fn unmatched_inline_backtick_does_not_hide_a_real_call() {
    let before = "Run the `grep first. ";
    let after = " Then `<｜tool_calls_begin｜>` stays.";
    let input = format!(
        "{before}{}\n\n{after}",
        template_call("read", &json!({"path":"a"}))
    );
    let mut state = deepseek_state();
    feed_characters(&mut state, &input);
    feed(&mut state, &json!({}), Some("stop"));
    let (_, message) = finish(state);
    assert_eq!(visible(&message), format!("{before}\n\n{after}"));
    assert_eq!(calls(&message).len(), 1);
}

#[test]
fn recovery_is_enabled_only_for_deepseek_provider_or_model_components() {
    let input = template_call("read", &json!({"path":"a"}));
    for (provider, model, expected) in [
        ("deepseek", "chat", true),
        ("openrouter", "deepseek/deepseek-v4-pro", true),
        ("fireworks", "accounts/fireworks/models/deepseek-v4.1-flash", true),
        ("ollama", "deepseek-r1:latest", true),
        ("openai", "gpt-4o", false),
        ("openai", "my-deepseek-helper", false),
        ("openai", "someone/deepseekish", false),
        ("not-deepseek", "qwen", false),
    ] {
        let mut state = state(provider, model);
        feed_characters(&mut state, &input);
        feed(&mut state, &json!({}), Some("stop"));
        let (_, message) = finish(state);
        if expected {
            assert_eq!(calls(&message).len(), 1, "{provider}/{model}");
            assert_eq!(visible(&message), "");
        } else {
            assert!(calls(&message).is_empty(), "{provider}/{model}");
            assert_eq!(visible(&message), input);
        }
    }
}

#[test]
fn native_calls_remain_authoritative_before_after_or_alongside_text_calls() {
    let text = template_call("read", &json!({"path":"same"}));
    let native = json!({"tool_calls": [{
        "index": 0, "id": "native-call",
        "function": {"name": "read", "arguments": "{\"path\":\"same\"}"}
    }]});
    for order in 0..3 {
        let mut state = deepseek_state();
        match order {
            0 => {
                feed(&mut state, &native, None);
                feed(&mut state, &json!({"content": text}), None);
            }
            1 => {
                feed(&mut state, &json!({"content": text}), None);
                feed(&mut state, &native, None);
            }
            _ => {
                let mut combined = native.clone();
                combined["content"] = Value::String(text.clone());
                feed(&mut state, &combined, None);
            }
        }
        feed(&mut state, &json!({}), Some("tool_calls"));
        let (events, message) = finish(state);
        let calls = calls(&message);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "native-call");
        assert_eq!(calls[0].arguments, json!({"path":"same"}));
        assert_eq!(visible(&message), "");
        assert_eq!(events.iter().filter(|event| matches!(event, StreamEvent::ToolCallStart { .. })).count(), 1);
        assert_eq!(events.iter().filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. })).count(), 1);
    }
}

#[test]
fn malformed_envelopes_fail_the_whole_batch_without_leaking_payloads() {
    let malformed = [
        "<｜tool_calls_begin｜><｜tool_call_begin｜>read<｜tool_sep｜>{\"private-key\":\"private-value\"}",
        "<｜tool_call_begin｜>read<｜tool_sep｜>{\"private-key\":<｜tool_call_end｜>",
        "<｜tool_call_begin｜>read<｜tool_sep｜>[]<｜tool_call_end｜>",
        "<｜tool_call_begin｜>read<｜tool_sep｜>{\"path\":\"private-value\",\"pa\\u0074h\":\"second\"}<｜tool_call_end｜>",
        "<｜tool_call_begin｜>invalid name<｜tool_sep｜>{}<｜tool_call_end｜>",
        "<|DSML|tool_calls><|DSML|invoke name=\"read\"><|DSML|parameter name=\"path\" string=\"false\">private-value</|DSML|parameter></|DSML|invoke></|DSML|tool_calls>",
        "<|DSML|tool_calls><|DSML|invoke name=\"read\"><|DSML|parameter name=\"path\">first</|DSML|parameter><|DSML|parameter name=\"path\">second</|DSML|parameter></|DSML|invoke></|DSML|tool_calls>",
        "<|DSML|tool_calls><|DSML|invoke name=\"read\" name=\"write\"></|DSML|invoke></|DSML|tool_calls>",
        "<|DSML|parameter name=\"path\">private-value</|DSML|parameter>",
        "<｜tool_call_begin｜",
    ];
    for malformed in malformed {
        let mut state = deepseek_state();
        feed(&mut state, &json!({"content": "Ordinary introduction. "}), None);
        // A valid peer must not escape before a malformed later call is checked.
        feed(&mut state, &json!({"content": template_call("read", &json!({"path":"ok"}))}), None);
        feed_characters(&mut state, malformed);
        feed(&mut state, &json!({}), Some("stop"));
        let (events, message) = finish(state);
        assert_eq!(message.stop_reason, StopReason::Error, "{malformed}");
        assert!(calls(&message).is_empty());
        assert_eq!(visible(&message), "Ordinary introduction. ");
        assert!(!events.iter().any(|event| matches!(
            event, StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. }
        )));
        let error = message.error_message.expect("controlled diagnostic");
        assert!(error.len() < 256);
        for secret in ["private-key", "private-value", "second", "invalid name"] {
            assert!(!error.contains(secret));
        }
    }
}

#[test]
fn native_calls_remain_authoritative_when_leaked_text_is_malformed_or_oversized() {
    for text in [
        "<｜tool_call_begin｜>read<｜tool_sep｜>{broken".to_string(),
        template_call("write", &json!({"content": "x".repeat(1024 * 1024)})),
    ] {
        let mut state = deepseek_state();
        // The text error can arrive before the authoritative native lane.
        feed(&mut state, &json!({"content": text}), None);
        feed(&mut state, &json!({"tool_calls": [{
            "index": 0, "id": "native", "function": {"name":"read", "arguments":"{}"}
        }]}), Some("stop"));
        let (events, message) = finish(state);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert!(message.error_message.is_none());
        let calls = calls(&message);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "native");
        assert_eq!(calls[0].arguments, json!({}));
        assert_eq!(events.iter().filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. })).count(), 1);
        assert_eq!(visible(&message), "");
    }
}

#[test]
fn failure_and_length_reasons_never_gain_recovered_calls() {
    for (wire_reason, expected) in [
        ("length", StopReason::Length),
        ("content_filter", StopReason::Error),
        ("error", StopReason::Error),
    ] {
        let mut state = deepseek_state();
        feed(&mut state, &json!({"content": template_call("read", &json!({"path":"a"}))}), Some(wire_reason));
        let (events, message) = finish(state);
        assert_eq!(message.stop_reason, expected);
        assert!(calls(&message).is_empty());
        assert!(!events.iter().any(|event| matches!(
            event, StreamEvent::ToolCallStart { .. } | StreamEvent::ToolCallEnd { .. }
        )));
    }

    let mut state = deepseek_state();
    feed(&mut state, &json!({"content": template_call("read", &json!({"path":"a"}))}), None);
    state.partial.stop_reason = StopReason::Aborted;
    let (_, message) = finish(state);
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert!(calls(&message).is_empty());
}

#[test]
fn truncated_call_at_length_is_an_explicit_error() {
    let mut state = deepseek_state();
    feed(&mut state, &json!({"content":"<｜tool_calls_begin｜><｜tool_call_begin｜>read<｜tool_sep｜>{\"path\":\"unfinished"}), Some("length"));
    let (_, message) = finish(state);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert!(calls(&message).is_empty());
    assert_eq!(visible(&message), "");
}

#[test]
fn recovery_preserves_reasoning_and_trailing_usage_and_finalizes_once() {
    let mut state = deepseek_state();
    feed(&mut state, &json!({"reasoning_content":"Inspect the source first."}), None);
    feed(&mut state, &json!({"content": template_call("read", &json!({"path":"a"}))}), Some("stop"));
    feed(&mut state, &json!({}), Some("stop"));
    state.process_event(&json!({
        "choices": [],
        "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}
    }).to_string()).unwrap();
    let (events, message) = finish(state);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.input, 10);
    assert_eq!(message.usage.output, 3);
    assert!(matches!(&message.content[0], ContentBlock::Thinking(thinking)
        if thinking.thinking == "Inspect the source first."));
    assert_eq!(calls(&message).len(), 1);
    assert_eq!(events.iter().filter(|event| matches!(event, StreamEvent::ToolCallEnd { .. })).count(), 1);
}

#[test]
fn recovery_limits_reject_the_whole_response_without_payload_echo() {
    let too_large = template_call("write", &json!({"content": "x".repeat(1024 * 1024)}));
    let too_many = format!(
        "<｜tool_calls_begin｜>{}<｜tool_calls_end｜>",
        "<｜tool_call_begin｜>read<｜tool_sep｜>{}<｜tool_call_end｜>".repeat(129)
    );
    for input in [too_large, too_many] {
        let mut state = deepseek_state();
        feed(&mut state, &json!({"content": input}), Some("stop"));
        let (events, message) = finish(state);
        assert_eq!(message.stop_reason, StopReason::Error);
        assert!(calls(&message).is_empty());
        assert_eq!(visible(&message), "");
        assert!(message.error_message.as_deref().is_some_and(|error| error.len() < 256));
        assert!(!events.iter().any(|event| matches!(event, StreamEvent::ToolCallEnd { .. })));
    }
}

#[test]
fn quoted_marker_with_long_fragmented_body_remains_text() {
    // Exercise incremental quote lookahead without repeatedly rescanning the
    // already received prefix; no tool is authorized by a quoted example.
    let input = format!(
        "`<｜tool_calls_begin｜>{}` Done.",
        "x".repeat(16 * 1024)
    );
    let mut state = deepseek_state();
    feed_characters(&mut state, &input);
    feed(&mut state, &json!({}), Some("stop"));
    let (_, message) = finish(state);
    assert_eq!(visible(&message), input);
    assert!(calls(&message).is_empty());
    assert_eq!(message.stop_reason, StopReason::Stop);
}

#[test]
fn reasoning_envelopes_never_authorize_embedded_tool_calls() {
    for example in [dsml_call("｜"), template_call("write", &json!({"path":"thought"}))] {
        for closed in [true, false] {
            for fragmented in [true, false] {
                let reasoning = format!("<think>Consider this example: {example}");
                let closing = if closed { "</think>" } else { "" };
                let actual = template_call("read", &json!({"path":"actual"}));
                let input = format!("{reasoning}{closing} Then {actual}");
                let mut state = deepseek_state();
                if fragmented {
                    feed_characters(&mut state, &input);
                } else {
                    feed(&mut state, &json!({"content": input}), None);
                }
                feed(&mut state, &json!({}), Some("stop"));
                let (_, message) = finish(state);
                let calls = calls(&message);
                if closed {
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0].name, "read");
                    assert_eq!(calls[0].arguments, json!({"path":"actual"}));
                    assert_eq!(visible(&message), format!("{reasoning}{closing} Then "));
                } else {
                    assert!(calls.is_empty());
                    assert_eq!(visible(&message), input);
                    assert_eq!(message.stop_reason, StopReason::Stop);
                }
            }
        }
    }
}

#[test]
fn captured_tail_respects_thinking_and_quoted_think_tags() {
    let quoted = "The `<think>` tag introduces reasoning. ";
    let actual = template_call("read", &json!({"path":"actual"}));
    let reasoning = format!("<think>Do not execute {}",
        template_call("write", &json!({"path":"thought"})));
    let input = format!("{quoted}{actual}{reasoning}");
    let mut state = deepseek_state();
    feed_characters(&mut state, &input);
    feed(&mut state, &json!({}), Some("stop"));
    let (_, message) = finish(state);
    let calls = calls(&message);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read");
    assert_eq!(visible(&message), format!("{quoted}{reasoning}"));
}
