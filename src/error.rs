//! Error types for the Pi application.

use crate::provider_metadata::{canonical_provider_id, provider_auth_env_keys};
use std::sync::OnceLock;
use thiserror::Error;

/// Result type alias using our error type.
pub type Result<T> = std::result::Result<T, Error>;

/// Main error type for the Pi application.
#[derive(Error, Debug)]
pub enum Error {
    /// Configuration errors
    #[error("Configuration error: {0}")]
    Config(String),

    /// Session errors
    #[error("Session error: {0}")]
    Session(String),

    /// Session not found
    #[error("Session not found: {path}")]
    SessionNotFound { path: String },

    /// Provider/API errors
    #[error("Provider error: {provider}: {message}")]
    Provider { provider: String, message: String },

    /// Authentication errors
    #[error("Authentication error: {0}")]
    Auth(String),

    /// Tool execution errors
    #[error("Tool error: {tool}: {message}")]
    Tool { tool: String, message: String },

    /// Validation errors
    #[error("Validation error: {0}")]
    Validation(String),

    /// Extension errors
    #[error("Extension error: {0}")]
    Extension(String),

    /// IO errors
    #[error("IO error: {0}")]
    Io(#[from] Box<std::io::Error>),

    /// JSON errors
    #[error("JSON error: {0}")]
    Json(#[from] Box<serde_json::Error>),

    /// SQLite errors
    #[error("SQLite error: {0}")]
    Sqlite(#[from] Box<sqlmodel_core::Error>),

    /// User aborted operation
    #[error("Operation aborted")]
    Aborted,

    /// API errors (generic)
    #[error("API error: {0}")]
    Api(String),
}

/// Stable machine codes for auth/config diagnostics across provider families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDiagnosticCode {
    MissingApiKey,
    InvalidApiKey,
    QuotaExceeded,
    MissingOAuthAuthorizationCode,
    OAuthTokenExchangeFailed,
    OAuthTokenRefreshFailed,
    MissingAzureDeployment,
    MissingRegion,
    MissingProject,
    MissingProfile,
    MissingEndpoint,
    MissingCredentialChain,
    UnknownAuthFailure,
}

impl AuthDiagnosticCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingApiKey => "auth.missing_api_key",
            Self::InvalidApiKey => "auth.invalid_api_key",
            Self::QuotaExceeded => "auth.quota_exceeded",
            Self::MissingOAuthAuthorizationCode => "auth.oauth.missing_authorization_code",
            Self::OAuthTokenExchangeFailed => "auth.oauth.token_exchange_failed",
            Self::OAuthTokenRefreshFailed => "auth.oauth.token_refresh_failed",
            Self::MissingAzureDeployment => "config.azure.missing_deployment",
            Self::MissingRegion => "config.auth.missing_region",
            Self::MissingProject => "config.auth.missing_project",
            Self::MissingProfile => "config.auth.missing_profile",
            Self::MissingEndpoint => "config.auth.missing_endpoint",
            Self::MissingCredentialChain => "auth.credential_chain.missing",
            Self::UnknownAuthFailure => "auth.unknown_failure",
        }
    }

    #[must_use]
    pub const fn remediation(self) -> &'static str {
        match self {
            Self::MissingApiKey => "Set the provider API key env var or run `/login <provider>`.",
            Self::InvalidApiKey => "Rotate or replace the API key and verify provider permissions.",
            Self::QuotaExceeded => {
                "Verify billing/quota limits for this API key or organization, then retry."
            }
            Self::MissingOAuthAuthorizationCode => {
                "Re-run `/login` and paste a full callback URL or authorization code."
            }
            Self::OAuthTokenExchangeFailed => {
                "Retry login flow and verify token endpoint/client configuration."
            }
            Self::OAuthTokenRefreshFailed => {
                "Re-authenticate with `/login` and confirm refresh-token validity."
            }
            Self::MissingAzureDeployment => {
                "Configure Azure resource+deployment in models.json before dispatch."
            }
            Self::MissingRegion => "Set provider region/cluster configuration before retrying.",
            Self::MissingProject => "Set provider project/workspace identifier before retrying.",
            Self::MissingProfile => "Set credential profile/source configuration before retrying.",
            Self::MissingEndpoint => "Configure provider base URL/endpoint in models.json.",
            Self::MissingCredentialChain => {
                "Configure credential-chain sources (env/profile/role) before retrying."
            }
            Self::UnknownAuthFailure => {
                "Inspect auth diagnostics and retry with explicit credentials."
            }
        }
    }

    #[must_use]
    pub const fn redaction_policy(self) -> &'static str {
        match self {
            Self::MissingApiKey
            | Self::InvalidApiKey
            | Self::QuotaExceeded
            | Self::MissingOAuthAuthorizationCode
            | Self::OAuthTokenExchangeFailed
            | Self::OAuthTokenRefreshFailed
            | Self::MissingAzureDeployment
            | Self::MissingRegion
            | Self::MissingProject
            | Self::MissingProfile
            | Self::MissingEndpoint
            | Self::MissingCredentialChain
            | Self::UnknownAuthFailure => "redact-secrets",
        }
    }
}

/// Structured auth/config diagnostic metadata for downstream tooling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthDiagnostic {
    pub code: AuthDiagnosticCode,
    pub remediation: &'static str,
    pub redaction_policy: &'static str,
}

impl Error {
    /// Create a configuration error.
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// Create a session error.
    pub fn session(message: impl Into<String>) -> Self {
        Self::Session(message.into())
    }

    /// Create a provider error.
    pub fn provider(provider: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Provider {
            provider: provider.into(),
            message: message.into(),
        }
    }

    /// Create an authentication error.
    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth(message.into())
    }

    /// Create a tool error.
    pub fn tool(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            tool: tool.into(),
            message: message.into(),
        }
    }

    /// Create a validation error.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    /// Create an extension error.
    pub fn extension(message: impl Into<String>) -> Self {
        Self::Extension(message.into())
    }

    /// Create an API error.
    pub fn api(message: impl Into<String>) -> Self {
        Self::Api(message.into())
    }

    /// Build the user-facing error for an I/O failure that terminated a
    /// streaming (SSE) response read.
    ///
    /// Retryability is decided here — at the source — from the *typed*
    /// [`std::io::ErrorKind`], because this is the last place the kind is
    /// available: the caller immediately flattens the returned error into a
    /// message string (`AssistantMessage::error_message`) that the retry loop
    /// consults. For a transient connection drop (reset/abort/EOF/broken
    /// pipe/timeout) a canonical `transient connection drop` marker is appended
    /// so the drop is re-driven regardless of the dependency's (rustls/hyper)
    /// prose, which changes phrasing between versions (pi_agent_rust#118).
    pub fn sse(err: &std::io::Error) -> Self {
        let base = format!("SSE error: {err}");
        if io_kind_is_transient(err.kind()) {
            Self::Api(format!("{base} (transient connection drop)"))
        } else {
            Self::Api(base)
        }
    }

    /// Whether this error is a transient transport failure that is safe to
    /// retry, classified from the *typed* error rather than its flattened
    /// message string.
    ///
    /// Walks the [`std::error::Error::source`] chain (and, for
    /// [`std::io::Error`] links, the [`std::io::Error::get_ref`] chain, which
    /// `source` deliberately skips) looking for an [`std::io::Error`] whose
    /// [`std::io::ErrorKind`] denotes a connection drop. This catches typed
    /// I/O failures (e.g. an `UnexpectedEof` mid-body that propagates as
    /// [`Error::Io`]) without depending on any substring match. Message-only
    /// transient cases (dependency prose with no typed cause) are handled by
    /// [`is_retryable_error`] as a documented fallback.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        // Direct io::Error link (get_ref walk covers io-wrapping-io).
        if let Self::Io(io_err) = self
            && io_error_chain_is_transient(io_err)
        {
            return true;
        }
        // Generic source chain: any link that is (or wraps) a transient
        // io::Error makes this retryable.
        let mut source = std::error::Error::source(self);
        while let Some(cause) = source {
            if let Some(io_err) = cause.downcast_ref::<std::io::Error>()
                && io_error_chain_is_transient(io_err)
            {
                return true;
            }
            source = cause.source();
        }
        false
    }

    /// Map this error to a hostcall taxonomy code.
    ///
    /// The hostcall ABI requires every error to be one of:
    /// `timeout`, `denied`, `io`, `invalid_request`, or `internal`.
    pub const fn hostcall_error_code(&self) -> &'static str {
        match self {
            Self::Validation(_) => "invalid_request",
            Self::Io(_) | Self::Session(_) | Self::SessionNotFound { .. } | Self::Sqlite(_) => "io",
            Self::Auth(_) => "denied",
            Self::Aborted => "timeout",
            Self::Json(_)
            | Self::Extension(_)
            | Self::Config(_)
            | Self::Provider { .. }
            | Self::Tool { .. }
            | Self::Api(_) => "internal",
        }
    }

    /// Stable machine-readable error category for automation and diagnostics.
    #[must_use]
    pub const fn category_code(&self) -> &'static str {
        match self {
            Self::Config(_) => "config",
            Self::Session(_) | Self::SessionNotFound { .. } => "session",
            Self::Provider { .. } => "provider",
            Self::Auth(_) => "auth",
            Self::Tool { .. } => "tool",
            Self::Validation(_) => "validation",
            Self::Extension(_) => "extension",
            Self::Io(_) => "io",
            Self::Json(_) => "json",
            Self::Sqlite(_) => "sqlite",
            Self::Aborted => "runtime",
            Self::Api(_) => "api",
        }
    }

    /// Classify auth/config errors into stable machine-readable diagnostics.
    #[must_use]
    pub fn auth_diagnostic(&self) -> Option<AuthDiagnostic> {
        match self {
            Self::Auth(message) => classify_auth_diagnostic(None, message),
            Self::Provider { provider, message } => {
                classify_auth_diagnostic(Some(provider.as_str()), message)
            }
            _ => None,
        }
    }

    /// Map internal errors to a stable, user-facing hint taxonomy.
    #[must_use]
    pub fn hints(&self) -> ErrorHints {
        let mut hints = match self {
            Self::Config(message) => config_hints(message),
            Self::Session(message) => session_hints(message),
            Self::SessionNotFound { path } => build_hints(
                "Session file not found.",
                vec![
                    "Use `pi --continue` to open the most recent session.".to_string(),
                    "Verify the path or move the session back into the sessions directory."
                        .to_string(),
                ],
                vec![("path", path.clone())],
            ),
            Self::Provider { provider, message } => provider_hints(provider, message),
            Self::Auth(message) => auth_hints(message),
            Self::Tool { tool, message } => tool_hints(tool, message),
            Self::Validation(message) => build_hints(
                "Validation failed for input or config.",
                vec![
                    "Check the specific fields mentioned in the error.".to_string(),
                    "Review CLI flags or settings for typos.".to_string(),
                ],
                vec![("details", message.clone())],
            ),
            Self::Extension(message) => build_hints(
                "Extension failed to load or run.",
                vec![
                    "Try `--no-extensions` to isolate the issue.".to_string(),
                    "Check the extension manifest and dependencies.".to_string(),
                ],
                vec![("details", message.clone())],
            ),
            Self::Io(err) => io_hints(err),
            Self::Json(err) => build_hints(
                "JSON parsing failed.",
                vec![
                    "Validate the JSON syntax (no trailing commas).".to_string(),
                    "Check that the file is UTF-8 and not truncated.".to_string(),
                ],
                vec![("details", err.to_string())],
            ),
            Self::Sqlite(err) => sqlite_hints(err),
            Self::Aborted => build_hints(
                "Operation aborted.",
                Vec::new(),
                vec![(
                    "details",
                    "Operation cancelled by user or runtime.".to_string(),
                )],
            ),
            Self::Api(message) => build_hints(
                "API request failed.",
                vec![
                    "Check your network connection and retry.".to_string(),
                    "Verify your API key and provider selection.".to_string(),
                ],
                vec![("details", message.clone())],
            ),
        };

        hints.context.push((
            "error_category".to_string(),
            self.category_code().to_string(),
        ));

        if let Some(diagnostic) = self.auth_diagnostic() {
            hints.context.push((
                "diagnostic_code".to_string(),
                diagnostic.code.as_str().to_string(),
            ));
            hints.context.push((
                "diagnostic_remediation".to_string(),
                diagnostic.remediation.to_string(),
            ));
            hints.context.push((
                "redaction_policy".to_string(),
                diagnostic.redaction_policy.to_string(),
            ));
        }

        hints
    }
}

