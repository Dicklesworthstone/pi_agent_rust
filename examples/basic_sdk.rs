// Examples are separate crates, so src/lib.rs's `recursion_limit` does not
// reach here; asupersync 0.5.0 nests its runtime future types deeply enough
// that proving `Send` exceeds the default 128. An SDK embedder hitting this in
// their own crate needs the same attribute.
#![recursion_limit = "256"]

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
//! cargo run --example basic_sdk -- --plan "Improve the parser's error messages"
//! ```
//!
//! `--plan` runs a real read-only planning turn, displays the exact proposal,
//! and requires typed confirmation before a separate execution turn. Only
//! scoped file edits are then auto-approved; no process tools are enabled.
//! This example is ephemeral. It does not install the default TUI's commands.
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

    runtime.block_on(Box::pin(run()))?;
    Ok(())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let task = plan_task(std::env::args().skip(1))?;
    // ── 1. Configure the session ────────────────────────────────────────
    //
    // SessionOptions mirrors the CLI flags. Unset fields use sensible
    // defaults (e.g. the default provider/model from ~/.config/pi/).
    let mut options = SessionOptions {
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

    if task.is_some() {
        // Plan submission and tool approval are separate decisions. Keep file
        // mutation denied until this example's explicit human confirmation.
        options.approval_state = Some(pi::approval::ApprovalState::new(
            pi::approval::ApprovalMode::AlwaysAsk, false, Vec::new(),
        ));
        options.enabled_tools = Some(
            ["read", "grep", "find", "ls", "write", "edit", "hashline_edit", "submit_plan"]
                .into_iter().map(str::to_string).collect(),
        );
    }

    // ── 2. Create the agent session ─────────────────────────────────────
    //
    // This performs the full startup sequence: loads config, resolves auth,
    // selects the provider, builds the system prompt, and registers tools.
    let mut handle: AgentSessionHandle = create_agent_session(options).await?;

    // Print which provider/model was selected.
    let (provider, model_id) = handle.model();
    eprintln!("Using {provider}/{model_id}");

    if let Some(task) = task {
        let outcome = run_reviewed_plan(&mut handle, &task).await;
        let shutdown = handle.shutdown_owned_resources().await;
        if !shutdown.completed_cleanly() {
            let issues = shutdown.failures().collect::<Vec<_>>().join("; ");
            eprintln!("Session cleanup incomplete: {issues}");
            outcome?;
            return Err(std::io::Error::other("session cleanup incomplete").into());
        }
        return outcome;
    }

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

fn plan_task(args: impl Iterator<Item = String>) -> Result<Option<String>, std::io::Error> {
    let mut args = args;
    let Some(flag) = args.next() else { return Ok(None) };
    if flag != "--plan" {
        return Err(std::io::Error::other("usage: basic_sdk [--plan <task>]"));
    }
    let task = args.collect::<Vec<_>>().join(" ");
    if task.trim().is_empty() || task.len() > 16 * 1024 {
        return Err(std::io::Error::other("plan task must be nonblank and at most 16 KiB"));
    }
    Ok(Some(task))
}

fn check_plan_change(change: &pi::plan::PlanChange) -> Result<(), std::io::Error> {
    if let pi::plan::PlanPersistence::Unconfirmed { reason } = &change.persistence {
        return Err(std::io::Error::other(format!(
            "Live plan state is {:?}, but saving was not confirmed: {reason}", change.mode,
        )));
    }
    Ok(())
}

async fn run_reviewed_plan(
    handle: &mut AgentSessionHandle,
    task: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead as _, Read as _, Write as _};

    let owner = pi::agent_cx::AgentCx::for_current_or_request();
    check_plan_change(&handle.enter_plan_mode(&owner).await?)?;
    let _ = handle.prompt(format!(
        "Plan this task without making changes: {task}\n\
         Finish by calling submit_plan with the complete plan and its files array."
    ), |_| {}).await?;
    let review = handle.pending_plan_review()?.ok_or_else(|| {
        std::io::Error::other(
            "No proposal was submitted for review; no execution turn started.",
        )
    })?;
    println!("\n--- Exact submitted plan (control characters escaped) ---");
    for line in review.text().split('\n') {
        println!("{}", line.escape_debug());
    }
    println!("\nType approve to accept this plan and allow its scoped file edits. Anything else rejects it.");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    // Bound input and release the stdin guard before any await. EOF rejects.
    std::io::stdin().lock().take(128).read_line(&mut answer)?;
    if answer.trim() != "approve" {
        check_plan_change(&handle.reject_plan_review(&owner, &review).await?)?;
        println!("Plan rejected; no execution turn started.");
        return Ok(());
    }

    // Retain the exact review captured above. Fetching a new one at this point
    // could silently apply an old decision to a replacement submission.
    check_plan_change(&handle.approve_plan_review(&owner, &review).await?)?;
    let policy = handle.session().agent.approval_state().ok_or_else(|| {
        std::io::Error::other("approval policy unavailable; execution was not started")
    })?;
    policy.set_plan_yolo(true);
    let assistant = handle.prompt(
        "Execute the approved plan pinned in context. Only its declared file edits are permitted.",
        |_| {},
    ).await?;
    for block in assistant.content {
        if let ContentBlock::Text(text) = block {
            for line in text.text.split('\n') {
                println!("{}", line.escape_debug());
            }
        }
    }
    check_plan_change(&handle.exit_plan_mode(&owner).await?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_task_is_explicit_and_bounded() {
        assert!(plan_task(std::iter::empty()).unwrap().is_none());
        for args in [vec!["--plan"], vec!["--plan", " "], vec!["--unknown", "task"]] {
            assert!(plan_task(args.into_iter().map(str::to_string)).is_err());
        }
        assert_eq!(
            plan_task(["--plan", "improve", "parser"].into_iter().map(str::to_string)).unwrap(),
            Some("improve parser".to_string()),
        );
        assert!(plan_task(["--plan".to_string(), "x".repeat(16 * 1024 + 1)].into_iter()).is_err());
    }
}
