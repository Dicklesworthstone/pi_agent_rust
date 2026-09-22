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
//! cargo run --example basic_sdk -- --save-plan ./plan-sessions "Improve parsing"
//! cargo run --example basic_sdk -- --resume-plan ./plan-sessions/<session>.jsonl
//! ```
//!
//! `--plan` runs a real read-only planning turn, displays the exact proposal,
//! and requires typed confirmation before a separate execution turn. Only
//! scoped file edits are then auto-approved; no process tools are enabled.
//! `--save-plan` creates a new saved session and prints its path. `later` or
//! EOF defers review without execution; `--resume-plan` opens that exact file
//! from the SAME workspace. Previously approved plans require fresh review.
//! Rejected drafts resume read-only revision; an exited plan does not execute.
//! Plan text is stored in the session file and may contain private context.
//! This example does not install the default TUI's commands.
//!
//! # What this example covers
//!
//! 1. Creating a [`SessionOptions`] with provider/model selection
//! 2. Initializing an in-process agent session via [`create_agent_session`]
//! 3. Sending a prompt and handling streaming [`AgentEvent`]s
//! 4. Inspecting the final [`AssistantMessage`] response
//! 5. Using session-level event listeners for tool execution hooks
//! 6. Querying session state after the prompt completes

use std::io;
use std::path::{Path, PathBuf};
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
    let invocation = parse_invocation(std::env::args().skip(1))?;
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

    if invocation != Invocation::Demo {
        // Plan submission and tool approval are separate decisions. Keep file
        // mutation denied until this example's explicit human confirmation.
        options.approval_state = Some(pi::approval::ApprovalState::new(
            pi::approval::ApprovalMode::AlwaysAsk,
            false,
            Vec::new(),
        ));
        options.enabled_tools = Some(
            [
                "read",
                "grep",
                "find",
                "ls",
                "write",
                "edit",
                "hashline_edit",
                "submit_plan",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        );
    }
    let expected_resume = prepare_storage(&invocation, &mut options).await?;

    // ── 2. Create the agent session ─────────────────────────────────────
    //
    // This performs the full startup sequence: loads config, resolves auth,
    // selects the provider, builds the system prompt, and registers tools.
    let mut handle: AgentSessionHandle = create_agent_session(options).await?;

    // Print which provider/model was selected.
    let (provider, model_id) = handle.model();
    eprintln!("Using {provider}/{model_id}");

    if invocation != Invocation::Demo {
        let outcome = run_reviewed_plan(&mut handle, &invocation, expected_resume.as_ref()).await;
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

#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Demo,
    Plan {
        task: String,
        directory: Option<PathBuf>,
    },
    Resume {
        path: PathBuf,
    },
}

fn usage() -> io::Error {
    io::Error::other(
        "usage: basic_sdk [--plan <task> | --save-plan <directory> <task> | --resume-plan <file>]",
    )
}

fn parse_invocation(args: impl Iterator<Item = String>) -> Result<Invocation, io::Error> {
    let mut args = args;
    let Some(flag) = args.next() else {
        return Ok(Invocation::Demo);
    };
    let directory = match flag.as_str() {
        "--plan" => None,
        "--save-plan" => Some(argument_path(args.next().ok_or_else(usage)?)?),
        "--resume-plan" => {
            let path = argument_path(args.next().ok_or_else(usage)?)?;
            if args.next().is_some() {
                return Err(usage());
            }
            return Ok(Invocation::Resume { path });
        }
        _ => return Err(usage()),
    };
    // Bound the accumulated task instead of collecting arbitrary argument
    // counts into an intermediate Vec before checking their total bytes.
    let mut task = String::new();
    for word in args {
        let separator = usize::from(!task.is_empty());
        if task
            .len()
            .saturating_add(separator)
            .saturating_add(word.len())
            > 16 * 1024
        {
            return Err(io::Error::other("plan task exceeds 16 KiB"));
        }
        if separator != 0 {
            task.push(' ');
        }
        task.push_str(&word);
    }
    if task.trim().is_empty() {
        return Err(io::Error::other("plan task must be nonblank"));
    }
    Ok(Invocation::Plan { task, directory })
}

fn argument_path(value: String) -> Result<PathBuf, io::Error> {
    if value.trim().is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(io::Error::other(
            "session path must be nonblank, control-free and at most 4096 UTF-8 bytes",
        ));
    }
    Ok(PathBuf::from(value))
}

struct ResumeIdentity {
    id: String,
    path: PathBuf,
    workspace: PathBuf,
}