/// Structured hints for error remediation.
#[derive(Debug, Clone)]
pub struct ErrorHints {
    /// Brief summary of the error category.
    pub summary: String,
    /// Actionable hints for the user.
    pub hints: Vec<String>,
    /// Key-value context pairs for display.
    pub context: Vec<(String, String)>,
}

fn build_hints(summary: &str, hints: Vec<String>, context: Vec<(&str, String)>) -> ErrorHints {
    ErrorHints {
        summary: summary.to_string(),
        hints,
        context: context
            .into_iter()
            .map(|(label, value)| (label.to_string(), value))
            .collect(),
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

const fn build_auth_diagnostic(code: AuthDiagnosticCode) -> AuthDiagnostic {
    AuthDiagnostic {
        code,
        remediation: code.remediation(),
        redaction_policy: code.redaction_policy(),
    }
}

#[allow(clippy::too_many_lines)]
fn classify_auth_diagnostic(provider: Option<&str>, message: &str) -> Option<AuthDiagnostic> {
    let lower = message.to_lowercase();
    let provider_lower = provider.map(str::to_lowercase);
    if contains_any(
        &lower,
        &[
            "missing authorization code",
            "authorization code is missing",
        ],
    ) {
        return Some(build_auth_diagnostic(
            AuthDiagnosticCode::MissingOAuthAuthorizationCode,
        ));
    }
    if contains_any(&lower, &["token exchange failed", "invalid token response"]) {
        return Some(build_auth_diagnostic(
            AuthDiagnosticCode::OAuthTokenExchangeFailed,
        ));
    }
    if contains_any(
        &lower,
        &[
            "token refresh failed",
            "oauth token refresh failed",
            "refresh token",
        ],
    ) {
        return Some(build_auth_diagnostic(
            AuthDiagnosticCode::OAuthTokenRefreshFailed,
        ));
    }
    if contains_any(
        &lower,
        &[
            "missing api key",
            "api key not configured",
            "api key is required",
            "you didn't provide an api key",
            "no api key provided",
            "missing bearer",
            "authorization header missing",
        ],
    ) {
        return Some(build_auth_diagnostic(AuthDiagnosticCode::MissingApiKey));
    }
    if contains_any(
        &lower,
        &[
            "insufficient_quota",
            "quota exceeded",
            "quota has been exceeded",
            "billing hard limit",
            "billing_not_active",
            "not enough credits",
            "credit balance is too low",
        ],
    ) {
        return Some(build_auth_diagnostic(AuthDiagnosticCode::QuotaExceeded));
    }
    if contains_any(
        &lower,
        &[
            "401",
            "unauthorized",
            "403",
            "forbidden",
            "invalid api key",
            "incorrect api key",
            "malformed api key",
            "api key is malformed",
            "revoked",
            "deactivated",
            "disabled api key",
            "expired api key",
        ],
    ) {
        return Some(build_auth_diagnostic(AuthDiagnosticCode::InvalidApiKey));
    }
    if contains_any(&lower, &["resource+deployment", "missing deployment"]) {
        return Some(build_auth_diagnostic(
            AuthDiagnosticCode::MissingAzureDeployment,
        ));
    }
    if contains_any(&lower, &["missing region", "region is required"]) {
        return Some(build_auth_diagnostic(AuthDiagnosticCode::MissingRegion));
    }
    if contains_any(&lower, &["missing project", "project is required"]) {
        return Some(build_auth_diagnostic(AuthDiagnosticCode::MissingProject));
    }
    if contains_any(&lower, &["missing profile", "profile is required"]) {
        return Some(build_auth_diagnostic(AuthDiagnosticCode::MissingProfile));
    }
    if contains_any(
        &lower,
        &[
            "missing endpoint",
            "missing base url",
            "base url is required",
        ],
    ) {
        return Some(build_auth_diagnostic(AuthDiagnosticCode::MissingEndpoint));
    }
    if contains_any(
        &lower,
        &[
            "credential chain",
            "aws_access_key_id",
            "credential source",
            "missing credentials",
        ],
    ) || provider_lower
        .as_deref()
        .is_some_and(|provider_id| provider_id.contains("bedrock") && lower.contains("credential"))
    {
        return Some(build_auth_diagnostic(
            AuthDiagnosticCode::MissingCredentialChain,
        ));
    }

    if lower.contains("oauth")
        || lower.contains("authentication")
        || lower.contains("credential")
        || lower.contains("api key")
    {
        return Some(build_auth_diagnostic(
            AuthDiagnosticCode::UnknownAuthFailure,
        ));
    }

    None
}

fn config_hints(message: &str) -> ErrorHints {
    let lower = message.to_lowercase();
    if contains_any(&lower, &["json", "parse", "serde"]) {
        return build_hints(
            "Configuration file is not valid JSON.",
            vec![
                "Fix JSON formatting in the active settings file.".to_string(),
                "Run `pi config` to see which settings file is in use.".to_string(),
            ],
            vec![("details", message.to_string())],
        );
    }
    if contains_any(&lower, &["missing", "not found", "no such file"]) {
        return build_hints(
            "Configuration file is missing.",
            vec![
                "Create `~/.pi/agent/settings.json` or set `PI_CONFIG_PATH`.".to_string(),
                "Run `pi config` to confirm the resolved path.".to_string(),
            ],
            vec![("details", message.to_string())],
        );
    }
    build_hints(
        "Configuration error.",
        vec![
            "Review your settings file for incorrect values.".to_string(),
            "Run `pi config` to verify settings precedence.".to_string(),
        ],
        vec![("details", message.to_string())],
    )
}

fn session_hints(message: &str) -> ErrorHints {
    let lower = message.to_lowercase();
    if contains_any(&lower, &["empty session file", "empty session"]) {
        return build_hints(
            "Session file is empty or corrupted.",
            vec![
                "Start a new session with `pi --no-session`.".to_string(),
                "Inspect the session file for truncation.".to_string(),
            ],
            vec![("details", message.to_string())],
        );
    }
    if contains_any(&lower, &["failed to read", "read dir", "read session"]) {
        return build_hints(
            "Failed to read session data.",
            vec![
                "Check file permissions for the sessions directory.".to_string(),
                "Verify `PI_SESSIONS_DIR` if you set it.".to_string(),
            ],
            vec![("details", message.to_string())],
        );
    }
    build_hints(
        "Session error.",
        vec![
            "Try `pi --continue` or specify `--session <path>`.".to_string(),
            "Check session file integrity in the sessions directory.".to_string(),
        ],
        vec![("details", message.to_string())],
    )
}

#[allow(clippy::too_many_lines)]
fn provider_hints(provider: &str, message: &str) -> ErrorHints {
    let lower = message.to_lowercase();
    let key_hint = provider_key_hint(provider);
    let context = vec![
        ("provider", provider.to_string()),
        ("details", message.to_string()),
    ];

    if contains_any(
        &lower,
        &[
            "missing api key",
            "you didn't provide an api key",
            "no api key provided",
            "authorization header missing",
        ],
    ) {
        return build_hints(
            "Provider API key is missing.",
            vec![
                key_hint,
                "Set the API key and retry the request.".to_string(),
            ],
            context,
        );
    }
    if contains_any(
        &lower,
        &["401", "unauthorized", "invalid api key", "api key"],
    ) {
        return build_hints(
            "Provider authentication failed.",
            vec![key_hint, "If using OAuth, run `/login` again.".to_string()],
            context,
        );
    }
    if contains_any(&lower, &["403", "forbidden"]) {
        return build_hints(
            "Provider access forbidden.",
            vec![
                "Verify the account has access to the requested model.".to_string(),
                "Check organization/project permissions for the API key.".to_string(),
            ],
            context,
        );
    }
    if contains_any(
        &lower,
        &[
            "insufficient_quota",
            "quota exceeded",
            "quota has been exceeded",
            "billing hard limit",
            "billing_not_active",
            "not enough credits",
            "credit balance is too low",
        ],
    ) {
        return build_hints(
            "Provider quota or billing limit reached.",
            vec![
                "Verify billing/credits and organization quota for this API key.".to_string(),
                key_hint,
            ],
            context,
        );
    }
    if contains_any(&lower, &["429", "rate limit", "too many requests"]) {
        return build_hints(
            "Provider rate limited the request.",
            vec![
                "Wait and retry, or reduce request rate.".to_string(),
                "Consider smaller max_tokens to lower load.".to_string(),
            ],
            context,
        );
    }
    if contains_any(&lower, &["529", "overloaded"]) {
        return build_hints(
            "Provider is overloaded.",
            vec![
                "Retry after a short delay.".to_string(),
                "Switch to a different model if available.".to_string(),
            ],
            context,
        );
    }
    if contains_any(&lower, &["timeout", "timed out"]) {
        return build_hints(
            "Provider request timed out.",
            vec![
                "Check network stability and retry.".to_string(),
                "Lower max_tokens to shorten responses.".to_string(),
            ],
            context,
        );
    }
    if contains_any(&lower, &["400", "bad request", "invalid request"]) {
        return build_hints(
            "Provider rejected the request.",
            vec![
                "Check model name, tools schema, and request size.".to_string(),
                "Reduce message size or tool payloads.".to_string(),
            ],
            context,
        );
    }
    if contains_any(&lower, &["500", "internal server error", "server error"]) {
        return build_hints(
            "Provider encountered a server error.",
            vec![
                "Retry after a short delay.".to_string(),
                "If persistent, try a different model/provider.".to_string(),
            ],
            context,
        );
    }
    build_hints(
        "Provider request failed.",
        vec![
            key_hint,
            "Check network connectivity and provider status.".to_string(),
        ],
        context,
    )
}

fn provider_key_hint(provider: &str) -> String {
    let canonical = canonical_provider_id(provider).unwrap_or(provider);
    let env_keys = provider_auth_env_keys(provider);
    if !env_keys.is_empty() {
        let key_list = env_keys
            .iter()
            .map(|key| format!("`{key}`"))
            .collect::<Vec<_>>()
            .join(" or ");
        if canonical == "anthropic" {
            return format!("Set {key_list} (or use `/login anthropic`).");
        }
        if canonical == "github-copilot" {
            return format!("Set {key_list} (or use `/login github-copilot`).");
        }
        return format!("Set {key_list} for provider `{canonical}`.");
    }

    format!("Check API key configuration for provider `{provider}`.")
}

fn auth_hints(message: &str) -> ErrorHints {
    let lower = message.to_lowercase();
    if contains_any(
        &lower,
        &["missing authorization code", "authorization code"],
    ) {
        return build_hints(
            "OAuth login did not complete.",
            vec![
                "Run `/login` again to restart the flow.".to_string(),
                "Ensure the browser redirect URL was opened.".to_string(),
            ],
            vec![("details", message.to_string())],
        );
    }
    if contains_any(&lower, &["token exchange failed", "invalid token response"]) {
        return build_hints(
            "OAuth token exchange failed.",
            vec![
                "Retry `/login` to refresh credentials.".to_string(),
                "Check network connectivity during the login flow.".to_string(),
            ],
            vec![("details", message.to_string())],
        );
    }
    build_hints(
        "Authentication error.",
        vec![
            "Verify API keys or run `/login`.".to_string(),
            "Check auth.json permissions in the Pi config directory.".to_string(),
        ],
        vec![("details", message.to_string())],
    )
}

fn tool_hints(tool: &str, message: &str) -> ErrorHints {
    let lower = message.to_lowercase();
    if contains_any(&lower, &["not found", "no such file", "command not found"]) {
        return build_hints(
            "Tool executable or target not found.",
            vec![
                "Check PATH and tool installation.".to_string(),
                "Verify the tool input path exists.".to_string(),
            ],
            vec![("tool", tool.to_string()), ("details", message.to_string())],
        );
    }
    build_hints(
        "Tool execution failed.",
        vec![
            "Check the tool output for details.".to_string(),
            "Re-run with simpler inputs to isolate the failure.".to_string(),
        ],
        vec![("tool", tool.to_string()), ("details", message.to_string())],
    )
}

fn io_hints(err: &std::io::Error) -> ErrorHints {
    let details = err.to_string();
    match err.kind() {
        std::io::ErrorKind::NotFound => build_hints(
            "Required file or directory not found.",
            vec![
                "Verify the path exists and is spelled correctly.".to_string(),
                "Check `PI_CONFIG_PATH` or `PI_SESSIONS_DIR` overrides.".to_string(),
            ],
            vec![
                ("error_kind", format!("{:?}", err.kind())),
                ("details", details),
            ],
        ),
        std::io::ErrorKind::PermissionDenied => build_hints(
            "Permission denied while accessing a file.",
            vec![
                "Check file permissions or ownership.".to_string(),
                "Try a different location with write access.".to_string(),
            ],
            vec![
                ("error_kind", format!("{:?}", err.kind())),
                ("details", details),
            ],
        ),
        std::io::ErrorKind::TimedOut => build_hints(
            "I/O operation timed out.",
            vec![
                "Check network or filesystem latency.".to_string(),
                "Retry after confirming connectivity.".to_string(),
            ],
            vec![
                ("error_kind", format!("{:?}", err.kind())),
                ("details", details),
            ],
        ),
        std::io::ErrorKind::ConnectionRefused => build_hints(
            "Connection refused.",
            vec![
                "Check network connectivity or proxy settings.".to_string(),
                "Verify the target service is reachable.".to_string(),
            ],
            vec![
                ("error_kind", format!("{:?}", err.kind())),
                ("details", details),
            ],
        ),
        std::io::ErrorKind::HostUnreachable | std::io::ErrorKind::NetworkUnreachable => {
            build_hints(
                "Network/host unreachable while connecting.",
                vec![
                    "Confirm the endpoint is reachable (e.g. `curl -v <url>`).".to_string(),
                    "Check VPN, proxy, and firewall settings.".to_string(),
                    "If curl works but Pi does not, this is a client connect-path issue \
                     (IPv6/IPv4 reachability or DNS) — please report it."
                        .to_string(),
                ],
                vec![
                    ("error_kind", format!("{:?}", err.kind())),
                    ("details", details),
                ],
            )
        }
        _ => build_hints(
            "I/O error occurred.",
            vec![
                "Check file paths and permissions.".to_string(),
                "Retry after resolving any transient issues.".to_string(),
            ],
            vec![
                ("error_kind", format!("{:?}", err.kind())),
                ("details", details),
            ],
        ),
    }
}

fn sqlite_hints(err: &sqlmodel_core::Error) -> ErrorHints {
    let details = err.to_string();
    let lower = details.to_lowercase();
    if contains_any(&lower, &["database is locked", "busy"]) {
        return build_hints(
            "SQLite database is locked.",
            vec![
                "Close other Pi instances using the same database.".to_string(),
                "Retry once the lock clears.".to_string(),
            ],
            vec![("details", details)],
        );
    }
    build_hints(
        "SQLite error.",
        vec![
            "Ensure the database path is writable.".to_string(),
            "Check for schema or migration issues.".to_string(),
        ],
        vec![("details", details)],
    )
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(Box::new(value))
    }
}

impl From<asupersync::sync::LockError> for Error {
    fn from(value: asupersync::sync::LockError) -> Self {
        match value {
            asupersync::sync::LockError::Cancelled => Self::Aborted,
            asupersync::sync::LockError::Poisoned
            | asupersync::sync::LockError::PolledAfterCompletion
            // asupersync 0.3.2 added LockError::TimedOut(Time) (lock-acquire deadline
            // elapsed); surface it as a session error — its Display carries the detail.
            | asupersync::sync::LockError::TimedOut(_) => {
                Self::session(value.to_string())
            }
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(Box::new(value))
    }
}

impl From<sqlmodel_core::Error> for Error {
    fn from(value: sqlmodel_core::Error) -> Self {
        Self::Sqlite(Box::new(value))
    }
}

// ─── Context overflow detection ─────────────────────────────────────────

/// All 15 pi-mono overflow substring patterns (case-insensitive).
const OVERFLOW_PATTERNS: &[&str] = &[
    "prompt is too long",
    "input is too long for requested model",
    "exceeds the context window",
    // "input token count.*exceeds the maximum" handled by regex below
    // "maximum prompt length is \\d+" handled by regex below
    "reduce the length of the messages",
    // "maximum context length is \\d+ tokens" handled by regex below
    // "exceeds the limit of \\d+" handled by regex below
    "exceeds the available context size",
    "greater than the context length",
    "context window exceeds limit",
    "exceeded model token limit",
    // "context[_ ]length[_ ]exceeded" handled by regex below
    "too many tokens",
    "token limit exceeded",
];

static OVERFLOW_RE: OnceLock<regex::RegexSet> = OnceLock::new();
static RETRYABLE_RE: OnceLock<regex::Regex> = OnceLock::new();

/// Check whether an error message indicates the prompt exceeded the context
/// window. Matches the 15 pi-mono overflow patterns plus Cerebras/Mistral
/// status code pattern.
///
/// Also detects "silent" overflow when `usage_input_tokens` exceeds
/// `context_window`.
pub fn is_context_overflow(
    error_message: &str,
    usage_input_tokens: Option<u64>,
    context_window: Option<u32>,
) -> bool {
    // Silent overflow: usage exceeds context window.
    if let (Some(input_tokens), Some(window)) = (usage_input_tokens, context_window)
        && input_tokens > u64::from(window)
    {
        return true;
    }

    let lower = error_message.to_lowercase();

    // Simple substring checks.
    if OVERFLOW_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
    {
        return true;
    }

    // Regex patterns for the remaining pi-mono checks.
    let re = OVERFLOW_RE.get_or_init(|| {
        regex::RegexSet::new([
            r"input token count.*exceeds the maximum",
            r"maximum prompt length is \d+",
            r"maximum context length is \d+ tokens",
            r"exceeds the limit of \d+",
            r"context[_ ]length[_ ]exceeded",
            // Cerebras/Mistral: "4XX (no body)" pattern.
            r"^4(00|13)\s*(status code)?\s*\(no body\)",
        ])
        .expect("overflow regex set")
    });

    re.is_match(&lower)
}

// ─── Retryable error classification ─────────────────────────────────────

/// Whether an [`std::io::ErrorKind`] denotes a *transient* transport failure.
///
/// A transient failure is a connection that dropped mid-request and is worth
/// retrying with a fresh connection, as opposed to a terminal error (refused,
/// DNS, cert, ...). These are the kinds a dropped/half-open TLS or TCP
/// connection surfaces: the peer reset/aborted the socket, the pipe broke, the
/// read hit an unexpected EOF (rustls also reports a `close_notify`-less close
/// as `UnexpectedEof`), the socket was reported not-connected, or the
/// operation timed out.
#[must_use]
pub const fn io_kind_is_transient(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::TimedOut
    )
}

/// Walk an [`std::io::Error`] and its inner `get_ref` chain (which
/// `Error::source` intentionally skips for io-wrapping-io) checking for a
/// transient [`std::io::ErrorKind`].
fn io_error_chain_is_transient(err: &std::io::Error) -> bool {
    let mut current = Some(err);
    while let Some(io_err) = current {
        if io_kind_is_transient(io_err.kind()) {
            return true;
        }
        current = io_err
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<std::io::Error>());
    }
    false
}

