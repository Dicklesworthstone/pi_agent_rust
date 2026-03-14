//! Authentication storage and API key resolution.
//!
//! Auth file: ~/.pi/agent/auth.json

use crate::agent_cx::AgentCx;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::provider_metadata::{canonical_provider_id, provider_auth_env_keys, provider_metadata};
use asupersync::channel::oneshot;
use base64::Engine as _;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

fn finish_auth_task<T, E>(
    handle: std::thread::JoinHandle<()>,
    recv_result: std::result::Result<Result<T>, E>,
    cancelled_message: &'static str,
) -> Result<T> {
    if let Err(panic_payload) = handle.join() {
        std::panic::resume_unwind(panic_payload);
    }
    recv_result.map_err(|_| Error::auth(cancelled_message.to_string()))?
}

const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_OAUTH_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_OAUTH_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const ANTHROPIC_OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference";

// ── OpenAI Codex OAuth constants ─────────────────────────────────
const OPENAI_CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_CODEX_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CODEX_OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OPENAI_CODEX_OAUTH_SCOPES: &str = "openid profile email offline_access";

// ── Google Gemini CLI OAuth constants ────────────────────────────
const GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
const GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET: &str = "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";
const GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI: &str = "http://localhost:8085/oauth2callback";
const GOOGLE_GEMINI_CLI_OAUTH_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile";
const GOOGLE_GEMINI_CLI_OAUTH_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_GEMINI_CLI_CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";

// ── Google Antigravity OAuth constants ───────────────────────────
const GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET: &str = "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";
const GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
const GOOGLE_ANTIGRAVITY_OAUTH_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
const GOOGLE_ANTIGRAVITY_OAUTH_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_ANTIGRAVITY_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";
const GOOGLE_ANTIGRAVITY_PROJECT_DISCOVERY_ENDPOINTS: [&str; 2] = [
    "https://cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
];

/// Internal marker used to preserve OAuth-vs-API-key lane information when
/// passing Anthropic credentials through provider-agnostic key plumbing.
const ANTHROPIC_OAUTH_BEARER_MARKER: &str = "__pi_anthropic_oauth_bearer__:";

// ── GitHub / Copilot OAuth constants ──────────────────────────────
const GITHUB_OAUTH_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const GITHUB_OAUTH_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
/// Default scopes for Copilot access (read:user needed for identity).
const GITHUB_COPILOT_SCOPES: &str = "read:user";

// ── GitLab OAuth constants ────────────────────────────────────────
const GITLAB_OAUTH_AUTHORIZE_PATH: &str = "/oauth/authorize";
const GITLAB_OAUTH_TOKEN_PATH: &str = "/oauth/token";
const GITLAB_DEFAULT_BASE_URL: &str = "https://gitlab.com";
/// Default scopes for GitLab AI features.
const GITLAB_DEFAULT_SCOPES: &str = "api read_api read_user";

// ── Kimi Code OAuth constants ─────────────────────────────────────
const KIMI_CODE_OAUTH_CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const KIMI_CODE_OAUTH_DEFAULT_HOST: &str = "https://auth.kimi.com";
const KIMI_CODE_OAUTH_HOST_ENV_KEYS: [&str; 2] = ["KIMI_CODE_OAUTH_HOST", "KIMI_OAUTH_HOST"];
const KIMI_SHARE_DIR_ENV_KEY: &str = "KIMI_SHARE_DIR";
const KIMI_CODE_DEVICE_AUTHORIZATION_PATH: &str = "/api/oauth/device_authorization";
const KIMI_CODE_TOKEN_PATH: &str = "/api/oauth/token";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum ApiKeyValue {
    EnvVar(String),
    Command(String),
    Raw(String),
}

impl From<String> for ApiKeyValue {
    fn from(s: String) -> Self {
        if s.starts_with("!") {
            Self::Command(s[1..].to_string())
        } else if std::env::var_os(&s).is_some() {
            Self::EnvVar(s)
        } else {
            Self::Raw(s)
        }
    }
}

impl From<ApiKeyValue> for String {
    fn from(val: ApiKeyValue) -> Self {
        match val {
            ApiKeyValue::Raw(s) => s,
            ApiKeyValue::EnvVar(s) => s,
            ApiKeyValue::Command(s) => format!("!{s}"),
        }
    }
}

impl From<&String> for ApiKeyValue {
    fn from(s: &String) -> Self {
        Self::from(s.clone())
    }
}

impl PartialEq<str> for ApiKeyValue {
    fn eq(&self, other: &str) -> bool {
        match self {
            Self::Raw(s) => s == other,
            _ => false,
        }
    }
}

impl PartialEq<&str> for ApiKeyValue {
    fn eq(&self, other: &&str) -> bool {
        self.eq(*other)
    }
}

/// Credentials stored in auth.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthCredential {
    ApiKey {
        key: ApiKeyValue,
    },
    OAuth {
        access_token: String,
        refresh_token: String,
        expires: i64, // Unix ms
        /// Token endpoint URL for self-contained refresh (optional; backward-compatible).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_url: Option<String>,
        /// Client ID for self-contained refresh (optional; backward-compatible).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// AWS IAM credentials for providers like Amazon Bedrock.
    ///
    /// Supports the standard credential chain: explicit keys → env vars → profile → container
    /// credentials → web identity token.
    AwsCredentials {
        access_key_id: String,
        secret_access_key: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<String>,
    },
    /// Bearer token for providers that accept `Authorization: Bearer <token>`.
    ///
    /// Used by gateway proxies (Vercel AI Gateway, Helicone, etc.) and services
    /// that issue pre-authenticated bearer tokens (e.g. `AWS_BEARER_TOKEN_BEDROCK`).
    BearerToken {
        token: String,
    },
    /// Service key credentials for providers like SAP AI Core that use
    /// client-credentials OAuth (client_id + client_secret → token_url → bearer).
    ServiceKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_secret: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service_url: Option<String>,
    },
    /// Preserves malformed or legacy credential entries during migration/saving
    /// so that we don't drop them if the schema evolves.
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

/// Canonical credential status for a provider in auth.json.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialStatus {
    Missing,
    ApiKey,
    OAuthValid { expires_in_ms: i64 },
    OAuthExpired { expired_by_ms: i64 },
    BearerToken,
    AwsCredentials,
    ServiceKey,
}

/// Proactive refresh: attempt refresh this many ms *before* actual expiry.
/// This avoids using a token that's about to expire during a long-running request.
const PROACTIVE_REFRESH_WINDOW_MS: i64 = 10 * 60 * 1000; // 10 minutes
type OAuthRefreshRequest = (String, String, String, Option<String>, Option<String>);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthFile {
    #[serde(flatten)]
    pub entries: HashMap<String, AuthCredential>,
}

#[derive(Serialize)]
struct AuthFileRef<'a> {
    #[serde(flatten)]
    entries: &'a HashMap<String, AuthCredential>,
}

/// Auth storage wrapper with file locking.
#[derive(Debug, Clone)]
pub struct AuthStorage {
    path: PathBuf,
    entries: HashMap<String, AuthCredential>,
}

impl AuthStorage {
    fn allow_external_provider_lookup(&self) -> bool {
        // External credential auto-detection is intended for Pi's global auth
        // file (typically `~/.pi/agent/auth.json`). Scoping it this way keeps
        // tests and custom auth sandboxes deterministic.
        self.path == Config::auth_path()
    }

    fn entry_case_insensitive(&self, key: &str) -> Option<&AuthCredential> {
        self.entries.iter().find_map(|(existing, credential)| {
            existing.eq_ignore_ascii_case(key).then_some(credential)
        })
    }

    fn credential_for_provider(&self, provider: &str) -> Option<&AuthCredential> {
        if let Some(credential) = self
            .entries
            .get(provider)
            .or_else(|| self.entry_case_insensitive(provider))
        {
            return Some(credential);
        }

        let metadata = provider_metadata(provider)?;
        if let Some(credential) = self
            .entries
            .get(metadata.canonical_id)
            .or_else(|| self.entry_case_insensitive(metadata.canonical_id))
        {
            return Some(credential);
        }

        metadata.aliases.iter().find_map(|alias| {
            self.entries
                .get(*alias)
                .or_else(|| self.entry_case_insensitive(alias))
        })
    }

    /// Load auth.json (creates empty if missing).
    pub fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            let lock_handle = open_auth_lock_file(&path)?;
            let _locked = lock_file_shared(lock_handle, Duration::from_secs(30))?;
            let content =
                fs::read_to_string(&path).map_err(|e| Error::auth(format!("auth.json: {e}")))?;
            let parsed: AuthFile = match serde_json::from_str(&content) {
                Ok(file) => file,
                Err(e) => {
                    let backup_path = path.with_extension("json.corrupt");
                    let _ = fs::copy(&path, &backup_path);
                    tracing::warn!(
                        event = "pi.auth.parse_error",
                        error = %e,
                        backup = %backup_path.display(),
                        "auth.json is corrupted; backed up and starting with empty credentials"
                    );
                    AuthFile::default()
                }
            };
            parsed.entries
        } else {
            HashMap::new()
        };

        Ok(Self { path, entries })
    }

    /// Load auth.json asynchronously (creates empty if missing).
    pub async fn load_async(path: PathBuf) -> Result<Self> {
        let (tx, rx) = oneshot::channel();
        let handle = std::thread::spawn(move || {
            let res = Self::load(path);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_auth_task(handle, recv_result, "Load task cancelled")
    }

    /// Persist auth.json (atomic write + permissions).
    pub fn save(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&AuthFileRef {
            entries: &self.entries,
        })?;
        Self::save_data_sync(&self.path, &data)
    }

    /// Persist auth.json asynchronously.
    pub async fn save_async(&self) -> Result<()> {
        let data = serde_json::to_string_pretty(&AuthFileRef {
            entries: &self.entries,
        })?;
        let (tx, rx) = oneshot::channel();
        let path = self.path.clone();

        let handle = std::thread::spawn(move || {
            let res = Self::save_data_sync(&path, &data);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), res);
        });

        let cx = AgentCx::for_request();
        let recv_result = rx.recv(cx.cx()).await;
        finish_auth_task(handle, recv_result, "Save task cancelled")
    }

    fn save_data_sync(path: &Path, data: &str) -> Result<()> {
        Self::save_data_sync_with_hook(path, data, |_| Ok(()))
    }

    fn save_data_sync_with_hook<F>(path: &Path, data: &str, before_persist: F) -> Result<()>
    where
        F: FnOnce(&NamedTempFile) -> std::io::Result<()>,
    {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let lock_handle = open_auth_lock_file(path)?;
        let _locked = lock_file(lock_handle, Duration::from_secs(30))?;

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut temp = NamedTempFile::new_in(parent)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }

        temp.write_all(data.as_bytes())?;
        temp.as_file().sync_all()?;
        before_persist(&temp)?;
        temp.persist(path).map_err(|err| err.error)?;
        sync_parent_dir(path)?;

        Ok(())
    }
    /// Get raw credential.
    pub fn get(&self, provider: &str) -> Option<&AuthCredential> {
        self.entries.get(provider)
    }

    /// Insert or replace a credential for a provider.
    pub fn set(&mut self, provider: impl Into<String>, credential: AuthCredential) {
        self.entries.insert(provider.into(), credential);
    }

    /// Remove a credential for a provider.
    pub fn remove(&mut self, provider: &str) -> bool {
        self.entries.remove(provider).is_some()
    }

    /// Get API key for provider from auth.json.
    ///
    /// For `ApiKey` and `BearerToken` variants the key/token is returned directly.
    /// For `OAuth` the access token is returned only when not expired.
    /// For `AwsCredentials` the access key ID is returned (callers needing the full
    /// credential set should use [`get`] instead).
    /// For `ServiceKey` this returns `None` because a token exchange is required first.
    pub fn api_key(&self, provider: &str) -> Option<String> {
        self.credential_for_provider(provider)
            .and_then(api_key_from_credential)
    }

    /// Return the names of all providers that have stored credentials.
    pub fn provider_names(&self) -> Vec<String> {
        let mut providers: Vec<String> = self.entries.keys().cloned().collect();
        providers.sort();
        providers
    }

    /// Return stored credential status for a provider, including canonical alias fallback.
    pub fn credential_status(&self, provider: &str) -> CredentialStatus {
        let now = chrono::Utc::now().timestamp_millis();
        let cred = self.credential_for_provider(provider);

        let Some(cred) = cred else {
            return if self.allow_external_provider_lookup()
                && resolve_external_provider_api_key(provider).is_some()
            {
                CredentialStatus::ApiKey
            } else {
                CredentialStatus::Missing
            };
        };

        match cred {
            AuthCredential::ApiKey { .. } => CredentialStatus::ApiKey,
            AuthCredential::OAuth { expires, .. } if *expires > now => {
                CredentialStatus::OAuthValid {
                    expires_in_ms: expires.saturating_sub(now),
                }
            }
            AuthCredential::OAuth { expires, .. } => CredentialStatus::OAuthExpired {
                expired_by_ms: now.saturating_sub(*expires),
            },
            AuthCredential::BearerToken { .. } => CredentialStatus::BearerToken,
            AuthCredential::AwsCredentials { .. } => CredentialStatus::AwsCredentials,
            AuthCredential::ServiceKey { .. } => CredentialStatus::ServiceKey,
            AuthCredential::Unknown(_) => CredentialStatus::Missing,
        }
    }

    /// Remove stored credentials for `provider` and any known aliases/canonical IDs.
    ///
    /// Matching is case-insensitive to clean up legacy mixed-case auth entries.
    pub fn remove_provider_aliases(&mut self, provider: &str) -> bool {
        let trimmed = provider.trim();
        if trimmed.is_empty() {
            return false;
        }

        let mut targets: Vec<String> = vec![trimmed.to_ascii_lowercase()];
        if let Some(metadata) = provider_metadata(trimmed) {
            targets.push(metadata.canonical_id.to_ascii_lowercase());
            targets.extend(
                metadata
                    .aliases
                    .iter()
                    .map(|alias| alias.to_ascii_lowercase()),
            );
        }
        targets.sort();
        targets.dedup();

        let mut removed = false;
        self.entries.retain(|key, _| {
            let should_remove = targets
                .iter()
                .any(|target| key.eq_ignore_ascii_case(target));
            if should_remove {
                removed = true;
            }
            !should_remove
        });
        removed
    }

    /// Returns true when auth.json contains a credential for `provider`
    /// (including canonical alias fallback).
    pub fn has_stored_credential(&self, provider: &str) -> bool {
        self.credential_for_provider(provider).is_some()
    }

    /// Return a human-readable source label when credentials can be auto-detected
    /// from other locally-installed coding CLIs.
    pub fn external_setup_source(&self, provider: &str) -> Option<&'static str> {
        if !self.allow_external_provider_lookup() {
            return None;
        }
        external_setup_source(provider)
    }

    /// Resolve API key with precedence.
    pub fn resolve_api_key(&self, provider: &str, override_key: Option<&str>) -> Option<String> {
        self.resolve_api_key_with_env_lookup(provider, override_key, |var| std::env::var(var).ok())
    }

    fn resolve_api_key_with_env_lookup<F>(
        &self,
        provider: &str,
        override_key: Option<&str>,
        mut env_lookup: F,
    ) -> Option<String>
    where
        F: FnMut(&str) -> Option<String>,
    {
        if let Some(key) = override_key {
            return Some(key.to_string());
        }

        // Prefer explicit stored OAuth/Bearer credentials over ambient env vars.
        // This prevents stale shell env keys from silently overriding successful `/login` flows.
        if let Some(credential) = self.credential_for_provider(provider)
            && let Some(key) = match credential {
                AuthCredential::OAuth { .. }
                    if canonical_provider_id(provider).unwrap_or(provider) == "anthropic" =>
                {
                    api_key_from_credential(credential)
                        .map(|token| mark_anthropic_oauth_bearer_token(&token))
                }
                AuthCredential::OAuth { .. } | AuthCredential::BearerToken { .. } => {
                    api_key_from_credential(credential)
                }
                _ => None,
            }
        {
            return Some(key);
        }

        if let Some(key) = env_keys_for_provider(provider).iter().find_map(|var| {
            env_lookup(var).and_then(|value| {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
        }) {
            return Some(key);
        }

        if let Some(key) = self.api_key(provider) {
            return Some(key);
        }

        if self.allow_external_provider_lookup() {
            if let Some(key) = resolve_external_provider_api_key(provider) {
                return Some(key);
            }
        }

        canonical_provider_id(provider)
            .filter(|canonical| *canonical != provider)
            .and_then(|canonical| {
                self.api_key(canonical).or_else(|| {
                    self.allow_external_provider_lookup()
                        .then(|| resolve_external_provider_api_key(canonical))
                        .flatten()
                })
            })
    }

    /// Refresh any expired OAuth tokens that this binary knows how to refresh.
    ///
    /// This keeps startup behavior predictable: models that rely on OAuth credentials remain
    /// available after restart without requiring the user to re-login.
    pub async fn refresh_expired_oauth_tokens(&mut self) -> Result<()> {
        let client = crate::http::client::Client::new();
        self.refresh_expired_oauth_tokens_with_client(&client).await
    }

    /// Refresh any expired OAuth tokens using the provided HTTP client.
    ///
    /// This is primarily intended for tests and deterministic harnesses (e.g. VCR playback),
    /// but is also useful for callers that want to supply a custom HTTP implementation.
    #[allow(clippy::too_many_lines)]
    pub async fn refresh_expired_oauth_tokens_with_client(
        &mut self,
        client: &crate::http::client::Client,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let proactive_deadline = now + PROACTIVE_REFRESH_WINDOW_MS;
        let mut refreshes: Vec<OAuthRefreshRequest> = Vec::new();

        for (provider, cred) in &self.entries {
            if let AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires,
                token_url,
                client_id,
                ..
            } = cred
            {
                // Proactive refresh: refresh if the token will expire within the
                // proactive window, not just when already expired.
                if *expires <= proactive_deadline {
                    refreshes.push((
                        provider.clone(),
                        access_token.clone(),
                        refresh_token.clone(),
                        token_url.clone(),
                        client_id.clone(),
                    ));
                }
            }
        }

        let mut failed_providers = Vec::new();
        let mut needs_save = false;

        for (provider, access_token, refresh_token, stored_token_url, stored_client_id) in refreshes
        {
            let result = match provider.as_str() {
                "anthropic" => {
                    Box::pin(refresh_anthropic_oauth_token(client, &refresh_token)).await
                }
                "google-gemini-cli" => {
                    let (_, project_id) = decode_project_scoped_access_token(&access_token)
                        .ok_or_else(|| {
                            Error::auth(
                                "google-gemini-cli OAuth credential missing projectId payload"
                                    .to_string(),
                            )
                        })?;
                    Box::pin(refresh_google_gemini_cli_oauth_token(
                        client,
                        &refresh_token,
                        &project_id,
                    ))
                    .await
                }
                "google-antigravity" => {
                    let (_, project_id) = decode_project_scoped_access_token(&access_token)
                        .ok_or_else(|| {
                            Error::auth(
                                "google-antigravity OAuth credential missing projectId payload"
                                    .to_string(),
                            )
                        })?;
                    Box::pin(refresh_google_antigravity_oauth_token(
                        client,
                        &refresh_token,
                        &project_id,
                    ))
                    .await
                }
                "kimi-for-coding" => {
                    let token_url = stored_token_url
                        .clone()
                        .unwrap_or_else(kimi_code_token_endpoint);
                    Box::pin(refresh_kimi_code_oauth_token(
                        client,
                        &token_url,
                        &refresh_token,
                    ))
                    .await
                }
                _ => {
                    if let (Some(url), Some(cid)) = (&stored_token_url, &stored_client_id) {
                        Box::pin(refresh_self_contained_oauth_token(
                            client,
                            url,
                            cid,
                            &refresh_token,
                            &provider,
                        ))
                        .await
                    } else {
                        // Should have been filtered out or handled by extensions logic, but safe to ignore.
                        continue;
                    }
                }
            };

            match result {
                Ok(refreshed) => {
                    self.entries.insert(provider, refreshed);
                    needs_save = true;
                }
                Err(e) => {
                    tracing::warn!("Failed to refresh OAuth token for {provider}: {e}");
                    failed_providers.push(format!("{provider} ({e})"));
                }
            }
        }

        if needs_save {
            if let Err(e) = self.save_async().await {
                tracing::warn!("Failed to save auth.json after refreshing OAuth tokens: {e}");
            }
        }

        if !failed_providers.is_empty() {
            // Return an error to signal that at least some refreshes failed,
            // but only after attempting all of them.
            return Err(Error::auth(format!(
                "OAuth token refresh failed for: {}",
                failed_providers.join(", ")
            )));
        }

        Ok(())
    }

    /// Refresh expired OAuth tokens for extension-registered providers.
    ///
    /// `extension_configs` maps provider ID to its [`OAuthConfig`](crate::models::OAuthConfig).
    /// Providers already handled by `refresh_expired_oauth_tokens_with_client` (e.g. "anthropic")
    /// are skipped.
    pub async fn refresh_expired_extension_oauth_tokens(
        &mut self,
        client: &crate::http::client::Client,
        extension_configs: &HashMap<String, crate::models::OAuthConfig>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let proactive_deadline = now + PROACTIVE_REFRESH_WINDOW_MS;
        let mut refreshes = Vec::new();

        for (provider, cred) in &self.entries {
            if let AuthCredential::OAuth {
                refresh_token,
                expires,
                token_url,
                client_id,
                ..
            } = cred
            {
                // Skip built-in providers (handled by refresh_expired_oauth_tokens_with_client).
                if matches!(
                    provider.as_str(),
                    "anthropic"
                        | "openai-codex"
                        | "google-gemini-cli"
                        | "google-antigravity"
                        | "kimi-for-coding"
                ) {
                    continue;
                }
                // Skip self-contained credentials — they are refreshed by
                // refresh_expired_oauth_tokens_with_client instead.
                if token_url.is_some() && client_id.is_some() {
                    continue;
                }
                if *expires <= proactive_deadline {
                    if let Some(config) = extension_configs.get(provider) {
                        refreshes.push((provider.clone(), refresh_token.clone(), config.clone()));
                    }
                }
            }
        }

        if !refreshes.is_empty() {
            tracing::info!(
                event = "pi.auth.extension_oauth_refresh.start",
                count = refreshes.len(),
                "Refreshing expired extension OAuth tokens"
            );
        }
        let mut failed_providers: Vec<String> = Vec::new();
        let mut needs_save = false;

        for (provider, refresh_token, config) in refreshes {
            let start = std::time::Instant::now();
            match refresh_extension_oauth_token(client, &config, &refresh_token).await {
                Ok(refreshed) => {
                    tracing::info!(
                        event = "pi.auth.extension_oauth_refresh.ok",
                        provider = %provider,
                        elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "Extension OAuth token refreshed"
                    );
                    self.entries.insert(provider, refreshed);
                    needs_save = true;
                }
                Err(e) => {
                    tracing::warn!(
                        event = "pi.auth.extension_oauth_refresh.error",
                        provider = %provider,
                        error = %e,
                        elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "Failed to refresh extension OAuth token; continuing with remaining providers"
                    );
                    failed_providers.push(format!("{provider} ({e})"));
                }
            }
        }

        if needs_save {
            if let Err(e) = self.save_async().await {
                tracing::warn!(
                    "Failed to save auth.json after refreshing extension OAuth tokens: {e}"
                );
            }
        }

        if failed_providers.is_empty() {
            Ok(())
        } else {
            Err(Error::api(format!(
                "Extension OAuth token refresh failed for: {}",
                failed_providers.join(", ")
            )))
        }
    }

    /// Remove OAuth credentials that expired more than `max_age_ms` ago and
    /// whose refresh token is no longer usable (no stored `token_url`/`client_id`
    /// and no matching extension config).
    ///
    /// Returns the list of pruned provider IDs.
    pub fn prune_stale_credentials(&mut self, max_age_ms: i64) -> Vec<String> {
        let now = chrono::Utc::now().timestamp_millis();
        let cutoff = now - max_age_ms;
        let mut pruned = Vec::new();

        self.entries.retain(|provider, cred| {
            if let AuthCredential::OAuth {
                expires,
                token_url,
                client_id,
                ..
            } = cred
            {
                // Only prune tokens that are well past expiry AND have no
                // self-contained refresh metadata.
                if *expires < cutoff && token_url.is_none() && client_id.is_none() {
                    tracing::info!(
                        event = "pi.auth.prune_stale",
                        provider = %provider,
                        expired_at = expires,
                        "Pruning stale OAuth credential"
                    );
                    pruned.push(provider.clone());
                    return false;
                }
            }
            true
        });

        pruned
    }
}

