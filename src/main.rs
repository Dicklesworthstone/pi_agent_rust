//! Pi - High-performance AI coding agent CLI
//!
//! Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance: Sub-100ms startup, smooth TUI at 60fps
//! - Reliability: No panics in normal operation
//! - Efficiency: Single binary, minimal dependencies

#![forbid(unsafe_code)]
// Allow dead code and unused async during scaffolding phase - remove once implementation is complete
#![allow(dead_code, clippy::unused_async)]

use std::collections::HashSet;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{Datelike, Local};
use clap::Parser;
use glob::Pattern;
use pi::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession};
use pi::auth::AuthStorage;
use pi::cli;
use pi::config::Config;
use pi::model;
use pi::model::{AssistantMessage, ContentBlock, ImageContent, StopReason, TextContent};
use pi::models::{ModelEntry, ModelRegistry, default_models_path};
use pi::package_manager::{PackageEntry, PackageManager, PackageScope};
use pi::provider::{InputType, StreamOptions, ThinkingBudgets};
use pi::providers;
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::tools::{ToolRegistry, process_file_arguments};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    // Parse CLI arguments
    let cli = cli::Cli::parse();

    // Run the application
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

#[allow(clippy::too_many_lines)]
async fn run(mut cli: cli::Cli) -> Result<()> {
    if cli.version {
        print_version();
        return Ok(());
    }

    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if let Some(command) = cli.command.take() {
        handle_subcommand(command, &cwd).await?;
        return Ok(());
    }

    let config = Config::load()?;
    let package_manager = PackageManager::new(cwd.clone());
    let resource_cli = ResourceCliOptions {
        no_skills: cli.no_skills,
        no_prompt_templates: cli.no_prompt_templates,
        no_extensions: cli.no_extensions,
        no_themes: cli.no_themes,
        skill_paths: cli.skill.clone(),
        prompt_paths: cli.prompt_template.clone(),
        extension_paths: cli.extension.clone(),
        theme_paths: cli.theme.clone(),
    };
    let resources = match ResourceLoader::load(&package_manager, &cwd, &config, &resource_cli).await
    {
        Ok(resources) => resources,
        Err(err) => {
            eprintln!("Warning: Failed to load skills/prompts: {err}");
            ResourceLoader::empty(config.enable_skill_commands())
        }
    };
    let mut auth = AuthStorage::load(Config::auth_path())?;
    auth.refresh_expired_oauth_tokens().await?;
    let models_path = default_models_path(&Config::global_dir());
    let model_registry = ModelRegistry::load(&auth, Some(models_path));
    if let Some(error) = model_registry.error() {
        eprintln!("Warning: models.json error: {error}");
    }

    if let Some(pattern) = &cli.list_models {
        list_models(&model_registry, pattern.as_deref());
        return Ok(());
    }

    if cli.mode.as_deref() != Some("rpc") {
        if let Some(stdin_content) = read_piped_stdin()? {
            cli.print = true;
            cli.args.insert(0, stdin_content);
        }
    }

    if let Some(export_path) = cli.export.clone() {
        let output = cli.message_args().first().map(ToString::to_string);
        let output_path = export_session(&export_path, output.as_deref()).await?;
        println!("Exported to: {}", output_path.display());
        return Ok(());
    }

    if cli.mode.as_deref() == Some("rpc") && !cli.file_args().is_empty() {
        bail!("Error: @file arguments are not supported in RPC mode");
    }

    let mut messages: Vec<String> = cli.message_args().iter().map(ToString::to_string).collect();
    let file_args: Vec<String> = cli.file_args().iter().map(ToString::to_string).collect();
    let initial = prepare_initial_message(
        &cwd,
        &file_args,
        &mut messages,
        config
            .images
            .as_ref()
            .and_then(|i| i.auto_resize)
            .unwrap_or(true),
    )?;

    let is_interactive = !cli.print && cli.mode.is_none();
    let mode = cli.mode.clone().unwrap_or_else(|| "text".to_string());
    let enabled_tools = cli.enabled_tools();

    let scoped_patterns = if let Some(models_arg) = &cli.models {
        parse_models_arg(models_arg)
    } else {
        config.enabled_models.clone().unwrap_or_default()
    };
    let scoped_models = if scoped_patterns.is_empty() {
        Vec::new()
    } else {
        resolve_model_scope(&scoped_patterns, &model_registry, cli.api_key.is_some())
    };

    if cli.api_key.is_some()
        && cli.provider.is_none()
        && cli.model.is_none()
        && scoped_models.is_empty()
    {
        bail!("--api-key requires a model to be specified via --provider/--model or --models");
    }

    let mut session = Session::new(&cli, &config).await?;

    let selection =
        select_model_and_thinking(&cli, &config, &session, &model_registry, &scoped_models)?;

    update_session_for_selection(&mut session, &selection);

    if let Some(message) = &selection.fallback_message {
        eprintln!("Warning: {message}");
    }

    let api_key = resolve_api_key(&auth, &cli, &selection.model_entry)?;

    let skills_prompt = if enabled_tools.contains(&"read") {
        resources.format_skills_for_prompt()
    } else {
        String::new()
    };
    let system_prompt = build_system_prompt(
        &cli,
        &cwd,
        &enabled_tools,
        if skills_prompt.is_empty() {
            None
        } else {
            Some(skills_prompt.as_str())
        },
    );
    let provider =
        providers::create_provider(&selection.model_entry).map_err(anyhow::Error::new)?;
    let stream_options = build_stream_options(&config, api_key, &selection, &session);
    let agent_config = AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations: 50,
        stream_options,
    };

    let tools = ToolRegistry::new(&enabled_tools, &cwd, Some(&config));
    let mut agent_session = AgentSession::new(
        Agent::new(provider, tools, agent_config),
        session,
        !cli.no_session,
    );

    let history = agent_session.session.to_messages_for_current_path();
    if !history.is_empty() {
        agent_session.agent.replace_messages(history);
    }

    if mode == "rpc" {
        let available_models = model_registry.get_available();
        let rpc_scoped_models = selection
            .scoped_models
            .iter()
            .map(|sm| pi::rpc::RpcScopedModel {
                model: sm.model.clone(),
                thinking_level: sm.thinking_level,
            })
            .collect::<Vec<_>>();
        return run_rpc_mode(
            agent_session,
            resources,
            config.clone(),
            available_models,
            rpc_scoped_models,
            auth.clone(),
        )
        .await;
    }

    if is_interactive {
        let model_scope = selection
            .scoped_models
            .iter()
            .map(|sm| sm.model.clone())
            .collect::<Vec<_>>();
        let available_models = model_registry.get_available();

        return run_interactive_mode(
            agent_session,
            initial,
            messages,
            config.clone(),
            selection.model_entry.clone(),
            model_scope,
            available_models,
            !cli.no_session,
            resources,
            resource_cli,
            cwd.clone(),
        )
        .await;
    }

    run_print_mode(&mut agent_session, &mode, initial, messages, &resources).await
}

