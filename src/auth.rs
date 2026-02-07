//! Authentication storage and API key resolution.
//!
//! Auth file: ~/.pi/agent/auth.json

use crate::error::{Error, Result};
use base64::Engine as _;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_OAUTH_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_OAUTH_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const ANTHROPIC_OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference";

/// Credentials stored in auth.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthCredential {
    ApiKey {
        key: String,
    },
    OAuth {
        access_token: String,
        refresh_token: String,
        expires: i64, // Unix ms
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthFile {
    #[serde(flatten)]
    pub entries: HashMap<String, AuthCredential>,
}

/// Auth storage wrapper with file locking.
#[derive(Debug, Clone)]
pub struct AuthStorage {
    path: PathBuf,
    entries: HashMap<String, AuthCredential>,
}

impl AuthStorage {
    /// Load auth.json (creates empty if missing).
    pub fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            let file = File::open(&path).map_err(|e| Error::auth(format!("auth.json: {e}")))?;
            let mut locked = lock_file(file, Duration::from_secs(30))?;
            // Read from the locked file handle, not a new handle
            let mut content = String::new();
            locked.as_file_mut().read_to_string(&mut content)?;
            let parsed: AuthFile = match serde_json::from_str(&content) {
                Ok(file) => file,
                Err(e) => {
                    tracing::warn!(
                        event = "pi.auth.parse_error",
                        error = %e,
                        "auth.json is corrupted; starting with empty credentials"
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
        let entries = if path.exists() {
            let file = File::open(&path).map_err(|e| Error::auth(format!("auth.json: {e}")))?;
            let mut locked = lock_file_async(file, Duration::from_secs(30)).await?;
            let mut content = String::new();
            locked.as_file_mut().read_to_string(&mut content)?;
            let parsed: AuthFile = match serde_json::from_str(&content) {
                Ok(file) => file,
                Err(e) => {
                    tracing::warn!(
                        event = "pi.auth.parse_error",
                        error = %e,
                        "auth.json is corrupted; starting with empty credentials"
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

    /// Persist auth.json (atomic write + permissions).
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;
        let mut locked = lock_file(file, Duration::from_secs(30))?;

        let data = serde_json::to_string_pretty(&AuthFile {
            entries: self.entries.clone(),
        })?;

        // Write to the locked file handle, not a new handle
        let f = locked.as_file_mut();
        f.seek(SeekFrom::Start(0))?;
        f.set_len(0)?; // Truncate after seeking to avoid data loss
        f.write_all(data.as_bytes())?;
        f.flush()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&self.path, perms)?;
        }

        Ok(())
    }

    /// Persist auth.json asynchronously.
    pub async fn save_async(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)?;
        let mut locked = lock_file_async(file, Duration::from_secs(30)).await?;

        let data = serde_json::to_string_pretty(&AuthFile {
            entries: self.entries.clone(),
        })?;

        // Write to the locked file handle
        let f = locked.as_file_mut();
        f.seek(SeekFrom::Start(0))?;
        f.set_len(0)?;
        f.write_all(data.as_bytes())?;
        f.flush()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&self.path, perms)?;
        }

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
    pub fn api_key(&self, provider: &str) -> Option<String> {
        match self.entries.get(provider) {
            Some(AuthCredential::ApiKey { key }) => Some(key.clone()),
            Some(AuthCredential::OAuth {
                access_token,
                expires,
                ..
            }) => {
                let now = chrono::Utc::now().timestamp_millis();
                if *expires > now {
                    Some(access_token.clone())
                } else {
                    None
                }
            }
            None => None,
        }
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

        if let Some(key) = env_keys_for_provider(provider)
            .iter()
            .find_map(|var| env_lookup(var).filter(|value| !value.is_empty()))
        {
            return Some(key);
        }

        self.api_key(provider)
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
    pub async fn refresh_expired_oauth_tokens_with_client(
        &mut self,
        client: &crate::http::client::Client,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut refreshes = Vec::new();

        for (provider, cred) in &self.entries {
            if let AuthCredential::OAuth {
                refresh_token,
                expires,
                ..
            } = cred
            {
                if *expires <= now {
                    refreshes.push((provider.clone(), refresh_token.clone()));
                }
            }
        }

        for (provider, refresh_token) in refreshes {
            let refreshed = match provider.as_str() {
                "anthropic" => {
                    Box::pin(refresh_anthropic_oauth_token(client, &refresh_token)).await?
                }
                _ => continue,
            };
            self.entries.insert(provider, refreshed);
            self.save_async().await?;
        }

        Ok(())
    }

    /// Refresh expired OAuth tokens for extension-registered providers.
    ///
    /// `extension_configs` maps provider ID to its [`OAuthConfig`](crate::models::OAuthConfig).
    /// Providers already handled by [`refresh_expired_oauth_tokens_with_client`] (e.g. "anthropic")
    /// are skipped.
    pub async fn refresh_expired_extension_oauth_tokens(
        &mut self,
        client: &crate::http::client::Client,
        extension_configs: &HashMap<String, crate::models::OAuthConfig>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut refreshes = Vec::new();

        for (provider, cred) in &self.entries {
            if let AuthCredential::OAuth {
                refresh_token,
                expires,
                ..
            } = cred
            {
                // Skip built-in providers (handled by refresh_expired_oauth_tokens_with_client).
                if provider == "anthropic" {
                    continue;
                }
                if *expires <= now {
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
                    self.save_async().await?;
                }
                Err(e) => {
                    tracing::warn!(
                        event = "pi.auth.extension_oauth_refresh.error",
                        provider = %provider,
                        error = %e,
                        elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                        "Failed to refresh extension OAuth token; continuing with remaining providers"
                    );
                }
            }
        }
        Ok(())
    }
}

fn env_key_for_provider(provider: &str) -> Option<&'static str> {
    env_keys_for_provider(provider).first().copied()
}

fn env_keys_for_provider(provider: &str) -> &'static [&'static str] {
    match provider {
        "anthropic" => &["ANTHROPIC_API_KEY"],
        "openai" => &["OPENAI_API_KEY"],
        "google" => &["GOOGLE_API_KEY"],
        "google-vertex" => &["GOOGLE_CLOUD_API_KEY"],
        "amazon-bedrock" => &["AWS_ACCESS_KEY_ID"],
        "azure-openai" => &["AZURE_OPENAI_API_KEY"],
        "github-copilot" => &["GITHUB_COPILOT_API_KEY"],
        "xai" => &["XAI_API_KEY"],
        "groq" => &["GROQ_API_KEY"],
        "deepinfra" => &["DEEPINFRA_API_KEY"],
        "cerebras" => &["CEREBRAS_API_KEY"],
        "openrouter" => &["OPENROUTER_API_KEY"],
        "mistral" => &["MISTRAL_API_KEY"],
        "cohere" => &["COHERE_API_KEY"],
        "perplexity" => &["PERPLEXITY_API_KEY"],
        "deepseek" => &["DEEPSEEK_API_KEY"],
        "fireworks" => &["FIREWORKS_API_KEY"],
        "togetherai" => &["TOGETHER_API_KEY", "TOGETHER_AI_API_KEY"],
        // MoonshotAI is the API behind "Kimi".
        "moonshotai" | "moonshot" | "kimi" => &["MOONSHOT_API_KEY"],
        // Qwen models are served via Alibaba Cloud DashScope (OpenAI compatible mode).
        "alibaba" | "dashscope" | "qwen" => &["DASHSCOPE_API_KEY"],
        _ => &[],
    }
}

fn redact_known_secrets(text: &str, secrets: &[&str]) -> String {
    let mut redacted = text.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            redacted = redacted.replace(secret, "[REDACTED]");
        }
    }
    redacted
}