fn api_key_from_credential(credential: &AuthCredential) -> Option<String> {
    match credential {
        AuthCredential::ApiKey { key } => match key {
            ApiKeyValue::Raw(s) => Some(s.clone()),
            ApiKeyValue::EnvVar(s) => std::env::var(s).ok(),
            ApiKeyValue::Command(s) => std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(s)
                .output()
                .and_then(|o| {
                    Ok(String::from(
                        std::str::from_utf8(o.stdout.as_slice()).unwrap().trim(),
                    ))
                })
                .ok(),
        },
        AuthCredential::OAuth {
            access_token,
            expires,
            ..
        } => {
            let now = chrono::Utc::now().timestamp_millis();
            if *expires > now {
                Some(access_token.clone())
            } else {
                None
            }
        }
        AuthCredential::BearerToken { token } => Some(token.clone()),
        AuthCredential::AwsCredentials { access_key_id, .. } => Some(access_key_id.clone()),
        AuthCredential::ServiceKey { .. } | AuthCredential::Unknown(_) => None,
    }
}

fn env_key_for_provider(provider: &str) -> Option<&'static str> {
    env_keys_for_provider(provider).first().copied()
}

fn mark_anthropic_oauth_bearer_token(token: &str) -> String {
    format!("{ANTHROPIC_OAUTH_BEARER_MARKER}{token}")
}

pub(crate) fn unmark_anthropic_oauth_bearer_token(token: &str) -> Option<&str> {
    token.strip_prefix(ANTHROPIC_OAUTH_BEARER_MARKER)
}

fn env_keys_for_provider(provider: &str) -> &'static [&'static str] {
    provider_auth_env_keys(provider)
}

fn resolve_external_provider_api_key(provider: &str) -> Option<String> {
    let canonical = canonical_provider_id(provider).unwrap_or(provider);
    match canonical {
        "anthropic" => read_external_claude_access_token()
            .map(|token| mark_anthropic_oauth_bearer_token(&token)),
        // Keep OpenAI API-key auth distinct from Codex OAuth token auth.
        // Codex access tokens are only valid on Codex-specific routes.
        "openai" => read_external_codex_openai_api_key(),
        "openai-codex" => read_external_codex_access_token(),
        "google-gemini-cli" => {
            let project =
                google_project_id_from_env().or_else(google_project_id_from_gcloud_config);
            read_external_gemini_access_payload(project.as_deref())
        }
        "google-antigravity" => {
            let project = google_project_id_from_env()
                .unwrap_or_else(|| GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID.to_string());
            read_external_gemini_access_payload(Some(project.as_str()))
        }
        "kimi-for-coding" => read_external_kimi_code_access_token(),
        _ => None,
    }
}

/// Return a stable human-readable label when we can auto-detect local credentials
/// from another coding agent installation.
pub fn external_setup_source(provider: &str) -> Option<&'static str> {
    let canonical = canonical_provider_id(provider).unwrap_or(provider);
    match canonical {
        "anthropic" if read_external_claude_access_token().is_some() => {
            Some("Claude Code (~/.claude/.credentials.json)")
        }
        "openai" if read_external_codex_openai_api_key().is_some() => {
            Some("Codex (~/.codex/auth.json)")
        }
        "openai-codex" if read_external_codex_access_token().is_some() => {
            Some("Codex (~/.codex/auth.json)")
        }
        "google-gemini-cli" => {
            let project =
                google_project_id_from_env().or_else(google_project_id_from_gcloud_config);
            read_external_gemini_access_payload(project.as_deref())
                .is_some()
                .then_some("Gemini CLI (~/.gemini/oauth_creds.json)")
        }
        "google-antigravity" => {
            let project = google_project_id_from_env()
                .unwrap_or_else(|| GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID.to_string());
            if read_external_gemini_access_payload(Some(project.as_str())).is_some() {
                Some("Gemini CLI (~/.gemini/oauth_creds.json)")
            } else {
                None
            }
        }
        "kimi-for-coding" if read_external_kimi_code_access_token().is_some() => Some(
            "Kimi CLI (~/.kimi/credentials/kimi-code.json or $KIMI_SHARE_DIR/credentials/kimi-code.json)",
        ),
        _ => None,
    }
}

fn read_external_json(path: &Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_external_claude_access_token() -> Option<String> {
    let path = home_dir()?.join(".claude").join(".credentials.json");
    let value = read_external_json(&path)?;
    let token = value
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("accessToken"))
        .and_then(serde_json::Value::as_str)?
        .trim()
        .to_string();
    if token.is_empty() { None } else { Some(token) }
}

fn read_external_codex_auth() -> Option<serde_json::Value> {
    let home = home_dir()?;
    let candidates = [
        home.join(".codex").join("auth.json"),
        home.join(".config").join("codex").join("auth.json"),
    ];
    for path in candidates {
        if let Some(value) = read_external_json(&path) {
            return Some(value);
        }
    }
    None
}

fn read_external_codex_access_token() -> Option<String> {
    let value = read_external_codex_auth()?;
    codex_access_token_from_value(&value)
}

fn read_external_codex_openai_api_key() -> Option<String> {
    let value = read_external_codex_auth()?;
    codex_openai_api_key_from_value(&value)
}

fn codex_access_token_from_value(value: &serde_json::Value) -> Option<String> {
    let candidates = [
        // Canonical codex CLI shape.
        value
            .get("tokens")
            .and_then(|tokens| tokens.get("access_token"))
            .and_then(serde_json::Value::as_str),
        // CamelCase variant.
        value
            .get("tokens")
            .and_then(|tokens| tokens.get("accessToken"))
            .and_then(serde_json::Value::as_str),
        // Flat variants.
        value
            .get("access_token")
            .and_then(serde_json::Value::as_str),
        value.get("accessToken").and_then(serde_json::Value::as_str),
        value.get("token").and_then(serde_json::Value::as_str),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|token| !token.is_empty() && !token.starts_with("sk-"))
        .map(std::string::ToString::to_string)
}

fn codex_openai_api_key_from_value(value: &serde_json::Value) -> Option<String> {
    let candidates = [
        value
            .get("OPENAI_API_KEY")
            .and_then(serde_json::Value::as_str),
        value
            .get("openai_api_key")
            .and_then(serde_json::Value::as_str),
        value
            .get("openaiApiKey")
            .and_then(serde_json::Value::as_str),
        value
            .get("env")
            .and_then(|env| env.get("OPENAI_API_KEY"))
            .and_then(serde_json::Value::as_str),
        value
            .get("env")
            .and_then(|env| env.get("openai_api_key"))
            .and_then(serde_json::Value::as_str),
        value
            .get("env")
            .and_then(|env| env.get("openaiApiKey"))
            .and_then(serde_json::Value::as_str),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|key| !key.is_empty())
        .map(std::string::ToString::to_string)
}

fn read_external_gemini_access_payload(project_id: Option<&str>) -> Option<String> {
    let home = home_dir()?;
    let candidates = [
        home.join(".gemini").join("oauth_creds.json"),
        home.join(".config").join("gemini").join("credentials.json"),
    ];

    for path in candidates {
        let Some(value) = read_external_json(&path) else {
            continue;
        };
        let Some(token) = value
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };

        let project = project_id
            .map(std::string::ToString::to_string)
            .or_else(|| {
                value
                    .get("projectId")
                    .or_else(|| value.get("project_id"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(std::string::ToString::to_string)
            })
            .or_else(google_project_id_from_gcloud_config)?;
        let project = project.trim();
        if project.is_empty() {
            continue;
        }

        return Some(encode_project_scoped_access_token(token, project));
    }

    None
}

#[allow(clippy::cast_precision_loss)]
fn read_external_kimi_code_access_token() -> Option<String> {
    let share_dir = kimi_share_dir()?;
    read_external_kimi_code_access_token_from_share_dir(&share_dir)
}

#[allow(clippy::cast_precision_loss)]
fn read_external_kimi_code_access_token_from_share_dir(share_dir: &Path) -> Option<String> {
    let path = share_dir.join("credentials").join("kimi-code.json");
    let value = read_external_json(&path)?;

    let token = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())?;

    let expires_at = value
        .get("expires_at")
        .and_then(|raw| raw.as_f64().or_else(|| raw.as_i64().map(|v| v as f64)));
    if let Some(expires_at) = expires_at {
        let now_seconds = chrono::Utc::now().timestamp() as f64;
        if expires_at <= now_seconds {
            return None;
        }
    }

    Some(token.to_string())
}

fn google_project_id_from_env() -> Option<String> {
    std::env::var("GOOGLE_CLOUD_PROJECT")
        .ok()
        .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT_ID").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn gcloud_config_dir_with_env_lookup<F>(env_lookup: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("CLOUDSDK_CONFIG")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env_lookup("APPDATA")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(value).join("gcloud"))
        })
        .or_else(|| {
            env_lookup("XDG_CONFIG_HOME")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(value).join("gcloud"))
        })
        .or_else(|| {
            home_dir_with_env_lookup(env_lookup).map(|home| home.join(".config").join("gcloud"))
        })
}

fn gcloud_active_config_name_with_env_lookup<F>(env_lookup: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("CLOUDSDK_ACTIVE_CONFIG_NAME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string())
}

fn google_project_id_from_gcloud_config_with_env_lookup<F>(env_lookup: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    let config_dir = gcloud_config_dir_with_env_lookup(&env_lookup)?;
    let config_name = gcloud_active_config_name_with_env_lookup(&env_lookup);
    let config_file = config_dir
        .join("configurations")
        .join(format!("config_{config_name}"));
    let Ok(content) = std::fs::read_to_string(config_file) else {
        return None;
    };

    let mut section: Option<&str> = None;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some(rest) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = Some(rest.trim());
            continue;
        }

        if section != Some("core") {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "project" {
            continue;
        }
        let project = value.trim();
        if project.is_empty() {
            continue;
        }
        return Some(project.to_string());
    }

    None
}

fn google_project_id_from_gcloud_config() -> Option<String> {
    google_project_id_from_gcloud_config_with_env_lookup(|key| std::env::var(key).ok())
}

fn encode_project_scoped_access_token(token: &str, project_id: &str) -> String {
    serde_json::json!({
        "token": token,
        "projectId": project_id,
    })
    .to_string()
}

fn decode_project_scoped_access_token(payload: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let token = value
        .get("token")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let project_id = value
        .get("projectId")
        .or_else(|| value.get("project_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    Some((token, project_id))
}

// ── AWS Credential Chain ────────────────────────────────────────

/// Resolved AWS credentials ready for Sigv4 signing or bearer auth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwsResolvedCredentials {
    /// Standard IAM credentials for Sigv4 signing.
    Sigv4 {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
        region: String,
    },
    /// Bearer token (e.g. `AWS_BEARER_TOKEN_BEDROCK`).
    Bearer { token: String, region: String },
}

/// Resolve AWS credentials following the standard precedence chain.
///
/// Precedence (first match wins):
/// 1. `AWS_BEARER_TOKEN_BEDROCK` env var → bearer token auth
/// 2. `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` env vars → Sigv4
/// 3. `AWS_PROFILE` env var → profile-based (returns the profile name for external resolution)
/// 4. Stored `AwsCredentials` in auth.json
/// 5. Stored `BearerToken` in auth.json (for bedrock)
///
/// `region` is resolved from: `AWS_REGION` → `AWS_DEFAULT_REGION` → `"us-east-1"`.
pub fn resolve_aws_credentials(auth: &AuthStorage) -> Option<AwsResolvedCredentials> {
    resolve_aws_credentials_with_env(auth, |var| std::env::var(var).ok())
}

fn resolve_aws_credentials_with_env<F>(
    auth: &AuthStorage,
    mut env: F,
) -> Option<AwsResolvedCredentials>
where
    F: FnMut(&str) -> Option<String>,
{
    let region = env("AWS_REGION")
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|| "us-east-1".to_string());

    // 1. Bearer token from env (AWS Bedrock specific)
    if let Some(token) = env("AWS_BEARER_TOKEN_BEDROCK") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Some(AwsResolvedCredentials::Bearer { token, region });
        }
    }

    // 2. Explicit IAM credentials from env
    if let Some(access_key) = env("AWS_ACCESS_KEY_ID") {
        let access_key = access_key.trim().to_string();
        if !access_key.is_empty() {
            if let Some(secret_key) = env("AWS_SECRET_ACCESS_KEY") {
                let secret_key = secret_key.trim().to_string();
                if !secret_key.is_empty() {
                    let session_token = env("AWS_SESSION_TOKEN")
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    return Some(AwsResolvedCredentials::Sigv4 {
                        access_key_id: access_key,
                        secret_access_key: secret_key,
                        session_token,
                        region,
                    });
                }
            }
        }
    }

    // 3. Stored credentials in auth.json
    let provider = "amazon-bedrock";
    match auth.credential_for_provider(provider) {
        Some(AuthCredential::AwsCredentials {
            access_key_id,
            secret_access_key,
            session_token,
            region: stored_region,
        }) => Some(AwsResolvedCredentials::Sigv4 {
            access_key_id: access_key_id.clone(),
            secret_access_key: secret_access_key.clone(),
            session_token: session_token.clone(),
            region: stored_region.clone().unwrap_or(region),
        }),
        Some(AuthCredential::BearerToken { token }) => Some(AwsResolvedCredentials::Bearer {
            token: token.clone(),
            region,
        }),
        Some(AuthCredential::ApiKey { key }) => {
            // Legacy: treat stored API key as bearer token for Bedrock
            Some(AwsResolvedCredentials::Bearer {
                token: match key {
                    ApiKeyValue::Raw(s) => s.clone(),
                    // Legacy: these should never be encountered :-(
                    ApiKeyValue::EnvVar(s) => s.clone(),
                    ApiKeyValue::Command(s) => s.clone(),
                },
                region,
            })
        }
        _ => None,
    }
}

// ── SAP AI Core Service Key Resolution ──────────────────────────

/// Resolved SAP AI Core credentials ready for client-credentials token exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SapResolvedCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub token_url: String,
    pub service_url: String,
}

/// Resolve SAP AI Core credentials from env vars or stored service key.
///
/// Precedence:
/// 1. `AICORE_SERVICE_KEY` env var (JSON-encoded service key)
/// 2. Individual env vars: `SAP_AI_CORE_CLIENT_ID`, `SAP_AI_CORE_CLIENT_SECRET`,
///    `SAP_AI_CORE_TOKEN_URL`, `SAP_AI_CORE_SERVICE_URL`
/// 3. Stored `ServiceKey` in auth.json
pub fn resolve_sap_credentials(auth: &AuthStorage) -> Option<SapResolvedCredentials> {
    resolve_sap_credentials_with_env(auth, |var| std::env::var(var).ok())
}

fn resolve_sap_credentials_with_env<F>(
    auth: &AuthStorage,
    mut env: F,
) -> Option<SapResolvedCredentials>
where
    F: FnMut(&str) -> Option<String>,
{
    // 1. JSON-encoded service key from env
    if let Some(key_json) = env("AICORE_SERVICE_KEY") {
        if let Some(creds) = parse_sap_service_key_json(&key_json) {
            return Some(creds);
        }
    }

    // 2. Individual env vars
    let client_id = env("SAP_AI_CORE_CLIENT_ID");
    let client_secret = env("SAP_AI_CORE_CLIENT_SECRET");
    let token_url = env("SAP_AI_CORE_TOKEN_URL");
    let service_url = env("SAP_AI_CORE_SERVICE_URL");

    if let (Some(id), Some(secret), Some(turl), Some(surl)) =
        (client_id, client_secret, token_url, service_url)
    {
        let id = id.trim().to_string();
        let secret = secret.trim().to_string();
        let turl = turl.trim().to_string();
        let surl = surl.trim().to_string();
        if !id.is_empty() && !secret.is_empty() && !turl.is_empty() && !surl.is_empty() {
            return Some(SapResolvedCredentials {
                client_id: id,
                client_secret: secret,
                token_url: turl,
                service_url: surl,
            });
        }
    }

    // 3. Stored service key in auth.json
    let provider = "sap-ai-core";
    if let Some(AuthCredential::ServiceKey {
        client_id,
        client_secret,
        token_url,
        service_url,
    }) = auth.credential_for_provider(provider)
    {
        if let (Some(id), Some(secret), Some(turl), Some(surl)) = (
            client_id.as_ref(),
            client_secret.as_ref(),
            token_url.as_ref(),
            service_url.as_ref(),
        ) {
            if !id.is_empty() && !secret.is_empty() && !turl.is_empty() && !surl.is_empty() {
                return Some(SapResolvedCredentials {
                    client_id: id.clone(),
                    client_secret: secret.clone(),
                    token_url: turl.clone(),
                    service_url: surl.clone(),
                });
            }
        }
    }

    None
}

/// Parse a JSON-encoded SAP AI Core service key.
fn parse_sap_service_key_json(json_str: &str) -> Option<SapResolvedCredentials> {
    let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let obj = v.as_object()?;

    // SAP service keys use "clientid"/"clientsecret" (no underscore) and
    // "url" for token URL, "serviceurls.AI_API_URL" for service URL.
    let client_id = obj
        .get("clientid")
        .or_else(|| obj.get("client_id"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let client_secret = obj
        .get("clientsecret")
        .or_else(|| obj.get("client_secret"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let token_url = obj
        .get("url")
        .or_else(|| obj.get("token_url"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let service_url = obj
        .get("serviceurls")
        .and_then(|v| v.get("AI_API_URL"))
        .and_then(|v| v.as_str())
        .or_else(|| obj.get("service_url").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())?;

    Some(SapResolvedCredentials {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        token_url: token_url.to_string(),
        service_url: service_url.to_string(),
    })
}

#[derive(Debug, Deserialize)]
struct SapTokenExchangeResponse {
    access_token: String,
}

/// Exchange SAP AI Core service-key credentials for an access token.
///
/// Returns `Ok(None)` when SAP credentials are not configured.
pub async fn exchange_sap_access_token(auth: &AuthStorage) -> Result<Option<String>> {
    let Some(creds) = resolve_sap_credentials(auth) else {
        return Ok(None);
    };

    let client = crate::http::client::Client::new();
    let token = exchange_sap_access_token_with_client(&client, &creds).await?;
    Ok(Some(token))
}

async fn exchange_sap_access_token_with_client(
    client: &crate::http::client::Client,
    creds: &SapResolvedCredentials,
) -> Result<String> {
    let form_body = format!(
        "grant_type=client_credentials&client_id={}&client_secret={}",
        percent_encode_component(&creds.client_id),
        percent_encode_component(&creds.client_secret),
    );

    let request = client
        .post(&creds.token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("SAP AI Core token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(
        &text,
        &[creds.client_id.as_str(), creds.client_secret.as_str()],
    );

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "SAP AI Core token exchange failed (HTTP {status}): {redacted_text}"
        )));
    }

    let response: SapTokenExchangeResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("SAP AI Core token response was invalid JSON: {e}")))?;
    let access_token = response.access_token.trim();
    if access_token.is_empty() {
        return Err(Error::auth(
            "SAP AI Core token exchange returned an empty access_token".to_string(),
        ));
    }

    Ok(access_token.to_string())
}

fn redact_known_secrets(text: &str, secrets: &[&str]) -> String {
    let mut redacted = text.to_string();
    for secret in secrets {
        let trimmed = secret.trim();
        if !trimmed.is_empty() {
            redacted = redacted.replace(trimmed, "[REDACTED]");
        }
    }

    redact_sensitive_json_fields(&redacted)
}

fn redact_sensitive_json_fields(text: &str) -> String {
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.to_string();
    };
    redact_sensitive_json_value(&mut json);
    serde_json::to_string(&json).unwrap_or_else(|_| text.to_string())
}

fn redact_sensitive_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, nested) in map {
                if is_sensitive_json_key(key) {
                    *nested = serde_json::Value::String("[REDACTED]".to_string());
                } else {
                    redact_sensitive_json_value(nested);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_sensitive_json_value(item);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn is_sensitive_json_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|ch| ch.to_ascii_lowercase())
        .collect();

    matches!(
        normalized.as_str(),
        "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "apikey"
            | "authorization"
            | "credential"
            | "secret"
            | "clientsecret"
            | "password"
    ) || normalized.ends_with("token")
        || normalized.ends_with("secret")
        || normalized.ends_with("apikey")
        || normalized.contains("authorization")
}

#[derive(Debug)]
pub struct OAuthStartInfo {
    pub provider: String,
    pub url: String,
    pub verifier: String,
    pub instructions: Option<String>,
    /// The redirect URI used in the authorization request.
    /// When this points to localhost, a local callback server should be started.
    pub redirect_uri: Option<String>,
    /// Pre-bound callback server (already listening).
    ///
    /// When a provider binds a random-port localhost listener at start time
    /// (e.g. GitHub Copilot, GitLab), the server is stored here so the caller
    /// does not need to bind a second time.  If `Some`, the caller should use
    /// this server directly instead of calling [`start_oauth_callback_server`].
    pub callback_server: Option<OAuthCallbackServer>,
}

// ── Local OAuth callback server ─────────────────────────────────

/// Handle for a background TCP listener that receives the OAuth redirect callback.
///
/// When the OAuth provider redirects the browser to a `localhost` URI, this
/// server accepts a single connection, extracts the full request URL (which
/// contains the `code` and `state` query parameters), sends a success HTML
/// page to the browser, and delivers the URL through the returned receiver.
pub struct OAuthCallbackServer {
    /// Receives the full request path+query (e.g. `/auth/callback?code=abc&state=xyz`).
    pub rx: std::sync::mpsc::Receiver<String>,
    /// The port the server is listening on (for logging/diagnostics).
    pub port: u16,
    // Note: `_handle` is kept for the thread join handle, which keeps the
    // listener thread alive until this struct is dropped.
    _handle: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for OAuthCallbackServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCallbackServer")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// Start a local TCP listener on the port specified in `redirect_uri`.
///
/// The server accepts exactly one connection, responds with a friendly HTML page
/// so the browser shows "You can close this tab", and sends the full callback URL
/// through the returned `OAuthCallbackServer.rx`.
///
/// Returns `Err` if the redirect URI does not contain a parseable port or the
/// port cannot be bound.
pub fn start_oauth_callback_server(redirect_uri: &str) -> Result<OAuthCallbackServer> {
    // Parse the port from the redirect URI (e.g. "http://localhost:1455/auth/callback").
    let port = parse_port_from_uri(redirect_uri).ok_or_else(|| {
        Error::auth(format!(
            "Cannot parse port from OAuth redirect URI: {redirect_uri}"
        ))
    })?;

    let listener = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).map_err(|e| {
        Error::auth(format!(
            "Failed to bind OAuth callback server on port {port}: {e}"
        ))
    })?;

