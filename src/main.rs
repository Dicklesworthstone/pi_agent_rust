//! Pi - Native AI coding agent CLI
//!
//! Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance-oriented native architecture with instrumented startup and TUI paths
//! - Reliability through explicit errors, bounded cancellation, and conformance tests
//! - Distribution through one supported end-user binary in official release archives

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Result, bail};
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
use asupersync::sync::{Mutex, OwnedMutexGuard};
use bubbletea::{Cmd, KeyMsg, KeyType, Message as BubbleMessage, Program, quit};
use clap::error::ErrorKind;
use pi::agent::{
    AbortHandle, Agent, AgentConfig, AgentEvent, AgentSession, PreWarmedExtensionRuntime,
};
use pi::app::StartupError;
use pi::auth::{AuthCredential, AuthStorage};
use pi::cli;
use pi::compaction::ResolvedCompactionSettings;
use pi::config::Config;
use pi::config::SettingsScope;
use pi::extension_index::{
    DEFAULT_INDEX_MAX_AGE, ExtensionIndex, ExtensionIndexEntry, ExtensionIndexStore,
    ExtensionSafetyProvenance,
};
use pi::extensions::{
    ALL_CAPABILITIES, Capability, ExtensionLoadSpec, ExtensionRegion, ExtensionRuntimeHandle,
    JsExtensionRuntimeHandle, NativeRustExtensionRuntimeHandle, PolicyDecision,
    resolve_extension_load_spec,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::model::{AssistantMessage, ContentBlock, StopReason, ThinkingLevel};
use pi::models::{ModelEntry, ModelRegistry, default_models_path, fetched_models_path};
use pi::package_manager::{
    PackageEntry, PackageManager, PackageScope, ResolvedPaths, ResolvedResource, ResourceOrigin,
};
use pi::provider::InputType;
use pi::provider_metadata::{self, PROVIDER_METADATA};
use pi::providers;
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::session_index::SessionIndex;
use pi::swarm_progress_slo::{
    ProgressSloEvaluationInput, ProgressSloReport, SWARM_PROGRESS_SLO_SCHEMA, evaluate_progress_slo,
};
use pi::swarm_replay::{
    SWARM_REPLAY_POLICY_REPORT_SCHEMA, SWARM_REPLAY_REPORT_SCHEMA, SWARM_REPLAY_TRACE_SCHEMA,
    SwarmReplayBaselinePolicy, SwarmReplayPolicyAdapter, SwarmReplayPolicyComparison,
    SwarmReplayTrace, default_swarm_replay_baseline_policies,
    evaluate_swarm_replay_baseline_policies, replay_swarm_trace,
};
use pi::tools::ToolRegistry;
use pi::tui::PiConsole;
use pi::validation_broker::{
    VALIDATION_BROKER_CLI_LEASE_MUTATION_SCHEMA, VALIDATION_BROKER_CLI_PLAN_SCHEMA,
    VALIDATION_BROKER_CLI_STATUS_SCHEMA, VALIDATION_BROKER_DECISION_SCHEMA,
    VALIDATION_BROKER_INPUT_SCHEMA, ValidationAdmissionDecision, ValidationAdmissionDecisionRecord,
    ValidationAdmissionPolicy, ValidationAdmissionRequestContext, ValidationBrokerInputSnapshot,
    ValidationSlotLease, ValidationSlotRequest, ValidationSlotState, ValidationSlotStore,
    ValidationSlotStoreSnapshot, decide_validation_admission,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;

const EXIT_CODE_FAILURE: i32 = 1;
const EXIT_CODE_USAGE: i32 = 2;
const USAGE_ERROR_PATTERNS: &[&str] = &[
    "@file arguments are not supported in rpc mode",
    "--api-key requires a model to be specified via --provider/--model or --models",
    "context-preview requires",
    "swarm-progress requires",
    "swarm-replay-preview requires",
    "unsupported swarm-progress format",
    "unsupported swarm-replay-preview policy",
    "unknown --only categories",
    "--only must include at least one category",
    "--fetch-models cannot be combined",
    "theme file not found",
    "theme spec is empty",
];

fn main() {
    // On Windows CMD.exe, ANSI escape sequences render as garbage (e.g. "←[92m")
    // unless we call SetConsoleMode with ENABLE_VIRTUAL_TERMINAL_PROCESSING first.
    // This must happen before any colored output. Silently ignored on non-Windows
    // platforms and on older Windows versions that lack VT support.
    #[cfg(windows)]
    let _ = enable_ansi_support::enable_ansi_support();

    if let Err(err) = main_impl() {
        let exit_code = exit_code_for_error(&err);
        print_error_with_hints(&err);
        std::process::exit(exit_code);
    }
}

fn parse_cli_args(raw_args: Vec<String>) -> Result<Option<(cli::Cli, Vec<cli::ExtensionCliFlag>)>> {
    match cli::parse_with_extension_flags(raw_args) {
        Ok(parsed) => Ok(Some((parsed.cli, parsed.extension_flags))),
        Err(err) => {
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                err.print()?;
                return Ok(None);
            }
            Err(anyhow::Error::new(err))
        }
    }
}

type ParsedCliEnvironment = (cli::Cli, Vec<cli::ExtensionCliFlag>, Vec<String>);

fn parse_cli_from_env() -> Result<Option<ParsedCliEnvironment>> {
    let raw_args = std::env::args().collect::<Vec<_>>();
    Ok(parse_cli_args(raw_args.clone())?
        .map(|(cli, extension_flags)| (cli, extension_flags, raw_args)))
}

fn add_fetch_models_conflict(
    conflicts: &mut Vec<&'static str>,
    condition: bool,
    description: &'static str,
) {
    if condition {
        conflicts.push(description);
    }
}

fn collect_fetch_models_execution_conflicts(
    cli: &cli::Cli,
    extension_flags: &[cli::ExtensionCliFlag],
    raw_args: &[String],
    conflicts: &mut Vec<&'static str>,
) {
    add_fetch_models_conflict(conflicts, cli.version, "--version");
    add_fetch_models_conflict(
        conflicts,
        cli.explain_extension_policy,
        "--explain-extension-policy",
    );
    add_fetch_models_conflict(
        conflicts,
        cli.explain_repair_policy,
        "--explain-repair-policy",
    );
    add_fetch_models_conflict(conflicts, cli.list_models.is_some(), "--list-models");
    add_fetch_models_conflict(conflicts, cli.list_providers, "--list-providers");
    add_fetch_models_conflict(conflicts, cli.export.is_some(), "--export");
    add_fetch_models_conflict(
        conflicts,
        cli.rpc || cli.mode.as_deref().is_some_and(|mode| !mode.eq("text")),
        "output-mode arguments",
    );
    add_fetch_models_conflict(conflicts, cli.acp, "--acp");
    add_fetch_models_conflict(conflicts, cli.command.is_some(), "a subcommand");
    add_fetch_models_conflict(conflicts, !cli.args.is_empty(), "prompt or file arguments");
    add_fetch_models_conflict(
        conflicts,
        !cli.extension.is_empty() || !extension_flags.is_empty(),
        "extension arguments",
    );
    let has_selection_arguments = raw_args
        .iter()
        .skip(1)
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| {
            matches!(argument.as_str(), "--provider" | "--model" | "--tools")
                || argument.starts_with("--provider=")
                || argument.starts_with("--model=")
                || argument.starts_with("--tools=")
        });
    add_fetch_models_conflict(
        conflicts,
        has_selection_arguments,
        "provider, model, or tool-selection arguments",
    );
    add_fetch_models_conflict(conflicts, cli.models.is_some(), "--models");
    add_fetch_models_conflict(conflicts, cli.thinking.is_some(), "--thinking");
    add_fetch_models_conflict(
        conflicts,
        cli.system_prompt.is_some() || cli.append_system_prompt.is_some(),
        "system-prompt arguments",
    );
}

fn collect_fetch_models_context_conflicts(
    cli: &cli::Cli,
    raw_args: &[String],
    conflicts: &mut Vec<&'static str>,
) {
    let has_session_arguments = cli.r#continue
        || cli.resume
        || cli.session.is_some()
        || cli.session_dir.is_some()
        || cli.no_session
        || cli.session_durability.is_some();
    add_fetch_models_conflict(conflicts, has_session_arguments, "session arguments");
    add_fetch_models_conflict(conflicts, cli.no_mouse_capture, "--no-mouse-capture");
    add_fetch_models_conflict(conflicts, cli.no_migrations, "--no-migrations");
    add_fetch_models_conflict(conflicts, cli.verbose, "--verbose");
    add_fetch_models_conflict(conflicts, cli.no_tools, "--no-tools");
    add_fetch_models_conflict(
        conflicts,
        cli.extension_policy.is_some() || cli.repair_policy.is_some(),
        "policy arguments",
    );
    add_fetch_models_conflict(
        conflicts,
        !cli.skill.is_empty() || !cli.prompt_template.is_empty(),
        "skill or prompt-template arguments",
    );
    add_fetch_models_conflict(
        conflicts,
        cli.no_extensions || cli.no_skills || cli.no_prompt_templates || cli.no_themes,
        "resource-discovery disable arguments",
    );
    add_fetch_models_conflict(
        conflicts,
        cli.theme.is_some() || !cli.theme_path.is_empty(),
        "theme arguments",
    );
    let has_hide_cwd_argument = raw_args
        .iter()
        .skip(1)
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| {
            argument == "--hide-cwd-in-prompt" || argument.starts_with("--hide-cwd-in-prompt=")
        });
    add_fetch_models_conflict(conflicts, has_hide_cwd_argument, "--hide-cwd-in-prompt");
    add_fetch_models_conflict(
        conflicts,
        cli.max_tool_iterations.is_some(),
        "--max-tool-iterations",
    );
}

fn validate_fetch_models_is_standalone(
    cli: &cli::Cli,
    extension_flags: &[cli::ExtensionCliFlag],
    raw_args: &[String],
) -> Result<()> {
    if cli.fetch_models.is_none() {
        return Ok(());
    }

    let mut conflicts = Vec::new();
    collect_fetch_models_execution_conflicts(cli, extension_flags, raw_args, &mut conflicts);
    collect_fetch_models_context_conflicts(cli, raw_args, &mut conflicts);

    if conflicts.is_empty() {
        Ok(())
    } else {
        bail!(
            "--fetch-models cannot be combined with {}; run model discovery as a standalone command",
            conflicts.join(", ")
        )
    }
}

fn reload_model_registry_with_extra_entries(
    auth: &AuthStorage,
    models_path: &Path,
    extra_entries: &[ModelEntry],
) -> ModelRegistry {
    let mut registry = ModelRegistry::load(auth, Some(models_path.to_path_buf()));
    if let Some(error) = registry.error() {
        eprintln!("Warning: models.json error: {error}");
    }
    if !extra_entries.is_empty() {
        registry.merge_entries(extra_entries.to_vec());
    }
    registry
}

#[allow(clippy::too_many_arguments)]
async fn resolve_selection_with_auth(
    cli: &mut cli::Cli,
    config: &Config,
    session: &Session,
    model_registry: &mut ModelRegistry,
    scoped_patterns: &[String],
    auth: &mut AuthStorage,
    models_path: &Path,
    allow_setup_prompt: bool,
    extra_entries: &[ModelEntry],
) -> Result<Option<(pi::app::ModelSelection, Option<String>)>> {
    loop {
        let scoped_models = if scoped_patterns.is_empty() {
            Vec::new()
        } else {
            pi::app::resolve_model_scope(
                scoped_patterns,
                model_registry,
                has_cli_api_key_override(cli.api_key.as_deref()),
            )
        };

        let selection = match pi::app::select_model_and_thinking(
            cli,
            config,
            session,
            model_registry,
            &scoped_models,
            &Config::global_dir(),
        ) {
            Ok(selection) => selection,
            Err(err) => {
                if let Some(startup) = err.downcast_ref::<StartupError>()
                    && allow_setup_prompt
                {
                    if run_first_time_setup(startup, auth, cli, models_path).await? {
                        *model_registry = reload_model_registry_with_extra_entries(
                            auth,
                            models_path,
                            extra_entries,
                        );
                        continue;
                    }
                    return Ok(None);
                }
                return Err(err);
            }
        };

        match pi::app::resolve_api_key(auth, cli, &selection.model_entry) {
            // Structured SAP credentials are deliberately resolved in the provider, after
            // custom-header precedence is known. Eager exchange here would touch auth.json or
            // the network even when a complete Authorization override (or authHeader:false)
            // makes those credentials unused.
            Ok(key) => return Ok(Some((selection, key))),
            Err(err) => {
                if let Some(startup) = err.downcast_ref::<StartupError>()
                    && allow_setup_prompt
                {
                    if run_first_time_setup(startup, auth, cli, models_path).await? {
                        *model_registry = reload_model_registry_with_extra_entries(
                            auth,
                            models_path,
                            extra_entries,
                        );
                        continue;
                    }
                    return Ok(None);
                }
                return Err(err);
            }
        }
    }
}

fn should_retry_selection_after_extensions(
    cli: &cli::Cli,
    err: &anyhow::Error,
    has_extensions: bool,
) -> bool {
    if !has_extensions || (cli.provider.is_none() && cli.model.is_none()) {
        return false;
    }

    let message = err.to_string().to_ascii_lowercase();
    message.contains(" not found") || message.contains("no models available for provider")
}

fn build_extension_bootstrap_selection(
    config: &Config,
    model_registry: &ModelRegistry,
    models_path: &Path,
) -> Result<pi::app::ModelSelection> {
    let model_entry = pi::app::bootstrap_model_entry(model_registry).ok_or_else(|| {
        anyhow::Error::new(StartupError::NoModelsAvailable {
            models_path: models_path.to_path_buf(),
        })
    })?;
    let thinking_level = config
        .default_thinking_level
        .as_deref()
        .and_then(|value| value.parse::<ThinkingLevel>().ok());

    Ok(pi::app::ModelSelection {
        thinking_level: model_entry
            .clamp_thinking_level(thinking_level.unwrap_or(ThinkingLevel::XHigh)),
        model_entry,
        scoped_models: Vec::new(),
        fallback_message: None,
    })
}

fn context_window_tokens_for_entry(entry: &ModelEntry) -> u32 {
    if entry.model.context_window.eq(&0) {
        tracing::warn!(
            "Model {} reported context_window=0; falling back to default compaction window",
            entry.model.id
        );
        ResolvedCompactionSettings::default().context_window_tokens
    } else {
        entry.model.context_window
    }
}

#[allow(clippy::too_many_lines)]
fn main_impl() -> Result<()> {
    // Parse CLI arguments
    let Some((mut cli, extension_flags, raw_args)) = parse_cli_from_env()? else {
        return Ok(());
    };

    validate_fetch_models_is_standalone(&cli, &extension_flags, &raw_args)?;

    if cli.version {
        print_version();
        return Ok(());
    }

    // Validate theme file paths.
    // Named themes (without .json, /, ~) are validated later after resource loading.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    validate_theme_path_spec(cli.theme.as_deref(), &cwd)?;
    if cli.rpc && cli.mode.is_none() {
        cli.mode = Some("rpc".to_string());
    }

    // Ultra-fast paths that don't need tracing or the async runtime.
    if let Some(command) = &cli.command {
        match command {
            cli::Commands::Install { source, local } => {
                let manager = PackageManager::new(cwd);
                handle_package_install_blocking(&manager, source, *local)?;
                return Ok(());
            }
            cli::Commands::Remove { source, local } => {
                let manager = PackageManager::new(cwd);
                handle_package_remove_blocking(&manager, source, *local)?;
                return Ok(());
            }
            cli::Commands::Update { source } => {
                let manager = PackageManager::new(cwd);
                handle_package_update_blocking(&manager, source.as_deref())?;
                return Ok(());
            }
            cli::Commands::ContextPreview {
                format,
                bead,
                changed_paths,
                failing_command,
                max_items,
                max_bytes,
                query,
            } => {
                handle_context_preview_blocking(
                    &cwd,
                    format,
                    bead.as_deref(),
                    changed_paths,
                    failing_command.as_deref(),
                    *max_items,
                    *max_bytes,
                    query,
                )?;
                return Ok(());
            }
            cli::Commands::SwarmProgress {
                input,
                since,
                format,
                out_json,
                out_text,
            } => {
                handle_swarm_progress_blocking(
                    &cwd,
                    input,
                    since.as_deref(),
                    format,
                    out_json.as_deref(),
                    out_text.as_deref(),
                )?;
                return Ok(());
            }
            cli::Commands::SwarmReplayPreview {
                trace,
                policies,
                format,
                out_json,
                out_text,
                generated_at,
            } => {
                handle_swarm_replay_preview_blocking(
                    &cwd,
                    trace,
                    policies,
                    format,
                    out_json.as_deref(),
                    out_text.as_deref(),
                    generated_at.as_deref(),
                )?;
                return Ok(());
            }
            cli::Commands::ValidationBroker { command } => {
                handle_validation_broker_blocking(&cwd, command)?;
                return Ok(());
            }
            cli::Commands::List => {
                let manager = PackageManager::new(cwd);
                handle_package_list_blocking(&manager)?;
                return Ok(());
            }
            cli::Commands::Info { name } => {
                handle_info_blocking(name)?;
                return Ok(());
            }
            cli::Commands::Search {
                query,
                tag,
                sort,
                limit,
            } if handle_search_blocking(query, tag.as_deref(), sort, *limit)? => {
                return Ok(());
            }
            cli::Commands::Doctor {
                path,
                format,
                policy,
                fix,
                only,
            } => {
                handle_doctor(
                    &cwd,
                    path.as_deref(),
                    format,
                    policy.as_deref(),
                    *fix,
                    only.as_deref(),
                )?;
                return Ok(());
            }
            cli::Commands::Config { show, paths, json } => {
                if *paths && !*show && !*json {
                    handle_config_paths_fast(&cwd);
                    return Ok(());
                }
                if !*paths && (*show || *json) {
                    let manager = PackageManager::new(cwd.clone());
                    let entries = manager.list_packages_blocking()?;
                    if entries.is_empty() {
                        if *show {
                            handle_config_show_fast(&cwd);
                            return Ok(());
                        }
                        if *json {
                            handle_config_json_fast(&cwd)?;
                            return Ok(());
                        }
                    } else if let Some(packages) =
                        collect_config_packages_blocking(&manager, entries)?
                    {
                        let report = build_config_report(&cwd, &packages);
                        if *json {
                            println!("{}", serde_json::to_string_pretty(&report)?);
                        } else {
                            print_config_report(&report, true);
                        }
                        return Ok(());
                    }
                }
            }
            _ => {}
        }
    }

    if cli.explain_extension_policy {
        let config = Config::load()?;
        let resolved =
            config.resolve_extension_policy_with_metadata(cli.extension_policy.as_deref());
        print_resolved_extension_policy(&resolved)?;
        return Ok(());
    }

    if cli.explain_repair_policy {
        let config = Config::load()?;
        let resolved = config.resolve_repair_policy_with_metadata(cli.repair_policy.as_deref());
        print_resolved_repair_policy(&resolved)?;
        return Ok(());
    }

    // List-providers is a fast offline query that uses only static metadata.
    if cli.list_providers {
        list_providers();
        return Ok(());
    }

    // List-models is an offline query; avoid loading resources or booting the runtime when possible.
    //
    // IMPORTANT: if extension compat scanning is enabled, or explicit CLI extensions are provided,
    // we must boot the normal startup path so the compat ledger can be emitted deterministically.
    if cli.command.is_none()
        && let Some(pattern) = &cli.list_models
    {
        let compat_scan_enabled = std::env::var("PI_EXT_COMPAT_SCAN").is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        });
        let has_cli_extensions = !cli.extension.is_empty();

        if !compat_scan_enabled && !has_cli_extensions {
            // Note: we intentionally skip OAuth refresh here to keep this path fast and offline.
            let models_path = default_models_path(&Config::global_dir());
            if let Some(payload) = load_list_models_cache(&models_path) {
                if let Some(error) = &payload.error {
                    eprintln!("Warning: models.json error: {error}");
                }
                list_models_from_cached_rows(&payload.rows, pattern.as_deref());
                return Ok(());
            }

            let auth = AuthStorage::load(Config::auth_path())?;
            let registry = ModelRegistry::load_for_listing(&auth, Some(models_path.clone()));
            let error = registry.error().map(std::string::ToString::to_string);
            if let Some(error) = &error {
                eprintln!("Warning: models.json error: {error}");
            }

            let mut models = registry.available_models();
            models.sort_by(|a, b| {
                let provider_cmp = a.model.provider.cmp(&b.model.provider);
                if matches!(provider_cmp, std::cmp::Ordering::Equal) {
                    a.model.id.cmp(&b.model.id)
                } else {
                    provider_cmp
                }
            });
            let rows = build_model_rows(&models);
            let payload = ListModelsCachePayload {
                error,
                rows: rows
                    .into_iter()
                    .map(
                        |(provider, model, context, max_out, thinking, images)| CachedModelRow {
                            provider,
                            model,
                            context,
                            max_out,
                            thinking,
                            images,
                        },
                    )
                    .collect(),
            };
            save_list_models_cache(&models_path, &payload);
            list_models_from_cached_rows(&payload.rows, pattern.as_deref());
            return Ok(());
        }
    }

    if cli.command.is_none()
        && cli.fetch_models.is_none()
        && !cli.acp
        && cli.mode.as_deref().is_none_or(|mode| mode.ne("rpc"))
    {
        let stdin_content = read_piped_stdin()?;
        pi::app::apply_piped_stdin(&mut cli, stdin_content);
    }

    if !cli.print && cli.mode.is_none() && !cli.message_args().is_empty() {
        cli.print = true;
    }

    pi::app::normalize_cli(&mut cli);

    let early_mode = cli.mode.clone().unwrap_or_else(|| {
        if !cli.print && cli.export.is_none() {
            "interactive".to_string()
        } else {
            "text".to_string()
        }
    });
    if cli.command.is_none()
        && cli.fetch_models.is_none()
        && early_mode.eq("text")
        && cli.export.is_none()
        && cli.file_args().is_empty()
        && cli
            .message_args()
            .iter()
            .all(|message| message.trim().is_empty())
    {
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
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
        .blocking_threads(1, 2)
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let handle = runtime.handle();
    let result = runtime.block_on(run(cli, extension_flags, handle));
    // `run()` owns graceful application shutdown. Exiting here avoids waiting on
    // runtime-owned background tasks after the CLI/TUI has already finished.
    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            let exit_code = exit_code_for_error(&err);
            print_error_with_hints(&err);
            std::process::exit(exit_code);
        }
    }
}

fn print_error_with_hints(err: &anyhow::Error) {
    for cause in err.chain() {
        if let Some(pi_error) = cause.downcast_ref::<pi::error::Error>() {
            eprint!("{}", pi::error_hints::format_error_with_hints(pi_error));
            return;
        }
    }

    eprintln!("{err:?}");
}

fn exit_code_for_error(err: &anyhow::Error) -> i32 {
    if is_usage_error(err) {
        EXIT_CODE_USAGE
    } else {
        EXIT_CODE_FAILURE
    }
}

fn is_usage_error(err: &anyhow::Error) -> bool {
    if err
        .chain()
        .any(|cause| cause.downcast_ref::<clap::Error>().is_some())
    {
        return true;
    }

    if err.chain().any(|cause| {
        cause
            .downcast_ref::<pi::error::Error>()
            .is_some_and(|pi_error| matches!(pi_error, pi::error::Error::Validation(_)))
    }) {
        return true;
    }

    let message = err.to_string().to_ascii_lowercase();
    USAGE_ERROR_PATTERNS
        .iter()
        .any(|pattern| message.contains(pattern))
}

fn validate_theme_path_spec(theme_spec: Option<&str>, cwd: &Path) -> Result<()> {
    if let Some(theme_spec) = theme_spec
        && pi::theme::looks_like_theme_path(theme_spec)
    {
        pi::theme::Theme::resolve_spec(theme_spec, cwd).map_err(anyhow::Error::new)?;
    }
    Ok(())
}

fn parse_bool_flag_value(flag_name: &str, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(pi::error::Error::validation(format!(
            "Invalid boolean value for extension flag --{flag_name}: \"{raw}\". Use one of: true,false,1,0,yes,no,on,off."
        ))
        .into()),
    }
}

fn coerce_extension_flag_value(
    flag: &cli::ExtensionCliFlag,
    declared_type: &str,
) -> Result<serde_json::Value> {
    match declared_type.trim().to_ascii_lowercase().as_str() {
        "bool" | "boolean" => {
            if let Some(raw) = flag.value.as_deref() {
                Ok(Value::Bool(parse_bool_flag_value(&flag.name, raw)?))
            } else {
                Ok(Value::Bool(true))
            }
        }
        "number" | "int" | "integer" | "float" => {
            let Some(raw) = flag.value.as_deref() else {
                return Err(pi::error::Error::validation(format!(
                    "Extension flag --{} requires a numeric value.",
                    flag.name
                ))
                .into());
            };
            if let Ok(parsed) = raw.parse::<i64>() {
                return Ok(Value::Number(parsed.into()));
            }
            let parsed = raw.parse::<f64>().map_err(|_| {
                pi::error::Error::validation(format!(
                    "Invalid numeric value for extension flag --{}: \"{}\"",
                    flag.name, raw
                ))
            })?;
            let Some(number) = serde_json::Number::from_f64(parsed) else {
                return Err(pi::error::Error::validation(format!(
                    "Numeric value for extension flag --{} is not finite: \"{}\"",
                    flag.name, raw
                ))
                .into());
            };
            Ok(Value::Number(number))
        }
        _ => {
            let Some(raw) = flag.value.as_deref() else {
                return Err(pi::error::Error::validation(format!(
                    "Extension flag --{} requires a value.",
                    flag.name
                ))
                .into());
            };
            Ok(Value::String(raw.to_string()))
        }
    }
}

async fn apply_extension_cli_flags(
    manager: &pi::extensions::ExtensionManager,
    extension_flags: &[cli::ExtensionCliFlag],
) -> Result<()> {
    if extension_flags.is_empty() {
        return Ok(());
    }

    let registered = manager.list_flags();
    let known_names: std::collections::BTreeSet<String> = registered
        .iter()
        .filter_map(|flag| flag.get("name").and_then(Value::as_str))
        .map(ToString::to_string)
        .collect();

    for cli_flag in extension_flags {
        let matches = registered
            .iter()
            .filter(|flag| {
                flag.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|name| name.eq_ignore_ascii_case(&cli_flag.name))
            })
            .collect::<Vec<_>>();

        if matches.is_empty() {
            let known = if known_names.is_empty() {
                "(none)".to_string()
            } else {
                known_names
                    .iter()
                    .map(|name| format!("--{name}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            tracing::debug!(
                event = "pi.extensions.flags.ignored_unknown",
                flag = %cli_flag.display_name(),
                registered = %known,
                "Ignoring unknown extension flag (not registered by any loaded extension)."
            );
            continue;
        }

        for spec in matches {
            let Some(extension_id) = spec.get("extension_id").and_then(Value::as_str) else {
                return Err(pi::error::Error::validation(format!(
                    "Extension flag --{} cannot be set because extension metadata is missing extension_id.",
                    cli_flag.name
                ))
                .into());
            };
            if extension_id.trim().is_empty() {
                return Err(pi::error::Error::validation(format!(
                    "Extension flag --{} cannot be set because extension_id is empty.",
                    cli_flag.name
                ))
                .into());
            }
            let registered_name = spec.get("name").and_then(Value::as_str).ok_or_else(|| {
                pi::error::Error::validation(format!(
                    "Extension flag --{} is missing name metadata.",
                    cli_flag.name
                ))
            })?;
            let flag_type = spec.get("type").and_then(Value::as_str).unwrap_or("string");
            let value = coerce_extension_flag_value(cli_flag, flag_type)?;
            manager
                .set_flag_value(extension_id, registered_name, value)
                .await
                .map_err(anyhow::Error::new)?;
        }
    }

    Ok(())
}

fn policy_config_example(profile: &str, allow_dangerous: bool) -> serde_json::Value {
    serde_json::json!({
        "extensionPolicy": {
            "profile": profile,
            "allowDangerous": allow_dangerous,
        }
    })
}

fn policy_default_toggle_example(default_permissive: bool) -> serde_json::Value {
    serde_json::json!({
        "extensionPolicy": {
            "defaultPermissive": default_permissive,
        }
    })
}

fn extension_policy_migration_guardrails(
    resolved: &pi::config::ResolvedExtensionPolicy,
) -> serde_json::Value {
    serde_json::json!({
        "default_profile": "permissive",
        "active_default_profile": resolved.profile_source.eq("default") && resolved.effective_profile.eq("permissive"),
        "profile_source": resolved.profile_source,
        "permissive_by_default_reason": "Fresh installs favor extension compatibility and custom UI out of the box.",
        "override_cli": {
            "safe_strict_mode": "pi --extension-policy safe <your command>",
            "balanced_prompt_mode": "pi --extension-policy balanced <your command>",
            "balanced_with_dangerous_caps": "PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced <your command>",
            "explicit_permissive": "pi --extension-policy permissive <your command>",
        },
        "settings_examples": {
            "default_permissive": policy_default_toggle_example(true),
            "default_safe": policy_default_toggle_example(false),
            "safe_strict_mode": policy_config_example("safe", false),
            "balanced_prompt_mode": policy_config_example("balanced", false),
            "balanced_with_dangerous_caps": policy_config_example("balanced", true),
            "explicit_permissive": policy_config_example("permissive", false),
        },
        "revert_to_safe_cli": "pi --extension-policy safe <your command>",
    })
}

const fn maybe_print_extension_policy_migration_notice(
    _resolved: &pi::config::ResolvedExtensionPolicy,
) {
}

fn policy_reason_detail(reason: &str) -> &'static str {
    match reason {
        "extension_deny" => "Denied by an extension-specific override.",
        "deny_caps" => "Denied by the global deny list.",
        "extension_allow" => "Allowed by an extension-specific override.",
        "default_caps" => "Allowed by profile defaults.",
        "not_in_default_caps" => "Not part of profile defaults in strict mode.",
        "prompt_required" => "Requires an explicit runtime prompt decision.",
        "permissive" => "Allowed because permissive mode bypasses prompts.",
        "empty_capability" => "Invalid request: capability name is empty.",
        _ => "Policy engine returned an implementation-defined reason.",
    }
}