async fn handle_subcommand(command: cli::Commands, cwd: &Path) -> Result<()> {
    let manager = PackageManager::new(cwd.to_path_buf());
    match command {
        cli::Commands::Install { source, local } => {
            handle_package_install(&manager, &source, local)?;
        }
        cli::Commands::Remove { source, local } => {
            handle_package_remove(&manager, &source, local)?;
        }
        cli::Commands::Update { source } => {
            handle_package_update(&manager, source)?;
        }
        cli::Commands::List => {
            handle_package_list(&manager)?;
        }
        cli::Commands::Config => {
            handle_config(cwd)?;
        }
    }

    Ok(())
}

const fn scope_from_flag(local: bool) -> PackageScope {
    if local {
        PackageScope::Project
    } else {
        PackageScope::User
    }
}

fn handle_package_install(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    manager.install(source, scope)?;
    manager.add_package_source(source, scope)?;
    println!("Installed {source}");
    Ok(())
}

fn handle_package_remove(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    manager.remove(source, scope)?;
    manager.remove_package_source(source, scope)?;
    println!("Removed {source}");
    Ok(())
}

fn handle_package_update(manager: &PackageManager, source: Option<String>) -> Result<()> {
    let entries = manager.list_packages()?;

    if let Some(source) = source {
        let identity = manager.package_identity(&source);
        for entry in entries {
            if manager.package_identity(&entry.source) != identity {
                continue;
            }
            manager.update_source(&entry.source, entry.scope)?;
        }
        println!("Updated {source}");
        return Ok(());
    }

    for entry in entries {
        manager.update_source(&entry.source, entry.scope)?;
    }
    println!("Updated packages");
    Ok(())
}

