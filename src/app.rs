//! Helpers for `src/main.rs`.
//!
//! This module exists to make core CLI logic testable without invoking the full
//! interactive agent loop.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{Datelike, Local};
use glob::Pattern;
use thiserror::Error;

use crate::auth::AuthStorage;
use crate::cli;
use crate::config::Config;
use crate::model::{self, AssistantMessage, ContentBlock, ImageContent, TextContent};
use crate::models::{
    ModelEntry, ModelRegistry, default_models_path, model_entry_is_ready,
    model_requires_configured_credential, normalize_api_key_opt,
};
use crate::provider::{StreamOptions, ThinkingBudgets};
use crate::provider_metadata::{
    canonical_provider_id, provider_ids_match, split_provider_model_spec,
};
use crate::session::Session;
use crate::tools::process_file_arguments;

#[derive(Debug, Clone)]
pub struct InitialMessage {
    pub text: String,
    pub images: Vec<ImageContent>,
}

#[derive(Debug, Clone)]
pub struct ScopedModel {
    pub model: ModelEntry,
    pub thinking_level: Option<model::ThinkingLevel>,
}

#[derive(Debug, Clone)]
struct ParsedModelResult {
    model: Option<ModelEntry>,
    thinking_level: Option<model::ThinkingLevel>,
    warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelSelection {
    pub model_entry: ModelEntry,
    pub thinking_level: model::ThinkingLevel,
    pub scoped_models: Vec<ScopedModel>,
    pub fallback_message: Option<String>,
}

#[derive(Debug, Error)]
pub enum StartupError {
    #[error("No models available. Set API keys in environment variables or create {models_path}")]
    NoModelsAvailable { models_path: PathBuf },
    #[error("No API key found for provider {provider}. Set env var or use --api-key.")]
    MissingApiKey { provider: String },
}

#[derive(Debug, Clone)]
struct ContextFile {
    path: String,
    content: String,
}

struct RestoreResult {
    model: Option<ModelEntry>,
    fallback_message: Option<String>,
    deferred_warning: Option<String>,
}

pub fn apply_piped_stdin(cli: &mut cli::Cli, stdin_content: Option<String>) {
    if let Some(stdin_content) = stdin_content {
        // Match pi-mono's `.trim()` — strip all leading/trailing whitespace.
        let stdin_content = stdin_content.trim();
        if stdin_content.is_empty() {
            return;
        }
        cli.print = true;
        cli.args.insert(0, stdin_content.to_string());
    }
}

#[allow(clippy::missing_const_for_fn)]
pub fn normalize_cli(cli: &mut cli::Cli) {
    if cli.rpc && cli.mode.is_none() {
        cli.mode = Some("rpc".to_string());
    }

    if cli.print {
        cli.no_session = true;
    }

    if let Some(provider) = &mut cli.provider {
        *provider = provider.to_ascii_lowercase();
    }
}

pub fn validate_rpc_args(cli: &cli::Cli) -> Result<()> {
    let rpc_mode = cli.rpc || cli.mode.as_deref() == Some("rpc");
    if cli.rpc && cli.print {
        bail!("Error: RPC mode cannot be combined with --print");
    }
    if rpc_mode && !cli.file_args().is_empty() {
        bail!("Error: @file arguments are not supported in RPC mode");
    }
    Ok(())
}

pub fn prepare_initial_message(
    cwd: &Path,
    file_args: &[String],
    messages: &mut Vec<String>,
    auto_resize_images: bool,
) -> Result<Option<InitialMessage>> {
    if file_args.is_empty() {
        return Ok(None);
    }

    let processed = process_file_arguments(file_args, cwd, auto_resize_images)?;
    let mut initial_message = processed.text;
    let has_message = !messages.is_empty();
    if has_message {
        initial_message.push_str(&messages.remove(0));
    }

    if initial_message.is_empty() && processed.images.is_empty() && !has_message {
        return Ok(None);
    }

    Ok(Some(InitialMessage {
        text: initial_message,
        images: processed.images,
    }))
}

pub fn build_initial_content(initial: &InitialMessage) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    content.push(ContentBlock::Text(TextContent::new(initial.text.clone())));
    for image in &initial.images {
        content.push(ContentBlock::Image(image.clone()));
    }
    content
}

#[allow(clippy::too_many_arguments)]
pub fn build_system_prompt(
    cli: &cli::Cli,
    cwd: &Path,
    enabled_tools: &[&str],
    skills_prompt: Option<&str>,
    global_dir: &Path,
    package_dir: &Path,
    test_mode: bool,
    include_cwd: bool,
) -> Result<String> {
    use std::fmt::Write as _;

    let custom_prompt = resolve_prompt_input(cli.system_prompt.as_deref(), "system prompt")?;
    let append_prompt =
        resolve_prompt_input(cli.append_system_prompt.as_deref(), "append system prompt")?;
    let context_files = if test_mode {
        Vec::new()
    } else {
        load_project_context_files(cwd, global_dir)
    };

    let mut prompt =
        custom_prompt.unwrap_or_else(|| default_system_prompt(enabled_tools, package_dir));

    if let Some(append_prompt) = append_prompt {
        prompt.push_str("\n\n");
        prompt.push_str(&append_prompt);
    }

    if !context_files.is_empty() {
        prompt.push_str("\n\n# Project Context\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for file in &context_files {
            let _ = write!(prompt, "## {}\n\n{}\n\n", file.path, file.content);
        }
    }

    if let Some(skills_prompt) = skills_prompt {
        prompt.push_str(skills_prompt);
    }

    let date_time = if test_mode {
        "<TIMESTAMP>".to_string()
    } else {
        format_current_datetime()
    };
    let _ = write!(prompt, "\nCurrent date and time: {date_time}");
    if include_cwd {
        let cwd_display = if test_mode {
            "<CWD>".to_string()
        } else {
            cwd.display().to_string()
        };
        let _ = write!(prompt, "\nCurrent working directory: {cwd_display}");
    }

    Ok(prompt)
}

fn resolve_prompt_input(input: Option<&str>, description: &str) -> Result<Option<String>> {
    let Some(value) = input else {
        return Ok(None);
    };

    let path = Path::new(value);
    if path.exists() {
        let content = std::fs::read_to_string(path)
            .map_err(|err| anyhow::anyhow!("Could not read {description} file {value}: {err}"))?;
        Ok(Some(content))
    } else {
        Ok(Some(value.to_string()))
    }
}

fn default_system_prompt(enabled_tools: &[&str], package_dir: &Path) -> String {
    let tool_descriptions = [
        ("read", "Read file contents"),
        ("bash", "Execute bash commands (ls, grep, find, etc.)"),
        (
            "edit",
            "Make surgical edits to files (find exact text and replace)",
        ),
        ("write", "Create or overwrite files"),
        (
            "grep",
            "Search file contents for patterns (respects .gitignore, supports hashline=true for use with hashline_edit)",
        ),
        ("find", "Find files by glob pattern (respects .gitignore)"),
        ("ls", "List directory contents"),
        (
            "hashline_edit",
            "Apply precise file edits using LINE#HASH tags from read or grep with hashline=true",
        ),
        (
            "subagent",
            "Delegate isolated work to a named Rust Pi child agent; supports single, bounded parallel, and chained workflows",
        ),
    ];

    let mut tools = Vec::new();
    for tool in enabled_tools {
        if let Some((_, description)) = tool_descriptions.iter().find(|(name, _)| name == tool) {
            tools.push(format!("- {tool}: {description}"));
        }
    }

    let tools_list = if tools.is_empty() {
        "(none)".to_string()
    } else {
        tools.join("\n")
    };

    let has_tool = |name: &str| enabled_tools.contains(&name);
    let has_bash = has_tool("bash");
    let has_edit = has_tool("edit");
    let has_write = has_tool("write");
    let has_grep = has_tool("grep");
    let has_find = has_tool("find");
    let has_ls = has_tool("ls");
    let has_read = has_tool("read");
    let has_hashline_edit = has_tool("hashline_edit");

    let mut guidelines_list = Vec::new();
    if has_bash && !has_grep && !has_find && !has_ls {
        guidelines_list.push("Use bash for file operations like ls, rg, find");
    } else if has_bash && (has_grep || has_find || has_ls) {
        guidelines_list.push(
            "Prefer grep/find/ls tools over bash for file exploration (faster, respects .gitignore)",
        );
    }

    if has_read && has_edit {
        guidelines_list.push(
            "Use read to examine files before editing. You must use this tool instead of cat or sed.",
        );
    }
    if has_edit {
        guidelines_list.push("Use edit for precise changes (old text must match exactly)");
    }
    if has_hashline_edit && has_read {
        guidelines_list.push(
            "For large files or complex multi-site edits, use read or grep with hashline=true to get LINE#HASH tags, then use hashline_edit for precise line-addressed edits",
        );
    }
    if has_write {
        guidelines_list.push("Use write only for new files or complete rewrites");
    }
    if has_edit || has_write {
        guidelines_list.push(
            "When summarizing your actions, output plain text directly - do NOT use cat or bash to display what you did",
        );
    }

    guidelines_list.push("Be concise in your responses");
    guidelines_list.push("Show file paths clearly when working with files");

    let guidelines = guidelines_list
        .iter()
        .map(|g| format!("- {g}"))
        .collect::<Vec<_>>()
        .join("\n");

    let readme_path = package_dir.join("README.md").display().to_string();
    let docs_path = package_dir.join("docs").display().to_string();
    let examples_path = package_dir.join("examples").display().to_string();

    format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tools_list}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n\nGuidelines:\n{guidelines}\n\nPi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):\n- Main documentation: {readme_path}\n- Additional docs: {docs_path}\n- Examples: {examples_path} (extensions, custom tools, SDK)\n- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md)\n- When working on pi topics, read the docs and examples, and follow .md cross-references before implementing\n- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)"
    )
}

