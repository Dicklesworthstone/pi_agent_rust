//! Unit tests for custom streamSimple provider streaming (bd-izzp).
//!
//! These tests exercise the `ExtensionStreamSimpleProvider` focusing on areas
//! not covered by the inline tests in `src/providers/mod.rs`:
//! - Thinking event translation
//! - Tool call event translation
//! - String-only (legacy compat) streaming
//! - Empty stream handling
//! - Error event type from JS
//! - Context/Options serialization to JS
//! - `TextEnd` event handling
//! - Invalid JSON event rejection
//! - `build_js_context` / `build_js_options` fidelity

use futures::StreamExt;
use pi::agent::{Agent, AgentConfig, AgentEvent};
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::model::{
    AssistantMessageEvent, ContentBlock, Message, StopReason, StreamEvent, UserContent, UserMessage,
};
use pi::provider::{Context, StreamOptions};
use pi::providers::create_provider;
use pi::tools::ToolRegistry;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn load_extension(source: &str) -> (tempfile::TempDir, ExtensionManager) {
    let dir = tempdir().expect("tempdir");
    let entry_path = dir.path().join("ext.mjs");
    std::fs::write(&entry_path, source).expect("write extension");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));

    let js_runtime = JsExtensionRuntimeHandle::start(
        PiJsRuntimeConfig {
            cwd: dir.path().display().to_string(),
            ..Default::default()
        },
        Arc::clone(&tools),
        manager.clone(),
    )
    .await
    .expect("start js runtime");
    manager.set_js_runtime(js_runtime);

    let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
    manager
        .load_js_extensions(vec![spec])
        .await
        .expect("load extension");

    (dir, manager)
}

fn basic_context() -> Context<'static> {
    Context::owned(
        Some("system".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    )
}

fn basic_options() -> StreamOptions {
    StreamOptions {
        api_key: Some("sk-test".to_string()),
        ..Default::default()
    }
}

fn make_runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build")
}

/// Collect all stream events into a Vec.
async fn collect_events(
    provider: &dyn pi::provider::Provider,
    ctx: &Context<'_>,
    opts: &StreamOptions,
) -> Vec<Result<StreamEvent, pi::error::Error>> {
    let mut stream = provider.stream(ctx, opts).await.expect("stream");
    let mut events = Vec::new();
    while let Some(item) = stream.next().await {
        let is_terminal = matches!(
            &item,
            Ok(StreamEvent::Done { .. } | StreamEvent::Error { .. }) | Err(_)
        );
        events.push(item);
        if is_terminal {
            break;
        }
    }
    events
}

// ---------------------------------------------------------------------------
// JS Extension Sources
// ---------------------------------------------------------------------------

const THINKING_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("thinking-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "thinking-model", name: "Thinking Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      const partial = {
        role: "assistant",
        content: [
          { type: "thinking", thinking: "", thinkingSignature: null },
          { type: "text", text: "" }
        ],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      yield { type: "start", partial };
      yield { type: "thinking_start", contentIndex: 0, partial };
      partial.content[0].thinking = "Let me think about this...";
      yield { type: "thinking_delta", contentIndex: 0, delta: "Let me think about this...", partial };
      yield { type: "thinking_end", contentIndex: 0, content: "Let me think about this...", partial };
      yield { type: "text_start", contentIndex: 1, partial };
      partial.content[1].text = "The answer is 42.";
      yield { type: "text_delta", contentIndex: 1, delta: "The answer is 42.", partial };
      yield { type: "text_end", contentIndex: 1, content: "The answer is 42.", partial };
      yield { type: "done", reason: "stop", message: partial };
    }
  });
}
"#;

const TOOL_CALL_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("toolcall-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "toolcall-model", name: "ToolCall Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      const partial = {
        role: "assistant",
        content: [
          { type: "toolCall", id: "tc_1", name: "read", arguments: {} }
        ],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "toolUse",
        timestamp: 0
      };

      yield { type: "start", partial };
      yield { type: "toolcall_start", contentIndex: 0, partial };
      yield { type: "toolcall_delta", contentIndex: 0, delta: '{"path":"test.txt"}', partial };
      partial.content[0].arguments = { path: "test.txt" };
      yield {
        type: "toolcall_end",
        contentIndex: 0,
        toolCall: { id: "tc_1", name: "read", arguments: { path: "test.txt" } },
        partial
      };
      yield { type: "done", reason: "toolUse", message: partial };
    }
  });
}
"#;