fn capability_remediation(capability: Capability, decision: PolicyDecision) -> serde_json::Value {
    let is_dangerous = capability.is_dangerous();

    let (to_allow_cli, to_allow_config, recommendation) = match (is_dangerous, decision) {
        (true, PolicyDecision::Deny) => (
            vec![
                "PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced <your command>",
                "pi --extension-policy permissive <your command>",
            ],
            vec![
                policy_config_example("balanced", true),
                policy_config_example("permissive", false),
            ],
            "Prefer balanced + allowDangerous=true over permissive for narrower blast radius.",
        ),
        (true, PolicyDecision::Prompt) => (
            vec![
                "Approve the runtime capability prompt (Allow once/always).",
                "pi --extension-policy permissive <your command>",
            ],
            vec![
                policy_config_example("balanced", true),
                policy_config_example("permissive", false),
            ],
            "Use prompt approvals first; move to permissive only if prompts are operationally impossible.",
        ),
        (true, PolicyDecision::Allow) => (
            Vec::new(),
            Vec::new(),
            "Capability is already allowed; keep this only if the extension truly needs it.",
        ),
        (false, PolicyDecision::Deny) => (
            vec![
                "pi --extension-policy balanced <your command>",
                "pi --extension-policy permissive <your command>",
            ],
            vec![
                policy_config_example("balanced", false),
                policy_config_example("permissive", false),
            ],
            "Balanced is usually enough; permissive should be temporary.",
        ),
        (false, PolicyDecision::Prompt) => (
            vec![
                "Approve the runtime capability prompt (Allow once/always).",
                "pi --extension-policy permissive <your command>",
            ],
            vec![
                policy_config_example("balanced", false),
                policy_config_example("permissive", false),
            ],
            "Prompt mode keeps explicit approval in the loop while preserving least privilege.",
        ),
        (false, PolicyDecision::Allow) => (
            Vec::new(),
            Vec::new(),
            "Capability is already allowed in the active profile.",
        ),
    };

    let to_restrict_cli = if is_dangerous {
        vec![
            "pi --extension-policy balanced <your command>",
            "pi --extension-policy safe <your command>",
        ]
    } else {
        vec!["pi --extension-policy safe <your command>"]
    };
    let to_restrict_config = if is_dangerous {
        vec![
            policy_config_example("balanced", false),
            policy_config_example("safe", false),
        ]
    } else {
        vec![policy_config_example("safe", false)]
    };

    serde_json::json!({
        "dangerous_capability": is_dangerous,
        "to_allow_cli": to_allow_cli,
        "to_allow_config_examples": to_allow_config,
        "to_restrict_cli": to_restrict_cli,
        "to_restrict_config_examples": to_restrict_config,
        "recommendation": recommendation,
    })
}

fn print_resolved_extension_policy(resolved: &pi::config::ResolvedExtensionPolicy) -> Result<()> {
    let capability_decisions = ALL_CAPABILITIES
        .iter()
        .map(|capability| {
            let check = resolved.policy.evaluate(capability.as_str());
            serde_json::json!({
                "capability": capability.as_str(),
                "decision": check.decision,
                "reason": check.reason,
                "reason_detail": policy_reason_detail(&check.reason),
                "remediation": capability_remediation(*capability, check.decision),
            })
        })
        .collect::<Vec<_>>();

    let dangerous_capabilities = Capability::dangerous_list()
        .iter()
        .map(|capability| {
            let check = resolved.policy.evaluate(capability.as_str());
            serde_json::json!({
                "capability": capability.as_str(),
                "decision": check.decision,
                "reason": check.reason,
                "reason_detail": policy_reason_detail(&check.reason),
                "remediation": capability_remediation(*capability, check.decision),
            })
        })
        .collect::<Vec<_>>();

    let profile_presets = serde_json::json!([
        {
            "profile": "safe",
            "summary": "Strict deny-by-default profile.",
            "cli": "pi --extension-policy safe <your command>",
            "config_example": policy_config_example("safe", false),
        },
        {
            "profile": "balanced",
            "summary": "Prompt-based profile (legacy alias: standard).",
            "cli": "pi --extension-policy balanced <your command>",
            "config_example": policy_config_example("balanced", false),
        },
        {
            "profile": "permissive",
            "summary": "Allow-most profile for compatibility-first workflows.",
            "cli": "pi --extension-policy permissive <your command>",
            "config_example": policy_config_example("permissive", false),
        },
    ]);

    let payload = serde_json::json!({
        "requested_profile": resolved.requested_profile,
        "effective_profile": resolved.effective_profile,
        "profile_aliases": {
            "standard": "balanced",
        },
        "profile_source": resolved.profile_source,
        "allow_dangerous": resolved.allow_dangerous,
        "profile_presets": profile_presets,
        "dangerous_capability_opt_in": {
            "cli": "PI_EXTENSION_ALLOW_DANGEROUS=1 pi --extension-policy balanced <your command>",
            "env_var": "PI_EXTENSION_ALLOW_DANGEROUS=1",
            "config_example": policy_config_example("balanced", true),
        },
        "migration_guardrails": extension_policy_migration_guardrails(resolved),
        "mode": resolved.policy.mode,
        "default_caps": resolved.policy.default_caps.clone(),
        "deny_caps": resolved.policy.deny_caps.clone(),
        "dangerous_capabilities": dangerous_capabilities,
        "capability_decisions": capability_decisions,
    });

    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

fn print_resolved_repair_policy(resolved: &pi::config::ResolvedRepairPolicy) -> Result<()> {
    let payload = serde_json::json!({
        "requested_mode": resolved.requested_mode,
        "effective_mode": resolved.effective_mode,
        "source": resolved.source,
        "modes": {
            "off": "Disable all repair functionality.",
            "suggest": "Only suggest fixes in diagnostics (default).",
            "auto-safe": "Automatically apply safe fixes (e.g., config updates).",
            "auto-strict": "Automatically apply all fixes including code changes.",
        },
        "cli_override": "pi --repair-policy <mode> <your command>",
        "env_var": "PI_REPAIR_POLICY=<mode>",
    });

    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn run(
    mut cli: cli::Cli,
    extension_flags: Vec<cli::ExtensionCliFlag>,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Resolve the HTTP request timeout before any provider HTTP client is
    // constructed so the client's single resolution path sees it. The
    // `--request-timeout` flag is bound to the PI_HTTP_REQUEST_TIMEOUT_SECS env
    // var via clap, so `cli.request_timeout` already reflects either the flag
    // or that env var. Config-file values are applied at the lowest precedence
    // before the first provider request. See pi_agent_rust#90.
    if let Some(secs) = cli.request_timeout {
        pi::http::client::set_request_timeout_override(secs);
    }

    if let Some(command) = cli.command.take() {
        handle_subcommand(command, &cwd).await?;
        return Ok(());
    }

    if let Some(provider) = cli.fetch_models.take() {
        if cli.request_timeout.is_none()
            && let Some(secs) = Config::load()?.request_timeout_secs
        {
            pi::http::client::set_request_timeout_override(secs);
        }
        handle_fetch_models(
            &provider,
            cli.api_key.as_deref(),
            cli.refresh_models,
            cli.persist_models,
        )
        .await?;
        return Ok(());
    }

    if !cli.no_migrations {
        let migration_report = pi::migrations::run_startup_migrations(&cwd);
        for message in migration_report.messages() {
            eprintln!("{message}");
        }
    }

    let mut config = Config::load()?;
    if let Some(theme_spec) = cli.theme.as_deref() {
        // Theme already validated above
        config.theme = Some(theme_spec.to_string());
    }
    if cli.no_mouse_capture {
        // The CLI flag takes precedence over the persisted setting. The
        // PI_NO_MOUSE_CAPTURE env var is read separately by run_interactive so
        // only the literal value `1` is truthy. Workaround for #78.
        config.disable_mouse_capture = Some(true);
    }
    // Apply the persisted request-timeout setting at the lowest precedence:
    // only when neither the CLI flag nor the env var has already supplied one
    // (`cli.request_timeout` reflects both). See pi_agent_rust#90.
    if cli.request_timeout.is_none()
        && let Some(secs) = config.request_timeout_secs
    {
        pi::http::client::set_request_timeout_override(secs);
    }

    let startup_mode = cli.mode.clone().unwrap_or_else(|| {
        if !cli.print && cli.export.is_none() {
            "interactive".to_string()
        } else {
            "text".to_string()
        }
    });
    let startup_is_interactive = startup_mode.eq("interactive")
        && cli.command.is_none()
        && cli.export.is_none()
        && cli.list_models.is_none();
    if startup_is_interactive {
        spawn_session_index_maintenance();
    }
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
    // Run resource loading and auth loading in parallel — they are independent.
    let auth_path = Config::auth_path();
    let (resources_result, auth_result) = futures::future::join(
        ResourceLoader::load(&package_manager, &cwd, &config, &resource_cli),
        AuthStorage::load_async(auth_path),
    )
    .await;

    let mut resources = match resources_result {
        Ok(resources) => resources,
        Err(err) => {
            if resource_cli.has_explicit_paths() {
                return Err(anyhow::Error::new(err));
            }
            eprintln!("Warning: Failed to load skills/prompts/themes/extensions: {err}");
            ResourceLoader::empty(config.enable_skill_commands())
        }
    };

    // Fail early when extension flags were extracted from the CLI but no extensions
    // are available.  Without this check the binary proceeds to model selection which
    // may fail for an unrelated reason (e.g. "No models available") and mask the real
    // usage error.
    if !extension_flags.is_empty() && resources.extensions().is_empty() {
        let rendered = extension_flags
            .iter()
            .map(cli::ExtensionCliFlag::display_name)
            .collect::<Vec<_>>()
            .join(", ");
        tracing::debug!(
            event = "pi.extensions.flags.ignored_no_extensions",
            flags = %rendered,
            "Extension flags provided but no extensions are loaded; ignoring."
        );
    }

    let mut has_js_extensions = false;
    let mut has_native_extensions = false;
    for entry in resources.extensions() {
        match resolve_extension_load_spec(entry) {
            Ok(ExtensionLoadSpec::NativeRust(_)) => has_native_extensions = true,
            Ok(ExtensionLoadSpec::Js(_)) => has_js_extensions = true,
            #[cfg(feature = "wasm-host")]
            Ok(ExtensionLoadSpec::Wasm(_)) => {}
            Err(err) => {
                return Err(anyhow::Error::new(err));
            }
        }
    }

    if has_js_extensions && has_native_extensions {
        return Err(pi::error::Error::validation(
            "Mixed extension runtimes are not supported in one session yet. Use either JS/TS extensions (QuickJS) or native-rust descriptors (*.native.json), but not both at once."
                .to_string(),
        )
        .into());
    }

    let prewarm_policy = config
        .resolve_extension_policy_with_metadata(cli.extension_policy.as_deref())
        .policy;
    let prewarm_repair = config.resolve_repair_policy_with_metadata(cli.repair_policy.as_deref());
    let prewarm_repair_mode = if prewarm_repair.source.eq("default") {
        pi::extensions::RepairPolicyMode::AutoStrict
    } else {
        prewarm_repair.effective_mode
    };
    let prewarm_memory_limit_bytes =
        (prewarm_policy.max_memory_mb as usize).saturating_mul(1024 * 1024);

    // Pre-warm extension runtime in a background task so startup work can overlap
    // with auth refresh, model selection, and session creation.
    let extension_prewarm_handle = if resources.extensions().is_empty() || has_js_extensions {
        if resources.extensions().is_empty() {
            None
        } else {
            let pre_enabled_tools = cli.enabled_tools();
            let pre_mgr = pi::extensions::ExtensionManager::new();
            pre_mgr.set_cwd(cwd.display().to_string());

            let pre_tools = Arc::new(ToolRegistry::new(&pre_enabled_tools, &cwd, Some(&config)));

            let resolved_risk = config.resolve_extension_risk_with_metadata();
            pre_mgr.set_runtime_risk_config(resolved_risk.settings);

            let pre_mgr_for_runtime = pre_mgr.clone();
            let pre_tools_for_runtime = Arc::clone(&pre_tools);
            let prewarm_policy_for_runtime = prewarm_policy.clone();
            let prewarm_cwd = cwd.display().to_string();
            Some((
                pre_mgr,
                pre_tools,
                runtime_handle.spawn(async move {
                    let mut js_config = PiJsRuntimeConfig {
                        cwd: prewarm_cwd,
                        repair_mode: AgentSession::runtime_repair_mode_from_policy_mode(
                            prewarm_repair_mode,
                        ),
                        ..PiJsRuntimeConfig::default()
                    };
                    js_config.limits.memory_limit_bytes =
                        Some(prewarm_memory_limit_bytes).filter(|bytes| *bytes > 0);
                    let runtime = JsExtensionRuntimeHandle::start_with_policy(
                        js_config,
                        pre_tools_for_runtime,
                        pre_mgr_for_runtime,
                        prewarm_policy_for_runtime,
                    )
                    .await
                    .map(ExtensionRuntimeHandle::Js)
                    .map_err(anyhow::Error::new)?;
                    tracing::info!(
                        event = "pi.extension_runtime.engine_decision",
                        stage = "main_prewarm",
                        requested = "quickjs",
                        selected = "quickjs",
                        fallback = false,
                        "Extension runtime engine selected for prewarm (legacy JS/TS)"
                    );
                    Ok::<ExtensionRuntimeHandle, anyhow::Error>(runtime)
                }),
            ))
        }
    } else {
        let pre_enabled_tools = cli.enabled_tools();
        let pre_mgr = pi::extensions::ExtensionManager::new();
        pre_mgr.set_cwd(cwd.display().to_string());
        let pre_tools = Arc::new(ToolRegistry::new(&pre_enabled_tools, &cwd, Some(&config)));

        let resolved_risk = config.resolve_extension_risk_with_metadata();
        pre_mgr.set_runtime_risk_config(resolved_risk.settings);

        Some((
            pre_mgr,
            pre_tools,
            runtime_handle.spawn(async move {
                let runtime = NativeRustExtensionRuntimeHandle::start()
                    .await
                    .map(ExtensionRuntimeHandle::NativeRust)
                    .map_err(anyhow::Error::new)?;
                tracing::info!(
                    event = "pi.extension_runtime.engine_decision",
                    stage = "main_prewarm",
                    requested = "native-rust",
                    selected = "native-rust",
                    fallback = false,
                    "Extension runtime engine selected for prewarm (native-rust)"
                );
                Ok::<ExtensionRuntimeHandle, anyhow::Error>(runtime)
            }),
        ))
    };

    let mut auth = auth_result?;
    auth.refresh_expired_oauth_tokens().await?;

    // Prune stale credentials that are well past expiry and lack refresh metadata.
    // 7-day cutoff (in milliseconds).
    let pruned = auth.prune_stale_credentials(7 * 24 * 60 * 60 * 1000);
    if !pruned.is_empty() {
        tracing::info!(
            pruned_providers = ?pruned,
            "Pruned stale credentials during startup"
        );
        auth.save()?;
    }

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

    // ACP (Agent Client Protocol) mode — lightweight JSON-RPC 2.0 over stdio
    // for Zed editor integration. Sessions are created on-demand via the protocol
    // so we skip the normal session/model selection pipeline.
    if cli.acp {
        let available_models = model_registry.get_available();
        let acp_options = pi::acp::AcpOptions {
            config: config.clone(),
            available_models,
            model_registry: model_registry.clone(),
            auth: auth.clone(),
            runtime_handle: runtime_handle.clone(),
            session_dir: cli.session_dir.as_ref().map(PathBuf::from),
        };
        return run_acp_mode(acp_options).await;
    }

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
    messages.retain(|message| !message.trim().is_empty());

    let is_interactive = !cli.print && cli.mode.is_none() && cli.export.is_none();
    let mode = cli.mode.clone().unwrap_or_else(|| {
        if is_interactive {
            "interactive".to_string()
        } else {
            "text".to_string()
        }
    });
    let is_print_mode = mode.eq("text") || mode.eq("json");
    if is_print_mode {
        cli.no_session = true;
    }
    if mode.eq("text") && initial.is_none() && messages.is_empty() {
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    let scoped_patterns = if let Some(models_arg) = &cli.models {
        pi::app::parse_models_arg(models_arg)
    } else {
        config.enabled_models.clone().unwrap_or_default()
    };
    let scoped_models = if scoped_patterns.is_empty() {
        Vec::new()
    } else {
        pi::app::resolve_model_scope(
            &scoped_patterns,
            &model_registry,
            has_cli_api_key_override(cli.api_key.as_deref()),
        )
    };
    let has_extensions = !resources.extensions().is_empty();

    if has_cli_api_key_override(cli.api_key.as_deref())
        && cli.provider.is_none()
        && cli.model.is_none()
    {
        let allow_unresolved_scope = has_extensions && !scoped_patterns.is_empty();
        if scoped_models.is_empty() && !allow_unresolved_scope {
            bail!("--api-key requires a model to be specified via --provider/--model or --models");
        }
    }

    let allow_setup_prompt =
        is_interactive && io::stdin().is_terminal() && io::stdout().is_terminal();
    let session = Box::pin(Session::new(&cli, &config)).await?;

    let (mut selection, mut resolved_key) = match resolve_selection_with_auth(
        &mut cli,
        &config,
        &session,
        &mut model_registry,
        &scoped_patterns,
        &mut auth,
        &models_path,
        allow_setup_prompt,
        &[],
    )
    .await
    {
        Ok(Some(result)) => result,
        Ok(None) => return Ok(()),
        Err(err) => {
            if should_retry_selection_after_extensions(&cli, &err, has_extensions) {
                (
                    build_extension_bootstrap_selection(&config, &model_registry, &models_path)?,
                    None,
                )
            } else {
                return Err(err);
            }
        }
    };

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
        !cli.hide_cwd_in_prompt,
    )?;
    let provider =
        providers::create_provider(&selection.model_entry, None).map_err(anyhow::Error::new)?;
    let stream_options =
        pi::app::build_stream_options(&config, resolved_key.clone(), &selection, &session);
    // CLI flag wins; fall back to PI_MAX_TOOL_ITERATIONS env, then default.
    // `clamp_max_tool_iterations` keeps invalid values out of the loop and
    // emits a warning instead of failing the run.
    let max_tool_iterations = if cli.max_tool_iterations.is_some() {
        pi::agent::clamp_max_tool_iterations(cli.max_tool_iterations)
    } else {
        pi::agent::resolved_max_tool_iterations_default()
    };
    let agent_config = AgentConfig {
        system_prompt: Some(system_prompt),
        max_tool_iterations,
        stream_options,
        block_images: config.image_block_images(),
        fail_closed_hooks: config.fail_closed_hooks(),
        tool_approval: None,
    };

    let tools = ToolRegistry::new(&enabled_tools, &cwd, Some(&config));
    let session_arc = Arc::new(Mutex::new(session));
    let compaction_settings = ResolvedCompactionSettings {
        enabled: config.compaction_enabled(),
        reserve_tokens: config.compaction_reserve_tokens(),
        keep_recent_tokens: config.compaction_keep_recent_tokens(),
        context_window_tokens: context_window_tokens_for_entry(&selection.model_entry),
    };
    let mut agent_session = AgentSession::new(
        Agent::new(provider, tools, agent_config),
        session_arc,
        !cli.no_session,
        compaction_settings,
    )
    .with_runtime_handle(runtime_handle.clone());
    agent_session.set_api_key_override(cli.api_key.clone());
    let mut extension_model_entries = Vec::new();

    if !resources.extensions().is_empty() {
        // Await the pre-warmed extension runtime (spawned earlier to overlap with
        // auth refresh, model selection, and session creation).
        let pre_warmed = if let Some((mgr, tools, join_handle)) = extension_prewarm_handle {
            match join_handle.await {
                Ok(runtime) => {
                    tracing::info!(
                        event = "pi.extension_runtime.prewarm.success",
                        runtime = runtime.runtime_name(),
                        "Pre-warmed extension runtime ready"
                    );
                    Some(PreWarmedExtensionRuntime {
                        manager: mgr,
                        runtime,
                        tools,
                    })
                }
                Err(e) => {
                    tracing::warn!(
                        event = "pi.extension_runtime.prewarm.failed",
                        error = %e,
                        "Extension runtime pre-warm failed, falling back to inline creation"
                    );
                    None
                }
            }
        } else {
            None
        };

        let resolved_ext_policy =
            config.resolve_extension_policy_with_metadata(cli.extension_policy.as_deref());
        let resolved_repair_policy =
            config.resolve_repair_policy_with_metadata(cli.repair_policy.as_deref());
        let effective_repair_policy = if resolved_repair_policy.source.eq("default") {
            // Compatibility-first default for extension-heavy workloads:
            // if the user did not choose a repair policy explicitly, prefer
            // aggressive deterministic repairs while capability policy stays enforced.
            pi::extensions::RepairPolicyMode::AutoStrict
        } else {
            resolved_repair_policy.effective_mode
        };
        tracing::info!(
            event = "pi.extension_repair_policy.resolved",
            requested = %resolved_repair_policy.requested_mode,
            source = resolved_repair_policy.source,
            effective = ?effective_repair_policy,
            "Resolved extension repair policy for runtime"
        );
        maybe_print_extension_policy_migration_notice(&resolved_ext_policy);
        agent_session
            .enable_extensions_with_policy(
                &enabled_tools,
                &cwd,
                Some(&config),
                resources.extensions(),
                Some(resolved_ext_policy.policy),
                Some(effective_repair_policy),
                pre_warmed,
            )
            .await
            .map_err(anyhow::Error::new)?;

        if !extension_flags.is_empty() {
            if let Some(region) = &agent_session.extensions {
                apply_extension_cli_flags(region.manager(), &extension_flags).await?;
            } else {
                return Err(pi::error::Error::validation(
                    "Extension flags were provided, but extensions are not active in this session.",
                )
                .into());
            }
        }

        // Merge extension-registered providers into the model registry.
        if let Some(region) = &agent_session.extensions {
            extension_model_entries = region.manager().extension_model_entries();
            if !extension_model_entries.is_empty() {
                // Build OAuth configs map from model entries before merging.
                let ext_oauth_configs: std::collections::HashMap<String, pi::models::OAuthConfig> =
                    extension_model_entries
                        .iter()
                        .filter_map(|entry| {
                            entry
                                .oauth_config
                                .as_ref()
                                .map(|cfg| (entry.model.provider.clone(), cfg.clone()))
                        })
                        .collect();

                model_registry.merge_entries(extension_model_entries.clone());

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

            let discovered = region.manager().discover_resources(&cwd, "startup").await;
            if !discovered.is_empty() {
                if let Err(err) = resources.extend_with_paths(&cwd, &discovered) {
                    tracing::warn!(
                        event = "pi.resources.startup.extension_paths_failed",
                        error = %err,
                        "Failed to apply extension-discovered resource paths"
                    );
                } else {
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
                        test_mode,
                        !cli.hide_cwd_in_prompt,
                    )?;
                    agent_session.agent.set_system_prompt(Some(system_prompt));
                }
            }
        }
    } else if !extension_flags.is_empty() {
        let rendered = extension_flags
            .iter()
            .map(pi::cli::ExtensionCliFlag::display_name)
            .collect::<Vec<_>>()
            .join(", ");
        tracing::debug!(
            event = "pi.extensions.flags.ignored_no_extensions",
            flags = %rendered,
            "Extension flags provided but no extensions are loaded; ignoring."
        );
    }

    if has_extensions {
        let session_snapshot = {
            let cx = pi::agent_cx::AgentCx::for_request();
            let session = agent_session
                .session
                .lock(cx.cx())
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            session.clone()
        };

        let final_selection = resolve_selection_with_auth(
            &mut cli,
            &config,
            &session_snapshot,
            &mut model_registry,
            &scoped_patterns,
            &mut auth,
            &models_path,
            allow_setup_prompt,
            &extension_model_entries,
        )
        .await?;
        let Some((updated_selection, updated_key)) = final_selection else {
            return Ok(());
        };

        selection = updated_selection;
        resolved_key = updated_key;

        let provider = providers::create_provider(
            &selection.model_entry,
            agent_session
                .extensions
                .as_ref()
                .map(ExtensionRegion::manager),
        )
        .map_err(anyhow::Error::new)?;
        agent_session.agent.set_provider(provider);
        {
            let stream_options = agent_session.agent.stream_options_mut();
            stream_options.api_key.clone_from(&resolved_key);
            stream_options
                .headers
                .clone_from(&selection.model_entry.headers);
            stream_options.thinking_level = Some(selection.thinking_level);
        }
        agent_session
            .set_compaction_context_window(context_window_tokens_for_entry(&selection.model_entry));
        agent_session.refresh_extension_completion_host_state();
    }

    {
        let cx = pi::agent_cx::AgentCx::for_request();
        let mut session = agent_session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        pi::app::update_session_for_selection(&mut session, &selection);
    }

    if let Some(message) = &selection.fallback_message {
        eprintln!("Warning: {message}");
    }

    agent_session.set_model_registry(model_registry.clone());
    agent_session.set_auth_storage(auth.clone());

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

    // Clone session handle for shutdown flush (ensures autosave queue is drained).
    let session_handle = Arc::clone(&agent_session.session);

    let result = if mode.eq("rpc") {
        let available_models = rpc_available_models(&model_registry, cli.api_key.as_deref());
        let rpc_scoped_models = selection
            .scoped_models
            .iter()
            .map(|sm| pi::rpc::RpcScopedModel {
                model: sm.model.clone(),
                thinking_level: sm.thinking_level,
            })
            .collect::<Vec<_>>();
        // Boxed: this future is large (clippy::large_futures); boxing keeps the
        // enclosing future small.
        Box::pin(run_rpc_mode(
            agent_session,
            resources,
            config.clone(),
            available_models,
            rpc_scoped_models,
            cli.api_key.clone(),
            auth.clone(),
            runtime_handle.clone(),
        ))
        .await
    } else if is_interactive {
        let model_scope = selection
            .scoped_models
            .iter()
            .map(|sm| sm.model.clone())
            .collect::<Vec<_>>();
        let available_models = model_registry.get_available();

        run_interactive_mode(
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
        .await
    } else {
        let result = run_print_mode(
            &mut agent_session,
            &mode,
            initial,
            messages,
            &resources,
            runtime_handle.clone(),
            &config,
        )
        .await;
        // Explicitly shut down extension runtimes before the session drops.
        // Without this, ExtensionRegion::drop() runs synchronously and cannot
        // coordinate with the QuickJS runtime thread, causing a GC assertion
        // failure (non-empty gc_obj_list) when 2+ JS extensions are loaded.
        if let Some(ref ext) = agent_session.extensions {
            ext.shutdown().await;
        }
        result
    };

    // Best-effort autosave flush on shutdown. OwnedMutexGuard: the guard is
    // held across the flush await, and the borrowed MutexGuard is !Send
    // (clippy::future_not_send).
    if !cli.no_session {
        let cx = pi::agent_cx::AgentCx::for_request();
        if let Ok(mut guard) = OwnedMutexGuard::lock(Arc::clone(&session_handle), &cx).await
            && let Err(e) = guard.flush_autosave_on_shutdown().await
        {
            eprintln!("Warning: Failed to flush session autosave: {e}");
        }
    }

    result
}

#[allow(clippy::too_many_lines)]
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
        cli::Commands::ContextPreview {
            format,
            bead,
            changed_paths,
            failing_command,
            max_items,
            max_bytes,
            query,
        } => {
            handle_context_preview_blocking(
                cwd,
                &format,
                bead.as_deref(),
                &changed_paths,
                failing_command.as_deref(),
                max_items,
                max_bytes,
                &query,
            )?;
        }
        cli::Commands::SwarmProgress {
            input,
            since,
            format,
            out_json,
            out_text,
        } => {
            handle_swarm_progress_blocking(
                cwd,
                &input,
                since.as_deref(),
                &format,
                out_json.as_deref(),
                out_text.as_deref(),
            )?;
        }
        cli::Commands::SwarmReplayPreview {
            trace,
            policies,
            format,
            out_json,
            out_text,
            generated_at,
        } => {
            handle_swarm_replay_preview_blocking(
                cwd,
                &trace,
                &policies,
                &format,
                out_json.as_deref(),
                out_text.as_deref(),
                generated_at.as_deref(),
            )?;
        }
        cli::Commands::ValidationBroker { command } => {
            handle_validation_broker_blocking(cwd, &command)?;
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
            handle_info_blocking(&name)?;
        }
        cli::Commands::List => {
            handle_package_list(&manager).await?;
        }
        cli::Commands::Config { show, paths, json } => {
            handle_config(&manager, cwd, show, paths, json).await?;
        }
        cli::Commands::Doctor {
            path,
            format,
            policy,
            fix,
            only,
        } => {
            handle_doctor(
                cwd,
                path.as_deref(),
                &format,
                policy.as_deref(),
                fix,
                only.as_deref(),
            )?;
        }
        cli::Commands::Migrate { path, dry_run } => {
            handle_session_migrate(&path, dry_run)?;
        }
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct ValidationBrokerCommandReport {
    name: &'static str,
    action: String,
    cwd: String,
    store: String,
    output_writes: u8,
}

#[derive(Debug, Serialize)]
struct ValidationBrokerOutputPaths {
    json: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct ValidationBrokerGuards {
    read_only_plan: bool,
    live_mutations: u8,
    refuses_output_overwrite: bool,
    destructive_actions: u8,
    provider_calls: u8,
}

#[derive(Debug, Serialize)]
struct ValidationBrokerStoreSummary {
    path: String,
    schema: String,
    status: String,
    total_records: usize,
    total_slots: usize,
    active_slots: usize,
    reusable_slots: usize,
    stale_slots: usize,
    expired_at_report_time_slots: usize,
    state_counts: BTreeMap<String, usize>,
    degraded_reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ValidationBrokerStatusReport {
    schema: &'static str,
    generated_at_utc: String,
    command: ValidationBrokerCommandReport,
    store: ValidationBrokerStoreSummary,
    output_paths: ValidationBrokerOutputPaths,
    guards: ValidationBrokerGuards,
}

#[derive(Debug, Serialize)]
struct ValidationBrokerPlanReport {
    schema: &'static str,
    generated_at_utc: String,
    command: ValidationBrokerCommandReport,
    request_id: String,
    bead_id: String,
    read_only: bool,
    next_action: &'static str,
    decision: ValidationAdmissionDecisionRecord,
    store: ValidationBrokerStoreSummary,
    output_paths: ValidationBrokerOutputPaths,
    guards: ValidationBrokerGuards,
}

#[derive(Debug, Serialize)]
struct ValidationBrokerLeaseMutationReport {
    schema: &'static str,
    generated_at_utc: String,
    command: ValidationBrokerCommandReport,
    event: &'static str,
    lease: ValidationSlotLease,
    store: ValidationBrokerStoreSummary,
    output_paths: ValidationBrokerOutputPaths,
    guards: ValidationBrokerGuards,
}

#[allow(clippy::too_many_lines)]
fn handle_validation_broker_blocking(
    cwd: &Path,
    command: &cli::ValidationBrokerCommand,
) -> Result<()> {
    match command {
        cli::ValidationBrokerCommand::Status {
            store,
            format,
            out_json,
            out_text,
            generated_at,
        } => {
            let generated_at_utc = validation_broker_generated_at(
                "validation-broker status",
                generated_at.as_deref(),
            )?;
            let store_path = resolve_cli_path(cwd, store);
            let slot_store = ValidationSlotStore::new(&store_path);
            let snapshot = slot_store.load_snapshot();
            let output_paths =
                validation_broker_output_paths(out_json.as_deref(), out_text.as_deref());
            let report = ValidationBrokerStatusReport {
                schema: VALIDATION_BROKER_CLI_STATUS_SCHEMA,
                generated_at_utc: generated_at_utc.clone(),
                command: validation_broker_command_report(
                    cwd,
                    "status",
                    store,
                    output_paths.output_writes(),
                ),
                store: validation_store_summary(&store_path, &snapshot, &generated_at_utc),
                output_paths,
                guards: validation_broker_guards(true, 0),
            };
            emit_validation_broker_status(
                cwd,
                &report,
                format,
                out_json.as_deref(),
                out_text.as_deref(),
            )?;
        }
        cli::ValidationBrokerCommand::Plan {
            request,
            inputs,
            store,
            policy,
            format,
            out_json,
            out_text,
            generated_at,
        } => {
            let generated_at_utc =
                validation_broker_generated_at("validation-broker plan", generated_at.as_deref())?;
            let request_path = resolve_cli_path(cwd, request);
            let inputs_path = resolve_cli_path(cwd, inputs);
            let context =
                read_validation_broker_json::<ValidationAdmissionRequestContext>(&request_path)?;
            let input_snapshot =
                read_validation_broker_json::<ValidationBrokerInputSnapshot>(&inputs_path)?;
            if input_snapshot.schema != VALIDATION_BROKER_INPUT_SCHEMA {
                return Err(validation_broker_validation_error(format!(
                    "validation-broker plan requires inputs schema {VALIDATION_BROKER_INPUT_SCHEMA}, got {}",
                    input_snapshot.schema
                )));
            }
            let policy = match policy {
                Some(path) => read_validation_broker_json::<ValidationAdmissionPolicy>(
                    &resolve_cli_path(cwd, path),
                )?,
                None => ValidationAdmissionPolicy::default(),
            };
            let store_path = resolve_cli_path(cwd, store);
            let slot_store = ValidationSlotStore::new(&store_path);
            let snapshot = slot_store.load_snapshot();
            let decision = decide_validation_admission(
                context.clone(),
                &input_snapshot,
                &snapshot,
                &policy,
                &generated_at_utc,
            )?;
            if decision.schema != VALIDATION_BROKER_DECISION_SCHEMA {
                return Err(validation_broker_validation_error(format!(
                    "validation-broker plan produced unexpected decision schema {}",
                    decision.schema
                )));
            }
            let output_paths =
                validation_broker_output_paths(out_json.as_deref(), out_text.as_deref());
            let next_action = validation_broker_next_action(&decision.decision);
            let report = ValidationBrokerPlanReport {
                schema: VALIDATION_BROKER_CLI_PLAN_SCHEMA,
                generated_at_utc: generated_at_utc.clone(),
                command: validation_broker_command_report(
                    cwd,
                    "plan",
                    store,
                    output_paths.output_writes(),
                ),
                request_id: context.request_id,
                bead_id: context.request.bead_id,
                read_only: true,
                next_action,
                decision,
                store: validation_store_summary(&store_path, &snapshot, &generated_at_utc),
                output_paths,
                guards: validation_broker_guards(true, 0),
            };
            emit_validation_broker_plan(
                cwd,
                &report,
                format,
                out_json.as_deref(),
                out_text.as_deref(),
            )?;
        }
        cli::ValidationBrokerCommand::Acquire {
            request,
            store,
            started_at,
            expires_at,
            format,
            out_json,
            out_text,
        } => {
            let request_path = resolve_cli_path(cwd, request);
            let request = read_validation_broker_json::<ValidationSlotRequest>(&request_path)?;
            let lease =
                ValidationSlotLease::acquire(request, started_at.clone(), expires_at.clone())?;
            let store_path = resolve_cli_path(cwd, store);
            let slot_store = ValidationSlotStore::new(&store_path);
            let snapshot = slot_store.load_snapshot();
            ensure_validation_store_mutable(&snapshot)?;
            if snapshot.latest_by_slot_id.contains_key(&lease.slot_id) {
                return Err(validation_broker_validation_error(format!(
                    "validation-broker acquire refuses duplicate slot_id {}",
                    lease.slot_id
                )));
            }
            slot_store.append_lease("acquired", started_at.clone(), &lease)?;
            let updated = slot_store.load_snapshot();
            emit_validation_broker_lease_mutation(
                cwd,
                "acquire",
                "acquired",
                store,
                &store_path,
                &updated,
                lease,
                started_at,
                format,
                out_json.as_deref(),
                out_text.as_deref(),
            )?;
        }
        cli::ValidationBrokerCommand::Renew {
            store,
            slot_id,
            owner,
            heartbeat_at,
            expires_at,
            format,
            out_json,
            out_text,
        } => {
            let store_path = resolve_cli_path(cwd, store);
            let slot_store = ValidationSlotStore::new(&store_path);
            let snapshot = slot_store.load_snapshot();
            ensure_validation_store_mutable(&snapshot)?;
            let mut lease = validation_broker_latest_lease(&snapshot, slot_id)?;
            lease.renew(owner, heartbeat_at.clone(), expires_at.clone())?;
            slot_store.append_lease("renewed", heartbeat_at.clone(), &lease)?;
            let updated = slot_store.load_snapshot();
            emit_validation_broker_lease_mutation(
                cwd,
                "renew",
                "renewed",
                store,
                &store_path,
                &updated,
                lease,
                heartbeat_at,
                format,
                out_json.as_deref(),
                out_text.as_deref(),
            )?;
        }
        cli::ValidationBrokerCommand::Release {
            store,
            slot_id,
            owner,
            at,
            reason,
            format,
            out_json,
            out_text,
        } => {
            let store_path = resolve_cli_path(cwd, store);
            let slot_store = ValidationSlotStore::new(&store_path);
            let snapshot = slot_store.load_snapshot();
            ensure_validation_store_mutable(&snapshot)?;
            let mut lease = validation_broker_latest_lease(&snapshot, slot_id)?;
            lease.release(owner, at.clone(), reason.clone())?;
            slot_store.append_lease("released", at.clone(), &lease)?;
            let updated = slot_store.load_snapshot();
            emit_validation_broker_lease_mutation(
                cwd,
                "release",
                "released",
                store,
                &store_path,
                &updated,
                lease,
                at,
                format,
                out_json.as_deref(),
                out_text.as_deref(),
            )?;
        }
    }

    Ok(())
}

impl ValidationBrokerOutputPaths {
    fn output_writes(&self) -> u8 {
        u8::from(self.json.is_some()) + u8::from(self.text.is_some())
    }
}

fn validation_broker_output_paths(
    out_json: Option<&str>,
    out_text: Option<&str>,
) -> ValidationBrokerOutputPaths {
    ValidationBrokerOutputPaths {
        json: out_json.map(ToOwned::to_owned),
        text: out_text.map(ToOwned::to_owned),
    }
}

const fn validation_broker_guards(
    read_only_plan: bool,
    live_mutations: u8,
) -> ValidationBrokerGuards {
    ValidationBrokerGuards {
        read_only_plan,
        live_mutations,
        refuses_output_overwrite: true,
        destructive_actions: 0,
        provider_calls: 0,
    }
}

fn validation_broker_command_report(
    cwd: &Path,
    action: impl Into<String>,
    store: &str,
    output_writes: u8,
) -> ValidationBrokerCommandReport {
    ValidationBrokerCommandReport {
        name: "validation-broker",
        action: action.into(),
        cwd: cwd.display().to_string(),
        store: store.to_string(),
        output_writes,
    }
}

fn read_validation_broker_json<T>(path: &Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let raw = fs::read_to_string(path)?;
    serde_json::from_str(&raw).map_err(Into::into)
}

fn validation_store_summary(
    path: &Path,
    snapshot: &ValidationSlotStoreSnapshot,
    now_utc: &str,
) -> ValidationBrokerStoreSummary {
    let mut state_counts = BTreeMap::new();
    let mut active_slots = 0usize;
    let mut reusable_slots = 0usize;
    let mut stale_slots = 0usize;
    let mut expired_at_report_time_slots = 0usize;
    for lease in snapshot.latest_by_slot_id.values() {
        let state_key = validation_slot_state_key(&lease.state);
        *state_counts.entry(state_key.to_string()).or_insert(0) += 1;
        match lease.state {
            ValidationSlotState::Requested | ValidationSlotState::Active => active_slots += 1,
            ValidationSlotState::Reusable => reusable_slots += 1,
            ValidationSlotState::Stale => stale_slots += 1,
            ValidationSlotState::Failed
            | ValidationSlotState::Released
            | ValidationSlotState::Expired
            | ValidationSlotState::Degraded => {}
        }
        if lease.is_stale_at(now_utc).unwrap_or(false) {
            expired_at_report_time_slots += 1;
        }
    }

    ValidationBrokerStoreSummary {
        path: path.display().to_string(),
        schema: snapshot.schema.clone(),
        status: format!("{:?}", snapshot.status).to_ascii_lowercase(),
        total_records: snapshot.leases.len(),
        total_slots: snapshot.latest_by_slot_id.len(),
        active_slots,
        reusable_slots,
        stale_slots,
        expired_at_report_time_slots,
        state_counts,
        degraded_reasons: snapshot.degraded_reasons.clone(),
    }
}

const fn validation_slot_state_key(state: &ValidationSlotState) -> &'static str {
    match state {
        ValidationSlotState::Requested => "requested",
        ValidationSlotState::Active => "active",
        ValidationSlotState::Reusable => "reusable",
        ValidationSlotState::Stale => "stale",
        ValidationSlotState::Failed => "failed",
        ValidationSlotState::Released => "released",
        ValidationSlotState::Expired => "expired",
        ValidationSlotState::Degraded => "degraded",
    }
}

const fn validation_decision_key(decision: &ValidationAdmissionDecision) -> &'static str {
    match decision {
        ValidationAdmissionDecision::Allow => "allow",
        ValidationAdmissionDecision::Wait => "wait",
        ValidationAdmissionDecision::Coalesce => "coalesce",
        ValidationAdmissionDecision::Narrow => "narrow",
        ValidationAdmissionDecision::DenyLocalFallback => "deny_local_fallback",
        ValidationAdmissionDecision::StaleRecover => "stale_recover",
        ValidationAdmissionDecision::DegradedBlock => "degraded_block",
    }
}

const fn validation_broker_next_action(decision: &ValidationAdmissionDecision) -> &'static str {
    match decision {
        ValidationAdmissionDecision::Allow => "run_now",
        ValidationAdmissionDecision::Wait => "wait",
        ValidationAdmissionDecision::Coalesce => "coalesce_with_reusable_slot",
        ValidationAdmissionDecision::Narrow => "narrow_scope",
        ValidationAdmissionDecision::DenyLocalFallback
        | ValidationAdmissionDecision::DegradedBlock => "surface_blocker",
        ValidationAdmissionDecision::StaleRecover => "recover_stale_slot_or_bead",
    }
}

fn validation_broker_generated_at(label: &str, generated_at: Option<&str>) -> Result<String> {
    let Some(value) = generated_at.and_then(non_empty_string) else {
        return Ok(chrono::Utc::now().to_rfc3339());
    };
    match chrono::DateTime::parse_from_rfc3339(&value) {
        Ok(parsed) if parsed.offset().local_minus_utc() == 0 => {}
        Ok(_) => {
            return Err(validation_broker_validation_error(format!(
                "{label} requires --generated-at to use UTC offset: {value}"
            )));
        }
        Err(_) => {
            return Err(validation_broker_validation_error(format!(
                "{label} requires --generated-at to be RFC3339: {value}"
            )));
        }
    }
    Ok(value)
}

fn ensure_validation_store_mutable(snapshot: &ValidationSlotStoreSnapshot) -> Result<()> {
    if snapshot.is_degraded() {
        Err(validation_broker_validation_error(format!(
            "refusing to mutate degraded validation slot store: {}",
            snapshot.degraded_reasons.join("; ")
        )))
    } else {
        Ok(())
    }
}

fn validation_broker_latest_lease(
    snapshot: &ValidationSlotStoreSnapshot,
    slot_id: &str,
) -> Result<ValidationSlotLease> {
    snapshot
        .latest_by_slot_id
        .get(slot_id)
        .cloned()
        .ok_or_else(|| {
            validation_broker_validation_error(format!(
                "validation-broker slot_id {slot_id} not found"
            ))
        })
}

fn validation_broker_validation_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(pi::error::Error::validation(message.into()))
}

fn emit_validation_broker_status(
    cwd: &Path,
    report: &ValidationBrokerStatusReport,
    format: &str,
    out_json: Option<&str>,
    out_text: Option<&str>,
) -> Result<()> {
    let json_output = serde_json::to_string_pretty(report)?;
    let text_output = render_validation_broker_status_text(report);
    emit_validation_broker_output(cwd, &json_output, &text_output, format, out_json, out_text)
}

fn emit_validation_broker_plan(
    cwd: &Path,
    report: &ValidationBrokerPlanReport,
    format: &str,
    out_json: Option<&str>,
    out_text: Option<&str>,
) -> Result<()> {
    let json_output = serde_json::to_string_pretty(report)?;
    let text_output = render_validation_broker_plan_text(report);
    emit_validation_broker_output(cwd, &json_output, &text_output, format, out_json, out_text)
}

#[allow(clippy::too_many_arguments)]
fn emit_validation_broker_lease_mutation(
    cwd: &Path,
    action: &str,
    event: &'static str,
    store_arg: &str,
    store_path: &Path,
    snapshot: &ValidationSlotStoreSnapshot,
    lease: ValidationSlotLease,
    generated_at_utc: &str,
    format: &str,
    out_json: Option<&str>,
    out_text: Option<&str>,
) -> Result<()> {
    let output_paths = validation_broker_output_paths(out_json, out_text);
    let report = ValidationBrokerLeaseMutationReport {
        schema: VALIDATION_BROKER_CLI_LEASE_MUTATION_SCHEMA,
        generated_at_utc: generated_at_utc.to_string(),
        command: validation_broker_command_report(
            cwd,
            action,
            store_arg,
            output_paths.output_writes(),
        ),
        event,
        lease,
        store: validation_store_summary(store_path, snapshot, generated_at_utc),
        output_paths,
        guards: validation_broker_guards(false, 1),
    };
    let json_output = serde_json::to_string_pretty(&report)?;
    let text_output = render_validation_broker_lease_text(&report);
    emit_validation_broker_output(cwd, &json_output, &text_output, format, out_json, out_text)
}

fn emit_validation_broker_output(
    cwd: &Path,
    json_output: &str,
    text_output: &str,
    format: &str,
    out_json: Option<&str>,
    out_text: Option<&str>,
) -> Result<()> {
    if let Some(path) = out_json {
        write_validation_broker_output(&resolve_cli_path(cwd, path), json_output, "JSON output")?;
    }
    if let Some(path) = out_text {
        write_validation_broker_output(&resolve_cli_path(cwd, path), text_output, "text output")?;
    }
    if out_json.is_none() && out_text.is_none() {
        match format {
            "json" => println!("{json_output}"),
            "text" => print!("{text_output}"),
            other => {
                return Err(validation_broker_validation_error(format!(
                    "unsupported validation-broker format: {other}"
                )));
            }
        }
    }
    Ok(())
}

fn write_validation_broker_output(path: &Path, content: &str, label: &str) -> Result<()> {
    if path.exists() {
        return Err(validation_broker_validation_error(format!(
            "refusing to overwrite existing validation-broker {label}: {}",
            path.display()
        )));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn render_validation_broker_status_text(report: &ValidationBrokerStatusReport) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Validation Broker Status");
    let _ = writeln!(output, "schema: {}", report.schema);
    let _ = writeln!(output, "generated_at_utc: {}", report.generated_at_utc);
    push_validation_store_summary_text(&mut output, &report.store);
    push_validation_list(
        &mut output,
        "degraded_reasons",
        &report.store.degraded_reasons,
    );
    output
}

fn render_validation_broker_plan_text(report: &ValidationBrokerPlanReport) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Validation Broker Plan");
    let _ = writeln!(output, "schema: {}", report.schema);
    let _ = writeln!(output, "generated_at_utc: {}", report.generated_at_utc);
    let _ = writeln!(output, "read_only: {}", report.read_only);
    let _ = writeln!(output, "request_id: {}", report.request_id);
    let _ = writeln!(output, "bead_id: {}", report.bead_id);
    let _ = writeln!(
        output,
        "decision: {}",
        validation_decision_key(&report.decision.decision)
    );
    let _ = writeln!(output, "next_action: {}", report.next_action);
    let _ = writeln!(output, "confidence: {}", report.decision.confidence);
    push_validation_list(&mut output, "reasons", &report.decision.reasons);
    push_validation_list(
        &mut output,
        "required_actions",
        &report.decision.required_actions,
    );
    push_validation_list(&mut output, "no_claims", &report.decision.no_claims);
    push_validation_store_summary_text(&mut output, &report.store);
    output
}

fn render_validation_broker_lease_text(report: &ValidationBrokerLeaseMutationReport) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Validation Broker Lease");
    let _ = writeln!(output, "schema: {}", report.schema);
    let _ = writeln!(output, "generated_at_utc: {}", report.generated_at_utc);
    let _ = writeln!(output, "event: {}", report.event);
    let _ = writeln!(output, "slot_id: {}", report.lease.slot_id);
    let _ = writeln!(
        output,
        "state: {}",
        validation_slot_state_key(&report.lease.state)
    );
    let _ = writeln!(output, "owner_agent: {}", report.lease.owner_agent);
    let _ = writeln!(output, "bead_id: {}", report.lease.bead_id);
    push_validation_store_summary_text(&mut output, &report.store);
    output
}

