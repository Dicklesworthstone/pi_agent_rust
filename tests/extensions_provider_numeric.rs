//! End-to-end numeric decoding regressions for extension providers (gh #238).
//!
//! Unlike the JSON-only normalization tests, these exercise `QuickJS` number
//! tagging, the runtime coordinator, and the typed provider stream adapter.

#![recursion_limit = "1024"]

use futures::StreamExt;
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::model::{Message, StopReason, StreamEvent, UserContent, UserMessage};
use pi::provider::{Context, StreamOptions};
use pi::providers::create_provider;
use pi::tools::ToolRegistry;
use std::sync::Arc;

const NUMERIC_PROVIDER: &str = r#"
export default function (pi) {
  pi.registerProvider("numeric-provider", {
    api: "numeric-api",
    baseUrl: "https://unused.invalid",
    apiKey: "test",
    models: [{
      id: "numeric-model", name: "Numeric regression", reasoning: false,
      input: ["text"], contextWindow: 4096, maxTokens: 1024,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }
    }],
    async *streamSimple(model, _context, options) {
      const message = {
        role: "assistant", content: [], api: model.api,
        provider: model.provider, model: model.id,
        timestamp: options.sessionId === "live" ? Date.now() : 1789918884239,
        usage: {
          input: 2147483648, output: 1, cacheRead: 0, cacheWrite: 0,
          totalTokens: 2147483649,
          cost: { input: 0.125, output: 0, cacheRead: 0, cacheWrite: 0, total: 0.125 }
        },
        stopReason: "stop"
      };
      if (options.sessionId === "error") {
        message.stopReason = "error";
        message.errorMessage = "intentional provider error";
        yield { type: "error", reason: "error", error: message };
        return;
      }
      yield { type: "start", partial: message };
      yield { type: "done", reason: "stop", message };
    }
  });
}
"#;

fn make_runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build")
}

async fn run_case(case: &str) -> Vec<Result<StreamEvent, pi::error::Error>> {
    let dir = tempfile::tempdir().expect("tempdir");
    let entry_path = dir.path().join("numeric.mjs");
    std::fs::write(&entry_path, NUMERIC_PROVIDER).expect("write numeric provider");
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], dir.path(), None));
    let runtime = JsExtensionRuntimeHandle::start(
        PiJsRuntimeConfig {
            cwd: dir.path().display().to_string(),
            ..Default::default()
        },
        tools,
        manager.clone(),
    )
    .await
    .expect("start QuickJS runtime");
    manager.set_js_runtime(runtime);
    let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
    manager
        .load_js_extensions(vec![spec])
        .await
        .expect("load numeric provider");
    let entries = manager.extension_model_entries();
    let entry = entries
        .iter()
        .find(|entry| entry.model.provider == "numeric-provider")
        .expect("numeric provider entry");
    let provider = create_provider(entry, Some(&manager)).expect("create numeric provider");
    let context = Context::owned(
        Some("system".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text("hello".to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    );
    let options = StreamOptions {
        api_key: Some("sk-test".to_string()),
        session_id: Some(case.to_string()),
        ..Default::default()
    };
    let mut stream = provider.stream(&context, &options).await.expect("stream");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        let terminal = matches!(
            &event,
            Ok(StreamEvent::Done { .. } | StreamEvent::Error { .. }) | Err(_)
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

#[test]
fn millisecond_timestamps_and_large_token_counts_survive_streaming() {
    make_runtime().block_on(async {
        let events = run_case("fixed").await;
        assert_eq!(events.len(), 2);
        let StreamEvent::Start { partial } = events[0].as_ref().expect("start event") else {
            panic!("expected Start, got {:?}", events[0]);
        };
        assert_eq!(partial.timestamp, 1_789_918_884_239);
        let StreamEvent::Done { reason, message } = events[1].as_ref().expect("done event") else {
            panic!("expected Done, got {:?}", events[1]);
        };
        assert_eq!(*reason, StopReason::Stop);
        assert_eq!(message.timestamp, 1_789_918_884_239);
        let usage = serde_json::to_value(&message.usage).expect("serialize usage");
        assert_eq!(usage["input"], serde_json::json!(2_147_483_648_u64));
        assert_eq!(usage["totalTokens"], serde_json::json!(2_147_483_649_u64));
        assert_eq!(usage["cost"]["total"], serde_json::json!(0.125));
    });
}

#[test]
fn live_date_now_timestamp_survives_streaming() {
    make_runtime().block_on(async {
        let events = run_case("live").await;
        let StreamEvent::Done { message, .. } = events
            .last()
            .expect("terminal event")
            .as_ref()
            .expect("valid event")
        else {
            panic!("expected Done, got {events:?}");
        };
        assert!(message.timestamp > i64::from(i32::MAX));
    });
}

#[test]
fn timestamp_does_not_mask_the_providers_original_error() {
    make_runtime().block_on(async {
        let events = run_case("error").await;
        assert_eq!(events.len(), 1);
        let StreamEvent::Error { reason, error } = events[0].as_ref().expect("valid error event")
        else {
            panic!("expected provider Error, got {:?}", events[0]);
        };
        assert_eq!(*reason, StopReason::Error);
        assert_eq!(error.timestamp, 1_789_918_884_239);
        assert_eq!(
            error.error_message.as_deref(),
            Some("intentional provider error")
        );
    });
}
