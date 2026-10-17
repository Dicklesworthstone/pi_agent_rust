//! Exercise the extension provider boundary through `QuickJS` and the real agent.
//! Incomplete structured output must never become a local write operation.
#![recursion_limit = "512"]

use futures::StreamExt;
use pi::agent::{Agent, AgentConfig, AgentEvent};
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::model::{ContentBlock, StopReason, StreamEvent};
use pi::provider::{Context, Provider, StreamOptions};
use pi::providers::create_provider;
use pi::tools::ToolRegistry;
use std::sync::{Arc, Mutex};

const EXTENSION: &str = r#"
export default function init(pi) {
  pi.registerProvider("terminal-fixture", {
    api: "terminal-fixture-api",
    baseUrl: "https://unused.invalid",
    models: [{ id: "terminal-model", name: "Terminal model", contextWindow: 10000,
               maxTokens: 100, input: ["text"] }],
    streamSimple: async function* (model, context, options) {
      const message = {
        role: "assistant", content: [{type: "text", text: "complete"}],
        api: model.api, provider: model.provider, model: model.id,
        usage: {input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0,
                cost: {input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0}},
        stopReason: "stop", timestamp: 0
      };
      // A valid tool or paused turn must be followed by one ordinary answer.
      if (context.messages.some(m => m.role === "assistant")) {
        yield {type: "done", reason: "stop", message};
        return;
      }
      __BODY__
    }
  });
}
"#;

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}

async fn load(body: &str) -> (tempfile::TempDir, ExtensionManager, Arc<dyn Provider>) {
    let root = tempfile::tempdir().unwrap();
    let entry = root.path().join("terminal.mjs");
    std::fs::write(&entry, EXTENSION.replace("__BODY__", body)).unwrap();
    let manager = ExtensionManager::new();
    // The extension itself has no write tool. Only a successfully completed
    // provider tool request can reach the agent's separate write registry.
    let tools = Arc::new(ToolRegistry::new(&[], root.path(), None));
    let js = JsExtensionRuntimeHandle::start(
        PiJsRuntimeConfig {
            cwd: root.path().display().to_string(),
            ..Default::default()
        },
        tools,
        manager.clone(),
    )
    .await
    .unwrap();
    manager.set_js_runtime(js);
    manager
        .load_js_extensions(vec![JsExtensionLoadSpec::from_entry_path(&entry).unwrap()])
        .await
        .unwrap();
    let entries = manager.extension_model_entries();
    let provider = create_provider(&entries[0], Some(&manager)).unwrap();
    (root, manager, provider)
}

fn request_tool(body: &str) -> String {
    format!(
        r#"
      const call = {{type: "toolCall", id: "write-1", name: "write",
                     arguments: {{path: "result.txt", content: "committed"}}}};
      message.content = [call];
      message.stopReason = "toolUse";
      yield {{type: "start", partial: message}};
      yield {{type: "toolcall_start", contentIndex: 0, partial: message}};
      yield {{type: "toolcall_end", contentIndex: 0, toolCall: call, partial: message}};
      {body}
    "#
    )
}

/// The failure text of a run that must not complete. The agent reports a
/// provider-side failure as an assistant message stopped with `Error`, and an
/// `Error` stop returns before any tool dispatch; an `Err` is accepted too.
fn failure_text(result: pi::error::Result<pi::model::AssistantMessage>) -> String {
    match result {
        Err(error) => error.to_string(),
        Ok(message) => {
            assert_eq!(message.stop_reason, StopReason::Error, "{message:?}");
            message
                .error_message
                .expect("an Error stop names its failure")
        }
    }
}

fn agent(provider: Arc<dyn Provider>, root: &std::path::Path) -> Agent {
    Agent::new(
        provider,
        ToolRegistry::new(&["write"], root, None),
        AgentConfig {
            max_tool_iterations: 3,
            ..Default::default()
        },
    )
}

#[test]
fn incomplete_extension_tool_stream_cannot_write_or_emit_execution_start() {
    runtime().block_on(Box::pin(async {
        let (root, _manager, provider) = load(&request_tool("")).await;
        let mut agent = agent(provider, root.path());
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&events);
        let error = failure_text(
            agent
                .run("write the file", move |event| {
                    capture.lock().unwrap().push(event);
                })
                .await,
        );
        assert!(error.contains("PI_EXTENSION_STREAM_INCOMPLETE"), "{error}");
        assert!(!root.path().join("result.txt").exists());
        assert!(
            !events
                .lock()
                .unwrap()
                .iter()
                .any(|event| { matches!(event, AgentEvent::ToolExecutionStart { .. }) })
        );
    }));
}