const STRING_ONLY_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("string-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "string-model", name: "String Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      yield "Hello";
      yield ", ";
      yield "world";
      yield "!";
    }
  });
}
"#;

const EMPTY_STREAM_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("empty-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "empty-model", name: "Empty Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      // Yields nothing — immediate end.
    }
  });
}
"#;

const ERROR_EVENT_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("error-event-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "error-event-model", name: "Error Event Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      const partial = {
        role: "assistant",
        content: [{ type: "text", text: "" }],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      yield { type: "start", partial };
      yield { type: "error", reason: "stop", error: partial };
    }
  });
}
"#;

const CONTEXT_ECHO_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("echo-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "echo-model", name: "Echo Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      // Echo context/options back as text content for assertion.
      const info = JSON.stringify({
        hasSystemPrompt: !!context.systemPrompt,
        systemPrompt: context.systemPrompt || null,
        messageCount: context.messages.length,
        toolCount: (context.tools || []).length,
        hasApiKey: !!options.apiKey,
        hasSessionId: !!options.sessionId,
        cacheRetention: options.cacheRetention || null,
      });

      const partial = {
        role: "assistant",
        content: [{ type: "text", text: info }],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      yield { type: "start", partial };
      yield { type: "text_start", contentIndex: 0, partial };
      yield { type: "text_delta", contentIndex: 0, delta: info, partial };
      yield { type: "done", reason: "stop", message: partial };
    }
  });
}
"#;

const INVALID_EVENT_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("invalid-event-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "invalid-model", name: "Invalid Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      // Yield a non-string, non-valid-event object.
      yield { type: "nonexistent_event_type", foo: "bar" };
    }
  });
}
"#;

const TEXT_END_EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("textend-provider", {
    baseUrl: "https://api.example.test",
    apiKey: "EXAMPLE_KEY",
    api: "custom-api",
    models: [
      { id: "textend-model", name: "TextEnd Model", contextWindow: 100, maxTokens: 10, input: ["text"] }
    ],
    streamSimple: async function* (model, context, options) {
      const partial = {
        role: "assistant",
        content: [{ type: "text", text: "" }],
        api: model.api,
        provider: model.provider,
        model: model.id,
        usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
        stopReason: "stop",
        timestamp: 0
      };

      yield { type: "start", partial };
      yield { type: "text_start", contentIndex: 0, partial };
      partial.content[0].text = "chunk1";
      yield { type: "text_delta", contentIndex: 0, delta: "chunk1", partial };
      partial.content[0].text = "chunk1chunk2";
      yield { type: "text_delta", contentIndex: 0, delta: "chunk2", partial };
      yield { type: "text_end", contentIndex: 0, content: "chunk1chunk2", partial };
      yield { type: "done", reason: "stop", message: partial };
    }
  });
}
"#;

// ---------------------------------------------------------------------------
// Tests: Thinking events
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_thinking_events_translate_correctly() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(THINKING_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "thinking-provider")
            .expect("thinking-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        let mut saw_thinking_start = false;
        let mut saw_thinking_delta = false;
        let mut saw_thinking_end = false;
        let mut thinking_delta_text = String::new();
        let mut thinking_end_content = String::new();

        for event in &events {
            match event.as_ref().expect("event ok") {
                StreamEvent::ThinkingStart { content_index, .. } => {
                    assert_eq!(*content_index, 0);
                    saw_thinking_start = true;
                }
                StreamEvent::ThinkingDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    assert_eq!(*content_index, 0);
                    thinking_delta_text = delta.clone();
                    saw_thinking_delta = true;
                }
                StreamEvent::ThinkingEnd {
                    content_index,
                    content,
                    ..
                } => {
                    assert_eq!(*content_index, 0);
                    thinking_end_content = content.clone();
                    saw_thinking_end = true;
                }
                _ => {}
            }
        }

        assert!(saw_thinking_start, "expected ThinkingStart event");
        assert!(saw_thinking_delta, "expected ThinkingDelta event");
        assert!(saw_thinking_end, "expected ThinkingEnd event");
        assert_eq!(thinking_delta_text, "Let me think about this...");
        assert_eq!(thinking_end_content, "Let me think about this...");
    });
}