fn handle_package_list(manager: &PackageManager) -> Result<()> {
    let entries = manager.list_packages()?;

    let mut user = Vec::new();
    let mut project = Vec::new();
    for entry in entries {
        match entry.scope {
            PackageScope::User => user.push(entry),
            PackageScope::Project | PackageScope::Temporary => project.push(entry),
        }
    }

    if user.is_empty() && project.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }

    if !user.is_empty() {
        println!("User packages:");
        for entry in &user {
            print_package_entry(manager, entry)?;
        }
    }

    if !project.is_empty() {
        if !user.is_empty() {
            println!();
        }
        println!("Project packages:");
        for entry in &project {
            print_package_entry(manager, entry)?;
        }
    }

    Ok(())
}

fn print_package_entry(manager: &PackageManager, entry: &PackageEntry) -> Result<()> {
    let display = if entry.filter.is_some() {
        format!("{} (filtered)", entry.source)
    } else {
        entry.source.clone()
    };
    println!("  {display}");
    if let Some(path) = manager.installed_path(&entry.source, entry.scope)? {
        println!("    {}", path.display());
    }
    Ok(())
}

fn handle_config(cwd: &Path) -> Result<()> {
    let _ = Config::load()?;
    let config_path = std::env::var("PI_CONFIG_PATH")
        .ok()
        .map_or_else(|| Config::global_dir().join("settings.json"), PathBuf::from);
    let project_path = cwd.join(Config::project_dir()).join("settings.json");

    println!("Settings paths:");
    println!("  Global:  {}", config_path.display());
    println!("  Project: {}", project_path.display());
    println!();
    println!("Other paths:");
    println!("  Auth:     {}", Config::auth_path().display());
    println!("  Sessions: {}", Config::sessions_dir().display());
    println!();
    println!("Settings precedence:");
    println!("  1) CLI flags");
    println!("  2) Environment variables");
    println!("  3) Project settings ({})", project_path.display());
    println!("  4) Global settings ({})", config_path.display());
    println!("  5) Built-in defaults");

    Ok(())
}

fn print_version() {
    println!(
        "pi {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
    );
}

fn list_models(registry: &ModelRegistry, pattern: Option<&str>) {
    let mut models = registry.get_available();
    if models.is_empty() {
        println!("No models available. Set API keys in environment variables.");
        return;
    }

    if let Some(pattern) = pattern {
        models = filter_models_by_pattern(models, pattern);
        if models.is_empty() {
            println!("No models matching \"{pattern}\"");
            return;
        }
    }

    models.sort_by(|a, b| {
        let provider_cmp = a.model.provider.cmp(&b.model.provider);
        if provider_cmp == std::cmp::Ordering::Equal {
            a.model.id.cmp(&b.model.id)
        } else {
            provider_cmp
        }
    });

    let rows = build_model_rows(&models);
    print_model_table(&rows);
}

fn filter_models_by_pattern(models: Vec<ModelEntry>, pattern: &str) -> Vec<ModelEntry> {
    models
        .into_iter()
        .filter(|entry| {
            fuzzy_match(
                pattern,
                &format!("{} {}", entry.model.provider, entry.model.id),
            )
        })
        .collect()
}