fn check_workspace(recorded: &str, workspace: &Path) -> Result<(), io::Error> {
    let recorded = Path::new(recorded);
    if !recorded.is_absolute() || std::fs::canonicalize(recorded)? != workspace {
        return Err(io::Error::other(
            "resume refused: run from the saved session's original workspace; relative file scopes must not move to another root",
        ));
    }
    Ok(())
}

async fn prepare_storage(
    invocation: &Invocation,
    options: &mut SessionOptions,
) -> Result<Option<ResumeIdentity>, Box<dyn std::error::Error>> {
    if *invocation == Invocation::Demo {
        return Ok(None);
    }
    let workspace = std::fs::canonicalize(std::env::current_dir()?)?;
    options.working_directory = Some(workspace.clone());
    match invocation {
        Invocation::Plan {
            directory: Some(directory),
            ..
        } => {
            options.no_session = false;
            options.session_dir = Some(directory.clone());
            Ok(None)
        }
        Invocation::Resume { path } => {
            // Never interpret a missing file as a request for a new session.
            let path = std::fs::canonicalize(path)?;
            if !std::fs::metadata(&path)?.is_file() {
                return Err(io::Error::other("resume requires an existing session file").into());
            }
            let text_path = path
                .to_str()
                .ok_or_else(|| io::Error::other("session path is not UTF-8"))?;
            let saved = pi::sdk::Session::open(text_path).await?;
            check_workspace(&saved.header.cwd, &workspace)?;
            let expected = ResumeIdentity {
                id: saved.header.id.clone(),
                path: path.clone(),
                workspace,
            };
            options.no_session = false;
            options.session_path = Some(path);
            // Let SDK startup resolve the saved model, not this demo's default.
            options.provider = None;
            options.model = None;
            Ok(Some(expected))
        }
        Invocation::Demo
        | Invocation::Plan {
            directory: None, ..
        } => Ok(None),
    }
}

async fn verify_resumed_session(
    handle: &AgentSessionHandle,
    expected: &ResumeIdentity,
) -> Result<(), Box<dyn std::error::Error>> {
    let (id, cwd, path) = handle
        .with_session(|session| {
            (
                session.header.id.clone(),
                session.header.cwd.clone(),
                session.path.clone(),
            )
        })
        .await?;
    check_resume_identity(expected, &id, &cwd, path.as_deref())?;
    Ok(())
}

fn check_resume_identity(
    expected: &ResumeIdentity,
    id: &str,
    cwd: &str,
    path: Option<&Path>,
) -> Result<(), io::Error> {
    let path =
        path.ok_or_else(|| io::Error::other("SDK did not open the requested saved session"))?;
    if id != expected.id || std::fs::canonicalize(path)? != expected.path {
        return Err(io::Error::other(
            "session identity changed during startup; no plan was restored",
        ));
    }
    check_workspace(cwd, &expected.workspace)
}

async fn print_session_path(handle: &AgentSessionHandle) -> Result<(), Box<dyn std::error::Error>> {
    if handle.session().save_enabled()
        && let Some(path) = handle.with_session(|session| session.path.clone()).await?
    {
        eprintln!(
            "Saved plan session: {}",
            path.display().to_string().escape_debug()
        );
    }
    Ok(())
}

fn check_plan_change(change: &pi::plan::PlanChange) -> Result<(), std::io::Error> {
    if let pi::plan::PlanPersistence::Unconfirmed { reason } = &change.persistence {
        return Err(std::io::Error::other(format!(
            "Live plan state is {:?}, but saving was not confirmed: {reason}",
            change.mode,
        )));
    }
    Ok(())
}