fn load_project_context_files(cwd: &Path, global_dir: &Path) -> Vec<ContextFile> {
    let mut context_files = Vec::new();
    let mut seen = HashSet::new();

    if let Some(global) = load_context_file_from_dir(global_dir) {
        seen.insert(global.path.clone());
        context_files.push(global);
    }

    let mut ancestor_files = Vec::new();
    let mut current = cwd.to_path_buf();

    loop {
        if let Some(context) = load_context_file_from_dir(&current)
            && seen.insert(context.path.clone())
        {
            ancestor_files.push(context);
        }

        if !current.pop() {
            break;
        }
    }

    ancestor_files.reverse();
    context_files.extend(ancestor_files);
    context_files
}

fn load_context_file_from_dir(dir: &Path) -> Option<ContextFile> {
    let candidates = ["AGENTS.md", "CLAUDE.md"];
    for filename in candidates {
        let path = dir.join(filename);
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    return Some(ContextFile {
                        path: path.display().to_string(),
                        content,
                    });
                }
                Err(err) => {
                    eprintln!("Warning: Could not read {}: {err}", path.display());
                }
            }
        }
    }
    None
}

fn format_current_datetime() -> String {
    // Date only — deliberately no clock time. This string is part of the cached
    // system-prompt prefix; a per-second timestamp would invalidate the
    // provider's prompt/KV cache on every request (higher latency + cost). Date
    // granularity keeps the prefix stable within a day while still giving the
    // model the current date. (#103)
    let now = Local::now();
    format!(
        "{}, {} {}, {}",
        now.format("%A"),
        now.format("%B"),
        now.day(),
        now.year()
    )
}

#[allow(clippy::too_many_lines)]
pub fn select_model_and_thinking(
    cli: &cli::Cli,
    config: &Config,
    session: &Session,
    registry: &ModelRegistry,
    scoped_models: &[ScopedModel],
    global_dir: &Path,
) -> Result<ModelSelection> {
    let is_continuing = cli.r#continue || cli.resume || cli.session.is_some();
    let mut selected_model: Option<ModelEntry> = None;
    let mut scoped_thinking: Option<model::ThinkingLevel> = None;
    let mut fallback_message = None;
    let mut deferred_restore_warning = None;

    if let (Some(provider), Some(model_id)) = (cli.provider.as_deref(), cli.model.as_deref()) {
        let found = registry
            .find(provider, model_id)
            .or_else(|| crate::models::ad_hoc_model_entry(provider, model_id));
        if found.is_none() {
            bail!("Model {provider}/{model_id} not found");
        }
        selected_model = found;
    } else if let Some(provider) = cli.provider.as_deref() {
        let candidates: Vec<ModelEntry> = registry
            .models()
            .iter()
            .filter(|m| provider_ids_match(&m.model.provider, provider))
            .cloned()
            .collect();
        if candidates.is_empty() {
            // Providers configured purely from routing metadata (e.g. the
            // coding-plan presets) have no entries in the registry. Synthesize
            // an ad-hoc model entry so the provider is still usable; credentials
            // are resolved later by `resolve_api_key`.
            //
            // Honor `config.default_model` only when it is paired with this
            // provider (via `config.default_provider`); otherwise it belongs to
            // a different provider and we fall back to the provider's built-in
            // default model.
            let configured_default = config
                .default_provider
                .as_deref()
                .filter(|default_provider| provider_ids_match(provider, default_provider))
                .and(config.default_model.as_deref());
            let default_model = configured_default.or_else(|| provider_default_model_id(provider));
            selected_model = default_model
                .and_then(|model_id| crate::models::ad_hoc_model_entry(provider, model_id));
            if selected_model.is_none() {
                bail!("No models available for provider {provider}");
            }
        } else {
            let ready_candidates: Vec<ModelEntry> = candidates
                .iter()
                .filter(|entry| model_entry_is_ready(entry))
                .cloned()
                .collect();
            let preferred_pool = if ready_candidates.is_empty() {
                candidates.as_slice()
            } else {
                ready_candidates.as_slice()
            };
            selected_model = config
                .default_model
                .as_deref()
                .and_then(|default_model| registry.find(provider, default_model))
                .filter(|found| {
                    preferred_pool.iter().any(|candidate| {
                        candidate.model.id.eq_ignore_ascii_case(&found.model.id)
                            && provider_ids_match(&candidate.model.provider, &found.model.provider)
                    })
                })
                .or_else(|| Some(default_model_from_candidates(preferred_pool)));
        }
    } else if let Some(model_id) = cli.model.as_deref() {
        if let Some((provider, scoped_model_id)) = split_provider_model_spec(model_id) {
            selected_model = registry
                .find(provider, scoped_model_id)
                .or_else(|| crate::models::ad_hoc_model_entry(provider, scoped_model_id));
        }

        if selected_model.is_none() {
            let matches: Vec<ModelEntry> = registry
                .models()
                .iter()
                .filter(|m| m.model.id.eq_ignore_ascii_case(model_id))
                .cloned()
                .collect();
            if matches.is_empty() {
                bail!("Model {model_id} not found");
            }
            if let Some(default_provider) = config.default_provider.as_deref()
                && let Some(found) = matches
                    .iter()
                    .find(|m| provider_ids_match(&m.model.provider, default_provider))
            {
                selected_model = Some(found.clone());
            }
            if selected_model.is_none() {
                selected_model = select_preferred_exact_id_match(&matches);
            }
        }
    } else if !scoped_models.is_empty() && !is_continuing {
        if let (Some(default_provider), Some(default_model)) = (
            config.default_provider.as_deref(),
            config.default_model.as_deref(),
        ) && let Some(found) = scoped_models.iter().find(|sm| {
            provider_ids_match(&sm.model.model.provider, default_provider)
                && sm.model.model.id.eq_ignore_ascii_case(default_model)
        }) {
            selected_model = Some(found.model.clone());
            if cli.thinking.is_none() {
                scoped_thinking = found.thinking_level;
            }
        }
        if selected_model.is_none() {
            let first = &scoped_models[0];
            selected_model = Some(first.model.clone());
            if cli.thinking.is_none() {
                scoped_thinking = first.thinking_level;
            }
        }
    }

    if selected_model.is_none()
        && let Some((provider, model_id)) = model_from_session_state(session)
    {
        let restore = restore_model_from_session(&provider, &model_id, None, registry);
        selected_model = restore.model;
        fallback_message = restore.fallback_message;
        deferred_restore_warning = restore.deferred_warning;
    }

    if selected_model.is_none()
        && let (Some(default_provider), Some(default_model)) = (
            config.default_provider.as_deref(),
            config.default_model.as_deref(),
        )
        && let Some(found) = registry.find(default_provider, default_model)
    {
        selected_model = Some(found);
    }

    if selected_model.is_none() {
        let available = registry.get_available();
        if !available.is_empty() {
            let fallback = default_model_from_available(&available);
            if fallback_message.is_none()
                && let Some(warning) = deferred_restore_warning.take()
            {
                fallback_message = Some(format!(
                    "{warning} Using {}/{}.",
                    fallback.model.provider, fallback.model.id
                ));
            }
            selected_model = Some(fallback);
        }
    }

    // If we restored or defaulted into a model that requires credentials but has
    // none configured, prefer falling back to any ready model instead of forcing
    // an immediate setup prompt. (Explicit CLI selection should still error.)
    let explicit_model_selection = cli.provider.is_some() || cli.model.is_some();
    let missing_creds = if explicit_model_selection {
        None
    } else {
        selected_model.as_ref().and_then(|entry| {
            if model_entry_is_ready(entry) {
                None
            } else {
                Some((entry.model.provider.clone(), entry.model.id.clone()))
            }
        })
    };
    if let Some((missing_provider, missing_model_id)) = missing_creds {
        let available = registry.get_available();
        if !available.is_empty() {
            let fallback = default_model_from_available(&available);
            if fallback_message.is_none() {
                fallback_message = Some(format!(
                    "Missing credentials for {missing_provider}/{missing_model_id}. Using {}/{} based on detected keys.",
                    fallback.model.provider, fallback.model.id
                ));
            }
            selected_model = Some(fallback);
        } else if !registry.models().is_empty() {
            // No detected keys anywhere, but we still want to pick a stable default
            // so startup can guide the user through the correct login flow.
            let fallback = default_model_from_catalog(registry.models());
            if fallback_message.is_none() {
                fallback_message = Some(format!(
                    "Missing credentials for {missing_provider}/{missing_model_id}. Defaulting to {}/{} for setup.",
                    fallback.model.provider, fallback.model.id
                ));
            }
            selected_model = Some(fallback);
        }
    }

    // If nothing was selected yet, default to our preferred catalog entry even
    // when no credentials are configured. This keeps first-run UX consistent
    // and avoids the misleading "No models configured" path when built-ins exist.
    if selected_model.is_none() && !registry.models().is_empty() {
        let fallback = default_model_from_catalog(registry.models());
        if fallback_message.is_none()
            && let Some(warning) = deferred_restore_warning.take()
        {
            fallback_message = Some(format!(
                "{warning} Defaulting to {}/{} for setup.",
                fallback.model.provider, fallback.model.id
            ));
        }
        selected_model = Some(fallback);
    }

    let Some(model_entry) = selected_model else {
        let models_path = default_models_path(global_dir);
        return Err(StartupError::NoModelsAvailable { models_path }.into());
    };

    if let Some(warning) = deferred_restore_warning.take() {
        fallback_message = Some(match fallback_message.take() {
            Some(message) => format!("{warning} {message}"),
            None => format!(
                "{warning} Using {}/{}.",
                model_entry.model.provider, model_entry.model.id
            ),
        });
    }

    let mut thinking_level: Option<model::ThinkingLevel> = None;

    if let Some(cli_thinking) = cli.thinking.as_deref() {
        thinking_level = Some(parse_thinking_level(cli_thinking)?);
    } else if scoped_thinking.is_some() {
        thinking_level = scoped_thinking;
    } else if is_continuing && let Some(saved) = thinking_level_from_session_state(session) {
        thinking_level = Some(saved);
    }

    if thinking_level.is_none() {
        thinking_level = config
            .default_thinking_level
            .as_deref()
            .and_then(parse_thinking_level_opt);
    }

    let thinking_level =
        model_entry.clamp_thinking_level(thinking_level.unwrap_or(model::ThinkingLevel::XHigh));

    Ok(ModelSelection {
        model_entry,
        thinking_level,
        scoped_models: scoped_models.to_vec(),
        fallback_message,
    })
}