#[test]
fn stream_simple_thinking_followed_by_text() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(THINKING_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "thinking-provider")
            .expect("thinking-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        // Verify thinking comes before text in event order.
        let mut saw_thinking_end = false;
        let mut saw_text_after_thinking = false;
        for event in &events {
            match event.as_ref().expect("event ok") {
                StreamEvent::ThinkingEnd { .. } => saw_thinking_end = true,
                StreamEvent::TextDelta { delta, .. } if saw_thinking_end => {
                    assert_eq!(delta, "The answer is 42.");
                    saw_text_after_thinking = true;
                }
                _ => {}
            }
        }
        assert!(saw_text_after_thinking, "expected text after thinking");
    });
}

// ---------------------------------------------------------------------------
// Tests: Tool call events
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_tool_call_events_translate_correctly() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(TOOL_CALL_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "toolcall-provider")
            .expect("toolcall-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        let mut saw_toolcall_start = false;
        let mut saw_toolcall_delta = false;
        let mut saw_toolcall_end = false;
        let mut toolcall_delta_text = String::new();
        let mut tool_call_name = String::new();
        let mut tool_call_id = String::new();
        let mut done_reason = StopReason::Stop;

        for event in &events {
            match event.as_ref().expect("event ok") {
                StreamEvent::ToolCallStart { content_index, .. } => {
                    assert_eq!(*content_index, 0);
                    saw_toolcall_start = true;
                }
                StreamEvent::ToolCallDelta {
                    content_index,
                    delta,
                    ..
                } => {
                    assert_eq!(*content_index, 0);
                    toolcall_delta_text = delta.clone();
                    saw_toolcall_delta = true;
                }
                StreamEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    ..
                } => {
                    assert_eq!(*content_index, 0);
                    tool_call_name = tool_call.name.clone();
                    tool_call_id = tool_call.id.clone();
                    saw_toolcall_end = true;
                }
                StreamEvent::Done { reason, .. } => {
                    done_reason = *reason;
                }
                _ => {}
            }
        }

        assert!(saw_toolcall_start, "expected ToolCallStart event");
        assert!(saw_toolcall_delta, "expected ToolCallDelta event");
        assert!(saw_toolcall_end, "expected ToolCallEnd event");
        assert_eq!(toolcall_delta_text, "{\"path\":\"test.txt\"}");
        assert_eq!(tool_call_name, "read");
        assert_eq!(tool_call_id, "tc_1");
        assert_eq!(done_reason, StopReason::ToolUse);
    });
}

// ---------------------------------------------------------------------------
// Tests: String-only (legacy compat) streaming
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_string_chunks_map_to_text_deltas() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(STRING_ONLY_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "string-provider")
            .expect("string-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        let mut deltas = Vec::new();
        let mut final_text = String::new();
        for event in &events {
            match event.as_ref().expect("event ok") {
                StreamEvent::TextDelta { delta, .. } => deltas.push(delta.clone()),
                StreamEvent::Done { message, .. } => {
                    let ContentBlock::Text(text) = &message.content[0] else {
                        panic!("expected text content");
                    };
                    final_text = text.text.clone();
                }
                _ => {}
            }
        }

        assert_eq!(deltas, vec!["Hello", ", ", "world", "!"]);
        assert_eq!(final_text, "Hello, world!");
    });
}

#[test]
fn stream_simple_string_chunks_accumulate_in_partials() {
    make_runtime().block_on(async move {
        let (dir, manager) = load_extension(STRING_ONLY_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "string-provider")
            .expect("string-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let tools = ToolRegistry::new(&[], dir.path(), None);
        let mut agent = Agent::new(provider, tools, AgentConfig::default());
        let accumulated_texts = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&accumulated_texts);

        let final_message = agent
            .run("hello", move |event| {
                if let AgentEvent::MessageUpdate {
                    assistant_message_event: AssistantMessageEvent::TextDelta { partial, .. },
                    ..
                } = event
                {
                    let text = match partial.content.first() {
                        Some(ContentBlock::Text(text)) => text.text.clone(),
                        other => panic!("expected text content block, got {other:?}"),
                    };
                    captured.lock().expect("lock accumulated texts").push(text);
                }
            })
            .await
            .expect("agent run");

        assert_eq!(
            accumulated_texts
                .lock()
                .expect("lock accumulated texts")
                .as_slice(),
            vec!["Hello", "Hello, ", "Hello, world", "Hello, world!"]
        );
        let ContentBlock::Text(text) = &final_message.content[0] else {
            panic!("expected text content");
        };
        assert_eq!(text.text, "Hello, world!");
    });
}

