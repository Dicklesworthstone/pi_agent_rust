//! Session workload benchmark helper:
//! - `prepare`: create a large session file with synthetic messages
//! - `workload`: open/resume, append messages, save, emit timing JSON
//! - `prepare-realistic` / `workload-realistic`: daily-usage simulation with
//!   compactions, extension activity, slash-like state changes, forks, and exports.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use pi::error::Result;
use pi::model::{AssistantMessage, ContentBlock, StopReason, TextContent, Usage, UserContent};
use pi::session::{Session, SessionMessage};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Prepare,
    Workload,
    PrepareRealistic,
    WorkloadRealistic,
}

#[derive(Debug, Clone)]
struct Args {
    mode: Mode,
    session_path: PathBuf,
    messages: usize,
    append: usize,
    target_tokens: usize,
    compactions: usize,
    extension_ops: usize,
    slash_ops: usize,
    forks: usize,
    exports: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    scenario: String,
    mode: String,
    session_path: String,
    existing_entries: usize,
    appended: usize,
    open_ms: f64,
    append_ms: f64,
    save_ms: f64,
    total_ms: f64,
    file_bytes: u64,
    target_tokens: usize,
    compactions: usize,
    extension_ops: usize,
    slash_ops: usize,
    forks: usize,
    exports: usize,
}

fn parse_args() -> Result<Args> {
    let mut mode = Mode::Workload;
    let mut session_path = PathBuf::from("/tmp/pi_session_bench/rust_large_session.jsonl");
    let mut messages = 5_000usize;
    let mut append = 10usize;
    let mut target_tokens = 0usize;
    let mut compactions = 0usize;
    let mut extension_ops = 0usize;
    let mut slash_ops = 0usize;
    let mut forks = 0usize;
    let mut exports = 0usize;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => {
                let value = next_arg_value(&mut args, "--mode")?;
                mode = match value.as_str() {
                    "prepare" => Mode::Prepare,
                    "workload" => Mode::Workload,
                    "prepare-realistic" => Mode::PrepareRealistic,
                    "workload-realistic" => Mode::WorkloadRealistic,
                    _ => {
                        return Err(pi::Error::session(
                            "invalid --mode; use prepare|workload|prepare-realistic|workload-realistic",
                        ));
                    }
                };
            }
            "--session" => {
                let value = next_arg_value(&mut args, "--session")?;
                session_path = PathBuf::from(value);
            }
            "--messages" => messages = parse_usize_flag(&mut args, "--messages")?,
            "--append" => append = parse_usize_flag(&mut args, "--append")?,
            "--target-tokens" => target_tokens = parse_usize_flag(&mut args, "--target-tokens")?,
            "--compactions" => compactions = parse_usize_flag(&mut args, "--compactions")?,
            "--extension-ops" => extension_ops = parse_usize_flag(&mut args, "--extension-ops")?,
            "--slash-ops" => slash_ops = parse_usize_flag(&mut args, "--slash-ops")?,
            "--forks" => forks = parse_usize_flag(&mut args, "--forks")?,
            "--exports" => exports = parse_usize_flag(&mut args, "--exports")?,
            _ => {}
        }
    }

    Ok(Args {
        mode,
        session_path,
        messages,
        append,
        target_tokens,
        compactions,
        extension_ops,
        slash_ops,
        forks,
        exports,
    })
}

fn next_arg_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| pi::Error::session(format!("{flag} requires a value")))
}

fn parse_usize_flag(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize> {
    let value = next_arg_value(args, flag)?;
    value
        .parse::<usize>()
        .map_err(|_| pi::Error::session(format!("invalid {flag}")))
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| pi::Error::session("session path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    Ok(())
}

const fn mode_to_str(mode: &Mode) -> &'static str {
    match mode {
        Mode::Prepare => "prepare",
        Mode::Workload => "workload",
        Mode::PrepareRealistic => "prepare-realistic",
        Mode::WorkloadRealistic => "workload-realistic",
    }
}

const fn scenario_for_mode(mode: &Mode) -> &'static str {
    match mode {
        Mode::Prepare | Mode::Workload => "synthetic",
        Mode::PrepareRealistic | Mode::WorkloadRealistic => "realistic",
    }
}

fn report_to_json(report: &Report) -> Result<String> {
    serde_json::to_string(report).map_err(|err| pi::Error::Json(Box::new(err)))
}

fn per_entry_token_budget(target_tokens: usize, entries: usize) -> usize {
    if target_tokens == 0 || entries == 0 {
        return 12;
    }
    target_tokens.div_ceil(entries).max(4)
}

fn token_payload(prefix: &str, idx: usize, approx_tokens: usize) -> String {
    let mut out = String::new();
    let _ = write!(out, "{prefix} {idx}");
    for tok in 0..approx_tokens.saturating_sub(2) {
        let _ = write!(out, " tok{}_{}", idx % 127, tok % 97);
    }
    out
}

