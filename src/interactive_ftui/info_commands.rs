//! Read-only OMP info commands on the default stack (`/tools`,
//! `/extensions`, `/skills`, `/dirs`, `/context`, `/todo`, `/jobs`,
//! `/stats`). Each reads state the session already has and answers with one
//! system entry; none changes the session or starts a provider turn.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::mpsc::Sender;

use serde_json::Value;

use crate::autocomplete::{AutocompleteCatalog, NamedEntry};
use crate::interactive::PiMsg;
use crate::sdk::AgentSessionHandle;

/// Which info command the user ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoCommand {
    Tools,
    Extensions,
    Skills,
    Dirs,
    Context,
    Todo,
    Jobs,
    Stats,
}

impl InfoCommand {
    /// Parse the command token (with its slash). `None` for anything else.
    pub fn parse(token: &str) -> Option<Self> {
        Some(match token.to_ascii_lowercase().as_str() {
            "/tools" => Self::Tools,
            "/extensions" | "/status" => Self::Extensions,
            "/skills" => Self::Skills,
            "/dirs" => Self::Dirs,
            "/context" => Self::Context,
            "/todo" | "/todos" => Self::Todo,
            "/jobs" => Self::Jobs,
            "/stats" => Self::Stats,
            _ => return None,
        })
    }
}