fn push_validation_store_summary_text(output: &mut String, store: &ValidationBrokerStoreSummary) {
    let _ = writeln!(output, "store: {}", store.path);
    let _ = writeln!(output, "store_status: {}", store.status);
    let _ = writeln!(output, "total_records: {}", store.total_records);
    let _ = writeln!(output, "total_slots: {}", store.total_slots);
    let _ = writeln!(output, "active_slots: {}", store.active_slots);
    let _ = writeln!(output, "reusable_slots: {}", store.reusable_slots);
    let _ = writeln!(output, "stale_slots: {}", store.stale_slots);
    let _ = writeln!(
        output,
        "expired_at_report_time_slots: {}",
        store.expired_at_report_time_slots
    );
}

fn push_validation_list(output: &mut String, label: &str, values: &[String]) {
    let _ = writeln!(output, "{label}:");
    if values.is_empty() {
        let _ = writeln!(output, "- none");
    } else {
        for value in values {
            let _ = writeln!(output, "- {value}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_swarm_progress_blocking(
    cwd: &Path,
    input: &str,
    since: Option<&str>,
    format: &str,
    out_json: Option<&str>,
    out_text: Option<&str>,
) -> Result<()> {
    let input = read_swarm_progress_input(cwd, input)?;
    validate_swarm_progress_since(&input, since)?;
    let report = evaluate_progress_slo(input);
    if report.schema != SWARM_PROGRESS_SLO_SCHEMA {
        bail!(
            "swarm-progress produced unexpected schema {}, expected {SWARM_PROGRESS_SLO_SCHEMA}",
            report.schema
        );
    }
    emit_swarm_progress_output(cwd, &report, format, out_json, out_text)
}

fn read_swarm_progress_input(cwd: &Path, input: &str) -> Result<ProgressSloEvaluationInput> {
    let input_arg = non_empty_string(input)
        .ok_or_else(|| anyhow::anyhow!("swarm-progress requires --input"))?;
    let input_path = resolve_cli_path(cwd, &input_arg);
    let raw = fs::read_to_string(&input_path).map_err(|err| {
        anyhow::anyhow!(
            "swarm-progress failed to read --input {}: {err}",
            input_path.display()
        )
    })?;
    serde_json::from_str::<ProgressSloEvaluationInput>(&raw).map_err(|err| {
        anyhow::anyhow!(
            "swarm-progress requires normalized progress SLO input JSON at {}: {err}",
            input_path.display()
        )
    })
}

fn validate_swarm_progress_since(
    input: &ProgressSloEvaluationInput,
    since: Option<&str>,
) -> Result<()> {
    let Some(since) = since else {
        return Ok(());
    };
    let Some(since) = non_empty_string(since) else {
        bail!("swarm-progress requires non-empty --since values");
    };
    if input.time_window.comparison_baseline != since {
        bail!(
            "swarm-progress --since {since} does not match input time_window.comparison_baseline {}",
            input.time_window.comparison_baseline
        );
    }
    Ok(())
}

fn emit_swarm_progress_output(
    cwd: &Path,
    report: &ProgressSloReport,
    format: &str,
    out_json: Option<&str>,
    out_text: Option<&str>,
) -> Result<()> {
    let json_output = serde_json::to_string_pretty(report)?;
    let text_output = render_swarm_progress_text(report);
    if let Some(path) = out_json {
        write_swarm_progress_output(&resolve_cli_path(cwd, path), &json_output, "JSON output")?;
    }
    if let Some(path) = out_text {
        write_swarm_progress_output(&resolve_cli_path(cwd, path), &text_output, "text output")?;
    }
    if out_json.is_none() && out_text.is_none() {
        match format {
            "json" => println!("{json_output}"),
            "text" => print!("{text_output}"),
            other => bail!("unsupported swarm-progress format: {other}"),
        }
    }
    Ok(())
}

fn write_swarm_progress_output(path: &Path, content: &str, label: &str) -> Result<()> {
    if path.exists() {
        bail!(
            "refusing to overwrite existing swarm-progress {label}: {}",
            path.display()
        );
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn render_swarm_progress_text(report: &ProgressSloReport) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Swarm Progress SLO");
    let _ = writeln!(output, "schema: {}", report.schema);
    let _ = writeln!(output, "generated_at: {}", report.generated_at);
    let _ = writeln!(
        output,
        "status: {}",
        swarm_progress_json_key(&report.status)
    );
    let _ = writeln!(output, "confidence: {:.3}", report.confidence);
    let _ = writeln!(
        output,
        "window: {} -> {} ({}s, baseline={})",
        report.time_window.start_utc,
        report.time_window.end_utc,
        report.time_window.duration_seconds,
        report.time_window.comparison_baseline
    );
    let _ = writeln!(output, "advisory_only: true");
    let _ = writeln!(output, "read_only: true");
    let _ = writeln!(output, "live_mutations: 0");
    let _ = writeln!(
        output,
        "authority_boundary: no live Beads/git/Agent Mail/RCH mutations"
    );
    push_swarm_progress_list(&mut output, "reasons", &report.reason_ids);
    push_swarm_progress_metrics(&mut output, report);
    push_swarm_progress_saturation(&mut output, report);
    push_swarm_progress_list(&mut output, "next_actions", &report.next_actions);
    push_swarm_progress_list(&mut output, "suppressed_claims", &report.suppressed_claims);
    let _ = writeln!(output, "source_statuses: {}", report.source_statuses.len());
    output
}

fn push_swarm_progress_metrics(output: &mut String, report: &ProgressSloReport) {
    let metrics = &report.progress_metrics;
    let _ = writeln!(output, "metrics:");
    let _ = writeln!(output, "- closed_beads: {}", metrics.closed_beads);
    let _ = writeln!(output, "- open_beads: {}", metrics.open_beads);
    let _ = writeln!(output, "- in_progress_beads: {}", metrics.in_progress_beads);
    let _ = writeln!(output, "- ready_beads: {}", metrics.ready_beads);
    let _ = writeln!(
        output,
        "- dependency_blocked_beads: {}",
        metrics.dependency_blocked_beads
    );
    let _ = writeln!(output, "- commits: {}", metrics.commits);
    let _ = writeln!(output, "- pushed_commits: {}", metrics.pushed_commits);
    let _ = writeln!(
        output,
        "- stale_in_progress_candidates: {}",
        metrics.stale_in_progress_candidates
    );
    let _ = writeln!(
        output,
        "- agent_mail_health: {}",
        swarm_progress_json_key(&metrics.agent_mail_health)
    );
    let _ = writeln!(
        output,
        "- rch_posture: {}",
        swarm_progress_json_key(&metrics.rch_posture)
    );
    let _ = writeln!(
        output,
        "- validation_broker_posture: {}",
        swarm_progress_json_key(&metrics.validation_broker_posture)
    );
}

fn push_swarm_progress_saturation(output: &mut String, report: &ProgressSloReport) {
    let saturation = &report.saturation_summary;
    let _ = writeln!(output, "saturation:");
    let _ = writeln!(
        output,
        "- coordination_saturation: {}",
        swarm_progress_json_key(&saturation.coordination_saturation)
    );
    let _ = writeln!(
        output,
        "- build_saturation: {}",
        swarm_progress_json_key(&saturation.build_saturation)
    );
    let _ = writeln!(
        output,
        "- validation_saturation: {}",
        swarm_progress_json_key(&saturation.validation_saturation)
    );
    let _ = writeln!(
        output,
        "- queue_convergence: {}",
        swarm_progress_json_key(&saturation.queue_convergence)
    );
    let _ = writeln!(
        output,
        "- recommended_operator_posture: {}",
        swarm_progress_json_key(&saturation.recommended_operator_posture)
    );
}

fn push_swarm_progress_list(output: &mut String, label: &str, values: &[String]) {
    let _ = writeln!(output, "{label}:");
    if values.is_empty() {
        let _ = writeln!(output, "- none");
    } else {
        for value in values {
            let _ = writeln!(output, "- {value}");
        }
    }
}

fn swarm_progress_json_key(value: &impl Serialize) -> String {
    serde_json::to_string(value).map_or_else(
        |_| "unknown".to_string(),
        |raw| raw.trim_matches('"').to_string(),
    )
}

const SWARM_REPLAY_PREVIEW_SCHEMA: &str = "pi.swarm.replay_preview.v1";

#[derive(Debug, Serialize)]
struct SwarmReplayPreviewReport<'a> {
    schema: &'static str,
    generated_at_utc: String,
    command: SwarmReplayPreviewCommand,
    trace: SwarmReplayPreviewTraceSummary,
    replay: SwarmReplayPreviewReplaySummary,
    policies: SwarmReplayPreviewPolicySection<'a>,
    recommendation: Option<SwarmReplayPreviewPolicySummary<'a>>,
    output_paths: SwarmReplayPreviewOutputPaths,
    guards: SwarmReplayPreviewGuards,
}

#[derive(Debug, Serialize)]
struct SwarmReplayPreviewCommand {
    invocation: &'static str,
    cwd: String,
    read_only_replay: bool,
    provider_calls: u8,
    live_mutations: u8,
    output_writes: u8,
}

#[derive(Debug, Serialize)]
struct SwarmReplayPreviewTraceSummary {
    path: String,
    schema: String,
    trace_id: String,
    generated_at: String,
    source_count: u64,
    event_count: u64,
    first_event_id: Option<String>,
    last_event_id: Option<String>,
    redaction_status: String,
    uncertainty_state: String,
}

#[derive(Debug, Serialize)]
struct SwarmReplayPreviewReplaySummary {
    schema: &'static str,
    replayed_event_count: u64,
    final_logical_clock: u64,
    snapshot_count: u64,
    diagnostic_count: u64,
    diagnostics: Vec<SwarmReplayPreviewDiagnosticSummary>,
    final_state: SwarmReplayPreviewStateSummary,
    resource_saturation_points: u64,
    first_saturation_reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SwarmReplayPreviewDiagnosticSummary {
    code: String,
    severity: String,
    event_id: Option<String>,
    message: String,
}

#[derive(Debug, Serialize)]
struct SwarmReplayPreviewStateSummary {
    bead_count: u64,
    agent_count: u64,
    active_reservation_count: u64,
    active_build_slot_count: u64,
    rch_job_count: u64,
    validation_gate_count: u64,
    runpack_recommendation_count: u64,
    operator_handoff_count: u64,
    reservation_conflict_count: u64,
    agent_mail_available: bool,
    missing_agent_mail_evidence: bool,
    dirty_worktree: Option<bool>,
}

#[derive(Debug, Serialize)]
struct SwarmReplayPreviewPolicySection<'a> {
    schema: &'static str,
    requested_policy_ids: Vec<String>,
    evaluated_policy_ids: Vec<String>,
    decision_count: u64,
    comparison_count: u64,
    distinct_action_count: u64,
    score_spread: Option<i64>,
    comparisons: Vec<SwarmReplayPreviewPolicySummary<'a>>,
}

#[derive(Debug, Clone, Serialize)]
struct SwarmReplayPreviewPolicySummary<'a> {
    policy_id: &'a str,
    rank: u64,
    score: i64,
    confidence: &'a str,
    confidence_score: u64,
    throughput_actions: u64,
    validation_commands_deferred: u64,
    local_fallback_risk: &'a str,
    reservation_conflicts_avoided: u64,
    stale_work_reclaimed: u64,
    missing_data_claims: Vec<&'a str>,
    rationale: Vec<&'a str>,
}

#[derive(Debug, Serialize)]
struct SwarmReplayPreviewOutputPaths {
    json: Option<String>,
    text: Option<String>,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize)]
struct SwarmReplayPreviewGuards {
    read_only_replay: bool,
    no_live_mutation: bool,
    no_network_required: bool,
    output_artifacts_only: bool,
    runpack_not_source_of_truth: bool,
}

#[allow(clippy::too_many_arguments)]
fn handle_swarm_replay_preview_blocking(
    cwd: &Path,
    trace: &str,
    policy_names: &[String],
    format: &str,
    out_json: Option<&str>,
    out_text: Option<&str>,
    generated_at: Option<&str>,
) -> Result<()> {
    let trace_arg = non_empty_string(trace)
        .ok_or_else(|| anyhow::anyhow!("swarm-replay-preview requires --trace"))?;
    let trace_path = resolve_cli_path(cwd, &trace_arg);
    let trace_raw = fs::read_to_string(&trace_path)?;
    let trace = serde_json::from_str::<SwarmReplayTrace>(&trace_raw)?;
    if trace.schema != SWARM_REPLAY_TRACE_SCHEMA {
        bail!(
            "swarm-replay-preview requires trace schema {SWARM_REPLAY_TRACE_SCHEMA}, got {}",
            trace.schema
        );
    }
    let selected_policies = selected_swarm_replay_policies(policy_names)?;
    let replay_report = replay_swarm_trace(&trace)?;
    let policy_report =
        evaluate_swarm_replay_baseline_policies(&replay_report, &selected_policies)?;
    let generated_at_utc = swarm_replay_preview_generated_at(generated_at)?;
    let output_writes = u8::from(out_json.is_some()) + u8::from(out_text.is_some());
    let output_paths = SwarmReplayPreviewOutputPaths {
        json: out_json.map(ToString::to_string),
        text: out_text.map(ToString::to_string),
    };
    let report = build_swarm_replay_preview_report(
        cwd,
        &trace_arg,
        generated_at_utc,
        output_writes,
        output_paths,
        &trace,
        &replay_report,
        &policy_report,
    );
    let json_output = serde_json::to_string_pretty(&report)?;
    let text_output = render_swarm_replay_preview_text(&report);

    if let Some(path) = out_json {
        write_swarm_replay_preview_output(
            &resolve_cli_path(cwd, path),
            &json_output,
            "JSON preview",
        )?;
    }
    if let Some(path) = out_text {
        write_swarm_replay_preview_output(
            &resolve_cli_path(cwd, path),
            &text_output,
            "text preview",
        )?;
    }
    if out_json.is_none() && out_text.is_none() {
        match format {
            "json" => println!("{json_output}"),
            "text" => print!("{text_output}"),
            other => bail!("unsupported swarm-replay-preview format: {other}"),
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_swarm_replay_preview_report<'a>(
    cwd: &Path,
    trace_path: &str,
    generated_at_utc: String,
    output_writes: u8,
    output_paths: SwarmReplayPreviewOutputPaths,
    trace: &SwarmReplayTrace,
    replay_report: &pi::swarm_replay::SwarmReplayReport,
    policy_report: &'a pi::swarm_replay::SwarmReplayPolicyReport,
) -> SwarmReplayPreviewReport<'a> {
    let comparisons = policy_report
        .policy_comparisons
        .iter()
        .map(summarize_swarm_replay_policy_comparison)
        .collect::<Vec<_>>();
    let recommendation = policy_report
        .policy_comparisons
        .first()
        .map(summarize_swarm_replay_policy_comparison);
    let score_spread = policy_score_spread(&policy_report.policy_comparisons);
    let distinct_action_count = {
        let actions = policy_report
            .decisions
            .iter()
            .map(|decision| decision.action.as_str())
            .collect::<BTreeSet<_>>();
        u64::try_from(actions.len()).unwrap_or(u64::MAX)
    };
    let first_saturation_reasons = replay_report
        .resource_pressure_timeline
        .iter()
        .find(|snapshot| !snapshot.saturation_reasons.is_empty())
        .map(|snapshot| snapshot.saturation_reasons.clone())
        .unwrap_or_default();

    SwarmReplayPreviewReport {
        schema: SWARM_REPLAY_PREVIEW_SCHEMA,
        generated_at_utc,
        command: SwarmReplayPreviewCommand {
            invocation: "pi swarm-replay-preview",
            cwd: normalize_display_path(cwd),
            read_only_replay: true,
            provider_calls: 0,
            live_mutations: 0,
            output_writes,
        },
        trace: SwarmReplayPreviewTraceSummary {
            path: trace_path.to_string(),
            schema: trace.schema.clone(),
            trace_id: trace.trace_id.clone(),
            generated_at: trace.generated_at.clone(),
            source_count: u64::try_from(trace.source_inventory.len()).unwrap_or(u64::MAX),
            event_count: u64::try_from(trace.events.len()).unwrap_or(u64::MAX),
            first_event_id: trace.events.first().map(|event| event.event_id.clone()),
            last_event_id: trace.events.last().map(|event| event.event_id.clone()),
            redaction_status: swarm_replay_redaction_status(trace),
            uncertainty_state: swarm_replay_uncertainty_state(trace),
        },
        replay: SwarmReplayPreviewReplaySummary {
            schema: SWARM_REPLAY_REPORT_SCHEMA,
            replayed_event_count: replay_report.replayed_event_count,
            final_logical_clock: replay_report.final_logical_clock,
            snapshot_count: u64::try_from(replay_report.snapshots.len()).unwrap_or(u64::MAX),
            diagnostic_count: u64::try_from(replay_report.diagnostics.len()).unwrap_or(u64::MAX),
            diagnostics: replay_report
                .diagnostics
                .iter()
                .take(8)
                .map(|diagnostic| SwarmReplayPreviewDiagnosticSummary {
                    code: diagnostic.code.clone(),
                    severity: diagnostic.severity.clone(),
                    event_id: diagnostic.event_id.clone(),
                    message: diagnostic.message.clone(),
                })
                .collect(),
            final_state: SwarmReplayPreviewStateSummary {
                bead_count: u64::try_from(replay_report.final_state.beads.len())
                    .unwrap_or(u64::MAX),
                agent_count: u64::try_from(replay_report.final_state.agents.len())
                    .unwrap_or(u64::MAX),
                active_reservation_count: u64::try_from(
                    replay_report
                        .final_state
                        .reservations
                        .values()
                        .filter(|reservation| reservation.active)
                        .count(),
                )
                .unwrap_or(u64::MAX),
                active_build_slot_count: u64::try_from(replay_report.final_state.build_slots.len())
                    .unwrap_or(u64::MAX),
                rch_job_count: u64::try_from(replay_report.final_state.rch_jobs.len())
                    .unwrap_or(u64::MAX),
                validation_gate_count: u64::try_from(
                    replay_report.final_state.validation_gates.len(),
                )
                .unwrap_or(u64::MAX),
                runpack_recommendation_count: u64::try_from(
                    replay_report.final_state.runpack_recommendations.len(),
                )
                .unwrap_or(u64::MAX),
                operator_handoff_count: u64::try_from(
                    replay_report.final_state.operator_handoffs.len(),
                )
                .unwrap_or(u64::MAX),
                reservation_conflict_count: replay_report
                    .final_state
                    .coordination
                    .reservation_conflict_count,
                agent_mail_available: replay_report.final_state.coordination.agent_mail_available,
                missing_agent_mail_evidence: replay_report
                    .final_state
                    .coordination
                    .missing_agent_mail_evidence,
                dirty_worktree: replay_report
                    .final_state
                    .worktree
                    .as_ref()
                    .map(|worktree| worktree.dirty),
            },
            resource_saturation_points: u64::try_from(
                replay_report
                    .resource_pressure_timeline
                    .iter()
                    .filter(|snapshot| !snapshot.saturation_reasons.is_empty())
                    .count(),
            )
            .unwrap_or(u64::MAX),
            first_saturation_reasons,
        },
        policies: SwarmReplayPreviewPolicySection {
            schema: SWARM_REPLAY_POLICY_REPORT_SCHEMA,
            requested_policy_ids: policy_report.policy_ids.clone(),
            evaluated_policy_ids: policy_report.policy_ids.clone(),
            decision_count: policy_report.decision_count,
            comparison_count: policy_report.comparison_count,
            distinct_action_count,
            score_spread,
            comparisons,
        },
        recommendation,
        output_paths,
        guards: SwarmReplayPreviewGuards {
            read_only_replay: true,
            no_live_mutation: true,
            no_network_required: true,
            output_artifacts_only: true,
            runpack_not_source_of_truth: true,
        },
    }
}

fn swarm_replay_redaction_status(trace: &SwarmReplayTrace) -> String {
    if trace.redaction_summary.raw_secret_bytes_emitted > 0 {
        "unsafe_raw_secret_bytes_emitted".to_string()
    } else if trace.redaction_summary.redacted_count > 0
        || trace.redaction_summary.sensitive_omitted_count > 0
    {
        "redacted".to_string()
    } else {
        "clean".to_string()
    }
}

fn swarm_replay_uncertainty_state(trace: &SwarmReplayTrace) -> String {
    if !trace.uncertainty_summary.malformed_sources.is_empty() {
        "malformed_sources".to_string()
    } else if !trace.uncertainty_summary.missing_sources.is_empty() {
        "missing_sources".to_string()
    } else if !trace.uncertainty_summary.suppressed_claims.is_empty() {
        "suppressed_claims".to_string()
    } else if !trace.uncertainty_summary.stale_sources.is_empty() {
        "stale_sources".to_string()
    } else if trace
        .uncertainty_summary
        .event_count_by_uncertainty
        .keys()
        .any(|state| state != "certain")
    {
        "uncertain_events".to_string()
    } else {
        "certain".to_string()
    }
}

fn summarize_swarm_replay_policy_comparison(
    comparison: &SwarmReplayPolicyComparison,
) -> SwarmReplayPreviewPolicySummary<'_> {
    SwarmReplayPreviewPolicySummary {
        policy_id: comparison.policy_id.as_str(),
        rank: comparison.rank,
        score: comparison.score,
        confidence: comparison.confidence.level.as_str(),
        confidence_score: comparison.confidence.score,
        throughput_actions: comparison.metrics.throughput_actions,
        validation_commands_deferred: comparison.metrics.validation_commands_deferred,
        local_fallback_risk: comparison.metrics.local_fallback_risk.as_str(),
        reservation_conflicts_avoided: comparison.metrics.reservation_conflicts_avoided,
        stale_work_reclaimed: comparison.metrics.stale_work_reclaimed,
        missing_data_claims: comparison
            .missing_data
            .iter()
            .map(|missing| missing.claim.as_str())
            .collect(),
        rationale: comparison
            .rationale
            .iter()
            .take(4)
            .map(String::as_str)
            .collect(),
    }
}

fn policy_score_spread(comparisons: &[SwarmReplayPolicyComparison]) -> Option<i64> {
    let min = comparisons
        .iter()
        .map(|comparison| comparison.score)
        .min()?;
    let max = comparisons
        .iter()
        .map(|comparison| comparison.score)
        .max()?;
    Some(max.saturating_sub(min))
}

fn selected_swarm_replay_policies(
    policy_names: &[String],
) -> Result<Vec<SwarmReplayBaselinePolicy>> {
    if policy_names.is_empty() {
        return Ok(default_swarm_replay_baseline_policies().to_vec());
    }

    let mut seen = BTreeSet::new();
    let mut policies = Vec::new();
    for raw in policy_names {
        let Some(value) = non_empty_string(raw) else {
            bail!("swarm-replay-preview requires non-empty --policy values");
        };
        let Some(policy) = parse_swarm_replay_policy(&value) else {
            bail!(
                "unsupported swarm-replay-preview policy {value}; valid policies: {}",
                swarm_replay_policy_help()
            );
        };
        if seen.insert(policy) {
            policies.push(policy);
        }
    }
    Ok(policies)
}

fn parse_swarm_replay_policy(value: &str) -> Option<SwarmReplayBaselinePolicy> {
    let normalized = value.trim().replace('-', "_");
    match normalized.as_str() {
        "conservative_manual" => Some(SwarmReplayBaselinePolicy::ConservativeManual),
        "existing_autopilot" => Some(SwarmReplayBaselinePolicy::ExistingAutopilot),
        "rch_fanout_limited" => Some(SwarmReplayBaselinePolicy::RchFanoutLimited),
        "stale_bead_reclaiming" => Some(SwarmReplayBaselinePolicy::StaleBeadReclaiming),
        "build_slot_protective" => Some(SwarmReplayBaselinePolicy::BuildSlotProtective),
        _ => None,
    }
}

fn swarm_replay_policy_help() -> String {
    default_swarm_replay_baseline_policies()
        .iter()
        .map(SwarmReplayPolicyAdapter::policy_id)
        .collect::<Vec<_>>()
        .join(", ")
}

fn swarm_replay_preview_generated_at(generated_at: Option<&str>) -> Result<String> {
    let Some(value) = generated_at.and_then(non_empty_string) else {
        return Ok(chrono::Utc::now().to_rfc3339());
    };
    if chrono::DateTime::parse_from_rfc3339(&value).is_err() {
        bail!("swarm-replay-preview requires --generated-at to be RFC3339: {value}");
    }
    Ok(value)
}

fn resolve_cli_path(cwd: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn write_swarm_replay_preview_output(path: &Path, content: &str, label: &str) -> Result<()> {
    if path.exists() {
        bail!("refusing to overwrite existing {label}: {}", path.display());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content)?;
    Ok(())
}

fn render_swarm_replay_preview_text(report: &SwarmReplayPreviewReport<'_>) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Swarm Replay Preview");
    let _ = writeln!(output, "schema: {}", report.schema);
    let _ = writeln!(output, "trace: {}", report.trace.trace_id);
    let _ = writeln!(
        output,
        "events: {} replayed, {} snapshots",
        report.replay.replayed_event_count, report.replay.snapshot_count
    );
    let _ = writeln!(
        output,
        "sources: {} ({})",
        report.trace.source_count, report.trace.uncertainty_state
    );
    let _ = writeln!(
        output,
        "final_state: {} beads, {} agents, {} active reservations, {} reservation conflicts",
        report.replay.final_state.bead_count,
        report.replay.final_state.agent_count,
        report.replay.final_state.active_reservation_count,
        report.replay.final_state.reservation_conflict_count
    );
    let _ = writeln!(
        output,
        "policies: {} evaluated, {} decisions, {} distinct actions",
        report.policies.evaluated_policy_ids.len(),
        report.policies.decision_count,
        report.policies.distinct_action_count
    );
    if let Some(recommendation) = &report.recommendation {
        let _ = writeln!(
            output,
            "top_policy: {} rank {} score {} confidence {}",
            recommendation.policy_id,
            recommendation.rank,
            recommendation.score,
            recommendation.confidence
        );
        for reason in &recommendation.rationale {
            let _ = writeln!(output, "rationale: {reason}");
        }
    }
    if report.replay.diagnostic_count > 0 {
        let _ = writeln!(output, "diagnostics: {}", report.replay.diagnostic_count);
        for diagnostic in &report.replay.diagnostics {
            let _ = writeln!(
                output,
                "- {} {}: {}",
                diagnostic.severity, diagnostic.code, diagnostic.message
            );
        }
    } else {
        let _ = writeln!(output, "diagnostics: 0");
    }
    if report.replay.resource_saturation_points > 0 {
        let _ = writeln!(
            output,
            "resource_saturation_points: {}",
            report.replay.resource_saturation_points
        );
        for reason in &report.replay.first_saturation_reasons {
            let _ = writeln!(output, "saturation: {reason}");
        }
    }
    let _ = writeln!(
        output,
        "guards: read_only={} no_live_mutation={} no_network_required={} runpack_not_source_of_truth={}",
        report.guards.read_only_replay,
        report.guards.no_live_mutation,
        report.guards.no_network_required,
        report.guards.runpack_not_source_of_truth
    );
    output
}

#[derive(Debug, Serialize)]
struct ContextPreviewReport<'a> {
    schema: &'static str,
    generated_at_utc: String,
    command: ContextPreviewCommandProvenance,
    graph: ContextPreviewGraphSummary,
    request: &'a pi::semantic_workspace_graph::ContextBundleRequest,
    bundle: &'a pi::semantic_workspace_graph::SemanticContextBundle,
}

#[derive(Debug, Serialize)]
struct ContextPreviewCommandProvenance {
    invocation: &'static str,
    cwd: String,
    read_only: bool,
    provider_calls: u8,
    writes: u8,
}

#[derive(Debug, Serialize)]
struct ContextPreviewGraphSummary {
    root: String,
    nodes: usize,
    edges: usize,
    input_fingerprints: usize,
}

#[allow(clippy::too_many_arguments)]
fn handle_context_preview_blocking(
    cwd: &Path,
    format: &str,
    bead: Option<&str>,
    changed_paths: &[String],
    failing_command: Option<&str>,
    max_items: usize,
    max_bytes: u64,
    query: &[String],
) -> Result<()> {
    let query = non_empty_string(&query.join(" "));
    let bead_id = bead.and_then(non_empty_string);
    let failing_command = failing_command.and_then(non_empty_string);
    let changed_paths: Vec<String> = changed_paths
        .iter()
        .filter_map(|path| non_empty_string(path))
        .collect();

    if query.is_none() && bead_id.is_none() && changed_paths.is_empty() && failing_command.is_none()
    {
        bail!(
            "context-preview requires at least one context signal: query text, --bead, --changed-path, or --failing-command"
        );
    }

    let graph = pi::semantic_workspace_graph::SemanticWorkspaceGraphBuilder::new(cwd).build()?;
    let generated_at_utc = chrono::Utc::now().to_rfc3339();
    let request = pi::semantic_workspace_graph::ContextBundleRequest {
        query,
        bead_id,
        changed_paths,
        failing_command,
        workspace_id: Some(context_preview_workspace_id(cwd)),
        branch: context_preview_git_branch(cwd),
        session_id: None,
        generated_at_utc: Some(generated_at_utc.clone()),
        cache_ttl_seconds: 15 * 60,
        budget: pi::semantic_workspace_graph::ContextBundleBudget {
            max_items,
            max_bytes,
        },
    };
    let planner = pi::semantic_workspace_graph::SemanticContextBundlePlanner::new(&graph);
    let bundle = planner.plan(&request);
    let report = ContextPreviewReport {
        schema: "pi.context_bundle_preview.v1",
        generated_at_utc,
        command: ContextPreviewCommandProvenance {
            invocation: "pi context-preview",
            cwd: normalize_display_path(cwd),
            read_only: true,
            provider_calls: 0,
            writes: 0,
        },
        graph: ContextPreviewGraphSummary {
            root: graph.root.clone(),
            nodes: graph.nodes.len(),
            edges: graph.edges.len(),
            input_fingerprints: graph.input_fingerprints.len(),
        },
        request: &request,
        bundle: &bundle,
    };

    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        "text" => print_context_preview_text(&report),
        other => bail!("unsupported context-preview format: {other}"),
    }

    Ok(())
}