fn build_assistant_message(text: String) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        api: "benchmark".to_string(),
        provider: "benchmark".to_string(),
        model: "benchmark".to_string(),
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        stop_details: None,
        error_message: None,
        timestamp: chrono::Utc::now().timestamp_millis(),
    }
}

fn append_user_messages(
    session: &mut Session,
    count: usize,
    prefix: &str,
    per_entry_tokens: usize,
) {
    for idx in 0..count {
        session.append_message(SessionMessage::User {
            content: UserContent::Text(token_payload(prefix, idx, per_entry_tokens)),
            timestamp: None,
        });
    }
}

fn append_seed_messages_mixed(session: &mut Session, count: usize, per_entry_tokens: usize) {
    for idx in 0..count {
        if idx % 2 == 0 {
            session.append_message(SessionMessage::User {
                content: UserContent::Text(token_payload("seed user", idx, per_entry_tokens)),
                timestamp: None,
            });
            continue;
        }
        let assistant =
            build_assistant_message(token_payload("seed assistant", idx, per_entry_tokens));
        session.append_message(SessionMessage::Assistant { message: assistant });
    }
}

fn user_message_entry_ids(session: &Session) -> Vec<String> {
    session
        .entries
        .iter()
        .filter_map(|entry| {
            if let pi::session::SessionEntry::Message(msg_entry) = entry
                && matches!(msg_entry.message, SessionMessage::User { .. })
            {
                return msg_entry.base.id.clone();
            }
            None
        })
        .collect()
}

fn append_tool_result(session: &mut Session, idx: usize, per_entry_tokens: usize) {
    session.append_message(SessionMessage::ToolResult {
        tool_call_id: format!("call-{idx}"),
        tool_name: "read".to_string(),
        content: vec![ContentBlock::Text(TextContent::new(token_payload(
            "tool result",
            idx,
            per_entry_tokens,
        )))],
        details: Some(json!({"ok": true, "idx": idx})),
        is_error: false,
        timestamp: Some(chrono::Utc::now().timestamp_millis()),
    });
}

fn run_realistic_ops(session: &mut Session, args: &Args) -> Result<()> {
    let per_entry_tokens =
        per_entry_token_budget(args.target_tokens, args.append.saturating_mul(2));

    for idx in 0..args.append {
        let user_text = token_payload("daily user", idx, per_entry_tokens);
        session.append_message(SessionMessage::User {
            content: UserContent::Text(user_text),
            timestamp: None,
        });

        let assistant =
            build_assistant_message(token_payload("daily assistant", idx, per_entry_tokens));
        session.append_message(SessionMessage::Assistant { message: assistant });

        if idx % 2 == 0 {
            append_tool_result(session, idx, per_entry_tokens / 2);
        }
    }

    for idx in 0..args.extension_ops {
        session.append_custom_entry(
            "benchmark.extension.op".to_string(),
            Some(json!({"idx": idx, "phase": "workload"})),
        );
    }

    let user_ids = user_message_entry_ids(session);

    for idx in 0..args.slash_ops {
        match idx % 4 {
            0 => {
                session.append_model_change(
                    "benchmark-provider".to_string(),
                    format!("benchmark-model-{}", idx % 3),
                );
            }
            1 => {
                let level = if idx % 2 == 0 { "high" } else { "medium" };
                session.append_thinking_level_change(level.to_string());
            }
            2 => {
                session.append_session_info(Some(format!("session-name-{idx}")));
            }
            _ => {
                if let Some(target_id) = user_ids.get(idx % user_ids.len().max(1)) {
                    let _ = session.add_label(target_id, Some(format!("label-{idx}")));
                }
            }
        }
    }

    if args.compactions > 0 && !user_ids.is_empty() {
        let stride = user_ids.len().div_ceil(args.compactions).max(1);
        for idx in 0..args.compactions {
            let target = user_ids[(idx.saturating_mul(stride)).min(user_ids.len() - 1)].clone();
            session.append_compaction(
                format!("realistic compaction {idx}"),
                target.clone(),
                args.target_tokens as u64,
                Some(json!({"idx": idx, "target": target})),
                Some(false),
            );
        }
    }

    if args.forks > 0 && !user_ids.is_empty() {
        for idx in 0..args.forks {
            let target = user_ids[idx % user_ids.len()].clone();
            if let Ok(plan) = session.plan_fork_from_user_message(&target) {
                session.append_branch_summary(
                    target,
                    format!("fork simulation {idx}"),
                    Some(json!({
                        "entriesCopied": plan.entries.len(),
                        "selectedChars": plan.selected_text.len()
                    })),
                    Some(false),
                );
            }
        }
    }

    if args.exports > 0 {
        let stem = args
            .session_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session");
        for idx in 0..args.exports {
            let html = session.to_html();
            let export_path = args
                .session_path
                .with_file_name(format!("{stem}.export.{idx}.html"));
            std::fs::write(export_path, html)?;
        }
    }

    Ok(())
}