fn parse_thinking_level(value: &str) -> Result<model::ThinkingLevel> {
    value
        .parse()
        .map_err(|err| anyhow::anyhow!("Invalid thinking level \"{value}\": {err}"))
}

fn parse_thinking_level_opt(value: &str) -> Option<model::ThinkingLevel> {
    value.parse().ok()
}

fn last_model_from_session(session: &Session) -> Option<(String, String)> {
    session.effective_model_for_current_path()
}

fn last_thinking_level(session: &Session) -> Option<model::ThinkingLevel> {
    session
        .effective_thinking_level_for_current_path()
        .as_deref()
        .and_then(parse_thinking_level_opt)
}

fn model_from_session_state(session: &Session) -> Option<(String, String)> {
    last_model_from_session(session)
}

fn thinking_level_from_session_state(session: &Session) -> Option<model::ThinkingLevel> {
    last_thinking_level(session)
}

pub fn update_session_for_selection(session: &mut Session, selection: &ModelSelection) {
    let previous_model = model_from_session_state(session);
    let previous_thinking = thinking_level_from_session_state(session);
    let (stored_provider, stored_model_id, model_changed) = match previous_model {
        Some((provider, model_id))
            if provider_ids_match(&provider, &selection.model_entry.model.provider)
                && model_id.eq_ignore_ascii_case(&selection.model_entry.model.id) =>
        {
            (provider, model_id, false)
        }
        _ => (
            selection.model_entry.model.provider.clone(),
            selection.model_entry.model.id.clone(),
            true,
        ),
    };

    session.set_model_header(
        Some(stored_provider.clone()),
        Some(stored_model_id.clone()),
        Some(selection.thinking_level.to_string()),
    );

    if model_changed {
        session.append_model_change(stored_provider, stored_model_id);
    }

    let thinking_changed = previous_thinking != Some(selection.thinking_level);

    if thinking_changed {
        session.append_thinking_level_change(selection.thinking_level.to_string());
    }
}

fn restore_model_from_session(
    saved_provider: &str,
    saved_model_id: &str,
    current_model: Option<ModelEntry>,
    registry: &ModelRegistry,
) -> RestoreResult {
    let restored = registry
        .find(saved_provider, saved_model_id)
        .or_else(|| crate::models::ad_hoc_model_entry(saved_provider, saved_model_id));

    if restored.is_some() {
        return RestoreResult {
            model: restored,
            fallback_message: None,
            deferred_warning: None,
        };
    }

    let reason = "model no longer exists";

    if let Some(current) = current_model {
        return RestoreResult {
            model: Some(current.clone()),
            fallback_message: Some(format!(
                "Could not restore model {saved_provider}/{saved_model_id} ({reason}). Using {}/{}.",
                current.model.provider, current.model.id
            )),
            deferred_warning: None,
        };
    }

    let available = registry.get_available();
    if !available.is_empty() {
        let fallback = default_model_from_available(&available);
        return RestoreResult {
            model: Some(fallback.clone()),
            fallback_message: Some(format!(
                "Could not restore model {saved_provider}/{saved_model_id} ({reason}). Using {}/{}.",
                fallback.model.provider, fallback.model.id
            )),
            deferred_warning: None,
        };
    }

    RestoreResult {
        model: None,
        fallback_message: None,
        deferred_warning: Some(format!(
            "Could not restore model {saved_provider}/{saved_model_id} ({reason})."
        )),
    }
}

fn default_model_from_available(available: &[ModelEntry]) -> ModelEntry {
    default_model_from_candidates(available)
}

fn default_model_from_catalog(models: &[ModelEntry]) -> ModelEntry {
    default_model_from_candidates(models)
}

pub fn bootstrap_model_entry(registry: &ModelRegistry) -> Option<ModelEntry> {
    let available = registry.get_available();
    if !available.is_empty() {
        return Some(default_model_from_available(&available));
    }

    (!registry.models().is_empty()).then(|| default_model_from_catalog(registry.models()))
}

fn select_preferred_exact_id_match(candidates: &[ModelEntry]) -> Option<ModelEntry> {
    if candidates.is_empty() {
        return None;
    }

    let ready_candidates: Vec<ModelEntry> = candidates
        .iter()
        .filter(|entry| model_entry_is_ready(entry))
        .cloned()
        .collect();
    let preferred_pool = if ready_candidates.is_empty() {
        candidates
    } else {
        ready_candidates.as_slice()
    };

    Some(default_model_from_candidates(preferred_pool))
}

/// Preferred default model per provider, in descending priority order.
///
/// This single table drives two behaviors:
///   1. Picking the best entry out of a registry that already lists models for
///      a provider (`default_model_from_candidates`).
///   2. Synthesizing an ad-hoc default for providers that have no registry
///      models (e.g. coding-plan providers configured purely from routing
///      metadata) via [`provider_default_model_id`].
///
/// Multiple rows for the same provider are allowed and tried in order, which
/// lets a provider expose both a virtual model id (matched by the synthesized
/// ad-hoc entry) and concrete model ids (matched against registry listings).
const PROVIDER_DEFAULT_MODELS: &[(&str, &str)] = &[
    // Prefer Codex (ChatGPT OAuth) when available.
    ("openai-codex", "gpt-5.5"),
    ("openai-codex", "gpt-5.4"),
    ("openai-codex", "gpt-5.3-codex"),
    ("openai-codex", "gpt-5.2-codex"),
    ("openai-codex", "gpt-5.1-codex-max"),
    // Fall back to OpenAI API when configured.
    ("openai", "gpt-5.5"),
    ("openai", "gpt-5.4"),
    ("openai", "gpt-5.3-codex"),
    ("openai", "gpt-5.2-codex"),
    ("openai", "gpt-5.1-codex"),
    ("amazon-bedrock", "us.anthropic.claude-opus-4-20250514-v1:0"),
    ("anthropic", "claude-opus-4-5"),
    ("azure-openai-responses", "gpt-5.2"),
    ("google", "gemini-2.5-pro"),
    ("google-gemini-cli", "gemini-2.5-pro"),
    ("google-antigravity", "gemini-3-pro-high"),
    ("google-vertex", "gemini-3-pro-preview"),
    ("github-copilot", "gpt-4o"),
    ("openrouter", "openai/gpt-5.1-codex"),
    ("vercel-ai-gateway", "anthropic/claude-opus-4.5"),
    ("xai", "grok-4-fast-non-reasoning"),
    ("groq", "openai/gpt-oss-120b"),
    ("cerebras", "zai-glm-4.6"),
    // glm-5.2 (1M context) is the GLM Coding Plan flagship and is registered
    // under provider `zai` on the coding endpoint, so it is the coding-plan
    // default. Bare `zai` stays on glm-4.7 — the newest model on the general
    // (non-coding) z.ai endpoint (#115).
    ("zai", "glm-4.7"),
    ("zai-coding-plan", "glm-5.2"),
    ("zhipuai-coding-plan", "glm-5.2"),
    ("mistral", "devstral-medium-latest"),
    // MiniMax-M3 is the current MiniMax flagship; the prior `MiniMax-M2.7`
    // default was never a registered catalog entry (latent bug) (#115).
    ("minimax", "MiniMax-M3"),
    ("minimax-cn", "MiniMax-M3"),
    ("minimax-coding-plan", "MiniMax-M3"),
    ("minimax-cn-coding-plan", "MiniMax-M3"),
    ("huggingface", "moonshotai/Kimi-K2.5"),
    ("opencode", "claude-opus-4-6"),
    // The Kimi for Coding plan exposes a single stable virtual model id
    // (`kimi-for-coding`) that the backend remaps to the latest model. Prefer
    // it for the synthesized ad-hoc entry, then fall back to concrete ids when
    // a registry actually lists Kimi models. Fallbacks are ordered newest-first
    // so a registry that lacks the virtual id still picks the most recent
    // shipped Kimi release (#93 — K2.6 ships 2026-04, K2.5 ships 2026-01,
    // K2-thinking is the legacy default).
    ("kimi-for-coding", "kimi-for-coding"),
    ("kimi-for-coding", "kimi-k2.6"),
    ("kimi-for-coding", "kimi-k2.5"),
    ("kimi-for-coding", "kimi-k2-thinking"),
];

fn provider_default_matches(
    default_provider: &str,
    model_id: &str,
) -> impl Fn(&ModelEntry) -> bool {
    let canonical = |provider: &str| {
        canonical_provider_id(provider)
            .unwrap_or(provider)
            .to_ascii_lowercase()
    };
    let target_provider = canonical(default_provider);
    let target_model = model_id.to_string();
    move |m: &ModelEntry| {
        canonical(&m.model.provider) == target_provider
            && m.model.id.eq_ignore_ascii_case(&target_model)
    }
}