fn build_model_rows(
    models: &[ModelEntry],
) -> Vec<(String, String, String, String, String, String)> {
    models
        .iter()
        .map(|entry| {
            let provider = entry.model.provider.clone();
            let model = entry.model.id.clone();
            let context = format_token_count(entry.model.context_window);
            let max_out = format_token_count(entry.model.max_tokens);
            let thinking = if entry.model.reasoning { "yes" } else { "no" }.to_string();
            let images = if entry.model.input.contains(&InputType::Image) {
                "yes"
            } else {
                "no"
            }
            .to_string();
            (provider, model, context, max_out, thinking, images)
        })
        .collect()
}

fn print_model_table(rows: &[(String, String, String, String, String, String)]) {
    let headers = (
        "provider", "model", "context", "max-out", "thinking", "images",
    );

    let provider_w = rows
        .iter()
        .map(|r| r.0.len())
        .max()
        .unwrap_or(0)
        .max(headers.0.len());
    let model_w = rows
        .iter()
        .map(|r| r.1.len())
        .max()
        .unwrap_or(0)
        .max(headers.1.len());
    let context_w = rows
        .iter()
        .map(|r| r.2.len())
        .max()
        .unwrap_or(0)
        .max(headers.2.len());
    let max_out_w = rows
        .iter()
        .map(|r| r.3.len())
        .max()
        .unwrap_or(0)
        .max(headers.3.len());
    let thinking_w = rows
        .iter()
        .map(|r| r.4.len())
        .max()
        .unwrap_or(0)
        .max(headers.4.len());
    let images_w = rows
        .iter()
        .map(|r| r.5.len())
        .max()
        .unwrap_or(0)
        .max(headers.5.len());

    let (provider, model, context, max_out, thinking, images) = headers;
    println!(
        "{provider:<provider_w$}  {model:<model_w$}  {context:<context_w$}  {max_out:<max_out_w$}  {thinking:<thinking_w$}  {images:<images_w$}"
    );

    for (provider, model, context, max_out, thinking, images) in rows {
        println!(
            "{provider:<provider_w$}  {model:<model_w$}  {context:<context_w$}  {max_out:<max_out_w$}  {thinking:<thinking_w$}  {images:<images_w$}"
        );
    }
}

async fn export_session(input_path: &str, output_path: Option<&str>) -> Result<PathBuf> {
    let input = Path::new(input_path);
    if !input.exists() {
        bail!("File not found: {input_path}");
    }

    let session = Session::open(input_path).await?;
    let html = render_session_html(&session);
    let output_path = output_path.map_or_else(|| default_export_path(input), PathBuf::from);

    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&output_path, html)?;
    Ok(output_path)
}

async fn run_rpc_mode(
    session: AgentSession,
    resources: ResourceLoader,
    config: Config,
    available_models: Vec<ModelEntry>,
    scoped_models: Vec<pi::rpc::RpcScopedModel>,
    auth: AuthStorage,
) -> Result<()> {
    pi::rpc::run_stdio(
        session,
        pi::rpc::RpcOptions {
            config,
            resources,
            available_models,
            scoped_models,
            auth,
        },
    )
    .await
    .map_err(anyhow::Error::new)
}