// ---------------------------------------------------------------------------
// Tests: Empty stream
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_empty_stream_emits_done() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(EMPTY_STREAM_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "empty-provider")
            .expect("empty-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        // An empty stream should still emit a Done event.
        assert!(
            !events.is_empty(),
            "empty stream should emit at least a Done event"
        );

        let last = events.last().expect("at least one event");
        match last.as_ref().expect("event ok") {
            StreamEvent::Done { reason, message } => {
                assert_eq!(*reason, StopReason::Stop);
                // Done message should have empty text content.
                let ContentBlock::Text(text) = &message.content[0] else {
                    panic!("expected text content");
                };
                assert_eq!(text.text, "");
            }
            other => panic!("expected Done event, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// Tests: Error event type
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_error_event_from_js_translates() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(ERROR_EVENT_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "error-event-provider")
            .expect("error-event-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        let mut saw_error_event = false;
        for event in &events {
            if let Ok(StreamEvent::Error { reason, .. }) = event {
                assert_eq!(*reason, StopReason::Stop);
                saw_error_event = true;
            }
        }
        assert!(saw_error_event, "expected an Error stream event");
    });
}

// ---------------------------------------------------------------------------
// Tests: TextEnd event
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_text_end_event_carries_full_content() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(TEXT_END_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "textend-provider")
            .expect("textend-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        let mut saw_text_end = false;
        let mut text_end_content = String::new();
        for event in &events {
            if let Ok(StreamEvent::TextEnd { content, .. }) = event {
                text_end_content = content.clone();
                saw_text_end = true;
            }
        }
        assert!(saw_text_end, "expected TextEnd event");
        assert_eq!(text_end_content, "chunk1chunk2");
    });
}

#[test]
fn stream_simple_text_end_preceded_by_deltas() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(TEXT_END_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "textend-provider")
            .expect("textend-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        let mut deltas = Vec::new();
        for event in &events {
            if let Ok(StreamEvent::TextDelta { delta, .. }) = event {
                deltas.push(delta.clone());
            }
        }
        assert_eq!(deltas, vec!["chunk1", "chunk2"]);
    });
}

// ---------------------------------------------------------------------------
// Tests: Invalid JSON event
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_invalid_event_type_returns_error() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(INVALID_EVENT_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "invalid-event-provider")
            .expect("invalid-event-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        let mut saw_error = false;
        for event in &events {
            if let Err(err) = event {
                let msg = err.to_string();
                assert!(
                    msg.contains("invalid event") || msg.contains("streamSimple"),
                    "error should mention invalid event, got: {msg}"
                );
                saw_error = true;
            }
        }
        assert!(saw_error, "expected error for invalid event type");
    });
}

// ---------------------------------------------------------------------------
// Tests: Context and options serialization
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_context_passes_system_prompt_and_messages() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(CONTEXT_ECHO_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "echo-provider")
            .expect("echo-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let ctx = Context::owned(
            Some("test system prompt".to_string()),
            vec![
                Message::User(UserMessage {
                    content: UserContent::Text("msg1".to_string()),
                    timestamp: 0,
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("msg2".to_string()),
                    timestamp: 1,
                }),
            ],
            Vec::new(),
        );
        let opts = StreamOptions {
            api_key: Some("sk-test".to_string()),
            session_id: Some("sess-123".to_string()),
            ..Default::default()
        };
        let events = collect_events(provider.as_ref(), &ctx, &opts).await;

        // Find the TextDelta with the echoed info.
        let mut echo_json = String::new();
        for event in &events {
            if let Ok(StreamEvent::TextDelta { delta, .. }) = event {
                echo_json = delta.clone();
            }
        }

        let info: serde_json::Value = serde_json::from_str(&echo_json).expect("parse echo JSON");
        assert_eq!(info["hasSystemPrompt"], true);
        assert_eq!(info["systemPrompt"], "test system prompt");
        assert_eq!(info["messageCount"], 2);
        assert_eq!(info["hasApiKey"], true);
        assert_eq!(info["hasSessionId"], true);
    });
}

