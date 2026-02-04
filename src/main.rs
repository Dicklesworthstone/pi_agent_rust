//! Pi - High-performance AI coding agent CLI
//!
//! Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance: Sub-100ms startup, smooth TUI at 60fps
//! - Reliability: No panics in normal operation
//! - Efficiency: Single binary, minimal dependencies

#![forbid(unsafe_code)]
// Allow dead code and unused async during scaffolding phase - remove once implementation is complete
#![allow(dead_code, clippy::unused_async)]

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
use clap::Parser;
use pi::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession};
use pi::app::StartupError;
use pi::auth::{AuthCredential, AuthStorage};
use pi::cli;
use pi::config::Config;
use pi::model::{AssistantMessage, StopReason};
use pi::models::{ModelEntry, ModelRegistry, default_models_path};
use pi::package_manager::{PackageEntry, PackageManager, PackageScope};
use pi::provider::InputType;
use pi::providers;
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::session_index::SessionIndex;
use pi::tools::ToolRegistry;
use pi::tui::PiConsole;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .with_writer(io::stderr)
        .init();

    // Parse CLI arguments
    let cli = cli::Cli::parse();

    // Run the application
    let reactor = create_reactor()?;
    let runtime = RuntimeBuilder::multi_thread()
        .blocking_threads(1, 8)
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let handle = runtime.handle();
    let runtime_handle = handle.clone();
    let join = handle.spawn(Box::pin(run(cli, runtime_handle)));
    runtime.block_on(join)
}