fn value_name(value: &Value) -> Option<String> {
    value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn format_named(heading: &str, entries: &[NamedEntry], empty: &str) -> String {
    if entries.is_empty() {
        return empty.to_string();
    }
    let mut out = format!("{heading} ({}):", entries.len());
    for entry in entries {
        match &entry.description {
            Some(description) if !description.trim().is_empty() => {
                let _ = write!(out, "\n  {} — {}", entry.name, description.trim());
            }
            _ => {
                let _ = write!(out, "\n  {}", entry.name);
            }
        }
    }
    out
}

/// `/tools`: what the model can call right now.
pub fn format_tools(builtin: &[String], extension: &[String]) -> String {
    let mut out = format!(
        "Tools visible to the agent ({}):",
        builtin.len() + extension.len()
    );
    if !builtin.is_empty() {
        let _ = write!(out, "\n  built-in: {}", builtin.join(", "));
    }
    if !extension.is_empty() {
        let _ = write!(out, "\n  extensions: {}", extension.join(", "));
    }
    if builtin.is_empty() && extension.is_empty() {
        out.push_str("\n  (none)");
    }
    out
}

/// `/extensions`: what loaded extensions contribute.
pub fn format_extensions(commands: &[String], tools: &[String], hooks: usize) -> String {
    if commands.is_empty() && tools.is_empty() && hooks == 0 {
        return String::from("No extensions loaded.");
    }
    let mut out = String::from("Extensions:");
    let list = |names: &[String]| {
        if names.is_empty() {
            String::from("none")
        } else {
            names.join(", ")
        }
    };
    let _ = write!(out, "\n  commands ({}): {}", commands.len(), list(commands));
    let _ = write!(out, "\n  tools ({}): {}", tools.len(), list(tools));
    let _ = write!(out, "\n  event hooks: {hooks}");
    out
}

/// `/dirs`: the session's workspace roots, primary first.
pub fn format_dirs(roots: &[std::path::PathBuf]) -> String {
    let mut out = format!("Workspace directories ({}):", roots.len());
    for (index, root) in roots.iter().enumerate() {
        let marker = if index == 0 { " (primary)" } else { "" };
        let _ = write!(out, "\n  {}{marker}", root.display());
    }
    out
}

/// `/context`: how full the model's context window is, from the last
/// prompt's real token counts.
pub fn format_context(
    model: &str,
    window: Option<u32>,
    last_prompt: u64,
    messages: usize,
    tool_results: usize,
) -> String {
    let mut out = format!("Context for {model}:");
    match window.filter(|window| *window > 0) {
        Some(window) => {
            let percent = super::context_percent(last_prompt, Some(window));
            let _ = write!(
                out,
                "\n  last prompt: {last_prompt} / {window} tokens ({percent}%)"
            );
            let _ = write!(
                out,
                "\n  free: {} tokens",
                u64::from(window).saturating_sub(last_prompt)
            );
        }
        None => {
            let _ = write!(
                out,
                "\n  last prompt: {last_prompt} tokens (context window unknown)"
            );
        }
    }
    let _ = write!(
        out,
        "\n  messages on this branch: {messages} ({tool_results} tool results)"
    );
    out
}

/// `/jobs`: background jobs this session started.
pub fn format_jobs(jobs: &[crate::jobs::JobSnapshot]) -> String {
    if jobs.is_empty() {
        return String::from("No background jobs in this session.");
    }
    let mut out = format!("Background jobs ({}):", jobs.len());
    for job in jobs {
        let exit = job
            .exit_code
            .map_or_else(String::new, |code| format!(" (exit {code})"));
        let _ = write!(out, "\n  {} {}{exit} — {}", job.id, job.status, job.command);
    }
    out
}

fn describe_extensions(manager: &crate::extensions::ExtensionManager) -> String {
    let mut commands = manager
        .list_commands()
        .iter()
        .filter_map(value_name)
        .map(|name| format!("/{name}"))
        .collect::<Vec<_>>();
    commands.sort();
    let mut tools = manager
        .extension_tool_defs()
        .iter()
        .filter_map(value_name)
        .collect::<Vec<_>>();
    tools.sort();
    format_extensions(&commands, &tools, manager.list_event_hooks().len())
}

fn describe_tools(handle: &AgentSessionHandle) -> String {
    let mut builtin = handle
        .session()
        .agent
        .shared_tools()
        .snapshot()
        .tools()
        .iter()
        .map(|tool| tool.name().to_string())
        .collect::<Vec<_>>();
    builtin.sort();
    let mut extension = handle
        .extension_manager()
        .map(|manager| {
            manager
                .extension_tool_defs()
                .iter()
                .filter_map(value_name)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    extension.sort();
    // An extension tool installed into the registry shows once.
    extension.retain(|name| !builtin.contains(name));
    format_tools(&builtin, &extension)
}

/// Run one info command in the driver and send its answer to the UI.
pub async fn run(
    command: InfoCommand,
    handle: &AgentSessionHandle,
    catalog: &AutocompleteCatalog,
    cwd: &Path,
    agent_tx: &Sender<PiMsg>,
) {
    let message = match command {
        InfoCommand::Tools => describe_tools(handle),
        InfoCommand::Extensions => handle.extension_manager().map_or_else(
            || String::from("No extensions loaded."),
            describe_extensions,
        ),
        InfoCommand::Skills => format_named(
            "Skills",
            &catalog.skills,
            "No skills available (add SKILL.md files under .pi/skills or ~/.pi/agent/skills).",
        ),
        InfoCommand::Dirs => {
            let roots = handle
                .workspace()
                .map_or_else(|| vec![cwd.to_path_buf()], |workspace| workspace.roots());
            format_dirs(&roots)
        }
        InfoCommand::Context => {
            let entry = handle.session().current_model_entry();
            let (_, model_id) = handle.model();
            let model = entry
                .as_ref()
                .map_or(model_id, |entry| entry.model.status_label());
            let window = entry.as_ref().map(|entry| entry.model.context_window);
            match handle
                .with_session(|session| {
                    let messages = session.to_messages_for_current_path();
                    let last_prompt = messages
                        .iter()
                        .rev()
                        .find_map(|message| match message {
                            crate::model::Message::Assistant(assistant) => Some(
                                assistant.usage.input
                                    + assistant.usage.cache_read
                                    + assistant.usage.cache_write,
                            ),
                            _ => None,
                        })
                        .unwrap_or(0);
                    let tool_results = messages
                        .iter()
                        .filter(|message| matches!(message, crate::model::Message::ToolResult(_)))
                        .count();
                    (last_prompt, messages.len(), tool_results)
                })
                .await
            {
                Ok((last_prompt, messages, tool_results)) => {
                    format_context(&model, window, last_prompt, messages, tool_results)
                }
                Err(err) => format!("Context: unavailable ({err})"),
            }
        }
        InfoCommand::Todo => match handle
            .with_session(|session| {
                crate::todo::latest_from_entries(session.entries_for_current_path())
            })
            .await
        {
            Ok(list) if list.is_empty() => String::from("No todos yet."),
            Ok(list) => list.render(),
            Err(err) => format!("todo: {err}"),
        },
        InfoCommand::Jobs => match handle
            .with_session(|session| session.header.id.clone())
            .await
        {
            Ok(owner) => match crate::jobs::list(&owner) {
                Ok(jobs) => format_jobs(&jobs),
                Err(err) => format!("jobs: {err}"),
            },
            Err(err) => format!("jobs: {err}"),
        },
        InfoCommand::Stats => {
            let files =
                crate::stats::collect_session_files(&crate::config::Config::sessions_dir(), None);
            let report = crate::stats::aggregate(&files, &crate::stats::StatsFilter::default());
            crate::stats::render_text(&report)
        }
    };
    let _ = agent_tx.send(PiMsg::System(message));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_admits_exactly_the_info_commands() {
        assert_eq!(InfoCommand::parse("/tools"), Some(InfoCommand::Tools));
        assert_eq!(InfoCommand::parse("/TODO"), Some(InfoCommand::Todo));
        assert_eq!(InfoCommand::parse("/status"), Some(InfoCommand::Extensions));
        assert_eq!(InfoCommand::parse("/toolsx"), None);
        assert_eq!(InfoCommand::parse("/model"), None);
    }

    #[test]
    fn tools_listing_separates_extension_tools_and_reports_none() {
        let text = format_tools(
            &[String::from("bash"), String::from("read")],
            &[String::from("lsp")],
        );
        assert!(
            text.starts_with("Tools visible to the agent (3):"),
            "{text}"
        );
        assert!(text.contains("built-in: bash, read"), "{text}");
        assert!(text.contains("extensions: lsp"), "{text}");
        assert!(format_tools(&[], &[]).contains("(none)"));
    }

    #[test]
    fn extensions_listing_reports_an_empty_runtime_plainly() {
        assert_eq!(format_extensions(&[], &[], 0), "No extensions loaded.");
        let text = format_extensions(&[String::from("/deploy")], &[], 2);
        assert!(text.contains("commands (1): /deploy"), "{text}");
        assert!(text.contains("tools (0): none"), "{text}");
        assert!(text.contains("event hooks: 2"), "{text}");
    }

    #[test]
    fn context_reports_percent_free_and_unknown_windows() {
        let text = format_context("GPT-4o", Some(128_000), 32_000, 12, 3);
        assert!(text.contains("32000 / 128000 tokens (25%)"), "{text}");
        assert!(text.contains("free: 96000 tokens"), "{text}");
        assert!(text.contains("12 (3 tool results)"), "{text}");
        let unknown = format_context("custom", None, 10, 1, 0);
        assert!(unknown.contains("context window unknown"), "{unknown}");
    }

    #[test]
    fn dirs_marks_the_primary_root() {
        let text = format_dirs(&[
            std::path::PathBuf::from("/work/app"),
            std::path::PathBuf::from("/work/lib"),
        ]);
        assert!(text.contains("/work/app (primary)"), "{text}");
        assert!(!text.contains("/work/lib (primary)"), "{text}");
    }

    #[test]
    fn skills_listing_uses_descriptions_and_explains_an_empty_set() {
        let entries = [NamedEntry {
            name: String::from("rust-review"),
            description: Some(String::from("Review Rust changes")),
        }];
        let text = format_named("Skills", &entries, "none");
        assert!(text.contains("rust-review — Review Rust changes"), "{text}");
        assert_eq!(format_named("Skills", &[], "none"), "none");
    }
}