/// Resolve the preferred default model id for a provider that has no registry
/// candidates, so an ad-hoc model entry can be synthesized for it.
fn provider_default_model_id(provider: &str) -> Option<&'static str> {
    let canonical = |p: &str| canonical_provider_id(p).unwrap_or(p).to_ascii_lowercase();
    let target = canonical(provider);
    PROVIDER_DEFAULT_MODELS
        .iter()
        .find(|(p, _)| canonical(p) == target)
        .map(|(_, model_id)| *model_id)
}

fn default_model_from_candidates(candidates: &[ModelEntry]) -> ModelEntry {
    for (provider, model_id) in PROVIDER_DEFAULT_MODELS {
        let matches = provider_default_matches(provider, model_id);
        if let Some(found) = candidates.iter().find(|&m| matches(m)) {
            return found.clone();
        }
    }

    candidates[0].clone()
}

pub fn resolve_api_key(
    auth: &AuthStorage,
    cli: &cli::Cli,
    entry: &ModelEntry,
) -> Result<Option<String>> {
    let key = normalize_api_key_opt(cli.api_key.clone())
        .or_else(|| normalize_api_key_opt(auth.resolve_api_key(&entry.model.provider, None)))
        .or_else(|| normalize_api_key_opt(entry.api_key.clone()));

    if model_requires_configured_credential(entry) && key.is_none() {
        return Err(StartupError::MissingApiKey {
            provider: entry.model.provider.clone(),
        }
        .into());
    }

    Ok(key)
}

pub fn build_stream_options(
    config: &Config,
    api_key: Option<String>,
    selection: &ModelSelection,
    session: &Session,
) -> StreamOptions {
    let mut options = StreamOptions {
        api_key,
        headers: selection.model_entry.headers.clone(),
        session_id: Some(session.header.id.clone()),
        // Seed the per-request output cap from the model registry's `maxTokens`
        // so the value users configure in `models.json` actually takes effect.
        // Without this every provider falls back to its hardcoded per-request
        // default (e.g. 4096), truncating turns that emit large tool-call
        // arguments (most visibly the `write` tool). Embedders can still
        // override via `set_max_tokens`.
        max_tokens: Some(selection.model_entry.model.max_tokens),
        ..Default::default()
    };

    options.thinking_level = Some(selection.thinking_level);

    if let Some(budgets) = &config.thinking_budgets {
        let defaults = ThinkingBudgets::default();
        options.thinking_budgets = Some(ThinkingBudgets {
            minimal: budgets.minimal.unwrap_or(defaults.minimal),
            low: budgets.low.unwrap_or(defaults.low),
            medium: budgets.medium.unwrap_or(defaults.medium),
            high: budgets.high.unwrap_or(defaults.high),
            xhigh: budgets.xhigh.unwrap_or(defaults.xhigh),
            max: budgets.max.unwrap_or(defaults.max),
        });
    }

    options
}

// === Model scoping helpers (used by main + tests) ===

pub fn parse_models_arg(models: &str) -> Vec<String> {
    models
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn resolve_model_scope(
    patterns: &[String],
    registry: &ModelRegistry,
    allow_missing_keys: bool,
) -> Vec<ScopedModel> {
    let available_models = if allow_missing_keys {
        registry.models().to_vec()
    } else {
        registry.get_available()
    };

    let mut scoped_models: Vec<ScopedModel> = Vec::new();

    for pattern in patterns {
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            let mut glob_pattern = pattern.as_str();
            let mut thinking_level = None;
            if let Some((prefix, suffix)) = pattern.rsplit_once(':')
                && let Some(parsed) = parse_thinking_level_opt(suffix)
            {
                thinking_level = Some(parsed);
                glob_pattern = prefix;
            }

            let glob = match Pattern::new(&glob_pattern.to_lowercase()) {
                Ok(glob) => glob,
                Err(err) => {
                    eprintln!("Warning: Invalid model pattern \"{pattern}\": {err}");
                    continue;
                }
            };

            let mut matched_any = false;
            for model in &available_models {
                let full_id = format!("{}/{}", model.model.provider, model.model.id);
                let candidate_full = full_id.to_lowercase();
                let candidate_id = model.model.id.to_lowercase();
                if glob.matches(&candidate_full) || glob.matches(&candidate_id) {
                    matched_any = true;
                    if !scoped_models
                        .iter()
                        .any(|sm| models_equal(&sm.model, model))
                    {
                        scoped_models.push(ScopedModel {
                            model: model.clone(),
                            thinking_level,
                        });
                    }
                }
            }

            if !matched_any {
                eprintln!("Warning: No models match pattern \"{pattern}\"");
            }
            continue;
        }

        let parsed = parse_model_pattern(pattern, &available_models);
        if let Some(warning) = parsed.warning {
            eprintln!("Warning: {warning}");
        }

        if let Some(model) = parsed.model {
            if !scoped_models
                .iter()
                .any(|sm| models_equal(&sm.model, &model))
            {
                scoped_models.push(ScopedModel {
                    model,
                    thinking_level: parsed.thinking_level,
                });
            }
        } else {
            eprintln!("Warning: No models match pattern \"{pattern}\"");
        }
    }

    scoped_models
}

fn parse_model_pattern(pattern: &str, available_models: &[ModelEntry]) -> ParsedModelResult {
    // Try stripping a valid thinking-level suffix FIRST. This prevents
    // `provider/model:high` from being swallowed by `ad_hoc_model_entry`
    // which would create a model with id `model:high` instead of `model`.
    if let Some((prefix, suffix)) = pattern.rsplit_once(':')
        && let Some(thinking_level) = parse_thinking_level_opt(suffix)
    {
        let result = parse_model_pattern(prefix, available_models);
        if result.model.is_some() {
            return ParsedModelResult {
                model: result.model,
                thinking_level: if result.warning.is_some() {
                    None
                } else {
                    Some(thinking_level)
                },
                warning: result.warning,
            };
        }
    }

    if let Some(model) = try_match_model(pattern, available_models) {
        return ParsedModelResult {
            model: Some(model),
            thinking_level: None,
            warning: None,
        };
    }

    let Some((prefix, suffix)) = pattern.rsplit_once(':') else {
        return ParsedModelResult {
            model: None,
            thinking_level: None,
            warning: None,
        };
    };

    // Invalid thinking level suffix — still match the model but warn
    let result = parse_model_pattern(prefix, available_models);
    if result.model.is_some() {
        return ParsedModelResult {
            model: result.model,
            thinking_level: None,
            warning: Some(format!(
                "Invalid thinking level \"{suffix}\" in pattern \"{pattern}\". Using default instead."
            )),
        };
    }

    result
}

fn try_match_model(pattern: &str, available_models: &[ModelEntry]) -> Option<ModelEntry> {
    if let Some((provider, model_id)) = split_provider_model_spec(pattern) {
        if let Some(found) = available_models.iter().find(|m| {
            provider_ids_match(&m.model.provider, provider)
                && m.model.id.eq_ignore_ascii_case(model_id)
        }) {
            return Some(found.clone());
        }

        if let Some(ad_hoc) = crate::models::ad_hoc_model_entry(provider, model_id) {
            return Some(ad_hoc);
        }
    }

    let exact_matches: Vec<ModelEntry> = available_models
        .iter()
        .filter(|m| m.model.id.eq_ignore_ascii_case(pattern))
        .cloned()
        .collect();
    if let Some(found) = select_preferred_exact_id_match(&exact_matches) {
        return Some(found);
    }

    let pattern_lower = pattern.to_lowercase();
    let matches: Vec<ModelEntry> = available_models
        .iter()
        .filter(|m| {
            m.model.id.to_lowercase().contains(&pattern_lower)
                || m.model.name.to_lowercase().contains(&pattern_lower)
        })
        .cloned()
        .collect();

    if matches.is_empty() {
        return None;
    }

    let mut aliases: Vec<ModelEntry> = matches
        .iter()
        .filter(|m| is_alias(&m.model.id))
        .cloned()
        .collect();
    let mut dated: Vec<ModelEntry> = matches
        .iter()
        .filter(|m| !is_alias(&m.model.id))
        .cloned()
        .collect();

    if !aliases.is_empty() {
        aliases.sort_by(|a, b| b.model.id.cmp(&a.model.id));
        return aliases.first().cloned();
    }

    dated.sort_by(|a, b| b.model.id.cmp(&a.model.id));
    dated.first().cloned()
}

fn is_alias(model_id: &str) -> bool {
    if model_id.ends_with("-latest") {
        return true;
    }

    // Check for OpenAI style: YYYY-MM-DD
    let parts: Vec<&str> = model_id.split('-').collect();
    if parts.len() >= 3 {
        let y = parts[parts.len() - 3];
        let m = parts[parts.len() - 2];
        let d = parts[parts.len() - 1];
        if y.len() == 4
            && m.len() == 2
            && d.len() == 2
            && y.chars().all(|c| c.is_ascii_digit())
            && m.chars().all(|c| c.is_ascii_digit())
            && d.chars().all(|c| c.is_ascii_digit())
        {
            return false;
        }
    }

    let Some((_, date_suffix)) = model_id.rsplit_once('-') else {
        return true;
    };

    if date_suffix.len() == 8 && date_suffix.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }

    if date_suffix.len() == 4 && date_suffix.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }

    true
}

fn models_equal(left: &ModelEntry, right: &ModelEntry) -> bool {
    provider_ids_match(&left.model.provider, &right.model.provider)
        && left.model.id.eq_ignore_ascii_case(&right.model.id)
}

pub fn output_final_text(message: &AssistantMessage) {
    for block in &message.content {
        if let ContentBlock::Text(text) = block {
            println!("{}", text.text);
        }
    }
}