async fn run_reviewed_plan(
    handle: &mut AgentSessionHandle,
    invocation: &Invocation,
    expected_resume: Option<&ResumeIdentity>,
) -> Result<(), Box<dyn std::error::Error>> {
    let owner = pi::agent_cx::AgentCx::for_current_or_request();
    match invocation {
        Invocation::Plan { task, .. } => {
            check_plan_change(&handle.enter_plan_mode(&owner).await?)?;
            print_session_path(handle).await?;
            let _ = handle
                .prompt(
                    format!(
                        "Plan this task without making changes: {task}\n\
                 Finish by calling submit_plan with the complete plan and its files array."
                    ),
                    |_| {},
                )
                .await?;
        }
        Invocation::Resume { .. } => {
            let expected =
                expected_resume.ok_or_else(|| io::Error::other("missing resume identity"))?;
            verify_resumed_session(handle, expected).await?;
            let restored = handle.restore_plan_checkpoint(&owner).await?;
            check_plan_change(&restored)?;
            print_session_path(handle).await?;
            match restored.mode {
                pi::plan::PlanMode::PendingApproval => {}
                pi::plan::PlanMode::Planning => {
                    let draft = handle
                        .session()
                        .agent
                        .plan_state()
                        .plan()
                        .unwrap_or_default();
                    let _ = handle.prompt(format!(
                        "Resume read-only planning. Revise and complete the recovered draft below, \
                         then call submit_plan with the full plan and its files array. \
                         Do not execute the plan.\n\n{draft}"
                    ), |_| {}).await?;
                }
                pi::plan::PlanMode::Off => {
                    println!(
                        "This saved plan was exited. No provider turn or execution was started."
                    );
                    return Ok(());
                }
                pi::plan::PlanMode::Approved => {
                    return Err(io::Error::other(
                        "recovery unexpectedly granted approval; execution refused",
                    )
                    .into());
                }
            }
        }
        Invocation::Demo => return Err(usage().into()),
    }
    let review = handle.pending_plan_review()?.ok_or_else(|| {
        std::io::Error::other("No proposal was submitted for review; no execution turn started.")
    })?;
    // A prior submission save might have failed. Flush before asking for an
    // execution decision, without resubmitting or replacing the review identity.
    check_checkpoint(handle.checkpoint_plan(&owner).await?)?;
    review_and_execute(handle, &owner, &review).await
}

