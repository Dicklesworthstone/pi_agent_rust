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
            let parsed: AuthFile = serde_json::from_str(&content).unwrap_or_default();
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
            let parsed: AuthFile = serde_json::from_str(&content).unwrap_or_default();
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
        if let Some(key) = override_key {
            return Some(key.to_string());
        }

        if let Some(key) = self.api_key(provider) {
            return Some(key);
        }

        env_keys_for_provider(provider)
            .iter()
            .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
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

        for (provider, refresh_token, config) in refreshes {
            let refreshed = refresh_extension_oauth_token(client, &config, &refresh_token).await?;
            self.entries.insert(provider, refreshed);
            self.save_async().await?;
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

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!("Token exchange failed: {text}")));
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

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Anthropic token refresh failed: {text}"
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

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!("Token exchange failed: {text}")));
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

    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "Extension OAuth token refresh failed: {text}"
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

    fn next_token() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string()
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
}
