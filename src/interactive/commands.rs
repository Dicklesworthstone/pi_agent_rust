use super::*;

use crate::models::{
    ExtensionProviderBinding, ModelEntry, ModelRole, extension_provider_bindings,
    model_requires_configured_credential, normalize_api_key_opt,
};
use crate::provider_metadata::{
    ProviderMetadata, ProviderOnboardingMode, provider_ids_match, provider_metadata,
    split_provider_model_spec,
};

#[cfg(feature = "clipboard")]
use arboard::Clipboard as ArboardClipboard;

const BASH_COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExcludedBashPersistenceOutcome {
    Saved,
    Disabled,
    NotConfirmed {
        pending_mutations: Option<usize>,
        failed_flushes: Option<u64>,
    },
}

impl ExcludedBashPersistenceOutcome {
    fn warning_text(self) -> Option<String> {
        let Self::NotConfirmed {
            pending_mutations,
            failed_flushes,
        } = self
        else {
            return None;
        };

        let pending =
            pending_mutations.map_or_else(|| "unavailable".to_string(), |count| count.to_string());
        let failed =
            failed_flushes.map_or_else(|| "unavailable".to_string(), |count| count.to_string());
        Some(format!(
            "[Persistence warning]\n\
- Execution ended and may have performed side effects; do not rerun it to repair this save problem.\n\
- Session record: not confirmed saved.\n\
- Pending mutation slots (bounded/coalescing): {pending}\n\
- Total failed save attempts: {failed}"
        ))
    }
}

async fn persist_excluded_bash_execution(
    session: Arc<Mutex<Session>>,
    message: SessionMessage,
    save_enabled: bool,
    cx: &Cx,
) -> ExcludedBashPersistenceOutcome {
    let mut session_guard = match OwnedMutexGuard::lock(session, cx).await {
        Ok(guard) => guard,
        Err(err) => {
            tracing::error!(
                error = %err,
                "completed excluded-context bash command could not lock its session for recording"
            );
            return ExcludedBashPersistenceOutcome::NotConfirmed {
                pending_mutations: None,
                failed_flushes: None,
            };
        }
    };

    session_guard.append_message(message);
    if !save_enabled {
        return ExcludedBashPersistenceOutcome::Disabled;
    }

    if let Err(err) = session_guard.save().await {
        let metrics = session_guard.autosave_metrics();
        tracing::error!(
            error = %err,
            pending_mutations = metrics.pending_mutations,
            failed_flushes = metrics.flush_failed,
            "completed excluded-context bash command was retained in memory but its session save was not confirmed"
        );
        return ExcludedBashPersistenceOutcome::NotConfirmed {
            pending_mutations: Some(metrics.pending_mutations),
            failed_flushes: Some(metrics.flush_failed),
        };
    }

    ExcludedBashPersistenceOutcome::Saved
}

async fn persist_excluded_bash_execution_bounded(
    session: Arc<Mutex<Session>>,
    message: SessionMessage,
    save_enabled: bool,
    cx: &Cx,
) -> ExcludedBashPersistenceOutcome {
    asupersync::time::timeout(
        asupersync::time::wall_now(),
        BASH_COMPLETION_TIMEOUT,
        persist_excluded_bash_execution(session, message, save_enabled, cx),
    )
    .await
    .unwrap_or_else(|_| {
        tracing::error!(
            "completed excluded-context bash command exceeded its persistence cleanup budget"
        );
        ExcludedBashPersistenceOutcome::NotConfirmed {
            pending_mutations: None,
            failed_flushes: None,
        }
    })
}

async fn deliver_bash_result(
    event_tx: &asupersync::channel::mpsc::Sender<PiMsg>,
    cx: &Cx,
    message: PiMsg,
) {
    if !crate::interactive::enqueue_pi_event(event_tx, cx, message).await {
        tracing::error!("terminal bash result was not delivered before runtime shutdown");
    }
}

fn spawn_bash_completion(
    runtime_handle: &asupersync::runtime::RuntimeHandle,
    event_tx: asupersync::channel::mpsc::Sender<PiMsg>,
    persistence: Option<(Arc<Mutex<Session>>, SessionMessage, bool)>,
    mut display: String,
    content_for_agent: Option<Vec<ContentBlock>>,
) {
    if let Err(err) = runtime_handle.try_spawn_with_cx(move |completion_cx| async move {
        if let Some((session, message, save_enabled)) = persistence {
            let persistence = persist_excluded_bash_execution_bounded(
                session,
                message,
                save_enabled,
                &completion_cx,
            )
            .await;
            if let Some(warning) = persistence.warning_text() {
                display.push_str("\n\n");
                display.push_str(&warning);
            }
        }
        deliver_bash_result(
            &event_tx,
            &completion_cx,
            PiMsg::BashResult {
                display,
                content_for_agent,
            },
        )
        .await;
    }) {
        tracing::error!(
            error = %err,
            "terminal bash completion could not be admitted by the runtime"
        );
    }
}

/// Available slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Login,
    Logout,
    Clear,
    Model,
    Thinking,
    ScopedModels,
    Exit,
    History,
    Export,
    Session,
    Settings,
    Theme,
    Resume,
    New,
    Copy,
    Name,
    Hotkeys,
    Changelog,
    Tree,
    Fork,
    Compact,
    Reload,
    Template,
    Share,
    Mcp,
    Plan,
    Advisor,
    Checkpoint,
    Rewind,
    Fresh,
    Retry,
    Undo,
    Redo,
    Usage,
    Approval,
    Handoff,
    Rules,
    Omfg,
    Commit,
    Review,
    AddDir,
    RemoveDir,
    Crash,
    Btw,
    Tan,
}

impl SlashCommand {
    /// Parse a slash command from input.
    pub fn parse(input: &str) -> Option<(Self, &str)> {
        let input = input.trim();
        if !input.starts_with('/') {
            return None;
        }

        let (cmd, args) = input.split_once(char::is_whitespace).unwrap_or((input, ""));

        let command = match cmd.to_lowercase().as_str() {
            "/help" | "/h" | "/?" => Self::Help,
            "/login" => Self::Login,
            "/logout" => Self::Logout,
            "/clear" | "/cls" => Self::Clear,
            "/model" | "/m" => Self::Model,
            "/thinking" | "/think" | "/t" => Self::Thinking,
            "/scoped-models" | "/scoped" => Self::ScopedModels,
            "/exit" | "/quit" | "/q" => Self::Exit,
            "/history" | "/hist" => Self::History,
            "/export" => Self::Export,
            "/session" | "/info" => Self::Session,
            "/settings" => Self::Settings,
            "/theme" => Self::Theme,
            "/resume" | "/r" => Self::Resume,
            "/new" => Self::New,
            "/copy" | "/cp" => Self::Copy,
            "/name" => Self::Name,
            "/hotkeys" | "/keys" | "/keybindings" => Self::Hotkeys,
            "/changelog" => Self::Changelog,
            "/tree" => Self::Tree,
            "/fork" => Self::Fork,
            "/compact" => Self::Compact,
            "/reload" => Self::Reload,
            "/template" => Self::Template,
            "/share" => Self::Share,
            "/mcp" => Self::Mcp,
            "/plan" => Self::Plan,
            "/advisor" => Self::Advisor,
            "/checkpoint" | "/cp2" => Self::Checkpoint,
            "/rewind" => Self::Rewind,
            "/fresh" => Self::Fresh,
            "/retry" => Self::Retry,
            "/undo" => Self::Undo,
            "/redo" => Self::Redo,
            "/usage" => Self::Usage,
            "/approval" => Self::Approval,
            "/handoff" => Self::Handoff,
            "/review" => Self::Review,
            "/rules" => Self::Rules,
            "/add-dir" => Self::AddDir,
            "/remove-dir" => Self::RemoveDir,
            "/btw" => Self::Btw,
            "/tan" => Self::Tan,
            "/crash" => Self::Crash,
            "/omfg" => Self::Omfg,
            "/commit" => Self::Commit,
            _ => return None,
        };

        Some((command, args.trim()))
    }

    /// Get help text for all commands.
    pub const fn help_text() -> &'static str {
        r"Available commands:
  /help, /h, /?      - Show this help message
  /login [provider]  - Login/setup credentials; without provider shows status table
  /logout [provider] - Remove stored credentials
  /clear, /cls       - Clear conversation history
  /model, /m [id|provider/id] - Open model selector or switch directly
  /thinking, /t [level] - Set thinking level (off/minimal/low/medium/high/xhigh/max)
  /scoped-models [patterns|clear] - Show or set scoped models for cycling
  /history, /hist    - Show input history
  /export [path]     - Export conversation to HTML
  /session, /info    - Show session info (path, tokens, cost)
  /settings          - Open settings selector
  /theme [name]      - List or switch themes (dark/light/auto/custom)
  /resume, /r        - Pick and resume a previous session
  /new               - Start a new session
  /copy, /cp         - Copy last assistant message to clipboard
  /name <name>       - Set session display name
  /hotkeys, /keys    - Show keyboard shortcuts
  /changelog         - Show changelog entries
  /tree              - Show session branch tree summary
  /fork [id|index]   - Fork from a user message (default: last on current path)
  /compact [shake|aggressive] [notes] - Compact older context (shake: instant no-LLM tool-result dropping)
  /reload            - Reload skills/prompts from disk
  /template <name> [args] - Expand a prompt template by name
  /share             - Upload current branch to an unlisted gist (not private; inspect sensitive context)
  /mcp               - Manage MCP servers: list, add, remove, test, trust (Model Context Protocol)
  /plan [approve|reject|off|status] - Enter plan mode / review a submitted plan
  /approval [always-ask|write|yolo|status] - Set or show tool approval mode
  /handoff [to] [path] - Generate structured cross-session/cross-agent handoff brief
  /rules [list|remove|toggle] - Manage time-traveling stream rules (TTSR)
  /add-dir <dir>     - Grant access to an additional workspace root
  /remove-dir <dir>  - Revoke an additional workspace root immediately
  /crash [show|delete] - Inspect or clear redacted crash bundles
  /btw <question>    - Ephemeral side question on the smol role (never persisted)
  /tan <work>        - Run tangential work in a background task-role child
  /omfg <complaint>  - Record user grievance and draft a candidate stream rule
  /commit [dry-run|all|bead] - Create dependency-ordered atomic commits from changes
  /review [target]   - Run prioritized code review on changes with ship verdict card
  /advisor [status|pause|resume] - Manage the turn-review advisor model
  /undo [n] [force]  - Roll back the last n agent file edits (force: skip external-change guard)
  /redo [n] [force]  - Re-apply previously undone file edits
  /usage [refresh]   - Show provider usage/quota state
  /exit, /quit, /q   - Exit Pi

  Tips:
    • Use ↑/↓ arrows to navigate input history
    • Use Ctrl+L to open model selector
    • Use Ctrl+P to cycle scoped models
    • Use Shift+Enter (Ctrl+Enter on Windows) to insert a newline
    • Use PageUp/PageDown to scroll conversation history
    • Use Escape to cancel current input
    • Use /skill:name or /template to expand resources"
    }
}

pub(super) fn parse_extension_command(input: &str) -> Option<(String, &str)> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }

    // Built-in slash commands are handled elsewhere.
    if SlashCommand::parse(input).is_some() {
        return None;
    }

    let (cmd, rest) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    let cmd = cmd.trim_start_matches('/').trim();
    if cmd.is_empty() {
        return None;
    }
    Some((cmd.to_string(), rest.trim()))
}

pub(super) fn parse_bash_command(input: &str) -> Option<(String, bool)> {
    let trimmed = input.trim_start();
    let (rest, force) = trimmed
        .strip_prefix("!!")
        .map(|r| (r, true))
        .or_else(|| trimmed.strip_prefix('!').map(|r| (r, false)))?;
    let command = rest.trim();
    if command.is_empty() {
        return None;
    }
    Some((command.to_string(), force))
}

pub(super) fn normalize_api_key_input(raw: &str) -> std::result::Result<String, String> {
    let key = raw.trim();
    if key.is_empty() {
        return Err("API key cannot be empty".to_string());
    }
    if key.chars().any(char::is_whitespace) {
        return Err("API key must not contain whitespace".to_string());
    }
    Ok(key.to_string())
}

pub(super) fn normalize_auth_provider_input(raw: &str) -> String {
    let provider = raw.trim().to_ascii_lowercase();
    crate::provider_metadata::canonical_provider_id(&provider)
        .unwrap_or(provider.as_str())
        .to_string()
}

fn provider_has_dedicated_login_flow(provider: &str) -> bool {
    BUILTIN_LOGIN_PROVIDERS
        .iter()
        .any(|(builtin, _)| provider_ids_match(builtin, provider))
}