#[derive(Debug, Clone)]
pub struct OAuthStartInfo {
    pub provider: String,
    pub url: String,
    pub verifier: String,
    pub instructions: Option<String>,
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
    })
}

/// Complete Anthropic OAuth by exchanging an authorization code for access/refresh tokens.
pub async fn complete_anthropic_oauth(code_input: &str, verifier: &str) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());

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
    })
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
}

fn oauth_expires_at_ms(expires_in_seconds: i64) -> i64 {
    chrono::Utc::now().timestamp_millis() + expires_in_seconds.saturating_mul(1000) - 5 * 60 * 1000
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
    loop {
        if matches!(FileExt::try_lock_exclusive(&file), Ok(true)) {
            return Ok(LockedFile { file });
        }

        if start.elapsed() >= timeout {
            return Err(Error::auth("Timed out waiting for auth lock".to_string()));
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

async fn lock_file_async(file: File, timeout: Duration) -> Result<LockedFile> {
    let start = Instant::now();
    loop {
        let lock_result = FileExt::try_lock_exclusive(&file);
        if matches!(lock_result, Ok(true)) {
            return Ok(LockedFile { file });
        }

        if start.elapsed() >= timeout {
            return Err(Error::auth("Timed out waiting for auth lock".to_string()));
        }

        asupersync::time::sleep(asupersync::time::wall_now(), Duration::from_millis(50)).await;
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

    fn next_token() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string()
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
                    key: "stored-openai-key".to_string(),
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
                key: "stored-openai-key".to_string(),
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
    fn test_resolve_api_key_returns_none_when_unconfigured() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };

        let resolved = auth.resolve_api_key_with_env_lookup("openai", None, |_| None);
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
                        if access_token == &initial_access
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
                        if access_token == &initial_access_token
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
                        if access_token == &initial_access_token
                ),
                "expected OAuth credential"
            );
        });
    }

    #[test]
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
                } => {
                    assert_eq!(access_token, "new-access");
                    assert_eq!(refresh_token, "new-refresh");
                    assert!(*expires > now);
                }
                other @ AuthCredential::ApiKey { .. } => {
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
                other @ AuthCredential::ApiKey { .. } => {
                    unreachable!("expected oauth credential, got: {other:?}");
                }
            }
        });
    }

    #[test]
    fn test_refresh_extension_oauth_token_redacts_secret_in_error() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let refresh_secret = "secret-refresh-token-123";
            let token_url = spawn_json_server(
                401,
                &format!(r#"{{"error":"invalid_grant","echo":"{refresh_secret}"}}"#),
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
        });
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
            } => {
                assert_eq!(access_token, &expected_access_token);
                assert_eq!(refresh_token, &expected_refresh_token);
                assert_eq!(*expires, 9_999_999_999_000);
            }
            other @ AuthCredential::ApiKey { .. } => {
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
            },
        );

        assert_eq!(auth.api_key("ext-provider"), None);
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
                key: "key-123".to_string(),
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
            other @ AuthCredential::OAuth { .. } => panic!("expected ApiKey, got: {other:?}"),
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
                key: "stored-key".to_string(),
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
                key: "stored-key".to_string(),
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
                key: "stored-key".to_string(),
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
                key: "sk-openai-test".to_string(),
            },
        );

        assert_eq!(auth.api_key("openai").as_deref(), Some("sk-openai-test"));
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
                key: "sk-ant".to_string(),
            },
        );
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "sk-oai".to_string(),
            },
        );
        let far_future = chrono::Utc::now().timestamp_millis() + 3_600_000;
        auth.set(
            "google",
            AuthCredential::OAuth {
                access_token: "goog-token".to_string(),
                refresh_token: "goog-refresh".to_string(),
                expires: far_future,
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
                key: "nested-key".to_string(),
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
                key: "secret".to_string(),
            },
        );
        auth.save().expect("save");

        let metadata = fs::metadata(&auth_path).expect("metadata");
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "auth.json should be owner-only read/write");
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
    fn test_env_keys_moonshotai_aliases() {
        for alias in &["moonshotai", "moonshot", "kimi"] {
            let keys = env_keys_for_provider(alias);
            assert_eq!(
                keys,
                &["MOONSHOT_API_KEY"],
                "alias {alias} should map to MOONSHOT_API_KEY"
            );
        }
    }

    #[test]
    fn test_env_keys_alibaba_aliases() {
        for alias in &["alibaba", "dashscope", "qwen"] {
            let keys = env_keys_for_provider(alias);
            assert_eq!(
                keys,
                &["DASHSCOPE_API_KEY"],
                "alias {alias} should map to DASHSCOPE_API_KEY"
            );
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
                key: "first-key".to_string(),
            },
        );
        assert_eq!(auth.api_key("anthropic").as_deref(), Some("first-key"));

        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "second-key".to_string(),
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
                    key: "old-key".to_string(),
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
                    key: "new-key".to_string(),
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
                key: "test-key".to_string(),
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
        let text = "token=aaa refresh=bbb echo=aaa";
        let redacted = redact_known_secrets(text, &["aaa", "bbb"]);
        assert!(!redacted.contains("aaa"));
        assert!(!redacted.contains("bbb"));
        assert_eq!(
            redacted,
            "token=[REDACTED] refresh=[REDACTED] echo=[REDACTED]"
        );
    }

    #[test]
    fn test_redact_known_secrets_no_match() {
        let text = "safe text with no secrets";
        let redacted = redact_known_secrets(text, &["not-present"]);
        assert_eq!(redacted, text);
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
}