pub fn render_session_html(session: &Session) -> String {
    session.to_html()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use clap::Parser;
    use tempfile::tempdir;

    use super::*;
    use crate::auth::AuthStorage;
    use crate::provider::{InputType, Model, ModelCost};

    fn test_model_entry(id: &str, provider: &str, reasoning: bool) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-responses".to_string(),
                provider: provider.to_string(),
                base_url: "https://example.test/v1".to_string(),
                reasoning,
                input: vec![InputType::Text],
                cost: ModelCost {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                },
                context_window: 128_000,
                max_tokens: 8_192,
                headers: HashMap::new(),
            },
            api_key: Some("test-key".to_string()),
            headers: HashMap::new(),
            auth_header: true,
            compat: None,
            oauth_config: None,
        }
    }

    fn registry_with_entries(entries: Vec<ModelEntry>) -> ModelRegistry {
        ModelRegistry::from_entries_for_tests(entries)
    }

    #[test]
    fn parse_models_arg_splits_and_trims() {
        assert_eq!(
            parse_models_arg("gpt-4*, claude* ,,"),
            vec!["gpt-4*".to_string(), "claude*".to_string()]
        );
    }

    #[test]
    fn default_model_from_available_prefers_azure_legacy_default() {
        let available = vec![
            test_model_entry("gpt-4o-mini", "azure-openai-responses", true),
            test_model_entry("gpt-5.2", "azure-openai-responses", true),
        ];

        let selected = default_model_from_available(&available);
        assert_eq!(selected.model.provider, "azure-openai-responses");
        assert_eq!(selected.model.id, "gpt-5.2");
    }

    #[test]
    fn default_model_from_available_applies_vercel_gateway_alias_mapping() {
        let available = vec![
            test_model_entry("gpt-4o-mini", "vercel", true),
            test_model_entry("anthropic/claude-opus-4.5", "vercel", true),
        ];

        let selected = default_model_from_available(&available);
        assert_eq!(selected.model.provider, "vercel");
        assert_eq!(selected.model.id, "anthropic/claude-opus-4.5");
    }

    #[test]
    fn resolve_api_key_allows_keyless_model_when_credentials_not_required() {
        let dir = tempdir().expect("tempdir");
        let auth = AuthStorage::load(dir.path().join("auth.json")).expect("load auth");
        let mut entry = test_model_entry("llama3.2", "ollama", false);
        entry.api_key = None;
        entry.auth_header = false;

        let cli = cli::Cli::parse_from(["pi"]);
        let resolved = resolve_api_key(&auth, &cli, &entry).expect("resolve keyless model");
        assert!(resolved.is_none());
    }

    #[test]
    fn resolve_api_key_still_requires_credentials_for_remote_provider() {
        let dir = tempdir().expect("tempdir");
        let auth = AuthStorage::load(dir.path().join("auth.json")).expect("load auth");
        let mut entry = test_model_entry("gpt-4o-mini", "openai", true);
        entry.api_key = None;
        entry.auth_header = true;

        let cli = cli::Cli::parse_from(["pi"]);
        let err = resolve_api_key(&auth, &cli, &entry).unwrap_err();
        let startup = err
            .downcast_ref::<StartupError>()
            .expect("missing key should map to startup error");
        assert!(matches!(
            startup,
            StartupError::MissingApiKey { provider } if provider == "openai"
        ));
    }

    #[test]
    fn default_model_from_available_applies_kimi_coding_alias_mapping() {
        let available = vec![
            test_model_entry("kimi-k2-instruct", "kimi-for-coding", true),
            test_model_entry("kimi-k2-thinking", "kimi-for-coding", true),
        ];

        let selected = default_model_from_available(&available);
        assert_eq!(selected.model.provider, "kimi-for-coding");
        assert_eq!(selected.model.id, "kimi-k2-thinking");
    }

    #[test]
    fn default_model_from_available_prefers_latest_openai_codex_default() {
        let available = vec![
            test_model_entry("gpt-5.3-codex", "openai-codex", true),
            test_model_entry("gpt-5.4", "openai-codex", true),
        ];

        let selected = default_model_from_available(&available);
        assert_eq!(selected.model.provider, "openai-codex");
        assert_eq!(selected.model.id, "gpt-5.4");
    }

    #[test]
    fn default_model_from_available_matches_default_id_case_insensitively() {
        let available = vec![test_model_entry("GPT-5.4", "openai-codex", true)];
        let selected = default_model_from_available(&available);
        assert_eq!(selected.model.provider, "openai-codex");
        assert_eq!(selected.model.id, "GPT-5.4");
    }

    #[test]
    fn apply_piped_stdin_trims_newlines_and_prepends_message() {
        let mut cli = cli::Cli::parse_from(["pi", "existing-message"]);
        apply_piped_stdin(&mut cli, Some("from-stdin\n".to_string()));

        assert!(cli.print);
        assert_eq!(
            cli.args,
            vec!["from-stdin".to_string(), "existing-message".to_string()]
        );
    }

    #[test]
    fn apply_piped_stdin_ignores_empty_input() {
        let mut cli = cli::Cli::parse_from(["pi", "existing-message"]);
        apply_piped_stdin(&mut cli, Some("\n".to_string()));

        assert!(!cli.print);
        assert_eq!(cli.args, vec!["existing-message".to_string()]);
    }

    #[test]
    fn normalize_cli_enables_no_session_for_print_and_lowercases_provider() {
        let mut cli = cli::Cli::parse_from(["pi", "--provider", "OpenAI", "--print", "hello"]);
        assert!(!cli.no_session);
        assert_eq!(cli.provider.as_deref(), Some("OpenAI"));

        normalize_cli(&mut cli);

        assert!(cli.no_session);
        assert_eq!(cli.provider.as_deref(), Some("openai"));
    }

    #[test]
    fn validate_rpc_args_rejects_file_arguments() {
        let cli = cli::Cli::parse_from(["pi", "--mode", "rpc", "@src/main.rs", "hello"]);

        let err = validate_rpc_args(&cli).expect_err("rpc mode should reject @file args");
        assert!(
            err.to_string()
                .contains("@file arguments are not supported in RPC mode")
        );
    }

    #[test]
    fn validate_rpc_args_allows_print_with_explicit_mode_rpc() {
        let cli = cli::Cli::parse_from(["pi", "--mode", "rpc", "--print", "hello"]);

        assert!(validate_rpc_args(&cli).is_ok());
    }

    #[test]
    fn validate_rpc_args_allows_non_rpc_file_arguments() {
        let cli = cli::Cli::parse_from(["pi", "--mode", "json", "@src/main.rs", "hello"]);
        assert!(validate_rpc_args(&cli).is_ok());
    }

    #[test]
    fn parse_model_pattern_prefers_alias_when_alias_and_dated_match() {
        let available = vec![
            test_model_entry("gpt-5.1-codex-20250101", "openai", true),
            test_model_entry("gpt-5.1-codex-latest", "openai", true),
        ];

        let parsed = parse_model_pattern("gpt-5.1-codex", &available);
        let model = parsed.model.expect("model should match");

        assert_eq!(model.model.id, "gpt-5.1-codex-latest");
        assert!(parsed.thinking_level.is_none());
        assert!(parsed.warning.is_none());
    }

    #[test]
    fn try_match_model_prefers_existing_entry_for_provider_alias() {
        let mut openrouter = test_model_entry("openai/gpt-4o-mini", "openrouter", true);
        openrouter
            .headers
            .insert("x-test".to_string(), "1".to_string());

        let matched = try_match_model("open-router/openai/gpt-4o-mini", &[openrouter.clone()])
            .expect("provider alias should match existing entry");

        assert_eq!(matched.model.provider, "openrouter");
        assert_eq!(matched.model.id, "openai/gpt-4o-mini");
        assert_eq!(
            matched.headers.get("x-test").map(String::as_str),
            Some("1"),
            "must preserve existing model metadata instead of falling back to ad-hoc"
        );
    }

    #[test]
    fn select_model_and_thinking_provider_only_accepts_provider_alias() {
        let cli = cli::Cli::parse_from(["pi", "--provider", "open-router"]);
        let config = Config::default();
        let session = Session::in_memory();
        let registry = registry_with_entries(vec![test_model_entry(
            "openai/gpt-4o-mini",
            "openrouter",
            true,
        )]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("provider alias should resolve");

        assert!(provider_ids_match(
            &selection.model_entry.model.provider,
            "open-router"
        ));
        assert!(!selection.model_entry.model.id.is_empty());
    }

    #[test]
    fn select_model_and_thinking_provider_only_prefers_ready_model() {
        let cli = cli::Cli::parse_from(["pi", "--provider", "acme"]);
        let config = Config::default();
        let session = Session::in_memory();

        let mut unready_remote = test_model_entry("cloud-model", "acme", true);
        unready_remote.api_key = None;
        unready_remote.auth_header = true;

        let mut keyless_ready = test_model_entry("local-model", "acme", false);
        keyless_ready.api_key = None;
        keyless_ready.auth_header = false;

        let registry = registry_with_entries(vec![unready_remote, keyless_ready]);
        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("provider selection should prefer ready models");

        assert_eq!(selection.model_entry.model.provider, "acme");
        assert_eq!(selection.model_entry.model.id, "local-model");
    }

    #[test]
    fn select_model_and_thinking_provider_only_synthesizes_ad_hoc_for_coding_plan_provider() {
        // Coding-plan providers have no registry entries; selecting them by
        // provider alone must synthesize an ad-hoc model from the default table
        // instead of failing with "No models available".
        for (provider_arg, expected_model_id) in [
            ("zai-coding-plan", "glm-5.2"),
            ("minimax-coding-plan", "MiniMax-M3"),
            ("kimi-for-coding", "kimi-for-coding"),
        ] {
            let cli = cli::Cli::parse_from(["pi", "--provider", provider_arg]);
            let config = Config::default();
            let session = Session::in_memory();
            let registry =
                registry_with_entries(vec![test_model_entry("unrelated-model", "openai", true)]);

            let selection = select_model_and_thinking(
                &cli,
                &config,
                &session,
                &registry,
                &[],
                Path::new("/tmp"),
            )
            .unwrap_or_else(|err| {
                panic!("provider {provider_arg} should synthesize an ad-hoc model: {err}")
            });

            assert!(
                provider_ids_match(&selection.model_entry.model.provider, provider_arg),
                "synthesized entry should belong to provider {provider_arg}"
            );
            assert_eq!(
                selection.model_entry.model.id, expected_model_id,
                "provider {provider_arg} should default to {expected_model_id}"
            );
        }
    }

    #[test]
    fn provider_default_model_id_resolves_coding_plan_and_corrected_defaults() {
        assert_eq!(
            provider_default_model_id("zai-coding-plan"),
            Some("glm-5.2")
        );
        assert_eq!(provider_default_model_id("zai"), Some("glm-4.7"));
        assert_eq!(
            provider_default_model_id("minimax-coding-plan"),
            Some("MiniMax-M3")
        );
        assert_eq!(provider_default_model_id("minimax"), Some("MiniMax-M3"));
        // The Kimi for Coding plan uses a stable virtual model id.
        assert_eq!(
            provider_default_model_id("kimi-for-coding"),
            Some("kimi-for-coding")
        );
        // Legacy alias still resolves via canonicalization.
        assert_eq!(
            provider_default_model_id("kimi-coding"),
            Some("kimi-for-coding")
        );
        assert_eq!(provider_default_model_id("totally-unknown"), None);
    }

    #[test]
    fn select_model_and_thinking_provider_only_prefers_provider_default_over_registry_order() {
        let cli = cli::Cli::parse_from(["pi", "--provider", "openai"]);
        let config = Config::default();
        let session = Session::in_memory();
        let registry = registry_with_entries(vec![
            test_model_entry("gpt-4o", "openai", true),
            test_model_entry("gpt-5.4", "openai", true),
        ]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("provider-only selection should honor preferred defaults");

        assert_eq!(selection.model_entry.model.provider, "openai");
        assert_eq!(selection.model_entry.model.id, "gpt-5.4");
    }

    #[test]
    fn select_model_and_thinking_provider_only_honors_configured_default_model() {
        let cli = cli::Cli::parse_from(["pi", "--provider", "openai"]);
        let config = Config {
            default_model: Some("gpt-4o-mini".to_string()),
            ..Config::default()
        };
        let session = Session::in_memory();
        let registry = registry_with_entries(vec![
            test_model_entry("gpt-5.4", "openai", true),
            test_model_entry("gpt-4o-mini", "openai", true),
        ]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("provider-only selection should honor configured default_model");

        assert_eq!(selection.model_entry.model.provider, "openai");
        assert_eq!(selection.model_entry.model.id, "gpt-4o-mini");
    }

    #[test]
    fn select_model_and_thinking_provider_only_skips_unready_configured_default_model() {
        let cli = cli::Cli::parse_from(["pi", "--provider", "acme"]);
        let config = Config {
            default_model: Some("cloud-model".to_string()),
            ..Config::default()
        };
        let session = Session::in_memory();

        let mut unready_remote = test_model_entry("cloud-model", "acme", true);
        unready_remote.api_key = None;
        unready_remote.auth_header = true;

        let mut keyless_ready = test_model_entry("local-model", "acme", false);
        keyless_ready.api_key = None;
        keyless_ready.auth_header = false;

        let registry = registry_with_entries(vec![unready_remote, keyless_ready]);
        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("provider-only selection should still prefer a ready model");

        assert_eq!(selection.model_entry.model.provider, "acme");
        assert_eq!(selection.model_entry.model.id, "local-model");
    }

    #[test]
    fn select_model_and_thinking_preserves_restore_warning_when_defaulting_for_setup() {
        let cli = cli::Cli::parse_from(["pi"]);
        let config = Config::default();
        let mut session = Session::in_memory();
        session.append_model_change("missing-provider".to_string(), "missing-model".to_string());

        let mut setup_default = test_model_entry("gpt-5.4", "openai-codex", true);
        setup_default.api_key = None;
        setup_default.auth_header = true;

        let registry = registry_with_entries(vec![setup_default]);
        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("selection should fall back to a stable setup model");

        assert_eq!(selection.model_entry.model.provider, "openai-codex");
        assert_eq!(selection.model_entry.model.id, "gpt-5.4");
        assert_eq!(
            selection.fallback_message.as_deref(),
            Some(
                "Could not restore model missing-provider/missing-model (model no longer exists). Defaulting to openai-codex/gpt-5.4 for setup."
            )
        );
    }

    #[test]
    fn select_model_and_thinking_preserves_restore_warning_when_using_config_default() {
        let cli = cli::Cli::parse_from(["pi"]);
        let config = Config {
            default_provider: Some("openai-codex".to_string()),
            default_model: Some("gpt-4o-mini".to_string()),
            ..Config::default()
        };
        let mut session = Session::in_memory();
        session.append_model_change("missing-provider".to_string(), "missing-model".to_string());

        let registry =
            registry_with_entries(vec![test_model_entry("gpt-4o-mini", "openai-codex", true)]);
        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("selection should use the configured default model");

        assert_eq!(selection.model_entry.model.provider, "openai-codex");
        assert_eq!(selection.model_entry.model.id, "gpt-4o-mini");
        assert_eq!(
            selection.fallback_message.as_deref(),
            Some(
                "Could not restore model missing-provider/missing-model (model no longer exists). Using openai-codex/gpt-4o-mini."
            )
        );
    }

    #[test]
    fn select_model_and_thinking_restores_model_from_header_when_history_missing() {
        let cli = cli::Cli::parse_from(["pi"]);
        let config = Config::default();
        let mut session = Session::in_memory();
        session.header.provider = Some("openai-codex".to_string());
        session.header.model_id = Some("gpt-5.4".to_string());

        let registry = registry_with_entries(vec![
            test_model_entry("gpt-5.4", "openai-codex", true),
            test_model_entry("gpt-4o-mini", "openai", true),
        ]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("header-only session should restore saved model");

        assert_eq!(selection.model_entry.model.provider, "openai-codex");
        assert_eq!(selection.model_entry.model.id, "gpt-5.4");
    }

    #[test]
    fn select_model_and_thinking_restores_thinking_from_header_when_history_missing() {
        let cli = cli::Cli::parse_from(["pi", "--continue"]);
        let config = Config::default();
        let mut session = Session::in_memory();
        session.header.provider = Some("openai-codex".to_string());
        session.header.model_id = Some("gpt-5.4".to_string());
        session.header.thinking_level = Some("high".to_string());

        let registry =
            registry_with_entries(vec![test_model_entry("gpt-5.4", "openai-codex", true)]);
        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("header-only session should restore saved thinking level");

        assert_eq!(selection.thinking_level, model::ThinkingLevel::High);
    }

    #[test]
    fn select_model_and_thinking_restores_model_from_active_branch_only() {
        let cli = cli::Cli::parse_from(["pi"]);
        let config = Config::default();
        let mut session = Session::in_memory();
        let root_id = session.append_message(crate::session::SessionMessage::User {
            content: crate::model::UserContent::Text("root".to_string()),
            timestamp: Some(0),
        });
        let openai_id =
            session.append_model_change("openai-codex".to_string(), "test-gpt-5.4".to_string());
        assert!(session.create_branch_from(&root_id));
        session.append_model_change("anthropic".to_string(), "test-claude-sonnet-4".to_string());
        assert!(session.create_branch_from(&openai_id));

        let registry = registry_with_entries(vec![
            test_model_entry("test-gpt-5.4", "openai-codex", true),
            test_model_entry("test-claude-sonnet-4", "anthropic", true),
        ]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("active branch model should restore");

        assert_eq!(selection.model_entry.model.provider, "openai-codex");
        assert_eq!(selection.model_entry.model.id, "test-gpt-5.4");
    }

    #[test]
    fn select_model_and_thinking_restores_thinking_from_active_branch_only() {
        let cli = cli::Cli::parse_from(["pi", "--continue"]);
        let config = Config::default();
        let mut session = Session::in_memory();
        session.header.provider = Some("openai-codex".to_string());
        session.header.model_id = Some("gpt-5.4".to_string());
        let root_id = session.append_message(crate::session::SessionMessage::User {
            content: crate::model::UserContent::Text("root".to_string()),
            timestamp: Some(0),
        });
        let high_id = session.append_thinking_level_change("high".to_string());
        assert!(session.create_branch_from(&root_id));
        session.append_thinking_level_change("minimal".to_string());
        assert!(session.create_branch_from(&high_id));

        let registry =
            registry_with_entries(vec![test_model_entry("gpt-5.4", "openai-codex", true)]);
        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("active branch thinking level should restore");

        assert_eq!(selection.thinking_level, model::ThinkingLevel::High);
    }

    #[test]
    fn update_session_for_selection_skips_duplicate_changes_for_header_only_session() {
        let mut session = Session::in_memory();
        session.header.provider = Some("openai-codex".to_string());
        session.header.model_id = Some("gpt-5.4".to_string());
        session.header.thinking_level = Some("high".to_string());

        let selection = ModelSelection {
            model_entry: test_model_entry("gpt-5.4", "openai-codex", true),
            thinking_level: model::ThinkingLevel::High,
            scoped_models: Vec::new(),
            fallback_message: None,
        };

        update_session_for_selection(&mut session, &selection);

        assert!(
            session.entries.is_empty(),
            "header-only session with unchanged selection should not invent history entries"
        );
    }

    #[test]
    fn update_session_for_selection_preserves_alias_equivalent_model_state() {
        let mut session = Session::in_memory();
        session.append_model_change("gemini".to_string(), "gemini-2.5-pro".to_string());
        session.set_model_header(
            Some("gemini".to_string()),
            Some("gemini-2.5-pro".to_string()),
            Some("high".to_string()),
        );

        let selection = ModelSelection {
            model_entry: test_model_entry("GEMINI-2.5-PRO", "google", true),
            thinking_level: model::ThinkingLevel::High,
            scoped_models: Vec::new(),
            fallback_message: None,
        };

        update_session_for_selection(&mut session, &selection);

        let model_changes: Vec<_> = session
            .entries_for_current_path()
            .iter()
            .filter_map(|entry| {
                if let crate::session::SessionEntry::ModelChange(change) = entry {
                    Some((change.provider.as_str(), change.model_id.as_str()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            model_changes,
            vec![("gemini", "gemini-2.5-pro")],
            "alias-equivalent startup restore should not append duplicate history"
        );
        assert_eq!(session.header.provider.as_deref(), Some("gemini"));
        assert_eq!(session.header.model_id.as_deref(), Some("gemini-2.5-pro"));
    }

    #[test]
    fn select_model_and_thinking_model_only_prefers_default_provider_alias() {
        let model_id = "__test-openrouter-alias-model__";
        let cli = cli::Cli::parse_from(["pi", "--model", model_id]);
        let config = Config {
            default_provider: Some("open-router".to_string()),
            ..Config::default()
        };
        let session = Session::in_memory();
        let registry = registry_with_entries(vec![
            test_model_entry(model_id, "openai", true),
            test_model_entry(model_id, "openrouter", true),
        ]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("default provider alias should resolve in model-only selection");

        assert_eq!(selection.model_entry.model.provider, "openrouter");
        assert_eq!(selection.model_entry.model.id, model_id);
    }

    #[test]
    fn select_model_and_thinking_model_only_matches_case_insensitively() {
        let model_id = "__test-case-insensitive-model__";
        let cli = cli::Cli::parse_from(["pi", "--model", "__TEST-CASE-INSENSITIVE-MODEL__"]);
        let config = Config::default();
        let session = Session::in_memory();
        let registry = registry_with_entries(vec![test_model_entry(model_id, "openai", true)]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("model-only selection should be case-insensitive");

        assert_eq!(selection.model_entry.model.provider, "openai");
        assert_eq!(selection.model_entry.model.id, model_id);
    }

    #[test]
    fn select_model_and_thinking_model_only_prefers_openai_codex_for_duplicate_latest_id() {
        let cli = cli::Cli::parse_from(["pi", "--model", "gpt-5.4"]);
        let config = Config::default();
        let session = Session::in_memory();
        let registry = registry_with_entries(vec![
            test_model_entry("gpt-5.4", "openai", true),
            test_model_entry("gpt-5.4", "openai-codex", true),
        ]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("duplicate exact-id matches should honor preferred provider ordering");

        assert_eq!(selection.model_entry.model.provider, "openai-codex");
        assert_eq!(selection.model_entry.model.id, "gpt-5.4");
    }

    #[test]
    fn select_model_and_thinking_model_only_prefers_ready_duplicate_exact_id_match() {
        let model_id = "__test-ready-duplicate-model__";
        let cli = cli::Cli::parse_from(["pi", "--model", model_id]);
        let config = Config {
            default_provider: None,
            ..Config::default()
        };
        let session = Session::in_memory();
        let mut codex = test_model_entry(model_id, "openai-codex", true);
        codex.api_key = None;
        codex.auth_header = true;
        let registry =
            registry_with_entries(vec![test_model_entry(model_id, "openai", true), codex]);

        let selection =
            select_model_and_thinking(&cli, &config, &session, &registry, &[], Path::new("/tmp"))
                .expect("duplicate exact-id matches should still prefer ready entries");

        assert_eq!(selection.model_entry.model.provider, "openai");
        assert_eq!(selection.model_entry.model.id, model_id);
    }

    #[test]
    fn select_model_and_thinking_scoped_models_prefers_default_provider_alias() {
        let cli = cli::Cli::parse_from(["pi"]);
        let config = Config {
            default_provider: Some("open-router".to_string()),
            default_model: Some("gpt-4o-mini".to_string()),
            ..Config::default()
        };
        let session = Session::in_memory();
        let registry = registry_with_entries(Vec::new());
        let scoped_models = vec![
            ScopedModel {
                model: test_model_entry("gpt-4o-mini", "openai", true),
                thinking_level: None,
            },
            ScopedModel {
                model: test_model_entry("gpt-4o-mini", "openrouter", true),
                thinking_level: Some(model::ThinkingLevel::High),
            },
        ];

        let selection = select_model_and_thinking(
            &cli,
            &config,
            &session,
            &registry,
            &scoped_models,
            Path::new("/tmp"),
        )
        .expect("scoped models should honor default provider alias");

        assert_eq!(selection.model_entry.model.provider, "openrouter");
        assert_eq!(selection.model_entry.model.id, "gpt-4o-mini");
        assert_eq!(selection.thinking_level, model::ThinkingLevel::High);
    }

    #[test]
    fn select_model_and_thinking_scoped_models_matches_default_model_case_insensitively() {
        let cli = cli::Cli::parse_from(["pi"]);
        let config = Config {
            default_provider: Some("open-router".to_string()),
            default_model: Some("GPT-4O-MINI".to_string()),
            ..Config::default()
        };
        let session = Session::in_memory();
        let registry = registry_with_entries(Vec::new());
        let scoped_models = vec![
            ScopedModel {
                model: test_model_entry("gpt-4o-mini", "openrouter", true),
                thinking_level: Some(model::ThinkingLevel::Low),
            },
            ScopedModel {
                model: test_model_entry("gpt-4o", "openrouter", true),
                thinking_level: Some(model::ThinkingLevel::High),
            },
        ];

        let selection = select_model_and_thinking(
            &cli,
            &config,
            &session,
            &registry,
            &scoped_models,
            Path::new("/tmp"),
        )
        .expect("scoped default model should match case-insensitively");

        assert_eq!(selection.model_entry.model.provider, "openrouter");
        assert_eq!(selection.model_entry.model.id, "gpt-4o-mini");
        assert_eq!(selection.thinking_level, model::ThinkingLevel::Low);
    }

    #[test]
    fn parse_model_pattern_picks_latest_dated_when_no_alias_exists() {
        let available = vec![
            test_model_entry("gpt-5.1-codex-20250101", "openai", true),
            test_model_entry("gpt-5.1-codex-20250601", "openai", true),
        ];

        let parsed = parse_model_pattern("gpt-5.1-codex", &available);
        let model = parsed.model.expect("model should match");

        assert_eq!(model.model.id, "gpt-5.1-codex-20250601");
        assert!(parsed.thinking_level.is_none());
        assert!(parsed.warning.is_none());
    }

    #[test]
    fn split_provider_model_spec_preserves_nested_model_paths() {
        let parsed = split_provider_model_spec("openrouter/anthropic/claude-sonnet-4.5")
            .expect("provider/model spec");
        assert_eq!(parsed.0, "openrouter");
        assert_eq!(parsed.1, "anthropic/claude-sonnet-4.5");

        assert!(split_provider_model_spec("openrouter/").is_none());
        assert!(split_provider_model_spec("/anthropic/claude").is_none());
        assert!(split_provider_model_spec("no-slash").is_none());
    }

    #[test]
    fn try_match_model_supports_openrouter_dynamic_provider_model_ids() {
        let matched = try_match_model("openrouter/google/gemini-2.5-pro", &[])
            .expect("openrouter ad-hoc fallback should resolve");
        assert_eq!(matched.model.provider, "openrouter");
        assert_eq!(matched.model.id, "google/gemini-2.5-pro");
        assert_eq!(matched.model.api, "openai-completions");
        assert_eq!(matched.model.base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn try_match_model_prefers_openai_codex_for_duplicate_exact_id_matches() {
        let matched = try_match_model(
            "gpt-5.4",
            &[
                test_model_entry("gpt-5.4", "openai", true),
                test_model_entry("gpt-5.4", "openai-codex", true),
            ],
        )
        .expect("duplicate exact-id matches should honor preferred provider ordering");

        assert_eq!(matched.model.provider, "openai-codex");
        assert_eq!(matched.model.id, "gpt-5.4");
    }

    #[test]
    fn is_alias_handles_non_ascii_model_ids_without_panicking() {
        assert!(is_alias("é123456789"));
        assert!(is_alias("model-é2345678"));
        assert!(!is_alias("model-20250101"));
    }

    #[test]
    fn parse_model_pattern_parses_thinking_suffix() {
        let available = vec![test_model_entry("gpt-5.1-codex", "openai", true)];
        let parsed = parse_model_pattern("openai/gpt-5.1-codex:high", &available);

        let model = parsed.model.expect("model should match");
        assert_eq!(model.model.id, "gpt-5.1-codex");
        assert_eq!(parsed.thinking_level, Some(model::ThinkingLevel::High));
        assert!(parsed.warning.is_none());
    }

    #[test]
    fn parse_model_pattern_warns_for_invalid_thinking_suffix() {
        let available = vec![test_model_entry("gpt-5.1-codex", "openai", true)];
        let parsed = parse_model_pattern("gpt-5.1-codex:extreme", &available);

        assert!(parsed.model.is_some());
        assert!(parsed.thinking_level.is_none());
        assert!(
            parsed
                .warning
                .expect("warning should be present")
                .contains("Invalid thinking level")
        );
    }

    #[test]
    fn clamp_thinking_level_returns_off_for_non_reasoning_models() {
        let model_entry = test_model_entry("gpt-4o-mini", "openai", false);
        let clamped = model_entry.clamp_thinking_level(model::ThinkingLevel::High);
        assert_eq!(clamped, model::ThinkingLevel::Off);
    }

    #[test]
    fn clamp_thinking_level_clamps_xhigh_for_unsupported_models() {
        let model_entry = test_model_entry("gpt-4o", "openai", true);
        let clamped = model_entry.clamp_thinking_level(model::ThinkingLevel::XHigh);
        assert_eq!(clamped, model::ThinkingLevel::High);
    }

    #[test]
    fn clamp_thinking_level_keeps_xhigh_for_supported_models() {
        let model_entry = test_model_entry("gpt-5.2", "openai", true);
        let clamped = model_entry.clamp_thinking_level(model::ThinkingLevel::XHigh);
        assert_eq!(clamped, model::ThinkingLevel::XHigh);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        // ====================================================================
        // parse_models_arg
        // ====================================================================

        proptest! {
            #[test]
            fn parse_models_no_empty_strings(s in "([a-z0-9*-]{0,5},?){0,6}") {
                let result = parse_models_arg(&s);
                for m in &result {
                    assert!(!m.is_empty(), "parse_models_arg produced empty string from {s:?}");
                }
            }

            #[test]
            fn parse_models_whitespace_trimmed(m1 in "[a-z]{1,8}", m2 in "[a-z]{1,8}") {
                let with_spaces = format!("  {m1}  ,  {m2}  ");
                let result = parse_models_arg(&with_spaces);
                assert_eq!(result, vec![m1, m2]);
            }

            #[test]
            fn parse_models_round_trip(models in prop::collection::vec("[a-z0-9-]{1,10}", 1..6)) {
                let joined = models.join(",");
                let result = parse_models_arg(&joined);
                assert_eq!(result, models);
            }

            #[test]
            fn parse_models_empty_csv(s in "[ ,]*") {
                let result = parse_models_arg(&s);
                assert!(result.is_empty(), "whitespace/commas-only should yield empty vec");
            }
        }

        // ====================================================================
        // apply_piped_stdin / normalize_cli
        // ====================================================================

        proptest! {
            #[test]
            fn apply_piped_stdin_trims_sets_print_and_prepends(
                existing in prop::collection::vec("[A-Za-z0-9._/-]{1,16}", 0..4),
                leading_ws in "[ \\t\\n\\r]{0,4}",
                core in "[A-Za-z0-9._/-]{1,24}",
                trailing_ws in "[ \\t\\n\\r]{0,4}",
            ) {
                let mut cli = cli::Cli::parse_from(["pi"]);
                cli.args = existing.clone();
                cli.print = false;

                let raw = format!("{leading_ws}{core}{trailing_ws}");
                apply_piped_stdin(&mut cli, Some(raw));

                prop_assert!(cli.print);
                prop_assert_eq!(cli.args.len(), existing.len() + 1);
                prop_assert_eq!(cli.args.first().map(String::as_str), Some(core.as_str()));
                prop_assert_eq!(&cli.args[1..], existing.as_slice());
            }

            #[test]
            fn apply_piped_stdin_none_or_whitespace_is_noop(
                existing in prop::collection::vec("[A-Za-z0-9._/-]{1,16}", 0..4),
                initial_print in any::<bool>(),
                initial_no_session in any::<bool>(),
                whitespace in "[ \\t\\n\\r]{0,16}",
            ) {
                let mut cli = cli::Cli::parse_from(["pi"]);
                cli.args = existing.clone();
                cli.print = initial_print;
                cli.no_session = initial_no_session;

                apply_piped_stdin(&mut cli, None);
                prop_assert_eq!(&cli.args, &existing);
                prop_assert_eq!(cli.print, initial_print);
                prop_assert_eq!(cli.no_session, initial_no_session);

                apply_piped_stdin(&mut cli, Some(whitespace));
                prop_assert_eq!(&cli.args, &existing);
                prop_assert_eq!(cli.print, initial_print);
                prop_assert_eq!(cli.no_session, initial_no_session);
            }

            #[test]
            fn normalize_cli_lowercases_provider_and_applies_print_semantics(
                provider in prop::option::of("[A-Za-z0-9_-]{1,20}"),
                print in any::<bool>(),
                initial_no_session in any::<bool>(),
            ) {
                let mut cli = cli::Cli::parse_from(["pi"]);
                cli.provider = provider.clone();
                cli.print = print;
                cli.no_session = initial_no_session;

                normalize_cli(&mut cli);

                let expected_provider = provider.map(|value: String| value.to_ascii_lowercase());
                let expected_no_session = if print { true } else { initial_no_session };

                prop_assert_eq!(cli.provider, expected_provider);
                prop_assert_eq!(cli.no_session, expected_no_session);
            }

            #[test]
            fn normalize_cli_is_idempotent(
                provider in prop::option::of("[A-Za-z0-9_-]{1,20}"),
                print in any::<bool>(),
                initial_no_session in any::<bool>(),
            ) {
                let mut cli = cli::Cli::parse_from(["pi"]);
                cli.provider = provider;
                cli.print = print;
                cli.no_session = initial_no_session;

                normalize_cli(&mut cli);
                let provider_once = cli.provider.clone();
                let no_session_once = cli.no_session;
                let print_once = cli.print;

                normalize_cli(&mut cli);

                prop_assert_eq!(cli.provider, provider_once);
                prop_assert_eq!(cli.no_session, no_session_once);
                prop_assert_eq!(cli.print, print_once);
            }
        }

        // ====================================================================
        // split_provider_model_spec
        // ====================================================================

        proptest! {
            #[test]
            fn split_spec_first_slash(pre in "[a-z]{1,8}", mid in "[a-z]{1,8}", post in "[a-z]{1,8}") {
                let input = format!("{pre}/{mid}/{post}");
                let (p, m) = split_provider_model_spec(&input).unwrap();
                assert_eq!(p, pre.as_str());
                assert_eq!(m, format!("{mid}/{post}"));
            }

            #[test]
            fn split_spec_trims_whitespace(p in "[a-z]{1,6}", m in "[a-z]{1,6}") {
                let input = format!("  {p}  /  {m}  ");
                let (prov, model) = split_provider_model_spec(&input).unwrap();
                assert_eq!(prov, p.as_str());
                assert_eq!(model, m.as_str());
            }

            #[test]
            fn split_spec_rejects_empty_halves(valid in "[a-z]{1,8}") {
                assert!(split_provider_model_spec(&format!("{valid}/")).is_none());
                assert!(split_provider_model_spec(&format!("/{valid}")).is_none());
            }

            #[test]
            fn split_spec_none_without_slash(s in "[a-z0-9]{1,12}") {
                assert!(split_provider_model_spec(&s).is_none());
            }
        }

        // ====================================================================
        // is_alias
        // ====================================================================

        proptest! {
            #[test]
            fn is_alias_latest_suffix(prefix in "[a-z]{1,10}") {
                assert!(is_alias(&format!("{prefix}-latest")));
            }

            #[test]
            fn is_alias_eight_digits_not_alias(prefix in "[a-z]{1,8}", d in "[0-9]{8}") {
                let id = format!("{prefix}-{d}");
                assert!(!is_alias(&id), "{id} should not be alias (8-digit suffix)");
            }

            #[test]
            fn is_alias_non_eight_digit_suffix(prefix in "[a-z]{1,6}", suffix in "[a-z0-9]{1,7}") {
                let id = format!("{prefix}-{suffix}");
                let is_pure_digits = suffix.chars().all(|c| c.is_ascii_digit());
                if is_pure_digits && (suffix.len() == 8 || suffix.len() == 4) {
                    assert!(!is_alias(&id));
                } else {
                    assert!(is_alias(&id));
                }
            }

            #[test]
            fn is_alias_no_hyphen(id in "[a-z0-9]{1,12}") {
                if !id.contains('-') {
                    assert!(is_alias(&id));
                }
            }

            #[test]
            fn is_alias_non_ascii_no_panic(id in ".{1,20}") {
                let _ = is_alias(&id); // must not panic
            }
        }

        // ====================================================================
        // models_equal
        // ====================================================================

        proptest! {
            #[test]
            fn models_equal_reflexive(provider in "[a-z]{1,6}", id in "[a-z0-9-]{1,10}") {
                let m = test_model_entry(&id, &provider, true);
                assert!(models_equal(&m, &m));
            }

            #[test]
            fn models_equal_symmetric(provider in "[a-z]{1,6}", id in "[a-z0-9-]{1,10}") {
                let a = test_model_entry(&id, &provider, true);
                let b = test_model_entry(&id, &provider, false);
                assert_eq!(models_equal(&a, &b), models_equal(&b, &a));
            }

            #[test]
            fn models_equal_different_providers(id in "[a-z]{1,8}", p1 in "[a-z]{1,5}", p2 in "[a-z]{1,5}") {
                if p1 != p2 {
                    let a = test_model_entry(&id, &p1, true);
                    let b = test_model_entry(&id, &p2, true);
                    assert!(!models_equal(&a, &b));
                }
            }

            #[test]
            fn models_equal_different_ids(id1 in "[a-z]{1,6}", id2 in "[a-z]{1,6}", prov in "[a-z]{1,5}") {
                if id1 != id2 {
                    let a = test_model_entry(&id1, &prov, true);
                    let b = test_model_entry(&id2, &prov, true);
                    assert!(!models_equal(&a, &b));
                }
            }
        }

        #[test]
        fn models_equal_normalizes_provider_aliases_and_model_case() {
            let left = test_model_entry("openai/gpt-4o-mini", "openrouter", true);
            let right = test_model_entry("OPENAI/GPT-4O-MINI", "open-router", false);
            assert!(models_equal(&left, &right));
        }
    }
}
