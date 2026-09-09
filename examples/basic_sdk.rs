//! Basic SDK example: create an agent session and send a prompt programmatically.
//!
//! This demonstrates how to embed Pi as a library crate rather than using the CLI.
//!
//! # Prerequisites
//!
//! Set your API key via environment variable before running:
//!
//! ```sh
//! export ANTHROPIC_API_KEY="sk-..."
//! ```
//!
//! # Running
//!
//! ```sh
//! cargo run --example basic_sdk
//! ```
//!
//! # What this example covers
//!
//! 1. Creating a [`SessionOptions`] with provider/model selection
//! 2. Initializing an in-process agent session via [`create_agent_session`]
//! 3. Sending a prompt and handling streaming [`AgentEvent`]s
//! 4. Inspecting the final [`AssistantMessage`] response
//! 5. Using session-level event listeners for tool execution hooks
//! 6. Querying session state after the prompt completes

use std::sync::{Arc, Mutex};

use pi::sdk::{AgentEvent, AgentSessionHandle, ContentBlock, SessionOptions, create_agent_session};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the async runtime (pi uses asupersync, not tokio).
    let reactor = asupersync::runtime::reactor::create_reactor().expect("failed to create reactor");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .expect("failed to build runtime");

    runtime.block_on(async { run().await })?;
    Ok(())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Configure the session ────────────────────────────────────────
    //
    // SessionOptions mirrors the CLI flags. Unset fields use sensible
    // defaults (e.g. the default provider/model from ~/.config/pi/).
    let options = SessionOptions {
        // Explicitly select a provider and model (optional — omit to use
        // whatever is configured as default).
        provider: Some("anthropic".to_string()),
        model: Some("claude-sonnet-4-20250514".to_string()),

        // Ephemeral session — nothing is persisted to disk.
        no_session: true,

        // Optionally restrict the tool set (None = all built-in tools).
        // Use an empty Vec to disable tools entirely.
        enabled_tools: None,

        // Cap the agentic tool-use loop at 10 iterations for this example.
        max_tool_iterations: 10,

        // Session-level typed hooks for tool execution (fire for every prompt).
        on_tool_start: Some(Arc::new(|tool_name, _args| {
            eprintln!("[hook] tool started: {tool_name}");
        })),
        on_tool_end: Some(Arc::new(|tool_name, _output, is_error| {
            eprintln!("[hook] tool ended: {tool_name} (error={is_error})");
        })),

        ..SessionOptions::default()
    };

    // ── 2. Create the agent session ─────────────────────────────────────
    //
    // This performs the full startup sequence: loads config, resolves auth,
    // selects the provider, builds the system prompt, and registers tools.
    let mut handle: AgentSessionHandle = create_agent_session(options).await?;

    // Print which provider/model was selected.
    let (provider, model_id) = handle.model();
    eprintln!("Using {provider}/{model_id}");

    // ── 3. Register a session-level event listener (optional) ───────────
    //
    // Subscribers receive every AgentEvent for all future prompts.
    // The returned SubscriptionId can be used to unsubscribe later.
    let event_count = Arc::new(Mutex::new(0u64));
    let counter = Arc::clone(&event_count);
    let _sub_id = handle.subscribe(move |_event: AgentEvent| {
        let mut count = counter.lock().expect("lock poisoned");
        *count += 1;
    });

    // ── 4. Send a prompt and handle streaming events ────────────────────
    //
    // The callback receives AgentEvent variants as they arrive:
    //   - AgentStart / AgentEnd (lifecycle)
    //   - TurnStart / TurnEnd (per agentic turn)
    //   - MessageStart / MessageUpdate / MessageEnd (streaming text)
    //   - ToolExecutionStart / ToolExecutionUpdate / ToolExecutionEnd
    let assistant = handle
        .prompt("What is 2 + 2? Reply in one sentence.", |event| {
            match &event {
                AgentEvent::MessageUpdate {
                    assistant_message_event,
                    ..
                } => {
                    // Print streaming text deltas to stderr as they arrive.
                    use pi::model::AssistantMessageEvent;
                    if let AssistantMessageEvent::TextDelta { delta, .. } = assistant_message_event
                    {
                        eprint!("{delta}");
                    }
                }
                AgentEvent::ToolExecutionStart { tool_name, .. } => {
                    eprintln!("\n[event] executing tool: {tool_name}");
                }
                AgentEvent::AgentEnd { .. } => {
                    eprintln!("\n[event] agent finished");
                }
                _ => {}
            }
        })
        .await?;

    // ── 5. Inspect the completed response ───────────────────────────────
    eprintln!("\n--- Final response ---");
    for block in &assistant.content {
        match block {
            ContentBlock::Text(text) => {
                println!("{}", text.text);
            }
            ContentBlock::Thinking(thinking) => {
                eprintln!("[thinking] {}", thinking.thinking);
            }
            ContentBlock::ToolCall(call) => {
                eprintln!("[tool_call] {} -> {}", call.name, call.arguments);
            }
            ContentBlock::Image(_) => {
                eprintln!("[image block]");
            }
            ContentBlock::Media(media) => {
                // Never print `media.data`: it is the whole base64 payload.
                eprintln!(
                    "[media block] {} ({})",
                    media.name.as_deref().unwrap_or("unnamed"),
                    media.mime_type
                );
            }
            ContentBlock::RedactedThinking(_) => {
                eprintln!("[thinking] (redacted)");
            }
        }
    }

    eprintln!(
        "Model: {}/{} | Stop reason: {:?}",
        assistant.provider, assistant.model, assistant.stop_reason
    );
    eprintln!(
        "Tokens — input: {}, output: {}",
        assistant.usage.input, assistant.usage.output
    );

    // ── 6. Query session state ──────────────────────────────────────────
    let state = handle.state().await?;
    eprintln!(
        "Session state: provider={}, model={}, messages={}",
        state.provider, state.model_id, state.message_count
    );

    let total_events = *event_count.lock().expect("lock poisoned");
    eprintln!("Total AgentEvents received by subscriber: {total_events}");

    Ok(())
}
