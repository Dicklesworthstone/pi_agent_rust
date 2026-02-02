//! Pi - High-performance AI coding agent CLI
//!
//! Rust port of pi-mono (TypeScript) with emphasis on:
//! - Performance: Sub-100ms startup, smooth TUI at 60fps
//! - Reliability: No panics in normal operation
//! - Efficiency: Single binary, minimal dependencies

#![forbid(unsafe_code)]
// Allow dead code and unused async during scaffolding phase - remove once implementation is complete
#![allow(dead_code, clippy::unused_async)]

use std::io::{self, BufRead, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use pi::agent::{Agent, AgentConfig, AgentEvent};
use pi::auth::AuthStorage;
use pi::providers::anthropic::AnthropicProvider;
use pi::tools::ToolRegistry;
use pi::tui::PiConsole;
use pi::{cli, config, session};
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

async fn run(cli: cli::Cli) -> Result<()> {
    // Handle early exits
    if cli.version {
        print_version();
        return Ok(());
    }

    if let Some(pattern) = &cli.list_models {
        return list_models(pattern.as_deref()).await;
    }

    if let Some(export_path) = &cli.export {
        return export_session(export_path).await;
    }

    // Load configuration
    let config = config::Config::load()?;

    // Create session
    let session = session::Session::new(&cli, &config).await?;

    // Run the appropriate mode
    match cli.mode.as_deref() {
        Some("rpc") => run_rpc_mode(session).await,
        _ if cli.print => run_print_mode(session, &cli, &config).await,
        _ => run_interactive_mode(session).await,
    }
}

fn print_version() {
    println!(
        "pi {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
    );
}

async fn list_models(_pattern: Option<&str>) -> Result<()> {
    // TODO: Implement model listing
    println!("Model listing not yet implemented");
    Ok(())
}

async fn export_session(_path: &str) -> Result<()> {
    // TODO: Implement session export
    println!("Session export not yet implemented");
    Ok(())
}

async fn run_rpc_mode(_session: session::Session) -> Result<()> {
    // TODO: Implement RPC mode
    println!("RPC mode not yet implemented");
    Ok(())
}

async fn run_print_mode(
    _session: session::Session,
    cli: &cli::Cli,
    config: &config::Config,
) -> Result<()> {
    // Get the input message
    let input = get_input_message(cli)?;
    if input.is_empty() {
        anyhow::bail!("No input provided. Use: pi -p \"your message\" or pipe input via stdin");
    }

    // Get API key from auth storage with precedence: --api-key > auth.json > env
    let auth_path = dirs::home_dir()
        .map(|h| h.join(".pi/agent/auth.json"))
        .unwrap_or_else(|| std::path::PathBuf::from(".pi/agent/auth.json"));
    let auth = AuthStorage::load(auth_path).unwrap_or_else(|_| {
        AuthStorage::load(std::path::PathBuf::new()).unwrap()
    });
    let api_key = auth
        .resolve_api_key("anthropic", cli.api_key.as_deref())
        .ok_or_else(|| {
            anyhow::anyhow!("No API key found. Set ANTHROPIC_API_KEY, use --api-key, or add to ~/.pi/agent/auth.json")
        })?;

    // Create provider
    let model = cli.model.as_deref().unwrap_or("claude-sonnet-4-20250514");
    let provider = Arc::new(AnthropicProvider::new(model.to_string()));

    // Create tools
    let cwd = std::env::current_dir()?;
    let enabled_tools = cli.enabled_tools();
    let tools = ToolRegistry::new(&enabled_tools, &cwd, Some(config));

    // Create agent config with API key
    let stream_options = pi::provider::StreamOptions {
        api_key: Some(api_key),
        ..Default::default()
    };

    let agent_config = AgentConfig {
        system_prompt: cli.system_prompt.clone(),
        max_tool_iterations: 50,
        stream_options,
    };

    // Create agent
    let mut agent = Agent::new(provider, tools, agent_config);

    // Create console for styled output
    let console = PiConsole::new();

    // Run and stream output
    let result = agent
        .run(&input, move |event| {
            match event {
                AgentEvent::TextDelta { text } => {
                    console.render_text_delta(&text);
                }
                AgentEvent::ThinkingDelta { text } => {
                    console.render_thinking_delta(&text);
                }
                AgentEvent::ToolExecuteStart { name, .. } => {
                    console.render_tool_start(&name, "");
                }
                AgentEvent::ToolExecuteEnd { name, is_error, .. } => {
                    console.render_tool_end(&name, is_error);
                }
                AgentEvent::Error { error } => {
                    console.render_error(&error);
                }
                AgentEvent::Done { .. } => {
                    console.newline();
                }
                _ => {}
            }
        })
        .await;

    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            let console = PiConsole::new();
            console.render_error(&e.to_string());
            std::process::exit(1);
        }
    }
}

/// Get input message from CLI args or stdin.
fn get_input_message(cli: &cli::Cli) -> Result<String> {
    let mut parts = Vec::new();

    // Add file contents
    for file_arg in cli.file_args() {
        let path = PathBuf::from(file_arg);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", path.display(), e))?;
        parts.push(format!(
            "Contents of {}:\n```\n{}\n```",
            path.display(),
            content
        ));
    }

    // Add message args
    let messages: Vec<&str> = cli.message_args();
    if !messages.is_empty() {
        parts.push(messages.join(" "));
    }

    // Add stdin if not a TTY
    if !io::stdin().is_terminal() {
        let stdin = io::stdin();
        let mut stdin_content = String::new();
        for line in stdin.lock().lines() {
            stdin_content.push_str(&line?);
            stdin_content.push('\n');
        }
        if !stdin_content.trim().is_empty() {
            parts.push(stdin_content);
        }
    }

    Ok(parts.join("\n\n"))
}

async fn run_interactive_mode(_session: session::Session) -> Result<()> {
    // TODO: Implement interactive mode
    println!("Interactive mode not yet implemented");
    println!("Pi is being ported to Rust. Check back soon!");
    Ok(())
}