#[test]
fn stream_simple_context_without_system_prompt() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(CONTEXT_ECHO_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "echo-provider")
            .expect("echo-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let ctx = Context::owned(
            None,
            vec![Message::User(UserMessage {
                content: UserContent::Text("hello".to_string()),
                timestamp: 0,
            })],
            Vec::new(),
        );
        let opts = basic_options();
        let events = collect_events(provider.as_ref(), &ctx, &opts).await;

        let mut echo_json = String::new();
        for event in &events {
            if let Ok(StreamEvent::TextDelta { delta, .. }) = event {
                echo_json = delta.clone();
            }
        }

        let info: serde_json::Value = serde_json::from_str(&echo_json).expect("parse echo JSON");
        assert_eq!(info["hasSystemPrompt"], false);
        assert!(info["systemPrompt"].is_null());
        assert_eq!(info["messageCount"], 1);
    });
}

#[test]
fn stream_simple_cache_retention_passed_to_options() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(CONTEXT_ECHO_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "echo-provider")
            .expect("echo-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let ctx = basic_context();
        let opts = StreamOptions {
            api_key: Some("sk-test".to_string()),
            cache_retention: pi::provider::CacheRetention::Long,
            ..Default::default()
        };
        let events = collect_events(provider.as_ref(), &ctx, &opts).await;

        let mut echo_json = String::new();
        for event in &events {
            if let Ok(StreamEvent::TextDelta { delta, .. }) = event {
                echo_json = delta.clone();
            }
        }

        let info: serde_json::Value = serde_json::from_str(&echo_json).expect("parse echo JSON");
        assert_eq!(info["cacheRetention"], "long");
    });
}

// ---------------------------------------------------------------------------
// Tests: Provider metadata
// ---------------------------------------------------------------------------

#[test]
fn stream_simple_provider_model_fields_match_registered() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(THINKING_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "thinking-provider")
            .expect("thinking-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        assert_eq!(provider.name(), "thinking-provider");
        assert_eq!(provider.model_id(), "thinking-model");
        assert_eq!(provider.api(), "custom-api");
    });
}

#[test]
fn stream_simple_done_message_contains_model_info() {
    make_runtime().block_on(async move {
        let (_dir, manager) = load_extension(STRING_ONLY_EXTENSION).await;
        let entries = manager.extension_model_entries();
        let entry = entries
            .iter()
            .find(|e| e.model.provider == "string-provider")
            .expect("string-provider entry");

        let provider = create_provider(entry, Some(&manager)).expect("create provider");
        let events = collect_events(provider.as_ref(), &basic_context(), &basic_options()).await;

        for event in &events {
            if let Ok(StreamEvent::Done { message, .. }) = event {
                assert_eq!(message.model, "string-model");
                assert_eq!(message.provider, "string-provider");
                return;
            }
        }
        panic!("no Done event found");
    });
}