/// Choose the GitHub Copilot device flow over the browser flow when the
/// current process cannot rely on a localhost OAuth redirect — i.e. the
/// session is running headless / over SSH and the user's browser cannot reach
/// the callback server bound on this host. `PI_COPILOT_FORCE_DEVICE_FLOW=1`
/// opts in unconditionally.
///
/// When `GITHUB_COPILOT_CLIENT_ID` is unset we fall back to the well-known
/// public Copilot client id (`crate::auth::DEFAULT_COPILOT_CLIENT_ID`), so both
/// flows now succeed out of the box (#97). We still prefer the device flow when
/// no client id is explicitly configured, since that path is the most robust on
/// headless/SSH sessions where a localhost OAuth redirect can't be reached.
fn should_use_copilot_device_flow() -> bool {
    if std::env::var("PI_COPILOT_FORCE_DEVICE_FLOW")
        .is_ok_and(|v| matches!(v.as_str(), "1" | "true" | "yes"))
    {
        return true;
    }
    if std::env::var("GITHUB_COPILOT_CLIENT_ID").map_or(true, |v| v.trim().is_empty()) {
        return true;
    }
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

fn provider_supports_interactive_api_key_login(metadata: &ProviderMetadata) -> bool {
    if metadata.auth_env_keys.is_empty() || provider_has_dedicated_login_flow(metadata.canonical_id)
    {
        return false;
    }

    match metadata.onboarding {
        ProviderOnboardingMode::OpenAICompatiblePreset => metadata.routing_defaults.is_some(),
        ProviderOnboardingMode::BuiltInNative => metadata
            .routing_defaults
            .is_some_and(|defaults| !defaults.base_url.is_empty()),
        ProviderOnboardingMode::NativeAdapterRequired => false,
    }
}

fn generic_api_key_login_prompt(metadata: &ProviderMetadata) -> String {
    let provider = metadata.canonical_id;
    let label = metadata.display_name.unwrap_or(provider);
    let mut prompt = format!(
        "API key login: {provider}\n\n\
Paste your {label} API key to save it in auth.json under {provider}.\n"
    );

    if let Some(defaults) = metadata.routing_defaults
        && !defaults.base_url.is_empty()
    {
        let _ = writeln!(prompt, "Default base URL: {}", defaults.base_url);
    }

    if !metadata.auth_env_keys.is_empty() {
        let _ = writeln!(
            prompt,
            "Accepted env vars: {}",
            metadata.auth_env_keys.join(", ")
        );
    }

    prompt
        .push_str("\nYour input will be treated as sensitive and is not added to message history.");
    prompt
}

pub(super) fn api_key_login_prompt(provider: &str) -> Option<String> {
    match provider {
        "openai" => Some(String::from(
            "API key login: openai\n\n\
Paste your OpenAI API key to save it in auth.json.\n\
Get a key from platform.openai.com/api-keys.\n\
Rotate/revoke keys from that dashboard if compromised.\n\n\
Your input will be treated as sensitive and is not added to message history.",
        )),
        "google" => Some(String::from(
            "API key login: google/gemini\n\n\
Paste your Google Gemini API key to save it in auth.json under google.\n\
Get a key from ai.google.dev/gemini-api/docs/api-key.\n\
Rotate/revoke keys from Google AI Studio if compromised.\n\n\
Your input will be treated as sensitive and is not added to message history.",
        )),
        _ => provider_metadata(provider)
            .filter(|metadata| provider_supports_interactive_api_key_login(metadata))
            .map(generic_api_key_login_prompt),
    }
}

pub(super) fn save_provider_credential(
    auth: &mut crate::auth::AuthStorage,
    provider: &str,
    credential: crate::auth::AuthCredential,
) {
    let requested = provider.trim().to_ascii_lowercase();
    let canonical = normalize_auth_provider_input(&requested);
    let _ = auth.remove_provider_aliases(&requested);
    if requested != canonical {
        let _ = auth.remove_provider_aliases(&canonical);
    }
    auth.set(canonical.clone(), credential);
}

pub(super) fn remove_provider_credentials(
    auth: &mut crate::auth::AuthStorage,
    requested_provider: &str,
) -> bool {
    let requested = requested_provider.trim().to_ascii_lowercase();
    let canonical = normalize_auth_provider_input(&requested);

    let mut removed = auth.remove_provider_aliases(&canonical);
    if requested != canonical {
        removed |= auth.remove_provider_aliases(&requested);
    }
    removed
}

const BUILTIN_LOGIN_PROVIDERS: [(&str, &str); 7] = [
    ("anthropic", "OAuth"),
    ("openai-codex", "OAuth"),
    ("google-gemini-cli", "OAuth"),
    ("google-antigravity", "OAuth"),
    ("kimi-for-coding", "OAuth"),
    ("github-copilot", "OAuth"),
    ("gitlab", "OAuth"),
];

const STARTUP_PRIORITY_OAUTH_PROVIDERS: [(&str, &str); 3] = [
    ("anthropic", "Claude Code"),
    ("openai-codex", "Codex"),
    ("google-gemini-cli", "Gemini CLI"),
];

fn format_compact_duration(ms: i64) -> String {
    let seconds = (ms.max(0) / 1000).max(1);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else if seconds < 24 * 60 * 60 {
        format!("{}h", seconds / (60 * 60))
    } else {
        format!("{}d", seconds / (24 * 60 * 60))
    }
}

fn format_credential_status(status: &crate::auth::CredentialStatus) -> String {
    match status {
        crate::auth::CredentialStatus::Missing => "Not authenticated".to_string(),
        crate::auth::CredentialStatus::ApiKey
        | crate::auth::CredentialStatus::BearerToken
        | crate::auth::CredentialStatus::AwsCredentials
        | crate::auth::CredentialStatus::ServiceKey => "Authenticated".to_string(),
        crate::auth::CredentialStatus::OAuthValid { expires_in_ms } => {
            format!(
                "Authenticated (expires in {})",
                format_compact_duration(*expires_in_ms)
            )
        }
        crate::auth::CredentialStatus::OAuthExpired { expired_by_ms } => {
            format!(
                "Authenticated (expired {} ago)",
                format_compact_duration(*expired_by_ms)
            )
        }
    }
}

fn format_provider_status(auth: &crate::auth::AuthStorage, provider: &str) -> String {
    if let Some(source) = auth.external_setup_source(provider)
        && !auth.has_stored_credential(provider)
    {
        return format!("Auto-detected from {source}");
    }

    let status = auth.credential_status(provider);
    format_credential_status(&status)
}

fn collect_extension_oauth_providers(
    available_models: &[ModelEntry],
    registered_extension_bindings: &[ExtensionProviderBinding],
) -> Vec<String> {
    let mut providers = registered_extension_bindings
        .iter()
        .filter(|binding| binding.oauth_config.is_some())
        .map(|binding| {
            let provider = binding.provider.as_str();
            crate::provider_metadata::canonical_provider_id(provider)
                .unwrap_or(provider)
                .to_string()
        })
        .collect::<Vec<_>>();
    providers.extend(
        available_models
            .iter()
            .filter(|entry| entry.oauth_config.is_some())
            .map(|entry| {
                let provider = entry.model.provider.as_str();
                crate::provider_metadata::canonical_provider_id(provider)
                    .unwrap_or(provider)
                    .to_string()
            }),
    );

    providers.retain(|provider| {
        !BUILTIN_LOGIN_PROVIDERS
            .iter()
            .any(|(builtin, _)| provider == builtin)
    });
    providers.sort_unstable();
    providers.dedup();
    providers
}

fn extension_oauth_config_for_provider(
    available_models: &[ModelEntry],
    registered_extension_bindings: &[ExtensionProviderBinding],
    provider: &str,
) -> Option<crate::models::OAuthConfig> {
    registered_extension_bindings
        .iter()
        .find_map(|binding| {
            let registered_provider = binding.provider.as_str();
            let canonical = crate::provider_metadata::canonical_provider_id(registered_provider)
                .unwrap_or(registered_provider);
            if canonical.eq_ignore_ascii_case(provider) {
                binding.oauth_config.clone()
            } else {
                None
            }
        })
        .or_else(|| {
            available_models.iter().find_map(|entry| {
                let model_provider = entry.model.provider.as_str();
                let canonical = crate::provider_metadata::canonical_provider_id(model_provider)
                    .unwrap_or(model_provider);
                if canonical.eq_ignore_ascii_case(provider) {
                    entry.oauth_config.clone()
                } else {
                    None
                }
            })
        })
}

fn registered_extension_provider_bindings(
    extensions: Option<&ExtensionManager>,
) -> crate::error::Result<Vec<ExtensionProviderBinding>> {
    extensions.map_or_else(
        || Ok(Vec::new()),
        |manager| extension_provider_bindings(&manager.extension_providers()),
    )
}

fn append_provider_rows(output: &mut String, heading: &str, rows: &[(String, String, String)]) {
    let provider_width = rows
        .iter()
        .map(|(provider, _, _)| provider.len())
        .max()
        .unwrap_or("provider".len())
        .max("provider".len());
    let method_width = rows
        .iter()
        .map(|(_, method, _)| method.len())
        .max()
        .unwrap_or("method".len())
        .max("method".len());

    let _ = writeln!(output, "{heading}:");
    let _ = writeln!(
        output,
        "  {:<provider_width$}  {:<method_width$}  status",
        "provider", "method"
    );
    for (provider, method, status) in rows {
        let _ = writeln!(
            output,
            "  {provider:<provider_width$}  {method:<method_width$}  {status}"
        );
    }
}

pub(super) fn format_login_provider_listing(
    auth: &crate::auth::AuthStorage,
    available_models: &[ModelEntry],
    registered_extension_bindings: &[ExtensionProviderBinding],
) -> String {
    let mut output = String::from("Available login providers:\n\n");

    let mut built_in_rows: Vec<(String, String, String)> = BUILTIN_LOGIN_PROVIDERS
        .iter()
        .map(|(provider, method)| {
            (
                (*provider).to_string(),
                (*method).to_string(),
                format_provider_status(auth, provider),
            )
        })
        .collect();
    let mut api_key_rows: Vec<(String, String, String)> =
        crate::provider_metadata::PROVIDER_METADATA
            .iter()
            .filter(|meta| provider_supports_interactive_api_key_login(meta))
            .map(|meta| {
                let provider = meta.canonical_id.to_string();
                (
                    provider.clone(),
                    "API key".to_string(),
                    format_provider_status(auth, &provider),
                )
            })
            .collect();
    api_key_rows.sort_by(|left, right| left.0.cmp(&right.0));
    built_in_rows.extend(api_key_rows);
    append_provider_rows(&mut output, "Built-in", &built_in_rows);

    let extension_providers =
        collect_extension_oauth_providers(available_models, registered_extension_bindings);
    if !extension_providers.is_empty() {
        let extension_rows: Vec<(String, String, String)> = extension_providers
            .iter()
            .map(|provider| {
                (
                    provider.clone(),
                    "OAuth".to_string(),
                    format_provider_status(auth, provider),
                )
            })
            .collect();
        output.push('\n');
        append_provider_rows(&mut output, "Extension providers", &extension_rows);
    }

    output.push_str("\nUsage: /login <provider>");
    output
}

pub(super) fn format_startup_oauth_hint(auth: &crate::auth::AuthStorage) -> String {
    let mut output = String::new();
    output.push_str("  No provider credentials were detected.\n");
    output.push_str("  Connect one of these providers:\n");
    for (provider, label) in STARTUP_PRIORITY_OAUTH_PROVIDERS {
        let status = format_provider_status(auth, provider);
        let _ = writeln!(output, "  - {provider} ({label}): {status}");
    }
    output.push_str("  Use /login <provider> to connect or refresh credentials.\n");
    output.push_str("  Use /login to see all providers and auth methods.");
    output
}

pub(super) fn should_show_startup_oauth_hint(auth: &crate::auth::AuthStorage) -> bool {
    let has_any_credential = crate::provider_metadata::PROVIDER_METADATA
        .iter()
        .map(|meta| meta.canonical_id)
        .any(|provider| {
            auth.has_stored_credential(provider)
                || auth.external_setup_source(provider).is_some()
                || auth.resolve_api_key(provider, None).is_some()
        });
    if has_any_credential {
        return false;
    }

    STARTUP_PRIORITY_OAUTH_PROVIDERS
        .iter()
        .all(|(provider, _)| {
            auth.resolve_api_key(provider, None).is_none()
                && !auth.has_stored_credential(provider)
                && auth.external_setup_source(provider).is_none()
        })
}

pub fn strip_thinking_level_suffix(pattern: &str) -> &str {
    let Some((prefix, suffix)) = pattern.rsplit_once(':') else {
        return pattern;
    };
    match suffix.to_ascii_lowercase().as_str() {
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => prefix,
        _ => pattern,
    }
}

pub fn parse_scoped_model_patterns(args: &str) -> Vec<String> {
    args.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn model_entry_matches(left: &ModelEntry, right: &ModelEntry) -> bool {
    let left_provider = crate::provider_metadata::canonical_provider_id(&left.model.provider)
        .unwrap_or(&left.model.provider);
    let right_provider = crate::provider_metadata::canonical_provider_id(&right.model.provider)
        .unwrap_or(&right.model.provider);

    left_provider.eq_ignore_ascii_case(right_provider)
        && left.model.id.eq_ignore_ascii_case(&right.model.id)
}

pub(super) fn resolve_model_key_with_auth(
    auth: &crate::auth::AuthStorage,
    entry: &ModelEntry,
) -> Option<String> {
    normalize_api_key_opt(auth.resolve_api_key(&entry.model.provider, None))
        .or_else(|| normalize_api_key_opt(entry.api_key.clone()))
}

pub(super) fn resolve_model_key_from_default_auth(entry: &ModelEntry) -> Option<String> {
    let auth_path = crate::config::Config::auth_path();
    crate::auth::AuthStorage::load(auth_path)
        .ok()
        .and_then(|auth| resolve_model_key_with_auth(&auth, entry))
        .or_else(|| normalize_api_key_opt(entry.api_key.clone()))
}

fn session_thinking_level(
    session: &crate::session::Session,
) -> Option<crate::model::ThinkingLevel> {
    session
        .effective_thinking_level_for_current_path()
        .as_deref()
        .and_then(|value| value.parse::<crate::model::ThinkingLevel>().ok())
}

fn model_entry_event_payload(entry: &ModelEntry) -> Value {
    json!({
        "id": entry.model.id.clone(),
        "name": entry.model.name.clone(),
        "provider": entry.model.provider.clone(),
        "api": entry.model.api.clone(),
        "baseUrl": entry.model.base_url.clone(),
        "contextWindow": entry.model.context_window,
        "maxTokens": entry.model.max_tokens,
        "input": &entry.model.input,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionThinkingSyncPlan {
    effective: crate::model::ThinkingLevel,
    thinking_changed: bool,
    persist_needed: bool,
}

fn plan_session_thinking_sync(
    session_thinking: Option<&str>,
    current_thinking: crate::model::ThinkingLevel,
    target_entry: &ModelEntry,
) -> SessionThinkingSyncPlan {
    let parsed_session_thinking = session_thinking.and_then(|raw| {
        raw.parse::<crate::model::ThinkingLevel>().map_or_else(
            |_| {
                tracing::warn!("Ignoring invalid session thinking level: {raw}");
                None
            },
            Some,
        )
    });
    let requested_thinking = parsed_session_thinking.unwrap_or(current_thinking);
    let effective = target_entry.clamp_thinking_level(requested_thinking);
    let thinking_changed = effective != current_thinking;
    let persist_needed = if session_thinking.is_some() {
        parsed_session_thinking != Some(effective)
    } else {
        thinking_changed
    };

    SessionThinkingSyncPlan {
        effective,
        thinking_changed,
        persist_needed,
    }
}

fn parse_user_bash_event_result(value: &Value) -> Option<crate::tools::BashRunResult> {
    let result = value
        .as_object()
        .map_or(value, |obj| obj.get("result").unwrap_or(value));
    let obj = result.as_object()?;

    let output = obj
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let exit_code = obj
        .get("exitCode")
        .and_then(Value::as_i64)
        .or_else(|| obj.get("exit_code").and_then(Value::as_i64))
        .unwrap_or(0);
    let cancelled = obj
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let truncated = obj
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let full_output_path = obj
        .get("fullOutputPath")
        .or_else(|| obj.get("full_output_path"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let cancellation = obj.get("cancellation").and_then(Value::as_object);
    let cancellation_reason = cancellation
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .and_then(|reason| match reason {
            "timeout" => Some(crate::tools::BashCancellationReason::Timeout),
            "ambient_cancellation" => {
                Some(crate::tools::BashCancellationReason::AmbientCancellation)
            }
            _ => None,
        });
    let timeout_ms = cancellation
        .and_then(|details| details.get("timeoutMs"))
        .or_else(|| obj.get("timeoutMs"))
        .and_then(Value::as_u64);

    Some(crate::tools::BashRunResult {
        output,
        exit_code: i32::try_from(exit_code).unwrap_or(0),
        cancelled,
        cancellation_reason,
        timeout_ms,
        truncated,
        full_output_path,
        truncation: None,
    })
}

pub fn resolve_scoped_model_entries(
    patterns: &[String],
    available_models: &[ModelEntry],
) -> Result<Vec<ModelEntry>, String> {
    let mut resolved: Vec<ModelEntry> = Vec::new();

    for pattern in patterns {
        let raw_pattern = strip_thinking_level_suffix(pattern);
        let is_glob =
            raw_pattern.contains('*') || raw_pattern.contains('?') || raw_pattern.contains('[');

        if is_glob {
            let glob = Pattern::new(&raw_pattern.to_lowercase())
                .map_err(|err| format!("Invalid model pattern \"{pattern}\": {err}"))?;

            for entry in available_models {
                let full_id = format!("{}/{}", entry.model.provider, entry.model.id);
                let full_id_lower = full_id.to_lowercase();
                let id_lower = entry.model.id.to_lowercase();

                if (glob.matches(&full_id_lower) || glob.matches(&id_lower))
                    && !resolved
                        .iter()
                        .any(|existing| model_entry_matches(existing, entry))
                {
                    resolved.push(entry.clone());
                }
            }
            continue;
        }

        for entry in available_models {
            let full_id = format!("{}/{}", entry.model.provider, entry.model.id);
            if raw_pattern.eq_ignore_ascii_case(&full_id)
                || raw_pattern.eq_ignore_ascii_case(&entry.model.id)
            {
                if !resolved
                    .iter()
                    .any(|existing| model_entry_matches(existing, entry))
                {
                    resolved.push(entry.clone());
                }
                break;
            }
        }
    }

    resolved.sort_by(|a, b| {
        let left = format!("{}/{}", a.model.provider, a.model.id);
        let right = format!("{}/{}", b.model.provider, b.model.id);
        left.cmp(&right)
    });

    Ok(resolved)
}

pub(super) const fn kind_rank(kind: &DiagnosticKind) -> u8 {
    match kind {
        DiagnosticKind::Warning => 0,
        DiagnosticKind::Collision => 1,
    }
}

pub(super) fn format_resource_diagnostics(
    label: &str,
    diagnostics: &[ResourceDiagnostic],
) -> (String, usize) {
    let mut ordered: Vec<&ResourceDiagnostic> = diagnostics.iter().collect();
    ordered.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| kind_rank(&a.kind).cmp(&kind_rank(&b.kind)))
            .then_with(|| a.message.cmp(&b.message))
    });

    let mut out = String::new();
    let _ = writeln!(out, "{label}:");
    for diag in ordered {
        let kind = match diag.kind {
            DiagnosticKind::Warning => "warning",
            DiagnosticKind::Collision => "collision",
        };
        let _ = write!(out, "- {kind}: {} ({})", diag.message, diag.path.display());
        if let Some(collision) = &diag.collision {
            let _ = write!(
                out,
                " [winner: {} loser: {}]",
                collision.winner_path.display(),
                collision.loser_path.display()
            );
        }
        out.push('\n');
    }
    (out, diagnostics.len())
}

fn build_reload_diagnostics(
    models_error: Option<String>,
    resources: &ResourceLoader,
) -> (Option<String>, usize) {
    let mut sections = Vec::new();
    let mut count = 0usize;

    if let Some(err) = models_error {
        count = count.saturating_add(1);
        sections.push(format!("models.json:\n{err}"));
    }

    let mut resource_sections = Vec::new();
    let (skills_text, skills_count) =
        format_resource_diagnostics("Skills", resources.skill_diagnostics());
    if skills_count > 0 {
        resource_sections.push(skills_text);
        count = count.saturating_add(skills_count);
    }

    let (prompts_text, prompts_count) =
        format_resource_diagnostics("Prompts", resources.prompt_diagnostics());
    if prompts_count > 0 {
        resource_sections.push(prompts_text);
        count = count.saturating_add(prompts_count);
    }

    let (themes_text, themes_count) =
        format_resource_diagnostics("Themes", resources.theme_diagnostics());
    if themes_count > 0 {
        resource_sections.push(themes_text);
        count = count.saturating_add(themes_count);
    }

    if !resource_sections.is_empty() {
        sections.push(format!(
            "Resource diagnostics:\n{}",
            resource_sections.join("\n")
        ));
    }

    if sections.is_empty() {
        (None, 0)
    } else {
        (
            Some(format!("Reload diagnostics:\n\n{}", sections.join("\n\n"))),
            count,
        )
    }
}

/// Expand a leading `~`/`~/` to `$HOME` for /add-dir and /remove-dir.
/// `Path::join` on an absolute component replaces the base, so the slash
/// must be stripped before joining.
fn expand_home_path(raw: &str) -> std::path::PathBuf {
    let Ok(home) = std::env::var("HOME") else {
        return std::path::PathBuf::from(raw);
    };
    if raw == "~" {
        return std::path::PathBuf::from(home);
    }
    raw.strip_prefix("~/").map_or_else(
        || std::path::PathBuf::from(raw),
        |rest| std::path::PathBuf::from(home).join(rest),
    )
}

impl PiApp {
    /// Thinking level for a session started with `/new`: the configured
    /// default clamped to the current model, exactly as launch resolution
    /// does (issue #197 — this used to be hard-coded to `Off`, so a
    /// `defaultThinkingLevel: max` setup showed "[thinking: off]" after
    /// every `/new` even though thinking still ran).
    pub(super) fn new_session_thinking_level(&self) -> ThinkingLevel {
        let configured = self
            .config
            .default_thinking_level
            .as_deref()
            .and_then(|level| level.parse::<ThinkingLevel>().ok());
        self.model_entry
            .clamp_thinking_level(configured.unwrap_or(ThinkingLevel::XHigh))
    }

    pub(super) fn sync_active_provider_credentials(&mut self, changed_provider: &str) {
        let changed_canonical = normalize_auth_provider_input(changed_provider);
        let auth = match crate::auth::AuthStorage::load(crate::config::Config::auth_path()) {
            Ok(auth) => auth,
            Err(err) => {
                tracing::warn!(
                    event = "pi.auth.sync_credentials.load_failed",
                    provider = %changed_canonical,
                    error = %err,
                    "Skipping in-memory credential sync because auth storage could not be loaded"
                );
                return;
            }
        };

        let provider_matches_changed =
            |provider: &str| normalize_auth_provider_input(provider) == changed_canonical;

        if !provider_matches_changed(&self.model_entry.model.provider) {
            return;
        }

        // Keep catalog/model-scope entries immutable here so inline model keys
        // are never overwritten by transient auth state. We only refresh the
        // active runtime key.
        let fallback_inline_key = self
            .available_models
            .iter()
            .find(|entry| model_entry_matches(entry, &self.model_entry))
            .and_then(|entry| normalize_api_key_opt(entry.api_key.clone()))
            .or_else(|| normalize_api_key_opt(self.model_entry.api_key.clone()));

        let resolved_key_opt =
            normalize_api_key_opt(auth.resolve_api_key(&changed_canonical, None))
                .or(fallback_inline_key);

        if let Ok(mut agent_guard) = self.agent.try_lock() {
            agent_guard
                .stream_options_mut()
                .api_key
                .clone_from(&resolved_key_opt);
        }

        self.model_entry.api_key.clone_from(&resolved_key_opt);
        if let Ok(mut shared_entry) = self.model_entry_shared.lock() {
            shared_entry.api_key.clone_from(&resolved_key_opt);
        }

        // #81: when the active model no longer has resolvable
        // credentials (e.g. the user just `/logout`ed its provider, or
        // `/login`ed a different provider entirely), try to migrate
        // the active model so the next user message doesn't fail with
        // an auth error against the old provider. Preference order:
        //   1. A model from the just-changed provider (if it now has
        //      credentials — i.e. the user logged INTO that provider).
        //   2. Any model whose provider has stored credentials.
        // If no authenticated model is available we leave the active
        // model alone — the user will see the auth error and can run
        // `/login <provider>` to fix it.
        let model_still_authenticated = self
            .model_entry
            .api_key
            .as_deref()
            .is_some_and(|k| !k.is_empty())
            || !model_requires_configured_credential(&self.model_entry);
        if !model_still_authenticated {
            self.auto_switch_to_authenticated_model(&auth, &changed_canonical);
        }
    }

    /// Try to switch the active model to one that can actually
    /// authenticate now. Called from sync_active_provider_credentials
    /// when the current model lost its credentials. Best-effort: if
    /// any step fails we just leave the model where it is and let the
    /// regular auth-error flow surface the issue to the user.
    fn auto_switch_to_authenticated_model(
        &mut self,
        auth: &crate::auth::AuthStorage,
        preferred_provider: &str,
    ) {
        let preferred_canonical = normalize_auth_provider_input(preferred_provider);

        // Helper closure: given a candidate, return Some(resolved_key)
        // if it can authenticate now.
        let resolved_key_for = |entry: &ModelEntry| -> Option<String> {
            let resolved = auth.resolve_api_key(&entry.model.provider, None);
            normalize_api_key_opt(resolved).or_else(|| normalize_api_key_opt(entry.api_key.clone()))
        };

        // First: try a model from the just-changed provider.
        let preferred_match = self
            .available_models
            .iter()
            .find(|entry| {
                normalize_auth_provider_input(&entry.model.provider) == preferred_canonical
                    && resolved_key_for(entry).is_some()
            })
            .cloned();

        // Fallback: any model whose provider has stored credentials.
        let any_authenticated = preferred_match.or_else(|| {
            self.available_models
                .iter()
                .find(|entry| resolved_key_for(entry).is_some())
                .cloned()
        });

        let Some(next) = any_authenticated else {
            self.status_message = Some(format!(
                "Active model {}/{} is no longer authenticated. Run /login <provider> to restore access.",
                self.model_entry.model.provider, self.model_entry.model.id
            ));
            return;
        };

        if model_entry_matches(&next, &self.model_entry) {
            return; // nothing to do
        }

        let resolved_key_opt = resolved_key_for(&next);
        let provider_impl = match crate::providers::create_provider(&next, self.extensions.as_ref())
        {
            Ok(p) => p,
            Err(err) => {
                self.status_message = Some(format!("Auto-switch failed: {err}"));
                return;
            }
        };

        let previous_id = format!(
            "{}/{}",
            self.model_entry.model.provider, self.model_entry.model.id
        );
        if let Err(message) = self.switch_active_model(
            &next,
            provider_impl,
            resolved_key_opt.as_deref(),
            "auth-sync",
        ) {
            self.status_message = Some(format!(
                "Auto-switch from {previous_id} aborted: {message}. Use /model <provider/model> to pick one manually."
            ));
            return;
        }

        let next_id = format!("{}/{}", next.model.provider, next.model.id);
        self.status_message = Some(format!(
            "Active model auto-switched from {previous_id} to {next_id} because {previous_id} can no longer authenticate."
        ));
    }

    pub(super) fn switch_active_model(
        &mut self,
        next: &ModelEntry,
        provider_impl: std::sync::Arc<dyn crate::provider::Provider>,
        resolved_key_opt: Option<&str>,
        source: &str,
    ) -> Result<(), String> {
        let previous_entry = self.model_entry.clone();
        let Ok(mut agent_guard) = self.agent.try_lock() else {
            return Err("Agent busy; try again".to_string());
        };
        let Ok(mut session_guard) = self.session.try_lock() else {
            return Err("Session busy; try again".to_string());
        };
        let resolved_key_opt = resolved_key_opt.map(str::to_string);

        let current_thinking = agent_guard
            .stream_options()
            .thinking_level
            .unwrap_or_default();
        let next_thinking = next.clamp_thinking_level(current_thinking);
        let previous_thinking = session_thinking_level(&session_guard);

        agent_guard.set_provider(provider_impl);
        agent_guard.set_keyword_max_thinking_level(
            next.clamp_thinking_level(crate::model::ThinkingLevel::Max),
        );
        let stream_options = agent_guard.stream_options_mut();
        stream_options.api_key.clone_from(&resolved_key_opt);
        stream_options.headers.clone_from(&next.headers);
        // Pick up the new model's configured output cap so an interactive
        // model switch honors its registry `maxTokens` instead of carrying
        // over the previous model's limit.
        stream_options.max_tokens = Some(next.model.max_tokens);
        stream_options.thinking_level = Some(next_thinking);

        session_guard.header.provider = Some(next.model.provider.clone());
        session_guard.header.model_id = Some(next.model.id.clone());
        session_guard.append_model_change(next.model.provider.clone(), next.model.id.clone());
        session_guard.header.thinking_level = Some(next_thinking.to_string());
        if previous_thinking != Some(next_thinking) {
            session_guard.append_thinking_level_change(next_thinking.to_string());
        }

        drop(session_guard);
        drop(agent_guard);
        self.spawn_save_session();

        self.model_entry = next.clone();
        if let Ok(mut guard) = self.model_entry_shared.lock() {
            *guard = next.clone();
        }
        self.model = format!("{}/{}", next.model.provider, next.model.id);
        self.dispatch_model_select_event(next, Some(&previous_entry), source);
        Ok(())
    }

    fn dispatch_model_select_event(
        &self,
        next: &ModelEntry,
        previous: Option<&ModelEntry>,
        source: &str,
    ) {
        let Some(manager) = self.extensions.clone() else {
            return;
        };
        // gh #167: bump the ctx generation before dispatching so the
        // model_select handler (and every later event) sees the fresh
        // ctx.model instead of a payload cached for the previous model.
        manager.set_current_model(
            Some(next.model.provider.clone()),
            Some(next.model.id.clone()),
        );
        let runtime_handle = self.runtime_handle.clone();
        let source = match source {
            "selector" | "command" => "set",
            other => other,
        };
        let payload = json!({
            "model": model_entry_event_payload(next),
            "previousModel": previous.map(model_entry_event_payload),
            "source": source,
        });

        runtime_handle.spawn(async move {
            let _ = manager
                .dispatch_event(ExtensionEventName::ModelSelect, Some(payload))
                .await;
        });
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn sync_runtime_selection_from_session_header(&mut self) -> Result<(), String> {
        let previous_entry = self.model_entry.clone();
        let Ok(mut agent_guard) = self.agent.try_lock() else {
            return Err("Agent busy; try again".to_string());
        };
        let Ok(mut session_guard) = self.session.try_lock() else {
            return Err("Session busy; try again".to_string());
        };

        let session_model = session_guard.effective_model_for_current_path();
        let session_thinking = session_guard.effective_thinking_level_for_current_path();

        let (target_entry, sync_model) = match session_model.as_ref() {
            Some((provider, model_id)) => {
                if provider_ids_match(&self.model_entry.model.provider, provider)
                    && self.model_entry.model.id.eq_ignore_ascii_case(model_id)
                {
                    (self.model_entry.clone(), true)
                } else {
                    (
                        self.available_models
                            .iter()
                            .find(|entry| {
                                provider_ids_match(&entry.model.provider, provider)
                                    && entry.model.id.eq_ignore_ascii_case(model_id)
                            })
                            .cloned()
                            .ok_or_else(|| {
                                format!("Unable to switch provider/model to {provider}/{model_id}")
                            })?,
                        true,
                    )
                }
            }
            None => (self.model_entry.clone(), false),
        };

        let current_thinking = agent_guard
            .stream_options()
            .thinking_level
            .unwrap_or_default();
        let thinking_sync = plan_session_thinking_sync(
            session_thinking.as_deref(),
            current_thinking,
            &target_entry,
        );

        let provider = agent_guard.provider();
        let runtime_matches_target =
            provider_ids_match(provider.name(), &target_entry.model.provider)
                && provider
                    .model_id()
                    .eq_ignore_ascii_case(&target_entry.model.id);
        if !runtime_matches_target {
            let resolved_key_opt = target_entry
                .api_key
                .clone()
                .or_else(|| resolve_model_key_from_default_auth(&target_entry));
            if model_requires_configured_credential(&target_entry) && resolved_key_opt.is_none() {
                return Err(format!(
                    "Missing credentials for provider {}. Run /login {}.",
                    target_entry.model.provider, target_entry.model.provider
                ));
            }

            let provider_impl = providers::create_provider(&target_entry, self.extensions.as_ref())
                .map_err(|err| err.to_string())?;
            agent_guard.set_provider(provider_impl);
            let stream_options = agent_guard.stream_options_mut();
            stream_options.api_key.clone_from(&resolved_key_opt);
            stream_options.headers.clone_from(&target_entry.headers);
            // Pick up the new model's configured output cap so an interactive
            // model switch honors its registry `maxTokens` instead of carrying
            // over the previous model's limit.
            stream_options.max_tokens = Some(target_entry.model.max_tokens);
        }
        agent_guard.set_keyword_max_thinking_level(
            target_entry.clamp_thinking_level(crate::model::ThinkingLevel::Max),
        );
        agent_guard.set_tool_call_dialect(target_entry.tool_call_dialect());
        agent_guard.stream_options_mut().thinking_level = Some(thinking_sync.effective);
        drop(agent_guard);

        let persist_needed = if thinking_sync.persist_needed {
            let previous_thinking = session_thinking_level(&session_guard);
            session_guard.header.thinking_level = Some(thinking_sync.effective.to_string());
            if thinking_sync.thinking_changed && previous_thinking != Some(thinking_sync.effective)
            {
                session_guard.append_thinking_level_change(thinking_sync.effective.to_string());
            }
            true
        } else {
            false
        };
        drop(session_guard);

        let model_changed = if sync_model && !model_entry_matches(&self.model_entry, &target_entry)
        {
            self.model_entry = target_entry.clone();
            if let Ok(mut guard) = self.model_entry_shared.lock() {
                *guard = target_entry.clone();
            }
            self.model = format!("{}/{}", target_entry.model.provider, target_entry.model.id);
            true
        } else {
            false
        };

        if persist_needed {
            self.spawn_save_session();
        }

        if model_changed {
            self.dispatch_model_select_event(&target_entry, Some(&previous_entry), "restore");
        }

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn submit_oauth_code(
        &mut self,
        code_input: &str,
        pending: PendingOAuth,
    ) -> Option<Cmd> {
        // Do not store OAuth codes in history or session.
        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        self.agent_state = AgentState::Processing;
        self.scroll_to_bottom();

        let event_tx = self.event_tx.clone();
        let PendingOAuth {
            provider,
            kind,
            verifier,
            oauth_config,
            device_code,
            redirect_uri,
        } = pending;
        let code_input = code_input.to_string();

        let runtime_handle = self.runtime_handle.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        runtime_handle.spawn(async move {
            let auth_path = crate::config::Config::auth_path();
            let mut auth = match crate::auth::AuthStorage::load_async(auth_path).await {
                Ok(a) => a,
                Err(e) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::AgentError(e.to_string()),
                    )
                    .await;
                    return;
                }
            };

            let credential = match kind {
                PendingLoginKind::ApiKey => normalize_api_key_input(&code_input)
                    .map(|key| crate::auth::AuthCredential::ApiKey { key })
                    .map_err(crate::error::Error::auth),
                PendingLoginKind::OAuth => {
                    if provider == "anthropic" {
                        Box::pin(crate::auth::complete_anthropic_oauth(
                            &code_input,
                            &verifier,
                        ))
                        .await
                    } else if provider == "openai-codex" {
                        Box::pin(crate::auth::complete_openai_codex_oauth(
                            &code_input,
                            &verifier,
                        ))
                        .await
                    } else if provider == "google-gemini-cli" {
                        Box::pin(crate::auth::complete_google_gemini_cli_oauth(
                            &code_input,
                            &verifier,
                        ))
                        .await
                    } else if provider == "google-antigravity" {
                        Box::pin(crate::auth::complete_google_antigravity_oauth(
                            &code_input,
                            &verifier,
                        ))
                        .await
                    } else if provider == "github-copilot" || provider == "copilot" {
                        let client_id =
                            crate::auth::resolved_copilot_client_id();
                        let copilot_config = crate::auth::CopilotOAuthConfig {
                            client_id,
                            ..crate::auth::CopilotOAuthConfig::default()
                        };
                        Box::pin(crate::auth::complete_copilot_browser_oauth(
                            &copilot_config,
                            &code_input,
                            &verifier,
                            redirect_uri.as_deref(),
                        ))
                        .await
                    } else if provider == "gitlab" || provider == "gitlab-duo" {
                        let client_id = std::env::var("GITLAB_CLIENT_ID").unwrap_or_default();
                        let base_url = std::env::var("GITLAB_BASE_URL")
                            .unwrap_or_else(|_| "https://gitlab.com".to_string());
                        let gitlab_config = crate::auth::GitLabOAuthConfig {
                            client_id,
                            base_url,
                            ..crate::auth::GitLabOAuthConfig::default()
                        };
                        let gitlab_redirect_uri = redirect_uri
                            .clone()
                            .or_else(|| oauth_config.as_ref().and_then(|c| c.redirect_uri.clone()));
                        Box::pin(crate::auth::complete_gitlab_oauth(
                            &gitlab_config,
                            &code_input,
                            &verifier,
                            gitlab_redirect_uri.as_deref(),
                        ))
                        .await
                    } else if let Some(config) = &oauth_config {
                        Box::pin(crate::auth::complete_extension_oauth(
                            config,
                            &code_input,
                            &verifier,
                        ))
                        .await
                    } else {
                        Err(crate::error::Error::auth(format!(
                            "OAuth provider not supported: {provider}"
                        )))
                    }
                }
                PendingLoginKind::DeviceFlow => match device_code {
                    Some(dc) => {
                        let poll_result = if provider == "kimi-for-coding" {
                            Box::pin(crate::auth::poll_kimi_code_device_flow(&dc)).await
                        } else if provider == "github-copilot" || provider == "copilot" {
                            let client_id =
                                crate::auth::resolved_copilot_client_id();
                            let copilot_config = crate::auth::CopilotOAuthConfig {
                                client_id,
                                ..crate::auth::CopilotOAuthConfig::default()
                            };
                            Box::pin(crate::auth::poll_copilot_device_flow(&copilot_config, &dc))
                                .await
                        } else {
                            crate::auth::DeviceFlowPollResult::Error(format!(
                                "Device flow polling not supported for {provider}"
                            ))
                        };
                        match poll_result {
                            crate::auth::DeviceFlowPollResult::Success(cred) => Ok(cred),
                            crate::auth::DeviceFlowPollResult::Error(e) => {
                                Err(crate::error::Error::auth(e))
                            }
                            crate::auth::DeviceFlowPollResult::Expired => {
                                Err(crate::error::Error::auth(format!(
                                    "Device code expired for {provider}. Run /login {provider} again."
                                )))
                            }
                            crate::auth::DeviceFlowPollResult::AccessDenied => {
                                Err(crate::error::Error::auth(format!(
                                    "Access denied for {provider}."
                                )))
                            }
                            crate::auth::DeviceFlowPollResult::Pending => {
                                Err(crate::error::Error::auth(format!(
                                    "Authorization for {provider} is still pending. Complete the browser step and submit again."
                                )))
                            }
                            crate::auth::DeviceFlowPollResult::SlowDown => {
                                Err(crate::error::Error::auth(format!(
                                    "Authorization server asked to slow down for {provider}. Wait a few seconds and submit again."
                                )))
                            }
                        }
                    }
                    None => Err(crate::error::Error::auth(
                        "Device flow missing device_code".to_string(),
                    )),
                },
            };

            let credential = match credential {
                Ok(c) => c,
                Err(e) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::AgentError(e.to_string()),
                    )
                    .await;
                    return;
                }
            };

            save_provider_credential(&mut auth, &provider, credential);
            if let Err(e) = auth.save_async().await {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &task_cx,
                    PiMsg::AgentError(e.to_string()),
                )
                .await;
                return;
            }
            let _ = crate::interactive::enqueue_pi_event(
                &event_tx,
                &task_cx,
                PiMsg::CredentialUpdated {
                    provider: provider.clone(),
                },
            )
            .await;

            let status = match kind {
                PendingLoginKind::ApiKey => {
                    format!("API key saved for {provider}. Credentials saved to auth.json.")
                }
                PendingLoginKind::OAuth | PendingLoginKind::DeviceFlow => {
                    format!(
                        "OAuth login successful for {provider}. Credentials saved to auth.json."
                    )
                }
            };
            let _ = crate::interactive::enqueue_pi_event(
                &event_tx,
                &task_cx,
                PiMsg::System(status),
            )
            .await;
        });

        None
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn submit_bash_command(
        &mut self,
        raw_message: &str,
        command: String,
        exclude_from_context: bool,
    ) -> Option<Cmd> {
        if self.bash_running {
            self.status_message = Some("A bash command is already running.".to_string());
            return None;
        }

        self.bash_running = true;
        self.agent_state = AgentState::ToolRunning;
        self.current_tool = Some("bash".to_string());
        self.history.push(raw_message.to_string());

        self.input.reset();
        self.input_mode = InputMode::SingleLine;
        self.set_input_height(3);

        let event_tx = self.event_tx.clone();
        let session = Arc::clone(&self.session);
        let save_enabled = self.save_enabled;
        let cwd = self.cwd.clone();
        let cwd_display = cwd.display().to_string();
        let shell_path = self.config.shell_path.clone();
        let command_prefix = self.config.shell_command_prefix.clone();
        let extensions = self.extensions.clone();
        let runtime_handle = self.runtime_handle.clone();
        let completion_runtime_handle = runtime_handle.clone();

        runtime_handle.spawn_with_cx(move |_command_cx| async move {
            let mut override_result = None;
            if let Some(manager) = extensions {
                let response = manager
                    .dispatch_event_with_response(
                        ExtensionEventName::UserBash,
                        Some(json!({
                            "command": command.clone(),
                            "excludeFromContext": exclude_from_context,
                            "cwd": cwd_display,
                        })),
                        EXTENSION_EVENT_TIMEOUT_MS,
                    )
                    .await
                    .unwrap_or(None);
                if let Some(value) = response {
                    override_result = parse_user_bash_event_result(&value);
                }
            }

            let result = match override_result {
                Some(result) => Ok(result),
                None => {
                    crate::tools::run_bash_command(
                        &cwd,
                        shell_path.as_deref(),
                        command_prefix.as_deref(),
                        &command,
                        None,
                        None,
                    )
                    .await
                }
            };
            match result {
                Ok(result) => {
                    let display = bash_execution_to_text(
                        &command,
                        &result.output,
                        result.exit_code,
                        result.cancelled,
                        result.truncated,
                        result.full_output_path.as_deref(),
                    );

                    if exclude_from_context {
                        let mut extra = HashMap::new();
                        extra.insert("excludeFromContext".to_string(), Value::Bool(true));

                        let bash_message = SessionMessage::BashExecution {
                            command: command.clone(),
                            output: result.output.clone(),
                            exit_code: result.exit_code,
                            cancelled: Some(result.cancelled),
                            truncated: Some(result.truncated),
                            full_output_path: result.full_output_path,
                            timestamp: Some(Utc::now().timestamp_millis()),
                            extra,
                        };

                        let mut display = display;
                        display.push_str("\n\n[Output excluded from model context]");
                        spawn_bash_completion(
                            &completion_runtime_handle,
                            event_tx.clone(),
                            Some((Arc::clone(&session), bash_message, save_enabled)),
                            display,
                            None,
                        );
                    } else {
                        let content_for_agent =
                            vec![ContentBlock::Text(TextContent::new(display.clone()))];
                        spawn_bash_completion(
                            &completion_runtime_handle,
                            event_tx.clone(),
                            None,
                            display,
                            Some(content_for_agent),
                        );
                    }
                }
                Err(err) => {
                    spawn_bash_completion(
                        &completion_runtime_handle,
                        event_tx,
                        None,
                        format!("Bash command failed: {err}"),
                        None,
                    );
                }
            }
        });

        None
    }

    pub(super) fn format_themes_list(&self) -> String {
        let mut names = Vec::new();
        names.push("dark".to_string());
        names.push("light".to_string());
        names.push("solarized".to_string());

        for path in Theme::discover_themes(&self.cwd) {
            if let Ok(theme) = Theme::load(&path) {
                names.push(theme.name);
            }
        }

        names.sort_by_key(|a| a.to_ascii_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

        let mut output = String::from("Available themes:\n");
        for name in names {
            let marker = if name.eq_ignore_ascii_case(&self.theme.name) {
                "* "
            } else {
                "  "
            };
            let _ = writeln!(output, "{marker}{name}");
        }
        output.push_str(
            "\nUse /theme <name> to switch, or /theme auto to match the terminal background",
        );
        output
    }

    pub(super) fn format_scoped_models_status(&self) -> String {
        let patterns = self.config.enabled_models.as_deref().unwrap_or(&[]);
        let scope_configured = !patterns.is_empty();

        let mut output = String::new();
        let current = format!(
            "{}/{}",
            self.model_entry.model.provider, self.model_entry.model.id
        );
        let _ = writeln!(output, "Current model: {current}");
        let _ = writeln!(output);

        if !scope_configured {
            let _ = writeln!(output, "Scoped models: (all models)");
            let _ = writeln!(output);
            output.push_str("Use /scoped-models <patterns> to scope Ctrl+P cycling.\n");
            output.push_str("Use /scoped-models clear to clear scope.\n");
            return output;
        }

        output.push_str("Scoped model patterns:\n");
        for pattern in patterns {
            let _ = writeln!(output, "  - {pattern}");
        }
        let _ = writeln!(output);

        output.push_str("Scoped models (matched):\n");
        if self.model_scope.is_empty() {
            output.push_str("  (none)\n");
        } else {
            let mut models = self
                .model_scope
                .iter()
                .map(|entry| format!("{}/{}", entry.model.provider, entry.model.id))
                .collect::<Vec<_>>();
            models.sort_by_key(|value| value.to_ascii_lowercase());
            models.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
            for model in models {
                let _ = writeln!(output, "  - {model}");
            }
        }
        let _ = writeln!(output);

        output.push_str("Use /scoped-models clear to cycle all models.\n");
        output
    }

    pub(super) fn format_input_history(&self) -> String {
        let entries = self.history.entries();
        if entries.is_empty() {
            return "No input history yet.".to_string();
        }

        let mut output = String::from("Input history (most recent first):\n");
        for (idx, entry) in entries.iter().rev().take(50).enumerate() {
            let trimmed = entry.value.trim();
            if trimmed.is_empty() {
                continue;
            }
            let preview = trimmed.replace('\n', "\\n");
            let preview = preview.chars().take(120).collect::<String>();
            let _ = writeln!(output, "  {}. {preview}", idx + 1);
        }
        output
    }

    pub(super) fn format_session_info(&self, session: &Session) -> String {
        let file = session.path.as_ref().map_or_else(
            || "(not saved yet)".to_string(),
            |p| p.display().to_string(),
        );
        let name = session.get_name().unwrap_or_else(|| "-".to_string());
        let thinking = session
            .header
            .thinking_level
            .as_deref()
            .unwrap_or("off")
            .to_string();

        let message_count = session
            .entries_for_current_path()
            .iter()
            .filter(|entry| matches!(entry, SessionEntry::Message(_)))
            .count();

        let total_tokens = self.total_usage.total_tokens;
        let total_cost = self.total_usage.cost.total;
        let cost_str = if total_cost > 0.0 {
            format!("${total_cost:.4}")
        } else {
            "$0.0000".to_string()
        };

        let mut info = format!(
            "Session info:\n  file: {file}\n  id: {id}\n  name: {name}\n  model: {model}\n  thinking: {thinking}\n  messageCount: {message_count}\n  tokens: {total_tokens}\n  cost: {cost_str}",
            id = session.header.id,
            model = self.model,
        );
        info.push_str("\n\n");
        info.push_str(&self.frame_timing.summary());
        info.push_str("\n\n");
        info.push_str(&self.memory_monitor.summary());
        info
    }

    /// Handle a slash command.
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_command(&mut self, cmd: SlashCommand, args: &str) -> Option<Cmd> {
        // Clear input
        self.input.reset();

        match cmd {
            SlashCommand::Help => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: SlashCommand::help_text().to_string(),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_last_match("Available commands:");
                None
            }
            SlashCommand::Login => self.handle_slash_login(args),
            SlashCommand::Logout => self.handle_slash_logout(args),
            SlashCommand::Clear => {
                self.messages.clear();
                self.current_response.clear();
                self.current_thinking.clear();
                self.current_tool = None;
                self.pending_tool_output = None;
                self.abort_handle = None;
                self.autocomplete.close();
                self.message_render_cache.clear();
                self.status_message = Some("Conversation cleared".to_string());
                self.scroll_to_bottom();
                None
            }
            SlashCommand::Model => self.handle_slash_model(args),
            SlashCommand::Thinking => self.handle_slash_thinking(args),
            SlashCommand::ScopedModels => self.handle_slash_scoped_models(args),
            SlashCommand::Exit => Some(self.quit_cmd()),
            SlashCommand::History => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: self.format_input_history(),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_last_match("Input history");
                None
            }
            SlashCommand::Export => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot export while processing".to_string());
                    return None;
                }

                let (output_path, html) = {
                    let Ok(session_guard) = self.session.try_lock() else {
                        self.status_message = Some("Session busy; try again".to_string());
                        return None;
                    };
                    let output_path = if args.trim().is_empty() {
                        self.default_export_path(&session_guard)
                    } else {
                        self.resolve_output_path(args)
                    };
                    let html = session_guard.to_html();
                    (output_path, html)
                };

                if let Some(parent) = output_path.parent()
                    && !parent.as_os_str().is_empty()
                    && let Err(err) = std::fs::create_dir_all(parent)
                {
                    self.status_message = Some(format!("Failed to create dir: {err}"));
                    return None;
                }
                if let Err(err) = std::fs::write(&output_path, html) {
                    self.status_message = Some(format!("Failed to write export: {err}"));
                    return None;
                }

                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!("Exported HTML: {}", output_path.display()),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.status_message = Some(format!("Exported: {}", output_path.display()));
                None
            }
            SlashCommand::Session => {
                let Ok(session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
                let info = self.format_session_info(&session_guard);
                drop(session_guard);
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: info,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                None
            }
            SlashCommand::Settings => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot open settings while processing".to_string());
                    return None;
                }

                let mut settings = SettingsUiState::new();
                settings.max_visible = super::overlay_max_visible(self.term_height);
                self.settings_ui = Some(settings);
                self.session_picker = None;
                self.autocomplete.close();
                None
            }
            SlashCommand::Theme => {
                let name = args.trim();
                if name.is_empty() {
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: self.format_themes_list(),
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_last_match("Available themes:");
                    return None;
                }

                let is_auto_spec = name.eq_ignore_ascii_case("light/dark")
                    || name.eq_ignore_ascii_case("auto")
                    || name.eq_ignore_ascii_case("system");
                let theme = if name.eq_ignore_ascii_case("dark") {
                    Theme::dark()
                } else if name.eq_ignore_ascii_case("light") {
                    Theme::light()
                } else if name.eq_ignore_ascii_case("solarized") {
                    Theme::solarized()
                } else if is_auto_spec {
                    Theme::detected()
                } else {
                    match Theme::load_by_name(name, &self.cwd) {
                        Ok(theme) => theme,
                        Err(err) => {
                            self.status_message = Some(err.to_string());
                            return None;
                        }
                    }
                };

                let theme_name = theme.name.clone();
                self.apply_theme(theme);
                // Persist auto specs as typed so future sessions keep
                // re-detecting instead of pinning today's detected theme.
                let persisted_spec = if is_auto_spec {
                    name.to_ascii_lowercase()
                } else {
                    theme_name.clone()
                };
                self.config.theme = Some(persisted_spec.clone());
                let display_name = if is_auto_spec {
                    format!("{persisted_spec} (detected: {theme_name})")
                } else {
                    theme_name
                };

                if let Err(err) = self.persist_project_theme(&persisted_spec) {
                    tracing::warn!("Failed to persist theme preference: {err}");
                    self.status_message = Some(format!(
                        "Switched to theme: {display_name} (not saved: {err})"
                    ));
                } else {
                    self.status_message = Some(format!("Switched to theme: {display_name}"));
                }

                None
            }
            SlashCommand::Resume => {
                if let Some(reason) = self.session_transition_blocker() {
                    self.status_message = Some(reason.to_string());
                    return None;
                }

                let override_dir = self
                    .session
                    .try_lock()
                    .ok()
                    .and_then(|guard| guard.session_dir.clone());
                let base_dir = override_dir.clone().unwrap_or_else(Config::sessions_dir);
                let sessions = crate::session_picker::list_sessions_for_project(
                    &self.cwd,
                    override_dir.as_deref(),
                );
                if sessions.is_empty() {
                    self.status_message = Some("No sessions found for this project".to_string());
                    return None;
                }

                let mut picker = SessionPickerOverlay::new_with_root(sessions, Some(base_dir));
                picker.max_visible = super::overlay_max_visible(self.term_height);
                self.session_picker = Some(picker);
                self.autocomplete.close();
                None
            }
            SlashCommand::New => {
                if let Some(reason) = self.session_transition_blocker() {
                    self.status_message = Some(reason.to_string());
                    return None;
                }

                let Some(extensions) = self.extensions.clone() else {
                    let Ok(mut agent_guard) = self.agent.try_lock() else {
                        self.status_message = Some("Session busy; try again".to_string());
                        return None;
                    };
                    let Ok(mut session_guard) = self.session.try_lock() else {
                        self.status_message = Some("Session busy; try again".to_string());
                        return None;
                    };
                    let reset_thinking = self.new_session_thinking_level();
                    let session_dir = session_guard.session_dir.clone();
                    *session_guard = Session::create_with_dir(session_dir);
                    session_guard.header.provider = Some(self.model_entry.model.provider.clone());
                    session_guard.header.model_id = Some(self.model_entry.model.id.clone());
                    session_guard.header.thinking_level = Some(reset_thinking.to_string());
                    let new_session_id = session_guard.header.id.clone();
                    agent_guard.replace_messages(Vec::new());
                    agent_guard.stream_options_mut().thinking_level = Some(reset_thinking);
                    drop(session_guard);
                    drop(agent_guard);
                    self.session_action_admission.advance_generation();

                    self.messages.clear();
                    self.message_render_cache.clear();
                    self.total_usage = Usage::default();
                    self.current_response.clear();
                    self.current_thinking.clear();
                    self.current_tool = None;
                    self.current_tool_id = None;
                    self.current_tool_summary.clear();
                    self.todo_summary = None;
                    self.pending_tool_output = None;
                    self.abort_handle = None;
                    self.pending_oauth = None;
                    self.title_requested = false;
                    self.role_model_overrides.clear();
                    self.displayed_session_id = Some(new_session_id);
                    self.session_picker = None;
                    self.tree_ui = None;
                    self.drain_capability_prompts_for_session_reset();
                    self.autocomplete.close();
                    self.message_render_cache.clear();

                    self.status_message = Some(format!(
                        "Started new session\nModel set to {}\nThinking level: {reset_thinking}",
                        self.model
                    ));
                    self.scroll_to_bottom();
                    self.input.focus();
                    return None;
                };

                let model_provider = self.model_entry.model.provider.clone();
                let model_id = self.model_entry.model.id.clone();
                let model_label = self.model.clone();
                let reset_thinking = self.new_session_thinking_level();
                let event_tx = self.event_tx.clone();
                let session = Arc::clone(&self.session);
                let agent = Arc::clone(&self.agent);
                let admission = self.session_action_admission.clone();
                let runtime_handle = self.runtime_handle.clone();

                let (session_dir, previous_session_file) = {
                    let Ok(guard) = self.session.try_lock() else {
                        self.status_message = Some("Session busy; try again".to_string());
                        return None;
                    };
                    (
                        guard.session_dir.clone(),
                        guard.path.as_ref().map(|p| p.display().to_string()),
                    )
                };

                self.agent_state = AgentState::Processing;
                self.status_message = Some("Starting new session...".to_string());

                let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
                runtime_handle.spawn(async move {
                    let cancelled = extensions
                        .dispatch_cancellable_event(
                            ExtensionEventName::SessionBeforeSwitch,
                            Some(json!({ "reason": "new" })),
                            EXTENSION_EVENT_TIMEOUT_MS,
                        )
                        .await
                        .unwrap_or(false);
                    if cancelled {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::System("Session switch cancelled by extension".to_string()),
                        )
                        .await;
                        return;
                    }

                    let mut new_session = Session::create_with_dir(session_dir);
                    new_session.header.provider = Some(model_provider);
                    new_session.header.model_id = Some(model_id);
                    new_session.header.thinking_level = Some(reset_thinking.to_string());
                    let new_session_id = new_session.header.id.clone();
                    if let Err(err) = Self::try_install_session(
                        &session,
                        &agent,
                        &admission,
                        new_session,
                        Vec::new(),
                        Some(reset_thinking),
                    )
                    .await
                    {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::AgentError(err.to_string()),
                        )
                        .await;
                        return;
                    }

                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::ConversationReset {
                            session_id: new_session_id.clone(),
                            messages: Vec::new(),
                            usage: Usage::default(),
                            status: Some(format!(
                                "Started new session\nModel set to {model_label}\nThinking level: {reset_thinking}"
                            )),
                        },
                    )
                    .await;

                    let _ = extensions
                        .dispatch_event(
                            ExtensionEventName::SessionSwitch,
                            Some(json!({
                                "reason": "new",
                                "previousSessionFile": previous_session_file,
                                "sessionId": new_session_id,
                            })),
                        )
                        .await;
                });

                None
            }
            SlashCommand::Copy => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot copy while processing".to_string());
                    return None;
                }

                let text = self
                    .messages
                    .iter()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant && !m.content.trim().is_empty())
                    .map(|m| m.content.clone());

                let Some(text) = text else {
                    self.status_message = Some("No agent messages to copy yet.".to_string());
                    return None;
                };

                let write_fallback = |text: &str| -> std::io::Result<std::path::PathBuf> {
                    use std::io::Write;
                    let dir = std::env::temp_dir();
                    let filename = format!("pi_copy_{}.txt", Utc::now().timestamp_millis());
                    let path = dir.join(filename);

                    let mut options = std::fs::OpenOptions::new();
                    options.write(true).create_new(true);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        options.mode(0o600);
                    }

                    let mut file = options.open(&path)?;
                    file.write_all(text.as_bytes())?;

                    Ok(path)
                };

                #[cfg(feature = "clipboard")]
                {
                    match ArboardClipboard::new()
                        .and_then(|mut clipboard| clipboard.set_text(text.clone()))
                    {
                        Ok(()) => self.status_message = Some("Copied to clipboard".to_string()),
                        Err(err) => match write_fallback(&text) {
                            Ok(path) => {
                                self.status_message = Some(format!(
                                    "Clipboard support is disabled or unavailable ({err}). Wrote to {}",
                                    path.display()
                                ));
                            }
                            Err(io_err) => {
                                self.status_message = Some(format!(
                                    "Clipboard support is disabled or unavailable ({err}); also failed to write fallback file: {io_err}"
                                ));
                            }
                        },
                    }
                }

                #[cfg(not(feature = "clipboard"))]
                {
                    match write_fallback(&text) {
                        Ok(path) => {
                            self.status_message = Some(format!(
                                "Clipboard support is disabled. Wrote to {}",
                                path.display()
                            ));
                        }
                        Err(err) => {
                            self.status_message = Some(format!(
                                "Clipboard support is disabled; failed to write fallback file: {err}"
                            ));
                        }
                    }
                }

                None
            }
            SlashCommand::Name => {
                let name = args.trim();
                if name.is_empty() {
                    self.status_message = Some("Usage: /name <name>".to_string());
                    return None;
                }

                let Ok(mut session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
                session_guard.append_session_info(Some(name.to_string()));
                drop(session_guard);
                self.spawn_save_session();

                self.status_message = Some(format!("Session name: {name}"));
                None
            }
            SlashCommand::Hotkeys => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: self.format_hotkeys(),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                None
            }
            SlashCommand::Changelog => {
                let content = crate::embedded_assets::changelog().to_string();
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_last_match("# ");
                None
            }
            SlashCommand::Tree => {
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot open tree while processing".to_string());
                    return None;
                }

                if let Some(extensions) = self.extensions.clone() {
                    let owner_session_id = if let Ok(session) = self.session.try_lock() {
                        session.header.id.clone()
                    } else {
                        self.status_message = Some("Session busy; try again".to_string());
                        return None;
                    };
                    let session = Arc::clone(&self.session);
                    let event_tx = self.event_tx.clone();
                    let runtime_handle = self.runtime_handle.clone();
                    let args = args.to_string();
                    let task_cx = Cx::current().unwrap_or_else(Cx::for_request);

                    runtime_handle.spawn(async move {
                        let cx = Cx::current().unwrap_or_else(Cx::for_request);
                        let (initial_selected_id, branch_count, entry_count) =
                            match OwnedMutexGuard::lock(Arc::clone(&session), &cx).await {
                                Ok(session_guard) => {
                                    if session_guard.header.id.as_str() != owner_session_id.as_str()
                                    {
                                        return;
                                    }
                                    let initial_selected_id =
                                        resolve_tree_selector_initial_id(&session_guard, &args);
                                    let branch_count = session_guard.list_leaves().len();
                                    let entry_count = session_guard.entries.len();
                                    (initial_selected_id, branch_count, entry_count)
                                }
                                Err(err) => {
                                    let _ = crate::interactive::enqueue_pi_event(
                                        &event_tx,
                                        &task_cx,
                                        PiMsg::SessionSystemNote {
                                            owner_session_id: owner_session_id.clone(),
                                            message: format!("Failed to lock session: {err}"),
                                        },
                                    )
                                    .await;
                                    return;
                                }
                            };

                        let response = extensions
                            .dispatch_event_with_response(
                                ExtensionEventName::SessionBeforeTree,
                                Some(json!({
                                    "sessionId": owner_session_id.clone(),
                                    "preparation": {
                                        "branchCount": branch_count,
                                        "entryCount": entry_count,
                                    }
                                })),
                                EXTENSION_EVENT_TIMEOUT_MS,
                            )
                            .await
                            .unwrap_or(None);

                        let mut label = None;
                        let mut cancelled = false;
                        if let Some(value) = response {
                            if value.as_bool() == Some(false) {
                                cancelled = true;
                            }
                            if let Some(obj) = value.as_object() {
                                if obj.get("cancel").and_then(Value::as_bool).unwrap_or(false)
                                    || obj
                                        .get("cancelled")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false)
                                {
                                    cancelled = true;
                                }
                                if let Some(custom_label) = obj.get("label").and_then(Value::as_str)
                                {
                                    label = Some(custom_label.to_string());
                                }
                            }
                        }

                        if cancelled {
                            let _ = crate::interactive::enqueue_pi_event(
                                &event_tx,
                                &task_cx,
                                PiMsg::SessionSystemNote {
                                    owner_session_id,
                                    message: "Session tree cancelled by extension".to_string(),
                                },
                            )
                            .await;
                            return;
                        }

                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &task_cx,
                            PiMsg::OpenTree {
                                owner_session_id,
                                initial_selected_id,
                                label,
                            },
                        )
                        .await;
                    });

                    self.status_message = Some("Preparing tree...".to_string());
                    return None;
                }

                let Ok(session_guard) = self.session.try_lock() else {
                    self.status_message = Some("Session busy; try again".to_string());
                    return None;
                };
                let initial_selected_id = resolve_tree_selector_initial_id(&session_guard, args);
                let selector = TreeSelectorState::new(
                    &session_guard,
                    self.term_height,
                    initial_selected_id.as_deref(),
                    None,
                );
                drop(session_guard);
                self.tree_ui = Some(TreeUiState::Selector(selector));
                None
            }
            SlashCommand::Fork => self.handle_slash_fork(args),
            SlashCommand::Compact => self.handle_slash_compact(args),
            SlashCommand::Reload => self.handle_slash_reload(),
            SlashCommand::Template => self.handle_slash_template(args),
            SlashCommand::Share => self.handle_slash_share(args),
            SlashCommand::Mcp => self.handle_slash_mcp(args),
            SlashCommand::Plan => self.handle_slash_plan(args),
            SlashCommand::Advisor => self.handle_slash_advisor(args),
            SlashCommand::Checkpoint => self.handle_slash_checkpoint(args),
            SlashCommand::Rewind => self.handle_slash_rewind(args),
            SlashCommand::Fresh => self.handle_slash_fresh(),
            SlashCommand::Retry => self.handle_slash_retry(),
            SlashCommand::Undo => self.handle_slash_undo(args),
            SlashCommand::Redo => self.handle_slash_redo(args),
            SlashCommand::Usage => self.handle_slash_usage(args),
            SlashCommand::Approval => self.handle_slash_approval(args),
            SlashCommand::Handoff => self.handle_slash_handoff(args),
            SlashCommand::Rules => self.handle_slash_rules(args),
            SlashCommand::AddDir => self.handle_slash_add_dir(args),
            SlashCommand::RemoveDir => self.handle_slash_remove_dir(args),
            SlashCommand::Crash => self.handle_slash_crash(args),
            SlashCommand::Btw => self.handle_slash_btw(args),
            SlashCommand::Tan => self.handle_slash_tan(args),
            SlashCommand::Omfg => self.handle_slash_omfg(args),
            SlashCommand::Commit => self.handle_slash_commit(args),
            SlashCommand::Review => self.handle_slash_review(args),
        }
    }

    pub(super) fn handle_slash_login(&mut self, args: &str) -> Option<Cmd> {
        let auth_path = crate::config::Config::auth_path();
        self.handle_slash_login_with_auth_path(args, &auth_path)
    }

    #[allow(clippy::too_many_lines)]
    fn handle_slash_login_with_auth_path(
        &mut self,
        args: &str,
        auth_path: &std::path::Path,
    ) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot login while processing".to_string());
            return None;
        }

        let args = args.trim();
        if args.is_empty() {
            match crate::auth::AuthStorage::load(auth_path.to_path_buf()) {
                Ok(auth) => {
                    let registered_extension_bindings =
                        match registered_extension_provider_bindings(self.extensions.as_ref()) {
                            Ok(bindings) => bindings,
                            Err(err) => {
                                self.status_message = Some(format!(
                                    "Unable to load extension login providers: {err}"
                                ));
                                return None;
                            }
                        };
                    let listing = format_login_provider_listing(
                        &auth,
                        &self.available_models,
                        &registered_extension_bindings,
                    );
                    self.messages.push(ConversationMessage {
                        role: MessageRole::System,
                        content: listing,
                        thinking: None,
                        collapsed: false,
                    });
                    self.scroll_to_last_match("Available login providers:");
                }
                Err(err) => {
                    self.status_message = Some(format!("Unable to load auth status: {err}"));
                }
            }
            return None;
        }

        let requested_provider = args.split_whitespace().next().unwrap_or(args).to_string();
        let provider = normalize_auth_provider_input(&requested_provider);

        if provider == "kimi-for-coding" {
            self.status_message = Some("Starting Kimi Code login...".to_string());
            let event_tx = self.event_tx.clone();
            let provider_clone = provider;
            let runtime_handle = self.runtime_handle.clone();
            let cx = asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request);

            runtime_handle.spawn(async move {
                match crate::auth::start_kimi_code_device_flow().await {
                    Ok(device) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::OAuthDeviceFlowStarted {
                                provider: provider_clone,
                                device_code: device.device_code,
                                user_code: device.user_code,
                                verification_uri: device
                                    .verification_uri_complete
                                    .unwrap_or(device.verification_uri),
                                expires_in: device.expires_in,
                            },
                        )
                        .await;
                    }
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("OAuth login failed: {err}")),
                        )
                        .await;
                    }
                }
            });
            return None;
        }

        if (provider == "github-copilot" || provider == "copilot")
            && should_use_copilot_device_flow()
        {
            self.status_message = Some("Starting GitHub Copilot device flow login...".to_string());
            let event_tx = self.event_tx.clone();
            let provider_clone = provider;
            let runtime_handle = self.runtime_handle.clone();
            let cx = asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request);
            let client_id = crate::auth::resolved_copilot_client_id();
            let copilot_config = crate::auth::CopilotOAuthConfig {
                client_id,
                ..crate::auth::CopilotOAuthConfig::default()
            };

            runtime_handle.spawn(async move {
                match crate::auth::start_copilot_device_flow(&copilot_config).await {
                    Ok(device) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::OAuthDeviceFlowStarted {
                                provider: provider_clone,
                                device_code: device.device_code,
                                user_code: device.user_code,
                                verification_uri: device
                                    .verification_uri_complete
                                    .unwrap_or(device.verification_uri),
                                expires_in: device.expires_in,
                            },
                        )
                        .await;
                    }
                    Err(err) => {
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &cx,
                            PiMsg::AgentError(format!("OAuth login failed: {err}")),
                        )
                        .await;
                    }
                }
            });
            return None;
        }

        if let Some(prompt) = api_key_login_prompt(&provider) {
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: prompt,
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_bottom();
            self.pending_oauth = Some(PendingOAuth {
                provider,
                kind: PendingLoginKind::ApiKey,
                verifier: String::new(),
                oauth_config: None,
                device_code: None,
                redirect_uri: None,
            });
            self.input_mode = InputMode::SingleLine;
            self.set_input_height(3);
            self.input.focus();
            return None;
        }

        // Look up OAuth config: built-in providers or extension-registered OAuth config.
        let oauth_result = if provider == "anthropic" {
            crate::auth::start_anthropic_oauth().map(|info| (info, None))
        } else if provider == "openai-codex" {
            crate::auth::start_openai_codex_oauth().map(|info| (info, None))
        } else if provider == "google-gemini-cli" {
            crate::auth::start_google_gemini_cli_oauth().map(|info| (info, None))
        } else if provider == "google-antigravity" {
            crate::auth::start_google_antigravity_oauth().map(|info| (info, None))
        } else if provider == "github-copilot" || provider == "copilot" {
            let client_id = crate::auth::resolved_copilot_client_id();
            let copilot_config = crate::auth::CopilotOAuthConfig {
                client_id,
                ..crate::auth::CopilotOAuthConfig::default()
            };
            crate::auth::start_copilot_browser_oauth(&copilot_config).map(|info| (info, None))
        } else if provider == "gitlab" || provider == "gitlab-duo" {
            let client_id = std::env::var("GITLAB_CLIENT_ID").unwrap_or_default();
            let base_url = std::env::var("GITLAB_BASE_URL")
                .unwrap_or_else(|_| "https://gitlab.com".to_string());
            let gitlab_config = crate::auth::GitLabOAuthConfig {
                client_id,
                base_url,
                ..crate::auth::GitLabOAuthConfig::default()
            };
            crate::auth::start_gitlab_oauth(&gitlab_config).map(|info| (info, None))
        } else {
            // Check extension providers for OAuth config.
            let registered_extension_bindings =
                match registered_extension_provider_bindings(self.extensions.as_ref()) {
                    Ok(bindings) => bindings,
                    Err(err) => {
                        self.status_message =
                            Some(format!("Unable to load extension login providers: {err}"));
                        return None;
                    }
                };
            let ext_oauth = extension_oauth_config_for_provider(
                &self.available_models,
                &registered_extension_bindings,
                &provider,
            );
            if let Some(config) = ext_oauth {
                crate::auth::start_extension_oauth(&provider, &config)
                    .map(|info| (info, Some(config)))
            } else {
                self.status_message = Some(format!(
                    "Login not supported for {provider} (no built-in flow or OAuth config)"
                ));
                return None;
            }
        };

        match oauth_result {
            Ok((info, ext_config)) => {
                // Use the pre-bound callback server when the provider already
                // created one (e.g. Copilot/GitLab with random port).  Otherwise
                // start a new one for localhost redirect URIs (issue #22).
                let callback_server = info.callback_server.or_else(|| {
                    info.redirect_uri
                        .as_deref()
                        .filter(|uri| crate::auth::redirect_uri_needs_callback_server(uri))
                        .and_then(|uri| crate::auth::start_oauth_callback_server(uri).ok())
                });

                let mut message = format!(
                    "OAuth login: {}\n\nOpen this URL:\n{}\n",
                    info.provider, info.url
                );
                if info.provider == "anthropic" {
                    message.push_str(
                        "\nWARNING: Anthropic OAuth (Claude Code consumer account) is no longer recommended.\n\
Using consumer OAuth tokens outside the official client may violate Anthropic's consumer Terms of Service and can\n\
result in account suspension/ban. Prefer using an Anthropic API key (ANTHROPIC_API_KEY) instead.\n",
                    );
                }
                if callback_server.is_some() {
                    message.push_str(
                        "\nListening for callback — complete authorization in your browser.\n\
                         Pi will continue automatically, or you can paste the code manually.",
                    );
                } else if let Some(instructions) = info.instructions {
                    message.push('\n');
                    message.push_str(&instructions);
                    message.push('\n');
                    message.push_str(
                        "\nPaste the callback URL or authorization code into Pi to continue.",
                    );
                } else {
                    message.push_str(
                        "\nPaste the callback URL or authorization code into Pi to continue.",
                    );
                }

                // Spawn a thread to wait for the callback and inject the code
                // via the event channel when the browser redirect arrives.
                if let Some(server) = callback_server {
                    let event_tx = self.event_tx.clone();
                    std::thread::spawn(move || {
                        // Block until the callback arrives or the sender is dropped.
                        if let Ok(path) = server.rx.recv() {
                            let full_url = format!("http://localhost{path}");
                            let mut send_result =
                                event_tx.try_send(PiMsg::OAuthCallbackReceived(full_url));
                            while let Err(asupersync::channel::mpsc::SendError::Full(unsent)) =
                                send_result
                            {
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                send_result = event_tx.try_send(unsent);
                            }
                        }
                    });
                }

                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: message,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.pending_oauth = Some(PendingOAuth {
                    provider: info.provider,
                    kind: PendingLoginKind::OAuth,
                    verifier: info.verifier,
                    oauth_config: ext_config,
                    device_code: None,
                    redirect_uri: info.redirect_uri,
                });
                self.input_mode = InputMode::SingleLine;
                self.set_input_height(3);
                self.input.focus();
                None
            }
            Err(err) => {
                self.status_message = Some(format!("OAuth login failed: {err}"));
                None
            }
        }
    }

    pub(super) fn handle_slash_logout(&mut self, args: &str) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot logout while processing".to_string());
            return None;
        }

        let requested_provider = if args.is_empty() {
            self.model_entry.model.provider.clone()
        } else {
            args.split_whitespace().next().unwrap_or(args).to_string()
        };
        let requested_provider = requested_provider.trim().to_ascii_lowercase();
        let provider = normalize_auth_provider_input(&requested_provider);

        let auth_path = crate::config::Config::auth_path();
        match crate::auth::AuthStorage::load(auth_path) {
            Ok(mut auth) => {
                let removed = remove_provider_credentials(&mut auth, &requested_provider);
                if let Err(err) = auth.save() {
                    self.status_message = Some(err.to_string());
                    return None;
                }
                self.sync_active_provider_credentials(&provider);
                if removed {
                    self.status_message =
                        Some(format!("Removed stored credentials for {provider}."));
                } else {
                    self.status_message = Some(format!("No stored credentials for {provider}."));
                }
            }
            Err(err) => {
                self.status_message = Some(err.to_string());
            }
        }
        None
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_model(&mut self, args: &str) -> Option<Cmd> {
        // Role targeting (bd-cv653.3.1): `/model <role>` shows the role's
        // assignment; `/model <role> <pattern>` assigns a model to the role
        // for this session. Non-role first tokens fall through to the classic
        // selector flow below.
        {
            let trimmed = args.trim();
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let first = parts.next().unwrap_or("");
            let rest = parts.next().unwrap_or("").trim();
            if !first.is_empty()
                && let Some(role) = ModelRole::from_name(first)
            {
                if rest.is_empty() {
                    let current = self
                        .role_model_overrides
                        .get(&role)
                        .map(|(p, m)| format!("{p}/{m}"))
                        .or_else(|| {
                            self.config
                                .model_roles
                                .as_ref()
                                .and_then(|roles| crate::app::role_spec_from_settings(roles, role))
                                .map(str::to_string)
                        })
                        .unwrap_or_else(|| "(inherits default)".to_string());
                    self.status_message = Some(format!("Role {role}: {current}"));
                    return None;
                }
                if self.agent_state != AgentState::Idle {
                    self.status_message = Some("Cannot switch models while processing".to_string());
                    return None;
                }
                return self.assign_model_to_role(role, rest);
            }
        }

        if args.trim().is_empty() {
            self.open_model_selector_configured_only();
            return None;
        }

        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot switch models while processing".to_string());
            return None;
        }

        let pattern = args.trim();
        let pattern_lower = pattern.to_ascii_lowercase();
        let provider_scoped_pattern = split_provider_model_spec(pattern);

        let mut exact_matches = Vec::new();
        for entry in &self.available_models {
            let full = format!("{}/{}", entry.model.provider, entry.model.id);
            if full.eq_ignore_ascii_case(pattern)
                || entry.model.id.eq_ignore_ascii_case(pattern)
                || provider_scoped_pattern.is_some_and(|(provider, model_id)| {
                    provider_ids_match(&entry.model.provider, provider)
                        && entry.model.id.eq_ignore_ascii_case(model_id)
                })
            {
                exact_matches.push(entry.clone());
            }
        }

        let mut matches = if exact_matches.is_empty() {
            let mut fuzzy = Vec::new();
            for entry in &self.available_models {
                let full = format!("{}/{}", entry.model.provider, entry.model.id);
                let full_lower = full.to_ascii_lowercase();
                if full_lower.contains(&pattern_lower)
                    || entry.model.id.to_ascii_lowercase().contains(&pattern_lower)
                {
                    fuzzy.push(entry.clone());
                }
            }
            fuzzy
        } else {
            exact_matches
        };

        matches.sort_by(|a, b| {
            let left = format!("{}/{}", a.model.provider, a.model.id);
            let right = format!("{}/{}", b.model.provider, b.model.id);
            left.to_ascii_lowercase().cmp(&right.to_ascii_lowercase())
        });
        matches.dedup_by(|a, b| model_entry_matches(a, b));

        if matches.is_empty()
            && let Some((provider, model_id)) = pattern.split_once('/')
        {
            let provider = normalize_auth_provider_input(provider);
            let model_id = model_id.trim();
            if !provider.is_empty()
                && !model_id.is_empty()
                && let Some(entry) = crate::models::ad_hoc_model_entry(&provider, model_id)
            {
                matches.push(entry);
            }
        }

        if matches.is_empty() {
            self.status_message = Some(format!("Model not found: {pattern}"));
            return None;
        }
        if matches.len() > 1 {
            let preview = matches
                .iter()
                .take(8)
                .map(|m| format!("  - {}/{}", m.model.provider, m.model.id))
                .collect::<Vec<_>>()
                .join("\n");
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: format!(
                    "Ambiguous model pattern \"{pattern}\". Matches:\n{preview}\n\nUse /model provider/id for an exact match."
                ),
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_bottom();
            return None;
        }

        let next = matches.pop().expect("matches is exactly length 1 here");

        let resolved_key_opt = resolve_model_key_from_default_auth(&next);
        if model_requires_configured_credential(&next) && resolved_key_opt.is_none() {
            self.status_message = Some(format!(
                "Missing credentials for provider {}. Run /login {}.",
                next.model.provider, next.model.provider
            ));
            return None;
        }

        if model_entry_matches(&next, &self.model_entry) {
            self.status_message = Some(format!("Current model: {}", self.model));
            return None;
        }

        let provider_impl = match providers::create_provider(&next, self.extensions.as_ref()) {
            Ok(provider_impl) => provider_impl,
            Err(err) => {
                self.status_message = Some(err.to_string());
                return None;
            }
        };

        if let Err(message) =
            self.switch_active_model(&next, provider_impl, resolved_key_opt.as_deref(), "command")
        {
            self.status_message = Some(message);
            return None;
        }

        if !self
            .available_models
            .iter()
            .any(|entry| model_entry_matches(entry, &next))
        {
            self.available_models.push(next.clone());
        }

        self.status_message = Some(format!("Switched model: {}", self.model));
        None
    }

    /// Assign a model to a role for this session (`/model <role> <pattern>`,
    /// bd-cv653.3.1). Records the override in app state and appends a
    /// role-tagged `ModelChange` entry so the assignment replays.
    fn assign_model_to_role(&mut self, role: ModelRole, pattern: &str) -> Option<Cmd> {
        let mut found: Option<ModelEntry> = None;
        for entry in &self.available_models {
            let full = format!("{}/{}", entry.model.provider, entry.model.id);
            if full.eq_ignore_ascii_case(pattern)
                || entry.model.id.eq_ignore_ascii_case(pattern)
                || split_provider_model_spec(pattern).is_some_and(|(provider, model_id)| {
                    provider_ids_match(&entry.model.provider, provider)
                        && entry.model.id.eq_ignore_ascii_case(model_id)
                })
            {
                found = Some(entry.clone());
                break;
            }
        }
        if found.is_none()
            && let Some((provider, model_id)) = split_provider_model_spec(pattern)
        {
            let provider = normalize_auth_provider_input(provider);
            if !provider.is_empty() && !model_id.trim().is_empty() {
                found = crate::models::ad_hoc_model_entry(&provider, model_id.trim());
            }
        }
        let Some(entry) = found else {
            self.status_message = Some(format!("Model not found: {pattern}"));
            return None;
        };
        // Borrow for the /btw rebinding decision BEFORE `entry.model.id`
        // moves out below (E0382, bd-9jgrt).
        let btw_rebinding = self.rebuild_btw_client(&entry);
        let provider = entry.model.provider.clone();
        let model_id = entry.model.id;
        self.role_model_overrides
            .insert(role, (provider.clone(), model_id.clone()));
        if let Ok(mut session_guard) = self.session.try_lock() {
            session_guard.append_model_change_with_role(
                provider.clone(),
                model_id.clone(),
                Some(role.as_str().to_string()),
            );
        }
        // Rebind the /btw smol-role client when the role changes (bd-9jgrt).
        // Without the factory (non-startup surfaces) disclose the stale bind
        // instead of silently serving questions through the old provider.
        self.status_message = Some(if role == ModelRole::Smol {
            match btw_rebinding {
                Some(true) => {
                    format!("Role {role} set to {provider}/{model_id} (/btw rebound)")
                }
                Some(false) => format!(
                    "Role {role} set to {provider}/{model_id} (/btw rebinding failed; \
                     keeping previous binding)"
                ),
                None => format!(
                    "Role {role} set to {provider}/{model_id} (/btw keeps its \
                     startup binding until restart)"
                ),
            }
        } else {
            format!("Role {role} set to {provider}/{model_id}")
        });
        None
    }

    fn handle_slash_plan(&mut self, args: &str) -> Option<Cmd> {
        let sub = args.trim().to_ascii_lowercase();
        let plan_state = {
            let Ok(agent_guard) = self.agent.try_lock() else {
                self.status_message = Some("Agent busy; try again".to_string());
                return None;
            };
            agent_guard.plan_state()
        };
        match sub.as_str() {
            "" | "on" | "start" => self.enter_plan_mode(&plan_state),
            "status" => {
                self.status_message = Some(format!("Plan mode: {}", plan_state.mode().as_str()));
            }
            "approve" => self.approve_plan_mode(&plan_state),
            "reject" => {
                if plan_state.reject() {
                    Self::log_plan_transition(&self.session, "rejected");
                    self.status_message =
                        Some("Plan rejected — still planning; revise and resubmit".to_string());
                } else {
                    self.status_message = Some("No submitted plan to reject".to_string());
                }
            }
            "off" | "exit" => {
                plan_state.exit();
                Self::log_plan_transition(&self.session, "off");
                self.status_message = Some("Plan mode off".to_string());
            }
            other => {
                self.status_message = Some(format!(
                    "Unknown /plan subcommand {other:?}: use /plan [approve|reject|off|status]"
                ));
            }
        }
        None
    }

    fn log_plan_transition(session: &Arc<Mutex<crate::session::Session>>, mode: &str) {
        if let Ok(mut guard) = session.try_lock() {
            guard.append_custom_entry(
                "plan_mode".to_string(),
                Some(serde_json::json!({"mode": mode})),
            );
        }
    }

    fn handle_slash_approval(&mut self, args: &str) -> Option<Cmd> {
        let sub = args.trim().to_ascii_lowercase();
        let approval_state = {
            let Ok(agent_guard) = self.agent.try_lock() else {
                self.status_message = Some("Agent is busy".to_string());
                return None;
            };
            agent_guard.approval_state()
        };

        let Some(state) = approval_state else {
            self.status_message = Some("Tool approval state not configured".to_string());
            return None;
        };

        match sub.as_str() {
            "" | "status" => {
                let mode = state.mode();
                let dual_classes = state.dual_confirm_classes();
                let dual_str = if dual_classes.is_empty() {
                    "none".to_string()
                } else {
                    dual_classes
                        .iter()
                        .map(|c| c.label())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                self.status_message = Some(format!(
                    "Approval mode: {} | Dual-confirm classes: {}",
                    mode.as_str(),
                    dual_str
                ));
            }
            "always-ask" | "always_ask" | "always" | "ask" => {
                state.set_mode(crate::approval::ApprovalMode::AlwaysAsk);
                Self::log_approval_transition(
                    &self.session,
                    crate::approval::ApprovalMode::AlwaysAsk,
                );
                self.status_message = Some("Approval mode set to always-ask".to_string());
            }
            "write" | "files" => {
                state.set_mode(crate::approval::ApprovalMode::Write);
                Self::log_approval_transition(&self.session, crate::approval::ApprovalMode::Write);
                self.status_message =
                    Some("Approval mode set to write (file mutations auto-approved)".to_string());
            }
            "yolo" | "auto-approve" | "auto" | "all" => {
                state.set_mode(crate::approval::ApprovalMode::Yolo);
                Self::log_approval_transition(&self.session, crate::approval::ApprovalMode::Yolo);
                self.status_message = Some(
                    "Approval mode set to yolo (all auto-approved except hard policy gates)"
                        .to_string(),
                );
            }
            other => {
                self.status_message = Some(format!(
                    "Unknown /approval mode {other:?}: use /approval [always-ask|write|yolo|status]"
                ));
            }
        }
        None
    }

    fn log_approval_transition(
        session: &Arc<Mutex<crate::session::Session>>,
        mode: crate::approval::ApprovalMode,
    ) {
        if let Ok(mut guard) = session.try_lock() {
            guard.append_custom_entry(
                "approval_mode".to_string(),
                Some(serde_json::json!({"mode": mode.as_str()})),
            );
        }
    }

    pub(super) fn handle_slash_handoff(&mut self, args: &str) -> Option<Cmd> {
        let args = args.trim();
        let (to_target, out_path) = if args.is_empty() {
            (crate::handoff::HandoffTarget::Human, None)
        } else {
            let mut parts = args.split_whitespace();
            let target_str = parts.next().unwrap_or("human");
            let path_str = parts.next().map(std::path::PathBuf::from);
            (crate::handoff::HandoffTarget::parse(target_str), path_str)
        };

        let Ok(session_guard) = self.session.try_lock() else {
            self.status_message = Some("Session busy; try again".to_string());
            return None;
        };

        let doc = crate::handoff::HandoffGenerator::generate_from_session(&session_guard);
        drop(session_guard);

        match crate::handoff::HandoffGenerator::deliver(&doc, &to_target, out_path.as_deref()) {
            Ok(report) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!(
                        "### 📋 Handoff Brief Generated\n\n{}\n\n*{}*",
                        doc.to_markdown(),
                        report.status
                    ),
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.status_message = Some("Handoff brief generated successfully".to_string());
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to generate handoff: {e}"));
            }
        }

        None
    }

    pub(super) fn handle_slash_rules(&mut self, args: &str) -> Option<Cmd> {
        let args = args.trim();
        let project_root = self.cwd.clone();
        let mut store = crate::stream_rules::StreamRuleStore::load_for_project(&project_root);

        if args.is_empty() || args == "list" {
            let rules = store.list_all_rules();
            let mut text = format!("### 🛡️ Active Stream Rules ({})\n\n", rules.len());
            if rules.is_empty() {
                text.push_str("No stream rules configured. Use `/rules add <id> <pattern> <body>` or `/omfg <complaint>` to create one.\n");
            } else {
                for r in &rules {
                    let status = if r.enabled {
                        "✅ enabled"
                    } else {
                        "⏸️ disabled"
                    };
                    let _ = writeln!(
                        text,
                        "- **{}** [{status}]: `/{}/`\n  {}",
                        r.name, r.pattern, r.body
                    );
                }
            }
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: text,
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_bottom();
        } else if let Some(rest) = args.strip_prefix("remove ") {
            let id = rest.trim();
            match store.remove_rule(id) {
                Ok(true) => {
                    self.status_message = Some(format!("Removed stream rule '{id}'"));
                }
                Ok(false) => {
                    self.status_message = Some(format!("Stream rule '{id}' not found"));
                }
                Err(e) => {
                    self.status_message = Some(format!("Error removing rule: {e}"));
                }
            }
        } else if let Some(rest) = args.strip_prefix("toggle ") {
            let id = rest.trim();
            let current = store
                .list_all_rules()
                .into_iter()
                .find(|r| r.id == id)
                .is_none_or(|r| r.enabled);
            match store.toggle_rule(id, !current) {
                Ok(true) => {
                    let st = if current { "disabled" } else { "enabled" };
                    self.status_message = Some(format!("Stream rule '{id}' is now {st}"));
                }
                _ => {
                    self.status_message = Some(format!("Stream rule '{id}' not found"));
                }
            }
        } else {
            self.status_message = Some("Usage: /rules [list|remove <id>|toggle <id>]".to_string());
        }

        None
    }

    /// /add-dir <dir> — grant access to an additional workspace root
    /// (bd-cv653.3.12). Validated, canonicalized, added to the shared handle
    /// every tool consults, and persisted into the session header.
    pub(super) fn handle_slash_add_dir(&mut self, args: &str) -> Option<Cmd> {
        let raw = args.trim();
        if raw.is_empty() {
            self.status_message = Some("Usage: /add-dir <directory>".to_string());
            self.scroll_to_bottom();
            return None;
        }
        let expanded = expand_home_path(raw);
        let canonical = match crate::workspace::validate_new_root(&expanded) {
            Ok(canonical) => canonical,
            Err(err) => {
                self.status_message = Some(err.to_string());
                self.scroll_to_bottom();
                return None;
            }
        };
        let already = self
            .workspace
            .snapshot_or(&self.cwd)
            .contains_canonical(&canonical);
        if !already {
            self.workspace.add_root(&canonical);
        }
        // Persist exactly the live root set: persisting a path the handle
        // rejected as "already covered" would silently promote it to a real
        // root on session resume.
        let roots = self.workspace.additional_roots();
        let persisted = self.session.try_lock().is_ok_and(|mut guard| {
            guard.set_additional_roots(&roots);
            true
        });
        let display = canonical.display().to_string();
        self.status_message = Some(if already {
            format!("Already a workspace root: {display}")
        } else if persisted {
            format!("Workspace root added: {display}")
        } else {
            format!("Workspace root added (not persisted; session busy): {display}")
        });
        self.scroll_to_bottom();
        None
    }

    /// /remove-dir <dir> — revoke an additional workspace root immediately
    /// (bd-cv653.3.12). Tools snapshot the shared set at execution time, so
    /// the next read/edit/search in that root fails closed. The primary cwd
    /// can never be removed.
    pub(super) fn handle_slash_remove_dir(&mut self, args: &str) -> Option<Cmd> {
        let raw = args.trim();
        if raw.is_empty() {
            self.status_message = Some("Usage: /remove-dir <directory>".to_string());
            self.scroll_to_bottom();
            return None;
        }
        let canonical = crate::extensions::safe_canonicalize(&expand_home_path(raw));
        if self.workspace.snapshot_or(&self.cwd).primary() == canonical.as_path() {
            self.status_message = Some("Cannot remove the primary working directory".to_string());
            self.scroll_to_bottom();
            return None;
        }
        let removed = self.workspace.remove_root(&canonical);
        if removed {
            let remaining: Vec<std::path::PathBuf> = self
                .workspace
                .additional_roots()
                .into_iter()
                .filter(|root| root != &canonical)
                .collect();
            if let Ok(mut guard) = self.session.try_lock() {
                guard.set_additional_roots(&remaining);
            }
            self.status_message = Some(format!("Workspace root removed: {}", canonical.display()));
        } else {
            self.status_message = Some(format!("Not a workspace root: {}", canonical.display()));
        }
        self.scroll_to_bottom();
        None
    }

    /// /crash [show|delete] — inspect or clear redacted crash bundles
    /// (bd-cv653.7.12). Bare `/crash` lists bundles; `send` is intentionally
    /// absent from auto-transmission — use the bundle path with your own
    /// transport after reviewing the preview.
    /// `/btw <question>` — ephemeral side question on the smol role
    /// (bd-cv653.3.16). The answer renders as a system card and is never
    /// written to the session JSONL: the call builds a throwaway message
    /// list that shares nothing with the session writer.
    pub(super) fn handle_slash_btw(&mut self, args: &str) -> Option<Cmd> {
        // Lock scope computes the outcome; self mutations happen after the
        // guard drops.
        enum BtwPrepared {
            Ready { context: String, question: String },
            TransformRefused { message: String },
            AgentBusy,
        }
        let Some(client) = self.btw_client.clone() else {
            self.status_message = Some(
                "/btw unavailable: no smol role model configured (set --smol or model_roles.smol)"
                    .to_string(),
            );
            self.scroll_to_bottom();
            return None;
        };
        let question = args.trim().to_string();
        if question.is_empty() {
            self.status_message = Some("Usage: /btw <question>".to_string());
            self.scroll_to_bottom();
            return None;
        }
        // Context + question get the SAME outbound hygiene as the main
        // provider path: the live message list carries raw user text (the
        // vault only rewrites the outbound clone), and the smol role can be
        // a different vendor entirely. Block mode refuses here too.
        let prepared = self.agent.try_lock().map_or(
            // Contended agent lock: answer without context, but say so —
            // a silent empty context reads as a model failure.
            BtwPrepared::AgentBusy,
            |mut agent| {
                let snapshot = agent.messages().to_vec();
                let summary = pi::btw::build_context_summary(&snapshot);
                let transformed =
                    agent
                        .secrets_transform_outbound_text(&summary)
                        .and_then(|context| {
                            agent
                                .secrets_transform_outbound_text(&question)
                                .map(|question| (context, question))
                        });
                match transformed {
                    Ok((context, question)) => BtwPrepared::Ready { context, question },
                    Err(err) => BtwPrepared::TransformRefused {
                        message: format!("/btw refused: {err}"),
                    },
                }
            },
        );
        let (context, question) = match prepared {
            BtwPrepared::Ready { context, question } => (context, question),
            BtwPrepared::TransformRefused { message } => {
                self.status_message = Some(message);
                self.scroll_to_bottom();
                return None;
            }
            BtwPrepared::AgentBusy => {
                self.status_message = Some(String::from(
                    "(/btw) agent busy — answering without conversation context",
                ));
                (String::new(), question)
            }
        };
        // asupersync TryLockError carries the guard, so the match temporary
        // would pin the immutable borrow across self mutations (bd-9x70g
        // unblock). Extract an owned decision first, mutate after.
        let (owner_session_id, session_busy) = self.session.try_lock().map_or_else(
            |_| (String::new(), true),
            |session| (session.header.id.clone(), false),
        );
        if session_busy {
            self.status_message = Some("/btw unavailable: session is busy".to_string());
            self.scroll_to_bottom();
            return None;
        }
        self.messages.push(ConversationMessage {
            role: MessageRole::System,
            content: format!("(/btw) {question}"),
            thinking: None,
            collapsed: false,
        });
        self.status_message = Some("(/btw) thinking...".to_string());
        let runtime = self.runtime_handle.clone();
        let event_tx = self.event_tx.clone();
        runtime.spawn(async move {
            let result = client.ask(&context, &question).await;
            // Display-only delivery via the UI event channel; the session
            // writer never sees this message.
            // SessionSystemNote is display-only. PiMsg::System/AgentError
            // reset live agent state (Idle + dropped abort handle), while an
            // answer landing after a session switch must be discarded.
            let message = match result {
                Ok(answer) => format!("(/btw) {answer}"),
                Err(err) => format!("(/btw) failed: {err}"),
            };
            let msg = PiMsg::SessionSystemNote {
                owner_session_id,
                message,
            };
            let _ = crate::interactive::enqueue_pi_event(&event_tx, &Cx::for_request(), msg).await;
        });
        self.scroll_to_bottom();
        None
    }

    fn completed_tan_event(
        owner_session_id: String,
        completion: &pi::subagents::TanCompletion,
    ) -> PiMsg {
        let card = pi::jobs::push_completion_notice(&owner_session_id, completion.follow_up_text())
            .map_or_else(
                |err| format!("(/tan failed to queue follow-up)\n{err}"),
                |()| completion.card_text(),
            );
        PiMsg::SessionSystemNote {
            owner_session_id,
            message: card,
        }
    }

    /// `/tan <work>` — run tangential work in a background task-role child
    /// (bd-cv653.3.16). The command returns immediately; the child joins the
    /// hub roster as `kind=tan`, renders a display-only completion card, and
    /// queues its summary through the background-jobs follow-up seam for the
    /// parent agent's next idle turn boundary.
    pub(super) fn handle_slash_tan(&mut self, args: &str) -> Option<Cmd> {
        enum TanGate {
            Enabled,
            Disabled,
            Busy,
        }

        let work = args.trim().to_string();
        if work.is_empty() {
            self.status_message = Some("Usage: /tan <work>".to_string());
            self.scroll_to_bottom();
            return None;
        }

        let gate = self.agent.try_lock().map_or(TanGate::Busy, |agent| {
            if agent.has_tool("subagent") {
                TanGate::Enabled
            } else {
                TanGate::Disabled
            }
        });
        match gate {
            TanGate::Busy => {
                self.status_message = Some("/tan unavailable: agent session is busy".to_string());
                self.scroll_to_bottom();
                return None;
            }
            TanGate::Disabled => {
                self.status_message = Some(
                    "/tan unavailable: enable the opt-in subagent tool with --tools ...subagent"
                        .to_string(),
                );
                self.scroll_to_bottom();
                return None;
            }
            TanGate::Enabled => {}
        }

        // Same TryLockError-guard temporary pattern as /btw (bd-9x70g).
        let (owner_session_id, session_busy) = self.session.try_lock().map_or_else(
            |_| (String::new(), true),
            |session| (session.header.id.clone(), false),
        );
        if session_busy {
            self.status_message = Some("/tan unavailable: session is busy".to_string());
            self.scroll_to_bottom();
            return None;
        }

        let tool = pi::subagents::SubagentTool::new(&self.cwd)
            .with_role_model_spec(pi::app::subagent_role_spec(&self.config));
        let runtime = self.runtime_handle.clone();
        let event_tx = self.event_tx.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        let display_work = work.clone();
        runtime.spawn(async move {
            let event = match tool.run_background_tan(&work).await {
                Ok(completion) => Self::completed_tan_event(owner_session_id, &completion),
                Err(err) => PiMsg::SessionSystemNote {
                    owner_session_id,
                    message: format!("(/tan failed)\n{err}"),
                },
            };
            let _ = crate::interactive::enqueue_pi_event(&event_tx, &task_cx, event).await;
        });

        self.messages.push(ConversationMessage {
            role: MessageRole::System,
            content: format!("(/tan started) {display_work}"),
            thinking: None,
            collapsed: false,
        });
        self.status_message = Some("(/tan) running in background".to_string());
        self.scroll_to_bottom();
        None
    }

    pub(super) fn handle_slash_crash(&mut self, args: &str) -> Option<Cmd> {
        let agent_dir = crate::config::Config::global_dir();
        match args.trim() {
            "" => {
                let bundles = pi::crash::list_bundles(&agent_dir);
                let message = if bundles.is_empty() {
                    "No crash bundles recorded.".to_string()
                } else {
                    bundles
                        .iter()
                        .map(|b| {
                            format!(
                                "{} {} {}{}",
                                b.created_at,
                                b.kind,
                                b.dir.display(),
                                if b.noticed { "" } else { " (new)" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: message,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
            }
            "show" => {
                let report = pi::crash::show_latest(&agent_dir)
                    .unwrap_or_else(|| "No crash bundles recorded.".into());
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: report,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
            }
            "delete" => {
                let removed = pi::crash::delete_all(&agent_dir);
                self.status_message = Some(format!("Deleted {removed} crash bundle(s)"));
                self.scroll_to_bottom();
            }
            other => {
                self.status_message = Some(format!("Usage: /crash [show|delete] (got: {other})"));
                self.scroll_to_bottom();
            }
        }
        None
    }

    pub(super) fn handle_slash_omfg(&mut self, args: &str) -> Option<Cmd> {
        let args = args.trim();
        if args.is_empty() {
            self.status_message = Some("Usage: /omfg <complaint about model behavior>".to_string());
            return None;
        }

        let project_root = self.cwd.clone();
        match crate::stream_rules::GrievancesLedger::record_complaint(&project_root, args, None) {
            Ok(g) => {
                let candidate = crate::stream_rules::GrievancesLedger::forge_candidate_rule(&g);
                let mut store =
                    crate::stream_rules::StreamRuleStore::load_for_project(&project_root);
                let _ = store.add_rule(candidate.clone(), false);

                let content = format!(
                    "### 📝 Grievance Logged & Stream Rule Forged\n\n\
                     - **Grievance ID:** `{gid}`\n\
                     - **Complaint:** {complaint}\n\n\
                     **Generated TTSR Stream Rule (`{rid}`):**\n\
                     - **Name:** {name}\n\
                     - **Pattern:** `/{pattern}/`\n\
                     - **Directive:** {body}\n\n\
                     *Rule is now active for this project and will abort & retry if this pattern occurs mid-stream.*",
                    gid = g.id,
                    complaint = g.complaint,
                    rid = candidate.id,
                    name = candidate.name,
                    pattern = candidate.pattern,
                    body = candidate.body,
                );

                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.status_message = Some(format!(
                    "Forged and activated stream rule '{}'",
                    candidate.id
                ));
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to record grievance: {e}"));
            }
        }

        None
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_commit(&mut self, args: &str) -> Option<Cmd> {
        let args = args.trim();
        let dry_run = args.contains("--dry-run")
            || args.contains("-n")
            || args == "dry-run"
            || args == "plan";
        let include_lockfiles = args.contains("--include-lockfiles");

        let status_out = match std::process::Command::new("git")
            .arg("status")
            .arg("--porcelain")
            .current_dir(&self.cwd)
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                self.status_message = Some(format!("Failed to run git status: {e}"));
                return None;
            }
        };

        let status_str = String::from_utf8_lossy(&status_out.stdout);
        let mut changed_files = Vec::new();
        for line in status_str.lines() {
            let trimmed = line.trim();
            if trimmed.len() > 3 {
                let file_path = &trimmed[3..].trim();
                let actual_path = if let Some((_, new_p)) = file_path.split_once(" -> ") {
                    new_p.trim()
                } else {
                    file_path
                };
                changed_files.push(actual_path.to_string());
            }
        }

        if changed_files.is_empty() {
            self.status_message = Some("Working tree clean; nothing to commit.".to_string());
            return None;
        }

        let diff_out = std::process::Command::new("git")
            .arg("diff")
            .arg("HEAD")
            .current_dir(&self.cwd)
            .output()
            .ok();

        let hunks = if let Some(out) = diff_out {
            let diff_str = String::from_utf8_lossy(&out.stdout);
            crate::commit_split::DiffParser::parse_unified_diff(&diff_str).unwrap_or_default()
        } else {
            Vec::new()
        };

        let options = crate::commit_split::CommitOptions {
            dry_run,
            include_lockfiles,
            all_untracked: false,
            bead_reference: None,
            custom_prefix: None,
        };

        match crate::commit_split::CommitPlanner::plan(&hunks, &changed_files, &options) {
            Ok(plan) => {
                if plan.units.is_empty() {
                    self.status_message = Some("No eligible files to commit.".to_string());
                    return None;
                }

                let mut card = format!("### 📦 Planned Atomic Commits ({})\n\n", plan.units.len());
                for (idx, unit) in plan.units.iter().enumerate() {
                    let msg = unit.formatted_message(None);
                    let _ = writeln!(card, "{}. **{}** (`{}`)", idx + 1, msg, unit.scope);
                    for f in &unit.files {
                        let _ = writeln!(card, "   - `{f}`");
                    }
                }

                if dry_run {
                    card.push_str("\n*Dry run: no commits were created.*");
                } else {
                    match crate::commit_split::CommitExecutor::execute(&self.cwd, &plan, &options) {
                        Ok(results) => {
                            let successful = results.iter().filter(|r| r.success).count();
                            let _ = writeln!(
                                card,
                                "\n\n**Committed {successful}/{} units successfully.**",
                                plan.units.len()
                            );
                            for res in results {
                                if let Some(ref sha) = res.commit_sha {
                                    let _ = writeln!(card, "- `[{sha}]` {}", res.message);
                                }
                            }
                        }
                        Err(e) => {
                            let _ = write!(card, "\n\n**Error executing commits:** {e}");
                        }
                    }
                }

                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: card,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.status_message = Some(format!(
                    "Generated commit plan with {} units",
                    plan.units.len()
                ));
            }
            Err(e) => {
                self.status_message = Some(format!("Failed to plan commits: {e}"));
            }
        }

        None
    }

    pub(super) fn handle_slash_review(&mut self, args: &str) -> Option<Cmd> {
        let args = args.trim();
        let target = if args.is_empty() {
            None
        } else {
            Some(args.to_string())
        };

        let options = crate::review::ReviewOptions {
            target,
            fail_on: None,
            confidence_threshold: 0.70,
            format: "markdown".to_string(),
            max_findings: 15,
            out_file: None,
        };

        match crate::review::CodeReviewer::review(&self.cwd, &options) {
            Ok(report) => {
                let badge = report.verdict.badge();
                let summary = report.summary.clone();
                let markdown = report.format_markdown();

                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: markdown,
                    thinking: None,
                    collapsed: false,
                });
                self.scroll_to_bottom();
                self.status_message = Some(format!("{badge}: {summary}"));
            }
            Err(e) => {
                self.status_message = Some(format!("Review failed: {e}"));
            }
        }

        None
    }

    /// Switch the active model to a role-resolved spec when one is configured.
    fn switch_to_role_spec(&mut self, spec: &str, source: &str) {
        let Some((provider, model_id)) = crate::provider_metadata::split_provider_model_spec(spec)
        else {
            return;
        };
        let entry = self
            .available_models
            .iter()
            .find(|m| {
                crate::provider_metadata::provider_ids_match(&m.model.provider, provider)
                    && m.model.id.eq_ignore_ascii_case(model_id)
            })
            .cloned()
            .or_else(|| crate::models::ad_hoc_model_entry(provider, model_id));
        if let Some(entry) = entry {
            let key = resolve_model_key_from_default_auth(&entry);
            if let Ok(provider_impl) = providers::create_provider(&entry, self.extensions.as_ref())
            {
                let _ = self.switch_active_model(&entry, provider_impl, key.as_deref(), source);
            }
        }
    }

    fn enter_plan_mode(&mut self, plan_state: &crate::plan::PlanState) {
        if plan_state.mode() != crate::plan::PlanMode::Off {
            self.status_message = Some(format!(
                "Already in plan mode ({})",
                plan_state.mode().as_str()
            ));
            return;
        }
        plan_state.enter_planning();
        plan_state
            .stash_previous_model(&self.model_entry.model.provider, &self.model_entry.model.id);
        if let Some(spec) = self
            .config
            .model_roles
            .as_ref()
            .and_then(|roles| crate::app::role_spec_from_settings(roles, ModelRole::Plan))
            .map(str::to_string)
        {
            self.switch_to_role_spec(&spec, "plan-role");
        }
        Self::log_plan_transition(&self.session, "planning");
        self.messages.push(ConversationMessage {
            role: MessageRole::System,
            content: "Plan mode: read-only. Inspect with read/grep/find/ls, then call submit_plan with the full plan for review.".to_string(),
            thinking: None,
            collapsed: false,
        });
        self.status_message = Some("Plan mode: planning (read-only)".to_string());
        self.scroll_to_bottom();
    }

    fn approve_plan_mode(&mut self, plan_state: &crate::plan::PlanState) {
        match plan_state.approve() {
            Some(plan) => {
                // Pin the plan into the agent's system context for the
                // execution turns (bd-cv653.3.5).
                if let Ok(mut agent_guard) = self.agent.try_lock() {
                    let existing = agent_guard
                        .system_prompt()
                        .map(str::to_string)
                        .unwrap_or_default();
                    agent_guard.set_system_prompt(Some(format!(
                        "{existing}\n\n## Approved Plan (execute this)\n\n{plan}"
                    )));
                }
                if let Some((provider, model_id)) = plan_state.take_previous_model() {
                    self.switch_to_role_spec(&format!("{provider}/{model_id}"), "plan-restore");
                }
                Self::log_plan_transition(&self.session, "approved");
                self.status_message = Some("Plan approved — execute it.".to_string());
            }
            None => {
                self.status_message = Some("No submitted plan to approve".to_string());
            }
        }
    }

    /// `/advisor` (bd-cv653.3.3): status + pause/resume for the turn-review
    /// second model.
    fn handle_slash_advisor(&mut self, args: &str) -> Option<Cmd> {
        let sub = args.trim().to_ascii_lowercase();
        let configured = self
            .config
            .model_roles
            .as_ref()
            .and_then(|roles| crate::app::role_spec_from_settings(roles, ModelRole::Advisor))
            .map(str::to_string);
        match sub.as_str() {
            "" | "status" => {
                let paused =
                    crate::advisor::ADVISOR_PAUSED.load(std::sync::atomic::Ordering::SeqCst);
                let state = match (&configured, paused) {
                    (Some(spec), false) => format!("active on {spec}"),
                    (Some(spec), true) => format!("configured ({spec}) but paused"),
                    (None, _) => "not configured (set modelRoles.advisor or --advisor)".to_string(),
                };
                self.status_message = Some(format!("Advisor: {state}"));
            }
            "pause" => {
                crate::advisor::ADVISOR_PAUSED.store(true, std::sync::atomic::Ordering::SeqCst);
                self.status_message = Some("Advisor paused".to_string());
            }
            "resume" => {
                crate::advisor::ADVISOR_PAUSED.store(false, std::sync::atomic::Ordering::SeqCst);
                self.status_message = Some("Advisor resumed".to_string());
            }
            other => {
                self.status_message = Some(format!(
                    "Unknown /advisor subcommand {other:?}: use /advisor [status|pause|resume]"
                ));
            }
        }
        None
    }

    pub(super) fn handle_slash_thinking(&mut self, args: &str) -> Option<Cmd> {
        let value = args.trim();
        if value.is_empty() {
            let current = self
                .session
                .try_lock()
                .ok()
                .and_then(|guard| guard.header.thinking_level.clone())
                .unwrap_or_else(|| ThinkingLevel::Off.to_string());
            self.status_message = Some(format!("Thinking level: {current}"));
            return None;
        }

        let level: ThinkingLevel = match value.parse() {
            Ok(level) => level,
            Err(err) => {
                self.status_message = Some(err);
                return None;
            }
        };

        let effective_level = self.model_entry.clamp_thinking_level(level);
        let Ok(mut session_guard) = self.session.try_lock() else {
            self.status_message = Some("Session busy; try again".to_string());
            return None;
        };
        let previous_level = session_thinking_level(&session_guard);
        session_guard.header.thinking_level = Some(effective_level.to_string());
        let changed = previous_level != Some(effective_level);
        if changed {
            session_guard.append_thinking_level_change(effective_level.to_string());
        }
        drop(session_guard);
        if changed {
            self.spawn_save_session();
        }

        if let Ok(mut agent_guard) = self.agent.try_lock() {
            agent_guard.stream_options_mut().thinking_level = Some(effective_level);
        }

        self.status_message = Some(format!("Thinking level: {effective_level}"));
        None
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_scoped_models(&mut self, args: &str) -> Option<Cmd> {
        let value = args.trim();
        if value.is_empty() {
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: self.format_scoped_models_status(),
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_last_match("Scoped models");
            return None;
        }

        if value.eq_ignore_ascii_case("clear") {
            let previous_patterns = self
                .config
                .enabled_models
                .as_deref()
                .unwrap_or(&[])
                .to_vec();
            self.config.enabled_models = Some(Vec::new());
            self.model_scope.clear();

            let global_dir = Config::global_dir();
            let patch = json!({ "enabled_models": [] });
            let cleared_msg = if previous_patterns.is_empty() {
                "Scoped models cleared (was: all models)".to_string()
            } else {
                format!(
                    "Scoped models cleared: removed {} pattern(s) (was: {})",
                    previous_patterns.len(),
                    previous_patterns.join(", ")
                )
            };
            if let Err(err) = Config::patch_settings_with_roots(
                SettingsScope::Project,
                &global_dir,
                &self.cwd,
                patch,
            ) {
                tracing::warn!("Failed to persist enabled_models: {err}");
                self.status_message = Some(format!("{cleared_msg} (not saved: {err})"));
            } else {
                self.status_message = Some(cleared_msg);
            }
            return None;
        }

        let patterns = parse_scoped_model_patterns(value);
        if patterns.is_empty() {
            self.status_message = Some("Usage: /scoped-models [patterns|clear]".to_string());
            return None;
        }

        let resolved = match resolve_scoped_model_entries(&patterns, &self.available_models) {
            Ok(resolved) => resolved,
            Err(err) => {
                self.status_message =
                    Some(format!("{err}\n  Example: /scoped-models gpt-4*,claude-3*"));
                return None;
            }
        };

        self.model_scope = resolved;
        self.config.enabled_models = Some(patterns.clone());

        let match_count = self.model_scope.len();

        // Build a preview of matched models for the conversation pane.
        let mut preview = String::new();
        if match_count == 0 {
            let _ = writeln!(
                preview,
                "Warning: No models matched patterns: {}",
                patterns.join(", ")
            );
            let _ = writeln!(preview, "Ctrl+P cycling will use all available models.");
        } else {
            let _ = writeln!(preview, "Matching {match_count} model(s):");
            let mut model_names: Vec<String> = self
                .model_scope
                .iter()
                .map(|e| format!("{}/{}", e.model.provider, e.model.id))
                .collect();
            model_names.sort_by_key(|s| s.to_ascii_lowercase());
            model_names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
            for name in &model_names {
                let _ = writeln!(preview, "  {name}");
            }
        }
        let _ = writeln!(
            preview,
            "Patterns saved. Press Ctrl+P to cycle through matched models."
        );

        self.messages.push(ConversationMessage {
            role: MessageRole::System,
            content: preview,
            thinking: None,
            collapsed: false,
        });
        self.scroll_to_bottom();

        let status = if match_count == 0 {
            "Scoped models updated: 0 matched; cycling will use all available models".to_string()
        } else {
            format!("Scoped models updated: {match_count} matched")
        };
        let global_dir = Config::global_dir();
        let patch = json!({ "enabled_models": patterns });
        if let Err(err) =
            Config::patch_settings_with_roots(SettingsScope::Project, &global_dir, &self.cwd, patch)
        {
            tracing::warn!("Failed to persist enabled_models: {err}");
            self.status_message = Some(format!("{status} (not saved: {err})"));
        } else {
            self.status_message = Some(status);
        }
        None
    }

    pub(super) fn handle_slash_reload(&mut self) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot reload while processing".to_string());
            return None;
        }

        let config = self.config.clone();
        let cli = self.resource_cli.clone();
        let package_manager = self.package_manager.clone();
        let cwd = self.cwd.clone();
        let event_tx = self.event_tx.clone();
        let extensions = self.extensions.clone();
        let runtime_handle = self.runtime_handle.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);

        runtime_handle.spawn(async move {
            match ResourceLoader::load(&package_manager, &cwd, &config, &cli).await {
                Ok(mut resources) => {
                    if let Some(manager) = extensions {
                        let discovered = manager.discover_resources(&cwd, "reload").await;
                        if !discovered.is_empty()
                            && let Err(err) = resources.extend_with_paths(&cwd, &discovered)
                        {
                            tracing::warn!(
                                event = "pi.resources.reload.extension_paths_failed",
                                error = %err,
                                "Failed to apply extension-discovered resource paths"
                            );
                        }
                    }

                    let models_error =
                        match crate::auth::AuthStorage::load_async(Config::auth_path()).await {
                            Ok(auth) => {
                                let models_path = default_models_path(&Config::global_dir());
                                let registry = ModelRegistry::load(&auth, Some(models_path));
                                registry.error().map(ToString::to_string)
                            }
                            Err(err) => Some(format!("Failed to load auth.json: {err}")),
                        };

                    let (diagnostics, diag_count) =
                        build_reload_diagnostics(models_error, &resources);

                    let mut status = format!(
                        "Reloaded resources: {} skills, {} prompts, {} themes",
                        resources.skills().len(),
                        resources.prompts().len(),
                        resources.themes().len()
                    );
                    if diag_count > 0 {
                        let _ = write!(status, " ({diag_count} diagnostics)");
                    }

                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::ResourcesReloaded {
                            resources,
                            status,
                            diagnostics,
                        },
                    )
                    .await;
                }
                Err(err) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &task_cx,
                        PiMsg::AgentError(format!("Failed to reload resources: {err}")),
                    )
                    .await;
                }
            }
        });

        self.status_message = Some("Reloading resources...".to_string());
        None
    }

    /// MCP (Model Context Protocol) server management: list, add, remove,
    /// test, trust lifecycle (bd-cv653.6.1).
    ///
    /// One client registry unifies three config sources: native files
    /// (`.pi/mcp.json`, `.agents/mcp.json`, `~/.pi/agent/mcp.json`,
    /// `--mcp-config`), foreign files (`.claude/`, `.cursor/`, ...), and
    /// extension-registered specs. Server processes are capability-equivalent
    /// to `exec`: they never spawn until acknowledged via `/mcp trust`.
    pub(super) fn handle_slash_mcp(&mut self, args: &str) -> Option<Cmd> {
        let Some(manager) = self.mcp_manager.clone() else {
            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: "MCP client is unavailable (bootstrap failed at startup).".to_string(),
                thinking: None,
                collapsed: false,
            });
            return None;
        };
        let mut parts = args.split_whitespace();
        let subcommand = parts.next().unwrap_or("list");
        let rest: Vec<&str> = parts.collect();
        match subcommand {
            "list" => self.mcp_handle_list(&manager),
            "add" => self.mcp_handle_add(&rest),
            "remove" => self.mcp_handle_remove(&rest),
            "trust" | "deny" | "test" => self.mcp_handle_action(&manager, subcommand, &rest),
            other => {
                self.status_message = Some(format!(
                    "unknown /mcp subcommand {other:?}; expected list|add|remove|test|trust|deny"
                ));
                None
            }
        }
    }

    /// `/mcp list`: every server with provenance, trust state, and health.
    fn mcp_handle_list(&mut self, manager: &crate::mcp::McpManager) -> Option<Cmd> {
        let rows = manager.list();
        let mut content = String::from("MCP servers (Model Context Protocol)\n");
        if rows.is_empty() {
            content.push_str(
                "\n  No MCP servers configured. Add one with:\n\
                 \x20   /mcp add <name> <command> [args...]     (stdio server)\n\
                 \x20   /mcp add <name> --url <https://...>     (HTTP server)\n\
                 or create .pi/mcp.json. Foreign configs (.claude/mcp.json,\n\
                 .cursor/mcp.json, ...) are discovered automatically.\n",
            );
        } else {
            let _ = writeln!(content, "\n  {} configured:", rows.len());
            for row in &rows {
                let _ = writeln!(
                    content,
                    "    • {} — {} [{}; trust: {}; {}]",
                    row.name, row.target, row.provenance, row.trust, row.health
                );
            }
            if rows.iter().any(|row| row.trust == "pending") {
                content.push_str(
                    "\nPending servers never spawn. Acknowledge one with /mcp trust <name>.\n",
                );
            }
        }
        for warning in manager.warnings() {
            let _ = writeln!(
                content,
                "  ⚠ {}: {} ({})",
                warning.source_file.display(),
                warning.entry,
                warning.reason
            );
        }
        self.messages.push(ConversationMessage {
            role: MessageRole::System,
            content,
            thinking: None,
            collapsed: false,
        });
        self.scroll_to_last_match("MCP servers");
        None
    }

    /// `/mcp add <name> <command...>` or `/mcp add <name> --url <url>`.
    fn mcp_handle_add(&mut self, rest: &[&str]) -> Option<Cmd> {
        let Some(name) = rest.first() else {
            self.status_message = Some(
                "usage: /mcp add <name> <command...> | /mcp add <name> --url <url>".to_string(),
            );
            return None;
        };
        let name = (*name).to_string();
        let entry_value = if rest.get(1) == Some(&"--url") {
            let Some(url) = rest.get(2) else {
                self.status_message = Some("/mcp add --url requires a URL".to_string());
                return None;
            };
            serde_json::json!({ "url": url })
        } else {
            match rest.get(1) {
                Some(command) if !command.is_empty() => {
                    let args: Vec<&str> = rest.iter().skip(2).copied().collect();
                    serde_json::json!({ "command": command, "args": args })
                }
                _ => {
                    self.status_message = Some("/mcp add requires a command or --url".to_string());
                    return None;
                }
            }
        };
        let path = self.cwd.join(".pi/mcp.json");
        let result = crate::mcp::config::read_project_config(&path).and_then(|mut value| {
            value["mcpServers"][&name] = entry_value;
            crate::mcp::config::write_project_config(&path, &value)
        });
        match result {
            Ok(()) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!(
                        "Added MCP server {name:?} to {}. It is pending trust — run /mcp trust {name} to allow spawning it (takes effect next session for tool mounting).",
                        path.display()
                    ),
                    thinking: None,
                    collapsed: false,
                });
            }
            Err(err) => {
                self.status_message = Some(format!("/mcp add failed: {err}"));
            }
        }
        None
    }

    /// `/mcp remove <name>`.
    fn mcp_handle_remove(&mut self, rest: &[&str]) -> Option<Cmd> {
        let Some(name) = rest.first() else {
            self.status_message = Some("usage: /mcp remove <name>".to_string());
            return None;
        };
        let name = (*name).to_string();
        let path = self.cwd.join(".pi/mcp.json");
        let result = crate::mcp::config::read_project_config(&path).and_then(|mut value| {
            if let Some(servers) = value["mcpServers"].as_object_mut()
                && servers.remove(&name).is_none()
            {
                return Err(crate::error::Error::tool(
                    "mcp",
                    format!("[MCP_UNKNOWN_SERVER] {name:?} is not in {}", path.display()),
                ));
            }
            crate::mcp::config::write_project_config(&path, &value)
        });
        match result {
            Ok(()) => {
                self.messages.push(ConversationMessage {
                    role: MessageRole::System,
                    content: format!(
                        "Removed MCP server {name:?} from {} (takes effect next session).",
                        path.display()
                    ),
                    thinking: None,
                    collapsed: false,
                });
            }
            Err(err) => self.status_message = Some(format!("/mcp remove failed: {err}")),
        }
        None
    }

    /// `/mcp trust|deny|test <name>` — async via the runtime, reporting back
    /// as a system message; newly available tools mount into the live agent.
    fn mcp_handle_action(
        &mut self,
        manager: &std::sync::Arc<crate::mcp::McpManager>,
        subcommand: &str,
        rest: &[&str],
    ) -> Option<Cmd> {
        let Some(name) = rest.first() else {
            self.status_message = Some(format!("usage: /mcp {subcommand} <name>"));
            return None;
        };
        let name = (*name).to_string();
        let status_label = name.clone();
        let subcommand = subcommand.to_string();
        let status_verb = subcommand.clone();
        let runtime_handle = self.runtime_handle.clone();
        let event_tx = self.event_tx.clone();
        let agent = Arc::clone(&self.agent);
        let manager = manager.clone();
        let task_cx = Cx::current().unwrap_or_else(Cx::for_request);
        runtime_handle.spawn(async move {
            let outcome = match subcommand.as_str() {
                "deny" => manager.deny(&name).await.map(|()| Vec::new()),
                "test" => manager.test(&name).await,
                _ => manager.trust(&name).await,
            };
            let message = match outcome {
                Ok(_) if subcommand == "deny" => format!("MCP server {name:?} denied and stopped."),
                Ok(tools) => {
                    // Mount any newly available tools into the live agent
                    // (extend_tools invalidates the def cache).
                    let wrappers = crate::mcp::mount_tools(&manager);
                    let mounted = wrappers.len();
                    if mounted > 0
                        && let Ok(mut agent) = agent.lock(&task_cx).await
                    {
                        agent.extend_tools(wrappers);
                    }
                    let verb = if subcommand == "test" {
                        "tested"
                    } else {
                        "trusted"
                    };
                    let mut line = format!(
                        "MCP server {name:?} {verb}: {} tool(s) available.",
                        tools.len()
                    );
                    for tool in tools.iter().take(12) {
                        let _ = writeln!(line, "  • {} — {}", tool.name, tool.description);
                    }
                    if tools.len() > 12 {
                        let _ = writeln!(line, "  … and {} more", tools.len() - 12);
                    }
                    if mounted > 0 {
                        let _ = writeln!(
                            line,
                            "Mounted {mounted} mcp__* tool(s) into the live session."
                        );
                    }
                    line
                }
                Err(err) => format!("MCP {name:?}: {err}"),
            };
            let _ = enqueue_pi_event(&event_tx, &task_cx, PiMsg::System(message)).await;
        });
        self.status_message = Some(format!("MCP {status_verb} {status_label:?} started…"));
        None
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_template(&mut self, args: &str) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot expand template while processing".to_string());
            return None;
        }

        let trimmed = args.trim();
        if trimmed.is_empty() {
            let templates = self.resources.prompts();
            if templates.is_empty() {
                self.status_message = Some("No prompt templates loaded".to_string());
                return None;
            }

            let mut listing = String::from("Available prompt templates:\n");
            for template in templates {
                if template.description.trim().is_empty() {
                    let _ = writeln!(listing, "  /{}", template.name);
                } else {
                    let _ = writeln!(listing, "  /{} - {}", template.name, template.description);
                }
            }

            self.messages.push(ConversationMessage {
                role: MessageRole::System,
                content: listing,
                thinking: None,
                collapsed: false,
            });
            self.scroll_to_last_match("Available prompt templates");
            return None;
        }

        let history_entry = format!("/template {trimmed}");

        let (name, rest) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        let name = name.trim_start_matches('/');
        if name.is_empty() {
            self.status_message = Some("Usage: /template <name> [args]".to_string());
            return None;
        }

        let raw_input = if rest.trim().is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {rest}")
        };

        let expanded = {
            let templates = self.resources.prompts();
            if templates.iter().all(|template| template.name != name) {
                self.status_message = Some(format!("Template not found: {name}"));
                return None;
            }
            crate::resources::expand_prompt_template(&raw_input, templates)
        };

        if expanded.trim().is_empty() {
            self.status_message = Some("Template expansion produced empty output".to_string());
            return None;
        }

        let (message_without_refs, file_refs) = self.extract_file_references(&expanded);
        let message_for_agent = message_without_refs.trim().to_string();

        if !file_refs.is_empty() {
            let auto_resize = self
                .config
                .images
                .as_ref()
                .and_then(|images| images.auto_resize)
                .unwrap_or(true);

            let processed = match process_file_arguments(
                &file_refs,
                &self.cwd,
                auto_resize,
                self.workspace(),
            ) {
                Ok(processed) => processed,
                Err(err) => {
                    self.status_message = Some(err.to_string());
                    return None;
                }
            };

            let keyword_scan_source = raw_input;
            let mut text = processed.text;
            if !message_for_agent.trim().is_empty() {
                text.push_str(&message_for_agent);
            }

            let mut content = Vec::new();
            if !text.trim().is_empty() {
                content.push(ContentBlock::Text(TextContent::new(text)));
            }
            for image in processed.images {
                content.push(ContentBlock::Image(image));
            }

            if content.is_empty() {
                self.status_message =
                    Some("Template expansion produced no usable content".to_string());
                return None;
            }

            self.history.push(history_entry);
            let display = super::conversation::content_blocks_to_text(&content);
            return self.submit_content_with_display_and_keyword_source(
                content,
                &display,
                Some(keyword_scan_source),
            );
        }

        if message_for_agent.is_empty() {
            self.status_message = Some("Template expansion produced empty output".to_string());
            return None;
        }

        self.history.push(history_entry);
        let content = vec![ContentBlock::Text(TextContent::new(message_for_agent))];
        let display = super::conversation::content_blocks_to_text(&content);
        self.submit_content_with_display_and_keyword_source(content, &display, Some(raw_input))
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentState, PendingInput, PendingLoginKind, PiApp, SlashCommand};
    use super::{
        ExcludedBashPersistenceOutcome, PiMsg, parse_bash_command, parse_extension_command,
        persist_excluded_bash_execution, should_show_startup_oauth_hint, spawn_bash_completion,
    };
    use crate::agent::{Agent, AgentConfig, QueuedAgentMessage};
    use crate::auth::{AuthCredential, AuthStorage};
    use crate::config::Config;
    use crate::extensions::ExtensionManager;
    use crate::keybindings::KeyBindings;
    use crate::model::{Message as ModelMessage, StreamEvent, Usage, UserContent, UserMessage};
    use crate::models::{ExtensionProviderBinding, ModelEntry};
    use crate::package_manager::PackageManager;
    use crate::provider::{Context, Provider, StreamOptions};
    use crate::provider::{InputType, Model, ModelCost};
    use crate::resources::{ResourceCliOptions, ResourceLoader};
    use crate::session::{Session, SessionEntry, SessionMessage};
    use crate::tools::ToolRegistry;
    use asupersync::Cx;
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::sync::{Mutex, OwnedMutexGuard};
    use futures::stream;
    use std::collections::{HashMap, HashSet};
    use std::path::Path;
    use std::pin::Pin;
    use std::sync::{Arc, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    struct DummyProvider;

    #[async_trait::async_trait]
    impl Provider for DummyProvider {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn api(&self) -> &'static str {
            "dummy"
        }

        fn model_id(&self) -> &'static str {
            "dummy-model"
        }

        async fn stream(
            &self,
            _context: &Context<'_>,
            _options: &StreamOptions,
        ) -> crate::error::Result<
            Pin<Box<dyn futures::Stream<Item = crate::error::Result<StreamEvent>> + Send>>,
        > {
            Ok(Box::pin(stream::empty()))
        }
    }

    fn runtime() -> &'static asupersync::runtime::Runtime {
        static RT: OnceLock<asupersync::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| {
            RuntimeBuilder::multi_thread()
                .blocking_threads(1, 4)
                .build()
                .expect("build runtime")
        })
    }

    fn excluded_bash_message(command: &str) -> SessionMessage {
        SessionMessage::BashExecution {
            command: command.to_string(),
            output: "side effect completed".to_string(),
            exit_code: 0,
            cancelled: Some(false),
            truncated: Some(false),
            full_output_path: None,
            timestamp: None,
            extra: HashMap::from([(
                "excludeFromContext".to_string(),
                serde_json::Value::Bool(true),
            )]),
        }
    }

    fn assert_exact_excluded_bash_record(
        session: &Session,
        expected_command: &str,
        expected_output: &str,
        expected_timestamp_range: Option<std::ops::RangeInclusive<i64>>,
    ) {
        let records = session
            .entries
            .iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message(message) => match &message.message {
                    SessionMessage::BashExecution {
                        command,
                        output,
                        exit_code,
                        cancelled,
                        truncated,
                        full_output_path,
                        timestamp,
                        extra,
                    } => Some((
                        command,
                        output,
                        exit_code,
                        cancelled,
                        truncated,
                        full_output_path,
                        timestamp,
                        extra,
                    )),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 1, "exactly one bash record must exist");
        let (command, output, exit_code, cancelled, truncated, full_output_path, timestamp, extra) =
            records[0];
        assert_eq!(command, expected_command);
        assert_eq!(output, expected_output);
        assert_eq!(*exit_code, 0);
        assert_eq!(*cancelled, Some(false));
        assert_eq!(*truncated, Some(false));
        assert!(full_output_path.is_none());
        match expected_timestamp_range {
            Some(range) => assert!(
                timestamp
                    .as_ref()
                    .is_some_and(|value| range.contains(value)),
                "bash timestamp {timestamp:?} must fall inside the submission window {range:?}"
            ),
            None => assert!(timestamp.is_none()),
        }
        assert_eq!(
            extra,
            &HashMap::from([(
                "excludeFromContext".to_string(),
                serde_json::Value::Bool(true),
            )])
        );
    }

    #[test]
    fn excluded_bash_persistence_success_reopens_exact_record() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::create_with_dir(Some(
            temp.path().join("sessions"),
        ))));
        let cx = Cx::for_testing();

        let persisted_path = runtime().block_on(async {
            let outcome = persist_excluded_bash_execution(
                Arc::clone(&session),
                excluded_bash_message("create-once"),
                true,
                &cx,
            )
            .await;
            assert_eq!(outcome, ExcludedBashPersistenceOutcome::Saved);

            let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock saved session");
            guard.path.clone().expect("saved session path")
        });

        let reopened = runtime()
            .block_on(Session::open(persisted_path.to_string_lossy().as_ref()))
            .expect("reopen saved session");
        assert_exact_excluded_bash_record(&reopened, "create-once", "side effect completed", None);
    }

    #[test]
    fn excluded_bash_save_failure_preserves_result_and_warns_against_rerun() {
        let temp = TempDir::new().expect("tempdir");
        let blocked_path = temp.path().join("blocked.jsonl");
        std::fs::create_dir(&blocked_path).expect("create directory at session path");

        let mut raw_session = Session::create_with_dir(Some(temp.path().join("sessions")));
        raw_session.path = Some(blocked_path.clone());
        let session = Arc::new(Mutex::new(raw_session));
        let cx = Cx::for_testing();

        let outcome = runtime().block_on(persist_excluded_bash_execution(
            Arc::clone(&session),
            excluded_bash_message("charge-card-once"),
            true,
            &cx,
        ));
        assert_eq!(
            outcome,
            ExcludedBashPersistenceOutcome::NotConfirmed {
                pending_mutations: Some(1),
                failed_flushes: Some(1),
            }
        );

        let warning = outcome.warning_text().expect("persistence warning");
        assert!(warning.contains("Execution ended and may have performed side effects"));
        assert!(warning.contains("do not rerun"));
        assert!(warning.contains("Pending mutation slots (bounded/coalescing): 1"));
        assert!(warning.contains("Total failed save attempts: 1"));
        assert!(
            !warning.contains(blocked_path.to_string_lossy().as_ref()),
            "user-facing warning should not leak storage paths"
        );

        runtime().block_on(async {
            let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock failed-save session");
            assert_exact_excluded_bash_record(
                &guard,
                "charge-card-once",
                "side effect completed",
                None,
            );
            let metrics = guard.autosave_metrics();
            assert_eq!(metrics.pending_mutations, 1);
            assert_eq!(metrics.flush_failed, 1);
        });
    }

    #[test]
    fn excluded_bash_completion_survives_cancelled_command_context() {
        let temp = TempDir::new().expect("tempdir");
        let blocked_path = temp.path().join("blocked.jsonl");
        std::fs::create_dir(&blocked_path).expect("create directory at session path");

        let mut raw_session = Session::create_with_dir(Some(temp.path().join("sessions")));
        raw_session.path = Some(blocked_path);
        let session = Arc::new(Mutex::new(raw_session));
        let (event_tx, mut event_rx) = asupersync::channel::mpsc::channel(4);
        let command_runtime_handle = runtime().handle();
        let completion_runtime_handle = command_runtime_handle.clone();
        let session_for_task = Arc::clone(&session);

        command_runtime_handle.spawn_with_cx(move |command_cx| async move {
            command_cx.set_cancel_requested(true);
            spawn_bash_completion(
                &completion_runtime_handle,
                event_tx,
                Some((
                    session_for_task,
                    excluded_bash_message("side-effect-before-cancellation"),
                    true,
                )),
                "side effect completed\n\n[Output excluded from model context]".to_string(),
                None,
            );
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let result = loop {
            match event_rx.try_recv() {
                Ok(message @ PiMsg::BashResult { .. }) => break message,
                Ok(_) | Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(other) => panic!("unexpected event after bash deadline: {other:?}"),
                Err(err) => panic!("bash completion was not delivered before deadline: {err}"),
            }
        };
        let PiMsg::BashResult {
            display,
            content_for_agent,
        } = result
        else {
            unreachable!("loop only exits with BashResult")
        };
        assert!(display.contains("side effect completed"));
        assert!(display.contains("[Persistence warning]"));
        assert!(display.contains("do not rerun"));
        assert!(content_for_agent.is_none());

        let cx = Cx::for_testing();
        runtime().block_on(async {
            let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock completion session");
            assert_exact_excluded_bash_record(
                &guard,
                "side-effect-before-cancellation",
                "side effect completed",
                None,
            );
            assert_eq!(guard.autosave_metrics().pending_mutations, 1);
            assert_eq!(guard.autosave_metrics().flush_failed, 1);
        });
    }

    #[test]
    fn bash_completion_waits_for_event_capacity_instead_of_dropping_terminal_result() {
        let (event_tx, mut event_rx) = asupersync::channel::mpsc::channel(1);
        event_tx
            .try_send(PiMsg::System("occupy channel".to_string()))
            .expect("fill event channel");
        let runtime_handle = runtime().handle();
        spawn_bash_completion(
            &runtime_handle,
            event_tx,
            None,
            "terminal bash result".to_string(),
            None,
        );

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(PiMsg::System(message)) if message == "occupy channel"
        ));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match event_rx.try_recv() {
                Ok(PiMsg::BashResult {
                    display,
                    content_for_agent,
                }) => {
                    assert_eq!(display, "terminal bash result");
                    assert!(content_for_agent.is_none());
                    break;
                }
                Ok(other) => panic!("unexpected event while awaiting bash result: {other:?}"),
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("bash result was not delivered after capacity freed: {err}"),
            }
        }
    }

    fn build_bash_test_app(
        session: Arc<Mutex<Session>>,
        cwd: &Path,
    ) -> (PiApp, asupersync::channel::mpsc::Receiver<PiMsg>) {
        let current = test_model_entry("dummy", "dummy-model");
        let agent = Agent::new(
            Arc::new(DummyProvider),
            ToolRegistry::new(&[], cwd, None),
            AgentConfig::default(),
        );
        let resources = ResourceLoader::empty(false);
        let resource_cli = ResourceCliOptions {
            no_skills: false,
            no_prompt_templates: false,
            no_extensions: false,
            no_themes: false,
            skill_paths: Vec::new(),
            prompt_paths: Vec::new(),
            extension_paths: Vec::new(),
            theme_paths: Vec::new(),
        };
        let (event_tx, event_rx) = asupersync::channel::mpsc::channel(64);
        let config = Config {
            last_changelog_version: Some(crate::platform::VERSION.to_string()),
            ..Config::default()
        };
        let app = PiApp::new(
            agent,
            session,
            config,
            resources,
            resource_cli,
            cwd.to_path_buf(),
            current.clone(),
            Vec::new(),
            vec![current],
            None,
            Vec::new(),
            event_tx,
            runtime().handle(),
            true,
            false,
            None,
            Some(KeyBindings::new()),
            Vec::new(),
            Usage::default(),
            None,
        );
        (app, event_rx)
    }

    #[test]
    fn reload_reuses_startup_package_trust_and_keeps_explicit_resources() {
        let temp = TempDir::new().expect("tempdir");
        let cwd = temp.path();
        let project_skill = cwd.join(".pi/skills/project-only/SKILL.md");
        let explicit_skill = cwd.join("explicit/explicit-only/SKILL.md");
        std::fs::create_dir_all(project_skill.parent().expect("project skill parent"))
            .expect("create project skill directory");
        std::fs::create_dir_all(explicit_skill.parent().expect("explicit skill parent"))
            .expect("create explicit skill directory");
        std::fs::write(
            &project_skill,
            "---\nname: project-only\ndescription: Project trust sentinel\n---\nProject body.\n",
        )
        .expect("write project skill");
        std::fs::write(cwd.join(".pi/settings.json"), "{}\n")
            .expect("write project trust sentinel");
        std::fs::write(
            &explicit_skill,
            "---\nname: explicit-only\ndescription: Explicit path sentinel\n---\nExplicit body.\n",
        )
        .expect("write explicit skill");

        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (mut app, mut event_rx) = build_bash_test_app(session, cwd);
        app.resource_cli.no_prompt_templates = true;
        app.resource_cli.no_extensions = true;
        app.resource_cli.no_themes = true;
        app.resource_cli.skill_paths = vec![explicit_skill.to_string_lossy().to_string()];

        assert!(app.handle_slash_reload().is_none());
        let untrusted = runtime().block_on(async {
            let cx = Cx::for_testing();
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(5),
                event_rx.recv(&cx),
            )
            .await
            .expect("untrusted reload event before timeout")
            .expect("untrusted reload event")
        });
        let PiMsg::ResourcesReloaded { resources, .. } = untrusted else {
            panic!("unexpected untrusted reload event: {untrusted:?}");
        };
        assert!(
            resources
                .skills()
                .iter()
                .any(|skill| skill.name == "explicit-only"),
            "explicit CLI resources remain authorized in an untrusted workspace"
        );
        assert!(
            resources
                .skills()
                .iter()
                .all(|skill| skill.name != "project-only"),
            "untrusted reload must not rediscover project resources"
        );

        app.set_reload_package_manager(
            PackageManager::new(cwd.to_path_buf()).with_project_trust(true),
        );
        assert!(app.handle_slash_reload().is_none());
        let trusted = runtime().block_on(async {
            let cx = Cx::for_testing();
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(5),
                event_rx.recv(&cx),
            )
            .await
            .expect("trusted reload event before timeout")
            .expect("trusted reload event")
        });
        let PiMsg::ResourcesReloaded { resources, .. } = trusted else {
            panic!("unexpected trusted reload event: {trusted:?}");
        };
        for expected in ["explicit-only", "project-only"] {
            assert!(
                resources
                    .skills()
                    .iter()
                    .any(|skill| skill.name == expected),
                "trusted reload should include {expected}"
            );
        }
    }

    fn stage_private_follow_up(app: &PiApp) {
        let mut agent = app.agent.try_lock().expect("test agent lock");
        agent.queue_follow_up(ModelMessage::User(UserMessage {
            content: UserContent::Text("old-session follow-up".to_string()),
            timestamp: 0,
        }));
    }

    fn current_session_id(app: &PiApp) -> String {
        app.session
            .try_lock()
            .expect("test session lock")
            .header
            .id
            .clone()
    }

    #[test]
    fn completed_tan_event_keeps_card_and_follow_up_bound_to_the_origin_session() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (mut app, mut event_rx) = build_bash_test_app(session, temp.path());
        let origin_session_id = current_session_id(&app);
        let task_marker = "origin-only-tan-task";
        let output_marker = "origin-only-tan-output";
        let completion = pi::subagents::TanCompletion {
            schema: "pi.background-tan.result.v1",
            hub_id: Some("tan-origin-proof".to_string()),
            task: task_marker.to_string(),
            status: "completed".to_string(),
            output: output_marker.to_string(),
            error: None,
            is_error: false,
        };
        let expected_card = completion.card_text();
        let expected_follow_up = completion.follow_up_text();
        let event = PiApp::completed_tan_event(origin_session_id.clone(), &completion);
        assert!(matches!(
            &event,
            PiMsg::SessionSystemNote {
                owner_session_id,
                message,
            } if owner_session_id == &origin_session_id
                && message == &expected_card
        ));
        assert!(runtime().block_on(async {
            let cx = Cx::for_testing();
            crate::interactive::enqueue_pi_event(&app.event_tx, &cx, event).await
        }));

        let replacement_session = Session::in_memory();
        let replacement_session_id = replacement_session.header.id.clone();
        assert_ne!(replacement_session_id, origin_session_id);
        *app.session.try_lock().expect("replace live session") = replacement_session;
        let messages_before_delivery = app.messages.len();

        let queued_event = runtime().block_on(async {
            let cx = Cx::for_testing();
            asupersync::time::timeout(
                asupersync::time::wall_now(),
                std::time::Duration::from_secs(5),
                event_rx.recv(&cx),
            )
            .await
            .expect("/tan completion event before timeout")
            .expect("/tan completion event")
        });
        let _ = app.handle_pi_message(queued_event);
        assert_eq!(
            app.messages.len(),
            messages_before_delivery,
            "the replacement transcript must discard the origin-bound /tan card"
        );
        assert!(app.messages.iter().all(|message| {
            !message.content.contains(task_marker) && !message.content.contains(output_marker)
        }));

        let origin_notices = pi::jobs::take_completion_notices(&origin_session_id);
        assert_eq!(
            origin_notices.len(),
            1,
            "the production completion helper must queue exactly one origin follow-up"
        );
        let ModelMessage::User(UserMessage {
            content: UserContent::Text(follow_up),
            ..
        }) = &origin_notices[0]
        else {
            panic!("/tan follow-up must be a user message");
        };
        assert_eq!(follow_up, &expected_follow_up);
        assert!(
            pi::jobs::take_completion_notices(&replacement_session_id).is_empty(),
            "the replacement session must not inherit the origin model follow-up"
        );
    }

    fn assert_staged_transition_rejected(app: &PiApp, original_session_id: &str) {
        assert!(matches!(app.agent_state, AgentState::Idle));
        assert_eq!(current_session_id(app), original_session_id);
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|status| status.contains("Queued input is still pending"))
        );
    }

    #[test]
    fn new_session_rejects_staged_old_session_delivery() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (mut app, _event_rx) = build_bash_test_app(session, temp.path());
        let original_session_id = current_session_id(&app);
        stage_private_follow_up(&app);

        let _ = app.handle_slash_command(SlashCommand::New, "");

        assert_staged_transition_rejected(&app, &original_session_id);
        assert_eq!(
            app.agent
                .try_lock()
                .expect("test agent lock")
                .queued_message_count(),
            1,
            "rejected transition must leave the old-session delivery intact"
        );
    }

    #[test]
    fn fork_rejects_staged_old_session_delivery() {
        let temp = TempDir::new().expect("tempdir");
        let mut raw_session = Session::in_memory();
        raw_session.append_model_message(ModelMessage::User(UserMessage {
            content: UserContent::Text("fork source".to_string()),
            timestamp: 0,
        }));
        let session = Arc::new(Mutex::new(raw_session));
        let (mut app, _event_rx) = build_bash_test_app(session, temp.path());
        let original_session_id = current_session_id(&app);
        app.message_queue
            .lock()
            .expect("test user queue lock")
            .push_follow_up(QueuedAgentMessage::generated(ModelMessage::User(
                UserMessage {
                    content: UserContent::Text("queued authored delivery".to_string()),
                    timestamp: 0,
                },
            )));

        let _ = app.handle_slash_fork("");

        assert_staged_transition_rejected(&app, &original_session_id);
        assert_eq!(
            app.message_queue
                .lock()
                .expect("test user queue lock")
                .follow_up_len(),
            1
        );
    }

    #[test]
    fn resume_rejects_staged_old_session_delivery() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (mut app, _event_rx) = build_bash_test_app(session, temp.path());
        let original_session_id = current_session_id(&app);
        app.injected_queue
            .lock()
            .expect("test injected queue lock")
            .push_follow_up(ModelMessage::User(UserMessage {
                content: UserContent::Text("queued extension delivery".to_string()),
                timestamp: 0,
            }));

        let _ = app.load_session_from_path(
            temp.path()
                .join("unused-session.jsonl")
                .to_string_lossy()
                .as_ref(),
        );

        assert_staged_transition_rejected(&app, &original_session_id);
        assert_eq!(
            app.injected_queue
                .lock()
                .expect("test injected queue lock")
                .pending_count(),
            1
        );
    }

    #[test]
    fn new_session_rejects_unconsumed_pending_input() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (mut app, _event_rx) = build_bash_test_app(session, temp.path());
        let original_session_id = current_session_id(&app);
        app.pending_inputs
            .push_back(PendingInput::Text("old-session startup input".to_string()));

        let _ = app.handle_slash_command(SlashCommand::New, "");

        assert_staged_transition_rejected(&app, &original_session_id);
        assert_eq!(app.pending_inputs.len(), 1);
    }

    #[test]
    fn new_session_clears_session_derived_title_and_todo_state() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (mut app, _event_rx) = build_bash_test_app(session, temp.path());
        let original_session_id = current_session_id(&app);
        app.title_requested = true;
        app.todo_summary = Some("1/2 todos complete".to_string());
        app.role_model_overrides.insert(
            crate::models::ModelRole::Smol,
            ("fixture-provider".to_string(), "fixture-model".to_string()),
        );

        let _ = app.handle_slash_command(SlashCommand::New, "");

        let new_session_id = current_session_id(&app);
        assert_ne!(new_session_id, original_session_id);
        assert!(!app.title_requested);
        assert!(app.todo_summary.is_none());
        assert!(app.role_model_overrides.is_empty());
        assert_eq!(
            app.displayed_session_id.as_deref(),
            Some(new_session_id.as_str())
        );
    }

    #[test]
    fn atomic_session_install_does_not_mutate_agent_when_session_is_busy() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (app, _event_rx) = build_bash_test_app(Arc::clone(&session), temp.path());
        let original_session_id = current_session_id(&app);
        app.agent
            .try_lock()
            .expect("test agent lock")
            .add_message(ModelMessage::User(UserMessage {
                content: UserContent::Text("old-agent-history".to_string()),
                timestamp: 0,
            }));
        let held_session = session.try_lock().expect("hold session lock");
        let replacement_message = ModelMessage::User(UserMessage {
            content: UserContent::Text("replacement-history".to_string()),
            timestamp: 0,
        });

        let result = runtime().block_on(PiApp::try_install_session(
            &session,
            &app.agent,
            &app.session_action_admission,
            Session::in_memory(),
            vec![replacement_message],
            None,
        ));

        assert!(result.is_err());
        drop(held_session);
        assert_eq!(current_session_id(&app), original_session_id);
        let agent = app.agent.try_lock().expect("test agent lock");
        assert_eq!(agent.messages().len(), 1);
        assert!(matches!(
            &agent.messages()[0],
            ModelMessage::User(UserMessage {
                content: UserContent::Text(text),
                ..
            }) if text == "old-agent-history"
        ));
    }

    #[test]
    fn resume_marks_processing_before_async_session_load() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (mut app, mut event_rx) = build_bash_test_app(session, temp.path());

        let _ = app.load_session_from_path(
            temp.path()
                .join("missing-session.jsonl")
                .to_string_lossy()
                .as_ref(),
        );
        assert!(matches!(app.agent_state, AgentState::Processing));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let terminal = loop {
            match event_rx.try_recv() {
                Ok(message @ PiMsg::AgentError(_)) => break message,
                Ok(other) => panic!("unexpected resume event: {other:?}"),
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("resume error was not delivered before deadline: {err}"),
            }
        };
        assert!(
            matches!(app.agent_state, AgentState::Processing),
            "background completion must not mutate UI state before its event is handled"
        );
        let _ = app.handle_pi_message(terminal);
        assert!(matches!(app.agent_state, AgentState::Idle));
    }

    #[cfg(unix)]
    #[test]
    fn excluded_bash_production_path_ignores_cancelled_ambient_context_during_cleanup() {
        let temp = TempDir::new().expect("tempdir");
        let blocked_path = temp.path().join("blocked.jsonl");
        std::fs::create_dir(&blocked_path).expect("create directory at session path");

        let mut raw_session = Session::create_with_dir(Some(temp.path().join("sessions")));
        raw_session.path = Some(blocked_path.clone());
        let session = Arc::new(Mutex::new(raw_session));
        let (mut app, mut event_rx) = build_bash_test_app(Arc::clone(&session), temp.path());

        let cancelled_ambient = Cx::for_testing();
        cancelled_ambient.set_cancel_requested(true);
        let _current_cx = Cx::set_current(Some(cancelled_ambient));

        let command = "printf side-effect-output";
        let submitted_at = chrono::Utc::now().timestamp_millis();
        let _ = app.submit_bash_command(&format!("! {command}"), command.to_string(), true);
        assert!(
            app.bash_running,
            "command should be marked running until its result is handled"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let result_message = loop {
            match event_rx.try_recv() {
                Ok(message @ PiMsg::BashResult { .. }) => break message,
                Ok(_) | Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Ok(other) => panic!("unexpected event after bash deadline: {other:?}"),
                Err(err) => panic!("bash result was not delivered before deadline: {err}"),
            }
        };

        let PiMsg::BashResult {
            display,
            content_for_agent,
        } = &result_message
        else {
            unreachable!("loop only exits with BashResult")
        };
        assert!(display.contains("side-effect-output"));
        assert!(display.contains("[Output excluded from model context]"));
        assert!(display.contains("[Persistence warning]"));
        assert!(display.contains("do not rerun"));
        assert!(content_for_agent.is_none());
        let delivered_at = chrono::Utc::now().timestamp_millis();
        assert!(
            !display.contains(blocked_path.to_string_lossy().as_ref()),
            "user-facing result should not leak storage paths"
        );

        let _ = app.handle_pi_message(result_message);
        assert!(
            !app.bash_running,
            "handling the terminal result must clear running state"
        );
        let visible = app.messages.last().expect("visible bash result");
        assert!(visible.content.contains("side-effect-output"));
        assert!(visible.content.contains("[Persistence warning]"));

        runtime().block_on(async {
            let cx = Cx::for_testing();
            let guard = OwnedMutexGuard::lock(Arc::clone(&session), &cx)
                .await
                .expect("lock production session");
            assert_exact_excluded_bash_record(
                &guard,
                command,
                "side-effect-output",
                Some(submitted_at..=delivered_at),
            );
            let metrics = guard.autosave_metrics();
            assert_eq!(metrics.pending_mutations, 1);
            assert_eq!(metrics.flush_failed, 1);
        });
    }

    fn empty_auth_storage() -> AuthStorage {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pi_auth_storage_test_{nonce}.json"));
        AuthStorage::load(path).expect("load empty auth storage")
    }

    fn test_model_entry(provider: &str, id: &str) -> ModelEntry {
        ModelEntry {
            model: Model {
                id: id.to_string(),
                name: id.to_string(),
                api: "openai-responses".to_string(),
                provider: provider.to_string(),
                base_url: "https://example.test/v1".to_string(),
                reasoning: true,
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
    fn plan_session_thinking_sync_repairs_missing_header_when_model_clamps_runtime_level() {
        let mut target = test_model_entry("acme", "plain-model");
        target.model.reasoning = false;

        let plan =
            super::plan_session_thinking_sync(None, crate::model::ThinkingLevel::High, &target);

        assert_eq!(plan.effective, crate::model::ThinkingLevel::Off);
        assert!(plan.thinking_changed);
        assert!(plan.persist_needed);
    }

    #[test]
    fn plan_session_thinking_sync_repairs_invalid_header_without_fake_runtime_change() {
        let mut target = test_model_entry("acme", "plain-model");
        target.model.reasoning = false;

        let plan = super::plan_session_thinking_sync(
            Some("definitely-invalid"),
            crate::model::ThinkingLevel::Off,
            &target,
        );

        assert_eq!(plan.effective, crate::model::ThinkingLevel::Off);
        assert!(!plan.thinking_changed);
        assert!(plan.persist_needed);
    }

    #[test]
    fn parse_ext_cmd_basic() {
        let result = parse_extension_command("/deploy");
        assert_eq!(result, Some(("deploy".to_string(), "")));
    }

    #[test]
    fn parse_ext_cmd_with_args() {
        let result = parse_extension_command("/deploy staging fast");
        assert_eq!(result, Some(("deploy".to_string(), "staging fast")));
    }

    #[test]
    fn parse_ext_cmd_builtin_filtered() {
        assert!(parse_extension_command("/help").is_none());
        assert!(parse_extension_command("/clear").is_none());
        assert!(parse_extension_command("/model").is_none());
        assert!(parse_extension_command("/exit").is_none());
        assert!(parse_extension_command("/compact").is_none());
        assert!(matches!(
            super::SlashCommand::parse("/undo 3 force"),
            Some((super::SlashCommand::Undo, "3 force"))
        ));
        assert!(matches!(
            super::SlashCommand::parse("/redo"),
            Some((super::SlashCommand::Redo, ""))
        ));
        assert!(matches!(
            super::SlashCommand::parse("/tan update the changelog"),
            Some((super::SlashCommand::Tan, "update the changelog"))
        ));
        assert!(parse_extension_command("/tan inspect this").is_none());
    }

    #[test]
    fn parse_ext_cmd_no_slash() {
        assert!(parse_extension_command("deploy").is_none());
        assert!(parse_extension_command("hello world").is_none());
    }

    #[test]
    fn parse_ext_cmd_empty_slash() {
        assert!(parse_extension_command("/").is_none());
        assert!(parse_extension_command("/  ").is_none());
    }

    #[test]
    fn parse_ext_cmd_whitespace_trimming() {
        let result = parse_extension_command("  /deploy  arg1  arg2  ");
        assert_eq!(result, Some(("deploy".to_string(), "arg1  arg2")));
    }

    #[test]
    fn parse_ext_cmd_single_arg() {
        let result = parse_extension_command("/greet world");
        assert_eq!(result, Some(("greet".to_string(), "world")));
    }

    #[test]
    fn parse_ext_cmd_preserves_raw_argument_spacing_and_quotes() {
        let result = parse_extension_command(r#"/deploy   --message "hello world"   --force"#);
        assert_eq!(
            result,
            Some(("deploy".to_string(), r#"--message "hello world"   --force"#))
        );
    }

    #[test]
    fn parse_bash_command_distinguishes_exclusion() {
        let (command, exclude) = parse_bash_command("! ls -la").expect("bang command");
        assert_eq!(command, "ls -la");
        assert!(!exclude);

        let (command, exclude) = parse_bash_command("!! ls -la").expect("double bang command");
        assert_eq!(command, "ls -la");
        assert!(exclude);
    }

    #[test]
    fn parse_bash_command_empty_bang() {
        assert!(parse_bash_command("!").is_none());
        assert!(parse_bash_command("!!").is_none());
        assert!(parse_bash_command("!  ").is_none());
    }

    #[test]
    fn parse_bash_command_no_bang() {
        assert!(parse_bash_command("ls -la").is_none());
        assert!(parse_bash_command("").is_none());
    }

    #[test]
    fn parse_bash_command_leading_whitespace() {
        let (cmd, exclude) = parse_bash_command("  ! echo hi").expect("should parse");
        assert_eq!(cmd, "echo hi");
        assert!(!exclude);
    }

    #[test]
    fn startup_hint_is_hidden_when_priority_provider_is_available() {
        let mut auth = empty_auth_storage();
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "test-key".to_string(),
            },
        );
        assert!(!should_show_startup_oauth_hint(&auth));
    }

    #[test]
    fn startup_hint_is_hidden_when_non_oauth_provider_is_available() {
        let mut auth = empty_auth_storage();
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "test-openai-key".to_string(),
            },
        );
        assert!(!should_show_startup_oauth_hint(&auth));
    }

    #[test]
    fn startup_hint_copy_no_longer_uses_front_and_center_phrase() {
        let auth = empty_auth_storage();
        let hint = super::format_startup_oauth_hint(&auth);
        assert!(hint.contains("No provider credentials were detected."));
        assert!(!hint.contains("front and center"));
    }

    #[test]
    fn builtin_login_providers_cover_legacy_oauth_registry() {
        let login_oauth: HashSet<&str> = super::BUILTIN_LOGIN_PROVIDERS
            .iter()
            .filter_map(|(provider, mode)| (*mode == "OAuth").then_some(*provider))
            .collect();

        // Legacy pi-mono OAuth provider registry (packages/ai/src/utils/oauth/index.ts)
        // includes exactly these built-ins.
        let legacy_oauth = [
            "anthropic",
            "openai-codex",
            "google-gemini-cli",
            "google-antigravity",
            "github-copilot",
        ];

        let missing: Vec<&str> = legacy_oauth
            .iter()
            .copied()
            .filter(|provider| !login_oauth.contains(provider))
            .collect();

        assert!(
            missing.is_empty(),
            "missing legacy OAuth providers in /login table: {}",
            missing.join(", ")
        );

        assert!(
            login_oauth.contains("kimi-for-coding"),
            "kimi-for-coding should remain available in /login OAuth providers"
        );
    }

    #[test]
    fn metadata_backed_api_key_prompt_supports_openai_compatible_presets() {
        let prompt = super::api_key_login_prompt("openrouter").expect("openrouter prompt");
        assert!(prompt.contains("API key login: openrouter"));
        assert!(prompt.contains("OpenRouter"));
        assert!(prompt.contains("https://openrouter.ai/api/v1"));
        assert!(prompt.contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn dedicated_login_flows_still_take_priority_over_generic_api_key_prompts() {
        assert!(super::api_key_login_prompt("anthropic").is_none());
        assert!(super::api_key_login_prompt("kimi-for-coding").is_none());
    }

    #[test]
    fn login_provider_listing_includes_metadata_backed_api_key_providers() {
        let auth = empty_auth_storage();
        let listing = super::format_login_provider_listing(&auth, &[], &[]);
        assert!(listing.contains("openrouter"));
        assert!(listing.contains("cohere"));
        assert!(listing.contains("API key"));
    }

    #[test]
    fn model_entry_matches_provider_aliases_case_insensitively() {
        let left = test_model_entry("openrouter", "openai/gpt-4o-mini");
        let right = test_model_entry("open-router", "openai/gpt-4o-mini");
        assert!(super::model_entry_matches(&left, &right));
    }

    #[test]
    fn provider_ids_match_normalizes_aliases() {
        assert!(super::provider_ids_match("openrouter", "open-router"));
        assert!(super::provider_ids_match("google-gemini-cli", "gemini-cli"));
        assert!(super::provider_ids_match("kimi-for-coding", "kimi-code"));
        assert!(!super::provider_ids_match("openai", "anthropic"));
    }

    #[test]
    fn normalize_auth_provider_input_maps_kimi_code_alias() {
        assert_eq!(
            super::normalize_auth_provider_input("kimi-code"),
            "kimi-for-coding"
        );
    }

    #[test]
    fn resolve_scoped_model_entries_dedupes_provider_alias_variants() {
        let available = vec![
            test_model_entry("openrouter", "openai/gpt-4o-mini"),
            test_model_entry("open-router", "openai/gpt-4o-mini"),
        ];
        let patterns = vec!["openrouter/openai/gpt-4o-mini".to_string()];
        let resolved = super::resolve_scoped_model_entries(&patterns, &available)
            .expect("resolve scoped models");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].model.id, "openai/gpt-4o-mini");
    }

    #[test]
    fn save_provider_credential_canonicalizes_alias_input() {
        let mut auth = empty_auth_storage();
        super::save_provider_credential(
            &mut auth,
            "gemini",
            AuthCredential::ApiKey {
                key: "new-google-token".to_string(),
            },
        );

        assert!(auth.get("gemini").is_none());
        assert!(matches!(
            auth.get("google"),
            Some(AuthCredential::ApiKey { key }) if key == "new-google-token"
        ));
    }

    #[test]
    fn resolve_model_key_with_auth_prefers_stored_key_over_inline_key() {
        let mut auth = empty_auth_storage();
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "stored-auth-sample".to_string(),
            },
        );

        let mut entry = test_model_entry("openai", "gpt-4o-mini");
        entry.api_key = Some("inline-model-sample".to_string());

        assert_eq!(
            super::resolve_model_key_with_auth(&auth, &entry).as_deref(),
            Some("stored-auth-sample")
        );
    }

    #[test]
    fn resolve_model_key_with_auth_falls_back_to_inline_key() {
        let auth = empty_auth_storage();
        let mut entry = test_model_entry("openai", "gpt-4o-mini");
        entry.api_key = Some("inline-model-sample".to_string());

        assert_eq!(
            super::resolve_model_key_with_auth(&auth, &entry).as_deref(),
            Some("inline-model-sample")
        );
    }

    #[test]
    fn remove_provider_credentials_removes_alias_entries() {
        let mut auth = empty_auth_storage();
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "google-key".to_string(),
            },
        );
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: "gemini-key".to_string(),
            },
        );

        assert!(super::remove_provider_credentials(&mut auth, "gemini"));
        assert!(auth.get("google").is_none());
        assert!(auth.get("gemini").is_none());
    }

    #[test]
    fn extension_oauth_config_selection_skips_non_oauth_entries() {
        let mut no_oauth = test_model_entry("ext-provider", "model-a");
        no_oauth.oauth_config = None;
        let mut with_oauth = test_model_entry("ext-provider", "model-b");
        with_oauth.oauth_config = Some(crate::models::OAuthConfig {
            auth_url: "https://example.test/oauth/authorize".to_string(),
            token_url: "https://example.test/oauth/token".to_string(),
            scopes: vec!["scope:a".to_string()],
            client_id: "client-id".to_string(),
            redirect_uri: Some("http://localhost/callback".to_string()),
        });

        let registered_extension_bindings = [ExtensionProviderBinding {
            provider: with_oauth.model.provider.clone(),
            oauth_config: with_oauth.oauth_config,
        }];
        let selected = super::extension_oauth_config_for_provider(
            &[no_oauth],
            &registered_extension_bindings,
            "ext-provider",
        );
        let selected = selected.expect("expected oauth config");
        assert_eq!(selected.auth_url, "https://example.test/oauth/authorize");
        assert_eq!(selected.token_url, "https://example.test/oauth/token");
        assert_eq!(selected.client_id, "client-id");
        assert_eq!(selected.scopes, vec!["scope:a".to_string()]);
        assert_eq!(
            selected.redirect_uri.as_deref(),
            Some("http://localhost/callback")
        );

        let auth = empty_auth_storage();
        let listing =
            super::format_login_provider_listing(&auth, &[], &registered_extension_bindings);
        assert!(listing.contains("Extension providers"));
        assert!(listing.contains("ext-provider"));
        assert!(listing.contains("OAuth"));
    }

    #[test]
    fn zero_model_extension_oauth_is_reachable_through_actual_login_command() {
        let temp = TempDir::new().expect("tempdir");
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let (mut app, _event_rx) = build_bash_test_app(session, temp.path());
        let manager = ExtensionManager::new();
        manager.register_provider(serde_json::json!({
            "id": "Acme",
            "models": [],
            "hasStreamSimple": false,
            "oauth": {
                "authUrl": "https://auth.example.test/authorize",
                "tokenUrl": "https://auth.example.test/token",
                "clientId": "zero-model-client",
                "scopes": ["models:use"]
            }
        }));
        app.extensions = Some(manager);
        app.available_models.clear();

        let invalid_auth_path = temp.path().join("auth-as-directory");
        std::fs::create_dir_all(&invalid_auth_path).expect("create invalid auth-path directory");
        let messages_before_invalid_path = app.messages.len();
        assert!(
            app.handle_slash_login_with_auth_path("", &invalid_auth_path)
                .is_none()
        );
        assert_eq!(app.messages.len(), messages_before_invalid_path);
        assert!(
            app.status_message
                .as_deref()
                .is_some_and(|message| message.starts_with("Unable to load auth status:"))
        );

        app.status_message = None;
        let auth_path = temp.path().join("auth.json");
        assert!(
            app.handle_slash_login_with_auth_path("", &auth_path)
                .is_none()
        );
        let listing = &app.messages.last().expect("login listing message").content;
        assert!(listing.contains("Extension providers"));
        assert!(listing.contains("Acme"));
        assert!(listing.contains("OAuth"));

        assert!(
            app.handle_slash_login_with_auth_path("acme", &auth_path)
                .is_none()
        );
        let pending = app
            .pending_oauth
            .as_ref()
            .expect("extension OAuth flow should be pending");
        assert_eq!(pending.provider, "acme");
        assert!(matches!(pending.kind, PendingLoginKind::OAuth));
        let oauth = pending
            .oauth_config
            .as_ref()
            .expect("extension OAuth metadata should reach the command");
        assert_eq!(oauth.client_id, "zero-model-client");
        assert_eq!(oauth.token_url, "https://auth.example.test/token");
    }
}