fn seed_realistic_session(session: &mut Session, args: &Args) {
    let per_entry_tokens = per_entry_token_budget(args.target_tokens, args.messages);
    append_seed_messages_mixed(session, args.messages, per_entry_tokens);

    for idx in 0..args.extension_ops {
        session.append_custom_entry(
            "benchmark.extension.seed".to_string(),
            Some(json!({"idx": idx, "phase": "prepare"})),
        );
    }

    let user_ids = user_message_entry_ids(session);
    if args.compactions > 0 && !user_ids.is_empty() {
        let stride = user_ids.len().div_ceil(args.compactions).max(1);
        for idx in 0..args.compactions {
            let keep_id = user_ids[(idx.saturating_mul(stride)).min(user_ids.len() - 1)].clone();
            session.append_compaction(
                format!("seed compaction {idx}"),
                keep_id,
                args.target_tokens as u64,
                Some(json!({"phase": "prepare", "idx": idx})),
                Some(false),
            );
        }
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;
    ensure_parent_dir(&args.session_path)?;

    if matches!(args.mode, Mode::Prepare | Mode::PrepareRealistic) {
        let mut session = Session::create();
        session.path = Some(args.session_path.clone());

        let append_started = Instant::now();
        match args.mode {
            Mode::Prepare => {
                let per_entry_tokens = per_entry_token_budget(args.target_tokens, args.messages);
                append_seed_messages_mixed(&mut session, args.messages, per_entry_tokens);
            }
            Mode::PrepareRealistic => {
                seed_realistic_session(&mut session, &args);
            }
            Mode::Workload | Mode::WorkloadRealistic => panic!(),
        }
        let append_ms = append_started.elapsed().as_secs_f64() * 1000.0;

        let save_started = Instant::now();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .map_err(|err| pi::Error::session(format!("runtime init failed: {err}")))?;
        runtime.block_on(async { session.save().await })?;
        let save_ms = save_started.elapsed().as_secs_f64() * 1000.0;

        let file_bytes = std::fs::metadata(&args.session_path)?.len();
        let report = Report {
            scenario: scenario_for_mode(&args.mode).to_string(),
            mode: mode_to_str(&args.mode).to_string(),
            session_path: args.session_path.display().to_string(),
            existing_entries: session.entries.len(),
            appended: session.entries.len(),
            open_ms: 0.0,
            append_ms,
            save_ms,
            total_ms: append_ms + save_ms,
            file_bytes,
            target_tokens: args.target_tokens,
            compactions: args.compactions,
            extension_ops: args.extension_ops,
            slash_ops: args.slash_ops,
            forks: args.forks,
            exports: args.exports,
        };

        println!("{}", report_to_json(&report)?);
        return Ok(());
    }

    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .map_err(|err| pi::Error::session(format!("runtime init failed: {err}")))?;

    let open_started = Instant::now();
    let mut session = runtime
        .block_on(async { Session::open(args.session_path.to_string_lossy().as_ref()).await })?;
    let open_ms = open_started.elapsed().as_secs_f64() * 1000.0;
    let existing_entries = session.entries.len();

    let append_started = Instant::now();
    match args.mode {
        Mode::Workload => {
            let per_entry_tokens = per_entry_token_budget(args.target_tokens, args.append);
            append_user_messages(
                &mut session,
                args.append,
                "workload append",
                per_entry_tokens,
            );
        }
        Mode::WorkloadRealistic => {
            run_realistic_ops(&mut session, &args)?;
        }
        Mode::Prepare | Mode::PrepareRealistic => panic!(),
    }
    let append_ms = append_started.elapsed().as_secs_f64() * 1000.0;

    let save_started = Instant::now();
    runtime.block_on(async { session.save().await })?;
    let save_ms = save_started.elapsed().as_secs_f64() * 1000.0;

    let total_ms = open_ms + append_ms + save_ms;
    let file_bytes = std::fs::metadata(&args.session_path)?.len();
    let report = Report {
        scenario: scenario_for_mode(&args.mode).to_string(),
        mode: mode_to_str(&args.mode).to_string(),
        session_path: args.session_path.display().to_string(),
        existing_entries,
        appended: session.entries.len().saturating_sub(existing_entries),
        open_ms,
        append_ms,
        save_ms,
        total_ms,
        file_bytes,
        target_tokens: args.target_tokens,
        compactions: args.compactions,
        extension_ops: args.extension_ops,
        slash_ops: args.slash_ops,
        forks: args.forks,
        exports: args.exports,
    };

    println!("{}", report_to_json(&report)?);
    Ok(())
}

fn main() {
    if let Err(err) = run() {
        eprintln!("session_workload_bench error: {err}");
        std::process::exit(1);
    }
}