fn non_empty_string(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn context_preview_workspace_id(cwd: &Path) -> String {
    format!("workspace:{}", normalize_display_path(cwd))
}

fn context_preview_git_branch(cwd: &Path) -> Option<String> {
    let head_path = context_preview_git_head_path(cwd)?;
    let head = fs::read_to_string(head_path).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/").map_or_else(
        || {
            head.get(..12.min(head.len()))
                .and_then(|short| non_empty_string(&format!("detached:{short}")))
        },
        non_empty_string,
    )
}

fn context_preview_git_head_path(cwd: &Path) -> Option<PathBuf> {
    let dot_git = cwd.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git.join("HEAD"));
    }
    let git_file = fs::read_to_string(&dot_git).ok()?;
    let git_dir = git_file
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let git_dir = PathBuf::from(git_dir);
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        cwd.join(git_dir)
    };
    Some(git_dir.join("HEAD"))
}

fn print_context_preview_text(report: &ContextPreviewReport<'_>) {
    let bundle = report.bundle;
    println!("Context Bundle Preview");
    println!("schema: {}", report.schema);
    println!("read_only: true");
    println!("provider_calls: 0");
    println!("writes: 0");
    println!(
        "graph: {} nodes, {} edges, {} inputs",
        report.graph.nodes, report.graph.edges, report.graph.input_fingerprints
    );
    println!(
        "selected: {}  excluded: {}  stale_suppressions: {}",
        bundle.selected_items.len(),
        bundle.excluded_items.len(),
        bundle.stale_evidence_suppressions.len()
    );
    println!(
        "estimated: {} bytes / {} tokens",
        bundle.estimated_bytes, bundle.estimated_tokens
    );
    println!(
        "redaction: {:?}  selected_redacted={}  unsafe_suppressed={}",
        bundle.redaction_summary.overall_status,
        bundle.redaction_summary.selected_redacted_nodes,
        bundle.redaction_summary.suppressed_unsafe_nodes
    );
    println!(
        "cache: cacheable={} ttl_seconds={} expires_at={}",
        bundle.invalidation_policy.cacheable,
        bundle.invalidation_policy.cache_ttl_seconds,
        bundle
            .invalidation_policy
            .expires_at_utc
            .as_deref()
            .unwrap_or("(none)")
    );

    if !bundle.path_normalization.is_empty() {
        println!();
        println!("Changed Paths");
        for path in &bundle.path_normalization {
            let normalized = path.normalized_path.as_deref().unwrap_or("(rejected)");
            println!(
                "- {} -> {} [{}]",
                terminal_safe(&path.raw_path),
                terminal_safe(normalized),
                terminal_safe(&path.reason)
            );
        }
    }

    println!();
    println!("Selected Items");
    if bundle.selected_items.is_empty() {
        println!("- (none)");
    } else {
        for item in &bundle.selected_items {
            println!(
                "- {} {} score={} reason={}",
                terminal_safe(&item.source_path),
                terminal_safe(&item.title),
                item.score,
                terminal_safe(&item.reason)
            );
        }
    }

    println!();
    println!("Excluded Items");
    if bundle.excluded_items.is_empty() {
        println!("- (none)");
    } else {
        for item in bundle.excluded_items.iter().take(12) {
            println!(
                "- {} {} reason={}",
                terminal_safe(&item.source_path),
                terminal_safe(&item.title),
                terminal_safe(&item.reason)
            );
        }
        if bundle.excluded_items.len() > 12 {
            println!("- ... {} more", bundle.excluded_items.len() - 12);
        }
    }

    print_context_preview_stale_suppressions(&bundle.stale_evidence_suppressions);

    println!();
    println!("Suggested Validation Commands");
    if bundle.suggested_validation_commands.is_empty() {
        println!("- (none)");
    } else {
        for command in &bundle.suggested_validation_commands {
            println!("- {}", terminal_safe(command));
        }
    }
}

fn print_context_preview_stale_suppressions(
    suppressions: &[pi::semantic_workspace_graph::ContextBundleExclusion],
) {
    println!();
    println!("Stale Evidence Suppressions");
    if suppressions.is_empty() {
        println!("- (none)");
        return;
    }

    for item in suppressions {
        println!(
            "- {} {} reason={} freshness={}",
            terminal_safe(&item.source_path),
            terminal_safe(&item.title),
            terminal_safe(&item.reason),
            item.freshness_status
                .map_or_else(|| "unknown".to_string(), |status| format!("{status:?}"))
        );
    }
}

fn terminal_safe(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\n' | '\r' | '\t') {
            output.push(' ');
        } else if ch.is_control() {
            output.push('?');
        } else {
            output.push(ch);
        }
    }
    output
}

