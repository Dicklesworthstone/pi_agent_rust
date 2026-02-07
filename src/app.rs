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
use crate::models::{ModelEntry, ModelRegistry, default_models_path};
use crate::provider::{StreamOptions, ThinkingBudgets};
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
}

pub fn apply_piped_stdin(cli: &mut cli::Cli, stdin_content: Option<String>) {
    if let Some(stdin_content) = stdin_content {
        let stdin_content = stdin_content.trim_end_matches(&['\r', '\n'][..]);
        if stdin_content.is_empty() {
            return;
        }
        cli.print = true;
        cli.args.insert(0, stdin_content.to_string());
    }
}

#[allow(clippy::missing_const_for_fn)]
pub fn normalize_cli(cli: &mut cli::Cli) {
    if cli.print {
        cli.no_session = true;
    }

    if let Some(provider) = &mut cli.provider {
        *provider = provider.to_ascii_lowercase();
    }
}

pub fn validate_rpc_args(cli: &cli::Cli) -> Result<()> {
    if cli.mode.as_deref() == Some("rpc") && !cli.file_args().is_empty() {
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

pub fn build_system_prompt(
    cli: &cli::Cli,
    cwd: &Path,
    enabled_tools: &[&str],
    skills_prompt: Option<&str>,
    global_dir: &Path,
    package_dir: &Path,
    test_mode: bool,
) -> String {
    use std::fmt::Write as _;

    let custom_prompt = resolve_prompt_input(cli.system_prompt.as_deref(), "system prompt");
    let append_prompt =
        resolve_prompt_input(cli.append_system_prompt.as_deref(), "append system prompt");
    let context_files = load_project_context_files(cwd, global_dir);

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
    let cwd_display = if test_mode {
        "<CWD>".to_string()
    } else {
        cwd.display().to_string()
    };
    let _ = write!(prompt, "\nCurrent working directory: {cwd_display}");

    prompt
}

fn resolve_prompt_input(input: Option<&str>, description: &str) -> Option<String> {
    let value = input?;

    let path = Path::new(value);
    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(err) => {
                eprintln!("Warning: Could not read {description} file {value}: {err}");
                Some(value.to_string())
            }
        }
    } else {
        Some(value.to_string())
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
            "Search file contents for patterns (respects .gitignore)",
        ),
        ("find", "Find files by glob pattern (respects .gitignore)"),
        ("ls", "List directory contents"),
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
        if let Some(context) = load_context_file_from_dir(&current) {
            if seen.insert(context.path.clone()) {
                ancestor_files.push(context);
            }
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
    let now = Local::now();
    let date = format!(
        "{}, {} {}, {}",
        now.format("%A"),
        now.format("%B"),
        now.day(),
        now.year()
    );
    let time = format!("{} {}", now.format("%I:%M:%S %p"), now.format("%Z"));
    format!("{date}, {time}")
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

    if let (Some(provider), Some(model_id)) = (cli.provider.as_deref(), cli.model.as_deref()) {
        let found = registry
            .find(provider, model_id)
            .or_else(|| crate::models::ad_hoc_model_entry(provider, model_id));
        if found.is_none() {
            bail!("Model {provider}/{model_id} not found");
        }
        selected_model = found;
    } else if let Some(provider) = cli.provider.as_deref() {
        let mut candidates: Vec<ModelEntry> = registry
            .models()
            .iter()
            .filter(|m| m.model.provider == provider)
            .cloned()
            .collect();
        if candidates.is_empty() {
            bail!("No models available for provider {provider}");
        }
        if let Some(found) = candidates.iter().find(|m| m.api_key.is_some()) {
            selected_model = Some(found.clone());
        } else {
            selected_model = Some(candidates.remove(0));
        }
    } else if let Some(model_id) = cli.model.as_deref() {
        let matches: Vec<ModelEntry> = registry
            .models()
            .iter()
            .filter(|m| m.model.id == model_id)
            .cloned()
            .collect();
        if matches.is_empty() {
            bail!("Model {model_id} not found");
        }
        if let Some(default_provider) = config.default_provider.as_deref() {
            if let Some(found) = matches
                .iter()
                .find(|m| m.model.provider == default_provider)
            {
                selected_model = Some(found.clone());
            }
        }
        if selected_model.is_none() {
            if let Some(found) = matches.iter().find(|m| m.api_key.is_some()) {
                selected_model = Some(found.clone());
            }
        }
        if selected_model.is_none() {
            selected_model = Some(matches[0].clone());
        }
    } else if !scoped_models.is_empty() && !is_continuing {
        if let (Some(default_provider), Some(default_model)) = (
            config.default_provider.as_deref(),
            config.default_model.as_deref(),
        ) {
            if let Some(found) = scoped_models.iter().find(|sm| {
                sm.model.model.provider == default_provider && sm.model.model.id == default_model
            }) {
                selected_model = Some(found.model.clone());
                if cli.thinking.is_none() {
                    scoped_thinking = found.thinking_level;
                }
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

    if selected_model.is_none() {
        if let Some((provider, model_id)) = last_model_from_session(session) {
            let restore = restore_model_from_session(&provider, &model_id, None, registry);
            selected_model = restore.model;
            fallback_message = restore.fallback_message;
        }
    }

    if selected_model.is_none() {
        if let (Some(default_provider), Some(default_model)) = (
            config.default_provider.as_deref(),
            config.default_model.as_deref(),
        ) {
            if let Some(found) = registry.find(default_provider, default_model) {
                selected_model = Some(found);
            }
        }
    }

    if selected_model.is_none() {
        let available = registry.get_available();
        if !available.is_empty() {
            selected_model = Some(default_model_from_available(&available));
        }
    }

    let Some(model_entry) = selected_model else {
        let models_path = default_models_path(global_dir);
        return Err(StartupError::NoModelsAvailable { models_path }.into());
    };

    let mut thinking_level: Option<model::ThinkingLevel> = None;

    if let Some(cli_thinking) = cli.thinking.as_deref() {
        thinking_level = Some(parse_thinking_level(cli_thinking)?);
    } else if scoped_thinking.is_some() {
        thinking_level = scoped_thinking;
    } else if is_continuing {
        if let Some(saved) = last_thinking_level(session) {
            thinking_level = Some(saved);
        }
    }

    if thinking_level.is_none() {
        thinking_level = config
            .default_thinking_level
            .as_deref()
            .and_then(parse_thinking_level_opt);
    }

    let thinking_level =
        model_entry.clamp_thinking_level(thinking_level.unwrap_or(model::ThinkingLevel::Medium));

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
    for entry in session.entries.iter().rev() {
        if let crate::session::SessionEntry::ModelChange(change) = entry {
            return Some((change.provider.clone(), change.model_id.clone()));
        }
    }
    None
}

fn last_thinking_level(session: &Session) -> Option<model::ThinkingLevel> {
    for entry in session.entries.iter().rev() {
        if let crate::session::SessionEntry::ThinkingLevelChange(change) = entry {
            if let Some(level) = parse_thinking_level_opt(&change.thinking_level) {
                return Some(level);
            }
        }
    }
    None
}

pub fn update_session_for_selection(session: &mut Session, selection: &ModelSelection) {
    session.set_model_header(
        Some(selection.model_entry.model.provider.clone()),
        Some(selection.model_entry.model.id.clone()),
        Some(selection.thinking_level.to_string()),
    );

    let model_changed = match last_model_from_session(session) {
        Some((provider, model_id)) => {
            provider != selection.model_entry.model.provider
                || model_id != selection.model_entry.model.id
        }
        None => true,
    };

    if model_changed {
        session.append_model_change(
            selection.model_entry.model.provider.clone(),
            selection.model_entry.model.id.clone(),
        );
    }

    let thinking_changed = last_thinking_level(session) != Some(selection.thinking_level);

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
        };
    }

    RestoreResult {
        model: None,
        fallback_message: None,
    }
}

fn default_model_from_available(available: &[ModelEntry]) -> ModelEntry {
    let defaults = [
        ("anthropic", "claude-opus-4-5"),
        ("openai", "gpt-5.1-codex"),
        ("google", "gemini-2.5-pro"),
    ];

    for (provider, model_id) in defaults {
        if let Some(found) = available
            .iter()
            .find(|m| m.model.provider == provider && m.model.id == model_id)
        {
            return found.clone();
        }
    }

    available[0].clone()
}

pub fn resolve_api_key(auth: &AuthStorage, cli: &cli::Cli, entry: &ModelEntry) -> Result<String> {
    auth.resolve_api_key(&entry.model.provider, cli.api_key.as_deref())
        .or_else(|| entry.api_key.clone())
        .ok_or_else(|| {
            StartupError::MissingApiKey {
                provider: entry.model.provider.clone(),
            }
            .into()
        })
}

pub fn build_stream_options(
    config: &Config,
    api_key: String,
    selection: &ModelSelection,
    session: &Session,
) -> StreamOptions {
    let mut options = StreamOptions {
        api_key: Some(api_key),
        headers: selection.model_entry.headers.clone(),
        session_id: Some(session.header.id.clone()),
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
            if let Some((prefix, suffix)) = pattern.rsplit_once(':') {
                if let Some(parsed) = parse_thinking_level_opt(suffix) {
                    thinking_level = Some(parsed);
                    glob_pattern = prefix;
                }
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

    if let Some(thinking_level) = parse_thinking_level_opt(suffix) {
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
        return result;
    }

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
    if let Some((provider, model_id)) = pattern.split_once('/') {
        if let Some(found) = available_models.iter().find(|m| {
            m.model.provider.eq_ignore_ascii_case(provider)
                && m.model.id.eq_ignore_ascii_case(model_id)
        }) {
            return Some(found.clone());
        }
    }

    if let Some(found) = available_models
        .iter()
        .find(|m| m.model.id.eq_ignore_ascii_case(pattern))
    {
        return Some(found.clone());
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

    let Some((_, date_suffix)) = model_id.rsplit_once('-') else {
        return true;
    };

    date_suffix.len() != 8 || !date_suffix.chars().all(|c| c.is_ascii_digit())
}

fn models_equal(left: &ModelEntry, right: &ModelEntry) -> bool {
    left.model.provider == right.model.provider && left.model.id == right.model.id
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

    use super::*;
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

    #[test]
    fn parse_models_arg_splits_and_trims() {
        assert_eq!(
            parse_models_arg("gpt-4*, claude* ,,"),
            vec!["gpt-4*".to_string(), "claude*".to_string()]
        );
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
}