#[test]
fn two_extension_streams_with_colliding_inner_ids_stay_routed_to_their_owner() {
    let scenario = async move {
        let dir = tempdir().expect("tempdir");
        let workspace = dir.path().join("workspace");
        let ext_a_dir = dir.path().join("stream-a");
        let ext_b_dir = dir.path().join("stream-b");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        std::fs::create_dir_all(&ext_a_dir).expect("extension A dir");
        std::fs::create_dir_all(&ext_b_dir).expect("extension B dir");

        let ext_a = ext_a_dir.join("index.mjs");
        let ext_b = ext_b_dir.join("index.mjs");
        std::fs::write(
            &ext_a,
            r#"
export default function init(pi) {
  pi.registerProvider("collision-provider-a", {
    baseUrl: "https://a.example.test",
    apiKey: "EXAMPLE_KEY_A",
    api: "custom-api",
    models: [{ id: "collision-model-a", name: "A", contextWindow: 100, maxTokens: 10, input: ["text"] }],
    streamSimple: async function* () {
      yield "A-1";
      await Promise.resolve();
      yield "A-2";
    },
  });
}
"#,
        )
        .expect("write extension A");
        std::fs::write(
            &ext_b,
            r#"
export default function init(pi) {
  pi.registerProvider("collision-provider-b", {
    baseUrl: "https://b.example.test",
    apiKey: "EXAMPLE_KEY_B",
    api: "custom-api",
    models: [{ id: "collision-model-b", name: "B", contextWindow: 100, maxTokens: 10, input: ["text"] }],
    streamSimple: async function* () {
      yield "B-1";
      await Promise.resolve();
      yield "B-2";
    },
  });
}
"#,
        )
        .expect("write extension B");

        let manager = ExtensionManager::new();
        let tools = Arc::new(ToolRegistry::new(&[], &workspace, None));
        let runtime = JsExtensionRuntimeHandle::start(
            PiJsRuntimeConfig {
                cwd: workspace.display().to_string(),
                ..Default::default()
            },
            Arc::clone(&tools),
            manager.clone(),
        )
        .await
        .expect("start isolated runtime coordinator");
        manager.set_js_runtime(runtime);
        manager
            .load_js_extensions(vec![
                JsExtensionLoadSpec::from_entry_path(&ext_a).expect("extension A spec"),
                JsExtensionLoadSpec::from_entry_path(&ext_b).expect("extension B spec"),
            ])
            .await
            .expect("load both stream providers");

        let entries = manager.extension_model_entries();
        let entry_a = entries
            .iter()
            .find(|entry| entry.model.provider == "collision-provider-a")
            .expect("provider A entry");
        let entry_b = entries
            .iter()
            .find(|entry| entry.model.provider == "collision-provider-b")
            .expect("provider B entry");
        let provider_a = create_provider(entry_a, Some(&manager)).expect("provider A");
        let provider_b = create_provider(entry_b, Some(&manager)).expect("provider B");
        let context = basic_context();
        let options = basic_options();

        // Start both before polling either. Each fresh shard allocates its
        // first JS-local id (`provider-stream-1`); the coordinator must expose
        // distinct opaque routes and keep next/cancel operations owner-bound.
        let mut stream_a = provider_a
            .stream(&context, &options)
            .await
            .expect("start provider A stream");
        let mut stream_b = provider_b
            .stream(&context, &options)
            .await
            .expect("start provider B stream");

        let mut text_a = String::new();
        let mut text_b = String::new();
        for _ in 0..8 {
            let event = stream_a
                .next()
                .await
                .expect("provider A event")
                .expect("provider A event result");
            if let StreamEvent::TextDelta { delta, .. } = event {
                text_a.push_str(&delta);
            }
            if text_a == "A-1" {
                break;
            }
        }
        for _ in 0..8 {
            let event = stream_b
                .next()
                .await
                .expect("provider B event")
                .expect("provider B event result");
            if let StreamEvent::TextDelta { delta, .. } = event {
                text_b.push_str(&delta);
            }
            if text_b == "B-1" {
                break;
            }
        }
        assert_eq!(text_a, "A-1", "provider A received a peer chunk");
        assert_eq!(text_b, "B-1", "provider B received a peer chunk");

        // Dropping A enqueues cancellation for its opaque route. The next B
        // poll follows that cancellation on the same coordinator channel, so
        // a route keyed only by the colliding inner id would cancel B here.
        drop(stream_a);

        let mut done_b = false;
        for _ in 0..12 {
            let event = stream_b
                .next()
                .await
                .expect("provider B event after peer cancellation")
                .expect("provider B event result after peer cancellation");
            match event {
                StreamEvent::TextDelta { delta, .. } => text_b.push_str(&delta),
                StreamEvent::Done { .. } => done_b = true,
                _ => {}
            }
            if done_b {
                break;
            }
        }

        assert!(done_b, "provider B must survive cancellation of provider A");
        assert_eq!(text_b, "B-1B-2", "provider B received peer chunks");
        assert!(manager.shutdown(Duration::from_secs(3)).await);
    };

    make_runtime()
        .block_on(async move {
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(8),
                scenario,
            )
            .await
        })
        .expect("provider collision isolation scenario exceeded its independent 8-second bound");
}