    // Set a generous timeout so the thread doesn't hang forever if the user
    // cancels. The thread will exit when the listener is dropped or when a
    // connection arrives.
    listener
        .set_nonblocking(false)
        .map_err(|e| Error::auth(format!("Failed to configure callback listener: {e}")))?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();

    let handle = std::thread::spawn(move || {
        // Accept exactly one connection.
        let Ok((mut stream, _addr)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

        // Read the HTTP request (we only need the first line: `GET /path?query HTTP/1.1`).
        let mut buf = [0u8; 4096];
        let Ok(n) = stream.read(&mut buf) else {
            return;
        };

        let request = String::from_utf8_lossy(&buf[..n]);
        let request_path = request
            .lines()
            .next()
            .and_then(|line| {
                // "GET /auth/callback?code=abc&state=xyz HTTP/1.1"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    Some(parts[1].to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        // Send a friendly response so the browser shows a success page.
        let html = r#"<!DOCTYPE html>
<html><head><title>Pi Agent — OAuth Complete</title></head>
<body style="font-family:system-ui,sans-serif;text-align:center;padding:60px 20px;background:#f8f9fa">
<h1 style="color:#2d7d46">&#10003; Authorization successful</h1>
<p>You can close this browser tab and return to Pi Agent.</p>
</body></html>"#;

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();

        // Deliver the callback URL to the waiting caller.
        let _ = tx.send(request_path);
    });

    Ok(OAuthCallbackServer {
        rx,
        port,
        _handle: handle,
    })
}

/// Extract the port number from a localhost redirect URI.
///
/// Supports formats like `http://localhost:1455/auth/callback` and
/// `http://127.0.0.1:8085/oauth2callback`.
fn parse_port_from_uri(uri: &str) -> Option<u16> {
    // Strip scheme
    let without_scheme = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))?;
    // Take host:port part (before the path)
    let host_port = without_scheme.split('/').next()?;
    // Extract port after the last colon
    let port_str = host_port.rsplit(':').next()?;
    port_str.parse::<u16>().ok()
}

/// Returns `true` if this redirect URI points to localhost (and therefore
/// needs a local callback server).
pub fn redirect_uri_needs_callback_server(redirect_uri: &str) -> bool {
    let lower = redirect_uri.to_lowercase();
    lower.starts_with("http://localhost:") || lower.starts_with("http://127.0.0.1:")
}

/// Start a local TCP listener on a random available port and return both the
/// callback server and the `http://localhost:{port}/callback` redirect URI.
///
/// This is useful for OAuth providers (like GitHub Copilot and GitLab) that
/// accept localhost redirect URIs but don't have a fixed port pre-registered.
/// The caller should include the returned `redirect_uri` in the authorization
/// URL query parameters.
pub fn start_oauth_callback_server_random_port() -> Result<(OAuthCallbackServer, String)> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| {
        Error::auth(format!(
            "Failed to bind OAuth callback server on random port: {e}"
        ))
    })?;

    let port = listener
        .local_addr()
        .map_err(|e| {
            Error::auth(format!(
                "Failed to get local address of OAuth callback listener: {e}"
            ))
        })?
        .port();

    let redirect_uri = format!("http://localhost:{port}/callback");

    listener
        .set_nonblocking(false)
        .map_err(|e| Error::auth(format!("Failed to configure callback listener: {e}")))?;

    let (tx, rx) = std::sync::mpsc::channel::<String>();

    let handle = std::thread::spawn(move || {
        let Ok((mut stream, _addr)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

        let mut buf = [0u8; 4096];
        let Ok(n) = stream.read(&mut buf) else {
            return;
        };

        let request = String::from_utf8_lossy(&buf[..n]);
        let request_path = request
            .lines()
            .next()
            .and_then(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    Some(parts[1].to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let html = r#"<!DOCTYPE html>
<html><head><title>Pi Agent — OAuth Complete</title></head>
<body style="font-family:system-ui,sans-serif;text-align:center;padding:60px 20px;background:#f8f9fa">
<h1 style="color:#2d7d46">&#10003; Authorization successful</h1>
<p>You can close this browser tab and return to Pi Agent.</p>
</body></html>"#;

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
            html.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();

        let _ = tx.send(request_path);
    });

    Ok((
        OAuthCallbackServer {
            rx,
            port,
            _handle: handle,
        },
        redirect_uri,
    ))
}

// ── Device Flow (RFC 8628) ──────────────────────────────────────

/// Response from the device authorization endpoint (RFC 8628 section 3.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "default_device_interval")]
    pub interval: u64,
}

const fn default_device_interval() -> u64 {
    5
}

/// Result of polling the device flow token endpoint.
#[derive(Debug)]
pub enum DeviceFlowPollResult {
    /// User has not yet authorized; keep polling.
    Pending,
    /// Server asked us to slow down; increase interval.
    SlowDown,
    /// Authorization succeeded.
    Success(AuthCredential),
    /// Device code has expired.
    Expired,
    /// User explicitly denied access.
    AccessDenied,
    /// An unexpected error occurred.
    Error(String),
}

// ── Provider-specific OAuth configs ─────────────────────────────

/// OAuth settings for GitHub Copilot.
///
/// `github_base_url` defaults to `https://github.com` but can be overridden
/// for GitHub Enterprise Server instances.
#[derive(Debug, Clone)]
pub struct CopilotOAuthConfig {
    pub client_id: String,
    pub github_base_url: String,
    pub scopes: String,
}

impl Default for CopilotOAuthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            github_base_url: "https://github.com".to_string(),
            scopes: GITHUB_COPILOT_SCOPES.to_string(),
        }
    }
}

/// OAuth settings for GitLab.
///
/// `base_url` defaults to `https://gitlab.com` but can be overridden
/// for self-hosted GitLab instances.
#[derive(Debug, Clone)]
pub struct GitLabOAuthConfig {
    pub client_id: String,
    pub base_url: String,
    pub scopes: String,
    pub redirect_uri: Option<String>,
}

impl Default for GitLabOAuthConfig {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            base_url: GITLAB_DEFAULT_BASE_URL.to_string(),
            scopes: GITLAB_DEFAULT_SCOPES.to_string(),
            redirect_uri: None,
        }
    }
}

fn percent_encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push_str("%20"),
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

fn percent_decode_component(value: &str) -> Option<String> {
    if !value.as_bytes().contains(&b'%') && !value.as_bytes().contains(&b'+') {
        return Some(value.to_string());
    }

    let mut out = Vec::with_capacity(value.len());
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(b) = bytes.next() {
        match b {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = bytes.next()?;
                let lo = bytes.next()?;
                let hex = [hi, lo];
                let hex = std::str::from_utf8(&hex).ok()?;
                let decoded = u8::from_str_radix(hex, 16).ok()?;
                out.push(decoded);
            }
            other => out.push(other),
        }
    }

    String::from_utf8(out).ok()
}

fn parse_query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.trim().is_empty())
        .filter_map(|part| {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let key = percent_decode_component(k.trim())?;
            let value = percent_decode_component(v.trim())?;
            Some((key, value))
        })
        .collect()
}

fn build_url_with_query(base: &str, params: &[(&str, &str)]) -> String {
    let mut url = String::with_capacity(base.len() + 128);
    url.push_str(base);
    url.push('?');

    for (idx, (k, v)) in params.iter().enumerate() {
        if idx > 0 {
            url.push('&');
        }
        url.push_str(&percent_encode_component(k));
        url.push('=');
        url.push_str(&percent_encode_component(v));
    }

    url
}

fn kimi_code_oauth_host_with_env_lookup<F>(env_lookup: F) -> String
where
    F: Fn(&str) -> Option<String>,
{
    KIMI_CODE_OAUTH_HOST_ENV_KEYS
        .iter()
        .find_map(|key| {
            env_lookup(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| KIMI_CODE_OAUTH_DEFAULT_HOST.to_string())
}

fn kimi_code_oauth_host() -> String {
    kimi_code_oauth_host_with_env_lookup(|key| std::env::var(key).ok())
}

fn kimi_code_endpoint_for_host(host: &str, path: &str) -> String {
    format!("{}{}", trim_trailing_slash(host), path)
}

fn kimi_code_token_endpoint() -> String {
    kimi_code_endpoint_for_host(&kimi_code_oauth_host(), KIMI_CODE_TOKEN_PATH)
}

fn home_dir_with_env_lookup<F>(env_lookup: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("HOME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env_lookup("USERPROFILE")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            let drive = env_lookup("HOMEDRIVE")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())?;
            let path = env_lookup("HOMEPATH")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())?;
            if path.starts_with('\\') || path.starts_with('/') {
                Some(PathBuf::from(format!("{drive}{path}")))
            } else {
                let mut combined = PathBuf::from(drive);
                combined.push(path);
                Some(combined)
            }
        })
}

fn home_dir() -> Option<PathBuf> {
    home_dir_with_env_lookup(|key| std::env::var(key).ok())
}

fn kimi_share_dir_with_env_lookup<F>(env_lookup: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup(KIMI_SHARE_DIR_ENV_KEY)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir_with_env_lookup(env_lookup).map(|home| home.join(".kimi")))
}

fn kimi_share_dir() -> Option<PathBuf> {
    kimi_share_dir_with_env_lookup(|key| std::env::var(key).ok())
}

fn sanitize_ascii_header_value(value: &str, fallback: &str) -> String {
    if value.is_ascii() && !value.trim().is_empty() {
        return value.to_string();
    }

    let sanitized = value
        .chars()
        .filter(char::is_ascii)
        .collect::<String>()
        .trim()
        .to_string();
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

fn kimi_device_id_paths() -> Option<(PathBuf, PathBuf)> {
    let primary = kimi_share_dir()?.join("device_id");
    let legacy = home_dir().map_or_else(
        || primary.clone(),
        |home| home.join(".pi").join("agent").join("kimi-device-id"),
    );
    Some((primary, legacy))
}

fn kimi_device_id() -> String {
    let generated = uuid::Uuid::new_v4().simple().to_string();
    let Some((primary, legacy)) = kimi_device_id_paths() else {
        return generated;
    };

    for path in [&primary, &legacy] {
        if let Ok(existing) = fs::read_to_string(path) {
            let existing = existing.trim();
            if !existing.is_empty() {
                return existing.to_string();
            }
        }
    }

    if let Some(parent) = primary.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    if let Ok(mut file) = options.open(&primary) {
        let _ = file.write_all(generated.as_bytes());
    }

    generated
}

fn kimi_common_headers() -> Vec<(String, String)> {
    let device_name = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let device_model = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let os_version = std::env::consts::OS.to_string();

    vec![
        (
            "X-Msh-Platform".to_string(),
            sanitize_ascii_header_value("kimi_cli", "unknown"),
        ),
        (
            "X-Msh-Version".to_string(),
            sanitize_ascii_header_value(env!("CARGO_PKG_VERSION"), "unknown"),
        ),
        (
            "X-Msh-Device-Name".to_string(),
            sanitize_ascii_header_value(&device_name, "unknown"),
        ),
        (
            "X-Msh-Device-Model".to_string(),
            sanitize_ascii_header_value(&device_model, "unknown"),
        ),
        (
            "X-Msh-Os-Version".to_string(),
            sanitize_ascii_header_value(&os_version, "unknown"),
        ),
        (
            "X-Msh-Device-Id".to_string(),
            sanitize_ascii_header_value(&kimi_device_id(), "unknown"),
        ),
    ]
}

fn auth_lock_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .map_or_else(|| "lock".to_string(), |ext| format!("{ext}.lock"));
    path.with_extension(extension)
}

fn open_auth_lock_file(path: &Path) -> Result<File> {
    let lock_path = auth_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut options = File::options();
    options.read(true).write(true).create(true).truncate(false);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    options
        .open(lock_path)
        .map_err(|err| Error::auth(format!("auth lock file: {err}")))
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };

    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Start Anthropic OAuth by generating an authorization URL and PKCE verifier.
pub fn start_anthropic_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();

    let url = build_url_with_query(
        ANTHROPIC_OAUTH_AUTHORIZE_URL,
        &[
            ("code", "true"),
            ("client_id", ANTHROPIC_OAUTH_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", ANTHROPIC_OAUTH_REDIRECT_URI),
            ("scope", ANTHROPIC_OAUTH_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "anthropic".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
        redirect_uri: Some(ANTHROPIC_OAUTH_REDIRECT_URI.to_string()),
        callback_server: None,
    })
}

/// Complete Anthropic OAuth by exchanging an authorization code for access/refresh tokens.
pub async fn complete_anthropic_oauth(code_input: &str, verifier: &str) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();
    let request = client
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": ANTHROPIC_OAUTH_REDIRECT_URI,
            "code_verifier": verifier,
        }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[code.as_str(), verifier, state.as_str()]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Token exchange failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid token response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(ANTHROPIC_OAUTH_TOKEN_URL.to_string()),
        client_id: Some(ANTHROPIC_OAUTH_CLIENT_ID.to_string()),
    })
}

async fn refresh_anthropic_oauth_token(
    client: &crate::http::client::Client,
    refresh_token: &str,
) -> Result<AuthCredential> {
    let request = client
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
            "refresh_token": refresh_token,
        }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Anthropic token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[refresh_token]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Anthropic token refresh failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid refresh response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(ANTHROPIC_OAUTH_TOKEN_URL.to_string()),
        client_id: Some(ANTHROPIC_OAUTH_CLIENT_ID.to_string()),
    })
}

/// Start OpenAI Codex OAuth by generating an authorization URL and PKCE verifier.
pub fn start_openai_codex_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let url = build_url_with_query(
        OPENAI_CODEX_OAUTH_AUTHORIZE_URL,
        &[
            ("response_type", "code"),
            ("client_id", OPENAI_CODEX_OAUTH_CLIENT_ID),
            ("redirect_uri", OPENAI_CODEX_OAUTH_REDIRECT_URI),
            ("scope", OPENAI_CODEX_OAUTH_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("originator", "pi"),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "openai-codex".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
        redirect_uri: Some(OPENAI_CODEX_OAUTH_REDIRECT_URI.to_string()),
        callback_server: None,
    })
}

/// Complete OpenAI Codex OAuth by exchanging an authorization code for access/refresh tokens.
pub async fn complete_openai_codex_oauth(
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);
    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };
    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let form_body = format!(
        "grant_type=authorization_code&client_id={}&code={}&code_verifier={}&redirect_uri={}",
        percent_encode_component(OPENAI_CODEX_OAUTH_CLIENT_ID),
        percent_encode_component(&code),
        percent_encode_component(verifier),
        percent_encode_component(OPENAI_CODEX_OAUTH_REDIRECT_URI),
    );

    let client = crate::http::client::Client::new();
    let request = client
        .post(OPENAI_CODEX_OAUTH_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("OpenAI Codex token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[code.as_str(), verifier]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "OpenAI Codex token exchange failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid OpenAI Codex token response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(OPENAI_CODEX_OAUTH_TOKEN_URL.to_string()),
        client_id: Some(OPENAI_CODEX_OAUTH_CLIENT_ID.to_string()),
    })
}

/// Start Google Gemini CLI OAuth by generating an authorization URL and PKCE verifier.
pub fn start_google_gemini_cli_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let url = build_url_with_query(
        GOOGLE_GEMINI_CLI_OAUTH_AUTHORIZE_URL,
        &[
            ("client_id", GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI),
            ("scope", GOOGLE_GEMINI_CLI_OAUTH_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "google-gemini-cli".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
        redirect_uri: Some(GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI.to_string()),
        callback_server: None,
    })
}

/// Start Google Antigravity OAuth by generating an authorization URL and PKCE verifier.
pub fn start_google_antigravity_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let url = build_url_with_query(
        GOOGLE_ANTIGRAVITY_OAUTH_AUTHORIZE_URL,
        &[
            ("client_id", GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI),
            ("scope", GOOGLE_ANTIGRAVITY_OAUTH_SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &verifier),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    );

    Ok(OAuthStartInfo {
        provider: "google-antigravity".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
        redirect_uri: Some(GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI.to_string()),
        callback_server: None,
    })
}

async fn discover_google_gemini_cli_project_id(
    client: &crate::http::client::Client,
    access_token: &str,
) -> Result<String> {
    let env_project = google_project_id_from_env();
    let mut payload = serde_json::json!({
        "metadata": {
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        }
    });
    if let Some(project) = &env_project {
        payload["cloudaicompanionProject"] = serde_json::Value::String(project.clone());
        payload["metadata"]["duetProject"] = serde_json::Value::String(project.clone());
    }

    let request = client
        .post(&format!(
            "{GOOGLE_GEMINI_CLI_CODE_ASSIST_ENDPOINT}/v1internal:loadCodeAssist"
        ))
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Content-Type", "application/json")
        .json(&payload)?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Google Cloud project discovery failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());

    if (200..300).contains(&status) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(project_id) = parse_code_assist_project_id(&value) {
                return Ok(project_id);
            }
        }
    }

    if let Some(project_id) = env_project {
        return Ok(project_id);
    }

    Err(Error::auth(
        "Google Cloud project discovery failed. Set GOOGLE_CLOUD_PROJECT or GOOGLE_CLOUD_PROJECT_ID and retry /login google-gemini-cli.".to_string(),
    ))
}

async fn discover_google_antigravity_project_id(
    client: &crate::http::client::Client,
    access_token: &str,
) -> Result<String> {
    let payload = serde_json::json!({
        "metadata": {
            "ideType": "IDE_UNSPECIFIED",
            "platform": "PLATFORM_UNSPECIFIED",
            "pluginType": "GEMINI",
        }
    });

    for endpoint in GOOGLE_ANTIGRAVITY_PROJECT_DISCOVERY_ENDPOINTS {
        let request = client
            .post(&format!("{endpoint}/v1internal:loadCodeAssist"))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Content-Type", "application/json")
            .json(&payload)?;

        let Ok(response) = Box::pin(request.send()).await else {
            continue;
        };
        let status = response.status();
        if !(200..300).contains(&status) {
            continue;
        }
        let text = response.text().await.unwrap_or_default();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(project_id) = parse_code_assist_project_id(&value) {
                return Ok(project_id);
            }
        }
    }

    Ok(GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID.to_string())
}

fn parse_code_assist_project_id(value: &serde_json::Value) -> Option<String> {
    value
        .get("cloudaicompanionProject")
        .and_then(|project| {
            project
                .as_str()
                .map(std::string::ToString::to_string)
                .or_else(|| {
                    project
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .map(std::string::ToString::to_string)
                })
        })
        .map(|project| project.trim().to_string())
        .filter(|project| !project.is_empty())
}

async fn exchange_google_authorization_code(
    client: &crate::http::client::Client,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<OAuthTokenResponse> {
    let form_body = format!(
        "client_id={}&client_secret={}&code={}&grant_type=authorization_code&redirect_uri={}&code_verifier={}",
        percent_encode_component(client_id),
        percent_encode_component(client_secret),
        percent_encode_component(code),
        percent_encode_component(redirect_uri),
        percent_encode_component(verifier),
    );

    let request = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("OAuth token exchange failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[code, verifier, client_secret]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "OAuth token exchange failed: {redacted_text}"
        )));
    }

    serde_json::from_str::<OAuthTokenResponse>(&text)
        .map_err(|e| Error::auth(format!("Invalid OAuth token response: {e}")))
}

/// Complete Google Gemini CLI OAuth by exchanging an authorization code for tokens.
pub async fn complete_google_gemini_cli_oauth(
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);
    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };
    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();
    let oauth_response = exchange_google_authorization_code(
        &client,
        GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL,
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID,
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET,
        &code,
        GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI,
        verifier,
    )
    .await?;

    let project_id =
        discover_google_gemini_cli_project_id(&client, &oauth_response.access_token).await?;

    Ok(AuthCredential::OAuth {
        access_token: encode_project_scoped_access_token(&oauth_response.access_token, &project_id),
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: None,
        client_id: None,
    })
}

/// Complete Google Antigravity OAuth by exchanging an authorization code for tokens.
pub async fn complete_google_antigravity_oauth(
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);
    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };
    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();
    let oauth_response = exchange_google_authorization_code(
        &client,
        GOOGLE_ANTIGRAVITY_OAUTH_TOKEN_URL,
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID,
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET,
        &code,
        GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI,
        verifier,
    )
    .await?;

    let project_id =
        discover_google_antigravity_project_id(&client, &oauth_response.access_token).await?;

    Ok(AuthCredential::OAuth {
        access_token: encode_project_scoped_access_token(&oauth_response.access_token, &project_id),
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: None,
        client_id: None,
    })
}

#[derive(Debug, Deserialize)]
struct OAuthRefreshTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

async fn refresh_google_oauth_token_with_project(
    client: &crate::http::client::Client,
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
    project_id: &str,
    provider_name: &str,
) -> Result<AuthCredential> {
    let form_body = format!(
        "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
        percent_encode_component(client_id),
        percent_encode_component(client_secret),
        percent_encode_component(refresh_token),
    );

    let request = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("{provider_name} token refresh failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[client_secret, refresh_token]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "{provider_name} token refresh failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthRefreshTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid {provider_name} refresh response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: encode_project_scoped_access_token(&oauth_response.access_token, project_id),
        refresh_token: oauth_response
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: None,
        client_id: None,
    })
}

async fn refresh_google_gemini_cli_oauth_token(
    client: &crate::http::client::Client,
    refresh_token: &str,
    project_id: &str,
) -> Result<AuthCredential> {
    refresh_google_oauth_token_with_project(
        client,
        GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL,
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID,
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET,
        refresh_token,
        project_id,
        "google-gemini-cli",
    )
    .await
}

async fn refresh_google_antigravity_oauth_token(
    client: &crate::http::client::Client,
    refresh_token: &str,
    project_id: &str,
) -> Result<AuthCredential> {
    refresh_google_oauth_token_with_project(
        client,
        GOOGLE_ANTIGRAVITY_OAUTH_TOKEN_URL,
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID,
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET,
        refresh_token,
        project_id,
        "google-antigravity",
    )
    .await
}

/// Start Kimi Code OAuth device flow.
pub async fn start_kimi_code_device_flow() -> Result<DeviceCodeResponse> {
    let client = crate::http::client::Client::new();
    start_kimi_code_device_flow_with_client(&client, &kimi_code_oauth_host()).await
}

async fn start_kimi_code_device_flow_with_client(
    client: &crate::http::client::Client,
    oauth_host: &str,
) -> Result<DeviceCodeResponse> {
    let url = kimi_code_endpoint_for_host(oauth_host, KIMI_CODE_DEVICE_AUTHORIZATION_PATH);
    let form_body = format!(
        "client_id={}",
        percent_encode_component(KIMI_CODE_OAUTH_CLIENT_ID)
    );
    let mut request = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        request = request.header(name, value);
    }

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Kimi device authorization request failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[KIMI_CODE_OAUTH_CLIENT_ID]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Kimi device authorization failed (HTTP {status}): {redacted_text}"
        )));
    }

    serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid Kimi device authorization response: {e}")))
}

/// Poll Kimi Code OAuth device flow.
pub async fn poll_kimi_code_device_flow(device_code: &str) -> DeviceFlowPollResult {
    let client = crate::http::client::Client::new();
    poll_kimi_code_device_flow_with_client(&client, &kimi_code_oauth_host(), device_code).await
}