/// Check whether an error is retryable (transient) from its flattened message
/// string.
///
/// This is the **fallback** classifier for errors that only exist as
/// prose (dependency messages with no typed cause); typed errors should be
/// classified with [`Error::is_transient`] first — see pi_agent_rust#118.
/// Matches pi-mono's `_isRetryableError()` logic:
///
/// 1. Error message must be non-empty.
/// 2. Must NOT be context overflow (those need compaction, not retry).
/// 3. Must match a retryable pattern (rate limit, server error, transient
///    connection drop, etc.).
pub fn is_retryable_error(
    error_message: &str,
    usage_input_tokens: Option<u64>,
    context_window: Option<u32>,
) -> bool {
    if error_message.is_empty() {
        return false;
    }

    // Context overflow is NOT retryable.
    if is_context_overflow(error_message, usage_input_tokens, context_window) {
        return false;
    }

    let lower = error_message.to_lowercase();

    let re = RETRYABLE_RE.get_or_init(|| {
        regex::Regex::new(
            r"overloaded|rate.?limit|too many requests|429|500|502|503|504|service.?unavailable|server error|internal error|connection.?error|connection.?refused|connection.?reset|connection.?aborted|connection.?closed|connection.?dropped|other side closed|closed before headers|closed before message|close_notify|broken pipe|unexpected eof|unexpected end of file|transient connection|fetch failed|upstream.?connect|reset before headers|terminated|retry delay",
        )
        .expect("retryable regex")
    });

    re.is_match(&lower)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_value<'a>(hints: &'a ErrorHints, key: &str) -> Option<&'a str> {
        hints
            .context
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value.as_str())
    }

    // ─── Constructor tests ──────────────────────────────────────────────

    #[test]
    fn error_config_constructor() {
        let err = Error::config("bad config");
        assert!(matches!(err, Error::Config(ref msg) if msg == "bad config"));
    }

    #[test]
    fn error_session_constructor() {
        let err = Error::session("session corrupted");
        assert!(matches!(err, Error::Session(ref msg) if msg == "session corrupted"));
    }

    #[test]
    fn error_provider_constructor() {
        let err = Error::provider("anthropic", "timeout");
        assert!(matches!(err, Error::Provider { ref provider, ref message }
            if provider == "anthropic" && message == "timeout"));
    }

    #[test]
    fn error_auth_constructor() {
        let err = Error::auth("missing key");
        assert!(matches!(err, Error::Auth(ref msg) if msg == "missing key"));
    }

    #[test]
    fn error_tool_constructor() {
        let err = Error::tool("bash", "exit code 1");
        assert!(matches!(err, Error::Tool { ref tool, ref message }
            if tool == "bash" && message == "exit code 1"));
    }

    #[test]
    fn error_validation_constructor() {
        let err = Error::validation("field required");
        assert!(matches!(err, Error::Validation(ref msg) if msg == "field required"));
    }

    #[test]
    fn error_extension_constructor() {
        let err = Error::extension("manifest invalid");
        assert!(matches!(err, Error::Extension(ref msg) if msg == "manifest invalid"));
    }

    #[test]
    fn error_api_constructor() {
        let err = Error::api("404 not found");
        assert!(matches!(err, Error::Api(ref msg) if msg == "404 not found"));
    }

    #[test]
    fn error_category_code_is_stable() {
        assert_eq!(Error::auth("missing").category_code(), "auth");
        assert_eq!(Error::provider("openai", "429").category_code(), "provider");
        assert_eq!(Error::tool("bash", "failed").category_code(), "tool");
        assert_eq!(Error::Aborted.category_code(), "runtime");
    }

    #[test]
    fn hints_include_error_category_context() {
        let hints = Error::tool("bash", "exit code 1").hints();
        assert_eq!(context_value(&hints, "error_category"), Some("tool"));
    }

    // ─── Display message tests ──────────────────────────────────────────

    #[test]
    fn error_config_display() {
        let err = Error::config("missing settings.json");
        let msg = err.to_string();
        assert!(msg.contains("Configuration error"));
        assert!(msg.contains("missing settings.json"));
    }

    #[test]
    fn error_session_display() {
        let err = Error::session("tree corrupted");
        let msg = err.to_string();
        assert!(msg.contains("Session error"));
        assert!(msg.contains("tree corrupted"));
    }

    #[test]
    fn error_session_not_found_display() {
        let err = Error::SessionNotFound {
            path: "/home/user/.pi/sessions/abc.jsonl".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Session not found"));
        assert!(msg.contains("/home/user/.pi/sessions/abc.jsonl"));
    }

    #[test]
    fn error_provider_display() {
        let err = Error::provider("openai", "429 too many requests");
        let msg = err.to_string();
        assert!(msg.contains("Provider error"));
        assert!(msg.contains("openai"));
        assert!(msg.contains("429 too many requests"));
    }

    #[test]
    fn error_auth_display() {
        let err = Error::auth("API key expired");
        let msg = err.to_string();
        assert!(msg.contains("Authentication error"));
        assert!(msg.contains("API key expired"));
    }

    #[test]
    fn error_tool_display() {
        let err = Error::tool("read", "file not found: /tmp/x.txt");
        let msg = err.to_string();
        assert!(msg.contains("Tool error"));
        assert!(msg.contains("read"));
        assert!(msg.contains("file not found: /tmp/x.txt"));
    }

    #[test]
    fn error_validation_display() {
        let err = Error::validation("temperature must be 0-2");
        let msg = err.to_string();
        assert!(msg.contains("Validation error"));
        assert!(msg.contains("temperature must be 0-2"));
    }

    #[test]
    fn error_extension_display() {
        let err = Error::extension("manifest parse failed");
        let msg = err.to_string();
        assert!(msg.contains("Extension error"));
        assert!(msg.contains("manifest parse failed"));
    }

    #[test]
    fn error_aborted_display() {
        let err = Error::Aborted;
        let msg = err.to_string();
        assert!(msg.contains("Operation aborted"));
    }

    #[test]
    fn error_api_display() {
        let err = Error::api("GitHub API error 403");
        let msg = err.to_string();
        assert!(msg.contains("API error"));
        assert!(msg.contains("GitHub API error 403"));
    }

    #[test]
    fn error_io_display() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err = Error::from(io_err);
        let msg = err.to_string();
        assert!(msg.contains("IO error"));
    }

    #[test]
    fn error_json_display() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err = Error::from(json_err);
        let msg = err.to_string();
        assert!(msg.contains("JSON error"));
    }

    // ─── From impls ─────────────────────────────────────────────────────

    #[test]
    fn error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn error_from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("{invalid").unwrap_err();
        let err: Error = json_err.into();
        assert!(matches!(err, Error::Json(_)));
    }

    // ─── Hints method tests (error.rs hints) ────────────────────────────

    #[test]
    fn hints_config_json_parse_error() {
        let err = Error::config("JSON parse error in settings.json");
        let h = err.hints();
        assert!(h.summary.contains("not valid JSON"));
        assert!(h.hints.iter().any(|s| s.contains("JSON formatting")));
    }

    #[test]
    fn hints_config_missing_file() {
        let err = Error::config("config file not found: ~/.pi/settings");
        let h = err.hints();
        assert!(h.summary.contains("missing"));
    }

    #[test]
    fn hints_config_generic() {
        let err = Error::config("unknown config issue");
        let h = err.hints();
        assert!(h.summary.contains("Configuration error"));
    }

    #[test]
    fn hints_session_empty() {
        let err = Error::session("empty session file");
        let h = err.hints();
        assert!(h.summary.contains("empty") || h.summary.contains("corrupted"));
    }

    #[test]
    fn hints_session_read_failure() {
        let err = Error::session("failed to read session directory");
        let h = err.hints();
        assert!(h.summary.contains("Failed to read"));
    }

    #[test]
    fn hints_session_not_found() {
        let err = Error::SessionNotFound {
            path: "/tmp/session.jsonl".to_string(),
        };
        let h = err.hints();
        assert!(h.summary.contains("not found"));
        assert!(
            h.context
                .iter()
                .any(|(k, v)| k == "path" && v.contains("/tmp/session.jsonl"))
        );
    }

    #[test]
    fn hints_provider_401() {
        let err = Error::provider("anthropic", "HTTP 401 unauthorized");
        let h = err.hints();
        assert!(h.summary.contains("authentication failed"));
        assert!(h.hints.iter().any(|s| s.contains("ANTHROPIC_API_KEY")));
    }

    #[test]
    fn hints_provider_403() {
        let err = Error::provider("openai", "403 forbidden");
        let h = err.hints();
        assert!(h.summary.contains("forbidden"));
    }

    #[test]
    fn hints_provider_429() {
        let err = Error::provider("anthropic", "429 rate limit");
        let h = err.hints();
        assert!(h.summary.contains("rate limited"));
    }

    #[test]
    fn hints_provider_529() {
        let err = Error::provider("anthropic", "529 overloaded");
        let h = err.hints();
        assert!(h.summary.contains("overloaded"));
    }

    #[test]
    fn hints_provider_timeout() {
        let err = Error::provider("openai", "request timed out");
        let h = err.hints();
        assert!(h.summary.contains("timed out"));
    }

    #[test]
    fn hints_provider_400() {
        let err = Error::provider("gemini", "400 bad request");
        let h = err.hints();
        assert!(h.summary.contains("rejected"));
    }

    #[test]
    fn hints_provider_500() {
        let err = Error::provider("cohere", "500 internal server error");
        let h = err.hints();
        assert!(h.summary.contains("server error"));
    }

    #[test]
    fn hints_provider_generic() {
        let err = Error::provider("custom", "unknown issue");
        let h = err.hints();
        assert!(h.summary.contains("failed"));
        assert!(h.context.iter().any(|(k, _)| k == "provider"));
    }

    #[test]
    fn hints_provider_key_hint_openai() {
        let err = Error::provider("openai", "401 invalid api key");
        let h = err.hints();
        assert!(h.hints.iter().any(|s| s.contains("OPENAI_API_KEY")));
    }

    #[test]
    fn hints_provider_key_hint_gemini() {
        let err = Error::provider("gemini", "401 api key invalid");
        let h = err.hints();
        assert!(h.hints.iter().any(|s| s.contains("GOOGLE_API_KEY")));
    }

    #[test]
    fn hints_provider_key_hint_openrouter() {
        let err = Error::provider("openrouter", "401 unauthorized");
        let h = err.hints();
        assert!(h.hints.iter().any(|s| s.contains("OPENROUTER_API_KEY")));
    }

    #[test]
    fn hints_provider_key_hint_groq() {
        let err = Error::provider("groq", "401 unauthorized");
        let h = err.hints();
        assert!(h.hints.iter().any(|s| s.contains("GROQ_API_KEY")));
    }

    #[test]
    fn hints_provider_key_hint_alias_dashscope() {
        let err = Error::provider("dashscope", "401 invalid api key");
        let h = err.hints();
        assert!(h.hints.iter().any(|s| s.contains("DASHSCOPE_API_KEY")));
        assert!(h.hints.iter().any(|s| s.contains("QWEN_API_KEY")));
    }

    #[test]
    fn hints_provider_key_hint_alias_kimi() {
        let err = Error::provider("kimi", "401 invalid api key");
        let h = err.hints();
        assert!(h.hints.iter().any(|s| s.contains("MOONSHOT_API_KEY")));
        assert!(h.hints.iter().any(|s| s.contains("KIMI_API_KEY")));
    }

    #[test]
    fn hints_provider_key_hint_azure() {
        let err = Error::provider("azure-openai", "401 unauthorized");
        let h = err.hints();
        assert!(h.hints.iter().any(|s| s.contains("AZURE_OPENAI_API_KEY")));
    }

    #[test]
    fn hints_provider_key_hint_unknown() {
        let err = Error::provider("my-proxy", "401 unauthorized");
        let h = err.hints();
        assert!(h.hints.iter().any(|s| s.contains("my-proxy")));
    }

    #[test]
    fn hints_auth_authorization_code() {
        let err = Error::auth("missing authorization code");
        let h = err.hints();
        assert!(h.summary.contains("OAuth"));
    }

    #[test]
    fn hints_auth_token_exchange() {
        let err = Error::auth("token exchange failed");
        let h = err.hints();
        assert!(h.summary.contains("token exchange"));
    }

    #[test]
    fn hints_auth_generic() {
        let err = Error::auth("unknown auth issue");
        let h = err.hints();
        assert!(h.summary.contains("Authentication error"));
    }

    #[test]
    fn auth_diagnostic_provider_invalid_key_code_and_context() {
        let err = Error::provider("openai", "HTTP 401 unauthorized");
        let diagnostic = err.auth_diagnostic().expect("diagnostic should be present");
        assert_eq!(diagnostic.code, AuthDiagnosticCode::InvalidApiKey);
        assert_eq!(diagnostic.code.as_str(), "auth.invalid_api_key");
        assert_eq!(diagnostic.redaction_policy, "redact-secrets");

        let hints = err.hints();
        assert_eq!(
            context_value(&hints, "diagnostic_code"),
            Some("auth.invalid_api_key")
        );
        assert_eq!(
            context_value(&hints, "redaction_policy"),
            Some("redact-secrets")
        );
    }

    #[test]
    fn auth_diagnostic_missing_key_phrase_for_oai_provider() {
        let err = Error::provider(
            "openrouter",
            "You didn't provide an API key in the Authorization header",
        );
        let diagnostic = err.auth_diagnostic().expect("diagnostic should be present");
        assert_eq!(diagnostic.code, AuthDiagnosticCode::MissingApiKey);
        assert_eq!(diagnostic.code.as_str(), "auth.missing_api_key");
    }

    #[test]
    fn auth_diagnostic_revoked_key_maps_invalid() {
        let err = Error::provider("deepseek", "API key revoked for this project");
        let diagnostic = err.auth_diagnostic().expect("diagnostic should be present");
        assert_eq!(diagnostic.code, AuthDiagnosticCode::InvalidApiKey);
        assert_eq!(diagnostic.code.as_str(), "auth.invalid_api_key");
    }

    #[test]
    fn auth_diagnostic_quota_exceeded_code_and_context() {
        let err = Error::provider(
            "openai",
            "HTTP 429 insufficient_quota: You exceeded your current quota",
        );
        let diagnostic = err.auth_diagnostic().expect("diagnostic should be present");
        assert_eq!(diagnostic.code, AuthDiagnosticCode::QuotaExceeded);
        assert_eq!(diagnostic.code.as_str(), "auth.quota_exceeded");
        assert_eq!(
            diagnostic.remediation,
            "Verify billing/quota limits for this API key or organization, then retry."
        );

        let hints = err.hints();
        assert_eq!(
            context_value(&hints, "diagnostic_code"),
            Some("auth.quota_exceeded")
        );
        assert!(
            hints
                .hints
                .iter()
                .any(|s| s.contains("billing") || s.contains("quota")),
            "quota/billing guidance should be present"
        );
    }

    #[test]
    fn auth_diagnostic_oauth_exchange_failure_code() {
        let err = Error::auth("Token exchange failed: invalid_grant");
        let diagnostic = err.auth_diagnostic().expect("diagnostic should be present");
        assert_eq!(
            diagnostic.code,
            AuthDiagnosticCode::OAuthTokenExchangeFailed
        );
        assert_eq!(
            diagnostic.remediation,
            "Retry login flow and verify token endpoint/client configuration."
        );

        let hints = err.hints();
        assert_eq!(
            context_value(&hints, "diagnostic_code"),
            Some("auth.oauth.token_exchange_failed")
        );
    }

    #[test]
    fn auth_diagnostic_azure_missing_deployment_code() {
        let err = Error::provider(
            "azure-openai",
            "Azure OpenAI provider requires resource+deployment; configure via models.json",
        );
        let diagnostic = err.auth_diagnostic().expect("diagnostic should be present");
        assert_eq!(diagnostic.code, AuthDiagnosticCode::MissingAzureDeployment);
        assert_eq!(diagnostic.code.as_str(), "config.azure.missing_deployment");
    }

    #[test]
    fn auth_diagnostic_bedrock_missing_credential_chain_code() {
        let err = Error::provider(
            "amazon-bedrock",
            "AWS credential chain not configured for provider",
        );
        let diagnostic = err.auth_diagnostic().expect("diagnostic should be present");
        assert_eq!(diagnostic.code, AuthDiagnosticCode::MissingCredentialChain);
        assert_eq!(diagnostic.code.as_str(), "auth.credential_chain.missing");
    }

    #[test]
    fn auth_diagnostic_absent_for_non_auth_provider_error() {
        let err = Error::provider("anthropic", "429 rate limit");
        assert!(err.auth_diagnostic().is_none());

        let hints = err.hints();
        assert!(context_value(&hints, "diagnostic_code").is_none());
    }

    // ── Native provider diagnostic integration tests ─────────────────
    // Verify that actual provider error messages (as emitted by providers/*.rs
    // after the Error::config→Error::provider migration) are correctly classified
    // by the diagnostic taxonomy.

    #[test]
    fn native_provider_missing_key_anthropic() {
        let err = Error::provider(
            "anthropic",
            "Missing API key for Anthropic. Set ANTHROPIC_API_KEY or use `pi auth`.",
        );
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::MissingApiKey);
        let hints = err.hints();
        assert_eq!(context_value(&hints, "provider"), Some("anthropic"));
        assert!(
            hints.summary.contains("missing"),
            "summary: {}",
            hints.summary
        );
    }

    #[test]
    fn native_provider_missing_key_openai() {
        let err = Error::provider(
            "openai",
            "Missing API key for OpenAI. Set OPENAI_API_KEY or configure in settings.",
        );
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::MissingApiKey);
    }

    #[test]
    fn native_provider_missing_key_azure() {
        let err = Error::provider(
            "azure-openai",
            "Missing API key for Azure OpenAI. Set AZURE_OPENAI_API_KEY or configure in settings.",
        );
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::MissingApiKey);
    }

    #[test]
    fn native_provider_missing_key_cohere() {
        let err = Error::provider(
            "cohere",
            "Missing API key for Cohere. Set COHERE_API_KEY or configure in settings.",
        );
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::MissingApiKey);
    }

    #[test]
    fn native_provider_missing_key_gemini() {
        let err = Error::provider(
            "google",
            "Missing API key for Google/Gemini. Set GOOGLE_API_KEY or GEMINI_API_KEY.",
        );
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::MissingApiKey);
    }

    #[test]
    fn native_provider_http_401_anthropic() {
        let err = Error::provider(
            "anthropic",
            "Anthropic API error (HTTP 401): {\"error\":{\"type\":\"authentication_error\"}}",
        );
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::InvalidApiKey);
        let hints = err.hints();
        assert!(hints.summary.contains("authentication failed"));
    }

    #[test]
    fn native_provider_http_401_openai() {
        let err = Error::provider(
            "openai",
            "OpenAI API error (HTTP 401): Incorrect API key provided",
        );
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::InvalidApiKey);
    }

    #[test]
    fn native_provider_http_403_azure() {
        let err = Error::provider(
            "azure-openai",
            "Azure OpenAI API error (HTTP 403): Access denied",
        );
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::InvalidApiKey);
    }

    #[test]
    fn native_provider_http_429_quota_openai() {
        let err = Error::provider("openai", "OpenAI API error (HTTP 429): insufficient_quota");
        let d = err.auth_diagnostic().expect("diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::QuotaExceeded);
    }

    #[test]
    fn native_provider_http_500_no_diagnostic() {
        // Non-auth HTTP errors should NOT produce auth diagnostics.
        let err = Error::provider(
            "anthropic",
            "Anthropic API error (HTTP 500): Internal server error",
        );
        assert!(err.auth_diagnostic().is_none());
    }

    #[test]
    fn native_provider_hints_include_provider_context() {
        let err = Error::provider("cohere", "Cohere API error (HTTP 401): unauthorized");
        let hints = err.hints();
        assert_eq!(context_value(&hints, "provider"), Some("cohere"));
        assert!(context_value(&hints, "details").is_some());
    }

    #[test]
    fn native_provider_diagnostic_enriches_hints_context() {
        let err = Error::provider(
            "google",
            "Missing API key for Google/Gemini. Set GOOGLE_API_KEY or GEMINI_API_KEY.",
        );
        let hints = err.hints();
        assert_eq!(
            context_value(&hints, "diagnostic_code"),
            Some("auth.missing_api_key")
        );
        assert_eq!(
            context_value(&hints, "redaction_policy"),
            Some("redact-secrets")
        );
        assert!(context_value(&hints, "diagnostic_remediation").is_some());
    }

    #[test]
    fn hints_tool_not_found() {
        let err = Error::tool("bash", "command not found: xyz");
        let h = err.hints();
        assert!(h.summary.contains("not found"));
    }

    #[test]
    fn hints_tool_generic() {
        let err = Error::tool("read", "unexpected error");
        let h = err.hints();
        assert!(h.summary.contains("execution failed"));
    }

    #[test]
    fn hints_validation() {
        let err = Error::validation("invalid input");
        let h = err.hints();
        assert!(h.summary.contains("Validation"));
    }

    #[test]
    fn hints_extension() {
        let err = Error::extension("load error");
        let h = err.hints();
        assert!(h.summary.contains("Extension"));
    }

    #[test]
    fn hints_io_not_found() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err = Error::from(io_err);
        let h = err.hints();
        assert!(h.summary.contains("not found"));
    }

    #[test]
    fn hints_io_permission_denied() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = Error::from(io_err);
        let h = err.hints();
        assert!(h.summary.contains("Permission denied"));
    }

    #[test]
    fn hints_io_timed_out() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timed out");
        let err = Error::from(io_err);
        let h = err.hints();
        assert!(h.summary.contains("timed out"));
    }

    #[test]
    fn hints_io_connection_refused() {
        let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let err = Error::from(io_err);
        let h = err.hints();
        assert!(h.summary.contains("Connection refused"));
    }

    #[test]
    fn hints_io_generic() {
        let io_err = std::io::Error::other("something");
        let err = Error::from(io_err);
        let h = err.hints();
        assert!(h.summary.contains("I/O error"));
    }

    #[test]
    fn hints_json() {
        let json_err = serde_json::from_str::<serde_json::Value>("broken").unwrap_err();
        let err = Error::from(json_err);
        let h = err.hints();
        assert!(h.summary.contains("JSON"));
    }

    #[test]
    fn hints_aborted() {
        let err = Error::Aborted;
        let h = err.hints();
        assert!(h.summary.contains("aborted"));
    }

    #[test]
    fn hints_api() {
        let err = Error::api("connection reset");
        let h = err.hints();
        assert!(h.summary.contains("API"));
    }

    // ── E2E cross-provider diagnostic validation ────────────────────

    /// Every provider family's *actual* error message must produce the correct
    /// `AuthDiagnosticCode`. This matrix validates classifier + message alignment.
    #[test]
    fn e2e_all_native_providers_missing_key_diagnostic() {
        let cases: &[(&str, &str)] = &[
            (
                "anthropic",
                "Missing API key for Anthropic. Set ANTHROPIC_API_KEY or use `pi auth`.",
            ),
            (
                "openai",
                "Missing API key for OpenAI. Set OPENAI_API_KEY or configure in settings.",
            ),
            (
                "azure-openai",
                "Missing API key for Azure OpenAI. Set AZURE_OPENAI_API_KEY or configure in settings.",
            ),
            (
                "cohere",
                "Missing API key for Cohere. Set COHERE_API_KEY or configure in settings.",
            ),
            (
                "google",
                "Missing API key for Google/Gemini. Set GOOGLE_API_KEY or GEMINI_API_KEY.",
            ),
        ];
        for (provider, message) in cases {
            let err = Error::provider(*provider, *message);
            let d = err.auth_diagnostic().expect("expected auth diagnostic");
            assert_eq!(
                d.code,
                AuthDiagnosticCode::MissingApiKey,
                "wrong code for {provider}: {:?}",
                d.code
            );
        }
    }

    #[test]
    fn e2e_all_native_providers_401_diagnostic() {
        let cases: &[(&str, &str)] = &[
            (
                "anthropic",
                "Anthropic API error (HTTP 401): invalid x-api-key",
            ),
            (
                "openai",
                "OpenAI API error (HTTP 401): Incorrect API key provided",
            ),
            (
                "azure-openai",
                "Azure OpenAI API error (HTTP 401): unauthorized",
            ),
            ("cohere", "Cohere API error (HTTP 401): unauthorized"),
            ("google", "Gemini API error (HTTP 401): API key not valid"),
        ];
        for (provider, message) in cases {
            let err = Error::provider(*provider, *message);
            let d = err.auth_diagnostic().expect("expected auth diagnostic");
            assert_eq!(
                d.code,
                AuthDiagnosticCode::InvalidApiKey,
                "wrong code for {provider}: {:?}",
                d.code
            );
        }
    }

    /// Non-auth HTTP errors (5xx) must NOT produce auth diagnostics.
    #[test]
    fn e2e_non_auth_errors_no_diagnostic() {
        let cases: &[(&str, &str)] = &[
            (
                "anthropic",
                "Anthropic API error (HTTP 500): Internal server error",
            ),
            ("openai", "OpenAI API error (HTTP 503): Service unavailable"),
            ("google", "Gemini API error (HTTP 502): Bad gateway"),
            ("cohere", "Cohere API error (HTTP 504): Gateway timeout"),
        ];
        for (provider, message) in cases {
            let err = Error::provider(*provider, *message);
            assert!(
                err.auth_diagnostic().is_none(),
                "unexpected diagnostic for {provider} with message: {message}"
            );
        }
    }

    /// All auth diagnostics must carry the `redact-secrets` redaction policy.
    #[test]
    fn e2e_all_diagnostic_codes_have_redact_secrets_policy() {
        let codes = [
            AuthDiagnosticCode::MissingApiKey,
            AuthDiagnosticCode::InvalidApiKey,
            AuthDiagnosticCode::QuotaExceeded,
            AuthDiagnosticCode::MissingOAuthAuthorizationCode,
            AuthDiagnosticCode::OAuthTokenExchangeFailed,
            AuthDiagnosticCode::OAuthTokenRefreshFailed,
            AuthDiagnosticCode::MissingAzureDeployment,
            AuthDiagnosticCode::MissingRegion,
            AuthDiagnosticCode::MissingProject,
            AuthDiagnosticCode::MissingProfile,
            AuthDiagnosticCode::MissingEndpoint,
            AuthDiagnosticCode::MissingCredentialChain,
            AuthDiagnosticCode::UnknownAuthFailure,
        ];
        for code in &codes {
            assert_eq!(
                code.redaction_policy(),
                "redact-secrets",
                "code {code:?} missing redact-secrets policy",
            );
        }
    }

    /// `hints()` must always include diagnostic enrichment when auth diagnostics
    /// are present, and the enrichment must include code + remediation + policy.
    #[test]
    fn e2e_hints_enrichment_completeness() {
        let providers: &[(&str, &str)] = &[
            ("anthropic", "Missing API key for Anthropic"),
            ("openai", "OpenAI API error (HTTP 401): invalid key"),
            ("cohere", "insufficient_quota"),
            ("google", "Missing API key for Google"),
        ];
        for (provider, message) in providers {
            let err = Error::provider(*provider, *message);
            let hints = err.hints();
            assert!(
                context_value(&hints, "diagnostic_code").is_some(),
                "missing diagnostic_code for {provider}"
            );
            assert!(
                context_value(&hints, "diagnostic_remediation").is_some(),
                "missing diagnostic_remediation for {provider}"
            );
            assert_eq!(
                context_value(&hints, "redaction_policy"),
                Some("redact-secrets"),
                "wrong redaction_policy for {provider}"
            );
        }
    }

    /// Provider context must always appear in hints for provider errors.
    #[test]
    fn e2e_hints_always_include_provider_context() {
        let providers = [
            "anthropic",
            "openai",
            "azure-openai",
            "cohere",
            "google",
            "groq",
            "deepseek",
        ];
        for provider in &providers {
            let err = Error::provider(*provider, "some error");
            let hints = err.hints();
            assert_eq!(
                context_value(&hints, "provider"),
                Some(*provider),
                "missing provider context for {provider}"
            );
        }
    }

    /// Provider aliases must produce the same env key hints as canonical IDs.
    #[test]
    fn e2e_alias_env_key_consistency() {
        let alias_to_canonical: &[(&str, &str)] = &[
            ("gemini", "google"),
            ("azure", "azure-openai"),
            ("copilot", "github-copilot"),
            ("dashscope", "alibaba"),
            ("qwen", "alibaba"),
            ("kimi", "moonshotai"),
            ("moonshot", "moonshotai"),
            ("bedrock", "amazon-bedrock"),
            ("sap", "sap-ai-core"),
        ];
        for (alias, canonical) in alias_to_canonical {
            let alias_keys = crate::provider_metadata::provider_auth_env_keys(alias);
            let canonical_keys = crate::provider_metadata::provider_auth_env_keys(canonical);
            assert_eq!(
                alias_keys, canonical_keys,
                "alias {alias} env keys differ from canonical {canonical}"
            );
        }
    }

    /// Every native provider's env key list must be non-empty.
    #[test]
    fn e2e_all_native_providers_have_env_keys() {
        let native_providers = [
            "anthropic",
            "openai",
            "google",
            "cohere",
            "azure-openai",
            "amazon-bedrock",
            "github-copilot",
            "sap-ai-core",
        ];
        for provider in &native_providers {
            let keys = crate::provider_metadata::provider_auth_env_keys(provider);
            assert!(!keys.is_empty(), "provider {provider} has no auth env keys");
        }
    }

    /// Error messages must never contain raw API key values. This test verifies
    /// that provider error constructors don't embed secrets.
    #[test]
    fn e2e_error_messages_never_contain_secrets() {
        let fake_key = "sk-proj-FAKE123456789abcdef";
        // Construct errors the way providers do (from HTTP responses, not from keys).
        let err1 = Error::provider("openai", "OpenAI API error (HTTP 401): Invalid API key");
        let err2 = Error::provider("anthropic", "Missing API key for Anthropic");
        let err3 = Error::auth("OAuth token exchange failed");

        for err in [&err1, &err2, &err3] {
            let display = err.to_string();
            assert!(
                !display.contains(fake_key),
                "error message contains secret: {display}"
            );
            let hints = err.hints();
            for hint in &hints.hints {
                assert!(!hint.contains(fake_key), "hint contains secret: {hint}");
            }
            for (key, value) in &hints.context {
                assert!(
                    !value.contains(fake_key),
                    "context {key} contains secret: {value}"
                );
            }
        }
    }

    /// Bedrock credential-chain special handling: "credential" in message +
    /// "bedrock" in provider must produce `MissingCredentialChain`.
    #[test]
    fn e2e_bedrock_credential_chain_diagnostic() {
        let err = Error::provider("amazon-bedrock", "No credential source configured");
        let d = err
            .auth_diagnostic()
            .expect("expected credential chain diagnostic");
        assert_eq!(d.code, AuthDiagnosticCode::MissingCredentialChain);
    }

    /// Auth errors (not provider errors) must also produce diagnostics.
    #[test]
    fn e2e_auth_variant_diagnostics() {
        let cases: &[(&str, AuthDiagnosticCode)] = &[
            ("Missing API key", AuthDiagnosticCode::MissingApiKey),
            ("401 unauthorized", AuthDiagnosticCode::InvalidApiKey),
            ("insufficient_quota", AuthDiagnosticCode::QuotaExceeded),
            (
                "Missing authorization code",
                AuthDiagnosticCode::MissingOAuthAuthorizationCode,
            ),
            (
                "Token exchange failed",
                AuthDiagnosticCode::OAuthTokenExchangeFailed,
            ),
            (
                "OAuth token refresh failed",
                AuthDiagnosticCode::OAuthTokenRefreshFailed,
            ),
            (
                "Missing deployment",
                AuthDiagnosticCode::MissingAzureDeployment,
            ),
            ("Missing region", AuthDiagnosticCode::MissingRegion),
            ("Missing project", AuthDiagnosticCode::MissingProject),
            ("Missing profile", AuthDiagnosticCode::MissingProfile),
            ("Missing endpoint", AuthDiagnosticCode::MissingEndpoint),
            (
                "credential chain not configured",
                AuthDiagnosticCode::MissingCredentialChain,
            ),
        ];
        for (message, expected_code) in cases {
            let err = Error::auth(*message);
            let d = err.auth_diagnostic().expect("expected auth diagnostic");
            assert_eq!(
                d.code, *expected_code,
                "wrong code for Auth({message}): {:?}",
                d.code
            );
        }
    }

    /// Classifier must be case-insensitive.
    #[test]
    fn e2e_classifier_case_insensitive() {
        let variants = ["MISSING API KEY", "Missing Api Key", "missing api key"];
        for msg in &variants {
            let err = Error::provider("openai", *msg);
            let d = err.auth_diagnostic().expect("expected auth diagnostic");
            assert_eq!(
                d.code,
                AuthDiagnosticCode::MissingApiKey,
                "failed for: {msg}"
            );
        }
    }

    /// Non-auth error variants must never produce diagnostics.
    #[test]
    fn e2e_non_auth_variants_no_diagnostic() {
        let errors: Vec<Error> = vec![
            Error::config("bad json"),
            Error::session("timeout"),
            Error::tool("bash", "not found"),
            Error::validation("missing field"),
            Error::extension("crash"),
            Error::api("network error"),
            Error::Aborted,
        ];
        for err in &errors {
            assert!(
                err.auth_diagnostic().is_none(),
                "unexpected diagnostic for: {err}"
            );
        }
    }

    /// Quota-exceeded messages from different providers produce the same code.
    #[test]
    fn e2e_quota_messages_cross_provider() {
        let messages = [
            "insufficient_quota",
            "quota exceeded",
            "billing hard limit reached",
            "billing_not_active",
            "not enough credits",
            "credit balance is too low",
        ];
        for msg in &messages {
            let err = Error::provider("openai", *msg);
            let d = err.auth_diagnostic().expect("expected auth diagnostic");
            assert_eq!(
                d.code,
                AuthDiagnosticCode::QuotaExceeded,
                "wrong code for: {msg}"
            );
        }
    }

    /// OpenAI-compatible providers must resolve env keys through alias mapping.
    #[test]
    fn e2e_openai_compatible_providers_env_keys() {
        let providers_and_keys: &[(&str, &str)] = &[
            ("groq", "GROQ_API_KEY"),
            ("deepinfra", "DEEPINFRA_API_KEY"),
            ("cerebras", "CEREBRAS_API_KEY"),
            ("openrouter", "OPENROUTER_API_KEY"),
            ("mistral", "MISTRAL_API_KEY"),
            ("moonshotai", "MOONSHOT_API_KEY"),
            ("moonshotai", "KIMI_API_KEY"),
            ("alibaba", "DASHSCOPE_API_KEY"),
            ("alibaba", "QWEN_API_KEY"),
            ("alibaba-us", "DASHSCOPE_API_KEY"),
            ("alibaba-us", "QWEN_API_KEY"),
            ("deepseek", "DEEPSEEK_API_KEY"),
            ("perplexity", "PERPLEXITY_API_KEY"),
            ("xai", "XAI_API_KEY"),
        ];
        for (provider, expected_key) in providers_and_keys {
            let keys = crate::provider_metadata::provider_auth_env_keys(provider);
            assert!(
                keys.contains(expected_key),
                "provider {provider} missing env key {expected_key}, got: {keys:?}"
            );
        }
    }

    /// `provider_key_hint()` uses canonical ID and includes env vars in output.
    #[test]
    fn e2e_key_hint_format_consistency() {
        // Anthropic gets special `/login` hint.
        let hint = provider_key_hint("anthropic");
        assert!(hint.contains("ANTHROPIC_API_KEY"), "hint: {hint}");
        assert!(hint.contains("/login"), "hint: {hint}");

        // Copilot gets `/login` hint.
        let hint = provider_key_hint("github-copilot");
        assert!(hint.contains("/login"), "hint: {hint}");

        // OpenAI gets standard format.
        let hint = provider_key_hint("openai");
        assert!(hint.contains("OPENAI_API_KEY"), "hint: {hint}");
        assert!(!hint.contains("/login"), "hint: {hint}");

        // Unknown provider gets fallback.
        let hint = provider_key_hint("my-custom-proxy");
        assert!(hint.contains("my-custom-proxy"), "hint: {hint}");
    }

    /// Empty messages produce no diagnostic (no false positives).
    #[test]
    fn e2e_empty_message_no_diagnostic() {
        let err = Error::provider("openai", "");
        assert!(err.auth_diagnostic().is_none());
    }

    // ─── Context overflow detection tests ────────────────────────────

    #[test]
    fn overflow_prompt_is_too_long() {
        assert!(is_context_overflow(
            "prompt is too long: 150000 tokens",
            None,
            None
        ));
    }

    #[test]
    fn overflow_input_too_long_for_model() {
        assert!(is_context_overflow(
            "input is too long for requested model",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_exceeds_context_window() {
        assert!(is_context_overflow(
            "exceeds the context window",
            None,
            None
        ));
    }

    #[test]
    fn overflow_input_token_count_exceeds_maximum() {
        assert!(is_context_overflow(
            "input token count of 50000 exceeds the maximum of 32000",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_maximum_prompt_length() {
        assert!(is_context_overflow(
            "maximum prompt length is 32000",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_reduce_length_of_messages() {
        assert!(is_context_overflow(
            "reduce the length of the messages",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_maximum_context_length() {
        assert!(is_context_overflow(
            "maximum context length is 128000 tokens",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_exceeds_limit_of() {
        assert!(is_context_overflow(
            "exceeds the limit of 200000",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_exceeds_available_context_size() {
        assert!(is_context_overflow(
            "exceeds the available context size",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_greater_than_context_length() {
        assert!(is_context_overflow(
            "greater than the context length",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_context_window_exceeds_limit() {
        assert!(is_context_overflow(
            "context window exceeds limit",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_exceeded_model_token_limit() {
        assert!(is_context_overflow(
            "exceeded model token limit",
            None,
            None,
        ));
    }

    #[test]
    fn overflow_context_length_exceeded_underscore() {
        assert!(is_context_overflow("context_length_exceeded", None, None));
    }

    #[test]
    fn overflow_context_length_exceeded_space() {
        assert!(is_context_overflow("context length exceeded", None, None));
    }

    #[test]
    fn overflow_too_many_tokens() {
        assert!(is_context_overflow("too many tokens", None, None));
    }

    #[test]
    fn overflow_token_limit_exceeded() {
        assert!(is_context_overflow("token limit exceeded", None, None));
    }

    #[test]
    fn overflow_cerebras_400_no_body() {
        assert!(is_context_overflow("400 (no body)", None, None));
    }

    #[test]
    fn overflow_cerebras_413_no_body() {
        assert!(is_context_overflow("413 (no body)", None, None));
    }

    #[test]
    fn overflow_mistral_status_code_pattern() {
        assert!(is_context_overflow("413 status code (no body)", None, None,));
    }

    #[test]
    fn overflow_case_insensitive() {
        assert!(is_context_overflow("PROMPT IS TOO LONG", None, None));
        assert!(is_context_overflow("Token Limit Exceeded", None, None));
    }

    #[test]
    fn overflow_silent_usage_exceeds_window() {
        assert!(is_context_overflow(
            "some error",
            Some(250_000),
            Some(200_000),
        ));
    }

    #[test]
    fn overflow_usage_within_window() {
        assert!(!is_context_overflow(
            "some error",
            Some(100_000),
            Some(200_000),
        ));
    }

    #[test]
    fn overflow_no_usage_info() {
        assert!(!is_context_overflow("some error", None, None));
    }

    #[test]
    fn overflow_negative_not_matched() {
        assert!(!is_context_overflow("rate limit exceeded", None, None));
        assert!(!is_context_overflow("server error 500", None, None));
        assert!(!is_context_overflow("authentication error", None, None));
        assert!(!is_context_overflow("", None, None));
    }

    // ─── Retryable error classification tests ────────────────────────

    #[test]
    fn retryable_rate_limit() {
        assert!(is_retryable_error("429 rate limit exceeded", None, None));
    }

    #[test]
    fn retryable_too_many_requests() {
        assert!(is_retryable_error("too many requests", None, None));
    }

    #[test]
    fn retryable_overloaded() {
        assert!(is_retryable_error("API overloaded", None, None));
    }

    #[test]
    fn retryable_server_500() {
        assert!(is_retryable_error(
            "HTTP 500 internal server error",
            None,
            None
        ));
    }

    #[test]
    fn retryable_server_502() {
        assert!(is_retryable_error("502 bad gateway", None, None));
    }

    #[test]
    fn retryable_server_503() {
        assert!(is_retryable_error("503 service unavailable", None, None));
    }

    #[test]
    fn retryable_server_504() {
        assert!(is_retryable_error("504 gateway timeout", None, None));
    }

    #[test]
    fn retryable_service_unavailable() {
        assert!(is_retryable_error("service unavailable", None, None));
    }

    #[test]
    fn retryable_server_error() {
        assert!(is_retryable_error("server error", None, None));
    }

    #[test]
    fn retryable_internal_error() {
        assert!(is_retryable_error("internal error occurred", None, None));
    }

    #[test]
    fn retryable_connection_error() {
        assert!(is_retryable_error("connection error", None, None));
    }

    #[test]
    fn retryable_connection_refused() {
        assert!(is_retryable_error("connection refused", None, None));
    }

    #[test]
    fn retryable_other_side_closed() {
        assert!(is_retryable_error("other side closed", None, None));
    }

    #[test]
    fn retryable_fetch_failed() {
        assert!(is_retryable_error("fetch failed", None, None));
    }

    #[test]
    fn retryable_upstream_connect() {
        assert!(is_retryable_error("upstream connect error", None, None));
    }

    #[test]
    fn retryable_reset_before_headers() {
        assert!(is_retryable_error("reset before headers", None, None));
    }

    #[test]
    fn retryable_terminated() {
        assert!(is_retryable_error("request terminated", None, None));
    }

    #[test]
    fn retryable_retry_delay() {
        assert!(is_retryable_error("retry delay 30s", None, None));
    }

    #[test]
    fn not_retryable_context_overflow() {
        // Context overflow should NOT be retried.
        assert!(!is_retryable_error("prompt is too long", None, None));
        assert!(!is_retryable_error(
            "exceeds the context window",
            None,
            None,
        ));
        assert!(!is_retryable_error("too many tokens", None, None));
    }

    #[test]
    fn not_retryable_auth_errors() {
        assert!(!is_retryable_error("invalid api key", None, None));
        assert!(!is_retryable_error("unauthorized access", None, None));
        assert!(!is_retryable_error("permission denied", None, None));
    }

    #[test]
    fn not_retryable_empty_message() {
        assert!(!is_retryable_error("", None, None));
    }

    #[test]
    fn not_retryable_generic_error() {
        assert!(!is_retryable_error("something went wrong", None, None));
    }

    #[test]
    fn not_retryable_silent_overflow() {
        // Even if the message looks retryable, if usage > context window,
        // it's overflow, not retryable.
        assert!(!is_retryable_error(
            "500 server error",
            Some(250_000),
            Some(200_000),
        ));
    }

    #[test]
    fn retryable_case_insensitive() {
        assert!(is_retryable_error("RATE LIMIT", None, None));
        assert!(is_retryable_error("Service Unavailable", None, None));
    }

    // ─── pi_agent_rust#118: typed transient classification ──────────────

    #[test]
    fn transient_io_kinds_classified_from_type() {
        for kind in [
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::TimedOut,
        ] {
            assert!(io_kind_is_transient(kind), "{kind:?} should be transient");
        }
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::AlreadyExists,
        ] {
            assert!(
                !io_kind_is_transient(kind),
                "{kind:?} should NOT be transient"
            );
        }
    }

    #[test]
    fn typed_io_error_is_transient_without_string_match() {
        // Classified purely from the io::ErrorKind — the message contains no
        // retryable keyword, so a substring matcher would miss it.
        let reset = Error::Io(Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "opaque cause with no keywords",
        )));
        assert!(reset.is_transient());
        assert!(!is_retryable_error(&reset.to_string(), None, None));

        // Non-transient io kind is not retried.
        let denied = Error::Io(Box::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        )));
        assert!(!denied.is_transient());

        // Non-io variants have no transient io cause.
        assert!(!Error::auth("invalid key").is_transient());
        assert!(!Error::api("400 bad request").is_transient());
    }

    #[test]
    fn is_transient_walks_wrapped_io_error() {
        // io::Error wrapping another io::Error (reachable only via get_ref,
        // which Error::source deliberately skips).
        let inner = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken");
        let outer = Error::Io(Box::new(std::io::Error::other(inner)));
        assert!(outer.is_transient());
    }

    /// End-to-end: a mid-stream transient connection drop travels the REAL path
    /// a provider takes. A typed `io::Error` is wrapped by `Error::sse` at the
    /// source (the last place the kind is known), then flattened to a message
    /// string exactly as `agent.rs` does via `AssistantMessage::error_message`,
    /// and must be classified retryable by the string classifier the retry loop
    /// consults for `StopReason::Error` turns.
    #[test]
    fn sse_transient_drop_retryable_end_to_end() {
        // rustls reports a close_notify-less close as UnexpectedEof.
        let io_err = std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "tls connection closed without close_notify",
        );
        let flattened = Error::sse(&io_err).to_string();
        assert!(
            is_retryable_error(&flattened, None, None),
            "flattened transient SSE error should be retryable: {flattened}"
        );

        // A non-transient SSE failure (malformed body) is NOT retried.
        let fatal = Error::sse(&std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid utf-8 in event",
        ));
        assert!(!is_retryable_error(&fatal.to_string(), None, None));
    }

    /// The two exact strings reported in pi_agent_rust#118 are now retryable,
    /// both with the source-stamped canonical marker and as raw prose (the
    /// documented text fallback for dependency messages).
    #[test]
    fn issue_118_reported_strings_retryable() {
        assert!(is_retryable_error(
            "API error: HTTP connection closed before headers (transient connection drop)",
            None,
            None,
        ));
        assert!(is_retryable_error(
            "API error: SSE error: tls connection closed without close_notify \
             (transient connection drop)",
            None,
            None,
        ));
        // Raw dependency phrasings, no marker (prose-only fallback path).
        assert!(is_retryable_error(
            "HTTP connection closed before headers",
            None,
            None,
        ));
        assert!(is_retryable_error(
            "tls connection closed without close_notify",
            None,
            None,
        ));
    }

    mod proptest_error {
        use super::*;
        use proptest::prelude::*;

        const ALL_DIAGNOSTIC_CODES: &[AuthDiagnosticCode] = &[
            AuthDiagnosticCode::MissingApiKey,
            AuthDiagnosticCode::InvalidApiKey,
            AuthDiagnosticCode::QuotaExceeded,
            AuthDiagnosticCode::MissingOAuthAuthorizationCode,
            AuthDiagnosticCode::OAuthTokenExchangeFailed,
            AuthDiagnosticCode::OAuthTokenRefreshFailed,
            AuthDiagnosticCode::MissingAzureDeployment,
            AuthDiagnosticCode::MissingRegion,
            AuthDiagnosticCode::MissingProject,
            AuthDiagnosticCode::MissingProfile,
            AuthDiagnosticCode::MissingEndpoint,
            AuthDiagnosticCode::MissingCredentialChain,
            AuthDiagnosticCode::UnknownAuthFailure,
        ];

        proptest! {
            /// `as_str` always returns a non-empty dotted path.
            #[test]
            fn as_str_non_empty_dotted(idx in 0..13usize) {
                let code = ALL_DIAGNOSTIC_CODES[idx];
                let s = code.as_str();
                assert!(!s.is_empty());
                assert!(s.contains('.'), "diagnostic code should be dotted: {s}");
            }

            /// `as_str` values are unique across all codes.
            #[test]
            fn as_str_unique(a in 0..13usize, b in 0..13usize) {
                if a != b {
                    assert_ne!(
                        ALL_DIAGNOSTIC_CODES[a].as_str(),
                        ALL_DIAGNOSTIC_CODES[b].as_str()
                    );
                }
            }

            /// `remediation` always returns a non-empty string.
            #[test]
            fn remediation_non_empty(idx in 0..13usize) {
                let code = ALL_DIAGNOSTIC_CODES[idx];
                assert!(!code.remediation().is_empty());
            }

            /// `redaction_policy` is always `"redact-secrets"`.
            #[test]
            fn redaction_policy_constant(idx in 0..13usize) {
                let code = ALL_DIAGNOSTIC_CODES[idx];
                assert_eq!(code.redaction_policy(), "redact-secrets");
            }

            /// `hostcall_error_code` is one of the 5 known codes.
            #[test]
            fn hostcall_code_known(msg in "[a-z ]{1,20}") {
                let known = ["invalid_request", "io", "denied", "timeout", "internal"];
                let errors = [
                    Error::config(msg.clone()),
                    Error::session(msg.clone()),
                    Error::auth(msg.clone()),
                    Error::validation(msg.clone()),
                    Error::api(msg),
                ];
                for e in &errors {
                    assert!(known.contains(&e.hostcall_error_code()));
                }
            }

            /// `category_code` is a non-empty ASCII lowercase string.
            #[test]
            fn category_code_format(msg in "[a-z ]{1,20}") {
                let errors = [
                    Error::config(msg.clone()),
                    Error::session(msg.clone()),
                    Error::auth(msg.clone()),
                    Error::validation(msg.clone()),
                    Error::extension(msg.clone()),
                    Error::api(msg),
                ];
                for e in &errors {
                    let code = e.category_code();
                    assert!(!code.is_empty());
                    assert!(code.chars().all(|c| c.is_ascii_lowercase()));
                }
            }

            /// `is_context_overflow` detects token-based overflow.
            #[test]
            fn context_overflow_token_based(
                input_tokens in 100_001..500_000u64,
                window in 1..100_000u32
            ) {
                assert!(is_context_overflow(
                    "",
                    Some(input_tokens),
                    Some(window)
                ));
            }

            /// `is_context_overflow` does not fire when tokens are within window.
            #[test]
            fn context_overflow_within_window(
                window in 100..200_000u32,
                offset in 0..100u64
            ) {
                let input = u64::from(window).saturating_sub(offset);
                assert!(!is_context_overflow(
                    "some normal error",
                    Some(input),
                    Some(window)
                ));
            }

            /// `is_context_overflow` detects all substring patterns.
            #[test]
            fn context_overflow_pattern_detection(idx in 0..OVERFLOW_PATTERNS.len()) {
                let pattern = OVERFLOW_PATTERNS[idx];
                assert!(is_context_overflow(pattern, None, None));
            }

            /// `is_context_overflow` is case-insensitive for patterns.
            #[test]
            fn context_overflow_case_insensitive(idx in 0..OVERFLOW_PATTERNS.len()) {
                let pattern = OVERFLOW_PATTERNS[idx];
                assert!(is_context_overflow(&pattern.to_uppercase(), None, None));
            }

            /// `is_retryable_error` rejects empty messages.
            #[test]
            fn retryable_empty_is_false(_dummy in 0..1u8) {
                assert!(!is_retryable_error("", None, None));
            }

            /// Context overflow errors are NOT retryable.
            #[test]
            fn overflow_not_retryable(idx in 0..OVERFLOW_PATTERNS.len()) {
                let pattern = OVERFLOW_PATTERNS[idx];
                assert!(!is_retryable_error(pattern, None, None));
            }

            /// Known retryable patterns are detected.
            #[test]
            fn retryable_known_patterns(idx in 0..8usize) {
                let patterns = [
                    "overloaded",
                    "rate limit exceeded",
                    "too many requests",
                    "429 status code",
                    "502 bad gateway",
                    "503 service unavailable",
                    "connection error",
                    "fetch failed",
                ];
                assert!(is_retryable_error(patterns[idx], None, None));
            }

            /// Random gibberish is not retryable.
            #[test]
            fn random_not_retryable(s in "[a-z]{20,40}") {
                assert!(!is_retryable_error(&s, None, None));
            }

            /// Error constructors produce correct category codes.
            #[test]
            fn constructor_category_consistency(msg in "[a-z]{1,10}") {
                assert_eq!(Error::config(&msg).category_code(), "config");
                assert_eq!(Error::session(&msg).category_code(), "session");
                assert_eq!(Error::auth(&msg).category_code(), "auth");
                assert_eq!(Error::validation(&msg).category_code(), "validation");
                assert_eq!(Error::extension(&msg).category_code(), "extension");
                assert_eq!(Error::api(&msg).category_code(), "api");
            }
        }
    }
}