fn normalize_display_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn spawn_session_index_maintenance() {
    const MAX_INDEX_AGE: Duration = Duration::from_mins(30);
    let index = SessionIndex::new();

    // Always spawn the background thread to handle cleanup, regardless of reindexing needs.
    // Cleanup can be slow if there are many temp files, so we don't want to block main.
    std::thread::spawn(move || {
        // Clean up old bash tool logs in background
        pi::tools::cleanup_temp_files();

        if index.should_reindex(MAX_INDEX_AGE)
            && let Err(err) = index.reindex_all()
        {
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
    let safety_index = load_extension_safety_index();
    print_install_safety_advisory(&resolved_source, safety_index.as_ref());
    manager.install(&resolved_source, scope).await?;
    manager.add_package_source(&resolved_source, scope).await?;
    if resolved_source.eq(source) {
        println!("Installed {source}");
    } else {
        println!("Installed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

fn handle_package_install_blocking(
    manager: &PackageManager,
    source: &str,
    local: bool,
) -> Result<()> {
    let scope = scope_from_flag(local);
    let resolved_source = manager.resolve_install_source_alias(source);
    let safety_index = load_extension_safety_index();
    print_install_safety_advisory(&resolved_source, safety_index.as_ref());
    manager.install_blocking(&resolved_source, scope)?;
    manager.add_package_source_blocking(&resolved_source, scope)?;
    if resolved_source.eq(source) {
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
    if resolved_source.eq(source) {
        println!("Removed {source}");
    } else {
        println!("Removed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

fn handle_package_remove_blocking(
    manager: &PackageManager,
    source: &str,
    local: bool,
) -> Result<()> {
    let scope = scope_from_flag(local);
    let resolved_source = manager.resolve_install_source_alias(source);
    manager.remove_blocking(&resolved_source, scope)?;
    manager.remove_package_source_blocking(&resolved_source, scope)?;
    if resolved_source.eq(source) {
        println!("Removed {source}");
    } else {
        println!("Removed {source} (resolved to {resolved_source})");
    }
    Ok(())
}

async fn handle_package_update(manager: &PackageManager, source: Option<String>) -> Result<()> {
    let entries = manager.list_packages().await?;

    if let Some(source) = source {
        let source = source.trim();
        if source.is_empty() {
            bail!(pi::error::Error::validation(
                "Package source must be non-empty"
            ));
        }

        let resolved_source = manager.resolve_install_source_alias(source);
        let identity = manager.package_identity(&resolved_source);
        let mut matched = false;
        for entry in entries {
            if manager.package_identity(&entry.source).ne(&identity) {
                continue;
            }
            matched = true;
            manager.update_source(&entry.source, entry.scope).await?;
        }
        if !matched {
            bail!(pi::error::Error::validation(format!(
                "Package source not found: {source}"
            )));
        }
        if resolved_source.eq(source) {
            println!("Updated {source}");
        } else {
            println!("Updated {source} (resolved to {resolved_source})");
        }
        return Ok(());
    }

    let mut failed = 0;
    for entry in entries {
        if let Err(e) = manager.update_source(&entry.source, entry.scope).await {
            eprintln!("Failed to update {}: {}", entry.source, e);
            failed += 1;
        }
    }

    if failed > 0 {
        bail!("Failed to update {failed} packages");
    }
    println!("Updated packages");
    Ok(())
}

fn handle_package_update_blocking(manager: &PackageManager, source: Option<&str>) -> Result<()> {
    let entries = manager.list_packages_blocking()?;

    if let Some(source) = source {
        let source = source.trim();
        if source.is_empty() {
            bail!(pi::error::Error::validation(
                "Package source must be non-empty"
            ));
        }

        let resolved_source = manager.resolve_install_source_alias(source);
        let identity = manager.package_identity(&resolved_source);
        let mut matched = false;
        for entry in entries {
            if manager.package_identity(&entry.source).ne(&identity) {
                continue;
            }
            matched = true;
            manager.update_source_blocking(&entry.source, entry.scope)?;
        }
        if !matched {
            bail!(pi::error::Error::validation(format!(
                "Package source not found: {source}"
            )));
        }
        if resolved_source.eq(source) {
            println!("Updated {source}");
        } else {
            println!("Updated {source} (resolved to {resolved_source})");
        }
        return Ok(());
    }

    let mut failed = 0;
    for entry in entries {
        if let Err(e) = manager.update_source_blocking(&entry.source, entry.scope) {
            eprintln!("Failed to update {}: {}", entry.source, e);
            failed += 1;
        }
    }

    if failed > 0 {
        bail!("Failed to update {failed} packages");
    }
    println!("Updated packages");
    Ok(())
}

async fn handle_package_list(manager: &PackageManager) -> Result<()> {
    let entries = manager.list_packages().await?;
    let (user, project) = split_package_entries(entries);
    let safety_index = load_extension_safety_index();

    if user.is_empty() && project.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }

    if !user.is_empty() {
        println!("User packages:");
        for entry in &user {
            print_package_entry(manager, entry, safety_index.as_ref()).await?;
        }
    }

    if !project.is_empty() {
        if !user.is_empty() {
            println!();
        }
        println!("Project packages:");
        for entry in &project {
            print_package_entry(manager, entry, safety_index.as_ref()).await?;
        }
    }

    Ok(())
}

fn handle_package_list_blocking(manager: &PackageManager) -> Result<()> {
    let entries = manager.list_packages_blocking()?;
    let safety_index = load_extension_safety_index();
    print_package_list_entries_blocking(manager, entries, |manager, entry| {
        print_package_entry_blocking(manager, entry, safety_index.as_ref())
    })
}

fn split_package_entries(entries: Vec<PackageEntry>) -> (Vec<PackageEntry>, Vec<PackageEntry>) {
    let mut user = Vec::new();
    let mut project = Vec::new();
    for entry in entries {
        match entry.scope {
            PackageScope::User => user.push(entry),
            PackageScope::Project | PackageScope::Temporary => project.push(entry),
        }
    }
    (user, project)
}

fn print_package_list_entries_blocking<F>(
    manager: &PackageManager,
    entries: Vec<PackageEntry>,
    mut print_entry: F,
) -> Result<()>
where
    F: FnMut(&PackageManager, &PackageEntry) -> Result<()>,
{
    let (user, project) = split_package_entries(entries);

    if user.is_empty() && project.is_empty() {
        println!("No packages installed.");
        return Ok(());
    }

    if !user.is_empty() {
        println!("User packages:");
        for entry in &user {
            print_entry(manager, entry)?;
        }
    }

    if !project.is_empty() {
        if !user.is_empty() {
            println!();
        }
        println!("Project packages:");
        for entry in &project {
            print_entry(manager, entry)?;
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
    if has_cache && index.is_stale(chrono::Utc::now(), DEFAULT_INDEX_MAX_AGE) {
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

    render_search_results(&index, query, tag, sort, limit);
    Ok(())
}

fn handle_search_blocking(
    query: &str,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
) -> Result<bool> {
    let store = ExtensionIndexStore::default_store();
    let index = store.load_or_seed()?;

    // Preserve refresh semantics: if cache is stale, fall back to async path so we can
    // attempt network refresh before searching.
    let has_cache = store.path().exists();
    if has_cache && index.is_stale(chrono::Utc::now(), DEFAULT_INDEX_MAX_AGE) {
        return Ok(false);
    }

    render_search_results(&index, query, tag, sort, limit);
    Ok(true)
}

fn render_search_results(
    index: &pi::extension_index::ExtensionIndex,
    query: &str,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
) {
    let hits = collect_search_hits(index, tag, sort, limit, query);
    if hits.is_empty() {
        println!("No extensions found for \"{query}\".");
        return;
    }

    print_search_results(&hits, index);
}

fn collect_search_hits(
    index: &pi::extension_index::ExtensionIndex,
    tag: Option<&str>,
    sort: &str,
    limit: usize,
    query: &str,
) -> Vec<pi::extension_index::ExtensionSearchHit> {
    if limit.eq(&0) {
        return Vec::new();
    }

    let mut hits = index.search(query, index.entries.len());

    // Filter by tag if requested
    if let Some(tag_filter) = tag {
        let tag_lower = tag_filter.to_ascii_lowercase();
        hits.retain(|hit| {
            hit.entry
                .tags
                .iter()
                .any(|t| t.to_ascii_lowercase().eq(&tag_lower))
        });
    }

    // Sort by name if requested (relevance is the default from search())
    if sort.eq("name") {
        hits.sort_by(|a, b| {
            a.entry
                .name
                .to_ascii_lowercase()
                .cmp(&b.entry.name.to_ascii_lowercase())
        });
    }

    hits.truncate(limit);
    hits
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let truncated = value.chars().take(keep).collect::<String>();
    format!("{truncated}...")
}

#[allow(clippy::uninlined_format_args)]
fn print_search_results(hits: &[pi::extension_index::ExtensionSearchHit], index: &ExtensionIndex) {
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
    let safety_w = hits
        .iter()
        .map(|h| {
            ExtensionSafetyProvenance::from_index_entry(&h.entry, index, DEFAULT_INDEX_MAX_AGE)
                .compact_label()
                .len()
                .min(44)
        })
        .max()
        .unwrap_or(0)
        .max(6); // "Safety"

    // Header
    println!(
        "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}  {:<safety_w$}",
        "Name", "Description", "Tags", "Source", "Safety"
    );
    println!(
        "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}  {:<safety_w$}",
        "-".repeat(name_w),
        "-".repeat(desc_w),
        "-".repeat(tags_w),
        "-".repeat(source_w),
        "-".repeat(safety_w)
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
        let safety =
            ExtensionSafetyProvenance::from_index_entry(&hit.entry, index, DEFAULT_INDEX_MAX_AGE)
                .compact_label();
        let safety_truncated = truncate_chars(&safety, 44);
        println!(
            "  {:<name_w$}  {:<desc_w$}  {:<tags_w$}  {:<source_w$}  {:<safety_w$}",
            hit.entry.name, desc_truncated, tags_truncated, source_label, safety_truncated
        );
    }

    let count = hits.len();
    let noun = if count.eq(&1) {
        "extension"
    } else {
        "extensions"
    };
    println!("\n  {count} {noun} found. Install with: pi install <name>");
}

fn handle_info_blocking(name: &str) -> Result<()> {
    let index = ExtensionIndexStore::default_store().load_or_seed()?;
    match find_index_entry_by_name_or_id(&index, name) {
        ExtensionInfoLookup::Found(entry) => print_extension_info(entry, &index),
        ExtensionInfoLookup::Ambiguous => {
            println!("Extension query \"{name}\" is ambiguous.");
            println!("Try: pi search {name}");
        }
        ExtensionInfoLookup::NotFound => {
            println!("Extension \"{name}\" not found.");
            println!("Try: pi search {name}");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ExtensionInfoLookup<'a> {
    Found(&'a pi::extension_index::ExtensionIndexEntry),
    NotFound,
    Ambiguous,
}

fn find_index_entry_by_name_or_id<'a>(
    index: &'a pi::extension_index::ExtensionIndex,
    name: &str,
) -> ExtensionInfoLookup<'a> {
    // Look up by exact id, name, or fuzzy match when there is a single best hit.
    if let Some(entry) = index
        .entries
        .iter()
        .find(|e| e.id.eq_ignore_ascii_case(name) || e.name.eq_ignore_ascii_case(name))
    {
        return ExtensionInfoLookup::Found(entry);
    }

    let hits = index.search(name, 2);
    let Some(best_hit) = hits.first() else {
        return ExtensionInfoLookup::NotFound;
    };

    if hits
        .get(1)
        .is_some_and(|next_hit| next_hit.score.eq(&best_hit.score))
    {
        return ExtensionInfoLookup::Ambiguous;
    }

    index
        .entries
        .iter()
        .find(|entry| entry.id.eq(&best_hit.entry.id))
        .map_or(ExtensionInfoLookup::NotFound, ExtensionInfoLookup::Found)
}

fn print_extension_info(entry: &ExtensionIndexEntry, index: &ExtensionIndex) {
    let width = 60;
    let bar = "─".repeat(width);

    // Header
    println!("  ┌{bar}┐");
    let title = &entry.name;
    let padding = width.saturating_sub(title.len() + 1);
    println!("  │ {title}{:padding$}│", "");

    // ID (if different from name)
    if entry.id.ne(&entry.name) {
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

    // Safety provenance
    let safety = ExtensionSafetyProvenance::from_index_entry(entry, index, DEFAULT_INDEX_MAX_AGE);
    println!("  ├{bar}┤");
    for line in extension_safety_lines(&safety) {
        let padding = width.saturating_sub(line.len() + 1);
        println!("  │ {line}{:padding$}│", "");
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

fn load_extension_safety_index() -> Option<ExtensionIndex> {
    ExtensionIndexStore::default_store().load_or_seed().ok()
}

fn extension_safety_for_source(
    source: &str,
    index: Option<&ExtensionIndex>,
) -> ExtensionSafetyProvenance {
    if let Some(index) = index
        && let Some(entry) = index
            .entries
            .iter()
            .find(|entry| entry.install_source.as_deref() == Some(source))
    {
        return ExtensionSafetyProvenance::from_index_entry(entry, index, DEFAULT_INDEX_MAX_AGE);
    }
    ExtensionSafetyProvenance::from_install_source(source)
}

fn extension_safety_lines(safety: &ExtensionSafetyProvenance) -> Vec<String> {
    let capabilities = if safety.requested_capabilities.is_empty() {
        "none".to_string()
    } else {
        safety.requested_capabilities.join(",")
    };
    let mut lines = vec![
        format!(
            "Safety: source={} license={} risk={} confidence={}",
            safety.source_type,
            safety.license_status,
            safety.risk_profile,
            safety.source_confidence
        ),
        format!(
            "Signals: categories={} capabilities={} freshness={}",
            safety.registration_categories.join(","),
            capabilities,
            safety.freshness
        ),
    ];
    if !safety.degraded_reasons.is_empty() {
        lines.push(format!("Degraded: {}", safety.degraded_reasons.join(",")));
    }
    lines
}

fn print_install_safety_advisory(source: &str, index: Option<&ExtensionIndex>) {
    let safety = extension_safety_for_source(source, index);
    for line in extension_safety_lines(&safety) {
        println!("{line}");
    }
}

async fn print_package_entry(
    manager: &PackageManager,
    entry: &PackageEntry,
    index: Option<&ExtensionIndex>,
) -> Result<()> {
    let display = if entry.filter.is_some() {
        format!("{} (filtered)", entry.source)
    } else {
        entry.source.clone()
    };
    println!("  {display}");
    if let Some(path) = manager.installed_path(&entry.source, entry.scope).await? {
        println!("    {}", path.display());
    }
    let safety = extension_safety_for_source(&entry.source, index);
    println!("    Safety: {}", safety.compact_label());
    Ok(())
}

fn print_package_entry_blocking(
    manager: &PackageManager,
    entry: &PackageEntry,
    index: Option<&ExtensionIndex>,
) -> Result<()> {
    let display = if entry.filter.is_some() {
        format!("{} (filtered)", entry.source)
    } else {
        entry.source.clone()
    };
    println!("  {display}");
    if let Some(path) = manager.installed_path_blocking(&entry.source, entry.scope)? {
        println!("    {}", path.display());
    }
    let safety = extension_safety_for_source(&entry.source, index);
    println!("    Safety: {}", safety.compact_label());
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ConfigResourceKind {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

impl ConfigResourceKind {
    const ALL: [Self; 4] = [Self::Extensions, Self::Skills, Self::Prompts, Self::Themes];

    const fn field_name(self) -> &'static str {
        match self {
            Self::Extensions => "extensions",
            Self::Skills => "skills",
            Self::Prompts => "prompts",
            Self::Themes => "themes",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Extensions => "extension",
            Self::Skills => "skill",
            Self::Prompts => "prompt",
            Self::Themes => "theme",
        }
    }

    const fn order(self) -> usize {
        match self {
            Self::Extensions => 0,
            Self::Skills => 1,
            Self::Prompts => 2,
            Self::Themes => 3,
        }
    }
}

#[derive(Debug, Clone)]
struct ConfigResourceState {
    kind: ConfigResourceKind,
    path: String,
    enabled: bool,
}

#[derive(Debug, Clone)]
struct ConfigPackageState {
    scope: SettingsScope,
    source: String,
    resources: Vec<ConfigResourceState>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPathsReport {
    global: String,
    project: String,
    auth: String,
    sessions: String,
    packages: String,
    extension_index: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigResourceReport {
    kind: String,
    path: String,
    enabled: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigPackageReport {
    scope: String,
    source: String,
    resources: Vec<ConfigResourceReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigReport {
    paths: ConfigPathsReport,
    precedence: Vec<String>,
    config_valid: bool,
    config_error: Option<String>,
    packages: Vec<ConfigPackageReport>,
}

#[derive(Debug, Clone, Default)]
struct PackageFilterState {
    extensions: Option<Vec<String>>,
    skills: Option<Vec<String>>,
    prompts: Option<Vec<String>>,
    themes: Option<Vec<String>>,
}

impl PackageFilterState {
    fn set_kind(&mut self, kind: ConfigResourceKind, values: Vec<String>) {
        match kind {
            ConfigResourceKind::Extensions => self.extensions = Some(values),
            ConfigResourceKind::Skills => self.skills = Some(values),
            ConfigResourceKind::Prompts => self.prompts = Some(values),
            ConfigResourceKind::Themes => self.themes = Some(values),
        }
    }

    const fn values_for_kind(&self, kind: ConfigResourceKind) -> Option<&Vec<String>> {
        match kind {
            ConfigResourceKind::Extensions => self.extensions.as_ref(),
            ConfigResourceKind::Skills => self.skills.as_ref(),
            ConfigResourceKind::Prompts => self.prompts.as_ref(),
            ConfigResourceKind::Themes => self.themes.as_ref(),
        }
    }

    const fn has_any_field(&self) -> bool {
        self.extensions.is_some()
            || self.skills.is_some()
            || self.prompts.is_some()
            || self.themes.is_some()
    }
}

#[derive(Debug, Clone)]
struct ConfigUiResult {
    save_requested: bool,
    packages: Vec<ConfigPackageState>,
}

#[derive(bubbletea::Model)]
struct ConfigUiApp {
    packages: Vec<ConfigPackageState>,
    selected: usize,
    settings_summary: String,
    status: String,
    result_slot: Arc<StdMutex<Option<ConfigUiResult>>>,
}

impl ConfigUiApp {
    fn new(
        packages: Vec<ConfigPackageState>,
        settings_summary: String,
        result_slot: Arc<StdMutex<Option<ConfigUiResult>>>,
    ) -> Self {
        let status = if packages.iter().any(|pkg| !pkg.resources.is_empty()) {
            String::new()
        } else {
            "No package resources discovered. Press Enter to exit.".to_string()
        };

        Self {
            packages,
            selected: 0,
            settings_summary,
            status,
            result_slot,
        }
    }

    fn selectable_count(&self) -> usize {
        self.packages.iter().map(|pkg| pkg.resources.len()).sum()
    }

    fn selected_coords(&self) -> Option<(usize, usize)> {
        let mut cursor = 0usize;
        for (pkg_idx, pkg) in self.packages.iter().enumerate() {
            for (res_idx, _) in pkg.resources.iter().enumerate() {
                if cursor.eq(&self.selected) {
                    return Some((pkg_idx, res_idx));
                }
                cursor = cursor.saturating_add(1);
            }
        }
        None
    }

    fn move_selection(&mut self, delta: isize) {
        let total = self.selectable_count();
        if total.eq(&0) {
            self.selected = 0;
            return;
        }

        let max_index = total.saturating_sub(1);
        let step = delta.unsigned_abs();
        if delta.is_negative() {
            self.selected = self.selected.saturating_sub(step);
        } else {
            self.selected = self.selected.saturating_add(step).min(max_index);
        }
    }

    fn toggle_selected(&mut self) {
        if let Some((pkg_idx, res_idx)) = self.selected_coords()
            && let Some(resource) = self
                .packages
                .get_mut(pkg_idx)
                .and_then(|pkg| pkg.resources.get_mut(res_idx))
        {
            resource.enabled = !resource.enabled;
        }
    }

    fn finish(&self, save_requested: bool) -> Cmd {
        if let Ok(mut slot) = self.result_slot.lock() {
            *slot = Some(ConfigUiResult {
                save_requested,
                packages: self.packages.clone(),
            });
        }
        quit()
    }

    #[allow(clippy::missing_const_for_fn, clippy::unused_self)]
    fn init(&self) -> Option<Cmd> {
        None
    }

    #[allow(clippy::needless_pass_by_value)]
    fn update(&mut self, msg: BubbleMessage) -> Option<Cmd> {
        if let Some(key) = msg.downcast_ref::<KeyMsg>() {
            match key.key_type {
                KeyType::Up => self.move_selection(-1),
                KeyType::Down => self.move_selection(1),
                KeyType::Runes if key.runes.eq(&['k']) => self.move_selection(-1),
                KeyType::Runes if key.runes.eq(&['j']) => self.move_selection(1),
                KeyType::Space => self.toggle_selected(),
                KeyType::Enter => return Some(self.finish(true)),
                KeyType::Esc | KeyType::CtrlC => return Some(self.finish(false)),
                KeyType::Runes if key.runes.eq(&['q']) => return Some(self.finish(false)),
                _ => {}
            }
        }
        None
    }

    fn view(&self) -> String {
        let mut out = String::new();
        out.push_str("Pi Config UI\n");
        let _ = writeln!(out, "{}", self.settings_summary);
        out.push_str("Keys: ↑/↓ (or j/k) move, Space toggle, Enter save, q cancel\n\n");

        let mut cursor = 0usize;
        for package in &self.packages {
            let _ = writeln!(
                out,
                "{} package: {}",
                scope_label(package.scope),
                package.source
            );

            if package.resources.is_empty() {
                out.push_str("    (no discovered resources)\n");
                continue;
            }

            for resource in &package.resources {
                let selected = cursor.eq(&self.selected);
                let marker = if resource.enabled { "x" } else { " " };
                let prefix = if selected { ">" } else { " " };
                let _ = writeln!(
                    out,
                    "{} [{}] {:<10} {}",
                    prefix,
                    marker,
                    resource.kind.label(),
                    resource.path
                );
                cursor = cursor.saturating_add(1);
            }

            out.push('\n');
        }

        if !self.status.is_empty() {
            let _ = writeln!(out, "{}", self.status);
        }

        out
    }
}

const fn scope_label(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "Global",
        SettingsScope::Project => "Project",
    }
}

const fn scope_key(scope: SettingsScope) -> &'static str {
    match scope {
        SettingsScope::Global => "global",
        SettingsScope::Project => "project",
    }
}

const fn settings_scope_from_package_scope(scope: PackageScope) -> Option<SettingsScope> {
    match scope {
        PackageScope::User => Some(SettingsScope::Global),
        PackageScope::Project => Some(SettingsScope::Project),
        PackageScope::Temporary => None,
    }
}

fn package_lookup_key(scope: SettingsScope, source: &str) -> String {
    format!("{}::{source}", scope_key(scope))
}

fn normalize_path_for_display(path: &Path, base_dir: Option<&Path>) -> String {
    let rel = base_dir
        .and_then(|base| path.strip_prefix(base).ok())
        .unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

fn normalize_filter_entry(path: &str) -> String {
    path.replace('\\', "/")
}

fn merge_resolved_resources(
    kind: ConfigResourceKind,
    resources: &[ResolvedResource],
    packages: &mut Vec<ConfigPackageState>,
    lookup: &mut std::collections::HashMap<String, usize>,
) {
    for resource in resources {
        if !matches!(resource.metadata.origin, ResourceOrigin::Package) {
            continue;
        }

        let Some(scope) = settings_scope_from_package_scope(resource.metadata.scope) else {
            continue;
        };

        let key = package_lookup_key(scope, &resource.metadata.source);
        let idx = lookup.get(&key).copied().unwrap_or_else(|| {
            let idx = packages.len();
            packages.push(ConfigPackageState {
                scope,
                source: resource.metadata.source.clone(),
                resources: Vec::new(),
            });
            lookup.insert(key, idx);
            idx
        });

        let path =
            normalize_path_for_display(&resource.path, resource.metadata.base_dir.as_deref());
        packages[idx].resources.push(ConfigResourceState {
            kind,
            path,
            enabled: resource.enabled,
        });
    }
}

fn sort_and_dedupe_package_resources(packages: &mut [ConfigPackageState]) {
    for package in packages {
        package.resources.sort_by(|a, b| {
            (a.kind.order(), a.path.as_str()).cmp(&(b.kind.order(), b.path.as_str()))
        });

        let mut deduped: Vec<ConfigResourceState> = Vec::new();
        for resource in std::mem::take(&mut package.resources) {
            if let Some(existing) = deduped
                .iter_mut()
                .find(|r| r.kind.eq(&resource.kind) && r.path.eq(&resource.path))
            {
                existing.enabled = existing.enabled || resource.enabled;
            } else {
                deduped.push(resource);
            }
        }
        package.resources = deduped;
    }
}

fn collect_config_packages_from_entries(
    entries: Vec<PackageEntry>,
    resolved_paths: Option<ResolvedPaths>,
) -> Vec<ConfigPackageState> {
    let mut packages = Vec::new();
    let mut lookup = std::collections::HashMap::<String, usize>::new();

    for entry in entries {
        let Some(scope) = settings_scope_from_package_scope(entry.scope) else {
            continue;
        };
        let key = package_lookup_key(scope, &entry.source);
        if lookup.contains_key(&key) {
            continue;
        }
        lookup.insert(key, packages.len());
        packages.push(ConfigPackageState {
            scope,
            source: entry.source,
            resources: Vec::new(),
        });
    }

    if let Some(ResolvedPaths {
        extensions,
        skills,
        prompts,
        themes,
    }) = resolved_paths
    {
        merge_resolved_resources(
            ConfigResourceKind::Extensions,
            &extensions,
            &mut packages,
            &mut lookup,
        );
        merge_resolved_resources(
            ConfigResourceKind::Skills,
            &skills,
            &mut packages,
            &mut lookup,
        );
        merge_resolved_resources(
            ConfigResourceKind::Prompts,
            &prompts,
            &mut packages,
            &mut lookup,
        );
        merge_resolved_resources(
            ConfigResourceKind::Themes,
            &themes,
            &mut packages,
            &mut lookup,
        );
    }

    sort_and_dedupe_package_resources(&mut packages);
    packages
}

async fn collect_config_packages(manager: &PackageManager) -> Result<Vec<ConfigPackageState>> {
    let entries = manager.list_packages().await?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let resolved_paths = match manager.resolve().await {
        Ok(paths) => Some(paths),
        Err(err) => {
            eprintln!("Warning: failed to resolve package resources for config UI: {err}");
            None
        }
    };

    Ok(collect_config_packages_from_entries(
        entries,
        resolved_paths,
    ))
}

fn collect_config_packages_blocking(
    manager: &PackageManager,
    entries: Vec<PackageEntry>,
) -> Result<Option<Vec<ConfigPackageState>>> {
    let Some(resolved_paths) = manager.resolve_package_resources_blocking()? else {
        return Ok(None);
    };
    Ok(Some(collect_config_packages_from_entries(
        entries,
        Some(resolved_paths),
    )))
}

fn build_config_report(cwd: &Path, packages: &[ConfigPackageState]) -> ConfigReport {
    let global_dir = Config::global_dir();
    let config_override_path = Config::config_path_override_from_env(cwd);
    let config_path = config_override_path
        .clone()
        .unwrap_or_else(|| global_dir.join("settings.json"));
    let project_path = cwd.join(Config::project_dir()).join("settings.json");

    let (config_valid, config_error) =
        match Config::load_with_roots(config_override_path.as_deref(), &global_dir, cwd) {
            Ok(_) => (true, None),
            Err(err) => (false, Some(err.to_string())),
        };

    let packages = packages
        .iter()
        .map(|package| ConfigPackageReport {
            scope: scope_key(package.scope).to_string(),
            source: package.source.clone(),
            resources: package
                .resources
                .iter()
                .map(|resource| ConfigResourceReport {
                    kind: resource.kind.field_name().to_string(),
                    path: resource.path.clone(),
                    enabled: resource.enabled,
                })
                .collect(),
        })
        .collect::<Vec<_>>();

    ConfigReport {
        paths: ConfigPathsReport {
            global: config_path.display().to_string(),
            project: project_path.display().to_string(),
            auth: Config::auth_path().display().to_string(),
            sessions: Config::sessions_dir().display().to_string(),
            packages: Config::package_dir().display().to_string(),
            extension_index: Config::extension_index_path().display().to_string(),
        },
        precedence: vec![
            "CLI flags".to_string(),
            "Environment variables".to_string(),
            format!("Project settings ({})", project_path.display()),
            format!("Global settings ({})", config_path.display()),
            "Built-in defaults".to_string(),
        ],
        config_valid,
        config_error,
        packages,
    }
}

fn print_config_report(report: &ConfigReport, include_packages: bool) {
    println!("Settings paths:");
    println!("  Global:  {}", report.paths.global);
    println!("  Project: {}", report.paths.project);
    println!();
    println!("Other paths:");
    println!("  Auth:     {}", report.paths.auth);
    println!("  Sessions: {}", report.paths.sessions);
    println!("  Packages: {}", report.paths.packages);
    println!("  ExtIndex: {}", report.paths.extension_index);
    println!();
    println!("Settings precedence:");
    for (idx, entry) in report.precedence.iter().enumerate() {
        println!("  {}) {}", idx + 1, entry);
    }
    println!();

    if report.config_valid {
        println!("Current configuration is valid.");
    } else if let Some(error) = &report.config_error {
        println!("Configuration Error: {error}");
    }

    if !include_packages {
        return;
    }

    println!();
    println!("Package resources:");
    if report.packages.is_empty() {
        println!("  (no configured packages)");
        return;
    }

    for package in &report.packages {
        println!("  [{}] {}", package.scope, package.source);
        if package.resources.is_empty() {
            println!("    (no discovered resources)");
            continue;
        }
        for resource in &package.resources {
            let marker = if resource.enabled { "x" } else { " " };
            println!("    [{}] {:<10} {}", marker, resource.kind, resource.path);
        }
    }
}

fn handle_config_paths_fast(cwd: &Path) {
    let report = build_config_report(cwd, &[]);
    print_config_report(&report, false);
}

fn handle_config_show_fast(cwd: &Path) {
    let report = build_config_report(cwd, &[]);
    print_config_report(&report, true);
}

fn handle_config_json_fast(cwd: &Path) -> Result<()> {
    let report = build_config_report(cwd, &[]);
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn format_settings_summary(config: &Config) -> String {
    let provider = config.default_provider.as_deref().unwrap_or("(default)");
    let model = config.default_model.as_deref().unwrap_or("(default)");
    let thinking = config
        .default_thinking_level
        .as_deref()
        .unwrap_or("(default)");
    format!("provider={provider}  model={model}  thinking={thinking}")
}

fn interactive_config_settings_summary_with_roots(
    cwd: &Path,
    global_dir: &Path,
    config_override_path: Option<&Path>,
) -> Result<String> {
    let config = Config::load_with_roots(config_override_path, global_dir, cwd)?;
    Ok(format_settings_summary(&config))
}

fn interactive_config_settings_summary(cwd: &Path) -> Result<String> {
    let global_dir = Config::global_dir();
    let config_override_path = Config::config_path_override_from_env(cwd);
    interactive_config_settings_summary_with_roots(
        cwd,
        &global_dir,
        config_override_path.as_deref(),
    )
}

fn run_config_tui(
    packages: Vec<ConfigPackageState>,
    settings_summary: String,
) -> Result<Option<Vec<ConfigPackageState>>> {
    let result_slot = Arc::new(StdMutex::new(None));
    let app = ConfigUiApp::new(packages, settings_summary, Arc::clone(&result_slot));
    Program::new(app).with_alt_screen().run()?;

    let result = result_slot.lock().ok().and_then(|guard| guard.clone());
    match result {
        Some(result) if result.save_requested => Ok(Some(result.packages)),
        _ => Ok(None),
    }
}

fn load_settings_json_object(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }

    let content = std::fs::read_to_string(path)?;
    if content.trim().is_empty() {
        return Ok(json!({}));
    }

    let value: Value = serde_json::from_str(&content)?;
    if value.is_object() {
        Ok(value)
    } else {
        Ok(json!({}))
    }
}

fn extract_package_source(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string).or_else(|| {
        value
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn persist_package_toggles(cwd: &Path, packages: &[ConfigPackageState]) -> Result<()> {
    let global_dir = Config::global_dir();
    let config_override_path = Config::config_path_override_from_env(cwd);
    persist_package_toggles_with_roots(cwd, &global_dir, config_override_path.as_deref(), packages)
}

#[allow(clippy::too_many_lines)]
fn persist_package_toggles_with_roots(
    cwd: &Path,
    global_dir: &Path,
    config_override_path: Option<&Path>,
    packages: &[ConfigPackageState],
) -> Result<()> {
    let mut updates_by_scope: std::collections::HashMap<
        SettingsScope,
        std::collections::HashMap<String, PackageFilterState>,
    > = std::collections::HashMap::new();

    for package in packages {
        if package.resources.is_empty() {
            continue;
        }

        let mut state = PackageFilterState::default();
        for kind in ConfigResourceKind::ALL {
            let kind_resources = package
                .resources
                .iter()
                .filter(|resource| resource.kind.eq(&kind))
                .collect::<Vec<_>>();
            if kind_resources.is_empty() {
                continue;
            }

            let mut enabled = kind_resources
                .iter()
                .filter(|resource| resource.enabled)
                .map(|resource| normalize_filter_entry(&resource.path))
                .collect::<Vec<_>>();
            enabled.sort();
            enabled.dedup();
            state.set_kind(kind, enabled);
        }

        if !state.has_any_field() {
            continue;
        }

        // A full config override replaces the normal global/project split, so all
        // package filter writes must land in the override file.
        let scope = if config_override_path.is_some() {
            SettingsScope::Global
        } else {
            package.scope
        };

        updates_by_scope
            .entry(scope)
            .or_default()
            .insert(package.source.clone(), state);
    }

    let scopes: &[SettingsScope] = if config_override_path.is_some() {
        &[SettingsScope::Global]
    } else {
        &[SettingsScope::Global, SettingsScope::Project]
    };

    for &scope in scopes {
        let Some(scope_updates) = updates_by_scope.get(&scope) else {
            continue;
        };

        let settings_path = config_override_path.map_or_else(
            || Config::settings_path_with_roots(scope, global_dir, cwd),
            Path::to_path_buf,
        );
        let mut settings = load_settings_json_object(&settings_path)?;
        if !settings.is_object() {
            settings = json!({});
        }

        let packages_array = settings
            .as_object_mut()
            .expect("checked is object")
            .entry("packages".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !packages_array.is_array() {
            *packages_array = Value::Array(Vec::new());
        }

        let package_entries = packages_array
            .as_array_mut()
            .expect("forced packages to be an array");

        let mut updated_sources = std::collections::HashSet::new();
        for entry in package_entries.iter_mut() {
            let Some(source) = extract_package_source(entry) else {
                continue;
            };
            let Some(filter_state) = scope_updates.get(&source) else {
                continue;
            };

            let mut obj = entry
                .as_object()
                .cloned()
                .unwrap_or_else(serde_json::Map::new);
            obj.insert("source".to_string(), Value::String(source.clone()));
            for kind in ConfigResourceKind::ALL {
                if let Some(values) = filter_state.values_for_kind(kind) {
                    let arr = values
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>();
                    obj.insert(kind.field_name().to_string(), Value::Array(arr));
                }
            }
            *entry = Value::Object(obj);
            updated_sources.insert(source);
        }

        let mut new_sources: Vec<_> = scope_updates
            .iter()
            .filter(|(source, _)| !updated_sources.contains(*source))
            .collect();
        new_sources.sort_by_key(|(source, _)| *source);

        for (source, filter_state) in new_sources {
            let mut obj = serde_json::Map::new();
            obj.insert("source".to_string(), Value::String(source.clone()));
            for kind in ConfigResourceKind::ALL {
                if let Some(values) = filter_state.values_for_kind(kind) {
                    let arr = values
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>();
                    obj.insert(kind.field_name().to_string(), Value::Array(arr));
                }
            }
            package_entries.push(Value::Object(obj));
        }

        let patch = json!({ "packages": package_entries.clone() });
        Config::patch_settings_to_path(&settings_path, patch)?;
    }

    Ok(())
}

async fn handle_config(
    manager: &PackageManager,
    cwd: &Path,
    show: bool,
    paths: bool,
    json_output: bool,
) -> Result<()> {
    if json_output && (show || paths) {
        bail!("`pi config --json` cannot be combined with --show/--paths");
    }

    let interactive_requested = !show && !paths;
    let need_packages = show || json_output || interactive_requested;
    let packages = if need_packages {
        collect_config_packages(manager).await?
    } else {
        Vec::new()
    };
    let report = build_config_report(cwd, &packages);

    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let has_tty = io::stdin().is_terminal() && io::stdout().is_terminal();

    if interactive_requested && has_tty {
        let settings_summary = interactive_config_settings_summary(cwd)?;
        if let Some(updated) = run_config_tui(packages, settings_summary)? {
            persist_package_toggles(cwd, &updated)?;
            println!("Saved package resource toggles.");
        } else {
            println!("No changes saved.");
        }
        return Ok(());
    }

    print_config_report(&report, show);
    Ok(())
}

fn handle_session_migrate(path: &str, dry_run: bool) -> Result<()> {
    let path = std::path::Path::new(path);
    if !path.exists() {
        bail!("Path does not exist: {}", path.display());
    }

    // Collect JSONL files to migrate.
    let jsonl_files: Vec<std::path::PathBuf> = if path.is_dir() {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().is_some_and(|e| e.eq("jsonl")) {
                files.push(p);
            }
        }
        if files.is_empty() {
            bail!("No .jsonl session files found in {}", path.display());
        }
        files
    } else {
        vec![path.to_path_buf()]
    };

    let mut migrated = 0u64;
    let mut errors = 0u64;

    for jsonl_path in &jsonl_files {
        if dry_run {
            match pi::session::migrate_dry_run(jsonl_path) {
                Ok(verification) => {
                    let status = if verification.entry_count_match
                        && verification.hash_chain_match
                        && verification.index_consistent
                    {
                        "OK"
                    } else {
                        "MISMATCH"
                    };
                    println!(
                        "[dry-run] {}: {} (entries_match={}, hash_match={}, index_ok={})",
                        jsonl_path.display(),
                        status,
                        verification.entry_count_match,
                        verification.hash_chain_match,
                        verification.index_consistent,
                    );
                    migrated += 1;
                }
                Err(e) => {
                    eprintln!("[dry-run] {}: ERROR: {e}", jsonl_path.display());
                    errors += 1;
                }
            }
        } else {
            let correlation_id = uuid::Uuid::new_v4().to_string();
            match pi::session::migrate_jsonl_to_v2(jsonl_path, &correlation_id) {
                Ok(event) => {
                    println!(
                        "[migrated] {}: migration_id={}, entries_match={}, hash_match={}, index_ok={}",
                        jsonl_path.display(),
                        event.migration_id,
                        event.verification.entry_count_match,
                        event.verification.hash_chain_match,
                        event.verification.index_consistent,
                    );
                    migrated += 1;
                }
                Err(e) => {
                    eprintln!("[error] {}: {e}", jsonl_path.display());
                    errors += 1;
                }
            }
        }
    }

    println!(
        "\nSession migration complete: {migrated} succeeded, {errors} failed (dry_run={dry_run})"
    );
    if errors > 0 {
        bail!("{errors} session(s) failed migration");
    }
    Ok(())
}

fn handle_doctor(
    cwd: &Path,
    extension_path: Option<&str>,
    format: &str,
    policy_override: Option<&str>,
    fix: bool,
    only: Option<&str>,
) -> Result<()> {
    use pi::doctor::{CheckCategory, DoctorOptions};

    let only_set = if let Some(raw) = only {
        let mut parsed = std::collections::HashSet::new();
        let mut invalid = Vec::new();
        for part in raw.split(',') {
            let name = part.trim();
            if name.is_empty() {
                continue;
            }
            match name.parse::<CheckCategory>() {
                Ok(cat) => {
                    parsed.insert(cat);
                }
                Err(_) => invalid.push(name.to_string()),
            }
        }
        if !invalid.is_empty() {
            bail!(
                "Unknown --only categories: {} (valid: config, dirs, auth, shell, sessions, swarm, extensions)",
                invalid.join(", ")
            );
        }
        if parsed.is_empty() {
            bail!(
                "--only must include at least one category (valid: config, dirs, auth, shell, sessions, swarm, extensions)"
            );
        }
        Some(parsed)
    } else {
        None
    };

    let opts = DoctorOptions {
        cwd,
        extension_path,
        policy_override,
        fix,
        only: only_set,
    };

    let report = pi::doctor::run_doctor(&opts)?;

    match format {
        "json" => {
            println!("{}", report.to_json()?);
        }
        "markdown" | "md" => {
            print!("{}", report.render_markdown());
        }
        _ => {
            print!("{}", report.render_text());
        }
    }

    // Exit with code 1 if any failures (useful for CI)
    if matches!(report.overall, pi::doctor::Severity::Fail) {
        std::process::exit(1);
    }

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
    let mut models = registry.available_models();
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
        if matches!(provider_cmp, std::cmp::Ordering::Equal) {
            a.model.id.cmp(&b.model.id)
        } else {
            provider_cmp
        }
    });

    let rows = build_model_rows(&models);
    print_model_table(&rows);
    maybe_print_list_models_note(&rows, pattern);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedModelRow {
    provider: String,
    model: String,
    context: String,
    max_out: String,
    thinking: String,
    images: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ListModelsCachePayload {
    error: Option<String>,
    rows: Vec<CachedModelRow>,
}

fn list_models_from_cached_rows(rows: &[CachedModelRow], pattern: Option<&str>) {
    if rows.is_empty() {
        println!("No models available. Set API keys in environment variables.");
        return;
    }

    if let Some(pattern) = pattern {
        let filtered = rows
            .iter()
            .filter(|row| fuzzy_match_model_id(pattern, &row.provider, &row.model))
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            println!("No models matching \"{pattern}\"");
            return;
        }
        print_model_table(&filtered);
        maybe_print_list_models_note(&filtered, Some(pattern));
    } else {
        print_model_table(rows);
        maybe_print_list_models_note(rows, None);
    }
}

fn maybe_print_list_models_note<R: ModelTableRow>(rows: &[R], pattern: Option<&str>) {
    if pattern.is_some() {
        return;
    }

    let mut providers = BTreeSet::new();
    for row in rows {
        providers.insert(row.provider());
    }
    let shown = providers.len();
    let total = PROVIDER_METADATA.len();

    if shown < total {
        println!("Showing {shown} of {total} providers. Run `pi --list-providers` to see all.");
    }
}

fn should_fingerprint_model_env_var(key: &str) -> bool {
    if key.ends_with("_API_KEY") || key.ends_with("_TOKEN") || key.ends_with("_KEY") {
        return true;
    }
    PROVIDER_METADATA
        .iter()
        .any(|meta| meta.auth_env_keys.contains(&key))
}

const LIST_MODELS_CACHE_FINGERPRINT_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[cfg(unix)]
fn open_fingerprint_file(path: &Path) -> io::Result<fs::File> {
    let descriptor = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(fs::File::from(descriptor))
}

#[cfg(not(unix))]
fn open_fingerprint_file(path: &Path) -> io::Result<fs::File> {
    fs::File::open(path)
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    true
}

fn append_file_fingerprint(hasher: &mut Sha256, path: &Path) -> bool {
    let path_bytes = path.as_os_str().as_encoded_bytes();
    hasher.update((path_bytes.len() as u64).to_le_bytes());
    hasher.update(path_bytes);
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            hasher.update([1]);
            if !meta.file_type().is_file() || meta.len() > LIST_MODELS_CACHE_FINGERPRINT_MAX_BYTES {
                hasher.update([5]);
                return false;
            }
            hasher.update(meta.len().to_le_bytes());
            let modified = meta.modified().ok();
            if let Some(modified) = modified
                && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
            {
                hasher.update(duration.as_secs().to_le_bytes());
                hasher.update(duration.subsec_nanos().to_le_bytes());
            }
            let Ok(file) = open_fingerprint_file(path) else {
                hasher.update([3]);
                return false;
            };
            let Ok(capacity) = usize::try_from(meta.len()) else {
                hasher.update([4]);
                return false;
            };
            let mut contents = Vec::with_capacity(capacity);
            let mut limited = file.take(LIST_MODELS_CACHE_FINGERPRINT_MAX_BYTES + 1);
            if limited.read_to_end(&mut contents).is_err()
                || contents.len() as u64 != meta.len()
                || contents.len() as u64 > LIST_MODELS_CACHE_FINGERPRINT_MAX_BYTES
            {
                hasher.update([4]);
                return false;
            }
            let Ok(opened_after) = limited.get_ref().metadata() else {
                hasher.update([6]);
                return false;
            };
            let Ok(after) = fs::symlink_metadata(path) else {
                hasher.update([6]);
                return false;
            };
            if !opened_after.file_type().is_file()
                || !same_file_identity(&meta, &opened_after)
                || opened_after.len() != meta.len()
                || opened_after.modified().ok() != modified
                || !after.file_type().is_file()
                || !same_file_identity(&opened_after, &after)
                || after.len() != meta.len()
                || after.modified().ok() != modified
            {
                hasher.update([7]);
                return false;
            }
            hasher.update([2]);
            hasher.update(contents);
            true
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            hasher.update([0]);
            true
        }
        Err(_) => {
            hasher.update([8]);
            false
        }
    }
}

fn list_models_cache_path(models_path: &Path) -> Option<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update(pi::models::model_catalog_cache_fingerprint().to_le_bytes());
    if !append_file_fingerprint(&mut hasher, &Config::auth_path())
        || !append_file_fingerprint(&mut hasher, models_path)
        || !append_file_fingerprint(&mut hasher, &fetched_models_path(models_path))
    {
        return None;
    }

    let mut env_vars = std::env::vars_os()
        .filter_map(|(key, value)| {
            let key = key.into_string().ok()?;
            should_fingerprint_model_env_var(&key).then_some((key, value))
        })
        .collect::<Vec<_>>();
    env_vars.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for (key, value) in env_vars {
        hasher.update(key.as_bytes());
        hasher.update([0xff]);
        let value = value.as_os_str().as_encoded_bytes();
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value);
        hasher.update([0x00]);
    }

    let key = format!("{:x}", hasher.finalize());
    dirs::cache_dir().map(|dir| {
        dir.join("pi")
            .join("list-models-cache")
            .join(format!("{key}.json"))
    })
}

fn load_list_models_cache(models_path: &Path) -> Option<ListModelsCachePayload> {
    let cache_path = list_models_cache_path(models_path)?;
    let body = fs::read_to_string(cache_path).ok()?;
    serde_json::from_str::<ListModelsCachePayload>(&body).ok()
}

fn save_list_models_cache(models_path: &Path, payload: &ListModelsCachePayload) {
    let Some(cache_path) = list_models_cache_path(models_path) else {
        return;
    };
    let Some(parent) = cache_path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }

    let temp_path = cache_path.with_extension(format!("tmp-{}", std::process::id()));
    let Ok(file) = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp_path)
    else {
        return;
    };
    let mut writer = io::BufWriter::new(file);
    if serde_json::to_writer(&mut writer, payload).is_ok() && writer.flush().is_ok() {
        let _ = fs::rename(&temp_path, cache_path);
    } else {
        let _ = fs::remove_file(&temp_path);
    }
}

async fn handle_fetch_models(
    provider: &str,
    api_key_override: Option<&str>,
    refresh: bool,
    persist: bool,
) -> Result<()> {
    // SAP service-key resolution performs a token exchange. Establish that a
    // usable live-catalog route exists first so an unsupported native adapter
    // cannot trigger an unnecessary credential network request. Explicit
    // models.json SAP routes continue through the normal exchange path.
    if pi::provider_metadata::canonical_provider_id(provider)
        .is_some_and(|canonical| canonical == "sap-ai-core")
        && !pi::providers::model_fetch::provider_model_catalog_route_is_configured(provider)?
    {
        bail!(
            "provider {provider:?} has no built-in or models.json routing configuration for live model discovery"
        );
    }

    // Resolve the route before auth storage so keyless routes and routes with a
    // complete custom Authorization header cannot be delayed or rejected by an
    // unrelated auth.json lock. The plan keeps configured fallback credentials
    // lazy and reuses the already-resolved route headers for the actual request.
    let fetch_plan = pi::providers::prepare_provider_model_catalog_fetch(provider)?;
    let api_key = if fetch_plan.requires_runtime_api_key() {
        // Use the normal credential resolver: an explicit CLI override wins,
        // then stored OAuth/Bearer credentials, provider environment variables,
        // stored API keys, and supported external-CLI credentials.
        resolve_provider_api_key(provider, api_key_override).await?
    } else {
        String::new()
    };

    let catalog = fetch_plan.fetch(&api_key, refresh).await;

    let catalog = catalog.map_err(anyhow::Error::new)?;

    let used_static_fallback = matches!(
        catalog.source(),
        pi::providers::ModelCatalogSource::StaticFallback
    );

    if persist {
        if used_static_fallback {
            bail!(
                "Refusing to persist the static fallback for {provider:?}; \
                 configure provider credentials and retry a successful live fetch"
            );
        }
        let models_path = default_models_path(&Config::global_dir());
        let fetched_path = pi::providers::persist_provider_model_catalog(&models_path, &catalog)?;
        eprintln!(
            "Persisted {} models for {provider:?} to {}",
            catalog.models().len(),
            fetched_path.display()
        );
    }

    if catalog.models().is_empty() {
        bail!(
            "No models available for {provider:?}: live discovery failed and the static registry \
             has no matching entries. Check the provider name and credentials."
        );
    }

    if used_static_fallback {
        eprintln!(
            "Warning: live model discovery for {provider:?} was unavailable; \
             showing the static registry instead. Use --refresh-models to require a live result."
        );
    }

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for id in catalog.models() {
        writeln!(out, "{id}")?;
    }
    out.flush()?;
    Ok(())
}

async fn resolve_provider_api_key(provider: &str, override_key: Option<&str>) -> Result<String> {
    resolve_provider_api_key_with_auth_path_and_env(
        provider,
        override_key,
        Config::auth_path(),
        |name| std::env::var(name).ok(),
    )
    .await
}

fn resolve_ambient_provider_api_key_with_env<F>(provider: &str, mut env: F) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    let canonical_provider =
        pi::provider_metadata::canonical_provider_id(provider).unwrap_or(provider);
    let env_keys: &[&str] = match canonical_provider {
        // The remaining AWS variables are structured credential-chain inputs,
        // not standalone bearer tokens. Preserve AuthStorage's normal rule.
        "amazon-bedrock" => &["AWS_BEARER_TOKEN_BEDROCK"],
        // SAP's structured environment is exchanged by its provider-owned path.
        "sap-ai-core" => &[],
        _ => provider_metadata::provider_auth_env_keys(canonical_provider),
    };
    env_keys.iter().find_map(|name| {
        env(name).and_then(|value| {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        })
    })
}

async fn resolve_provider_api_key_with_auth_path_and_env<F>(
    provider: &str,
    override_key: Option<&str>,
    auth_path: PathBuf,
    ambient_env: F,
) -> Result<String>
where
    F: FnMut(&str) -> Option<String>,
{
    if let Some(key) = override_key.map(str::trim).filter(|key| !key.is_empty()) {
        if pi::provider_metadata::canonical_provider_id(provider)
            .is_some_and(|canonical| canonical == "sap-ai-core")
        {
            return Ok(pi::auth::resolve_sap_auth_candidate(key)
                .await?
                .unwrap_or_default());
        }
        return Ok(key.to_string());
    }
    match AuthStorage::load_with_lock_timeout_classified(
        auth_path,
        pi::auth::AUTH_RESOLUTION_LOCK_TIMEOUT,
    ) {
        Ok(mut auth) => {
            let requested_oauth_expired = matches!(
                auth.credential_status(provider),
                pi::auth::CredentialStatus::OAuthExpired { .. }
            );
            let refresh_error = if requested_oauth_expired {
                auth.refresh_expired_oauth_tokens().await.err()
            } else {
                None
            };
            let resolved = resolve_provider_api_key_from_auth(provider, &auth).await?;
            if resolved.trim().is_empty() {
                if let Some(error) = refresh_error {
                    return Err(anyhow::Error::new(error));
                }
            } else if refresh_error.is_some() {
                // Refresh processes all expiring stored OAuth entries. The requested provider may
                // still have refreshed successfully (or resolved through its next normal source)
                // even when an unrelated provider failed. Do not expose the aggregate refresh
                // diagnostic here because provider error bodies can contain credential material.
                tracing::warn!(
                    provider,
                    "one or more stored OAuth refreshes failed, but the requested model-catalog provider resolved successfully"
                );
            }
            Ok(resolved)
        }
        Err(failure @ pi::auth::AuthStorageLoadFailure::LockTimeout(_)) => {
            Err(anyhow::Error::new(failure.into_error()))
        }
        Err(pi::auth::AuthStorageLoadFailure::Other(error)) => {
            tracing::warn!(
                provider,
                error = %error,
                "stored provider credentials are unavailable; continuing model discovery without them"
            );
            if pi::provider_metadata::canonical_provider_id(provider)
                .is_some_and(|canonical| canonical == "sap-ai-core")
            {
                Ok(pi::auth::resolve_ambient_sap_auth_token()
                    .await?
                    .unwrap_or_default())
            } else {
                Ok(
                    resolve_ambient_provider_api_key_with_env(provider, ambient_env)
                        .unwrap_or_default(),
                )
            }
        }
    }
}

async fn resolve_provider_api_key_from_auth(provider: &str, auth: &AuthStorage) -> Result<String> {
    if pi::provider_metadata::canonical_provider_id(provider)
        .is_some_and(|canonical| canonical == "sap-ai-core")
    {
        return Ok(pi::auth::resolve_sap_auth_token(auth, None)
            .await?
            .unwrap_or_default());
    }

    if let Some(key) = auth
        .resolve_api_key(provider, None)
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty())
    {
        return Ok(key);
    }

    Ok(String::new())
}

fn list_providers() {
    let mut rows: Vec<(&str, &str, String, String, &str)> = PROVIDER_METADATA
        .iter()
        .map(|meta| {
            let display = meta.display_name.unwrap_or(meta.canonical_id);
            let aliases = if meta.aliases.is_empty() {
                String::new()
            } else {
                meta.aliases.join(", ")
            };
            let env_keys = meta.auth_env_keys.join(", ");
            let api = meta.routing_defaults.map_or("-", |defaults| defaults.api);
            (meta.canonical_id, display, aliases, env_keys, api)
        })
        .collect();
    rows.sort_by_key(|(id, _, _, _, _)| *id);

    let id_w = rows.iter().map(|r| r.0.len()).max().unwrap_or(0).max(8);
    let name_w = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(4);
    let alias_w = rows.iter().map(|r| r.2.len()).max().unwrap_or(0).max(7);
    let env_w = rows.iter().map(|r| r.3.len()).max().unwrap_or(0).max(8);
    let api_w = rows.iter().map(|r| r.4.len()).max().unwrap_or(0).max(3);

    // Buffer all output to reduce write syscalls from O(rows) to O(1).
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let _ = writeln!(
        out,
        "{:<id_w$}  {:<name_w$}  {:<alias_w$}  {:<env_w$}  {:<api_w$}",
        "provider", "name", "aliases", "auth env", "api",
    );
    let _ = writeln!(
        out,
        "{:<id_w$}  {:<name_w$}  {:<alias_w$}  {:<env_w$}  {:<api_w$}",
        "-".repeat(id_w),
        "-".repeat(name_w),
        "-".repeat(alias_w),
        "-".repeat(env_w),
        "-".repeat(api_w),
    );
    for (id, name, aliases, env_keys, api) in &rows {
        let _ = writeln!(
            out,
            "{id:<id_w$}  {name:<name_w$}  {aliases:<alias_w$}  {env_keys:<env_w$}  {api:<api_w$}"
        );
    }
    let _ = writeln!(out, "\n{} providers available.", rows.len());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupCredentialKind {
    ApiKey,
    OAuthPkce,
    OAuthDeviceFlow,
}

#[derive(Clone, Copy)]
struct ProviderChoice {
    provider: &'static str,
    label: &'static str,
    kind: SetupCredentialKind,
    env: &'static str,
}

const PROVIDER_CHOICES: &[ProviderChoice] = &[
    ProviderChoice {
        provider: "openai-codex",
        label: "OpenAI Codex (ChatGPT)",
        kind: SetupCredentialKind::OAuthPkce,
        env: "",
    },
    ProviderChoice {
        provider: "openai",
        label: "OpenAI",
        kind: SetupCredentialKind::ApiKey,
        env: "OPENAI_API_KEY",
    },
    ProviderChoice {
        provider: "anthropic",
        label: "Anthropic (Claude Code)",
        kind: SetupCredentialKind::OAuthPkce,
        env: "",
    },
    ProviderChoice {
        provider: "anthropic",
        label: "Anthropic (Claude API key)",
        kind: SetupCredentialKind::ApiKey,
        env: "ANTHROPIC_API_KEY",
    },
    ProviderChoice {
        provider: "kimi-for-coding",
        label: "Kimi for Coding",
        kind: SetupCredentialKind::OAuthDeviceFlow,
        env: "KIMI_API_KEY",
    },
    ProviderChoice {
        provider: "google-gemini-cli",
        label: "Google Cloud Code Assist",
        kind: SetupCredentialKind::OAuthPkce,
        env: "",
    },
    ProviderChoice {
        provider: "google",
        label: "Google Gemini",
        kind: SetupCredentialKind::ApiKey,
        env: "GOOGLE_API_KEY",
    },
    ProviderChoice {
        provider: "google-antigravity",
        label: "Google Antigravity",
        kind: SetupCredentialKind::OAuthPkce,
        env: "",
    },
    ProviderChoice {
        provider: "azure-openai",
        label: "Azure OpenAI",
        kind: SetupCredentialKind::ApiKey,
        env: "AZURE_OPENAI_API_KEY",
    },
    ProviderChoice {
        provider: "openrouter",
        label: "OpenRouter",
        kind: SetupCredentialKind::ApiKey,
        env: "OPENROUTER_API_KEY",
    },
    ProviderChoice {
        provider: "cohere",
        label: "Cohere",
        kind: SetupCredentialKind::ApiKey,
        env: "COHERE_API_KEY",
    },
    ProviderChoice {
        provider: "groq",
        label: "Groq",
        kind: SetupCredentialKind::ApiKey,
        env: "GROQ_API_KEY",
    },
    ProviderChoice {
        provider: "deepseek",
        label: "DeepSeek",
        kind: SetupCredentialKind::ApiKey,
        env: "DEEPSEEK_API_KEY",
    },
    ProviderChoice {
        provider: "mistral",
        label: "Mistral AI",
        kind: SetupCredentialKind::ApiKey,
        env: "MISTRAL_API_KEY",
    },
];

fn provider_choice_default_for_provider(provider: &str) -> Option<ProviderChoice> {
    let canonical = provider_metadata::canonical_provider_id(provider).unwrap_or(provider);
    PROVIDER_CHOICES
        .iter()
        .copied()
        .find(|choice| choice.provider.eq_ignore_ascii_case(canonical))
}

fn provider_choice_from_token(token: &str) -> Option<ProviderChoice> {
    let raw = token.trim();
    let normalized = raw.to_ascii_lowercase();
    let (first, rest) = normalized
        .split_once(char::is_whitespace)
        .map_or((normalized.as_str(), ""), |(a, b)| (a, b.trim()));
    let wants_oauth = rest.contains("oauth");
    let wants_key = rest.contains("key") || rest.contains("api");

    let select_choice_for_provider = |provider: &str| -> Option<ProviderChoice> {
        let canonical = provider_metadata::canonical_provider_id(provider).unwrap_or(provider);

        if (wants_oauth || wants_key)
            && let Some(found) = PROVIDER_CHOICES.iter().copied().find(|choice| {
                choice.provider.eq_ignore_ascii_case(canonical)
                    && ((wants_oauth
                        && matches!(
                            choice.kind,
                            SetupCredentialKind::OAuthPkce | SetupCredentialKind::OAuthDeviceFlow
                        ))
                        || (wants_key && matches!(choice.kind, SetupCredentialKind::ApiKey)))
            })
        {
            return Some(found);
        }

        provider_choice_default_for_provider(canonical)
    };

    // Try numbered choice first (1-N).
    if let Ok(num) = first.parse::<usize>() {
        if num >= 1 && num <= PROVIDER_CHOICES.len() {
            return Some(PROVIDER_CHOICES[num - 1]);
        }
        return None;
    }

    // Try exact match against listed labels.
    for choice in PROVIDER_CHOICES {
        if normalized.eq(&choice.label.to_ascii_lowercase()) {
            return Some(*choice);
        }
    }
    if let Some(found) = select_choice_for_provider(first) {
        return Some(found);
    }

    // Common nicknames.
    match first {
        "codex" | "chatgpt" | "gpt" => return select_choice_for_provider("openai-codex"),
        "claude" => return select_choice_for_provider("anthropic"),
        "gemini" => return select_choice_for_provider("google"),
        "kimi" => return select_choice_for_provider("kimi-for-coding"),
        _ => {}
    }

    // Fall back to provider_metadata registry for any canonical ID or alias.
    let meta = provider_metadata::provider_metadata(first)?;
    let canonical = meta.canonical_id;
    if let Some(found) = select_choice_for_provider(canonical) {
        return Some(found);
    }

    // Otherwise, fall back to API-key style with whatever env var hint we have.
    Some(ProviderChoice {
        provider: canonical,
        label: canonical,
        kind: SetupCredentialKind::ApiKey,
        env: meta.auth_env_keys.first().copied().unwrap_or(""),
    })
}

#[allow(clippy::too_many_lines)]
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
            console.print_markup("[bold]No authenticated models are available yet.[/]\n");
        }
        StartupError::MissingApiKey { provider } => {
            console.print_markup(&format!(
                "[bold]Missing credentials for provider:[/] {provider}\n"
            ));
        }
    }
    console.print_markup("Let’s authenticate.\n\n");

    let provider_hint = match startup_error {
        StartupError::MissingApiKey { provider } => provider_choice_from_token(provider),
        StartupError::NoModelsAvailable { .. } => {
            provider_choice_default_for_provider("openai-codex")
        }
    }
    .or_else(|| Some(PROVIDER_CHOICES[0]));

    console.print_markup("[bold]Choose a provider:[/]\n");
    for (idx, provider) in PROVIDER_CHOICES.iter().enumerate() {
        let is_default = provider_hint.is_some_and(|hint| {
            hint.provider.eq(provider.provider) && hint.kind.eq(&provider.kind)
        });
        let default_marker = if is_default { " [dim](default)[/]" } else { "" };
        let method = match provider.kind {
            SetupCredentialKind::ApiKey => "API key",
            SetupCredentialKind::OAuthPkce => "OAuth",
            SetupCredentialKind::OAuthDeviceFlow => "OAuth (device flow)",
        };
        let hint = if provider.env.trim().is_empty() {
            method.to_string()
        } else {
            format!("{method}  {}", provider.env)
        };
        console.print_markup(&format!(
            "  [cyan]{})[/] {}  [dim]{}[/]{}\n",
            idx + 1,
            provider.label,
            hint,
            default_marker
        ));
    }
    let num_choices = PROVIDER_CHOICES.len();
    console.print_markup(&format!(
        "  [cyan]{})[/] Custom provider via models.json\n",
        num_choices + 1
    ));
    console.print_markup(&format!("  [cyan]{})[/] Exit setup\n\n", num_choices + 2));
    console
        .print_markup("[dim]Or type any provider name (e.g., deepseek, cerebras, ollama).[/]\n\n");

    let custom_num = (num_choices + 1).to_string();
    let exit_num = (num_choices + 2).to_string();
    let provider = loop {
        let prompt = provider_hint.map_or_else(
            || format!("Select 1-{} or provider name: ", num_choices + 2),
            |default_provider| {
                format!(
                    "Select 1-{} or name (Enter for {}): ",
                    num_choices + 2,
                    default_provider.label
                )
            },
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
        if normalized.eq(&custom_num) || normalized.eq("custom") || normalized.eq("models") {
            console.render_info(&format!(
                "Create models.json at {} and restart Pi.",
                models_path.display()
            ));
            return Ok(false);
        }
        if normalized.eq(&exit_num)
            || normalized.eq("q")
            || normalized.eq("quit")
            || normalized.eq("exit")
        {
            console.render_warning("Setup cancelled.");
            return Ok(false);
        }
        if let Some(provider) = provider_choice_from_token(&normalized) {
            break provider;
        }
        console.render_warning("Unrecognized choice. Please try again.");
    };

    let credential = match provider.kind {
        SetupCredentialKind::ApiKey => {
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

            AuthCredential::ApiKey {
                key: key.to_string(),
            }
        }
        SetupCredentialKind::OAuthPkce => {
            let start = match provider.provider {
                "openai-codex" => pi::auth::start_openai_codex_oauth()?,
                "anthropic" => pi::auth::start_anthropic_oauth()?,
                "google-gemini-cli" => pi::auth::start_google_gemini_cli_oauth()?,
                "google-antigravity" => pi::auth::start_google_antigravity_oauth()?,
                _ => {
                    console.render_warning(&format!(
                        "OAuth login is not supported for {} in this setup flow. Start Pi and run /login {} instead.",
                        provider.provider, provider.provider
                    ));
                    return Ok(false);
                }
            };

            if start.provider.eq("anthropic") {
                console.render_warning(
                    "Anthropic OAuth (Claude Code consumer account) is no longer recommended.\n\
Using consumer OAuth tokens outside the official client may violate Anthropic's consumer Terms of Service and can\n\
result in account suspension/ban. Prefer using an Anthropic API key (ANTHROPIC_API_KEY) instead.",
                );
            }

            // Use the pre-bound callback server when the provider already
            // created one (e.g. Copilot/GitLab with random port).  Otherwise
            // start a new one for localhost redirect URIs (issue #22).
            let callback_server = start.callback_server.or_else(|| {
                start
                    .redirect_uri
                    .as_deref()
                    .filter(|uri| pi::auth::redirect_uri_needs_callback_server(uri))
                    .and_then(|uri| match pi::auth::start_oauth_callback_server(uri) {
                        Ok(server) => {
                            tracing::info!(port = server.port, "OAuth callback server listening");
                            Some(server)
                        }
                        Err(e) => {
                            tracing::warn!("Failed to start OAuth callback server: {e}");
                            None
                        }
                    })
            });

            let has_callback = callback_server.is_some();
            if has_callback {
                console.print_markup(&format!(
                    "[bold]OAuth login:[/] {}\n\n\
                     Open this URL:\n{}\n\n\
                     Listening for callback on port {}...\n\
                     Complete authorization in your browser — Pi will continue automatically.\n\
                     (Or paste the callback URL / authorization code manually.)\n",
                    start.provider,
                    start.url,
                    callback_server.as_ref().unwrap().port,
                ));
            } else {
                console.print_markup(&format!(
                    "[bold]OAuth login:[/] {}\n\nOpen this URL:\n{}\n\n{}\n",
                    start.provider,
                    start.url,
                    start.instructions.as_deref().unwrap_or_default()
                ));
            }

            // Race between the callback server (browser redirect) and manual paste.
            let code_input = if let Some(server) = callback_server {
                // Use a background thread to wait for the callback so we can
                // also accept manual paste from stdin.
                let (manual_tx, manual_rx) = std::sync::mpsc::channel::<String>();
                let prompt_thread = std::thread::spawn(move || {
                    if let Ok(Some(line)) =
                        prompt_line("Paste callback URL or code (or wait for browser): ")
                    {
                        let _ = manual_tx.send(line);
                    }
                });

                // Wait for whichever source delivers first.
                let code = loop {
                    // Check callback server (non-blocking).
                    if let Ok(path) = server.rx.try_recv() {
                        // Convert "/auth/callback?code=abc&state=xyz" to a full
                        // URL that parse_oauth_code_input can handle.
                        let full_url = format!("http://localhost{path}");
                        break full_url;
                    }
                    // Check manual input (non-blocking).
                    if let Ok(line) = manual_rx.try_recv() {
                        break line;
                    }
                    sleep_with_current_timer(std::time::Duration::from_millis(50)).await;
                };

                // Don't wait for the prompt thread — it will exit on its own
                // or when stdin closes.
                drop(prompt_thread);
                code
            } else {
                let Some(line) = prompt_line("Paste callback URL or code: ")? else {
                    console.render_warning("Setup cancelled (no input).");
                    return Ok(false);
                };
                line
            };

            let code_input = code_input.trim();
            if code_input.is_empty() {
                console.render_warning("No authorization code provided. Setup cancelled.");
                return Ok(false);
            }

            match start.provider.as_str() {
                "openai-codex" => {
                    pi::auth::complete_openai_codex_oauth(code_input, &start.verifier).await?
                }
                "anthropic" => {
                    pi::auth::complete_anthropic_oauth(code_input, &start.verifier).await?
                }
                "google-gemini-cli" => {
                    pi::auth::complete_google_gemini_cli_oauth(code_input, &start.verifier).await?
                }
                "google-antigravity" => {
                    pi::auth::complete_google_antigravity_oauth(code_input, &start.verifier).await?
                }
                other => {
                    console.render_warning(&format!(
                        "OAuth completion not supported for {other}. Setup cancelled."
                    ));
                    return Ok(false);
                }
            }
        }
        SetupCredentialKind::OAuthDeviceFlow => {
            if provider.provider.ne("kimi-for-coding") {
                console.render_warning(&format!(
                    "Device-flow login not supported for {} in this setup flow. Start Pi and run /login {} instead.",
                    provider.provider, provider.provider
                ));
                return Ok(false);
            }

            let device = pi::auth::start_kimi_code_device_flow().await?;
            let verification_url = device
                .verification_uri_complete
                .clone()
                .unwrap_or_else(|| device.verification_uri.clone());
            console.print_markup(&format!(
                "[bold]OAuth login:[/] kimi-for-coding\n\n\
Open this URL:\n{verification_url}\n\n\
If prompted, enter this code: {}\n\
Code expires in {} seconds.\n",
                device.user_code, device.expires_in
            ));

            let start = std::time::Instant::now();
            loop {
                let elapsed = start.elapsed().as_secs();
                if elapsed >= device.expires_in {
                    console.render_warning("Device code expired. Run setup again.");
                    return Ok(false);
                }

                let Some(input) = prompt_line("Press Enter to poll (or type q to cancel): ")?
                else {
                    console.render_warning("Setup cancelled (no input).");
                    return Ok(false);
                };
                if input.trim().eq_ignore_ascii_case("q") {
                    console.render_warning("Setup cancelled.");
                    return Ok(false);
                }

                match pi::auth::poll_kimi_code_device_flow(&device.device_code).await {
                    pi::auth::DeviceFlowPollResult::Success(cred) => break cred,
                    pi::auth::DeviceFlowPollResult::Pending => {
                        console.render_info("Authorization still pending. Complete the browser step and poll again.");
                    }
                    pi::auth::DeviceFlowPollResult::SlowDown => {
                        console.render_info("Authorization server asked to slow down. Wait a few seconds and poll again.");
                    }
                    pi::auth::DeviceFlowPollResult::Expired => {
                        console.render_warning("Device code expired. Run setup again.");
                        return Ok(false);
                    }
                    pi::auth::DeviceFlowPollResult::AccessDenied => {
                        console.render_warning("Access denied. Run setup again.");
                        return Ok(false);
                    }
                    pi::auth::DeviceFlowPollResult::Error(err) => {
                        console.render_warning(&format!("OAuth polling failed: {err}"));
                        return Ok(false);
                    }
                }
            }
        }
    };

    let _ = auth.remove_provider_aliases(provider.provider);
    auth.set(provider.provider.to_string(), credential);
    auth.save_async().await?;

    // Make the next startup attempt use the credential we just created.
    if cli
        .provider
        .as_deref()
        .is_none_or(|selected| selected.ne(provider.provider))
    {
        cli.provider = Some(provider.provider.to_string());
        cli.model = None;
    }
    if provider.provider.eq("openai-codex") {
        cli.model = Some("gpt-5.5".to_string());
    }

    let saved_label = match provider.kind {
        SetupCredentialKind::ApiKey => "API key",
        SetupCredentialKind::OAuthPkce | SetupCredentialKind::OAuthDeviceFlow => {
            "OAuth credentials"
        }
    };
    console.render_success(&format!(
        "Saved {label} for {provider} to {path}",
        label = saved_label,
        provider = provider.provider,
        path = Config::auth_path().display()
    ));
    console.render_info("Continuing startup...");
    Ok(true)
}

fn filter_models_by_pattern<'a>(models: Vec<&'a ModelEntry>, pattern: &str) -> Vec<&'a ModelEntry> {
    models
        .into_iter()
        .filter(|entry| fuzzy_match_model_id(pattern, &entry.model.provider, &entry.model.id))
        .collect()
}

fn build_model_rows(
    models: &[&ModelEntry],
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

trait ModelTableRow {
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn context(&self) -> &str;
    fn max_out(&self) -> &str;
    fn thinking(&self) -> &str;
    fn images(&self) -> &str;
}

impl ModelTableRow for CachedModelRow {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn context(&self) -> &str {
        &self.context
    }

    fn max_out(&self) -> &str {
        &self.max_out
    }

    fn thinking(&self) -> &str {
        &self.thinking
    }

    fn images(&self) -> &str {
        &self.images
    }
}

impl ModelTableRow for (String, String, String, String, String, String) {
    fn provider(&self) -> &str {
        &self.0
    }

    fn model(&self) -> &str {
        &self.1
    }

    fn context(&self) -> &str {
        &self.2
    }

    fn max_out(&self) -> &str {
        &self.3
    }

    fn thinking(&self) -> &str {
        &self.4
    }

    fn images(&self) -> &str {
        &self.5
    }
}

impl<T: ModelTableRow + ?Sized> ModelTableRow for &T {
    fn provider(&self) -> &str {
        (*self).provider()
    }

    fn model(&self) -> &str {
        (*self).model()
    }

    fn context(&self) -> &str {
        (*self).context()
    }

    fn max_out(&self) -> &str {
        (*self).max_out()
    }

    fn thinking(&self) -> &str {
        (*self).thinking()
    }

    fn images(&self) -> &str {
        (*self).images()
    }
}

fn write_model_table<R: ModelTableRow, W: Write>(out: &mut W, rows: &[R]) -> io::Result<()> {
    let headers = (
        "provider", "model", "context", "max-out", "thinking", "images",
    );

    let mut provider_w = headers.0.len();
    let mut model_w = headers.1.len();
    let mut context_w = headers.2.len();
    let mut max_out_w = headers.3.len();
    let mut thinking_w = headers.4.len();
    let mut images_w = headers.5.len();
    for row in rows {
        provider_w = provider_w.max(row.provider().len());
        model_w = model_w.max(row.model().len());
        context_w = context_w.max(row.context().len());
        max_out_w = max_out_w.max(row.max_out().len());
        thinking_w = thinking_w.max(row.thinking().len());
        images_w = images_w.max(row.images().len());
    }

    let (provider, model, context, max_out, thinking, images) = headers;
    writeln!(
        out,
        "{provider:<provider_w$}  {model:<model_w$}  {context:<context_w$}  {max_out:<max_out_w$}  {thinking:<thinking_w$}  {images:<images_w$}"
    )?;

    for row in rows {
        writeln!(
            out,
            "{provider:<provider_w$}  {model:<model_w$}  {context:<context_w$}  {max_out:<max_out_w$}  {thinking:<thinking_w$}  {images:<images_w$}",
            provider = row.provider(),
            model = row.model(),
            context = row.context(),
            max_out = row.max_out(),
            thinking = row.thinking(),
            images = row.images(),
        )?;
    }

    Ok(())
}

fn print_model_table<R: ModelTableRow>(rows: &[R]) {
    // Buffer all output to reduce write syscalls from O(rows) to O(1).
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    let _ = write_model_table(&mut out, rows);
}

fn prompt_line(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    let bytes = io::stdin().read_line(&mut input)?;
    if bytes.eq(&0) {
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

    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        asupersync::fs::create_dir_all(parent).await?;
    }
    asupersync::fs::write(&output_path, html).await?;
    Ok(output_path)
}

fn has_cli_api_key_override(api_key: Option<&str>) -> bool {
    api_key.is_some_and(|value| !value.trim().is_empty())
}

fn rpc_available_models(registry: &ModelRegistry, cli_api_key: Option<&str>) -> Vec<ModelEntry> {
    if has_cli_api_key_override(cli_api_key) {
        registry.models().to_vec()
    } else {
        registry.get_available()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_rpc_mode(
    session: AgentSession,
    resources: ResourceLoader,
    config: Config,
    available_models: Vec<ModelEntry>,
    scoped_models: Vec<pi::rpc::RpcScopedModel>,
    cli_api_key: Option<String>,
    auth: AuthStorage,
    runtime_handle: RuntimeHandle,
) -> Result<()> {
    use futures::FutureExt;

    let (abort_handle, abort_signal) = AbortHandle::new();
    let abort_listener = abort_handle.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        abort_listener.abort();
    }) {
        eprintln!("Warning: Failed to install Ctrl+C handler for RPC mode: {err}");
    }
    let rpc_task = pi::rpc::run_stdio(
        session,
        pi::rpc::RpcOptions {
            config,
            resources,
            available_models,
            scoped_models,
            cli_api_key,
            auth,
            runtime_handle,
        },
    )
    .fuse();

    let signal_task = abort_signal.wait().fuse();

    futures::pin_mut!(rpc_task, signal_task);

    match futures::future::select(rpc_task, signal_task).await {
        futures::future::Either::Left((result, _)) => match result {
            Ok(()) => Ok(()),
            Err(err) => Err(anyhow::Error::new(err)),
        },
        futures::future::Either::Right(((), _)) => {
            // Signal received, return Ok to trigger main_impl's shutdown flush
            Ok(())
        }
    }
}

async fn run_acp_mode(options: pi::acp::AcpOptions) -> Result<()> {
    use futures::FutureExt;

    let (abort_handle, abort_signal) = AbortHandle::new();
    let abort_listener = abort_handle.clone();
    if let Err(err) = ctrlc::set_handler(move || {
        abort_listener.abort();
    }) {
        eprintln!("Warning: Failed to install Ctrl+C handler for ACP mode: {err}");
    }
    let acp_task = pi::acp::run_stdio(options).fuse();
    let signal_task = abort_signal.wait().fuse();

    futures::pin_mut!(acp_task, signal_task);

    match futures::future::select(acp_task, signal_task).await {
        futures::future::Either::Left((result, _)) => match result {
            Ok(()) => Ok(()),
            Err(err) => Err(anyhow::Error::new(err)),
        },
        futures::future::Either::Right(((), _)) => Ok(()),
    }
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_print_mode(
    session: &mut AgentSession,
    mode: &str,
    initial: Option<InitialMessage>,
    messages: Vec<String>,
    resources: &ResourceLoader,
    runtime_handle: RuntimeHandle,
    config: &Config,
) -> Result<()> {
    if mode.ne("text") && mode.ne("json") {
        bail!("Unknown mode: {mode}");
    }

    if mode.eq("json") {
        let cx = pi::agent_cx::AgentCx::for_request();
        let session = session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("{}", serde_json::to_string(&session.header)?);
    }
    if initial.is_none() && messages.is_empty() {
        if mode.eq("json") {
            io::stdout().flush()?;
            return Ok(());
        }
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    let text_stream_state = Arc::new(StdMutex::new(PrintTextStreamState::default()));
    let extensions = session.extensions.as_ref().map(|r| r.manager().clone());
    let emit_json_events = mode.eq("json");
    let stream_text_events = mode.eq("text");
    let runtime_for_events = runtime_handle.clone();
    let text_stream_state_for_events = Arc::clone(&text_stream_state);
    let make_event_handler = move || {
        let extensions = extensions.clone();
        let runtime_for_events = runtime_for_events.clone();
        let text_stream_state = Arc::clone(&text_stream_state_for_events);
        let coalescer = extensions
            .as_ref()
            .map(|m| pi::extensions::EventCoalescer::new(m.clone()));
        move |event: AgentEvent| {
            if emit_json_events {
                if let Ok(serialized) = serde_json::to_string(&event) {
                    println!("{serialized}");
                }
            } else if stream_text_events
                && let Some(delta) = streamed_text_delta(&event)
                && emit_text_delta(delta).is_ok()
            {
                let mut guard = text_stream_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard.observe_delta(delta);
            }
            // Route non-lifecycle events through the coalescer for
            // batched/coalesced dispatch with lazy serialization.
            if let Some(coal) = &coalescer {
                coal.dispatch_agent_event_lazy(&event, &runtime_for_events);
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
        .filter(|message| !message.trim().is_empty())
        .collect::<Vec<_>>();

    if initial.is_none() && messages.is_empty() {
        if mode.eq("json") {
            io::stdout().flush()?;
            return Ok(());
        }
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    let retry_enabled = config.retry_enabled();
    let max_retries = config.retry_max_retries();
    let is_json = mode.eq("json");
    let mut sent_prompts = 0usize;

    if let Some(initial) = initial {
        let content = pi::app::build_initial_content(&initial);
        reset_print_text_stream_state(&text_stream_state);
        let message = run_print_prompt_with_retry(
            session,
            config,
            &abort_signal,
            &make_event_handler,
            retry_enabled,
            max_retries,
            is_json,
            &text_stream_state,
            PromptInput::Content(content),
        )
        .await?;
        sent_prompts = sent_prompts.saturating_add(1);
        if mode.eq("text") {
            finish_print_text_response(
                &message,
                snapshot_print_text_stream_state(&text_stream_state),
                config,
            )?;
        }
    }

    for message in messages {
        reset_print_text_stream_state(&text_stream_state);
        let response = run_print_prompt_with_retry(
            session,
            config,
            &abort_signal,
            &make_event_handler,
            retry_enabled,
            max_retries,
            is_json,
            &text_stream_state,
            PromptInput::Text(message),
        )
        .await?;
        sent_prompts = sent_prompts.saturating_add(1);
        if mode.eq("text") {
            finish_print_text_response(
                &response,
                snapshot_print_text_stream_state(&text_stream_state),
                config,
            )?;
        }
    }

    if sent_prompts.eq(&0) {
        if mode.eq("json") {
            io::stdout().flush()?;
            return Ok(());
        }
        bail!("No messages were sent");
    }

    io::stdout().flush()?;
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PrintTextStreamState {
    streamed_text: bool,
    ends_with_newline: bool,
}

impl PrintTextStreamState {
    fn observe_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.streamed_text = true;
        self.ends_with_newline = delta.ends_with('\n');
    }

    const fn should_render_final_message(self) -> bool {
        !self.streamed_text
    }

    const fn can_retry(self, is_json: bool) -> bool {
        is_json || !self.streamed_text
    }

    const fn needs_trailing_newline(self) -> bool {
        self.streamed_text && !self.ends_with_newline
    }
}

const fn streamed_text_delta(event: &AgentEvent) -> Option<&str> {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event: pi::model::AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => Some(delta.as_str()),
        _ => None,
    }
}

fn emit_text_delta(delta: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(delta.as_bytes())?;
    out.flush()
}

fn emit_trailing_print_newline(state: PrintTextStreamState) -> io::Result<()> {
    if !state.needs_trailing_newline() {
        return Ok(());
    }
    let stdout = io::stdout();
    let mut out = stdout.lock();
    out.write_all(b"\n")?;
    out.flush()
}

fn snapshot_print_text_stream_state(
    state: &Arc<StdMutex<PrintTextStreamState>>,
) -> PrintTextStreamState {
    *state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn reset_print_text_stream_state(state: &Arc<StdMutex<PrintTextStreamState>>) {
    let mut guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = PrintTextStreamState::default();
}

fn finish_print_text_response(
    message: &AssistantMessage,
    stream_state: PrintTextStreamState,
    config: &Config,
) -> Result<()> {
    if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
        emit_trailing_print_newline(stream_state)?;
        let error_message = message
            .error_message
            .clone()
            .unwrap_or_else(|| "Request error".to_string());
        bail!(error_message);
    }

    if stream_state.should_render_final_message() {
        // When stdout is a terminal, render markdown with formatting.
        // When piped, emit plain text via output_final_text to avoid escape codes.
        if std::io::IsTerminal::is_terminal(&io::stdout()) {
            let mut markdown = String::new();
            for block in &message.content {
                if let ContentBlock::Text(text) = block {
                    markdown.push_str(&text.text);
                    if !markdown.ends_with('\n') {
                        markdown.push('\n');
                    }
                }
            }

            if !markdown.is_empty() {
                let console = PiConsole::new();
                let code_block_indent = Some(config.markdown_code_block_indent() as usize);
                console.render_markdown_with_indent(&markdown, code_block_indent);
            }
        } else {
            pi::app::output_final_text(message);
        }
        return Ok(());
    }

    emit_trailing_print_newline(stream_state)?;
    Ok(())
}

/// Discriminated prompt input for retry helper.
enum PromptInput {
    Text(String),
    Content(Vec<ContentBlock>),
}

/// Compute retry delay with exponential backoff (mirrors RPC mode logic).
fn print_mode_retry_delay_ms(config: &Config, attempt: u32) -> u32 {
    let base = u64::from(config.retry_base_delay_ms());
    let max = u64::from(config.retry_max_delay_ms());
    let shift = attempt.saturating_sub(1);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let delay = base.saturating_mul(multiplier).min(max);
    u32::try_from(delay).unwrap_or(u32::MAX)
}

async fn sleep_with_current_timer(duration: Duration) {
    let now = asupersync::Cx::current()
        .and_then(|cx| cx.timer_driver())
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    asupersync::time::sleep(now, duration).await;
}

/// Emit a JSON-serialized [`AgentEvent`] to stdout (for JSON print mode).
fn emit_json_event(event: &AgentEvent) {
    if let Ok(serialized) = serde_json::to_string(event) {
        println!("{serialized}");
    }
}

/// Check whether a prompt result is a retryable error.
fn is_retryable_prompt_result(msg: &AssistantMessage) -> bool {
    if !matches!(msg.stop_reason, StopReason::Error) {
        return false;
    }
    let err_msg = msg.error_message.as_deref().unwrap_or("Request error");
    pi::error::is_retryable_error(err_msg, Some(msg.usage.input), None)
}

/// Execute a single prompt with automatic retry and `AutoRetryStart`/`AutoRetryEnd`
/// event emission. Mirrors the retry behaviour in RPC mode (`src/rpc.rs`).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_print_prompt_with_retry<H, EH>(
    session: &mut AgentSession,
    config: &Config,
    abort_signal: &pi::agent::AbortSignal,
    make_event_handler: &H,
    retry_enabled: bool,
    max_retries: u32,
    is_json: bool,
    text_stream_state: &Arc<StdMutex<PrintTextStreamState>>,
    input: PromptInput,
) -> Result<AssistantMessage>
where
    H: Fn() -> EH + Sync,
    EH: Fn(AgentEvent) + Send + Sync + 'static,
{
    // First attempt.
    let first_result = match &input {
        PromptInput::Text(text) => {
            session
                .run_text_with_abort(
                    text.clone(),
                    Some(abort_signal.clone()),
                    make_event_handler(),
                )
                .await
        }
        PromptInput::Content(content) => {
            session
                .run_with_content_with_abort(
                    content.clone(),
                    Some(abort_signal.clone()),
                    make_event_handler(),
                )
                .await
        }
    };

    // Fast path: no retry needed.
    if !retry_enabled {
        return first_result.map_err(anyhow::Error::new);
    }

    let mut retry_count: u32 = 0;
    let mut current_result = first_result;

    loop {
        match current_result {
            Ok(msg) if matches!(msg.stop_reason, StopReason::Aborted) => {
                if retry_count > 0 && is_json {
                    emit_json_event(&AgentEvent::AutoRetryEnd {
                        success: false,
                        attempt: retry_count,
                        final_error: Some("Aborted".to_string()),
                    });
                }
                return Ok(msg);
            }
            Ok(msg)
                if is_retryable_prompt_result(&msg)
                    && retry_count < max_retries
                    && snapshot_print_text_stream_state(text_stream_state).can_retry(is_json) =>
            {
                let err_msg = msg
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "Request error".to_string());

                retry_count += 1;
                let delay_ms = print_mode_retry_delay_ms(config, retry_count);
                if is_json {
                    emit_json_event(&AgentEvent::AutoRetryStart {
                        attempt: retry_count,
                        max_attempts: max_retries,
                        delay_ms: u64::from(delay_ms),
                        error_message: err_msg,
                    });
                }

                sleep_with_current_timer(Duration::from_millis(u64::from(delay_ms))).await;

                // Resume the turn instead of replaying it: strip only the failed
                // request's incomplete output (a transient drop leaves a partial
                // or error assistant), preserving the user prompt and every
                // completed tool cycle, then continue the loop. This re-issues
                // only the failed provider request — already-executed tool calls
                // are not re-run and prior work is not re-billed
                // (pi_agent_rust#125). Matches RPC retry behaviour.
                let _ = session.revert_incomplete_response().await;
                current_result = session
                    .run_continue_with_abort(Some(abort_signal.clone()), make_event_handler())
                    .await;
            }
            Ok(msg) => {
                // Success or non-retryable error or max retries reached.
                let success = !matches!(msg.stop_reason, StopReason::Error);
                if retry_count > 0 && is_json {
                    emit_json_event(&AgentEvent::AutoRetryEnd {
                        success,
                        attempt: retry_count,
                        final_error: if success {
                            None
                        } else {
                            msg.error_message.clone()
                        },
                    });
                }
                return Ok(msg);
            }
            Err(err) => {
                let err_str = err.to_string();
                // Classify from the TYPED error first (transient io::ErrorKind
                // via the source chain), then fall back to message-text matching
                // for prose-only errors (pi_agent_rust#118).
                if retry_count < max_retries
                    && (err.is_transient() || pi::error::is_retryable_error(&err_str, None, None))
                    && snapshot_print_text_stream_state(text_stream_state).can_retry(is_json)
                {
                    retry_count += 1;
                    let delay_ms = print_mode_retry_delay_ms(config, retry_count);
                    if is_json {
                        emit_json_event(&AgentEvent::AutoRetryStart {
                            attempt: retry_count,
                            max_attempts: max_retries,
                            delay_ms: u64::from(delay_ms),
                            error_message: err_str,
                        });
                    }

                    sleep_with_current_timer(Duration::from_millis(u64::from(delay_ms))).await;

                    // Resume the turn instead of replaying it (pi_agent_rust#125):
                    // strip only the failed request's incomplete output and
                    // continue from the last completed state. When the provider
                    // fails before emitting any assistant message there is nothing
                    // to strip and the resume simply re-issues the request — but
                    // any already-completed tool cycles from earlier in the turn
                    // are preserved rather than re-executed.
                    let _ = session.revert_incomplete_response().await;
                    current_result = session
                        .run_continue_with_abort(Some(abort_signal.clone()), make_event_handler())
                        .await;
                } else {
                    if retry_count > 0 && is_json {
                        emit_json_event(&AgentEvent::AutoRetryEnd {
                            success: false,
                            attempt: retry_count,
                            final_error: Some(err_str),
                        });
                    }
                    return Err(anyhow::Error::new(err));
                }
            }
        }
    }
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
    let interactive_result = pi::interactive::run_interactive(
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
    .await;
    // Explicitly shut down extension runtimes so the QuickJS GC can
    // collect all objects before JS_FreeRuntime asserts an empty gc_obj_list.
    // Must run even on error — otherwise ExtensionRegion::drop() runs
    // synchronously and the GC assertion fires.
    if let Some(ref region) = region {
        region.shutdown().await;
    }
    interactive_result?;
    Ok(())
}

type InitialMessage = pi::app::InitialMessage;

fn read_piped_stdin() -> Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut data = Vec::new();
    let mut handle = io::stdin().take(100 * 1024 * 1024); // 100MB limit
    handle.read_to_end(&mut data)?;
    if data.is_empty() {
        Ok(None)
    } else {
        Ok(Some(String::from_utf8_lossy(&data).into_owned()))
    }
}

fn format_token_count(count: u32) -> String {
    if count >= 1_000_000 {
        if (count % 1_000_000).eq(&0) {
            format!("{}M", count / 1_000_000)
        } else {
            let millions = f64::from(count) / 1_000_000.0;
            format!("{millions:.1}M")
        }
    } else if count >= 1_000 {
        if (count % 1_000).eq(&0) {
            format!("{}K", count / 1_000)
        } else {
            let thousands = f64::from(count) / 1_000.0;
            format!("{thousands:.1}K")
        }
    } else {
        count.to_string()
    }
}

#[cfg(test)]
fn fuzzy_match(pattern: &str, value: &str) -> bool {
    let mut needle = pattern
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace());
    let mut haystack = value.chars().flat_map(char::to_lowercase);
    for ch in needle.by_ref() {
        if !haystack.by_ref().any(|h| h.eq(&ch)) {
            return false;
        }
    }
    true
}

fn fuzzy_match_model_id(pattern: &str, provider: &str, model_id: &str) -> bool {
    let mut needle = pattern
        .chars()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace());
    let mut provider_chars = provider.chars().flat_map(char::to_lowercase);
    let mut model_chars = model_id.chars().flat_map(char::to_lowercase);

    for ch in needle.by_ref() {
        if provider_chars.by_ref().any(|h| h.eq(&ch)) {
            continue;
        }
        if model_chars.by_ref().any(|h| h.eq(&ch)) {
            continue;
        }
        return false;
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use serde_json::json;
    use tempfile::TempDir;

    fn spawn_auth_response_server(status: u16, body: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind auth fixture");
        let address = listener.local_addr().expect("auth fixture address");
        let body = body.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept auth request");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("bound auth request read");
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let reason = if status == 200 { "OK" } else { "Unauthorized" };
            write!(
                stream,
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .expect("write auth response");
        });
        format!("http://{address}/token")
    }

    fn render_model_table_for_test<R: ModelTableRow>(rows: &[R]) -> String {
        let mut buf = Vec::new();
        write_model_table(&mut buf, rows).expect("render model table");
        String::from_utf8(buf).expect("table output should be utf-8")
    }

    #[test]
    fn exit_code_classifier_marks_usage_errors() {
        let usage_err = anyhow!("Unknown --only categories: nope");
        assert_eq!(exit_code_for_error(&usage_err), EXIT_CODE_USAGE);

        let validation_err = anyhow::Error::new(pi::error::Error::validation("bad input"));
        assert_eq!(exit_code_for_error(&validation_err), EXIT_CODE_USAGE);
    }

    #[test]
    fn exit_code_classifier_defaults_to_general_failure() {
        let runtime_err = anyhow::Error::new(pi::error::Error::auth("missing key"));
        assert_eq!(exit_code_for_error(&runtime_err), EXIT_CODE_FAILURE);
    }

    #[test]
    fn fetch_models_api_key_override_has_highest_precedence() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let resolved = runtime
            .block_on(resolve_provider_api_key(
                "openai",
                Some("  explicit-cli-key  "),
            ))
            .expect("explicit key resolution");
        assert_eq!(resolved, "explicit-cli-key");
    }

    #[test]
    fn fetch_models_auth_load_failures_preserve_only_ambient_provider_keys() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let directory = TempDir::new().expect("tempdir");
        let invalid_utf8 = directory.path().join("invalid-auth.json");
        fs::write(&invalid_utf8, [0xff]).expect("write invalid UTF-8 auth fixture");
        let oversized = directory.path().join("oversized-auth.json");
        fs::write(&oversized, vec![b' '; 2 * 1024 * 1024]).expect("write oversized auth fixture");
        let nonregular = directory.path().join("directory-auth.json");
        fs::create_dir(&nonregular).expect("create non-regular auth fixture");

        for auth_path in [&invalid_utf8, &oversized, &nonregular] {
            let resolved = runtime
                .block_on(resolve_provider_api_key_with_auth_path_and_env(
                    "openai",
                    None,
                    (*auth_path).clone(),
                    |name| {
                        (name == "OPENAI_API_KEY").then(|| "  ambient-provider-value  ".to_string())
                    },
                ))
                .expect("ambient key remains usable after non-lock auth load failure");
            assert_eq!(resolved, "ambient-provider-value");
        }

        let absent = runtime
            .block_on(resolve_provider_api_key_with_auth_path_and_env(
                "openai",
                None,
                invalid_utf8,
                |_| None,
            ))
            .expect("missing ambient key remains an explicit empty result");
        assert!(absent.is_empty());
    }

    #[test]
    fn fetch_models_refreshes_expired_oauth_for_requested_provider() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let directory = TempDir::new().expect("tempdir");
        let auth_path = directory.path().join("auth.json");
        let token_url = spawn_auth_response_server(
            200,
            r#"{"access_token":"refreshed-catalog-token","refresh_token":"next-refresh-token","expires_in":3600}"#,
        );
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
        auth.set(
            "custom-openai",
            AuthCredential::OAuth {
                extra: std::collections::HashMap::new(),
                access_token: "expired-catalog-token".to_string(),
                refresh_token: "catalog-refresh-token".to_string(),
                expires: 0,
                token_url: Some(token_url),
                client_id: Some("catalog-client".to_string()),
            },
        );
        auth.save().expect("save expired OAuth fixture");

        let resolved = runtime
            .block_on(resolve_provider_api_key_with_auth_path_and_env(
                "custom-openai",
                None,
                auth_path.clone(),
                |_| None,
            ))
            .expect("refresh requested provider OAuth");

        assert_eq!(resolved, "refreshed-catalog-token");
        let reloaded = AuthStorage::load(auth_path).expect("reload refreshed auth");
        assert_eq!(
            reloaded.api_key("custom-openai").as_deref(),
            Some("refreshed-catalog-token"),
            "the normal OAuth lifecycle must durably retain the refreshed credential"
        );
    }

    #[test]
    fn fetch_models_returns_requested_provider_oauth_refresh_failure() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let directory = TempDir::new().expect("tempdir");
        let auth_path = directory.path().join("auth.json");
        let token_url = spawn_auth_response_server(401, r#"{"error":"invalid_grant"}"#);
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
        auth.set(
            "custom-openai",
            AuthCredential::OAuth {
                extra: std::collections::HashMap::new(),
                access_token: "expired-catalog-token".to_string(),
                refresh_token: "rejected-refresh-token".to_string(),
                expires: 0,
                token_url: Some(token_url),
                client_id: Some("catalog-client".to_string()),
            },
        );
        auth.save().expect("save expired OAuth fixture");

        let error = runtime
            .block_on(resolve_provider_api_key_with_auth_path_and_env(
                "custom-openai",
                None,
                auth_path,
                |_| None,
            ))
            .expect_err("the requested provider refresh failure must be surfaced");

        let message = error.to_string();
        assert!(message.contains("custom-openai"), "{message}");
        assert!(message.contains("token refresh failed"), "{message}");
    }

    #[test]
    fn fetch_models_sap_auth_uses_stored_bearer_or_exchanges_service_key() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let directory = TempDir::new().expect("tempdir");
        let auth_path = directory.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path).expect("load auth");

        auth.set(
            "sap-ai-core",
            AuthCredential::BearerToken {
                token: "stored-sap-bearer".to_string(),
            },
        );
        let bearer = runtime
            .block_on(resolve_provider_api_key_from_auth("sap", &auth))
            .expect("resolve stored SAP bearer");
        assert_eq!(bearer, "stored-sap-bearer");

        let token_url =
            spawn_auth_response_server(200, r#"{"access_token":"exchanged-sap-token"}"#);
        auth.set(
            "sap-ai-core",
            AuthCredential::ServiceKey {
                client_id: Some("sap-client".to_string()),
                // ubs:ignore test fixture credential, not live secret.
                client_secret: Some("sap-secret".to_string()),
                token_url: Some(token_url),
                service_url: Some("https://api.ai.sap.example.com".to_string()),
            },
        );
        let exchanged = runtime
            .block_on(resolve_provider_api_key_from_auth("sap-ai-core", &auth))
            .expect("exchange SAP service credentials");
        assert_eq!(exchanged, "exchanged-sap-token");

        let explicit_token_url =
            spawn_auth_response_server(200, r#"{"access_token":"explicit-sap-token"}"#);
        let explicit_service_key = serde_json::json!({
            "clientid": "explicit-client",
            // ubs:ignore test fixture credential, not live secret.
            "clientsecret": "explicit-secret",
            "url": explicit_token_url,
            "serviceurls": {"AI_API_URL": "https://api.ai.sap.example.com"}
        })
        .to_string();
        let explicit = runtime
            .block_on(resolve_provider_api_key("sap", Some(&explicit_service_key)))
            .expect("exchange explicit SAP service key");
        assert_eq!(explicit, "explicit-sap-token");
    }

    #[test]
    fn fetch_models_option_scan_stops_at_positional_separator() {
        let raw_args = [
            "pi",
            "--fetch-models",
            "openai",
            "--",
            "--hide-cwd-in-prompt",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        let (cli, extension_flags) = parse_cli_args(raw_args.clone())
            .expect("parse result")
            .expect("parsed CLI");
        assert_eq!(cli.args, vec!["--hide-cwd-in-prompt"]);

        let error = validate_fetch_models_is_standalone(&cli, &extension_flags, &raw_args)
            .expect_err("positional prompt remains incompatible with standalone fetch");
        let message = error.to_string();
        assert!(message.contains("prompt or file arguments"), "{message}");
        assert!(
            !message.contains("prompt or file arguments, --hide-cwd-in-prompt"),
            "the positional token must not be misclassified as an explicit flag: {message}"
        );
    }

    #[test]
    fn fetch_models_accepts_text_output_but_rejects_non_text_and_startup_resource_flags() {
        let text_args = [
            "pi",
            "--fetch-models",
            "openai",
            "--print",
            "--mode",
            "text",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        let (cli, extension_flags) = parse_cli_args(text_args.clone())
            .expect("parse result")
            .expect("parsed CLI");
        validate_fetch_models_is_standalone(&cli, &extension_flags, &text_args)
            .expect("redundant text output flags remain valid for standalone fetch");

        let conflicting_args = [
            "pi",
            "--fetch-models",
            "openai",
            "--mode",
            "json",
            "--no-migrations",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
        ]
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
        let (cli, extension_flags) = parse_cli_args(conflicting_args.clone())
            .expect("parse result")
            .expect("parsed CLI");

        let error = validate_fetch_models_is_standalone(&cli, &extension_flags, &conflicting_args)
            .expect_err("standalone fetch must reject silently ignored flags");
        let message = error.to_string();
        for expected in [
            "output-mode arguments",
            "--no-migrations",
            "resource-discovery disable arguments",
        ] {
            assert!(
                message.contains(expected),
                "missing {expected:?}: {message}"
            );
        }
    }

    #[test]
    fn file_fingerprint_binds_content_even_when_size_and_mtime_match() {
        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("models.fetched.json");
        fs::write(&path, b"first").expect("write first bytes");
        let original_mtime = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .expect("first mtime");
        let digest = |path: &Path| {
            let mut hasher = Sha256::new();
            assert!(append_file_fingerprint(&mut hasher, path));
            format!("{:x}", hasher.finalize())
        };
        let first = digest(&path);

        fs::write(&path, b"other").expect("replace with same-length bytes");
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(original_mtime))
            .expect("restore exact mtime");
        assert_eq!(fs::metadata(&path).expect("metadata").len(), 5);
        assert_ne!(
            first,
            digest(&path),
            "same-size, same-mtime replacements must invalidate the list-models cache"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_fingerprint_rejects_symlink_to_non_regular_input_without_opening_it() {
        use std::os::unix::fs::symlink;

        let directory = TempDir::new().expect("tempdir");
        let path = directory.path().join("models.fetched.json");
        symlink("/dev/zero", &path).expect("create device symlink");
        let mut hasher = Sha256::new();
        assert!(
            !append_file_fingerprint(&mut hasher, &path),
            "non-regular or symlinked cache inputs must disable the fast-path cache"
        );
    }

    #[test]
    fn parse_cli_args_extracts_extension_flags() {
        let parsed = parse_cli_args(vec![
            "pi".to_string(),
            "--model".to_string(),
            "gpt-4o".to_string(),
            "--plan".to_string(),
            "ship-it".to_string(),
            "--dry-run".to_string(),
            "--print".to_string(),
            "hello".to_string(),
        ])
        .expect("parse args")
        .expect("parsed cli payload");

        assert_eq!(parsed.0.model.as_deref(), Some("gpt-4o"));
        assert!(parsed.0.print);
        assert_eq!(parsed.1.len(), 2);
        assert_eq!(parsed.1[0].name, "plan");
        assert_eq!(parsed.1[0].value.as_deref(), Some("ship-it"));
        assert_eq!(parsed.1[1].name, "dry-run");
        assert!(parsed.1[1].value.is_none());
    }

    #[test]
    fn apply_extension_cli_flags_ignores_unknown_flags() {
        let manager = pi::extensions::ExtensionManager::new();
        let flags = vec![cli::ExtensionCliFlag {
            name: "plan".to_string(),
            value: Some("ship-it".to_string()),
        }];

        futures::executor::block_on(async {
            apply_extension_cli_flags(&manager, &flags)
                .await
                .expect("unknown extension flag should be ignored");
        });
    }

    #[test]
    fn parse_cli_args_keeps_subcommand_validation() {
        let result = parse_cli_args(vec![
            "pi".to_string(),
            "install".to_string(),
            "--bogus".to_string(),
            "pkg".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn fuzzy_match_model_id_matches_combined_haystack_behavior() {
        let cases = [
            ("g55", "openai-codex", "gpt-5.5"),
            ("oc55", "openai-codex", "gpt-5.5"),
            ("g54", "openai-codex", "gpt-5.4"),
            ("oc54", "openai-codex", "gpt-5.4"),
            ("g53", "openai-codex", "gpt-5.3-codex"),
            ("son46", "anthropic", "claude-sonnet-4-6"),
            ("opn router", "openrouter", "anthropic/claude-3.7-sonnet"),
            ("zzzz", "openai", "gpt-4o"),
            ("a4z", "anthropic", "claude-4"),
        ];

        for (pattern, provider, model_id) in cases {
            let combined = format!("{provider} {model_id}");
            assert_eq!(
                fuzzy_match_model_id(pattern, provider, model_id),
                fuzzy_match(pattern, &combined),
                "pattern={pattern} provider={provider} model_id={model_id}"
            );
        }
    }

    #[test]
    fn coerce_extension_flag_bool_defaults_to_true_without_value() {
        let flag = cli::ExtensionCliFlag {
            name: "dry-run".to_string(),
            value: None,
        };
        let value = coerce_extension_flag_value(&flag, "bool").expect("coerce bool");
        assert_eq!(value, Value::Bool(true));
    }

    #[test]
    fn coerce_extension_flag_rejects_invalid_bool_text() {
        let flag = cli::ExtensionCliFlag {
            name: "dry-run".to_string(),
            value: Some("maybe".to_string()),
        };
        let err = coerce_extension_flag_value(&flag, "bool").expect_err("invalid bool should fail");
        assert!(err.to_string().contains("Invalid boolean value"));
    }

    #[test]
    fn handle_package_update_rejects_blank_explicit_source() {
        let temp = TempDir::new().expect("tempdir");
        let manager = PackageManager::new(temp.path().to_path_buf());
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        let err = runtime
            .block_on(handle_package_update(&manager, Some("   ".to_string())))
            .expect_err("blank package update source should fail");

        assert!(err.to_string().contains("Package source must be non-empty"));
    }

    #[test]
    fn handle_package_update_errors_when_source_is_not_installed() {
        let temp = TempDir::new().expect("tempdir");
        let manager = PackageManager::new(temp.path().to_path_buf());
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");

        let err = runtime
            .block_on(handle_package_update(
                &manager,
                Some("npm:missing".to_string()),
            ))
            .expect_err("unknown explicit package source should fail");

        assert!(
            err.to_string()
                .contains("Package source not found: npm:missing")
        );
    }

    #[test]
    fn rpc_available_models_includes_remote_models_when_cli_api_key_is_present() {
        let temp = TempDir::new().expect("tempdir");
        let auth_path = temp.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("auth load");
        let registry = ModelRegistry::load(&auth, None);

        let without_cli_key = rpc_available_models(&registry, None);
        assert!(
            without_cli_key.iter().all(|entry| {
                !(entry.model.provider.eq("openai") && entry.model.id.eq("gpt-4o"))
            }),
            "OpenAI models should remain hidden without configured credentials"
        );

        let with_cli_key = rpc_available_models(&registry, Some("cli-override-key"));
        assert!(
            with_cli_key
                .iter()
                .any(|entry| entry.model.provider.eq("openai") && entry.model.id.eq("gpt-4o")),
            "CLI API-key override should expose remote models to RPC model switching"
        );
    }

    #[test]
    fn rpc_available_models_ignores_blank_cli_api_key_override() {
        let temp = TempDir::new().expect("tempdir");
        let auth_path = temp.path().join("auth.json");
        let auth = AuthStorage::load(auth_path).expect("auth load");
        let registry = ModelRegistry::load(&auth, None);

        let available_models = rpc_available_models(&registry, Some("   "));
        assert!(
            available_models.iter().all(|entry| {
                !(entry.model.provider.eq("openai") && entry.model.id.eq("gpt-4o"))
            }),
            "Blank CLI API-key values should not expose remote models"
        );
    }

    #[test]
    fn provider_choice_from_token_numbered_choices() {
        let choice = provider_choice_from_token("1").expect("provider 1");
        assert_eq!(choice.provider, "openai-codex");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("2").expect("provider 2");
        assert_eq!(choice.provider, "openai");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("3").expect("provider 3");
        assert_eq!(choice.provider, "anthropic");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("4").expect("provider 4");
        assert_eq!(choice.provider, "anthropic");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("5").expect("provider 5");
        assert_eq!(choice.provider, "kimi-for-coding");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthDeviceFlow);

        let choice = provider_choice_from_token("6").expect("provider 6");
        assert_eq!(choice.provider, "google-gemini-cli");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("7").expect("provider 7");
        assert_eq!(choice.provider, "google");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("8").expect("provider 8");
        assert_eq!(choice.provider, "google-antigravity");
        assert_eq!(choice.kind, SetupCredentialKind::OAuthPkce);

        let choice = provider_choice_from_token("9").expect("provider 9");
        assert_eq!(choice.provider, "azure-openai");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("10").expect("provider 10");
        assert_eq!(choice.provider, "openrouter");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);
        let choice = provider_choice_from_token("11").expect("provider 11");
        assert_eq!(choice.provider, "cohere");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("12").expect("provider 12");
        assert_eq!(choice.provider, "groq");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("13").expect("provider 13");
        assert_eq!(choice.provider, "deepseek");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        let choice = provider_choice_from_token("14").expect("provider 14");
        assert_eq!(choice.provider, "mistral");
        assert_eq!(choice.kind, SetupCredentialKind::ApiKey);

        // Out of range
        assert!(provider_choice_from_token("0").is_none());
        assert!(provider_choice_from_token("15").is_none());
    }

    #[test]
    fn provider_choice_from_token_common_nicknames() {
        assert_eq!(
            provider_choice_from_token("claude").unwrap().provider,
            "anthropic"
        );
        assert_eq!(
            provider_choice_from_token("gpt").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(
            provider_choice_from_token("chatgpt").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(
            provider_choice_from_token("gemini").unwrap().provider,
            "google"
        );
        assert_eq!(
            provider_choice_from_token("kimi").unwrap().provider,
            "kimi-for-coding"
        );
    }

    #[test]
    fn provider_choice_from_token_canonical_ids() {
        assert_eq!(
            provider_choice_from_token("anthropic").unwrap().provider,
            "anthropic"
        );
        assert_eq!(
            provider_choice_from_token("openai").unwrap().provider,
            "openai"
        );
        assert_eq!(
            provider_choice_from_token("openai-codex").unwrap().provider,
            "openai-codex"
        );
        assert_eq!(provider_choice_from_token("groq").unwrap().provider, "groq");
        assert_eq!(
            provider_choice_from_token("openrouter").unwrap().provider,
            "openrouter"
        );
        assert_eq!(
            provider_choice_from_token("mistral").unwrap().provider,
            "mistral"
        );
    }

    #[test]
    fn provider_choice_from_token_case_insensitive() {
        assert_eq!(
            provider_choice_from_token("ANTHROPIC").unwrap().provider,
            "anthropic"
        );
        assert_eq!(provider_choice_from_token("Groq").unwrap().provider, "groq");
        assert_eq!(
            provider_choice_from_token("OpenRouter").unwrap().provider,
            "openrouter"
        );
    }

    #[test]
    fn provider_choice_from_token_metadata_fallback() {
        // Providers not in the top-10 list but in provider_metadata registry
        assert_eq!(
            provider_choice_from_token("deepseek").unwrap().provider,
            "deepseek"
        );
        assert_eq!(
            provider_choice_from_token("cerebras").unwrap().provider,
            "cerebras"
        );
        assert_eq!(
            provider_choice_from_token("cohere").unwrap().provider,
            "cohere"
        );
        assert_eq!(
            provider_choice_from_token("perplexity").unwrap().provider,
            "perplexity"
        );
        // Aliases resolve through metadata
        assert_eq!(
            provider_choice_from_token("open-router").unwrap().provider,
            "openrouter"
        );
        assert_eq!(
            provider_choice_from_token("dashscope").unwrap().provider,
            "alibaba"
        );
    }

    #[test]
    fn collect_search_hits_filters_by_tag_before_limit() {
        let index = pi::extension_index::ExtensionIndex {
            schema: pi::extension_index::EXTENSION_INDEX_SCHEMA.to_string(),
            version: pi::extension_index::EXTENSION_INDEX_VERSION,
            generated_at: None,
            last_refreshed_at: None,
            entries: vec![
                pi::extension_index::ExtensionIndexEntry {
                    id: "npm/aaa-foo".to_string(),
                    name: "aaa-foo".to_string(),
                    description: Some("general extension".to_string()),
                    tags: vec!["general".to_string()],
                    license: None,
                    source: None,
                    install_source: Some("npm:aaa-foo".to_string()),
                },
                pi::extension_index::ExtensionIndexEntry {
                    id: "npm/zzz-foo".to_string(),
                    name: "zzz-foo".to_string(),
                    description: Some("automation extension".to_string()),
                    tags: vec!["automation".to_string()],
                    license: None,
                    source: None,
                    install_source: Some("npm:zzz-foo".to_string()),
                },
            ],
        };

        let hits = collect_search_hits(&index, Some("automation"), "relevance", 1, "foo");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.id, "npm/zzz-foo");
    }

    fn test_extension_index(
        entries: Vec<pi::extension_index::ExtensionIndexEntry>,
    ) -> pi::extension_index::ExtensionIndex {
        pi::extension_index::ExtensionIndex {
            schema: pi::extension_index::EXTENSION_INDEX_SCHEMA.to_string(),
            version: pi::extension_index::EXTENSION_INDEX_VERSION,
            generated_at: None,
            last_refreshed_at: None,
            entries,
        }
    }

    fn test_extension_entry(id: &str, name: &str) -> pi::extension_index::ExtensionIndexEntry {
        pi::extension_index::ExtensionIndexEntry {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            tags: Vec::new(),
            license: None,
            source: None,
            install_source: Some(format!("npm:{name}")),
        }
    }

    #[test]
    fn extension_safety_for_source_prefers_offline_index_metadata() {
        let mut index = test_extension_index(vec![pi::extension_index::ExtensionIndexEntry {
            id: "official/provider".to_string(),
            name: "provider".to_string(),
            description: None,
            tags: vec!["provider".to_string()],
            license: Some("MIT".to_string()),
            source: Some(pi::extension_index::ExtensionIndexSource::Git {
                repo: "https://github.com/badlogic/pi-mono".to_string(),
                path: Some("packages/coding-agent/examples/extensions/provider.ts".to_string()),
                r#ref: None,
            }),
            install_source: Some("npm:provider".to_string()),
        }]);
        index.generated_at = Some("2026-05-01T00:00:00Z".to_string());

        let safety = extension_safety_for_source("npm:provider", Some(&index));

        assert_eq!(safety.source_type, "official");
        assert_eq!(safety.license_status, "present");
        assert!(
            safety
                .registration_categories
                .contains(&"provider".to_string())
        );
        assert_eq!(safety.risk_profile, "elevated");
        assert_eq!(safety.source_confidence, "high");
    }

    #[test]
    fn extension_safety_lines_project_redacted_cli_provenance() {
        let safety = pi::extension_index::ExtensionSafetyProvenance {
            schema: pi::extension_index::EXTENSION_SAFETY_PROVENANCE_SCHEMA,
            source_type: "npm".to_string(),
            license_status: "present".to_string(),
            registration_categories: vec!["tool".to_string()],
            requested_capabilities: vec!["redacted-capability".to_string()],
            risk_profile: "unknown".to_string(),
            freshness: "offline".to_string(),
            source_confidence: "degraded".to_string(),
            degraded_reasons: vec!["redacted_capability_signal".to_string()],
        };

        let lines = extension_safety_lines(&safety);
        let rendered = lines.join("\n");

        assert!(rendered.contains("Safety: source=npm license=present"));
        assert!(rendered.contains("Signals: categories=tool capabilities=redacted-capability"));
        assert!(rendered.contains("Degraded: redacted_capability_signal"));
        assert!(!rendered.contains("OPENAI_API_KEY"));
        assert!(!rendered.contains("sk-should-not-appear"));
    }

    #[test]
    fn find_index_entry_by_name_or_id_returns_unique_fuzzy_hit() -> Result<(), String> {
        let index = test_extension_index(vec![
            test_extension_entry("npm/foo-helper", "foo-helper"),
            test_extension_entry("npm/bar-helper", "bar-helper"),
        ]);

        match find_index_entry_by_name_or_id(&index, "foo") {
            ExtensionInfoLookup::Found(entry) => assert_eq!(entry.id, "npm/foo-helper"),
            ExtensionInfoLookup::NotFound => {
                return Err("expected unique fuzzy match, got NotFound".to_string());
            }
            ExtensionInfoLookup::Ambiguous => {
                return Err("expected unique fuzzy match, got Ambiguous".to_string());
            }
        }
        Ok(())
    }

    #[test]
    fn find_index_entry_by_name_or_id_rejects_ambiguous_fuzzy_hit() {
        let index = test_extension_index(vec![
            test_extension_entry("npm/foo-alpha", "foo-alpha"),
            test_extension_entry("npm/foo-beta", "foo-beta"),
        ]);

        assert!(
            matches!(
                find_index_entry_by_name_or_id(&index, "foo"),
                ExtensionInfoLookup::Ambiguous
            ),
            "ambiguous fuzzy hits should fail safe instead of picking one arbitrarily"
        );
    }

    #[test]
    fn provider_choice_from_token_honors_method_preference() {
        let provider = provider_choice_from_token("anthropic oauth").expect("anthropic oauth");
        assert_eq!(provider.provider, "anthropic");
        assert_eq!(provider.kind, SetupCredentialKind::OAuthPkce);

        let provider = provider_choice_from_token("anthropic key").expect("anthropic key");
        assert_eq!(provider.provider, "anthropic");
        assert_eq!(provider.kind, SetupCredentialKind::ApiKey);
    }

    #[test]
    fn provider_choice_from_token_whitespace_handling() {
        assert_eq!(
            provider_choice_from_token("  groq  ").unwrap().provider,
            "groq"
        );
        assert_eq!(
            provider_choice_from_token(" 1 ").unwrap().provider,
            "openai-codex"
        );
    }

    #[test]
    fn provider_choice_from_token_unknown_returns_none() {
        assert!(provider_choice_from_token("nonexistent-provider-xyz").is_none());
        assert!(provider_choice_from_token("").is_none());
    }

    #[test]
    fn config_ui_app_empty_packages_shows_empty_message() {
        let result_slot = Arc::new(StdMutex::new(None));
        let app = ConfigUiApp::new(
            Vec::new(),
            "provider=(default)  model=(default)  thinking=(default)".to_string(),
            result_slot,
        );

        let view = app.view();
        assert!(
            view.contains("Pi Config UI"),
            "missing config ui header:\n{view}"
        );
        assert!(
            view.contains("No package resources discovered. Press Enter to exit."),
            "missing empty packages hint:\n{view}"
        );
    }

    #[test]
    fn config_ui_app_toggle_selected_updates_resource_state() {
        let result_slot = Arc::new(StdMutex::new(None));
        let mut app = ConfigUiApp::new(
            vec![ConfigPackageState {
                scope: SettingsScope::Project,
                source: "local:demo".to_string(),
                resources: vec![
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/a.js".to_string(),
                        enabled: true,
                    },
                    ConfigResourceState {
                        kind: ConfigResourceKind::Skills,
                        path: "skills/demo/SKILL.md".to_string(),
                        enabled: false,
                    },
                ],
            }],
            "provider=(default)  model=(default)  thinking=(default)".to_string(),
            result_slot,
        );

        assert!(
            app.packages[0].resources[0].enabled,
            "first resource should start enabled"
        );
        app.toggle_selected();
        assert!(
            !app.packages[0].resources[0].enabled,
            "toggling selected resource should flip enabled flag"
        );

        app.move_selection(1);
        app.toggle_selected();
        assert!(
            app.packages[0].resources[1].enabled,
            "second resource should toggle on after moving selection"
        );
    }

    #[test]
    fn format_settings_summary_uses_effective_config_values() {
        let config = Config {
            default_provider: Some("openai".to_string()),
            default_model: Some("gpt-4.1".to_string()),
            default_thinking_level: Some("high".to_string()),
            ..Config::default()
        };

        assert_eq!(
            format_settings_summary(&config),
            "provider=openai  model=gpt-4.1  thinking=high"
        );
    }

    #[test]
    fn interactive_config_settings_summary_with_roots_errors_on_invalid_settings() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path().join("repo");
        let global_dir = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&global_dir).expect("create global dir");
        std::fs::write(global_dir.join("settings.json"), "{not-json").expect("write settings");

        let err = interactive_config_settings_summary_with_roots(&cwd, &global_dir, None)
            .expect_err("invalid settings should be reported");

        assert!(
            err.to_string().contains("Failed to parse settings file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn persist_package_toggles_writes_filters_per_scope() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path().join("repo");
        let global_dir = temp.path().join("global");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&global_dir).expect("create global dir");
        std::fs::create_dir_all(cwd.join(".pi")).expect("create project .pi");

        std::fs::write(
            global_dir.join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "packages": ["npm:foo"]
            }))
            .expect("serialize global settings"),
        )
        .expect("write global settings");

        std::fs::write(
            cwd.join(".pi").join("settings.json"),
            serde_json::to_string_pretty(&json!({
                "packages": [
                    {
                        "source": "npm:bar",
                        "local": true,
                        "kind": "npm"
                    }
                ]
            }))
            .expect("serialize project settings"),
        )
        .expect("write project settings");

        let packages = vec![
            ConfigPackageState {
                scope: SettingsScope::Global,
                source: "npm:foo".to_string(),
                resources: vec![
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/a.js".to_string(),
                        enabled: true,
                    },
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/b.js".to_string(),
                        enabled: false,
                    },
                ],
            },
            ConfigPackageState {
                scope: SettingsScope::Project,
                source: "npm:bar".to_string(),
                resources: vec![ConfigResourceState {
                    kind: ConfigResourceKind::Skills,
                    path: "skills/demo/SKILL.md".to_string(),
                    enabled: true,
                }],
            },
        ];

        persist_package_toggles_with_roots(&cwd, &global_dir, None, &packages)
            .expect("persist package toggles");

        let global_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(global_dir.join("settings.json")).expect("read global"),
        )
        .expect("parse global json");
        let global_pkg = global_value["packages"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_object)
            .expect("global package object");
        assert_eq!(
            global_pkg
                .get("source")
                .and_then(serde_json::Value::as_str)
                .expect("source"),
            "npm:foo"
        );
        assert_eq!(
            global_pkg
                .get("extensions")
                .and_then(serde_json::Value::as_array)
                .expect("extensions")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["extensions/a.js"]
        );

        let project_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(cwd.join(".pi").join("settings.json")).expect("read project"),
        )
        .expect("parse project json");
        let project_pkg = project_value["packages"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(serde_json::Value::as_object)
            .expect("project package object");
        assert_eq!(
            project_pkg
                .get("source")
                .and_then(serde_json::Value::as_str)
                .expect("source"),
            "npm:bar"
        );
        assert_eq!(
            project_pkg
                .get("skills")
                .and_then(serde_json::Value::as_array)
                .expect("skills")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>(),
            vec!["skills/demo/SKILL.md"]
        );
        assert!(
            project_pkg
                .get("local")
                .and_then(serde_json::Value::as_bool)
                .expect("local")
        );
    }

    struct ConfigOverridePackageToggleFixture {
        _temp: TempDir,
        cwd: PathBuf,
        global_dir: PathBuf,
        override_path: PathBuf,
        global_original: String,
        project_original: String,
    }

    fn setup_config_override_package_toggle_fixture() -> ConfigOverridePackageToggleFixture {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path().join("repo");
        let global_dir = temp.path().join("global");
        let override_dir = temp.path().join("override");
        let override_path = override_dir.join("settings.json");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&global_dir).expect("create global dir");
        std::fs::create_dir_all(&override_dir).expect("create override dir");
        std::fs::create_dir_all(cwd.join(".pi")).expect("create project .pi");

        let global_original = serde_json::to_string_pretty(&json!({
            "packages": ["npm:global-default"]
        }))
        .expect("serialize global settings");
        std::fs::write(global_dir.join("settings.json"), &global_original)
            .expect("write global settings");

        let project_original = serde_json::to_string_pretty(&json!({
            "packages": ["npm:project-default"]
        }))
        .expect("serialize project settings");
        std::fs::write(cwd.join(".pi").join("settings.json"), &project_original)
            .expect("write project settings");

        std::fs::write(
            &override_path,
            serde_json::to_string_pretty(&json!({
                "packages": [
                    {
                        "source": "npm:override",
                        "kind": "npm",
                        "extensions": ["extensions/old.js"]
                    }
                ]
            }))
            .expect("serialize override settings"),
        )
        .expect("write override settings");

        ConfigOverridePackageToggleFixture {
            _temp: temp,
            cwd,
            global_dir,
            override_path,
            global_original,
            project_original,
        }
    }

    fn string_array_field<'a>(
        value: &'a serde_json::Value,
        field: &str,
        missing_message: &str,
    ) -> Vec<&'a str> {
        value
            .get(field)
            .and_then(serde_json::Value::as_array)
            .expect(missing_message)
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect()
    }

    fn assert_override_package(
        value: &serde_json::Value,
        expected_source: &str,
        field: &str,
        expected_paths: &[&str],
    ) {
        assert_eq!(
            value
                .get("source")
                .and_then(serde_json::Value::as_str)
                .expect("source"),
            expected_source
        );
        assert_eq!(
            string_array_field(value, field, field),
            expected_paths,
            "{field} mismatch for {expected_source}"
        );
    }

    #[test]
    fn persist_package_toggles_with_config_override_updates_override_only() {
        let fixture = setup_config_override_package_toggle_fixture();

        let packages = vec![
            ConfigPackageState {
                scope: SettingsScope::Global,
                source: "npm:override".to_string(),
                resources: vec![
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/new.js".to_string(),
                        enabled: true,
                    },
                    ConfigResourceState {
                        kind: ConfigResourceKind::Extensions,
                        path: "extensions/disabled.js".to_string(),
                        enabled: false,
                    },
                ],
            },
            // Defensive regression: a full config override uses one file, so mixed
            // scope package states must still be persisted together into that file.
            ConfigPackageState {
                scope: SettingsScope::Project,
                source: "npm:override-project".to_string(),
                resources: vec![ConfigResourceState {
                    kind: ConfigResourceKind::Skills,
                    path: "skills/demo/SKILL.md".to_string(),
                    enabled: true,
                }],
            },
        ];

        persist_package_toggles_with_roots(
            &fixture.cwd,
            &fixture.global_dir,
            Some(&fixture.override_path),
            &packages,
        )
        .expect("persist package toggles");

        let override_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&fixture.override_path).expect("read override"),
        )
        .expect("parse override json");
        let override_packages = override_value["packages"]
            .as_array()
            .expect("override packages array");
        assert_eq!(override_packages.len(), 2);

        assert_override_package(
            &override_packages[0],
            "npm:override",
            "extensions",
            &["extensions/new.js"],
        );
        assert_override_package(
            &override_packages[1],
            "npm:override-project",
            "skills",
            &["skills/demo/SKILL.md"],
        );

        assert_eq!(
            std::fs::read_to_string(fixture.global_dir.join("settings.json")).expect("read global"),
            fixture.global_original
        );
        assert_eq!(
            std::fs::read_to_string(fixture.cwd.join(".pi").join("settings.json"))
                .expect("read project"),
            fixture.project_original
        );
    }

    // ================================================================
    // Retry helper tests
    // ================================================================

    #[test]
    fn print_mode_retry_delay_first_attempt_is_base() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(3),
                base_delay_ms: Some(2000),
                max_delay_ms: Some(60_000),
            }),
            ..Config::default()
        };
        assert_eq!(print_mode_retry_delay_ms(&config, 1), 2000);
    }

    #[test]
    fn print_mode_retry_delay_doubles_each_attempt() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(5),
                base_delay_ms: Some(1000),
                max_delay_ms: Some(60_000),
            }),
            ..Config::default()
        };
        assert_eq!(print_mode_retry_delay_ms(&config, 2), 2000);
        assert_eq!(print_mode_retry_delay_ms(&config, 3), 4000);
    }

    #[test]
    fn print_mode_retry_delay_capped_at_max() {
        let config = Config {
            retry: Some(pi::config::RetrySettings {
                enabled: Some(true),
                max_retries: Some(10),
                base_delay_ms: Some(2000),
                max_delay_ms: Some(10_000),
            }),
            ..Config::default()
        };
        let delay = print_mode_retry_delay_ms(&config, 5);
        assert!(delay <= 10_000, "delay {delay} should be capped at 10000");
    }

    #[test]
    fn is_retryable_prompt_result_identifies_retryable_errors() {
        use pi::model::{AssistantMessage, Usage};

        let retryable = AssistantMessage {
            content: vec![],
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            stop_details: None,
            error_message: Some("429 rate limit exceeded".to_string()),
            timestamp: 0,
        };
        assert!(is_retryable_prompt_result(&retryable));

        let not_retryable = AssistantMessage {
            error_message: Some("invalid api key".to_string()),
            stop_details: None,
            ..retryable.clone()
        };
        assert!(!is_retryable_prompt_result(&not_retryable));

        let success = AssistantMessage {
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            ..retryable
        };
        assert!(!is_retryable_prompt_result(&success));
    }

    /// End-to-end (`pi_agent_rust#118`): a transient connection drop must be
    /// re-driven through the REAL retry-decision path. A provider surfaces a
    /// typed `io::Error` mid-stream; it is wrapped at the source by
    /// `Error::sse` (the last place the `io::ErrorKind` is known), then
    /// flattened to `AssistantMessage::error_message` exactly as
    /// `Agent::build_error_message` does, producing a `StopReason::Error`
    /// message. `is_retryable_prompt_result` — the function `main.rs`'s retry
    /// loop actually consults — must classify it retryable, even though the
    /// typed kind is gone by the time the message string is in hand.
    #[test]
    fn transient_connection_drop_retried_end_to_end() {
        use pi::model::{AssistantMessage, Usage};

        let build_error_turn = |flattened: String| AssistantMessage {
            content: vec![],
            api: "test".to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            usage: Usage::default(),
            stop_reason: StopReason::Error,
            stop_details: None,
            error_message: Some(flattened),
            timestamp: 0,
        };

        // Every transient io kind a dropped connection can surface, routed
        // through the real source wrapper + flatten, must be retried.
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::TimedOut,
        ] {
            let io_err = std::io::Error::new(kind, "opaque transport failure");
            let flattened = pi::error::Error::sse(&io_err).to_string();
            let turn = build_error_turn(flattened.clone());
            assert!(
                is_retryable_prompt_result(&turn),
                "{kind:?} drop should be retried, flattened: {flattened}"
            );
        }

        // The exact rustls close_notify string from the issue (prose fallback).
        let close_notify = build_error_turn(
            "API error: SSE error: tls connection closed without \
                 close_notify"
                .to_string(),
        );
        assert!(is_retryable_prompt_result(&close_notify));

        // A genuinely fatal stream error is NOT retried (no false positives).
        let fatal_io = std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid utf-8");
        let fatal = build_error_turn(pi::error::Error::sse(&fatal_io).to_string());
        assert!(!is_retryable_prompt_result(&fatal));
    }

    #[test]
    fn emit_json_event_serializes_retry_events() {
        let start = AgentEvent::AutoRetryStart {
            attempt: 1,
            max_attempts: 3,
            delay_ms: 2000,
            error_message: "rate limited".to_string(),
        };
        let json = serde_json::to_value(&start).unwrap();
        assert_eq!(json["type"], "auto_retry_start");
        assert_eq!(json["attempt"], 1);
        assert_eq!(json["maxAttempts"], 3);
        assert_eq!(json["delayMs"], 2000);

        let end = AgentEvent::AutoRetryEnd {
            success: true,
            attempt: 1,
            final_error: None,
        };
        let json = serde_json::to_value(&end).unwrap();
        assert_eq!(json["type"], "auto_retry_end");
        assert!(json["success"].as_bool().unwrap());
    }

    #[test]
    fn streamed_text_delta_only_matches_text_delta_updates() {
        let partial = Arc::new(AssistantMessage {
            content: vec![ContentBlock::Text(pi::model::TextContent::new("hello"))],
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            usage: pi::model::Usage::default(),
            stop_reason: StopReason::Stop,
            stop_details: None,
            error_message: None,
            timestamp: 0,
        });
        let delta_event = AgentEvent::MessageUpdate {
            message: pi::model::Message::Assistant(Arc::clone(&partial)),
            assistant_message_event: pi::model::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: " world".to_string(),
                partial,
            },
        };
        assert_eq!(streamed_text_delta(&delta_event), Some(" world"));

        let start_event = AgentEvent::MessageStart {
            message: pi::model::Message::assistant(AssistantMessage {
                content: Vec::new(),
                api: "test-api".to_string(),
                provider: "test-provider".to_string(),
                model: "test-model".to_string(),
                usage: pi::model::Usage::default(),
                stop_reason: StopReason::Stop,
                stop_details: None,
                error_message: None,
                timestamp: 0,
            }),
        };
        assert_eq!(streamed_text_delta(&start_event), None);
    }

    #[test]
    fn print_text_stream_state_tracks_visibility_newlines_and_retryability() {
        let mut state = PrintTextStreamState::default();
        assert!(state.should_render_final_message());
        assert!(state.can_retry(false));
        assert!(!state.needs_trailing_newline());

        state.observe_delta("");
        assert!(state.should_render_final_message());

        state.observe_delta("hello");
        assert!(!state.should_render_final_message());
        assert!(!state.can_retry(false));
        assert!(state.can_retry(true));
        assert!(state.needs_trailing_newline());

        state.observe_delta(" world\n");
        assert!(!state.needs_trailing_newline());
    }

    #[test]
    fn model_table_renderer_matches_cached_and_owned_rows() {
        let cached = vec![
            CachedModelRow {
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4-5".to_string(),
                context: "200k".to_string(),
                max_out: "8k".to_string(),
                thinking: "yes".to_string(),
                images: "yes".to_string(),
            },
            CachedModelRow {
                provider: "openai".to_string(),
                model: "gpt-5".to_string(),
                context: "128k".to_string(),
                max_out: "16k".to_string(),
                thinking: "no".to_string(),
                images: "yes".to_string(),
            },
        ];
        let owned = vec![
            (
                "anthropic".to_string(),
                "claude-sonnet-4-5".to_string(),
                "200k".to_string(),
                "8k".to_string(),
                "yes".to_string(),
                "yes".to_string(),
            ),
            (
                "openai".to_string(),
                "gpt-5".to_string(),
                "128k".to_string(),
                "16k".to_string(),
                "no".to_string(),
                "yes".to_string(),
            ),
        ];

        assert_eq!(
            render_model_table_for_test(&cached),
            render_model_table_for_test(&owned)
        );
    }

    #[test]
    fn model_table_renderer_supports_borrowed_cached_rows() {
        let cached = vec![
            CachedModelRow {
                provider: "openai".to_string(),
                model: "gpt-5".to_string(),
                context: "128k".to_string(),
                max_out: "16k".to_string(),
                thinking: "no".to_string(),
                images: "yes".to_string(),
            },
            CachedModelRow {
                provider: "openrouter".to_string(),
                model: "anthropic/claude-3.7-sonnet".to_string(),
                context: "200k".to_string(),
                max_out: "8k".to_string(),
                thinking: "yes".to_string(),
                images: "no".to_string(),
            },
        ];
        let borrowed = cached.iter().collect::<Vec<_>>();

        assert_eq!(
            render_model_table_for_test(&cached),
            render_model_table_for_test(&borrowed)
        );
    }
}