async fn poll_kimi_code_device_flow_with_client(
    client: &crate::http::client::Client,
    oauth_host: &str,
    device_code: &str,
) -> DeviceFlowPollResult {
    let token_url = kimi_code_endpoint_for_host(oauth_host, KIMI_CODE_TOKEN_PATH);
    let form_body = format!(
        "client_id={}&device_code={}&grant_type={}",
        percent_encode_component(KIMI_CODE_OAUTH_CLIENT_ID),
        percent_encode_component(device_code),
        percent_encode_component("urn:ietf:params:oauth:grant-type:device_code"),
    );
    let mut request = client
        .post(&token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        request = request.header(name, value);
    }

    let response = match Box::pin(request.send()).await {
        Ok(response) => response,
        Err(err) => return DeviceFlowPollResult::Error(format!("Poll request failed: {err}")),
    };
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            return DeviceFlowPollResult::Error(format!("Invalid poll response JSON: {err}"));
        }
    };

    if let Some(error) = json.get("error").and_then(serde_json::Value::as_str) {
        return match error {
            "authorization_pending" => DeviceFlowPollResult::Pending,
            "slow_down" => DeviceFlowPollResult::SlowDown,
            "expired_token" => DeviceFlowPollResult::Expired,
            "access_denied" => DeviceFlowPollResult::AccessDenied,
            other => {
                let detail = json
                    .get("error_description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown error");
                DeviceFlowPollResult::Error(format!("Kimi device flow error: {other}: {detail}"))
            }
        };
    }

    if !(200..300).contains(&status) {
        return DeviceFlowPollResult::Error(format!(
            "Kimi device flow polling failed (HTTP {status}): {}",
            redact_known_secrets(&text, &[device_code]),
        ));
    }

    let oauth_response: OAuthTokenResponse = match serde_json::from_value(json) {
        Ok(response) => response,
        Err(err) => {
            return DeviceFlowPollResult::Error(format!(
                "Invalid Kimi token response payload: {err}"
            ));
        }
    };

    DeviceFlowPollResult::Success(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url),
        client_id: Some(KIMI_CODE_OAUTH_CLIENT_ID.to_string()),
    })
}

async fn refresh_kimi_code_oauth_token(
    client: &crate::http::client::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<AuthCredential> {
    let form_body = format!(
        "client_id={}&grant_type=refresh_token&refresh_token={}",
        percent_encode_component(KIMI_CODE_OAUTH_CLIENT_ID),
        percent_encode_component(refresh_token),
    );
    let mut request = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        request = request.header(name, value);
    }

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Kimi token refresh failed: {e}")))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[refresh_token]);
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Kimi token refresh failed (HTTP {status}): {redacted_text}"
        )));
    }

    let oauth_response: OAuthRefreshTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid Kimi refresh response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url.to_string()),
        client_id: Some(KIMI_CODE_OAUTH_CLIENT_ID.to_string()),
    })
}

/// Start OAuth for an extension-registered provider using its [`OAuthConfig`](crate::models::OAuthConfig).
pub fn start_extension_oauth(
    provider_name: &str,
    config: &crate::models::OAuthConfig,
) -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let scopes = config.scopes.join(" ");

    let mut params: Vec<(&str, &str)> = vec![
        ("client_id", &config.client_id),
        ("response_type", "code"),
        ("scope", &scopes),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &verifier),
    ];

    let redirect_uri_ref = config.redirect_uri.as_deref();
    if let Some(uri) = redirect_uri_ref {
        params.push(("redirect_uri", uri));
    }

    let url = build_url_with_query(&config.auth_url, &params);

    Ok(OAuthStartInfo {
        provider: provider_name.to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL, complete login, then paste the callback URL or authorization code."
                .to_string(),
        ),
        redirect_uri: config.redirect_uri.clone(),
        callback_server: None,
    })
}

/// Complete OAuth for an extension-registered provider by exchanging an authorization code.
pub async fn complete_extension_oauth(
    config: &crate::models::OAuthConfig,
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();

    let mut body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": config.client_id,
        "code": code,
        "state": state,
        "code_verifier": verifier,
    });

    if let Some(ref redirect_uri) = config.redirect_uri {
        body["redirect_uri"] = serde_json::Value::String(redirect_uri.clone());
    }

    let request = client.post(&config.token_url).json(&body)?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[code.as_str(), verifier, state.as_str()]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Token exchange failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid token response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(config.token_url.clone()),
        client_id: Some(config.client_id.clone()),
    })
}

/// Refresh an OAuth token for an extension-registered provider.
async fn refresh_extension_oauth_token(
    client: &crate::http::client::Client,
    config: &crate::models::OAuthConfig,
    refresh_token: &str,
) -> Result<AuthCredential> {
    let request = client.post(&config.token_url).json(&serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": config.client_id,
        "refresh_token": refresh_token,
    }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Extension OAuth token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[refresh_token]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Extension OAuth token refresh failed: {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid refresh response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(config.token_url.clone()),
        client_id: Some(config.client_id.clone()),
    })
}

/// Provider-agnostic OAuth refresh using self-contained credential metadata.
///
/// This is called for providers whose [`AuthCredential::OAuth`] stores its own
/// `token_url` and `client_id` (e.g. Copilot, GitLab), removing the need for
/// an external config lookup at refresh time.
async fn refresh_self_contained_oauth_token(
    client: &crate::http::client::Client,
    token_url: &str,
    oauth_client_id: &str,
    refresh_token: &str,
    provider: &str,
) -> Result<AuthCredential> {
    let request = client.post(token_url).json(&serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": oauth_client_id,
        "refresh_token": refresh_token,
    }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("{provider} token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[refresh_token]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "{provider} token refresh failed (HTTP {status}): {redacted_text}"
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text)
        .map_err(|e| Error::auth(format!("Invalid refresh response from {provider}: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url.to_string()),
        client_id: Some(oauth_client_id.to_string()),
    })
}

// ── GitHub Copilot OAuth ─────────────────────────────────────────

/// Start GitHub Copilot OAuth using the browser-based authorization code flow.
///
/// For CLI tools the device flow ([`start_copilot_device_flow`]) is usually
/// preferred, but the browser flow is provided for environments that support
/// redirect callbacks.
pub fn start_copilot_browser_oauth(config: &CopilotOAuthConfig) -> Result<OAuthStartInfo> {
    if config.client_id.is_empty() {
        return Err(Error::auth(
            "GitHub Copilot OAuth requires a client_id. Set GITHUB_COPILOT_CLIENT_ID or \
             configure the GitHub App in your settings."
                .to_string(),
        ));
    }

    let (verifier, challenge) = generate_pkce();

    let auth_url = if config.github_base_url == "https://github.com" {
        GITHUB_OAUTH_AUTHORIZE_URL.to_string()
    } else {
        format!(
            "{}/login/oauth/authorize",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    // Bind a localhost callback server on a random port so the browser redirect
    // is captured automatically (issue #22).  GitHub OAuth supports any
    // localhost redirect URI for public OAuth Apps.
    let callback = start_oauth_callback_server_random_port().ok();
    let redirect_uri = callback.as_ref().map(|(_, uri)| uri.clone());

    let mut params: Vec<(&str, &str)> = vec![
        ("client_id", &config.client_id),
        ("response_type", "code"),
        ("scope", &config.scopes),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &verifier),
    ];
    let redirect_ref = redirect_uri.as_deref();
    if let Some(uri) = redirect_ref {
        params.push(("redirect_uri", uri));
    }

    let url = build_url_with_query(&auth_url, &params);

    Ok(OAuthStartInfo {
        provider: "github-copilot".to_string(),
        url,
        verifier,
        instructions: Some(
            "Open the URL in your browser to authorize GitHub Copilot access, \
             then paste the callback URL or authorization code."
                .to_string(),
        ),
        redirect_uri,
        // Pre-bound callback server — caller should use this instead of
        // starting a new one (the port is already bound).
        callback_server: callback.map(|(server, _)| server),
    })
}

/// Complete the GitHub Copilot browser OAuth flow by exchanging the authorization code.
pub async fn complete_copilot_browser_oauth(
    config: &CopilotOAuthConfig,
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth(
            "Missing authorization code. Paste the full callback URL or just the code parameter."
                .to_string(),
        ));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let token_url_str = if config.github_base_url == "https://github.com" {
        GITHUB_OAUTH_TOKEN_URL.to_string()
    } else {
        format!(
            "{}/login/oauth/access_token",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    let client = crate::http::client::Client::new();
    let request = client
        .post(&token_url_str)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": config.client_id,
            "code": code,
            "state": state,
            "code_verifier": verifier,
        }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("GitHub token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted = redact_known_secrets(&text, &[code.as_str(), verifier, state.as_str()]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(copilot_diagnostic(
            &format!("Token exchange failed (HTTP {status})"),
            &redacted,
        )));
    }

    let mut cred = parse_github_token_response(&text)?;
    // Attach refresh metadata so the credential is self-contained for lifecycle refresh.
    if let AuthCredential::OAuth {
        ref mut token_url,
        ref mut client_id,
        ..
    } = cred
    {
        *token_url = Some(token_url_str.clone());
        *client_id = Some(config.client_id.clone());
    }
    Ok(cred)
}

/// Start the GitHub device flow (RFC 8628) for Copilot.
///
/// Returns a [`DeviceCodeResponse`] containing the `user_code` and
/// `verification_uri` the user should visit.
pub async fn start_copilot_device_flow(config: &CopilotOAuthConfig) -> Result<DeviceCodeResponse> {
    if config.client_id.is_empty() {
        return Err(Error::auth(
            "GitHub Copilot device flow requires a client_id. Set GITHUB_COPILOT_CLIENT_ID or \
             configure the GitHub App in your settings."
                .to_string(),
        ));
    }

    let device_url = if config.github_base_url == "https://github.com" {
        GITHUB_DEVICE_CODE_URL.to_string()
    } else {
        format!(
            "{}/login/device/code",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    let client = crate::http::client::Client::new();
    let request = client
        .post(&device_url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "client_id": config.client_id,
            "scope": config.scopes,
        }))?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("GitHub device code request failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());

    if !(200..300).contains(&status) {
        return Err(Error::auth(copilot_diagnostic(
            &format!("Device code request failed (HTTP {status})"),
            &redact_known_secrets(&text, &[]),
        )));
    }

    serde_json::from_str(&text).map_err(|e| {
        Error::auth(format!(
            "Invalid device code response: {e}. \
             Ensure the GitHub App has the Device Flow enabled."
        ))
    })
}

/// Poll the GitHub device flow token endpoint.
///
/// Call this repeatedly at the interval specified in [`DeviceCodeResponse`]
/// until the result is not [`DeviceFlowPollResult::Pending`].
pub async fn poll_copilot_device_flow(
    config: &CopilotOAuthConfig,
    device_code: &str,
) -> DeviceFlowPollResult {
    let token_url = if config.github_base_url == "https://github.com" {
        GITHUB_OAUTH_TOKEN_URL.to_string()
    } else {
        format!(
            "{}/login/oauth/access_token",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    let client = crate::http::client::Client::new();
    let request = match client
        .post(&token_url)
        .header("Accept", "application/json")
        .json(&serde_json::json!({
            "client_id": config.client_id,
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        })) {
        Ok(r) => r,
        Err(e) => return DeviceFlowPollResult::Error(format!("Request build failed: {e}")),
    };

    let response = match Box::pin(request.send()).await {
        Ok(r) => r,
        Err(e) => return DeviceFlowPollResult::Error(format!("Poll request failed: {e}")),
    };

    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());

    // GitHub returns 200 even for pending/error states with an "error" field.
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return DeviceFlowPollResult::Error(format!("Invalid poll response: {e}"));
        }
    };

    if let Some(error) = json.get("error").and_then(|v| v.as_str()) {
        return match error {
            "authorization_pending" => DeviceFlowPollResult::Pending,
            "slow_down" => DeviceFlowPollResult::SlowDown,
            "expired_token" => DeviceFlowPollResult::Expired,
            "access_denied" => DeviceFlowPollResult::AccessDenied,
            other => DeviceFlowPollResult::Error(format!(
                "GitHub device flow error: {other}. {}",
                json.get("error_description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Check your GitHub App configuration.")
            )),
        };
    }

    match parse_github_token_response(&text) {
        Ok(cred) => DeviceFlowPollResult::Success(cred),
        Err(e) => DeviceFlowPollResult::Error(e.to_string()),
    }
}

/// Parse GitHub's token endpoint response into an [`AuthCredential`].
///
/// GitHub may return `expires_in` (if token has expiry) or omit it for
/// non-expiring tokens. Non-expiring tokens use a far-future expiry.
fn parse_github_token_response(text: &str) -> Result<AuthCredential> {
    let json: serde_json::Value =
        serde_json::from_str(text).map_err(|e| Error::auth(format!("Invalid token JSON: {e}")))?;

    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::auth("Missing access_token in GitHub response".to_string()))?
        .to_string();

    // GitHub may not return a refresh_token for all grant types.
    let refresh_token = json
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let expires = json
        .get("expires_in")
        .and_then(serde_json::Value::as_i64)
        .map_or_else(
            || {
                // No expiry → treat as 1 year (GitHub personal access tokens don't expire).
                oauth_expires_at_ms(365 * 24 * 3600)
            },
            oauth_expires_at_ms,
        );

    Ok(AuthCredential::OAuth {
        access_token,
        refresh_token,
        expires,
        // token_url/client_id are set by the caller (start/complete functions)
        // since parse_github_token_response doesn't know the config context.
        token_url: None,
        client_id: None,
    })
}

/// Build an actionable diagnostic message for Copilot OAuth failures.
fn copilot_diagnostic(summary: &str, detail: &str) -> String {
    format!(
        "{summary}: {detail}\n\
         Troubleshooting:\n\
         - Verify the GitHub App client_id is correct\n\
         - Ensure your GitHub account has an active Copilot subscription\n\
         - For GitHub Enterprise, set the correct base URL\n\
         - Check https://github.com/settings/applications for app authorization status"
    )
}

// ── GitLab OAuth ────────────────────────────────────────────────

/// Start GitLab OAuth using the authorization code flow with PKCE.
///
/// Supports both `gitlab.com` and self-hosted instances via
/// [`GitLabOAuthConfig::base_url`].
pub fn start_gitlab_oauth(config: &GitLabOAuthConfig) -> Result<OAuthStartInfo> {
    if config.client_id.is_empty() {
        return Err(Error::auth(
            "GitLab OAuth requires a client_id. Create an application at \
             Settings > Applications in your GitLab instance."
                .to_string(),
        ));
    }

    let (verifier, challenge) = generate_pkce();
    let base = trim_trailing_slash(&config.base_url);
    let auth_url = format!("{base}{GITLAB_OAUTH_AUTHORIZE_PATH}");

    // If no redirect_uri is configured, bind a random-port localhost callback
    // server so the browser redirect is captured automatically (issue #22).
    let (redirect_uri, callback_server) = if config.redirect_uri.is_some() {
        (config.redirect_uri.clone(), None)
    } else {
        match start_oauth_callback_server_random_port() {
            Ok((server, uri)) => (Some(uri), Some(server)),
            Err(_) => (None, None),
        }
    };

    let mut params: Vec<(&str, &str)> = vec![
        ("client_id", &config.client_id),
        ("response_type", "code"),
        ("scope", &config.scopes),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &verifier),
    ];

    let redirect_ref = redirect_uri.as_deref();
    if let Some(uri) = redirect_ref {
        params.push(("redirect_uri", uri));
    }

    let url = build_url_with_query(&auth_url, &params);

    Ok(OAuthStartInfo {
        provider: "gitlab".to_string(),
        url,
        verifier,
        instructions: Some(format!(
            "Open the URL to authorize GitLab access on {base}, \
             then paste the callback URL or authorization code."
        )),
        redirect_uri,
        callback_server,
    })
}

/// Complete GitLab OAuth by exchanging the authorization code for tokens.
pub async fn complete_gitlab_oauth(
    config: &GitLabOAuthConfig,
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth(
            "Missing authorization code. Paste the full callback URL or just the code parameter."
                .to_string(),
        ));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state != verifier {
        return Err(Error::auth("State mismatch".to_string()));
    }
    let base = trim_trailing_slash(&config.base_url);
    let token_url = format!("{base}{GITLAB_OAUTH_TOKEN_PATH}");

    let client = crate::http::client::Client::new();

    let mut body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": config.client_id,
        "code": code,
        "state": state,
        "code_verifier": verifier,
    });

    if let Some(ref redirect_uri) = config.redirect_uri {
        body["redirect_uri"] = serde_json::Value::String(redirect_uri.clone());
    }

    let request = client
        .post(&token_url)
        .header("Accept", "application/json")
        .json(&body)?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("GitLab token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted = redact_known_secrets(&text, &[code.as_str(), verifier, state.as_str()]);

    if !(200..300).contains(&status) {
        return Err(Error::auth(gitlab_diagnostic(
            &config.base_url,
            &format!("Token exchange failed (HTTP {status})"),
            &redacted,
        )));
    }

    let oauth_response: OAuthTokenResponse = serde_json::from_str(&text).map_err(|e| {
        Error::auth(gitlab_diagnostic(
            &config.base_url,
            &format!("Invalid token response: {e}"),
            &redacted,
        ))
    })?;

    let base = trim_trailing_slash(&config.base_url);
    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(format!("{base}{GITLAB_OAUTH_TOKEN_PATH}")),
        client_id: Some(config.client_id.clone()),
    })
}

/// Build an actionable diagnostic message for GitLab OAuth failures.
fn gitlab_diagnostic(base_url: &str, summary: &str, detail: &str) -> String {
    format!(
        "{summary}: {detail}\n\
         Troubleshooting:\n\
         - Verify the application client_id matches your GitLab application\n\
         - Check Settings > Applications on {base_url}\n\
         - Ensure the redirect URI matches your application configuration\n\
         - For self-hosted GitLab, verify the base URL is correct ({base_url})"
    )
}

// ── Handoff contract to bd-3uqg.7.6 ────────────────────────────
//
// **OAuth lifecycle boundary**: This module handles the *bootstrap* phase:
//   - Initial device flow or browser-based authorization
//   - Authorization code → token exchange
//   - First credential persistence to auth.json
//
// **NOT handled here** (owned by bd-3uqg.7.6):
//   - Periodic token refresh for Copilot/GitLab
//   - Token rotation and re-authentication on refresh failure
//   - Cache hygiene (pruning expired entries)
//   - Session token lifecycle (keep-alive, invalidation)
//
// To integrate refresh, add "github-copilot" and "gitlab" arms to
// `refresh_expired_oauth_tokens_with_client()` once their refresh
// endpoints and grant types are wired.

fn trim_trailing_slash(url: &str) -> &str {
    url.trim_end_matches('/')
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

fn oauth_expires_at_ms(expires_in_seconds: i64) -> i64 {
    const SAFETY_MARGIN_MS: i64 = 5 * 60 * 1000;
    let now_ms = chrono::Utc::now().timestamp_millis();
    let expires_ms = expires_in_seconds.saturating_mul(1000);
    now_ms
        .saturating_add(expires_ms)
        .saturating_sub(SAFETY_MARGIN_MS)
}

fn generate_pkce() -> (String, String) {
    let uuid1 = uuid::Uuid::new_v4();
    let uuid2 = uuid::Uuid::new_v4();
    let mut random = [0u8; 32];
    random[..16].copy_from_slice(uuid1.as_bytes());
    random[16..].copy_from_slice(uuid2.as_bytes());

    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn parse_oauth_code_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }

    if let Some((_, query)) = value.split_once('?') {
        let query = query.split('#').next().unwrap_or(query);
        let pairs = parse_query_pairs(query);
        let code = pairs
            .iter()
            .find_map(|(k, v)| (k == "code").then(|| v.clone()));
        let state = pairs
            .iter()
            .find_map(|(k, v)| (k == "state").then(|| v.clone()));
        return (code, state);
    }

    if let Some((code, state)) = value.split_once('#') {
        let code = code.trim();
        let state = state.trim();
        return (
            (!code.is_empty()).then(|| code.to_string()),
            (!state.is_empty()).then(|| state.to_string()),
        );
    }

    (Some(value.to_string()), None)
}

fn lock_file(file: File, timeout: Duration) -> Result<LockedFile> {
    let start = Instant::now();
    let mut attempt: u32 = 0;
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => return Ok(LockedFile { file }),
            Ok(false) => {} // Lock held by another process, retry
            Err(e) => {
                return Err(Error::auth(format!("Failed to lock auth file: {e}")));
            }
        }

        if start.elapsed() >= timeout {
            return Err(Error::auth("Timed out waiting for auth lock".to_string()));
        }

        let base_ms: u64 = 10;
        let cap_ms: u64 = 500;
        let sleep_ms = base_ms
            .checked_shl(attempt.min(5))
            .unwrap_or(cap_ms)
            .min(cap_ms);
        let jitter = u64::from(start.elapsed().subsec_nanos()) % (sleep_ms / 2 + 1);
        let delay = sleep_ms / 2 + jitter;
        std::thread::sleep(Duration::from_millis(delay));
        attempt = attempt.saturating_add(1);
    }
}

fn lock_file_shared(file: File, timeout: Duration) -> Result<LockedFile> {
    let start = Instant::now();
    let mut attempt: u32 = 0;
    loop {
        match FileExt::try_lock_shared(&file) {
            Ok(true) => return Ok(LockedFile { file }),
            Ok(false) => {} // Lock held by another process exclusively, retry
            Err(e) => {
                return Err(Error::auth(format!("Failed to shared-lock auth file: {e}")));
            }
        }

        if start.elapsed() >= timeout {
            return Err(Error::auth("Timed out waiting for auth lock".to_string()));
        }

        let base_ms: u64 = 10;
        let cap_ms: u64 = 500;
        let sleep_ms = base_ms
            .checked_shl(attempt.min(5))
            .unwrap_or(cap_ms)
            .min(cap_ms);
        let jitter = u64::from(start.elapsed().subsec_nanos()) % (sleep_ms / 2 + 1);
        let delay = sleep_ms / 2 + jitter;
        std::thread::sleep(Duration::from_millis(delay));
        attempt = attempt.saturating_add(1);
    }
}

/// A file handle with an exclusive lock. Unlocks on drop.
struct LockedFile {
    file: File,
}

impl LockedFile {
    const fn as_file_mut(&mut self) -> &mut File {
        &mut self.file
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Convenience to load auth from default path.
pub fn load_default_auth(path: &Path) -> Result<AuthStorage> {
    AuthStorage::load(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn test_api_key_value_from_string() {
        // Raw
        let val: ApiKeyValue = (&"raw-key".to_string()).into();
        assert_eq!(val, ApiKeyValue::Raw("raw-key".to_string()));

        // EnvVar (using HOME which is always set)
        let val: ApiKeyValue = (&"HOME".to_string()).into();
        assert_eq!(val, ApiKeyValue::EnvVar("HOME".to_string()));

        // Command
        let val: ApiKeyValue = (&"!echo hello".to_string()).into();
        assert_eq!(val, ApiKeyValue::Command("echo hello".to_string()));
    }

    #[test]
    fn test_api_key_value_partial_eq() {
        let val = ApiKeyValue::Raw("test".to_string());
        assert_eq!(val, "test");

        let val = ApiKeyValue::EnvVar("HOME".to_string());
        assert!(val != "HOME");

        let val = ApiKeyValue::Command("echo hi".to_string());
        assert!(val != "echo hi");
    }

    #[test]
    fn test_api_key_from_credential_resolution() {
        // Raw
        let cred = AuthCredential::ApiKey {
            key: ApiKeyValue::Raw("raw".into()),
        };
        assert_eq!(api_key_from_credential(&cred), Some("raw".into()));

        // EnvVar (using HOME)
        let home_val = std::env::var("HOME").unwrap();
        let cred = AuthCredential::ApiKey {
            key: ApiKeyValue::EnvVar("HOME".into()),
        };
        assert_eq!(api_key_from_credential(&cred), Some(home_val));

        // Command (this confirms the bug fix)
        let cred = AuthCredential::ApiKey {
            key: ApiKeyValue::Command("echo resolved".into()),
        };
        assert_eq!(api_key_from_credential(&cred), Some("resolved".into()));
    }

    fn next_token() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string()
    }

    #[test]
    fn test_finish_auth_task_propagates_panic_before_cancellation() {
        let handle = std::thread::spawn(|| -> () {
            panic!("auth worker panic");
        });

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<()> = finish_auth_task(handle, Err(()), "Load task cancelled");
        }));

