//! Live E2E smoke tests against a real ollama server.
//!
//! These tests are **skipped** unless ollama is reachable at `http://127.0.0.1:11434`.
//! They exercise the full provider stack: `create_provider` → stream → collect events.
//!
//! To run:
//!   1. `ollama serve` (if not already running)
//!   2. `ollama pull qwen2.5:0.5b`
//!   3. `cargo test --test e2e_ollama_live`

use asupersync::runtime::RuntimeBuilder;
use futures::StreamExt;
use pi::model::{Message, StreamEvent, UserContent, UserMessage};
use pi::models::ModelEntry;
use pi::provider::{Context, InputType, Model, ModelCost, StreamOptions};
use pi::providers::create_provider;
use std::collections::HashMap;

const OLLAMA_BASE: &str = "http://127.0.0.1:11434/v1";
const OLLAMA_MODEL: &str = "qwen2.5:0.5b";

/// Check whether ollama is reachable. Returns true if the models endpoint responds.
fn ollama_available() -> bool {
    std::net::TcpStream::connect_timeout(
        &"127.0.0.1:11434".parse().unwrap(),
        std::time::Duration::from_secs(2),
    )
    .is_ok_and(|stream| {
        let _ = stream.shutdown(std::net::Shutdown::Both);
        true
    })
}

fn ollama_entry() -> ModelEntry {
    ModelEntry {
        model: Model {
            id: OLLAMA_MODEL.to_string(),
            name: OLLAMA_MODEL.to_string(),
            api: "openai-completions".to_string(),
            provider: "ollama".to_string(),
            base_url: OLLAMA_BASE.to_string(),
            reasoning: false,
            input: vec![InputType::Text],
            cost: ModelCost {
                audio: None,
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 32768,
            max_tokens: 512,
            headers: HashMap::new(),
        },
        api_key: Some("ollama-no-key-needed".to_string()),
        headers: HashMap::new(),
        auth_header: false,
        compat: None,
        oauth_config: None,
    }
}

fn simple_context(prompt: &str) -> Context<'static> {
    Context::owned(
        Some("You are a helpful assistant. Be concise.".to_string()),
        vec![Message::User(UserMessage {
            content: UserContent::Text(prompt.to_string()),
            timestamp: 0,
        })],
        Vec::new(),
    )
}

fn stream_options() -> StreamOptions {
    StreamOptions {
        temperature: Some(0.0),
        max_tokens: Some(128),
        // Ollama doesn't need auth but the OpenAI-compat provider requires a non-empty key.
        api_key: Some("ollama-no-key-needed".to_string()),
        ..Default::default()
    }
}

/// Smoke test: ollama provider can be created and streams a text response.
#[test]
fn live_ollama_simple_text_streaming() {
    if !ollama_available() {
        eprintln!("SKIP: ollama not reachable at 127.0.0.1:11434");
        return;
    }

    let rt = RuntimeBuilder::current_thread().build().expect("runtime");

    rt.block_on(async {
        let entry = ollama_entry();
        let provider = create_provider(&entry, None).expect("create ollama provider");

        assert_eq!(provider.name(), "ollama");
        assert_eq!(provider.model_id(), OLLAMA_MODEL);

        let context = simple_context("What is 2+2? Answer with just the number.");
        let options = stream_options();

        let mut stream = provider
            .stream(&context, &options)
            .await
            .expect("stream start");

        let mut text_deltas = Vec::new();
        let mut got_done = false;
        let mut event_count = 0;

        while let Some(result) = stream.next().await {
            let event = result.expect("stream event");
            event_count += 1;

            match &event {
                StreamEvent::TextDelta { delta, .. } => {
                    text_deltas.push(delta.clone());
                }
                StreamEvent::Done { .. } => {
                    got_done = true;
                    break;
                }
                _ => {}
            }
        }

        let full_text: String = text_deltas.concat();
        eprintln!("ollama response ({event_count} events): {full_text:?}");

        assert!(got_done, "stream must end with Done event");
        assert!(!full_text.is_empty(), "response must not be empty");
        assert!(
            full_text.contains('4'),
            "expected '4' in response to 2+2, got: {full_text:?}"
        );
    });
}

/// Verify streaming produces Start → TextDelta(s) → Done event sequence.
#[test]
fn live_ollama_event_ordering() {
    if !ollama_available() {
        eprintln!("SKIP: ollama not reachable at 127.0.0.1:11434");
        return;
    }

    let rt = RuntimeBuilder::current_thread().build().expect("runtime");

    rt.block_on(async {
        let entry = ollama_entry();
        let provider = create_provider(&entry, None).expect("create ollama provider");

        let context = simple_context("Say hello.");
        let options = stream_options();

        let mut stream = provider
            .stream(&context, &options)
            .await
            .expect("stream start");

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            let event = result.expect("stream event");
            let label = match &event {
                StreamEvent::Start { .. } => "Start",
                StreamEvent::TextStart { .. } => "TextStart",
                StreamEvent::TextDelta { .. } => "TextDelta",
                StreamEvent::TextEnd { .. } => "TextEnd",
                StreamEvent::Done { .. } => "Done",
                _ => "Other",
            };
            events.push(label);
            if label == "Done" {
                break;
            }
        }

        eprintln!("event sequence: {events:?}");

        // Must start with Start and end with Done
        assert_eq!(events.first(), Some(&"Start"), "first event must be Start");
        assert_eq!(events.last(), Some(&"Done"), "last event must be Done");

        // Must have at least one TextDelta
        assert!(events.contains(&"TextDelta"), "must have TextDelta events");
    });
}

/// Provider creation for ollama uses correct API route.
#[test]
fn live_ollama_provider_properties() {
    let entry = ollama_entry();
    let provider = create_provider(&entry, None).expect("create ollama provider");

    assert_eq!(provider.name(), "ollama");
    assert_eq!(provider.model_id(), OLLAMA_MODEL);
    assert_eq!(provider.api(), "openai-completions");
}