async fn run_print_mode(
    session: &mut AgentSession,
    mode: &str,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    resources: &ResourceLoader,
) -> Result<()> {
    if mode != "text" && mode != "json" {
        bail!("Unknown mode: {mode}");
    }
    if initial.is_none() && messages.is_empty() {
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    if mode == "json" {
        println!("{}", serde_json::to_string(&session.session.header)?);
    }

    let mut last_message: Option<AssistantMessage> = None;
    let emit_json_events = mode == "json";
    let event_handler = move |event: AgentEvent| {
        if emit_json_events {
            if let Ok(serialized) = serde_json::to_string(&event) {
                println!("{serialized}");
            }
        }
    };
    let (abort_handle, abort_signal) = AbortHandle::new();
    let abort_listener = abort_handle.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        abort_listener.abort();
    });

    let mut initial = initial;
    if let Some(ref mut initial) = initial {
        initial.text = resources.expand_input(&initial.text);
    }

    let messages = messages
        .into_iter()
        .map(|message| resources.expand_input(&message))
        .collect::<Vec<_>>();

    if let Some(initial) = initial {
        let content = build_initial_content(&initial);
        last_message = Some(
            session
                .run_with_content_with_abort(content, Some(abort_signal.clone()), event_handler)
                .await?,
        );
    }

    for message in messages {
        last_message = Some(
            session
                .run_text_with_abort(message, Some(abort_signal.clone()), event_handler)
                .await?,
        );
    }

    let Some(last_message) = last_message else {
        bail!("No messages were sent");
    };

    if matches!(
        last_message.stop_reason,
        StopReason::Error | StopReason::Aborted
    ) {
        let message = last_message
            .error_message
            .unwrap_or_else(|| "Request error".to_string());
        bail!(message);
    }

    if mode == "text" {
        output_final_text(&last_message);
    }

    io::stdout().flush()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_interactive_mode(
    session: AgentSession,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    config: Config,
    model_entry: ModelEntry,
    model_scope: Vec<ModelEntry>,
    available_models: Vec<ModelEntry>,
    save_enabled: bool,
    resources: ResourceLoader,
    resource_cli: ResourceCliOptions,
    cwd: PathBuf,
) -> Result<()> {
    let mut pending = Vec::new();
    if let Some(initial) = initial {
        pending.push(pi::interactive::PendingInput::Content(
            build_initial_content(&initial),
        ));
    }
    for message in messages {
        pending.push(pi::interactive::PendingInput::Text(message));
    }

    let AgentSession { agent, session, .. } = session;
    pi::interactive::run_interactive(
        agent,
        session,
        config,
        model_entry,
        model_scope,
        available_models,
        pending,
        save_enabled,
        resources,
        resource_cli,
        cwd,
    )
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
struct InitialMessage {
    text: String,
    images: Vec<ImageContent>,
}

#[derive(Debug, Clone)]
struct ScopedModel {
    model: ModelEntry,
    thinking_level: Option<model::ThinkingLevel>,
}

#[derive(Debug, Clone)]
struct ParsedModelResult {
    model: Option<ModelEntry>,
    thinking_level: Option<model::ThinkingLevel>,
    warning: Option<String>,
}

#[derive(Debug, Clone)]
struct ModelSelection {
    model_entry: ModelEntry,
    thinking_level: model::ThinkingLevel,
    scoped_models: Vec<ScopedModel>,
    fallback_message: Option<String>,
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

fn read_piped_stdin() -> Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut data = String::new();
    io::stdin().read_to_string(&mut data)?;
    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(data))
    }
}