#[allow(clippy::too_many_lines)]
async fn run(mut cli: cli::Cli, runtime_handle: RuntimeHandle) -> Result<()> {
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
    spawn_session_index_maintenance();
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
    let mut auth = AuthStorage::load_async(Config::auth_path()).await?;
    auth.refresh_expired_oauth_tokens().await?;
    let global_dir = Config::global_dir();
    let package_dir = Config::package_dir();
    let models_path = default_models_path(&global_dir);
    let mut model_registry = ModelRegistry::load(&auth, Some(models_path.clone()));
    if let Some(error) = model_registry.error() {
        eprintln!("Warning: models.json error: {error}");
    }

    if let Some(pattern) = &cli.list_models {
        list_models(&model_registry, pattern.as_deref());
        return Ok(());
    }

    if cli.mode.as_deref() != Some("rpc") {
        let stdin_content = read_piped_stdin()?;
        pi::app::apply_piped_stdin(&mut cli, stdin_content);
    }
    pi::app::normalize_cli(&mut cli);

    if let Some(export_path) = cli.export.clone() {
        let output = cli.message_args().first().map(ToString::to_string);
        let output_path = export_session(&export_path, output.as_deref()).await?;
        println!("Exported to: {}", output_path.display());
        return Ok(());
    }

    pi::app::validate_rpc_args(&cli)?;

    let mut messages: Vec<String> = cli.message_args().iter().map(ToString::to_string).collect();
    let file_args: Vec<String> = cli.file_args().iter().map(ToString::to_string).collect();
    let initial = pi::app::prepare_initial_message(
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

    let scoped_patterns = if let Some(models_arg) = &cli.models {
        pi::app::parse_models_arg(models_arg)
    } else {
        config.enabled_models.clone().unwrap_or_default()
    };
    let mut scoped_models = if scoped_patterns.is_empty() {
        Vec::new()
    } else {
        pi::app::resolve_model_scope(&scoped_patterns, &model_registry, cli.api_key.is_some())
    };

    if cli.api_key.is_some()
        && cli.provider.is_none()
        && cli.model.is_none()
        && scoped_models.is_empty()
    {
        bail!("--api-key requires a model to be specified via --provider/--model or --models");
    }

    let mut session = Box::pin(Session::new(&cli, &config)).await?;

    let (selection, resolved_key) = loop {
        scoped_models = if scoped_patterns.is_empty() {
            Vec::new()
        } else {
            pi::app::resolve_model_scope(&scoped_patterns, &model_registry, cli.api_key.is_some())
        };

        let selection = match pi::app::select_model_and_thinking(
            &cli,
            &config,
            &session,
            &model_registry,
            &scoped_models,
            &global_dir,
        ) {
            Ok(selection) => selection,
            Err(err) => {
                if let Some(startup) = err.downcast_ref::<StartupError>() {
                    if is_interactive && io::stdin().is_terminal() && io::stdout().is_terminal() {
                        if run_first_time_setup(startup, &mut auth, &mut cli, &models_path).await? {
                            model_registry = ModelRegistry::load(&auth, Some(models_path.clone()));
                            if let Some(error) = model_registry.error() {
                                eprintln!("Warning: models.json error: {error}");
                            }
                            continue;
                        }
                        return Ok(());
                    }
                }
                return Err(err);
            }
        };

        match pi::app::resolve_api_key(&auth, &cli, &selection.model_entry) {
            Ok(key) => break (selection, key),
            Err(err) => {
                if let Some(startup) = err.downcast_ref::<StartupError>() {
                    if is_interactive && io::stdin().is_terminal() && io::stdout().is_terminal() {
                        if run_first_time_setup(startup, &mut auth, &mut cli, &models_path).await? {
                            model_registry = ModelRegistry::load(&auth, Some(models_path.clone()));
                            if let Some(error) = model_registry.error() {
                                eprintln!("Warning: models.json error: {error}");
                            }
                            continue;
                        }
                        return Ok(());
                    }
                }
                return Err(err);
            }
        }
    };

    pi::app::update_session_for_selection(&mut session, &selection);

    if let Some(message) = &selection.fallback_message {
        eprintln!("Warning: {message}");
    }

    let enabled_tools = cli.enabled_tools();
    let skills_prompt = if enabled_tools.contains(&"read") {
        resources.format_skills_for_prompt()
    } else {
        String::new()
    };
    let system_prompt = pi::app::build_system_prompt(
        &cli,
        &cwd,
        &enabled_tools,
        if skills_prompt.is_empty() {
            None
        } else {
            Some(skills_prompt.as_str())
        },
        &global_dir,
        &package_dir,
    );
    let provider =
        providers::create_provider(&selection.model_entry).map_err(anyhow::Error::new)?;
    let stream_options = pi::app::build_stream_options(&config, resolved_key, &selection, &session);
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
            runtime_handle.clone(),
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
            runtime_handle.clone(),
        )
        .await;
    }

    run_print_mode(&mut agent_session, &mode, initial, messages, &resources).await
}

async fn handle_subcommand(command: cli::Commands, cwd: &Path) -> Result<()> {
    let manager = PackageManager::new(cwd.to_path_buf());
    match command {
        cli::Commands::Install { source, local } => {
            handle_package_install(&manager, &source, local).await?;
        }
        cli::Commands::Remove { source, local } => {
            handle_package_remove(&manager, &source, local).await?;
        }
        cli::Commands::Update { source } => {
            handle_package_update(&manager, source).await?;
        }
        cli::Commands::List => {
            handle_package_list(&manager).await?;
        }
        cli::Commands::Config => {
            handle_config(cwd)?;
        }
    }

    Ok(())
}

fn spawn_session_index_maintenance() {
    const MAX_INDEX_AGE: Duration = Duration::from_secs(60 * 30);
    let index = SessionIndex::new();
    if !index.should_reindex(MAX_INDEX_AGE) {
        return;
    }
    std::thread::spawn(move || {
        if let Err(err) = index.reindex_all() {
            eprintln!("Warning: failed to reindex session index: {err}");
        }
    });
}

const fn scope_from_flag(local: bool) -> PackageScope {
    if local {
        PackageScope::Project
    } else {
        PackageScope::User
    }
}

