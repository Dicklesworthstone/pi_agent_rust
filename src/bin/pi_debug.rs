//! Debug wrapper for pi that traces each step of the `run()` sequence.
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};
use asupersync::runtime::reactor::create_reactor;
use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
use asupersync::sync::Mutex;
use clap::Parser;
use pi::agent::{Agent, AgentConfig, AgentSession};
use pi::auth::AuthStorage;
use pi::cli;
use pi::compaction::ResolvedCompactionSettings;
use pi::config::Config;
use pi::models::{ModelRegistry, default_models_path};
use pi::package_manager::PackageManager;
use pi::providers;
use pi::resources::{ResourceCliOptions, ResourceLoader};
use pi::session::Session;
use pi::tools::ToolRegistry;

macro_rules! step {
    ($($arg:tt)*) => {
        eprintln!("[pi_debug] {}", format!($($arg)*));
    };
}

fn main() {
    if let Err(err) = main_impl() {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

fn main_impl() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .with_writer(io::stderr)
        .init();

    let cli = cli::Cli::parse();
    step!("CLI parsed: print={}", cli.print);

    let reactor = create_reactor()?;
    let runtime = RuntimeBuilder::multi_thread()
        .blocking_threads(1, 8)
        .with_reactor(reactor)
        .build()
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let handle = runtime.handle();
    let runtime_handle = handle.clone();

    let join = handle.spawn(Box::pin(run_debug(cli, runtime_handle)));
    runtime.block_on(join)
}

#[allow(clippy::too_many_lines)]
async fn run_debug(mut cli: cli::Cli, _runtime_handle: RuntimeHandle) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    step!("1. Loading config...");
    let config = Config::load()?;
    step!("   Config loaded");

    step!("2. Loading resources...");
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
        Ok(r) => {
            step!("   Resources loaded: {} extensions", r.extensions().len());
            r
        }
        Err(err) => {
            step!("   Resources failed: {err}");
            ResourceLoader::empty(config.enable_skill_commands())
        }
    };

    step!("3. Loading auth...");
    let mut auth = AuthStorage::load_async(Config::auth_path()).await?;
    step!("   Auth loaded");

    step!("4. Refreshing tokens...");
    auth.refresh_expired_oauth_tokens().await?;
    step!("   Tokens refreshed");

    step!("5. Loading models...");
    let global_dir = Config::global_dir();
    let package_dir = Config::package_dir();
    let models_path = default_models_path(&global_dir);
    let model_registry = ModelRegistry::load(&auth, Some(models_path.clone()));
    step!(
        "   Models loaded, {} available",
        model_registry.get_available().len()
    );

    step!("6. Reading piped stdin...");
    // Don't read stdin to avoid blocking
    let stdin_content: Option<String> = if io::stdin().is_terminal() {
        None
    } else {
        let mut data = String::new();
        io::stdin().read_to_string(&mut data)?;
        if data.is_empty() { None } else { Some(data) }
    };
    pi::app::apply_piped_stdin(&mut cli, stdin_content);
    pi::app::normalize_cli(&mut cli);
    step!("   CLI normalized");

    step!("7. Preparing initial message...");
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
    step!(
        "   Initial message prepared: {:?}",
        initial.as_ref().map(|_| "present")
    );

    step!("8. Creating session...");
    let session = Box::pin(Session::new(&cli, &config)).await?;
    step!("   Session created");

    step!("9. Selecting model...");
    let scoped_patterns = config.enabled_models.clone().unwrap_or_default();
    let scoped_models: Vec<_> = if scoped_patterns.is_empty() {
        Vec::new()
    } else {
        pi::app::resolve_model_scope(&scoped_patterns, &model_registry, cli.api_key.is_some())
    };

    let selection = pi::app::select_model_and_thinking(
        &cli,
        &config,
        &session,
        &model_registry,
        &scoped_models,
        &global_dir,
    );
    let selection = match selection {
        Ok(s) => {
            step!(
                "   Model selected: {}/{}",
                s.model_entry.model.provider,
                s.model_entry.model.id
            );
            s
        }
        Err(err) => {
            step!("   Model selection FAILED: {err}");
            return Err(err);
        }
    };

    step!("10. Resolving API key...");
    let resolved_key = match pi::app::resolve_api_key(&auth, &cli, &selection.model_entry) {
        Ok(key) => {
            step!("    API key resolved");
            key
        }
        Err(err) => {
            step!("    API key resolution FAILED: {err}");
            return Err(err);
        }
    };

    step!("11. Building agent...");
    let mut session = session;
    pi::app::update_session_for_selection(&mut session, &selection);
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
    step!("    Agent built");

    step!("12. Loading session history...");
    let history = {
        let cx = pi::agent_cx::AgentCx::for_request();
        step!("    Locking session mutex...");
        let session = agent_session
            .session
            .lock(cx.cx())
            .await
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        step!("    Session mutex locked");
        session.to_messages_for_current_path()
    };
    if !history.is_empty() {
        agent_session.agent.replace_messages(history.clone());
    }
    step!("    History loaded: {} messages", history.len());

    step!("13. Checking extensions...");
    if resources.extensions().is_empty() {
        step!("    No extensions");
    } else {
        step!(
            "    Enabling {} extensions...",
            resources.extensions().len()
        );
        agent_session
            .enable_extensions(&enabled_tools, &cwd, Some(&config), resources.extensions())
            .await
            .map_err(anyhow::Error::new)?;
        step!("    Extensions enabled");
    }

    step!("14. Running agent...");
    // Build a single input string from initial message and follow-up messages.
    let mut parts: Vec<String> = Vec::new();
    if let Some(initial) = initial {
        parts.push(resources.expand_input(&initial.text));
    }
    for msg in messages {
        parts.push(resources.expand_input(&msg));
    }
    let input = parts.join("\n\n");
    if input.is_empty() {
        bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    let result = agent_session
        .run_text(input, |event| {
            step!("    event: {event:?}");
        })
        .await
        .map_err(anyhow::Error::new)?;
    step!("    Stop reason: {:?}", result.stop_reason);
    for block in &result.content {
        step!("    Content block: {block:?}");
    }
    step!("15. Done!");
    Ok(())
}