fn prepare_initial_message(
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

fn build_initial_content(initial: &InitialMessage) -> Vec<ContentBlock> {
    let mut content = Vec::new();
    content.push(ContentBlock::Text(TextContent::new(initial.text.clone())));
    for image in &initial.images {
        content.push(ContentBlock::Image(image.clone()));
    }
    content
}

fn build_system_prompt(
    cli: &cli::Cli,
    cwd: &Path,
    enabled_tools: &[&str],
    skills_prompt: Option<&str>,
) -> String {
    use std::fmt::Write as _;

    let custom_prompt = resolve_prompt_input(cli.system_prompt.as_deref(), "system prompt");
    let append_prompt =
        resolve_prompt_input(cli.append_system_prompt.as_deref(), "append system prompt");
    let context_files = load_project_context_files(cwd);

    let mut prompt = custom_prompt.unwrap_or_else(|| default_system_prompt(enabled_tools));

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

    let date_time = format_current_datetime();
    let _ = write!(prompt, "\nCurrent date and time: {date_time}");
    let _ = write!(prompt, "\nCurrent working directory: {}", cwd.display());

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

fn default_system_prompt(enabled_tools: &[&str]) -> String {
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

    let readme_path = Config::package_dir()
        .join("README.md")
        .display()
        .to_string();
    let docs_path = Config::package_dir().join("docs").display().to_string();
    let examples_path = Config::package_dir().join("examples").display().to_string();

    format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.\n\nAvailable tools:\n{tools_list}\n\nIn addition to the tools above, you may have access to other custom tools depending on the project.\n\nGuidelines:\n{guidelines}\n\nPi documentation (read only when the user asks about pi itself, its SDK, extensions, themes, skills, or TUI):\n- Main documentation: {readme_path}\n- Additional docs: {docs_path}\n- Examples: {examples_path} (extensions, custom tools, SDK)\n- When asked about: extensions (docs/extensions.md, examples/extensions/), themes (docs/themes.md), skills (docs/skills.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), pi packages (docs/packages.md)\n- When working on pi topics, read the docs and examples, and follow .md cross-references before implementing\n- Always read pi .md files completely and follow links to related docs (e.g., tui.md for TUI API details)"
    )
}

fn load_project_context_files(cwd: &Path) -> Vec<ContextFile> {
    let mut context_files = Vec::new();
    let mut seen = HashSet::new();

    if let Some(global) = load_context_file_from_dir(&Config::global_dir()) {
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

fn format_token_count(count: u32) -> String {
    if count >= 1_000_000 {
        let millions = f64::from(count) / 1_000_000.0;
        if millions.fract() == 0.0 {
            format!("{millions:.0}M")
        } else {
            format!("{millions:.1}M")
        }
    } else if count >= 1_000 {
        let thousands = f64::from(count) / 1_000.0;
        if thousands.fract() == 0.0 {
            format!("{thousands:.0}K")
        } else {
            format!("{thousands:.1}K")
        }
    } else {
        count.to_string()
    }
}

fn fuzzy_match(pattern: &str, value: &str) -> bool {
    let needle_str = pattern.to_lowercase();
    let haystack_str = value.to_lowercase();
    let mut needle = needle_str.chars().filter(|c| !c.is_whitespace());
    let mut haystack = haystack_str.chars();
    for ch in needle.by_ref() {
        if !haystack.by_ref().any(|h| h == ch) {
            return false;
        }
    }
    true
}

fn parse_models_arg(models: &str) -> Vec<String> {
    models
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn resolve_model_scope(
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
    if model_id.len() < 9 {
        return true;
    }
    let suffix = &model_id[model_id.len() - 9..];
    if !suffix.starts_with('-') {
        return true;
    }
    !suffix[1..].chars().all(|c| c.is_ascii_digit())
}

fn parse_thinking_level(value: &str) -> Result<model::ThinkingLevel> {
    value
        .parse()
        .map_err(|err| anyhow::anyhow!("Invalid thinking level \"{value}\": {err}"))
}

fn parse_thinking_level_opt(value: &str) -> Option<model::ThinkingLevel> {
    value.parse().ok()
}

#[allow(clippy::too_many_lines)]
fn select_model_and_thinking(
    cli: &cli::Cli,
    config: &Config,
    session: &Session,
    registry: &ModelRegistry,
    scoped_models: &[ScopedModel],
) -> Result<ModelSelection> {
    let is_continuing = cli.r#continue || cli.resume || cli.session.is_some();
    let mut selected_model: Option<ModelEntry> = None;
    let mut scoped_thinking: Option<model::ThinkingLevel> = None;
    let mut fallback_message = None;

    if let (Some(provider), Some(model_id)) = (cli.provider.as_deref(), cli.model.as_deref()) {
        let found = registry.find(provider, model_id);
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
        let models_path = default_models_path(&Config::global_dir());
        bail!(
            "No models available. Set API keys in environment variables or create {}",
            models_path.display()
        );
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

    let thinking_level = clamp_thinking_level(
        thinking_level.unwrap_or(model::ThinkingLevel::Medium),
        &model_entry,
    );

    Ok(ModelSelection {
        model_entry,
        thinking_level,
        scoped_models: scoped_models.to_vec(),
        fallback_message,
    })
}

fn last_model_from_session(session: &Session) -> Option<(String, String)> {
    for entry in session.entries.iter().rev() {
        if let pi::session::SessionEntry::ModelChange(change) = entry {
            return Some((change.provider.clone(), change.model_id.clone()));
        }
    }
    None
}

fn last_thinking_level(session: &Session) -> Option<model::ThinkingLevel> {
    for entry in session.entries.iter().rev() {
        if let pi::session::SessionEntry::ThinkingLevelChange(change) = entry {
            if let Some(level) = parse_thinking_level_opt(&change.thinking_level) {
                return Some(level);
            }
        }
    }
    None
}

fn restore_model_from_session(
    saved_provider: &str,
    saved_model_id: &str,
    current_model: Option<ModelEntry>,
    registry: &ModelRegistry,
) -> RestoreResult {
    let restored = registry.find(saved_provider, saved_model_id);
    let has_api_key = restored.as_ref().and_then(|m| m.api_key.clone()).is_some();

    if restored.is_some() && has_api_key {
        return RestoreResult {
            model: restored,
            fallback_message: None,
        };
    }

    let reason = if restored.is_none() {
        "model no longer exists"
    } else {
        "no API key available"
    };

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

fn models_equal(a: &ModelEntry, b: &ModelEntry) -> bool {
    a.model.provider == b.model.provider && a.model.id == b.model.id
}

fn update_session_for_selection(session: &mut Session, selection: &ModelSelection) {
    session.set_model_header(
        Some(selection.model_entry.model.provider.clone()),
        Some(selection.model_entry.model.id.clone()),
        Some(thinking_level_to_str(selection.thinking_level).to_string()),
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

    let thinking = thinking_level_to_str(selection.thinking_level).to_string();
    let thinking_changed = last_thinking_level(session)
        .is_none_or(|level| thinking_level_to_str(level) != thinking.as_str());

    if thinking_changed {
        session.append_thinking_level_change(thinking);
    }
}

fn resolve_api_key(auth: &AuthStorage, cli: &cli::Cli, entry: &ModelEntry) -> Result<String> {
    auth.resolve_api_key(&entry.model.provider, cli.api_key.as_deref())
        .or_else(|| entry.api_key.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No API key found for provider {}. Set env var or use --api-key.",
                entry.model.provider
            )
        })
}

fn build_stream_options(
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

fn supports_xhigh(model_id: &str) -> bool {
    matches!(model_id, "gpt-5.1-codex-max" | "gpt-5.2" | "gpt-5.2-codex")
}

fn clamp_thinking_level(
    thinking: model::ThinkingLevel,
    model_entry: &ModelEntry,
) -> model::ThinkingLevel {
    if !model_entry.model.reasoning {
        return model::ThinkingLevel::Off;
    }
    if thinking == model::ThinkingLevel::XHigh && !supports_xhigh(&model_entry.model.id) {
        return model::ThinkingLevel::High;
    }
    thinking
}

const fn thinking_level_to_str(level: model::ThinkingLevel) -> &'static str {
    match level {
        model::ThinkingLevel::Off => "off",
        model::ThinkingLevel::Minimal => "minimal",
        model::ThinkingLevel::Low => "low",
        model::ThinkingLevel::Medium => "medium",
        model::ThinkingLevel::High => "high",
        model::ThinkingLevel::XHigh => "xhigh",
    }
}

fn output_final_text(message: &AssistantMessage) {
    for block in &message.content {
        if let ContentBlock::Text(text) = block {
            println!("{}", text.text);
        }
    }
}

fn default_export_path(input: &Path) -> PathBuf {
    let basename = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    PathBuf::from(format!("pi-session-{basename}.html"))
}

fn render_session_html(session: &Session) -> String {
    session.to_html()
}