async fn handle_package_install(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    manager.install(source, scope).await?;
    manager.add_package_source(source, scope).await?;
    println!("Installed {source}");
    Ok(())
}

async fn handle_package_remove(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    manager.remove(source, scope).await?;
    manager.remove_package_source(source, scope).await?;
    println!("Removed {source}");
    Ok(())
}

async fn handle_package_update(manager: &PackageManager, source: Option<String>) -> Result<()> {
    let entries = manager.list_packages().await?;

    if let Some(source) = source {
        let identity = manager.package_identity(&source);
        for entry in entries {
            if manager.package_identity(&entry.source) != identity {
                continue;
            }
            manager.update_source(&entry.source, entry.scope).await?;
        }
        println!("Updated {source}");
        return Ok(());
    }

    for entry in entries {
        manager.update_source(&entry.source, entry.scope).await?;
    }
    println!("Updated packages");
    Ok(())
}

async fn handle_package_list(manager: &PackageManager) -> Result<()> {
    let entries = manager.list_packages().await?;

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
            print_package_entry(manager, entry).await?;
        }
    }

    if !project.is_empty() {
        if !user.is_empty() {
            println!();
        }
        println!("Project packages:");
        for entry in &project {
            print_package_entry(manager, entry).await?;
        }
    }

    Ok(())
}