#[test]
fn valid_explicit_tool_terminal_executes_once_and_then_completes() {
    runtime().block_on(Box::pin(async {
        let body = request_tool("yield {type: 'done', reason: 'toolUse', message};");
        let (root, _manager, provider) = load(&body).await;
        let mut agent = agent(provider, root.path());
        let starts = Arc::new(Mutex::new(0usize));
        let capture = Arc::clone(&starts);
        let result = agent
            .run("write the file", move |event| {
                if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
                    *capture.lock().unwrap() += 1;
                }
            })
            .await
            .unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert_eq!(
            std::fs::read_to_string(root.path().join("result.txt")).unwrap(),
            "committed"
        );
        assert_eq!(*starts.lock().unwrap(), 1);
    }));
}

#[test]
fn truncated_failed_and_inconsistent_terminals_do_not_execute_tools() {
    runtime().block_on(Box::pin(async {
        for terminal in [
            "message.stopReason = 'length'; yield {type:'done', reason:'length', message};",
            "message.errorMessage = 'failed'; yield {type:'done', reason:'toolUse', message};",
            "yield {type:'done', reason:'stop', message};",
            "message.content[0].arguments = 'incomplete'; yield {type:'done', reason:'toolUse', message};",
        ] {
            let (root, _manager, provider) = load(&request_tool(terminal)).await;
            let mut agent = agent(provider, root.path());
            let error = failure_text(agent.run("write the file", |_| {}).await);
            assert!(error.contains("PI_EXTENSION_STREAM_PROTOCOL"), "{error}");
            assert!(!root.path().join("result.txt").exists());
        }
    }));
}

#[test]
fn paused_server_tool_turn_is_replayed_without_local_execution() {
    runtime().block_on(Box::pin(async {
        let body = request_tool(
            "message.stopReason = 'pauseTurn'; yield {type:'done', reason:'pauseTurn', message};",
        );
        let (root, _manager, provider) = load(&body).await;
        let mut agent = agent(provider, root.path());
        let result = agent.run("continue server work", |_| {}).await.unwrap();
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert!(!root.path().join("result.txt").exists());
    }));
}

#[test]
fn text_chunks_still_finish_but_structured_or_mixed_eof_does_not() {
    runtime().block_on(Box::pin(async {
        for (body, failure, expected) in [
            ("yield 'hello '; yield '🦀';", None, "hello 🦀"),
            ("yield {type:'start', partial:message};", Some("INCOMPLETE"), ""),
            ("yield 'text'; yield {type:'done', reason:'stop', message};", Some("PROTOCOL"), ""),
            ("yield {type:'start', partial:message}; yield 'text';", Some("PROTOCOL"), ""),
            ("return;", Some("INCOMPLETE"), ""),
        ] {
            let (_root, _manager, provider) = load(body).await;
            let context = Context::default();
            let options = StreamOptions::default();
            let mut stream = provider.stream(&context, &options).await.unwrap();
            let mut completions = 0;
            let mut errors = 0;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamEvent::Done { message, .. }) => {
                        completions += 1;
                        assert!(failure.is_none());
                        assert!(matches!(&message.content[0], ContentBlock::Text(text) if text.text == expected));
                    }
                    Err(error) => {
                        errors += 1;
                        assert!(error.to_string().contains(failure.expect("unexpected failure")));
                    }
                    _ => {}
                }
            }
            assert_eq!(completions, usize::from(failure.is_none()));
            assert_eq!(errors, usize::from(failure.is_some()));
        }
    }));
}

#[test]
fn explicit_terminal_ends_without_pulling_more_iterator_work() {
    runtime().block_on(Box::pin(async {
        let (_root, _manager, provider) = load(
            "yield {type:'done', reason:'stop', message}; throw new Error('must not be pulled');",
        )
        .await;
        let context = Context::default();
        let options = StreamOptions::default();
        let mut stream = provider.stream(&context, &options).await.unwrap();
        assert!(matches!(
            stream.next().await.unwrap().unwrap(),
            StreamEvent::Done {
                reason: StopReason::Stop,
                ..
            }
        ));
        assert!(stream.next().await.is_none());
    }));
}
