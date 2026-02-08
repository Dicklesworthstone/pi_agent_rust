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
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
use asupersync::sync::Mutex;
use clap::Parser;
use pi::agent::{AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession};
use pi::app::StartupError;
use pi::auth::{AuthCredential, AuthStorage};
use pi::cli;
use pi::compaction::ResolvedCompactionSettings;
use pi::config::Config;
use pi::extension_index::ExtensionIndexStore;
use pi::extensions::extension_event_from_agent;
use pi::model::{AssistantMessage, ContentBlock, StopReason};
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

fn main() {
    if let Err(err) = main_impl() {
        print_error_with_hints(&err);
        std::process::exit(1);
    }
}

fn main_impl() -> Result<()> {
    // Parse CLI arguments
    let cli = cli::Cli::parse();

    // Early-validate theme file paths so invalid paths error before --version.
    // Named themes (without .json, /, ~) are validated later after resource loading.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    validate_theme_path_spec(cli.theme.as_deref(), &cwd)?;

    if cli.version {
        print_version();
        return Ok(());
    }

    // Ultra-fast paths that don't need tracing or the async runtime.
    if matches!(cli.command, Some(cli::Commands::Config)) {
        handle_config(&cwd)?;
        return Ok(());
    }

    // List-models is an offline query; avoid loading resources or booting the runtime when possible.
    //
    // IMPORTANT: if extension compat scanning is enabled, or explicit CLI extensions are provided,
    // we must boot the normal startup path so the compat ledger can be emitted deterministically.
    if cli.command.is_none() {
        if let Some(pattern) = &cli.list_models {
            let compat_scan_enabled =
                std::env::var("PI_EXT_COMPAT_SCAN")
                    .ok()
                    .is_some_and(|value| {
                        matches!(
                            value.trim().to_ascii_lowercase().as_str(),
                            "1" | "true" | "yes" | "on"
                        )
                    });
            let has_cli_extensions = !cli.extension.is_empty();

            if !compat_scan_enabled && !has_cli_extensions {
                // Note: we intentionally skip OAuth refresh here to keep this path fast and offline.
                let auth = AuthStorage::load(Config::auth_path())?;
                let models_path = default_models_path(&Config::global_dir());
                let registry = ModelRegistry::load(&auth, Some(models_path));
                if let Some(error) = registry.error() {
                    eprintln!("Warning: models.json error: {error}");
                }
                list_models(&registry, pattern.as_deref());
                return Ok(());
            }
        }
    }

    // Initialize logging (skip for ultra-fast paths like --version)
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .with_writer(io::stderr)
        .init();

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

fn print_error_with_hints(err: &anyhow::Error) {
    for cause in err.chain() {
        if let Some(pi_error) = cause.downcast_ref::<pi::error::Error>() {
            eprint!("{}", pi::error_hints::format_error_with_hints(pi_error));
            return;
        }
    }

    eprintln!("{err}");
}

fn validate_theme_path_spec(theme_spec: Option<&str>, cwd: &Path) -> Result<()> {
    if let Some(theme_spec) = theme_spec {
        if pi::theme::looks_like_theme_path(theme_spec) {
            pi::theme::Theme::resolve_spec(theme_spec, cwd).map_err(anyhow::Error::new)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn run(mut cli: cli::Cli, runtime_handle: RuntimeHandle) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    if let Some(command) = cli.command.take() {
        handle_subcommand(command, &cwd).await?;
        return Ok(());
    }

    let mut config = Config::load()?;
    if let Some(theme_spec) = cli.theme.as_deref() {
        // Theme already validated above
        config.theme = Some(theme_spec.to_string());
    }
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
        theme_paths: cli.theme_path.clone(),
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

    // Auto-detect print mode: if the user passed positional message args (e.g. `pi "hello"`)
    // or stdin was piped, run in non-interactive print mode automatically.
    if !cli.print && cli.mode.is_none() && !cli.message_args().is_empty() {
        cli.print = true;
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
            Ok(key) => {
                break (selection, key);
            }
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
    let test_mode = std::env::var_os("PI_TEST_MODE").is_some();
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
        test_mode,
    );
    let provider =
        providers::create_provider(&selection.model_entry, None).map_err(anyhow::Error::new)?;
    let stream_options = pi::app::build_stream_options(&config, resolved_key, &selection, &session);
    let agent_config = AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations: 50,
        stream_options,
    };

    let tools = ToolRegistry::new(&enabled_tools, &cwd, Some(&config));
    let session_arc = Arc::new(Mutex::new(session));
    let compaction_settings = ResolvedCompactionSettings {
        enabled: config.compaction_enabled(),
        reserve_tokens: config.compaction_reserve_tokens(),
        keep_recent_tokens: config.compaction_keep_recent_tokens(),
    };
    let mut agent_session = AgentSession::new(
        Agent::new(provider, tools, agent_config),
        session_arc,
        !cli.no_session,
        compaction_settings,
    );

    let history = {
        let cx = pi::agent_cx::AgentCx::for_request();
        let session = agent_session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        session.to_messages_for_current_path()
    };
    if !history.is_empty() {
        agent_session.agent.replace_messages(history);
    }

    if !resources.extensions().is_empty() {
        let ext_policy =
            config.resolve_extension_policy(cli.extension_policy.as_deref());
        agent_session
            .enable_extensions_with_policy(
                &enabled_tools,
                &cwd,
                Some(&config),
                resources.extensions(),
                Some(ext_policy),
            )
            .await
            .map_err(anyhow::Error::new)?;

        // Merge extension-registered providers into the model registry.
        if let Some(region) = &agent_session.extensions {
            let ext_entries = region.manager().extension_model_entries();
            if !ext_entries.is_empty() {
                // Build OAuth configs map from model entries before merging.
                let ext_oauth_configs: std::collections::HashMap<String, pi::models::OAuthConfig> =
                    ext_entries
                        .iter()
                        .filter_map(|entry| {
                            entry
                                .oauth_config
                                .as_ref()
                                .map(|cfg| (entry.model.provider.clone(), cfg.clone()))
                        })
                        .collect();

                model_registry.merge_entries(ext_entries);

                // Refresh expired OAuth tokens for extension-registered providers.
                if !ext_oauth_configs.is_empty() {
                    let client = pi::http::client::Client::new();
                    if let Err(e) = auth
                        .refresh_expired_extension_oauth_tokens(&client, &ext_oauth_configs)
                        .await
                    {
                        tracing::warn!(
                            event = "pi.auth.extension_oauth_refresh.failed",
                            error = %e,
                            "Failed to refresh extension OAuth tokens, continuing with existing credentials"
                        );
                    }
                }
            }
        }
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

    run_print_mode(
        &mut agent_session,
        &mode,
        initial,
        messages,
        &resources,
        runtime_handle.clone(),
    )
    .await
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
        cli::Commands::UpdateIndex => {
            handle_update_index().await?;
        }
        cli::Commands::Search {
            query,
            tag,
            sort,
            limit,
        } => {
            handle_search(&query, tag.as_deref(), &sort, limit).await?;
        }
        cli::Commands::Info { name } => {
            handle_info(&name).await?;
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
    let resolved_source = manager.resolve_install_source_alias(source);
    manager.install(&resolved_source, scope).await?;
    manager.add_package_source(&resolved_source, scope).await?;
    if resolved_source == source {
        println!("Installed {source}");
    } else {
        println!("Installed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

async fn handle_package_remove(manager: &PackageManager, source: &str, local: bool) -> Result<()> {
    let scope = scope_from_flag(local);
    let resolved_source = manager.resolve_install_source_alias(source);
    manager.remove(&resolved_source, scope).await?;
    manager
        .remove_package_source(&resolved_source, scope)
        .await?;
    if resolved_source == source {
        println!("Removed {source}");
    } else {
        println!("Removed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

async fn handle_package_update(manager: &PackageManager, source: Option<String>) -> Result<()> {
    let entries = manager.list_packages().await?;

    if let Some(source) = source {
        let resolved_source = manager.resolve_install_source_alias(&source);
        let identity = manager.package_identity(&resolved_source);
        for entry in entries {
            if manager.package_identity(&entry.source) != identity {
                continue;
            }
            manager.update_source(&entry.source, entry.scope).await?;
        }
        if resolved_source == source {
            println!("Updated {source}");
        } else {
            println!("Updated {source} (resolved to {resolved_source})");
        }
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

async fn handle_update_index() -> Result<()> {
    let store = ExtensionIndexStore::default_store();
    let client = pi::http::client::Client::new();
    let (_, stats) = store.refresh_best_effort(&client).await?;

    if !stats.refreshed {
        println!(
            "Extension index refresh skipped: remote sources unavailable; using existing seed/cache."
        );
        return Ok(());
    }

    println!(
        "Extension index refreshed: {} merged entries (npm: {}, github: {}) at {}",
        stats.merged_entries,
        stats.npm_entries,
        stats.github_entries,
        store.path().display()
    );
    Ok(())
}

async fn handle_search(query: &str, tag: Option<&str>, sort: &str, limit: usize) -> Result<()> {
    let store = ExtensionIndexStore::default_store();

    // Load cached index; auto-refresh only if a cache file exists but is stale.
    // If no cache exists, use the built-in seed index without a network call.
    let mut index = store.load_or_seed()?;
    let has_cache = store.path().exists();
    if has_cache
        && index.is_stale(
            chrono::Utc::now(),
            pi::extension_index::DEFAULT_INDEX_MAX_AGE,
        )
    {
        println!("Refreshing extension index...");
        let client = pi::http::client::Client::new();
        match store.refresh_best_effort(&client).await {
            Ok((refreshed, _)) => index = refreshed,
            Err(_) => {
                println!(
                    "Warning: Could not refresh index (network unavailable). Using cached results."
                );
            }
        }
    }

    // Search
    let mut hits = index.search(query, limit);

    // Filter by tag if requested
    if let Some(tag_filter) = tag {
        let tag_lower = tag_filter.to_ascii_lowercase();
        hits.retain(|hit| {
            hit.entry
                .tags
                .iter()
                .any(|t| t.to_ascii_lowercase() == tag_lower)
        });
    }

    // Sort by name if requested (relevance is the default from search())
    if sort == "name" {
        hits.sort_by(|a, b| {
            a.entry
                .name
                .to_ascii_lowercase()
                .cmp(&b.entry.name.to_ascii_lowercase())
        });
    }

    if hits.is_empty() {
        println!("No extensions found for \"{query}\".");
        return Ok(());
    }

    print_search_results(&hits);
    Ok(())
}

#[allow(clippy::uninlined_format_args)]
fn print_search_results(hits: &[pi::extension_index::ExtensionSearchHit]) {
    // Column widths
    let name_w = hits
        .iter()
        .map(|h| h.entry.name.len())
        .max()
        .unwrap_or(0)
        .max(4); // "Name"
    let desc_w = hits
        .iter()
        .map(|h| h.entry.description.as_deref().unwrap_or("").len().min(50))
        .max()
        .unwrap_or(0)
        .max(11); // "Description"
    let tags_w = hits
        .iter()
        .map(|h| h.entry.tags.join(", ").len().min(30))
        .max()
        .unwrap_or(0)
        .max(4); // "Tags"
    let source_w = 6; // "Source"

    // Header
    println!(
        "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
        "Name", "Description", "Tags", "Source"
    );
    println!(
        "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
        "-".repeat(name_w),
        "-".repeat(desc_w),
        "-".repeat(tags_w),
        "-".repeat(source_w)
    );

    // Rows
    for hit in hits {
        let desc = hit.entry.description.as_deref().unwrap_or("");
        let desc_truncated = if desc.chars().count() > 50 {
            let truncated: String = desc.chars().take(47).collect();
            format!("{truncated}...")
        } else {
            desc.to_string()
        };
        let tags_joined = hit.entry.tags.join(", ");
        let tags_truncated = if tags_joined.chars().count() > 30 {
            let truncated: String = tags_joined.chars().take(27).collect();
            format!("{truncated}...")
        } else {
            tags_joined
        };
        let source_label = match &hit.entry.source {
            Some(pi::extension_index::ExtensionIndexSource::Npm { .. }) => "npm",
            Some(pi::extension_index::ExtensionIndexSource::Git { .. }) => "git",
            Some(pi::extension_index::ExtensionIndexSource::Url { .. }) => "url",
            None => "-",
        };
        println!(
            "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}",
            hit.entry.name, desc_truncated, tags_truncated, source_label
        );
    }

    let count = hits.len();
    let noun = if count == 1 {
        "extension"
    } else {
        "extensions"
    };
    println!("\n  {count} {noun} found. Install with: pi install <name>");
}

async fn handle_info(name: &str) -> Result<()> {
    let store = ExtensionIndexStore::default_store();
    let index = store.load_or_seed()?;

    // Look up by exact id, name, or fuzzy match (top-1 search hit)
    let entry = index
        .entries
        .iter()
        .find(|e| e.id.eq_ignore_ascii_case(name) || e.name.eq_ignore_ascii_case(name))
        .or_else(|| {
            let hits = index.search(name, 1);
            hits.into_iter()
                .next()
                .map(|h| h.entry)
                .and_then(|matched| {
                    // Return a reference from the index, not the owned clone
                    index.entries.iter().find(|e| e.id == matched.id)
                })
        });

    let Some(entry) = entry else {
        println!("Extension \"{name}\" not found.");
        println!("Try: pi search {name}");
        return Ok(());
    };

    print_extension_info(entry);
    Ok(())
}

fn print_extension_info(entry: &pi::extension_index::ExtensionIndexEntry) {
    let width = 60;
    let bar = "─".repeat(width);

    // Header
    println!("  ┌{bar}┐");
    let title = &entry.name;
    let padding = width.saturating_sub(title.len() + 1);
    println!("  │ {title}{:padding$}│", "");

    // ID (if different from name)
    if entry.id != entry.name {
        let id_line = format!("id: {}", entry.id);
        let padding = width.saturating_sub(id_line.len() + 1);
        println!("  │ {id_line}{:padding$}│", "");
    }

    // Description
    if let Some(desc) = &entry.description {
        println!("  │{:width$}│", "");
        for line in wrap_text(desc, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    }

    // Separator
    println!("  ├{bar}┤");

    // Tags
    if !entry.tags.is_empty() {
        let tags_line = format!("Tags: {}", entry.tags.join(", "));
        let padding = width.saturating_sub(tags_line.len() + 1);
        println!("  │ {tags_line}{:padding$}│", "");
    }

    // License
    if let Some(license) = &entry.license {
        let lic_line = format!("License: {license}");
        let padding = width.saturating_sub(lic_line.len() + 1);
        println!("  │ {lic_line}{:padding$}│", "");
    }

    // Source
    if let Some(source) = &entry.source {
        let source_line = match source {
            pi::extension_index::ExtensionIndexSource::Npm {
                package, version, ..
            } => {
                let ver = version.as_deref().unwrap_or("latest");
                format!("Source: npm:{package}@{ver}")
            }
            pi::extension_index::ExtensionIndexSource::Git { repo, path, .. } => {
                let suffix = path.as_deref().map_or(String::new(), |p| format!(" ({p})"));
                format!("Source: git:{repo}{suffix}")
            }
            pi::extension_index::ExtensionIndexSource::Url { url } => {
                format!("Source: {url}")
            }
        };
        for line in wrap_text(&source_line, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    }

    // Install command
    println!("  ├{bar}┤");
    if let Some(install_source) = &entry.install_source {
        let install_line = format!("Install: pi install {install_source}");
        for line in wrap_text(&install_line, width - 2) {
            let padding = width.saturating_sub(line.len() + 1);
            println!("  │ {line}{:padding$}│", "");
        }
    } else {
        let hint = "Install source not available";
        let padding = width.saturating_sub(hint.len() + 1);
        println!("  │ {hint}{:padding$}│", "");
    }

    println!("  └{bar}┘");
}

/// Wrap text to fit within `max_width` characters.
fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if current.is_empty() {
                current = word.to_string();
            } else if current.len() + 1 + word.len() <= max_width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
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
    println!("  Packages: {}", Config::package_dir().display());
    println!("  ExtIndex: {}", Config::extension_index_path().display());
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

#[allow(clippy::too_many_lines)]
async fn run_print_mode(
    session: &mut AgentSession,
    mode: &str,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    resources: &ResourceLoader,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    if mode != "text" && mode != "json" {
        bail!("Unknown mode: {mode}");
    }
    if initial.is_none() && messages.is_empty() {
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    if mode == "json" {
        let cx = pi::agent_cx::AgentCx::for_request();
        let session = session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("{}", serde_json::to_string(&session.header)?);
    }

    let mut last_message: Option<AssistantMessage> = None;
    let extensions = session.extensions.as_ref().map(|r| r.manager().clone());
    let emit_json_events = mode == "json";
    let runtime_for_events = runtime_handle.clone();
    let make_event_handler = move || {
        let extensions = extensions.clone();
        let runtime_for_events = runtime_for_events.clone();
        move |event: AgentEvent| {
            if emit_json_events {
                if let Ok(serialized) = serde_json::to_string(&event) {
                    println!("{serialized}");
                }
            }
            if let Some(manager) = &extensions {
                if let Some((event_name, data)) = extension_event_from_agent(&event) {
                    let manager = manager.clone();
                    let runtime_handle = runtime_for_events.clone();
                    runtime_handle.spawn(async move {
                        let _ = manager.dispatch_event(event_name, data).await;
                    });
                }
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
                .run_with_content_with_abort(
                    content,
                    Some(abort_signal.clone()),
                    make_event_handler(),
                )
                .await?,
        );
    }

    for message in messages {
        last_message = Some(
            session
                .run_text_with_abort(message, Some(abort_signal.clone()), make_event_handler())
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
        let mut markdown = String::new();
        for block in &last_message.content {
            if let ContentBlock::Text(text) = block {
                markdown.push_str(&text.text);
                if !markdown.ends_with('\n') {
                    markdown.push('\n');
                }
            }
        }

        if !markdown.is_empty() {
            let console = PiConsole::new();
            console.render_markdown(&markdown);
        }
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

    let AgentSession {
        agent,
        session,
        extensions: region,
        ..
    } = session;
    // Extract manager for the interactive loop; the region stays alive to
    // handle shutdown when this scope exits.
    let extensions = region.as_ref().map(|r| r.manager().clone());
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
        extensions,
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