async fn print_package_entry(manager: &PackageManager, entry: &PackageEntry) -> Result<()> {
    let display = if entry.filter.is_some() {
        format!("{} (filtered)", entry.source)
    } else {
        entry.source.clone()
    };
    println!("  {display}");
    if let Some(path) = manager.installed_path(&entry.source, entry.scope).await? {
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

#[derive(Clone, Copy)]
struct ProviderChoice {
    id: &'static str,
    label: &'static str,
    env: &'static str,
}

const PROVIDER_CHOICES: [ProviderChoice; 3] = [
    ProviderChoice {
        id: "anthropic",
        label: "Anthropic (Claude)",
        env: "ANTHROPIC_API_KEY",
    },
    ProviderChoice {
        id: "openai",
        label: "OpenAI",
        env: "OPENAI_API_KEY",
    },
    ProviderChoice {
        id: "google",
        label: "Google Gemini",
        env: "GOOGLE_API_KEY",
    },
];

fn provider_from_token(token: &str) -> Option<ProviderChoice> {
    let normalized = token.trim().to_lowercase();
    match normalized.as_str() {
        "1" | "anthropic" | "claude" => Some(PROVIDER_CHOICES[0]),
        "2" | "openai" | "gpt" => Some(PROVIDER_CHOICES[1]),
        "3" | "google" | "gemini" => Some(PROVIDER_CHOICES[2]),
        _ => None,
    }
}

async fn run_first_time_setup(
    startup_error: &StartupError,
    auth: &mut AuthStorage,
    cli: &mut cli::Cli,
    models_path: &Path,
) -> Result<bool> {
    let console = PiConsole::new();

    console.render_rule(Some("Welcome to Pi"));
    match startup_error {
        StartupError::NoModelsAvailable { .. } => {
            console.print_markup("[bold]No models are configured yet.[/]\n");
        }
        StartupError::MissingApiKey { provider } => {
            console.print_markup(&format!(
                "[bold]Missing API key for provider:[/] {provider}\n"
            ));
        }
    }
    console.print_markup("Let’s add your first API key.\n\n");

    let provider_hint = match startup_error {
        StartupError::MissingApiKey { provider } => provider_from_token(provider),
        StartupError::NoModelsAvailable { .. } => None,
    };

    console.print_markup("[bold]Choose a provider:[/]\n");
    for (idx, provider) in PROVIDER_CHOICES.iter().enumerate() {
        let is_default = provider_hint.is_some_and(|hint| hint.id == provider.id);
        let default_marker = if is_default { " [dim](default)[/]" } else { "" };
        console.print_markup(&format!(
            "  [cyan]{})[/] {}  [dim]{}[/]{}\n",
            idx + 1,
            provider.label,
            provider.env,
            default_marker
        ));
    }
    console.print_markup("  [cyan]4)[/] Custom provider via models.json\n");
    console.print_markup("  [cyan]5)[/] Exit setup\n\n");

    let provider = loop {
        let prompt = provider_hint.map_or_else(
            || "Select 1-5: ".to_string(),
            |default_provider| format!("Select 1-5 (Enter for {}): ", default_provider.label),
        );
        let Some(input) = prompt_line(&prompt)? else {
            console.render_warning("Setup cancelled (no input).");
            return Ok(false);
        };
        let normalized = input.trim().to_lowercase();
        if normalized.is_empty() {
            if let Some(default_provider) = provider_hint {
                break default_provider;
            }
            continue;
        }
        if normalized == "4" || normalized == "custom" || normalized == "models" {
            console.render_info(&format!(
                "Create models.json at {} and restart Pi.",
                models_path.display()
            ));
            return Ok(false);
        }
        if normalized == "5" || normalized == "q" || normalized == "quit" || normalized == "exit" {
            console.render_warning("Setup cancelled.");
            return Ok(false);
        }
        if let Some(provider) = provider_from_token(&normalized) {
            break provider;
        }
        console.render_warning("Unrecognized choice. Please try again.");
    };

    console.print_markup("Paste your API key (input will be visible):\n");
    let Some(raw_key) = prompt_line("API key: ")? else {
        console.render_warning("Setup cancelled (no input).");
        return Ok(false);
    };
    let key = raw_key.trim();
    if key.is_empty() {
        console.render_warning("No API key entered. Setup cancelled.");
        return Ok(false);
    }

    auth.set(
        provider.id,
        AuthCredential::ApiKey {
            key: key.to_string(),
        },
    );
    auth.save_async().await?;

    if cli.provider.as_deref() != Some(provider.id) {
        cli.provider = Some(provider.id.to_string());
        cli.model = None;
    }

    console.render_success(&format!(
        "Saved {label} API key to {path}",
        label = provider.label,
        path = Config::auth_path().display()
    ));
    console.render_info("Continuing startup...");
    Ok(true)
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

fn prompt_line(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    let bytes = io::stdin().read_line(&mut input)?;
    if bytes == 0 {
        return Ok(None);
    }
    Ok(Some(input.trim().to_string()))
}

async fn export_session(input_path: &str, output_path: Option<&str>) -> Result<PathBuf> {
    let input = Path::new(input_path);
    if !input.exists() {
        bail!("File not found: {input_path}");
    }

    let session = Session::open(input_path).await?;
    let html = pi::app::render_session_html(&session);
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
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    pi::rpc::run_stdio(
        session,
        pi::rpc::RpcOptions {
            config,
            resources,
            available_models,
            scoped_models,
            auth,
            runtime_handle,
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
    if let Err(err) = ctrlc::set_handler(move || {
        abort_listener.abort();
    }) {
        eprintln!("Warning: Failed to install Ctrl+C handler: {err}");
    }

    let mut initial = initial;
    if let Some(ref mut initial) = initial {
        initial.text = resources.expand_input(&initial.text);
    }

    let messages = messages
        .into_iter()
        .map(|message| resources.expand_input(&message))
        .collect::<Vec<_>>();

    if let Some(initial) = initial {
        let content = pi::app::build_initial_content(&initial);
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
        pi::app::output_final_text(&last_message);
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
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    let mut pending = Vec::new();
    if let Some(initial) = initial {
        pending.push(pi::interactive::PendingInput::Content(
            pi::app::build_initial_content(&initial),
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
        runtime_handle,
    )
    .await?;
    Ok(())
}

type InitialMessage = pi::app::InitialMessage;

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

fn default_export_path(input: &Path) -> PathBuf {
    let basename = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    PathBuf::from(format!("pi-session-{basename}.html"))
}