fn check_checkpoint(persistence: pi::plan::PlanPersistence) -> Result<(), io::Error> {
    if let pi::plan::PlanPersistence::Unconfirmed { reason } = persistence {
        return Err(io::Error::other(format!(
            "Plan remains live, but checkpoint saving was not confirmed: {reason}"
        )));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum ReviewDecision {
    Approve,
    Reject,
    Later,
}

fn read_decision(input: impl io::BufRead) -> Result<ReviewDecision, io::Error> {
    let mut bytes = Vec::new();
    let mut bounded = io::Read::take(input, 129);
    io::BufRead::read_until(&mut bounded, b'\n', &mut bytes)?;
    if bytes.len() > 128 {
        return Err(io::Error::other(
            "confirmation line exceeds 128 bytes; no decision applied",
        ));
    }
    // A truncated or unterminated prefix must never count as an approval.
    // EOF defers; it does not reject and discard a saved pending proposal.
    if !bytes.ends_with(b"\n") {
        return Ok(ReviewDecision::Later);
    }
    let line = std::str::from_utf8(&bytes)
        .map_err(|_| io::Error::other("confirmation is not valid UTF-8"))?;
    match line.trim() {
        "approve" => Ok(ReviewDecision::Approve),
        "reject" => Ok(ReviewDecision::Reject),
        "later" | "" => Ok(ReviewDecision::Later),
        _ => Err(io::Error::other(
            "expected approve, reject or later; no decision applied",
        )),
    }
}

async fn review_and_execute(
    handle: &mut AgentSessionHandle,
    owner: &pi::agent_cx::AgentCx,
    review: &pi::plan::SessionPlanReview,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write as _;
    println!("\n--- Exact submitted plan (control characters escaped) ---");
    for line in review.text().split('\n') {
        println!("{}", line.escape_debug());
    }
    println!(
        "\nType approve to accept this plan and allow its scoped file edits, reject to revise it, or later to defer."
    );
    std::io::stdout().flush()?;
    // The blocking input guard ends before any await; the review does not.
    let decision = read_decision(std::io::stdin().lock())?;
    match decision {
        ReviewDecision::Later => {
            check_checkpoint(handle.checkpoint_plan(owner).await?)?;
            if handle.session().save_enabled() {
                println!("Review deferred and saved. Resume this session file to review it later.");
            } else {
                println!(
                    "No execution started. This ephemeral plan will not survive exit; use --save-plan to retain a future plan."
                );
            }
            return Ok(());
        }
        ReviewDecision::Reject => {
            check_plan_change(&handle.reject_plan_review(owner, review).await?)?;
            println!(
                "Plan rejected; no execution turn started. Saved sessions resume read-only revision."
            );
            return Ok(());
        }
        ReviewDecision::Approve => {}
    }

    // Retain the exact review captured above. Fetching a new one at this point
    // could silently apply an old decision to a replacement submission.
    check_plan_change(&handle.approve_plan_review(owner, review).await?)?;
    let policy = handle.session().agent.approval_state().ok_or_else(|| {
        std::io::Error::other("approval policy unavailable; execution was not started")
    })?;
    policy.set_plan_yolo(true);
    let assistant = handle.prompt(
        "Execute the approved plan pinned in context. Only its declared file edits are permitted.",
        |_| {},
    ).await?;
    let completed =
        assistant.stop_reason == pi::sdk::StopReason::Stop && !policy.surface_was_unavailable();
    for block in assistant.content {
        if let ContentBlock::Text(text) = block {
            for line in text.text.split('\n') {
                println!("{}", line.escape_debug());
            }
        }
    }
    if !completed {
        return Err(io::Error::other(
            "execution did not finish normally; its checkpoint was retained for fresh review, not marked complete",
        ).into());
    }
    check_plan_change(&handle.exit_plan_mode(owner).await?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_task_is_explicit_and_bounded() {
        assert_eq!(
            parse_invocation(std::iter::empty()).unwrap(),
            Invocation::Demo
        );
        for args in [
            vec!["--plan"],
            vec!["--plan", " "],
            vec!["--unknown", "task"],
        ] {
            assert!(parse_invocation(args.into_iter().map(str::to_string)).is_err());
        }
        assert_eq!(
            parse_invocation(
                ["--plan", "improve", "parser"]
                    .into_iter()
                    .map(str::to_string)
            )
            .unwrap(),
            Invocation::Plan {
                task: "improve parser".to_string(),
                directory: None
            },
        );
        assert!(
            parse_invocation(["--plan".to_string(), "x".repeat(16 * 1024 + 1)].into_iter())
                .is_err()
        );
    }

    #[test]
    fn saved_planning_and_exact_file_resume_have_distinct_required_arguments() {
        let parse = |args: &[&str]| parse_invocation(args.iter().map(|value| (*value).to_string()));
        assert_eq!(
            parse(&["--save-plan", "session folder", "improve", "parser"]).unwrap(),
            Invocation::Plan {
                task: "improve parser".to_string(),
                directory: Some(PathBuf::from("session folder"))
            }
        );
        assert_eq!(
            parse(&["--resume-plan", "session folder/计划.jsonl"]).unwrap(),
            Invocation::Resume {
                path: PathBuf::from("session folder/计划.jsonl")
            }
        );
        for args in [
            vec!["--save-plan"],
            vec!["--save-plan", "directory"],
            vec!["--save-plan", " ", "task"],
            vec!["--resume-plan"],
            vec!["--resume-plan", ""],
            vec!["--resume-plan", "file", "ignored task"],
            vec!["--resume-plan", "bad\npath"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn exact_task_limit_and_following_arguments_never_overflow_the_budget() {
        let exact = "é".repeat(16 * 1024 / 2);
        assert!(parse_invocation(["--plan".to_string(), exact.clone()].into_iter()).is_ok());
        for tail in ["", "x", " "] {
            assert!(
                parse_invocation(
                    ["--plan".to_string(), exact.clone(), tail.to_string()].into_iter()
                )
                .is_err()
            );
        }
        assert!(argument_path("é".repeat(2048)).is_ok());
        assert!(argument_path("é".repeat(2049)).is_err());
    }

    #[test]
    fn review_confirmation_requires_an_explicit_complete_line() {
        for (input, expected) in [
            ("approve\n", ReviewDecision::Approve),
            (" approve\r\n", ReviewDecision::Approve),
            ("reject\n", ReviewDecision::Reject),
            ("later\n", ReviewDecision::Later),
            ("\n", ReviewDecision::Later),
            ("", ReviewDecision::Later),
            ("approve", ReviewDecision::Later),
            ("reject", ReviewDecision::Later),
        ] {
            assert_eq!(
                read_decision(input.as_bytes()).unwrap(),
                expected,
                "{input:?}"
            );
        }
        for input in [
            "yes\n",
            "APPROVE\n",
            "approve another plan\n",
            "approve\0\n",
        ] {
            assert!(read_decision(input.as_bytes()).is_err());
        }
        assert!(read_decision(&b"approve\xff\n"[..]).is_err());
    }

    #[test]
    fn overlong_confirmation_cannot_authorize_from_a_truncated_valid_prefix() {
        let attack = format!("approve{}not approval\n", " ".repeat(128));
        assert!(read_decision(attack.as_bytes()).is_err());
        let exact = format!("approve{}\n", " ".repeat(120));
        assert_eq!(exact.len(), 128);
        assert_eq!(
            read_decision(exact.as_bytes()).unwrap(),
            ReviewDecision::Approve
        );
        let too_long = format!("approve{}\n", " ".repeat(121));
        assert!(read_decision(too_long.as_bytes()).is_err());
    }

    #[test]
    fn review_input_read_failure_is_not_a_decision() {
        struct Broken;
        impl io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("fixture read failure"))
            }
        }
        impl io::BufRead for Broken {
            fn fill_buf(&mut self) -> io::Result<&[u8]> {
                Err(io::Error::other("fixture read failure"))
            }
            fn consume(&mut self, _: usize) {}
        }
        assert!(read_decision(Broken).is_err());
    }

    #[test]
    fn resume_rejects_a_different_missing_or_relative_workspace() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(first.path()).unwrap();
        assert!(check_workspace(first.path().to_str().unwrap(), &root).is_ok());
        assert!(check_workspace(second.path().to_str().unwrap(), &root).is_err());
        assert!(check_workspace(".", &root).is_err());
        assert!(check_workspace(first.path().join("missing").to_str().unwrap(), &root).is_err());
    }

    #[test]
    fn unconfirmed_save_prevents_execution_while_acknowledging_the_live_decision() {
        use pi::plan::{PlanChange, PlanMode, PlanPersistence};
        for persistence in [
            PlanPersistence::Saved,
            PlanPersistence::MemoryOnly,
            PlanPersistence::Unchanged,
        ] {
            assert!(check_checkpoint(persistence.clone()).is_ok());
            assert!(
                check_plan_change(&PlanChange {
                    mode: PlanMode::PendingApproval,
                    changed: true,
                    persistence
                })
                .is_ok()
            );
        }
        let persistence = PlanPersistence::Unconfirmed {
            reason: "disk full".to_string(),
        };
        assert!(
            check_checkpoint(persistence.clone())
                .unwrap_err()
                .to_string()
                .contains("remains live")
        );
        let error = check_plan_change(&PlanChange {
            mode: PlanMode::Approved,
            changed: true,
            persistence,
        })
        .unwrap_err();
        assert!(error.to_string().contains("Approved"));
        assert!(error.to_string().contains("not confirmed"));
    }

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn resume_preflight_reads_an_existing_session_and_retains_its_exact_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("saved plan.jsonl");
        let workspace = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let mut saved = pi::sdk::Session::in_memory();
        saved.header.cwd = workspace.to_str().unwrap().to_string();
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&saved.header).unwrap()),
        )
        .unwrap();
        let invocation = Invocation::Resume { path: path.clone() };
        let mut options = SessionOptions::default();
        let expected = run(prepare_storage(&invocation, &mut options))
            .unwrap()
            .unwrap();
        assert_eq!(expected.id, saved.header.id);
        assert_eq!(expected.path, std::fs::canonicalize(&path).unwrap());
        assert_eq!(options.session_path, Some(expected.path.clone()));
        assert_eq!(options.working_directory, Some(workspace.clone()));
        assert!(!options.no_session);
        assert!(options.provider.is_none());
        assert!(options.model.is_none());
        assert!(
            check_resume_identity(&expected, &saved.header.id, &saved.header.cwd, Some(&path))
                .is_ok()
        );
        assert!(
            check_resume_identity(&expected, "replacement", &saved.header.cwd, Some(&path))
                .is_err()
        );
        assert!(
            check_resume_identity(&expected, &saved.header.id, &saved.header.cwd, None).is_err()
        );
        assert!(
            check_resume_identity(
                &expected,
                &saved.header.id,
                temp.path().to_str().unwrap(),
                Some(&path)
            )
            .is_err()
        );
        let other = temp.path().join("other.jsonl");
        std::fs::copy(&path, &other).unwrap();
        assert!(
            check_resume_identity(&expected, &saved.header.id, &saved.header.cwd, Some(&other))
                .is_err()
        );
    }

    #[test]
    fn resume_preflight_never_creates_a_missing_session_or_accepts_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.jsonl");
        for path in [missing.clone(), temp.path().to_path_buf()] {
            let mut options = SessionOptions::default();
            assert!(run(prepare_storage(&Invocation::Resume { path }, &mut options)).is_err());
            assert!(options.no_session);
            assert!(options.session_path.is_none());
        }
        assert!(!missing.exists());
    }

    #[test]
    fn saving_requests_a_new_session_directory_without_pinning_an_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("plans");
        let mut options = SessionOptions::default();
        let invocation = Invocation::Plan {
            task: "task".to_string(),
            directory: Some(directory.clone()),
        };
        assert!(
            run(prepare_storage(&invocation, &mut options))
                .unwrap()
                .is_none()
        );
        assert_eq!(options.session_dir, Some(directory.clone()));
        assert!(!options.no_session);
        assert!(options.session_path.is_none());
        assert!(
            !directory.exists(),
            "SDK creation owns persistence, not preflight"
        );
    }
}