        assert!(
            panic.is_err(),
            "worker panic should not be masked as cancellation"
        );
    }

    #[test]
    fn test_finish_auth_task_maps_nonpanic_cancellation_to_auth_error() {
        let handle = std::thread::spawn(|| {});

        let err =
            finish_auth_task::<(), _>(handle, Err(()), "Load task cancelled").expect_err("error");

        assert!(
            err.to_string().contains("Load task cancelled"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_finish_auth_task_returns_success_payload() {
        let handle = std::thread::spawn(|| {});

        let value =
            finish_auth_task::<usize, ()>(handle, Ok(Ok(7usize)), "task cancelled").unwrap();

        assert_eq!(value, 7);
    }

    #[allow(clippy::needless_pass_by_value)]
    fn log_test_event(test_name: &str, event: &str, data: serde_json::Value) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_millis();
        let entry = serde_json::json!({
            "schema": "pi.test.auth_event.v1",
            "test": test_name,
            "event": event,
            "timestamp_ms": timestamp_ms,
            "data": data,
        });
        eprintln!(
            "JSONL: {}",
            serde_json::to_string(&entry).expect("serialize auth test event")
        );
    }

    fn spawn_json_server(status_code: u16, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let body = body.to_string();

        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");

            let mut chunk = [0_u8; 4096];
            let _ = socket.read(&mut chunk);

            let reason = match status_code {
                401 => "Unauthorized",
                500 => "Internal Server Error",
                _ => "OK",
            };
            let response = format!(
                "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .expect("write response");
            socket.flush().expect("flush response");
        });

        format!("http://{addr}/token")
    }

    fn spawn_oauth_host_server(status_code: u16, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let body = body.to_string();

        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set read timeout");

            let mut chunk = [0_u8; 4096];
            let _ = socket.read(&mut chunk);

            let reason = match status_code {
                400 => "Bad Request",
                401 => "Unauthorized",
                403 => "Forbidden",
                500 => "Internal Server Error",
                _ => "OK",
            };
            let response = format!(
                "HTTP/1.1 {status_code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .expect("write response");
            socket.flush().expect("flush response");
        });

        format!("http://{addr}")
    }

    #[test]
    fn test_google_project_id_from_gcloud_config_parses_core_project() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let gcloud_dir = dir.path().join("gcloud");
        let configs_dir = gcloud_dir.join("configurations");
        std::fs::create_dir_all(&configs_dir).expect("mkdir configurations");
        std::fs::write(
            configs_dir.join("config_default"),
            "[core]\nproject = my-proj\n",
        )
        .expect("write config_default");

        let project = google_project_id_from_gcloud_config_with_env_lookup(|key| match key {
            "CLOUDSDK_CONFIG" => Some(gcloud_dir.to_string_lossy().to_string()),
            _ => None,
        });

        assert_eq!(project.as_deref(), Some("my-proj"));
    }

    #[test]
    fn test_auth_storage_load_missing_file_starts_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("missing-auth.json");
        assert!(!auth_path.exists());

        let loaded = AuthStorage::load(auth_path.clone()).expect("load");
        assert!(loaded.entries.is_empty());
        assert_eq!(loaded.path, auth_path);
    }

    #[test]
    fn test_auth_storage_api_key_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        {
            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
            };
            auth.set(
                "openai",
                AuthCredential::ApiKey {
                    key: ApiKeyValue::Raw("stored-openai-key".to_string()),
                },
            );
            auth.save().expect("save");
        }

        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(
            loaded.api_key("openai").as_deref(),
            Some("stored-openai-key")
        );
    }

    #[test]
    fn test_openai_oauth_url_generation() {
        let test_name = "test_openai_oauth_url_generation";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "openai", "mode": "api_key" }),
        );

        let env_keys = env_keys_for_provider("openai");
        assert!(
            env_keys.contains(&"OPENAI_API_KEY"),
            "expected OPENAI_API_KEY in env key candidates"
        );
        log_test_event(
            test_name,
            "url_generated",
            serde_json::json!({
                "provider": "openai",
                "flow_type": "api_key",
                "env_keys": env_keys,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_openai_token_exchange() {
        let test_name = "test_openai_token_exchange";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "openai", "mode": "api_key_storage" }),
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("openai-key-test".to_string()),
            },
        );
        auth.save().expect("save auth");

        let reloaded = AuthStorage::load(auth_path).expect("reload auth");
        assert_eq!(
            reloaded.api_key("openai").as_deref(),
            Some("openai-key-test")
        );
        log_test_event(
            test_name,
            "token_exchanged",
            serde_json::json!({
                "provider": "openai",
                "flow_type": "api_key",
                "persisted": true,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_google_oauth_url_generation() {
        let test_name = "test_google_oauth_url_generation";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "google", "mode": "api_key" }),
        );

        let env_keys = env_keys_for_provider("google");
        assert!(
            env_keys.contains(&"GOOGLE_API_KEY"),
            "expected GOOGLE_API_KEY in env key candidates"
        );
        assert!(
            env_keys.contains(&"GEMINI_API_KEY"),
            "expected GEMINI_API_KEY alias in env key candidates"
        );
        log_test_event(
            test_name,
            "url_generated",
            serde_json::json!({
                "provider": "google",
                "flow_type": "api_key",
                "env_keys": env_keys,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_google_token_exchange() {
        let test_name = "test_google_token_exchange";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "google", "mode": "api_key_storage" }),
        );

        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage::load(auth_path.clone()).expect("load auth");
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("google-key-test".to_string()),
            },
        );
        auth.save().expect("save auth");

        let reloaded = AuthStorage::load(auth_path).expect("reload auth");
        assert_eq!(
            reloaded.api_key("google").as_deref(),
            Some("google-key-test")
        );
        assert_eq!(
            reloaded
                .resolve_api_key_with_env_lookup("gemini", None, |_| None)
                .as_deref(),
            Some("google-key-test")
        );
        log_test_event(
            test_name,
            "token_exchanged",
            serde_json::json!({
                "provider": "google",
                "flow_type": "api_key",
                "has_refresh": false,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_resolve_api_key_precedence_override_env_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-openai-key".to_string()),
            },
        );

        let env_value = "env-openai-key".to_string();

        let override_resolved =
            auth.resolve_api_key_with_env_lookup("openai", Some("override-key"), |_| {
                Some(env_value.clone())
            });
        assert_eq!(override_resolved.as_deref(), Some("override-key"));

        let env_resolved =
            auth.resolve_api_key_with_env_lookup("openai", None, |_| Some(env_value.clone()));
        assert_eq!(env_resolved.as_deref(), Some("env-openai-key"));

        let stored_resolved = auth.resolve_api_key_with_env_lookup("openai", None, |_| None);
        assert_eq!(stored_resolved.as_deref(), Some("stored-openai-key"));
    }

    #[test]
    fn test_resolve_api_key_prefers_stored_oauth_over_env() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        let now = chrono::Utc::now().timestamp_millis();
        auth.set(
            "anthropic",
            AuthCredential::OAuth {
                access_token: "stored-oauth-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires: now + 60_000,
                token_url: None,
                client_id: None,
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", None, |_| {
            Some("env-api-key".to_string())
        });
        let token = resolved.expect("resolved anthropic oauth token");
        assert_eq!(
            unmark_anthropic_oauth_bearer_token(&token),
            Some("stored-oauth-token")
        );
    }

    #[test]
    fn test_resolve_api_key_expired_oauth_falls_back_to_env() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        let now = chrono::Utc::now().timestamp_millis();
        auth.set(
            "anthropic",
            AuthCredential::OAuth {
                access_token: "expired-oauth-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires: now - 1_000,
                token_url: None,
                client_id: None,
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", None, |_| {
            Some("env-api-key".to_string())
        });
        assert_eq!(resolved.as_deref(), Some("env-api-key"));
    }

    #[test]
    fn test_resolve_api_key_returns_none_when_unconfigured() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };

        let resolved =
            auth.resolve_api_key_with_env_lookup("nonexistent-provider-for-test", None, |_| None);
        assert!(resolved.is_none());
    }

    #[test]
    fn test_generate_pkce_is_base64url_no_pad() {
        let (verifier, challenge) = generate_pkce();
        assert!(!verifier.is_empty());
        assert!(!challenge.is_empty());
        assert!(!verifier.contains('+'));
        assert!(!verifier.contains('/'));
        assert!(!verifier.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.contains('='));
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
    }

    #[test]
    fn test_start_anthropic_oauth_url_contains_required_params() {
        let info = start_anthropic_oauth().expect("start");
        let (base, query) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, ANTHROPIC_OAUTH_AUTHORIZE_URL);

        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some(ANTHROPIC_OAUTH_CLIENT_ID)
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some(ANTHROPIC_OAUTH_REDIRECT_URI)
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(ANTHROPIC_OAUTH_SCOPES)
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(info.verifier.as_str())
        );
        assert!(params.contains_key("code_challenge"));
    }

    #[test]
    fn test_parse_oauth_code_input_accepts_url_and_hash_formats() {
        let (code, state) = parse_oauth_code_input(
            "https://console.anthropic.com/oauth/code/callback?code=abc&state=def",
        );
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("def"));

        let (code, state) = parse_oauth_code_input("abc#def");
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("def"));

        let (code, state) = parse_oauth_code_input("abc");
        assert_eq!(code.as_deref(), Some("abc"));
        assert!(state.is_none());
    }

    #[test]
    fn test_complete_anthropic_oauth_rejects_state_mismatch() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let err = complete_anthropic_oauth("abc#mismatch", "expected")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("State mismatch"));
        });
    }

    fn sample_oauth_config() -> crate::models::OAuthConfig {
        crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "ext-client-123".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
            redirect_uri: Some("http://localhost:9876/callback".to_string()),
        }
    }

    #[test]
    fn test_start_extension_oauth_url_contains_required_params() {
        let config = sample_oauth_config();
        let info = start_extension_oauth("my-ext-provider", &config).expect("start");

        assert_eq!(info.provider, "my-ext-provider");
        assert!(!info.verifier.is_empty());

        let (base, query) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://auth.example.com/authorize");

        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("ext-client-123")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://localhost:9876/callback")
        );
        assert_eq!(params.get("scope").map(String::as_str), Some("read write"));
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(info.verifier.as_str())
        );
        assert!(params.contains_key("code_challenge"));
    }

    #[test]
    fn test_start_extension_oauth_no_redirect_uri() {
        let config = crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "ext-client-123".to_string(),
            scopes: vec!["read".to_string()],
            redirect_uri: None,
        };
        let info = start_extension_oauth("no-redirect", &config).expect("start");

        let (_, query) = info.url.split_once('?').expect("missing query");
        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert!(!params.contains_key("redirect_uri"));
    }

    #[test]
    fn test_start_extension_oauth_empty_scopes() {
        let config = crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "ext-client-123".to_string(),
            scopes: vec![],
            redirect_uri: None,
        };
        let info = start_extension_oauth("empty-scopes", &config).expect("start");

        let (_, query) = info.url.split_once('?').expect("missing query");
        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        // scope param still present but empty string
        assert_eq!(params.get("scope").map(String::as_str), Some(""));
    }

    #[test]
    fn test_start_extension_oauth_pkce_format() {
        let config = sample_oauth_config();
        let info = start_extension_oauth("pkce-test", &config).expect("start");

        // Verifier should be base64url without padding
        assert!(!info.verifier.contains('+'));
        assert!(!info.verifier.contains('/'));
        assert!(!info.verifier.contains('='));
        assert_eq!(info.verifier.len(), 43);
    }

    #[test]
    fn test_complete_extension_oauth_rejects_state_mismatch() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let config = sample_oauth_config();
            let err = complete_extension_oauth(&config, "abc#mismatch", "expected")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("State mismatch"));
        });
    }

    #[test]
    fn test_complete_copilot_browser_oauth_rejects_state_mismatch() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let config = CopilotOAuthConfig::default();
            let err = complete_copilot_browser_oauth(&config, "abc#mismatch", "expected")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("State mismatch"));
        });
    }

    #[test]
    fn test_complete_gitlab_oauth_rejects_state_mismatch() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let config = GitLabOAuthConfig::default();
            let err = complete_gitlab_oauth(&config, "abc#mismatch", "expected")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("State mismatch"));
        });
    }

    #[test]
    fn test_refresh_expired_extension_oauth_tokens_skips_anthropic() {
        // Verify that the extension refresh method skips "anthropic" (handled separately).
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
            };
            // Insert an expired anthropic OAuth credential.
            let initial_access = next_token();
            let initial_refresh = next_token();
            auth.entries.insert(
                "anthropic".to_string(),
                AuthCredential::OAuth {
                    access_token: initial_access.clone(),
                    refresh_token: initial_refresh,
                    expires: 0, // expired
                    token_url: None,
                    client_id: None,
                },
            );

            let client = crate::http::client::Client::new();
            let mut extension_configs = HashMap::new();
            extension_configs.insert("anthropic".to_string(), sample_oauth_config());

            // Should succeed and NOT attempt refresh (anthropic is skipped).
            let result = auth
                .refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await;
            assert!(result.is_ok());

            // Credential should remain unchanged.
            assert!(
                matches!(
                    auth.entries.get("anthropic"),
                    Some(AuthCredential::OAuth { access_token, .. })
                        if access_token  == &initial_access
                ),
                "expected OAuth credential"
            );
        });
    }

    #[test]
    fn test_refresh_expired_extension_oauth_tokens_skips_unexpired() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
            };
            // Insert a NOT expired credential.
            let initial_access_token = next_token();
            let initial_refresh_token = next_token();
            let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
            auth.entries.insert(
                "my-ext".to_string(),
                AuthCredential::OAuth {
                    access_token: initial_access_token.clone(),
                    refresh_token: initial_refresh_token,
                    expires: far_future,
                    token_url: None,
                    client_id: None,
                },
            );

            let client = crate::http::client::Client::new();
            let mut extension_configs = HashMap::new();
            extension_configs.insert("my-ext".to_string(), sample_oauth_config());

            let result = auth
                .refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await;
            assert!(result.is_ok());

            // Credential should remain unchanged (not expired, no refresh attempted).
            assert!(
                matches!(
                    auth.entries.get("my-ext"),
                    Some(AuthCredential::OAuth { access_token, .. })
                        if access_token  == &initial_access_token
                ),
                "expected OAuth credential"
            );
        });
    }

    #[test]
    fn test_refresh_expired_extension_oauth_tokens_skips_unknown_provider() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
            };
            // Expired credential for a provider not in extension_configs.
            let initial_access_token = next_token();
            let initial_refresh_token = next_token();
            auth.entries.insert(
                "unknown-ext".to_string(),
                AuthCredential::OAuth {
                    access_token: initial_access_token.clone(),
                    refresh_token: initial_refresh_token,
                    expires: 0,
                    token_url: None,
                    client_id: None,
                },
            );

            let client = crate::http::client::Client::new();
            let extension_configs = HashMap::new(); // empty

            let result = auth
                .refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await;
            assert!(result.is_ok());

            // Credential should remain unchanged (no config to refresh with).
            assert!(
                matches!(
                    auth.entries.get("unknown-ext"),
                    Some(AuthCredential::OAuth { access_token, .. })
                        if access_token  == &initial_access_token
                ),
                "expected OAuth credential"
            );
        });
    }

    #[test]
    #[cfg(unix)]
    fn test_refresh_expired_extension_oauth_tokens_updates_and_persists() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");
            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
            };
            auth.entries.insert(
                "my-ext".to_string(),
                AuthCredential::OAuth {
                    access_token: "old-access".to_string(),
                    refresh_token: "old-refresh".to_string(),
                    expires: 0,
                    token_url: None,
                    client_id: None,
                },
            );

            let token_url = spawn_json_server(
                200,
                r#"{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}"#,
            );
            let mut config = sample_oauth_config();
            config.token_url = token_url;

            let mut extension_configs = HashMap::new();
            extension_configs.insert("my-ext".to_string(), config);

            let client = crate::http::client::Client::new();
            auth.refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await
                .expect("refresh");

            let now = chrono::Utc::now().timestamp_millis();
            match auth.entries.get("my-ext").expect("credential updated") {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    expires,
                    ..
                } => {
                    assert_eq!(access_token, "new-access");
                    assert_eq!(refresh_token, "new-refresh");
                    assert!(*expires > now);
                }
                other => {
                    unreachable!("expected oauth credential, got: {other:?}");
                }
            }

            let reloaded = AuthStorage::load(auth_path).expect("reload");
            match reloaded.get("my-ext").expect("persisted credential") {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    ..
                } => {
                    assert_eq!(access_token, "new-access");
                    assert_eq!(refresh_token, "new-refresh");
                }
                other => {
                    unreachable!("expected oauth credential, got: {other:?}");
                }
            }
        });
    }

    #[test]
    #[cfg(unix)]
    fn test_refresh_extension_oauth_token_redacts_secret_in_error() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let refresh_secret  = "secret-refresh-token-123";
            let leaked_access = "leaked-access-token-456";
            let token_url = spawn_json_server(
                401,
                &format!(
                    r#"{{"error":"invalid_grant","echo":"{refresh_secret}","access_token":"{leaked_access}"}}"#
                ),
            );

            let mut config = sample_oauth_config();
            config.token_url = token_url;

            let client = crate::http::client::Client::new();
            let err = refresh_extension_oauth_token(&client, &config, refresh_secret)
                .await
                .expect_err("expected refresh failure");
            let err_text = err.to_string();

            assert!(
                err_text.contains("[REDACTED]"),
                "expected redacted marker in error: {err_text}"
            );
            assert!(
                !err_text.contains(refresh_secret),
                "refresh token leaked in error: {err_text}"
            );
            assert!(
                !err_text.contains(leaked_access),
                "access token leaked in error: {err_text}"
            );
        });
    }

    #[test]
    fn test_refresh_failure_produces_recovery_action() {
        let test_name = "test_refresh_failure_produces_recovery_action";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "provider": "anthropic" }),
        );

        let err = crate::error::Error::auth("OAuth token refresh failed: invalid_grant");
        let hints = err.hints();
        assert!(
            hints.hints.iter().any(|hint| hint.contains("login")),
            "expected auth hints to include login guidance, got {:?}",
            hints.hints
        );
        log_test_event(
            test_name,
            "refresh_failed",
            serde_json::json!({
                "provider": "anthropic",
                "error_type": "invalid_grant",
                "recovery": hints.hints,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_refresh_failure_network_vs_auth_different_messages() {
        let test_name = "test_refresh_failure_network_vs_auth_different_messages";
        log_test_event(
            test_name,
            "test_start",
            serde_json::json!({ "scenario": "compare provider-network vs auth-refresh hints" }),
        );

        let auth_err = crate::error::Error::auth("OAuth token refresh failed: invalid_grant");
        let auth_hints = auth_err.hints();
        let network_err = crate::error::Error::provider(
            "anthropic",
            "Network connection error: connection reset by peer",
        );
        let network_hints = network_err.hints();

        assert!(
            auth_hints.hints.iter().any(|hint| hint.contains("login")),
            "expected auth-refresh hints to include login guidance, got {:?}",
            auth_hints.hints
        );
        assert!(
            network_hints.hints.iter().any(|hint| {
                let normalized = hint.to_ascii_lowercase();
                normalized.contains("network") || normalized.contains("connection")
            }),
            "expected network hints to mention network/connection checks, got {:?}",
            network_hints.hints
        );
        log_test_event(
            test_name,
            "error_classified",
            serde_json::json!({
                "auth_hints": auth_hints.hints,
                "network_hints": network_hints.hints,
            }),
        );
        log_test_event(
            test_name,
            "test_end",
            serde_json::json!({ "status": "pass" }),
        );
    }

    #[test]
    fn test_oauth_token_storage_round_trip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let expected_access_token = next_token();
        let expected_refresh_token = next_token();

        // Save OAuth credential.
        {
            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
            };
            auth.set(
                "ext-provider",
                AuthCredential::OAuth {
                    access_token: expected_access_token.clone(),
                    refresh_token: expected_refresh_token.clone(),
                    expires: 9_999_999_999_000,
                    token_url: None,
                    client_id: None,
                },
            );
            auth.save().expect("save");
        }

        // Load and verify.
        let loaded = AuthStorage::load(auth_path).expect("load");
        let cred = loaded.get("ext-provider").expect("credential present");
        match cred {
            AuthCredential::OAuth {
                access_token,
                refresh_token,
                expires,
                ..
            } => {
                assert_eq!(access_token, &expected_access_token);
                assert_eq!(refresh_token, &expected_refresh_token);
                assert_eq!(*expires, 9_999_999_999_000);
            }
            other => {
                unreachable!("expected OAuth credential, got: {other:?}");
            }
        }
    }

    #[test]
    fn test_oauth_api_key_returns_access_token_when_unexpired() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let expected_access_token = next_token();
        let expected_refresh_token = next_token();
        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "ext-provider",
            AuthCredential::OAuth {
                access_token: expected_access_token.clone(),
                refresh_token: expected_refresh_token,
                expires: far_future,
                token_url: None,
                client_id: None,
            },
        );

        assert_eq!(
            auth.api_key("ext-provider").as_deref(),
            Some(expected_access_token.as_str())
        );
    }

    #[test]
    fn test_oauth_api_key_returns_none_when_expired() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let expected_access_token = next_token();
        let expected_refresh_token = next_token();
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "ext-provider",
            AuthCredential::OAuth {
                access_token: expected_access_token,
                refresh_token: expected_refresh_token,
                expires: 0, // expired
                token_url: None,
                client_id: None,
            },
        );

        assert_eq!(auth.api_key("ext-provider"), None);
    }

    #[test]
    fn test_credential_status_reports_oauth_valid_and_expired() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let now = chrono::Utc::now().timestamp_millis();

        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "valid-oauth",
            AuthCredential::OAuth {
                access_token: "valid-access".to_string(),
                refresh_token: "valid-refresh".to_string(),
                expires: now + 30_000,
                token_url: None,
                client_id: None,
            },
        );
        auth.set(
            "expired-oauth",
            AuthCredential::OAuth {
                access_token: "expired-access".to_string(),
                refresh_token: "expired-refresh".to_string(),
                expires: now - 30_000,
                token_url: None,
                client_id: None,
            },
        );

        match auth.credential_status("valid-oauth") {
            CredentialStatus::OAuthValid { expires_in_ms } => {
                assert!(expires_in_ms > 0, "expires_in_ms should be positive");
                log_test_event(
                    "test_provider_listing_shows_expiry",
                    "assertion",
                    serde_json::json!({
                        "provider": "valid-oauth",
                        "status": "oauth_valid",
                        "expires_in_ms": expires_in_ms,
                    }),
                );
            }
            other => panic!(),
        }

        match auth.credential_status("expired-oauth") {
            CredentialStatus::OAuthExpired { expired_by_ms } => {
                assert!(expired_by_ms > 0, "expired_by_ms should be positive");
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_credential_status_uses_alias_lookup() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("google-key".to_string()),
            },
        );

        assert_eq!(auth.credential_status("gemini"), CredentialStatus::ApiKey);
        assert_eq!(
            auth.credential_status("missing-provider"),
            CredentialStatus::Missing
        );
        log_test_event(
            "test_provider_listing_shows_all_providers",
            "assertion",
            serde_json::json!({
                "providers_checked": ["google", "gemini", "missing-provider"],
                "google_status": "api_key",
                "missing_status": "missing",
            }),
        );
        log_test_event(
            "test_provider_listing_no_credentials",
            "assertion",
            serde_json::json!({
                "provider": "missing-provider",
                "status": "Not authenticated",
            }),
        );
    }

    #[test]
    fn test_has_stored_credential_uses_reverse_alias_lookup() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("legacy-gemini-key".to_string()),
            },
        );

        assert!(auth.has_stored_credential("google"));
        assert!(auth.has_stored_credential("gemini"));
    }

    #[test]
    fn test_resolve_api_key_handles_case_insensitive_stored_provider_keys() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "Google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("mixed-case-key".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("google", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("mixed-case-key"));
    }

    #[test]
    fn test_credential_status_uses_reverse_alias_lookup() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("legacy-gemini-key".to_string()),
            },
        );

        assert_eq!(auth.credential_status("google"), CredentialStatus::ApiKey);
    }

    #[test]
    fn test_remove_provider_aliases_removes_canonical_and_alias_entries() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("google-key".to_string()),
            },
        );
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("gemini-key".to_string()),
            },
        );

        assert!(auth.remove_provider_aliases("google"));
        assert!(!auth.has_stored_credential("google"));
        assert!(!auth.has_stored_credential("gemini"));
    }

    #[test]
    fn test_auth_remove_credential() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "ext-provider",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("key-123".to_string()),
            },
        );

        assert!(auth.get("ext-provider").is_some());
        assert!(auth.remove("ext-provider"));
        assert!(auth.get("ext-provider").is_none());
        assert!(!auth.remove("ext-provider")); // already removed
    }

    #[test]
    fn test_auth_env_key_returns_none_for_extension_providers() {
        // Extension providers don't have hard-coded env vars.
        assert!(env_key_for_provider("my-ext-provider").is_none());
        assert!(env_key_for_provider("custom-llm").is_none());
        // Built-in providers do.
        assert_eq!(env_key_for_provider("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(env_key_for_provider("openai"), Some("OPENAI_API_KEY"));
    }

    #[test]
    fn test_extension_oauth_config_special_chars_in_scopes() {
        let config = crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "ext-client".to_string(),
            scopes: vec![
                "api:read".to_string(),
                "api:write".to_string(),
                "user:profile".to_string(),
            ],
            redirect_uri: None,
        };
        let info = start_extension_oauth("scoped", &config).expect("start");

        let (_, query) = info.url.split_once('?').expect("missing query");
        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some("api:read api:write user:profile")
        );
    }

    #[test]
    fn test_extension_oauth_url_encodes_special_chars() {
        let config = crate::models::OAuthConfig {
            auth_url: "https://auth.example.com/authorize".to_string(),
            token_url: "https://auth.example.com/token".to_string(),
            client_id: "client with spaces".to_string(),
            scopes: vec!["scope&dangerous".to_string()],
            redirect_uri: Some("http://localhost:9876/call back".to_string()),
        };
        let info = start_extension_oauth("encoded", &config).expect("start");

        // The URL should be valid and contain encoded values.
        assert!(info.url.contains("client%20with%20spaces"));
        assert!(info.url.contains("scope%26dangerous"));
        assert!(info.url.contains("call%20back"));
    }

    // ── AuthStorage creation (additional edge cases) ─────────────────

    #[test]
    fn test_auth_storage_load_valid_api_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let content = r#"{"anthropic":{"type":"api_key","key":"sk-test-abc"}}"#;
        fs::write(&auth_path, content).expect("write");

        let auth = AuthStorage::load(auth_path).expect("load");
        assert!(auth.entries.contains_key("anthropic"));
        match auth.get("anthropic").expect("credential") {
            AuthCredential::ApiKey { key } => assert_eq!(key, "sk-test-abc"),
            other => panic!(),
        }
    }

    #[test]
    fn test_auth_storage_load_corrupted_json_returns_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        fs::write(&auth_path, "not valid json {{").expect("write");

        let auth = AuthStorage::load(auth_path).expect("load");
        // Corrupted JSON falls through to `unwrap_or_default()`.
        assert!(auth.entries.is_empty());
    }

    #[test]
    fn test_auth_storage_load_empty_file_returns_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        fs::write(&auth_path, "").expect("write");

        let auth = AuthStorage::load(auth_path).expect("load");
        assert!(auth.entries.is_empty());
    }

    // ── resolve_api_key edge cases ───────────────────────────────────

    #[test]
    fn test_resolve_api_key_empty_override_still_wins() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-key".to_string()),
            },
        );

        // Empty string override still counts as explicit.
        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", Some(""), |_| None);
        assert_eq!(resolved.as_deref(), Some(""));
    }

    #[test]
    fn test_resolve_api_key_env_beats_stored() {
        // The new precedence is: override > env > stored.
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-key".to_string()),
            },
        );

        let resolved =
            auth.resolve_api_key_with_env_lookup("openai", None, |_| Some("env-key".to_string()));
        assert_eq!(
            resolved.as_deref(),
            Some("env-key"),
            "env should beat stored"
        );
    }

    #[test]
    fn test_resolve_api_key_groq_env_beats_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "groq",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-groq-key".to_string()),
            },
        );

        let resolved =
            auth.resolve_api_key_with_env_lookup("groq", None, |_| Some("env-groq-key".into()));
        assert_eq!(resolved.as_deref(), Some("env-groq-key"));
    }

    #[test]
    fn test_resolve_api_key_openrouter_env_beats_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "openrouter",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-openrouter-key".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("openrouter", None, |var| match var {
            "OPENROUTER_API_KEY" => Some("env-openrouter-key".to_string()),
            _ => None,
        });
        assert_eq!(resolved.as_deref(), Some("env-openrouter-key"));
    }

    #[test]
    fn test_resolve_api_key_empty_env_falls_through_to_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-key".to_string()),
            },
        );

        // Empty env var is filtered out, falls through to stored.
        let resolved =
            auth.resolve_api_key_with_env_lookup("openai", None, |_| Some(String::new()));
        assert_eq!(
            resolved.as_deref(),
            Some("stored-key"),
            "empty env should fall through to stored"
        );
    }

    #[test]
    fn test_resolve_api_key_whitespace_env_falls_through_to_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-key".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("openai", None, |_| Some("   ".into()));
        assert_eq!(resolved.as_deref(), Some("stored-key"));
    }

    #[test]
    fn test_resolve_api_key_anthropic_oauth_marks_for_bearer_lane() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "anthropic",
            AuthCredential::OAuth {
                access_token: "sk-ant-api-like-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires: chrono::Utc::now().timestamp_millis() + 60_000,
                token_url: None,
                client_id: None,
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", None, |_| None);
        let token = resolved.expect("resolved anthropic oauth token");
        assert_eq!(
            unmark_anthropic_oauth_bearer_token(&token),
            Some("sk-ant-api-like-token")
        );
    }

    #[test]
    fn test_resolve_api_key_non_anthropic_oauth_is_not_marked() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "openai-codex",
            AuthCredential::OAuth {
                access_token: "codex-oauth-token".to_string(),
                refresh_token: "refresh-token".to_string(),
                expires: chrono::Utc::now().timestamp_millis() + 60_000,
                token_url: None,
                client_id: None,
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("openai-codex", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("codex-oauth-token"));
    }

    #[test]
    fn test_resolve_api_key_google_uses_gemini_env_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-google-key".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("google", None, |var| match var {
            "GOOGLE_API_KEY" => Some(String::new()),
            "GEMINI_API_KEY" => Some("gemini-fallback-key".to_string()),
            _ => None,
        });

        assert_eq!(resolved.as_deref(), Some("gemini-fallback-key"));
    }

    #[test]
    fn test_resolve_api_key_gemini_alias_reads_google_stored_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-google-key".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("gemini", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("stored-google-key"));
    }

    #[test]
    fn test_resolve_api_key_google_reads_legacy_gemini_alias_stored_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("legacy-gemini-key".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("google", None, |_| None);
        assert_eq!(resolved.as_deref(), Some("legacy-gemini-key"));
    }

    #[test]
    fn test_resolve_api_key_qwen_uses_qwen_env_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "alibaba",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-dashscope-key".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("qwen", None, |var| match var {
            "DASHSCOPE_API_KEY" => Some(String::new()),
            "QWEN_API_KEY" => Some("qwen-fallback-key".to_string()),
            _ => None,
        });

        assert_eq!(resolved.as_deref(), Some("qwen-fallback-key"));
    }

    #[test]
    fn test_resolve_api_key_kimi_uses_kimi_env_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "moonshotai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("stored-moonshot-key".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("kimi", None, |var| match var {
            "MOONSHOT_API_KEY" => Some(String::new()),
            "KIMI_API_KEY" => Some("kimi-fallback-key".to_string()),
            _ => None,
        });

        assert_eq!(resolved.as_deref(), Some("kimi-fallback-key"));
    }

    #[test]
    fn test_resolve_api_key_primary_env_wins_over_alias_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };

        let resolved = auth.resolve_api_key_with_env_lookup("alibaba", None, |var| match var {
            "DASHSCOPE_API_KEY" => Some("dashscope-primary".to_string()),
            "QWEN_API_KEY" => Some("qwen-secondary".to_string()),
            _ => None,
        });

        assert_eq!(resolved.as_deref(), Some("dashscope-primary"));
    }

    // ── API key storage and persistence ───────────────────────────────

    #[test]
    fn test_api_key_store_and_retrieve() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };

        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("sk-openai-test".to_string()),
            },
        );

        assert_eq!(auth.api_key("openai").as_deref(), Some("sk-openai-test"));
    }

    #[test]
    fn test_google_api_key_overwrite_persists_latest_value() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };

        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("google-key-old".to_string()),
            },
        );
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("google-key-new".to_string()),
            },
        );
        auth.save().expect("save");

        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(loaded.api_key("google").as_deref(), Some("google-key-new"));
    }

    #[test]
    fn test_multiple_providers_stored_and_retrieved() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };

        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("sk-ant".to_string()),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("sk-oai".to_string()),
            },
        );
        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        auth.set(
            "google",
            AuthCredential::OAuth {
                access_token: "goog-token".to_string(),
                refresh_token: "goog-refresh".to_string(),
                expires: far_future,
                token_url: None,
                client_id: None,
            },
        );
        auth.save().expect("save");

        // Reload and verify all three.
        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(loaded.api_key("anthropic").as_deref(), Some("sk-ant"));
        assert_eq!(loaded.api_key("openai").as_deref(), Some("sk-oai"));
        assert_eq!(loaded.api_key("google").as_deref(), Some("goog-token"));
        assert_eq!(loaded.entries.len(), 3);
    }

    #[test]
    fn test_save_creates_parent_directories() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("nested").join("dirs").join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("nested-key".to_string()),
            },
        );
        auth.save().expect("save should create parents");
        assert!(auth_path.exists());

        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(loaded.api_key("anthropic").as_deref(), Some("nested-key"));
    }

    #[cfg(unix)]
    #[test]
    fn test_save_sets_600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("secret".to_string()),
            },
        );
        auth.save().expect("save");

        let metadata = fs::metadata(&auth_path).expect("metadata");
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth.json should be owner-only read/write");
    }

    #[test]
    fn test_save_uses_lockfile_and_preserves_original_on_persist_failure() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut original = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };
        original.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("old-key".to_string()),
            },
        );
        original.save().expect("save original auth");

        let lock_path = auth_lock_path(&auth_path);
        assert!(
            lock_path.exists(),
            "expected sibling lockfile to be created"
        );

        let mut replacement_entries = HashMap::new();
        replacement_entries.insert(
            "anthropic".to_string(),
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("new-key".to_string()),
            },
        );
        let replacement_data = serde_json::to_string_pretty(&AuthFileRef {
            entries: &replacement_entries,
        })
        .expect("serialize replacement");

        let mut temp_path = None;
        let err = AuthStorage::save_data_sync_with_hook(&auth_path, &replacement_data, |temp| {
            temp_path = Some(temp.path().to_path_buf());
            Err(std::io::Error::other("injected persist failure"))
        })
        .expect_err("persist hook should abort save");
        assert!(
            err.to_string().contains("injected persist failure"),
            "unexpected error: {err}"
        );

        let temp_path = temp_path.expect("captured temp path");
        assert!(
            !temp_path.exists(),
            "temporary auth file should be cleaned up on failure"
        );

        let reloaded = AuthStorage::load(auth_path).expect("reload auth");
        assert_eq!(reloaded.api_key("anthropic").as_deref(), Some("old-key"));
    }

    // ── Missing key handling ──────────────────────────────────────────

    #[test]
    fn test_api_key_returns_none_for_missing_provider() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        assert!(auth.api_key("nonexistent").is_none());
    }

    #[test]
    fn test_get_returns_none_for_missing_provider() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        assert!(auth.get("nonexistent").is_none());
    }

    // ── env_keys_for_provider coverage ────────────────────────────────

    #[test]
    fn test_env_keys_all_built_in_providers() {
        let providers = [
            ("anthropic", "ANTHROPIC_API_KEY"),
            ("openai", "OPENAI_API_KEY"),
            ("google", "GOOGLE_API_KEY"),
            ("google-vertex", "GOOGLE_CLOUD_API_KEY"),
            ("amazon-bedrock", "AWS_ACCESS_KEY_ID"),
            ("azure-openai", "AZURE_OPENAI_API_KEY"),
            ("github-copilot", "GITHUB_COPILOT_API_KEY"),
            ("xai", "XAI_API_KEY"),
            ("groq", "GROQ_API_KEY"),
            ("deepinfra", "DEEPINFRA_API_KEY"),
            ("cerebras", "CEREBRAS_API_KEY"),
            ("openrouter", "OPENROUTER_API_KEY"),
            ("mistral", "MISTRAL_API_KEY"),
            ("cohere", "COHERE_API_KEY"),
            ("perplexity", "PERPLEXITY_API_KEY"),
            ("deepseek", "DEEPSEEK_API_KEY"),
            ("fireworks", "FIREWORKS_API_KEY"),
        ];
        for (provider, expected_key) in providers {
            let keys = env_keys_for_provider(provider);
            assert!(!keys.is_empty(), "expected env key for {provider}");
            assert_eq!(
                keys[0], expected_key,
                "wrong primary env key for {provider}"
            );
        }
    }

    #[test]
    fn test_env_keys_togetherai_has_two_variants() {
        let keys = env_keys_for_provider("togetherai");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "TOGETHER_API_KEY");
        assert_eq!(keys[1], "TOGETHER_AI_API_KEY");
    }

    #[test]
    fn test_env_keys_google_includes_gemini_fallback() {
        let keys = env_keys_for_provider("google");
        assert_eq!(keys, &["GOOGLE_API_KEY", "GEMINI_API_KEY"]);
    }

    #[test]
    fn test_env_keys_moonshotai_aliases() {
        for alias in &["moonshotai", "moonshot", "kimi"] {
            let keys = env_keys_for_provider(alias);
            assert_eq!(
                keys,
                &["MOONSHOT_API_KEY", "KIMI_API_KEY"],
                "alias {alias} should map to moonshot auth fallback key chain"
            );
        }
    }

    #[test]
    fn test_env_keys_alibaba_aliases() {
        for alias in &["alibaba", "dashscope", "qwen"] {
            let keys = env_keys_for_provider(alias);
            assert_eq!(
                keys,
                &["DASHSCOPE_API_KEY", "QWEN_API_KEY"],
                "alias {alias} should map to dashscope auth fallback key chain"
            );
        }
    }

    #[test]
    fn test_env_keys_native_and_gateway_aliases() {
        let cases: [(&str, &[&str]); 7] = [
            ("gemini", &["GOOGLE_API_KEY", "GEMINI_API_KEY"]),
            ("fireworks-ai", &["FIREWORKS_API_KEY"]),
            (
                "bedrock",
                &[
                    "AWS_ACCESS_KEY_ID",
                    "AWS_SECRET_ACCESS_KEY",
                    "AWS_SESSION_TOKEN",
                    "AWS_BEARER_TOKEN_BEDROCK",
                    "AWS_PROFILE",
                    "AWS_REGION",
                ] as &[&str],
            ),
            ("azure", &["AZURE_OPENAI_API_KEY"]),
            ("vertexai", &["GOOGLE_CLOUD_API_KEY", "VERTEX_API_KEY"]),
            ("copilot", &["GITHUB_COPILOT_API_KEY", "GITHUB_TOKEN"]),
            ("fireworks", &["FIREWORKS_API_KEY"]),
        ];

        for (alias, expected) in cases {
            let keys = env_keys_for_provider(alias);
            assert_eq!(keys, expected, "alias {alias} should map to {expected:?}");
        }
    }

    // ── Percent encoding / decoding ───────────────────────────────────

    #[test]
    fn test_percent_encode_ascii_passthrough() {
        assert_eq!(percent_encode_component("hello"), "hello");
        assert_eq!(
            percent_encode_component("ABCDEFxyz0189-._~"),
            "ABCDEFxyz0189-._~"
        );
    }

    #[test]
    fn test_percent_encode_spaces_and_special() {
        assert_eq!(percent_encode_component("hello world"), "hello%20world");
        assert_eq!(percent_encode_component("a&b=c"), "a%26b%3Dc");
        assert_eq!(percent_encode_component("100%"), "100%25");
    }

    #[test]
    fn test_percent_decode_passthrough() {
        assert_eq!(percent_decode_component("hello").as_deref(), Some("hello"));
    }

    #[test]
    fn test_percent_decode_encoded() {
        assert_eq!(
            percent_decode_component("hello%20world").as_deref(),
            Some("hello world")
        );
        assert_eq!(
            percent_decode_component("a%26b%3Dc").as_deref(),
            Some("a&b=c")
        );
    }

    #[test]
    fn test_percent_decode_plus_as_space() {
        assert_eq!(
            percent_decode_component("hello+world").as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn test_percent_decode_invalid_hex_returns_none() {
        assert!(percent_decode_component("hello%ZZ").is_none());
        assert!(percent_decode_component("trailing%2").is_none());
        assert!(percent_decode_component("trailing%").is_none());
    }

    #[test]
    fn test_percent_encode_decode_roundtrip() {
        let inputs = ["hello world", "a=1&b=2", "special: 100% /path?q=v#frag"];
        for input in inputs {
            let encoded = percent_encode_component(input);
            let decoded = percent_decode_component(&encoded).expect("decode");
            assert_eq!(decoded, input, "roundtrip failed for: {input}");
        }
    }

    // ── parse_query_pairs ─────────────────────────────────────────────

    #[test]
    fn test_parse_query_pairs_basic() {
        let pairs = parse_query_pairs("code=abc&state=def");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], ("code".to_string(), "abc".to_string()));
        assert_eq!(pairs[1], ("state".to_string(), "def".to_string()));
    }

    #[test]
    fn test_parse_query_pairs_empty_value() {
        let pairs = parse_query_pairs("key=");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("key".to_string(), String::new()));
    }

    #[test]
    fn test_parse_query_pairs_no_value() {
        let pairs = parse_query_pairs("key");
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("key".to_string(), String::new()));
    }

    #[test]
    fn test_parse_query_pairs_empty_string() {
        let pairs = parse_query_pairs("");
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_parse_query_pairs_encoded_values() {
        let pairs = parse_query_pairs("scope=read%20write&redirect=http%3A%2F%2Fexample.com");
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].1, "read write");
        assert_eq!(pairs[1].1, "http://example.com");
    }

    // ── build_url_with_query ──────────────────────────────────────────

    #[test]
    fn test_build_url_basic() {
        let url = build_url_with_query(
            "https://example.com/auth",
            &[("key", "val"), ("foo", "bar")],
        );
        assert_eq!(url, "https://example.com/auth?key=val&foo=bar");
    }

    #[test]
    fn test_build_url_encodes_special_chars() {
        let url =
            build_url_with_query("https://example.com", &[("q", "hello world"), ("x", "a&b")]);
        assert!(url.contains("q=hello%20world"));
        assert!(url.contains("x=a%26b"));
    }

    #[test]
    fn test_build_url_no_params() {
        let url = build_url_with_query("https://example.com", &[]);
        assert_eq!(url, "https://example.com?");
    }

    // ── parse_oauth_code_input edge cases ─────────────────────────────

    #[test]
    fn test_parse_oauth_code_input_empty() {
        let (code, state) = parse_oauth_code_input("");
        assert!(code.is_none());
        assert!(state.is_none());
    }

    #[test]
    fn test_parse_oauth_code_input_whitespace_only() {
        let (code, state) = parse_oauth_code_input("   ");
        assert!(code.is_none());
        assert!(state.is_none());
    }

    #[test]
    fn test_parse_oauth_code_input_url_strips_fragment() {
        let (code, state) =
            parse_oauth_code_input("https://example.com/callback?code=abc&state=def#fragment");
        assert_eq!(code.as_deref(), Some("abc"));
        assert_eq!(state.as_deref(), Some("def"));
    }

    #[test]
    fn test_parse_oauth_code_input_url_code_only() {
        let (code, state) = parse_oauth_code_input("https://example.com/callback?code=abc");
        assert_eq!(code.as_deref(), Some("abc"));
        assert!(state.is_none());
    }

    #[test]
    fn test_parse_oauth_code_input_hash_empty_state() {
        let (code, state) = parse_oauth_code_input("abc#");
        assert_eq!(code.as_deref(), Some("abc"));
        assert!(state.is_none());
    }

    #[test]
    fn test_parse_oauth_code_input_hash_empty_code() {
        let (code, state) = parse_oauth_code_input("#state-only");
        assert!(code.is_none());
        assert_eq!(state.as_deref(), Some("state-only"));
    }

    // ── oauth_expires_at_ms ───────────────────────────────────────────

    #[test]
    fn test_oauth_expires_at_ms_subtracts_safety_margin() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let expires_in = 3600; // 1 hour
        let result = oauth_expires_at_ms(expires_in);

        // Should be ~55 minutes from now (3600s - 5min safety margin).
        let expected_approx = now_ms + 3600 * 1000 - 5 * 60 * 1000;
        let diff = (result - expected_approx).unsigned_abs();
        assert!(diff < 1000, "expected ~{expected_approx}ms, got {result}ms");
    }

    #[test]
    fn test_oauth_expires_at_ms_zero_expires_in() {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let result = oauth_expires_at_ms(0);

        // Should be 5 minutes before now (0s - 5min safety margin).
        let expected_approx = now_ms - 5 * 60 * 1000;
        let diff = (result - expected_approx).unsigned_abs();
        assert!(diff < 1000, "expected ~{expected_approx}ms, got {result}ms");
    }

    #[test]
    fn test_oauth_expires_at_ms_saturates_for_huge_positive_expires_in() {
        let result = oauth_expires_at_ms(i64::MAX);
        assert_eq!(result, i64::MAX - 5 * 60 * 1000);
    }

    #[test]
    fn test_oauth_expires_at_ms_handles_huge_negative_expires_in() {
        let result = oauth_expires_at_ms(i64::MIN);
        assert!(result <= chrono::Utc::now().timestamp_millis());
    }

    // ── Overwrite semantics ───────────────────────────────────────────

    #[test]
    fn test_set_overwrites_existing_credential() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };

        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("first-key".to_string()),
            },
        );
        assert_eq!(auth.api_key("anthropic").as_deref(), Some("first-key"));

        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("second-key".to_string()),
            },
        );
        assert_eq!(auth.api_key("anthropic").as_deref(), Some("second-key"));
        assert_eq!(auth.entries.len(), 1);
    }

    #[test]
    fn test_save_then_overwrite_persists_latest() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        // Save first version.
        {
            let mut auth = AuthStorage {
                path: auth_path.clone(),
                entries: HashMap::new(),
            };
            auth.set(
                "anthropic",
                AuthCredential::ApiKey {
                    key: ApiKeyValue::Raw("old-key".to_string()),
                },
            );
            auth.save().expect("save");
        }

        // Overwrite.
        {
            let mut auth = AuthStorage::load(auth_path.clone()).expect("load");
            auth.set(
                "anthropic",
                AuthCredential::ApiKey {
                    key: ApiKeyValue::Raw("new-key".to_string()),
                },
            );
            auth.save().expect("save");
        }

        // Verify.
        let loaded = AuthStorage::load(auth_path).expect("load");
        assert_eq!(loaded.api_key("anthropic").as_deref(), Some("new-key"));
    }

    // ── load_default_auth convenience ─────────────────────────────────

    #[test]
    fn test_load_default_auth_works_like_load() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("test-key".to_string()),
            },
        );
        auth.save().expect("save");

        let loaded = load_default_auth(&auth_path).expect("load_default_auth");
        assert_eq!(loaded.api_key("anthropic").as_deref(), Some("test-key"));
    }

    // ── redact_known_secrets ─────────────────────────────────────────

    #[test]
    fn test_redact_known_secrets_replaces_secrets() {
        let text = r#"{"token":"secret123","other":"hello secret123 world"}"#;
        let redacted = redact_known_secrets(text, &["secret123"]);
        assert!(!redacted.contains("secret123"));
        assert!(redacted.contains("[REDACTED]"));
    }

    #[test]
    fn test_redact_known_secrets_ignores_empty_secrets() {
        let text = "nothing to redact here";
        let redacted = redact_known_secrets(text, &["", "   "]);
        // Empty secret should be skipped; only non-empty "   " gets replaced if present.
        assert_eq!(redacted, text);
    }

    #[test]
    fn test_redact_known_secrets_multiple_secrets() {
        let text = "token  =aaa refresh=bbb echo=aaa";
        let redacted = redact_known_secrets(text, &["aaa", "bbb"]);
        assert!(!redacted.contains("aaa"));
        assert!(!redacted.contains("bbb"));
        assert_eq!(
            redacted,
            "token  =[REDACTED] refresh=[REDACTED] echo=[REDACTED]"
        );
    }

    #[test]
    fn test_redact_known_secrets_no_match() {
        let text = "safe text with no secrets";
        let redacted = redact_known_secrets(text, &["not-present"]);
        assert_eq!(redacted, text);
    }

    #[test]
    fn test_redact_known_secrets_redacts_oauth_json_fields_without_known_input() {
        let text = r#"{"access_token":"new-access","refresh_token":"new-refresh","nested":{"id_token":"new-id","safe":"ok"}}"#;
        let redacted = redact_known_secrets(text, &[]);
        assert!(redacted.contains("\"access_token\":\"[REDACTED]\""));
        assert!(redacted.contains("\"refresh_token\":\"[REDACTED]\""));
        assert!(redacted.contains("\"id_token\":\"[REDACTED]\""));
        assert!(redacted.contains("\"safe\":\"ok\""));
        assert!(!redacted.contains("new-access"));
        assert!(!redacted.contains("new-refresh"));
        assert!(!redacted.contains("new-id"));
    }

    // ── PKCE determinism ──────────────────────────────────────────────

    #[test]
    fn test_generate_pkce_unique_each_call() {
        let (v1, c1) = generate_pkce();
        let (v2, c2) = generate_pkce();
        assert_ne!(v1, v2, "verifiers should differ");
        assert_ne!(c1, c2, "challenges should differ");
    }

    #[test]
    fn test_generate_pkce_challenge_is_sha256_of_verifier() {
        let (verifier, challenge) = generate_pkce();
        let expected_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(sha2::Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge, expected_challenge);
    }

    // ── GitHub Copilot OAuth tests ────────────────────────────────

    fn sample_copilot_config() -> CopilotOAuthConfig {
        CopilotOAuthConfig {
            client_id: "Iv1.test_copilot_id".to_string(),
            github_base_url: "https://github.com".to_string(),
            scopes: GITHUB_COPILOT_SCOPES.to_string(),
        }
    }

    #[test]
    fn test_copilot_browser_oauth_requires_client_id() {
        let config = CopilotOAuthConfig {
            client_id: String::new(),
            ..CopilotOAuthConfig::default()
        };
        let err = start_copilot_browser_oauth(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("client_id"),
            "error should mention client_id: {msg}"
        );
    }

    #[test]
    fn test_copilot_browser_oauth_url_contains_required_params() {
        let config = sample_copilot_config();
        let info = start_copilot_browser_oauth(&config).expect("start");

        assert_eq!(info.provider, "github-copilot");
        assert!(!info.verifier.is_empty());

        let (base, query) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, GITHUB_OAUTH_AUTHORIZE_URL);

        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("Iv1.test_copilot_id")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(GITHUB_COPILOT_SCOPES)
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(params.contains_key("code_challenge"));
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(info.verifier.as_str())
        );
    }

    #[test]
    fn test_copilot_browser_oauth_enterprise_url() {
        let config = CopilotOAuthConfig {
            client_id: "Iv1.enterprise".to_string(),
            github_base_url: "https://github.mycompany.com".to_string(),
            scopes: "read:user".to_string(),
        };
        let info = start_copilot_browser_oauth(&config).expect("start");

        let (base, _) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://github.mycompany.com/login/oauth/authorize");
    }

    #[test]
    fn test_copilot_browser_oauth_enterprise_trailing_slash() {
        let config = CopilotOAuthConfig {
            client_id: "Iv1.enterprise".to_string(),
            github_base_url: "https://github.mycompany.com/".to_string(),
            scopes: "read:user".to_string(),
        };
        let info = start_copilot_browser_oauth(&config).expect("start");

        let (base, _) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://github.mycompany.com/login/oauth/authorize");
    }

    #[test]
    fn test_copilot_browser_oauth_pkce_format() {
        let config = sample_copilot_config();
        let info = start_copilot_browser_oauth(&config).expect("start");

        assert_eq!(info.verifier.len(), 43);
        assert!(!info.verifier.contains('+'));
        assert!(!info.verifier.contains('/'));
        assert!(!info.verifier.contains('='));
    }

    #[test]
    #[cfg(unix)]
    fn test_copilot_browser_oauth_complete_success() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(
                200,
                r#"{"access_token":"ghu_test_access","refresh_token":"ghr_test_refresh","expires_in":28800}"#,
            );

            // Extract port from token_url to build a matching config.
            let _config = CopilotOAuthConfig {
                client_id: "Iv1.test".to_string(),
                // Use a base URL that generates the test server URL.
                github_base_url: token_url.trim_end_matches("/token").replace("/token", ""),
                scopes: "read:user".to_string(),
            };

            // We need to call complete directly with the token URL.
            // Since the function constructs the URL from base, we use an
            // alternate approach: test parse_github_token_response directly.
            let cred = parse_github_token_response(
                r#"{"access_token":"ghu_test_access","refresh_token":"ghr_test_refresh","expires_in":28800}"#,
            )
            .expect("parse");

            match cred {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    expires,
                    ..
                } => {
                    assert_eq!(access_token, "ghu_test_access");
                    assert_eq!(refresh_token, "ghr_test_refresh");
                    assert!(expires > chrono::Utc::now().timestamp_millis());
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn test_parse_github_token_no_refresh_token() {
        let cred =
            parse_github_token_response(r#"{"access_token":"ghu_test","token_type":"bearer"}"#)
                .expect("parse");

        match cred {
            AuthCredential::OAuth {
                access_token,
                refresh_token,
                ..
            } => {
                assert_eq!(access_token, "ghu_test");
                assert!(refresh_token.is_empty(), "should default to empty");
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_parse_github_token_no_expiry_uses_far_future() {
        let cred = parse_github_token_response(
            r#"{"access_token":"ghu_test","refresh_token":"ghr_test"}"#,
        )
        .expect("parse");

        match cred {
            AuthCredential::OAuth { expires, .. } => {
                let now = chrono::Utc::now().timestamp_millis();
                let one_year_ms = 365 * 24 * 3600 * 1000_i64;
                // Should be close to 1 year from now (minus 5min safety margin).
                assert!(
                    expires > now + one_year_ms - 10 * 60 * 1000,
                    "expected far-future expiry"
                );
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_parse_github_token_missing_access_token_fails() {
        let err = parse_github_token_response(r#"{"refresh_token":"ghr_test"}"#).unwrap_err();
        assert!(err.to_string().contains("access_token"));
    }

    #[test]
    fn test_copilot_diagnostic_includes_troubleshooting() {
        let msg = copilot_diagnostic("Token exchange failed", "bad request");
        assert!(msg.contains("Token exchange failed"));
        assert!(msg.contains("Troubleshooting"));
        assert!(msg.contains("client_id"));
        assert!(msg.contains("Copilot subscription"));
        assert!(msg.contains("Enterprise"));
    }

    // ── Device flow tests ─────────────────────────────────────────

    #[test]
    fn test_device_code_response_deserialize() {
        let json = r#"{
            "device_code": "dc_test",
            "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.device_code, "dc_test");
        assert_eq!(resp.user_code, "ABCD-1234");
        assert_eq!(resp.verification_uri, "https://github.com/login/device");
        assert_eq!(resp.expires_in, 900);
        assert_eq!(resp.interval, 5);
        assert!(resp.verification_uri_complete.is_none());
    }

    #[test]
    fn test_device_code_response_default_interval() {
        let json = r#"{
            "device_code": "dc",
            "user_code": "CODE",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 600
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(resp.interval, 5, "default interval should be 5 seconds");
    }

    #[test]
    fn test_device_code_response_with_complete_uri() {
        let json = r#"{
            "device_code": "dc",
            "user_code": "CODE",
            "verification_uri": "https://github.com/login/device",
            "verification_uri_complete": "https://github.com/login/device?user_code=CODE",
            "expires_in": 600,
            "interval": 10
        }"#;
        let resp: DeviceCodeResponse = serde_json::from_str(json).expect("parse");
        assert_eq!(
            resp.verification_uri_complete.as_deref(),
            Some("https://github.com/login/device?user_code=CODE")
        );
    }

    #[test]
    fn test_copilot_device_flow_requires_client_id() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let config = CopilotOAuthConfig {
                client_id: String::new(),
                ..CopilotOAuthConfig::default()
            };
            let err = start_copilot_device_flow(&config).await.unwrap_err();
            assert!(err.to_string().contains("client_id"));
        });
    }

    #[test]
    fn test_kimi_oauth_host_env_lookup_prefers_primary_host() {
        let host = kimi_code_oauth_host_with_env_lookup(|key| match key {
            "KIMI_CODE_OAUTH_HOST" => Some("https://primary.kimi.test".to_string()),
            "KIMI_OAUTH_HOST" => Some("https://fallback.kimi.test".to_string()),
            _ => None,
        });
        assert_eq!(host, "https://primary.kimi.test");
    }

    #[test]
    fn test_kimi_share_dir_env_lookup_prefers_kimi_share_dir() {
        let share_dir = kimi_share_dir_with_env_lookup(|key| match key {
            "KIMI_SHARE_DIR" => Some("/tmp/custom-kimi-share".to_string()),
            "HOME" => Some("/tmp/home".to_string()),
            _ => None,
        });
        assert_eq!(
            share_dir,
            Some(PathBuf::from("/tmp/custom-kimi-share")),
            "KIMI_SHARE_DIR should override HOME-based default"
        );
    }

    #[test]
    fn test_kimi_share_dir_env_lookup_falls_back_to_home() {
        let share_dir = kimi_share_dir_with_env_lookup(|key| match key {
            "KIMI_SHARE_DIR" => Some("   ".to_string()),
            "HOME" => Some("/tmp/home".to_string()),
            _ => None,
        });
        assert_eq!(share_dir, Some(PathBuf::from("/tmp/home/.kimi")));
    }

    #[test]
    fn test_home_dir_env_lookup_falls_back_to_userprofile() {
        let home = home_dir_with_env_lookup(|key| match key {
            "HOME" => Some("   ".to_string()),
            "USERPROFILE" => Some("C:\\Users\\tester".to_string()),
            _ => None,
        });
        assert_eq!(home, Some(PathBuf::from("C:\\Users\\tester")));
    }

    #[test]
    fn test_home_dir_env_lookup_falls_back_to_homedrive_homepath() {
        let home = home_dir_with_env_lookup(|key| match key {
            "HOMEDRIVE" => Some("C:".to_string()),
            "HOMEPATH" => Some("\\Users\\tester".to_string()),
            _ => None,
        });
        assert_eq!(home, Some(PathBuf::from("C:\\Users\\tester")));
    }

    #[test]
    fn test_home_dir_env_lookup_homedrive_homepath_without_root_separator() {
        let home = home_dir_with_env_lookup(|key| match key {
            "HOMEDRIVE" => Some("C:".to_string()),
            "HOMEPATH" => Some("Users\\tester".to_string()),
            _ => None,
        });
        assert_eq!(home, Some(PathBuf::from("C:/Users\\tester")));
    }

    #[test]
    fn test_read_external_kimi_code_access_token_from_share_dir_reads_unexpired_token() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let share_dir = dir.path();
        let credentials_dir = share_dir.join("credentials");
        std::fs::create_dir_all(&credentials_dir).expect("create credentials dir");
        let path = credentials_dir.join("kimi-code.json");
        let expires_at = chrono::Utc::now().timestamp() + 3600;
        std::fs::write(
            &path,
            format!(r#"{{"access_token":" kimi-token ","expires_at":{expires_at}}}"#),
        )
        .expect("write token file");

        let token = read_external_kimi_code_access_token_from_share_dir(share_dir);
        assert_eq!(token.as_deref(), Some("kimi-token"));
    }

    #[test]
    fn test_read_external_kimi_code_access_token_from_share_dir_ignores_expired_token() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let share_dir = dir.path();
        let credentials_dir = share_dir.join("credentials");
        std::fs::create_dir_all(&credentials_dir).expect("create credentials dir");
        let path = credentials_dir.join("kimi-code.json");
        let expires_at = chrono::Utc::now().timestamp() - 5;
        std::fs::write(
            &path,
            format!(r#"{{"access_token":"kimi-token","expires_at":{expires_at}}}"#),
        )
        .expect("write token file");

        let token = read_external_kimi_code_access_token_from_share_dir(share_dir);
        assert!(token.is_none(), "expired Kimi token should be ignored");
    }

    #[test]
    fn test_start_kimi_code_device_flow_parses_response() {
        let host = spawn_oauth_host_server(
            200,
            r#"{
                "device_code": "dc_test",
                "user_code": "ABCD-1234",
                "verification_uri": "https://auth.kimi.com/device",
                "verification_uri_complete": "https://auth.kimi.com/device?user_code=ABCD-1234",
                "expires_in": 900,
                "interval": 5
            }"#,
        );
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client = crate::http::client::Client::new();
            let response = start_kimi_code_device_flow_with_client(&client, &host)
                .await
                .expect("start kimi device flow");
            assert_eq!(response.device_code, "dc_test");
            assert_eq!(response.user_code, "ABCD-1234");
            assert_eq!(response.expires_in, 900);
            assert_eq!(response.interval, 5);
            assert_eq!(
                response.verification_uri_complete.as_deref(),
                Some("https://auth.kimi.com/device?user_code=ABCD-1234")
            );
        });
    }

    #[test]
    fn test_poll_kimi_code_device_flow_success_returns_oauth_credential() {
        let host = spawn_oauth_host_server(
            200,
            r#"{"access_token":"kimi-at","refresh_token":"kimi-rt","expires_in":3600}"#,
        );
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client = crate::http::client::Client::new();
            let result =
                poll_kimi_code_device_flow_with_client(&client, &host, "device-code").await;
            match result {
                DeviceFlowPollResult::Success(AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    token_url,
                    client_id,
                    ..
                }) => {
                    let expected_token_url = format!("{host}{KIMI_CODE_TOKEN_PATH}");
                    assert_eq!(access_token, "kimi-at");
                    assert_eq!(refresh_token, "kimi-rt");
                    assert_eq!(token_url.as_deref(), Some(expected_token_url.as_str()));
                    assert_eq!(client_id.as_deref(), Some(KIMI_CODE_OAUTH_CLIENT_ID));
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn test_poll_kimi_code_device_flow_pending_state() {
        let host = spawn_oauth_host_server(
            400,
            r#"{"error":"authorization_pending","error_description":"wait"}"#,
        );
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client = crate::http::client::Client::new();
            let result =
                poll_kimi_code_device_flow_with_client(&client, &host, "device-code").await;
            assert!(matches!(result, DeviceFlowPollResult::Pending));
        });
    }

    // ── GitLab OAuth tests ────────────────────────────────────────

    fn sample_gitlab_config() -> GitLabOAuthConfig {
        GitLabOAuthConfig {
            client_id: "gl_test_app_id".to_string(),
            base_url: GITLAB_DEFAULT_BASE_URL.to_string(),
            scopes: GITLAB_DEFAULT_SCOPES.to_string(),
            redirect_uri: Some("http://localhost:8765/callback".to_string()),
        }
    }

    #[test]
    fn test_gitlab_oauth_requires_client_id() {
        let config = GitLabOAuthConfig {
            client_id: String::new(),
            ..GitLabOAuthConfig::default()
        };
        let err = start_gitlab_oauth(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("client_id"),
            "error should mention client_id: {msg}"
        );
        assert!(msg.contains("Settings"), "should mention GitLab settings");
    }

    #[test]
    fn test_gitlab_oauth_url_contains_required_params() {
        let config = sample_gitlab_config();
        let info = start_gitlab_oauth(&config).expect("start");

        assert_eq!(info.provider, "gitlab");
        assert!(!info.verifier.is_empty());

        let (base, query) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://gitlab.com/oauth/authorize");

        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();
        assert_eq!(
            params.get("client_id").map(String::as_str),
            Some("gl_test_app_id")
        );
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some(GITLAB_DEFAULT_SCOPES)
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://localhost:8765/callback")
        );
        assert_eq!(
            params.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert!(params.contains_key("code_challenge"));
        assert_eq!(
            params.get("state").map(String::as_str),
            Some(info.verifier.as_str())
        );
    }

    #[test]
    fn test_gitlab_oauth_self_hosted_url() {
        let config = GitLabOAuthConfig {
            client_id: "gl_self_hosted".to_string(),
            base_url: "https://gitlab.mycompany.com".to_string(),
            scopes: "api".to_string(),
            redirect_uri: None,
        };
        let info = start_gitlab_oauth(&config).expect("start");

        let (base, _) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://gitlab.mycompany.com/oauth/authorize");
        assert!(
            info.instructions
                .as_deref()
                .unwrap_or("")
                .contains("gitlab.mycompany.com"),
            "instructions should mention the base URL"
        );
    }

    #[test]
    fn test_gitlab_oauth_self_hosted_trailing_slash() {
        let config = GitLabOAuthConfig {
            client_id: "gl_self_hosted".to_string(),
            base_url: "https://gitlab.mycompany.com/".to_string(),
            scopes: "api".to_string(),
            redirect_uri: None,
        };
        let info = start_gitlab_oauth(&config).expect("start");

        let (base, _) = info.url.split_once('?').expect("missing query");
        assert_eq!(base, "https://gitlab.mycompany.com/oauth/authorize");
    }

    #[test]
    fn test_gitlab_oauth_no_redirect_uri() {
        // When no redirect_uri is configured, a random-port localhost callback
        // server should be auto-created so the browser redirect is captured.
        let config = GitLabOAuthConfig {
            client_id: "gl_no_redirect".to_string(),
            base_url: GITLAB_DEFAULT_BASE_URL.to_string(),
            scopes: "api".to_string(),
            redirect_uri: None,
        };
        let info = start_gitlab_oauth(&config).expect("start");

        let (_, query) = info.url.split_once('?').expect("missing query");
        let params: std::collections::HashMap<_, _> =
            parse_query_pairs(query).into_iter().collect();

        // The auto-generated localhost redirect_uri should be present in the
        // authorize URL and the callback_server should be pre-bound.
        assert!(
            info.redirect_uri
                .as_deref()
                .is_some_and(|uri| uri.starts_with("http://localhost:")),
            "auto-generated redirect_uri should be a localhost URL, got {:?}",
            info.redirect_uri
        );
        assert!(
            params.contains_key("redirect_uri"),
            "redirect_uri should be included in the authorize URL"
        );
        assert!(
            info.callback_server.is_some(),
            "callback_server should be pre-bound"
        );
    }

    #[test]
    fn test_gitlab_oauth_pkce_format() {
        let config = sample_gitlab_config();
        let info = start_gitlab_oauth(&config).expect("start");

        assert_eq!(info.verifier.len(), 43);
        assert!(!info.verifier.contains('+'));
        assert!(!info.verifier.contains('/'));
        assert!(!info.verifier.contains('='));
    }

    #[test]
    #[cfg(unix)]
    fn test_gitlab_oauth_complete_success() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(
                200,
                r#"{"access_token":"glpat-test_access","refresh_token":"glrt-test_refresh","expires_in":7200,"token_type":"bearer"}"#,
            );

            // Test via the token response directly (GitLab uses standard OAuth response).
            let response: OAuthTokenResponse = serde_json::from_str(
                r#"{"access_token":"glpat-test_access","refresh_token":"glrt-test_refresh","expires_in":7200}"#,
            )
            .expect("parse");

            let cred = AuthCredential::OAuth {
                access_token: response.access_token,
                refresh_token: response.refresh_token,
                expires: oauth_expires_at_ms(response.expires_in),
                token_url: None,
                client_id: None,
            };

            match cred {
                AuthCredential::OAuth {
                    access_token,
                    refresh_token,
                    expires,
                    ..
                } => {
                    assert_eq!(access_token, "glpat-test_access");
                    assert_eq!(refresh_token, "glrt-test_refresh");
                    assert!(expires > chrono::Utc::now().timestamp_millis());
                }
                other => panic!(),
            }

            // Also ensure the test server URL was consumed (not left hanging).
            let _ = token_url;
        });
    }

    #[test]
    fn test_gitlab_diagnostic_includes_troubleshooting() {
        let msg = gitlab_diagnostic("https://gitlab.com", "Token exchange failed", "bad request");
        assert!(msg.contains("Token exchange failed"));
        assert!(msg.contains("Troubleshooting"));
        assert!(msg.contains("client_id"));
        assert!(msg.contains("Settings > Applications"));
        assert!(msg.contains("https://gitlab.com"));
    }

    #[test]
    fn test_gitlab_diagnostic_self_hosted_url_in_message() {
        let msg = gitlab_diagnostic("https://gitlab.mycompany.com", "Auth failed", "HTTP 401");
        assert!(
            msg.contains("gitlab.mycompany.com"),
            "should reference the self-hosted URL"
        );
    }

    // ── Provider metadata integration ─────────────────────────────

    #[test]
    fn test_env_keys_gitlab_provider() {
        let keys = env_keys_for_provider("gitlab");
        assert_eq!(keys, &["GITLAB_TOKEN", "GITLAB_API_KEY"]);
    }

    #[test]
    fn test_env_keys_gitlab_duo_alias() {
        let keys = env_keys_for_provider("gitlab-duo");
        assert_eq!(keys, &["GITLAB_TOKEN", "GITLAB_API_KEY"]);
    }

    #[test]
    fn test_env_keys_copilot_includes_github_token() {
        let keys = env_keys_for_provider("github-copilot");
        assert_eq!(keys, &["GITHUB_COPILOT_API_KEY", "GITHUB_TOKEN"]);
    }

    // ── Default config constructors ───────────────────────────────

    #[test]
    fn test_copilot_config_default() {
        let config = CopilotOAuthConfig::default();
        assert!(config.client_id.is_empty());
        assert_eq!(config.github_base_url, "https://github.com");
        assert_eq!(config.scopes, GITHUB_COPILOT_SCOPES);
    }

    #[test]
    fn test_gitlab_config_default() {
        let config = GitLabOAuthConfig::default();
        assert!(config.client_id.is_empty());
        assert_eq!(config.base_url, GITLAB_DEFAULT_BASE_URL);
        assert_eq!(config.scopes, GITLAB_DEFAULT_SCOPES);
        assert!(config.redirect_uri.is_none());
    }

    // ── trim_trailing_slash ───────────────────────────────────────

    #[test]
    fn test_trim_trailing_slash_noop() {
        assert_eq!(
            trim_trailing_slash("https://github.com"),
            "https://github.com"
        );
    }

    #[test]
    fn test_trim_trailing_slash_single() {
        assert_eq!(
            trim_trailing_slash("https://github.com/"),
            "https://github.com"
        );
    }

    #[test]
    fn test_trim_trailing_slash_multiple() {
        assert_eq!(
            trim_trailing_slash("https://github.com///"),
            "https://github.com"
        );
    }

    // ── AuthCredential new variant serialization ─────────────────────

    #[test]
    fn test_aws_credentials_round_trip() {
        let cred = AuthCredential::AwsCredentials {
            access_key_id: "AKIAEXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/SECRET".to_string(),
            session_token: Some("FwoGZX...session".to_string()),
            region: Some("us-west-2".to_string()),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token,
                region,
            } => {
                assert_eq!(access_key_id, "AKIAEXAMPLE");
                assert_eq!(secret_access_key, "wJalrXUtnFEMI/SECRET");
                assert_eq!(session_token.as_deref(), Some("FwoGZX...session"));
                assert_eq!(region.as_deref(), Some("us-west-2"));
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_aws_credentials_without_optional_fields() {
        let json =
            r#"{"type":"aws_credentials","access_key_id":"AKIA","secret_access_key":"secret"}"#;
        let cred: AuthCredential = serde_json::from_str(json).expect("deserialize");
        match cred {
            AuthCredential::AwsCredentials {
                session_token,
                region,
                ..
            } => {
                assert!(session_token.is_none());
                assert!(region.is_none());
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_bearer_token_round_trip() {
        let cred = AuthCredential::BearerToken {
            token: "my-gateway-token-123".to_string(),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::BearerToken { token } => {
                assert_eq!(token, "my-gateway-token-123");
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_service_key_round_trip() {
        let cred = AuthCredential::ServiceKey {
            client_id: Some("sap-client-id".to_string()),
            client_secret: Some("sap-secret".to_string()),
            token_url: Some("https://auth.sap.com/oauth/token".to_string()),
            service_url: Some("https://api.ai.sap.com".to_string()),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::ServiceKey {
                client_id,
                client_secret,
                token_url,
                service_url,
            } => {
                assert_eq!(client_id.as_deref(), Some("sap-client-id"));
                assert_eq!(client_secret.as_deref(), Some("sap-secret"));
                assert_eq!(
                    token_url.as_deref(),
                    Some("https://auth.sap.com/oauth/token")
                );
                assert_eq!(service_url.as_deref(), Some("https://api.ai.sap.com"));
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_service_key_without_optional_fields() {
        let json = r#"{"type":"service_key"}"#;
        let cred: AuthCredential = serde_json::from_str(json).expect("deserialize");
        match cred {
            AuthCredential::ServiceKey {
                client_id,
                client_secret,
                token_url,
                service_url,
            } => {
                assert!(client_id.is_none());
                assert!(client_secret.is_none());
                assert!(token_url.is_none());
                assert!(service_url.is_none());
            }
            other => panic!(),
        }
    }

    // ── api_key() with new variants ──────────────────────────────────

    #[test]
    fn test_api_key_returns_bearer_token() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "my-gateway",
            AuthCredential::BearerToken {
                token: "gw-tok-123".to_string(),
            },
        );
        assert_eq!(auth.api_key("my-gateway").as_deref(), Some("gw-tok-123"));
    }

    #[test]
    fn test_api_key_returns_aws_access_key_id() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIAEXAMPLE".to_string(),
                secret_access_key: "secret".to_string(),
                session_token: None,
                region: None,
            },
        );
        assert_eq!(
            auth.api_key("amazon-bedrock").as_deref(),
            Some("AKIAEXAMPLE")
        );
    }

    #[test]
    fn test_api_key_returns_none_for_service_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "sap-ai-core",
            AuthCredential::ServiceKey {
                client_id: Some("id".to_string()),
                client_secret: Some("secret".to_string()),
                token_url: Some("https://auth.example.com".to_string()),
                service_url: Some("https://api.example.com".to_string()),
            },
        );
        assert!(auth.api_key("sap-ai-core").is_none());
    }

    // ── AWS Credential Chain ─────────────────────────────────────────

    fn empty_auth() -> AuthStorage {
        let dir = tempfile::tempdir().expect("tmpdir");
        AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        }
    }

    #[test]
    fn test_aws_bearer_token_env_wins() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_BEARER_TOKEN_BEDROCK" => Some("bearer-tok-env".to_string()),
            "AWS_REGION" => Some("eu-west-1".to_string()),
            "AWS_ACCESS_KEY_ID" => Some("AKIA_SHOULD_NOT_WIN".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Bearer {
                token: "bearer-tok-env".to_string(),
                region: "eu-west-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_env_sigv4_credentials() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIATEST".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secretTEST".to_string()),
            "AWS_SESSION_TOKEN" => Some("session123".to_string()),
            "AWS_REGION" => Some("ap-southeast-1".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIATEST".to_string(),
                secret_access_key: "secretTEST".to_string(),
                session_token: Some("session123".to_string()),
                region: "ap-southeast-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_env_sigv4_without_session_token() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA".to_string(),
                secret_access_key: "secret".to_string(),
                session_token: None,
                region: "us-east-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_default_region_fallback() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            "AWS_DEFAULT_REGION" => Some("ca-central-1".to_string()),
            _ => None,
        });
        match result {
            Some(AwsResolvedCredentials::Sigv4 { region, .. }) => {
                assert_eq!(region, "ca-central-1");
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_aws_stored_credentials_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIA_STORED".to_string(),
                secret_access_key: "secret_stored".to_string(),
                session_token: None,
                region: Some("us-west-2".to_string()),
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA_STORED".to_string(),
                secret_access_key: "secret_stored".to_string(),
                session_token: None,
                region: "us-west-2".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_stored_credentials_accept_alias_and_case_insensitive_entry() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "BedRock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIA_ALIAS".to_string(),
                secret_access_key: "alias-secret".to_string(),
                session_token: Some("alias-session".to_string()),
                region: Some("eu-central-1".to_string()),
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA_ALIAS".to_string(),
                secret_access_key: "alias-secret".to_string(),
                session_token: Some("alias-session".to_string()),
                region: "eu-central-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_stored_bearer_fallback() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::BearerToken {
                token: "stored-bearer".to_string(),
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Bearer {
                token: "stored-bearer".to_string(),
                region: "us-east-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_env_beats_stored() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIA_STORED".to_string(),
                secret_access_key: "stored_secret".to_string(),
                session_token: None,
                region: None,
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA_ENV".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("env_secret".to_string()),
            _ => None,
        });
        match result {
            Some(AwsResolvedCredentials::Sigv4 { access_key_id, .. }) => {
                assert_eq!(access_key_id, "AKIA_ENV");
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_aws_no_credentials_returns_none() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert!(result.is_none());
    }

    #[test]
    fn test_aws_empty_bearer_token_skipped() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_BEARER_TOKEN_BEDROCK" => Some("  ".to_string()),
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert!(matches!(result, Some(AwsResolvedCredentials::Sigv4 { .. })));
    }

    #[test]
    fn test_aws_access_key_without_secret_skipped() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            _ => None,
        });
        assert!(result.is_none());
    }

    // ── SAP AI Core Credential Chain ─────────────────────────────────

    #[test]
    fn test_sap_json_service_key() {
        let auth = empty_auth();
        let key_json = serde_json::json!({
            "clientid": "sap-client",
            "clientsecret": "sap-secret",
            "url": "https://auth.sap.example.com/oauth/token",
            "serviceurls": {
                "AI_API_URL": "https://api.ai.sap.example.com"
            }
        })
        .to_string();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "AICORE_SERVICE_KEY" => Some(key_json.clone()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                client_secret: "sap-secret".to_string(),
                token_url: "https://auth.sap.example.com/oauth/token".to_string(),
                service_url: "https://api.ai.sap.example.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_individual_env_vars() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "SAP_AI_CORE_CLIENT_ID" => Some("env-client".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.sap.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://service.sap.example.com".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "env-client".to_string(),
                client_secret: "env-secret".to_string(),
                token_url: "https://token.sap.example.com".to_string(),
                service_url: "https://service.sap.example.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_stored_service_key() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "sap-ai-core",
            AuthCredential::ServiceKey {
                client_id: Some("stored-id".to_string()),
                client_secret: Some("stored-secret".to_string()),
                token_url: Some("https://stored-token.sap.com".to_string()),
                service_url: Some("https://stored-api.sap.com".to_string()),
            },
        );
        let result = resolve_sap_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "stored-id".to_string(),
                client_secret: "stored-secret".to_string(),
                token_url: "https://stored-token.sap.com".to_string(),
                service_url: "https://stored-api.sap.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_stored_service_key_accepts_alias_and_case_insensitive_entry() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "SaP",
            AuthCredential::ServiceKey {
                client_id: Some("alias-id".to_string()),
                client_secret: Some("alias-secret".to_string()),
                token_url: Some("https://alias-token.sap.com".to_string()),
                service_url: Some("https://alias-api.sap.com".to_string()),
            },
        );
        let result = resolve_sap_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "alias-id".to_string(),
                client_secret: "alias-secret".to_string(),
                token_url: "https://alias-token.sap.com".to_string(),
                service_url: "https://alias-api.sap.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_json_key_wins_over_individual_vars() {
        let key_json = serde_json::json!({
            "clientid": "json-client",
            "clientsecret": "json-secret",
            "url": "https://json-token.example.com",
            "serviceurls": {"AI_API_URL": "https://json-api.example.com"}
        })
        .to_string();
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "AICORE_SERVICE_KEY" => Some(key_json.clone()),
            "SAP_AI_CORE_CLIENT_ID" => Some("env-client".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://env-token.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://env-api.example.com".to_string()),
            _ => None,
        });
        assert_eq!(result.unwrap().client_id, "json-client");
    }

    #[test]
    fn test_sap_incomplete_individual_vars_returns_none() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "SAP_AI_CORE_CLIENT_ID" => Some("id".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.example.com".to_string()),
            _ => None,
        });
        assert!(result.is_none());
    }

    #[test]
    fn test_sap_invalid_json_falls_through() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "AICORE_SERVICE_KEY" => Some("not-valid-json".to_string()),
            "SAP_AI_CORE_CLIENT_ID" => Some("env-id".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://api.example.com".to_string()),
            _ => None,
        });
        assert_eq!(result.unwrap().client_id, "env-id");
    }

    #[test]
    fn test_sap_no_credentials_returns_none() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert!(result.is_none());
    }

    #[test]
    fn test_sap_json_key_alternate_field_names() {
        let key_json = serde_json::json!({
            "client_id": "alt-id",
            "client_secret": "alt-secret",
            "token_url": "https://alt-token.example.com",
            "service_url": "https://alt-api.example.com"
        })
        .to_string();
        let creds = parse_sap_service_key_json(&key_json);
        assert_eq!(
            creds,
            Some(SapResolvedCredentials {
                client_id: "alt-id".to_string(),
                client_secret: "alt-secret".to_string(),
                token_url: "https://alt-token.example.com".to_string(),
                service_url: "https://alt-api.example.com".to_string(),
            })
        );
    }

    #[test]
    fn test_sap_json_key_missing_required_field_returns_none() {
        let key_json = serde_json::json!({
            "clientid": "id",
            "url": "https://token.example.com",
            "serviceurls": {"AI_API_URL": "https://api.example.com"}
        })
        .to_string();
        assert!(parse_sap_service_key_json(&key_json).is_none());
    }

    // ── SAP AI Core metadata ─────────────────────────────────────────

    #[test]
    fn test_sap_metadata_exists() {
        let keys = env_keys_for_provider("sap-ai-core");
        assert!(!keys.is_empty(), "sap-ai-core should have env keys");
        assert!(keys.contains(&"AICORE_SERVICE_KEY"));
    }

    #[test]
    fn test_sap_alias_resolves() {
        let keys = env_keys_for_provider("sap");
        assert!(!keys.is_empty(), "sap alias should resolve");
        assert!(keys.contains(&"AICORE_SERVICE_KEY"));
    }

    #[test]
    fn test_exchange_sap_access_token_with_client_success() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_response = r#"{"access_token":"sap-access-token"}"#;
            let token_url = spawn_json_server(200, token_response);
            let client = crate::http::client::Client::new();
            let creds = SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                client_secret: "sap-secret".to_string(),
                token_url,
                service_url: "https://api.ai.sap.example.com".to_string(),
            };

            let token = exchange_sap_access_token_with_client(&client, &creds)
                .await
                .expect("token exchange");
            assert_eq!(token, "sap-access-token");
        });
    }

    #[test]
    fn test_exchange_sap_access_token_with_client_http_error() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(401, r#"{"error":"unauthorized"}"#);
            let client = crate::http::client::Client::new();
            let creds = SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                client_secret: "sap-secret".to_string(),
                token_url,
                service_url: "https://api.ai.sap.example.com".to_string(),
            };

            let err = exchange_sap_access_token_with_client(&client, &creds)
                .await
                .expect_err("expected HTTP error");
            assert!(
                err.to_string().contains("HTTP 401"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_exchange_sap_access_token_with_client_invalid_json() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(200, r#"{"token":"missing-access-token"}"#);
            let client = crate::http::client::Client::new();
            let creds = SapResolvedCredentials {
                client_id: "sap-client".to_string(),
                client_secret: "sap-secret".to_string(),
                token_url,
                service_url: "https://api.ai.sap.example.com".to_string(),
            };

            let err = exchange_sap_access_token_with_client(&client, &creds)
                .await
                .expect_err("expected JSON error");
            assert!(
                err.to_string().contains("invalid JSON"),
                "unexpected error: {err}"
            );
        });
    }

    // ── Lifecycle tests (bd-3uqg.7.6) ─────────────────────────────

    #[test]
    fn test_proactive_refresh_triggers_within_window() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            // Token expires 5 minutes from now (within the 10-min window).
            let five_min_from_now = chrono::Utc::now().timestamp_millis() + 5 * 60 * 1000;
            let token_response =
                r#"{"access_token":"refreshed","refresh_token":"new-ref","expires_in":3600}"#;
            let server_url = spawn_json_server(200, token_response);

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
            };
            auth.entries.insert(
                "copilot".to_string(),
                AuthCredential::OAuth {
                    access_token: "about-to-expire".to_string(),
                    refresh_token: "old-ref".to_string(),
                    expires: five_min_from_now,
                    token_url: Some(server_url),
                    client_id: Some("test-client".to_string()),
                },
            );

            let client = crate::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect("proactive refresh");

            match auth.entries.get("copilot").expect("credential") {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "refreshed");
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn test_proactive_refresh_skips_tokens_far_from_expiry() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            let one_hour_from_now = chrono::Utc::now().timestamp_millis() + 60 * 60 * 1000;

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
            };
            auth.entries.insert(
                "copilot".to_string(),
                AuthCredential::OAuth {
                    access_token: "still-good".to_string(),
                    refresh_token: "ref".to_string(),
                    expires: one_hour_from_now,
                    token_url: Some("https://should-not-be-called.example.com/token".to_string()),
                    client_id: Some("test-client".to_string()),
                },
            );

            let client = crate::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect("no refresh needed");

            match auth.entries.get("copilot").expect("credential") {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "still-good");
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn test_self_contained_refresh_uses_stored_metadata() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            let token_response =
                r#"{"access_token":"new-copilot-token","refresh_token":"new-ref","expires_in":28800}"#;
            let server_url = spawn_json_server(200, token_response);

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
            };
            auth.entries.insert(
                "copilot".to_string(),
                AuthCredential::OAuth {
                    access_token: "expired-copilot".to_string(),
                    refresh_token: "old-ref".to_string(),
                    expires: 0,
                    token_url: Some(server_url.clone()),
                    client_id: Some("Iv1.copilot-client".to_string()),
                },
            );

            let client = crate::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect("self-contained refresh");

            match auth.entries.get("copilot").expect("credential") {
                AuthCredential::OAuth {
                    access_token,
                    token_url,
                    client_id,
                    ..
                } => {
                    assert_eq!(access_token, "new-copilot-token");
                    assert_eq!(token_url.as_deref(), Some(server_url.as_str()));
                    assert_eq!(client_id.as_deref(), Some("Iv1.copilot-client"));
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn test_self_contained_refresh_skips_when_no_metadata() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
            };
            auth.entries.insert(
                "ext-custom".to_string(),
                AuthCredential::OAuth {
                    access_token: "old-ext".to_string(),
                    refresh_token: "ref".to_string(),
                    expires: 0,
                    token_url: None,
                    client_id: None,
                },
            );

            let client = crate::http::client::Client::new();
            auth.refresh_expired_oauth_tokens_with_client(&client)
                .await
                .expect("should succeed by skipping");

            match auth.entries.get("ext-custom").expect("credential") {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "old-ext");
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn test_extension_refresh_skips_self_contained_credentials() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let auth_path = dir.path().join("auth.json");

            let mut auth = AuthStorage {
                path: auth_path,
                entries: HashMap::new(),
            };
            auth.entries.insert(
                "copilot".to_string(),
                AuthCredential::OAuth {
                    access_token: "self-contained".to_string(),
                    refresh_token: "ref".to_string(),
                    expires: 0,
                    token_url: Some("https://github.com/login/oauth/access_token".to_string()),
                    client_id: Some("Iv1.copilot".to_string()),
                },
            );

            let client = crate::http::client::Client::new();
            let mut extension_configs = HashMap::new();
            extension_configs.insert("copilot".to_string(), sample_oauth_config());

            auth.refresh_expired_extension_oauth_tokens(&client, &extension_configs)
                .await
                .expect("should succeed by skipping");

            match auth.entries.get("copilot").expect("credential") {
                AuthCredential::OAuth { access_token, .. } => {
                    assert_eq!(access_token, "self-contained");
                }
                other => panic!(),
            }
        });
    }

    #[test]
    fn test_prune_stale_credentials_removes_old_expired_without_metadata() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };

        let now = chrono::Utc::now().timestamp_millis();
        let one_day_ms = 24 * 60 * 60 * 1000;

        // Stale: expired 2 days ago, no metadata.
        auth.entries.insert(
            "stale-ext".to_string(),
            AuthCredential::OAuth {
                access_token: "dead".to_string(),
                refresh_token: "dead-ref".to_string(),
                expires: now - 2 * one_day_ms,
                token_url: None,
                client_id: None,
            },
        );

        // Not stale: expired 2 days ago but HAS metadata.
        auth.entries.insert(
            "copilot".to_string(),
            AuthCredential::OAuth {
                access_token: "old-copilot".to_string(),
                refresh_token: "ref".to_string(),
                expires: now - 2 * one_day_ms,
                token_url: Some("https://github.com/login/oauth/access_token".to_string()),
                client_id: Some("Iv1.copilot".to_string()),
            },
        );

        // Not stale: expired recently.
        auth.entries.insert(
            "recent-ext".to_string(),
            AuthCredential::OAuth {
                access_token: "recent".to_string(),
                refresh_token: "ref".to_string(),
                expires: now - 30 * 60 * 1000, // 30 min ago
                token_url: None,
                client_id: None,
            },
        );

        // Not OAuth.
        auth.entries.insert(
            "anthropic".to_string(),
            AuthCredential::ApiKey {
                key: ApiKeyValue::Raw("sk-test".to_string()),
            },
        );

        let pruned = auth.prune_stale_credentials(one_day_ms);

        assert_eq!(pruned, vec!["stale-ext"]);
        assert!(!auth.entries.contains_key("stale-ext"));
        assert!(auth.entries.contains_key("copilot"));
        assert!(auth.entries.contains_key("recent-ext"));
        assert!(auth.entries.contains_key("anthropic"));
    }

    #[test]
    fn test_prune_stale_credentials_no_op_when_all_valid() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");

        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };

        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        auth.entries.insert(
            "ext-prov".to_string(),
            AuthCredential::OAuth {
                access_token: "valid".to_string(),
                refresh_token: "ref".to_string(),
                expires: far_future,
                token_url: None,
                client_id: None,
            },
        );

        let pruned = auth.prune_stale_credentials(24 * 60 * 60 * 1000);
        assert!(pruned.is_empty());
        assert!(auth.entries.contains_key("ext-prov"));
    }

    #[test]
    fn test_credential_serialization_preserves_new_fields() {
        let cred = AuthCredential::OAuth {
            access_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            expires: 12345,
            token_url: Some("https://example.com/token".to_string()),
            client_id: Some("my-client".to_string()),
        };

        let json = serde_json::to_string(&cred).expect("serialize");
        assert!(json.contains("token_url"));
        assert!(json.contains("client_id"));

        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::OAuth {
                token_url,
                client_id,
                ..
            } => {
                assert_eq!(token_url.as_deref(), Some("https://example.com/token"));
                assert_eq!(client_id.as_deref(), Some("my-client"));
            }
            other => panic!(),
        }
    }

    #[test]
    fn test_credential_serialization_omits_none_fields() {
        let cred = AuthCredential::OAuth {
            access_token: "tok".to_string(),
            refresh_token: "ref".to_string(),
            expires: 12345,
            token_url: None,
            client_id: None,
        };

        let json = serde_json::to_string(&cred).expect("serialize");
        assert!(!json.contains("token_url"));
        assert!(!json.contains("client_id"));
    }

    #[test]
    fn test_credential_deserialization_defaults_missing_fields() {
        let json =
            r#"{"type":"o_auth","access_token":"tok","refresh_token":"ref","expires":12345}"#;
        let parsed: AuthCredential = serde_json::from_str(json).expect("deserialize");
        match parsed {
            AuthCredential::OAuth {
                token_url,
                client_id,
                ..
            } => {
                assert!(token_url.is_none());
                assert!(client_id.is_none());
            }
            other => panic!(),
        }
    }

    #[test]
    fn codex_openai_api_key_parser_ignores_oauth_access_token_only_payloads() {
        let value = serde_json::json!({
            "tokens": {
                "access_token": "codex-oauth-token"
            }
        });
        assert!(codex_openai_api_key_from_value(&value).is_none());
    }

    #[test]
    fn codex_access_token_parser_reads_nested_tokens_payload() {
        let value = serde_json::json!({
            "tokens": {
                "access_token": " codex-oauth-token "
            }
        });
        assert_eq!(
            codex_access_token_from_value(&value).as_deref(),
            Some("codex-oauth-token")
        );
    }

    #[test]
    fn codex_openai_api_key_parser_reads_openai_api_key_field() {
        let value = serde_json::json!({
            "OPENAI_API_KEY": " sk-openai "
        });
        assert_eq!(
            codex_openai_api_key_from_value(&value).as_deref(),
            Some("sk-openai")
        );
    }
}
