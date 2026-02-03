//! Authentication storage and API key resolution.
//!
//! Auth file: ~/.pi/agent/auth.json

use crate::error::{Error, Result};
use base64::Engine as _;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::HashMap;
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

        env_key_for_provider(provider)
            .and_then(|var| std::env::var(var).ok())
            .filter(|v| !v.is_empty())
    }

    /// Refresh any expired OAuth tokens that this binary knows how to refresh.
    ///
    /// This keeps startup behavior predictable: models that rely on OAuth credentials remain
    /// available after restart without requiring the user to re-login.
    pub async fn refresh_expired_oauth_tokens(&mut self) -> Result<()> {
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
                "anthropic" => refresh_anthropic_oauth_token(&refresh_token).await?,
                _ => continue,
            };
            self.entries.insert(provider, refreshed);
            self.save()?;
        }

        Ok(())
    }
}

fn env_key_for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        "google" => Some("GOOGLE_API_KEY"),
        "google-vertex" => Some("GOOGLE_CLOUD_API_KEY"),
        "amazon-bedrock" => Some("AWS_ACCESS_KEY_ID"),
        "azure-openai" => Some("AZURE_OPENAI_API_KEY"),
        "github-copilot" => Some("GITHUB_COPILOT_API_KEY"),
        "xai" => Some("XAI_API_KEY"),
        "groq" => Some("GROQ_API_KEY"),
        "cerebras" => Some("CEREBRAS_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct OAuthStartInfo {
    pub provider: String,
    pub url: String,
    pub verifier: String,
    pub instructions: Option<String>,
}

/// Start Anthropic OAuth by generating an authorization URL and PKCE verifier.
pub fn start_anthropic_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();

    let mut url = reqwest::Url::parse(ANTHROPIC_OAUTH_AUTHORIZE_URL)
        .map_err(|e| Error::auth(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", ANTHROPIC_OAUTH_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", ANTHROPIC_OAUTH_REDIRECT_URI)
        .append_pair("scope", ANTHROPIC_OAUTH_SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &verifier);

    Ok(OAuthStartInfo {
        provider: "anthropic".to_string(),
        url: url.to_string(),
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

    let client = reqwest::Client::new();
    let response = client
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": ANTHROPIC_OAUTH_REDIRECT_URI,
            "code_verifier": verifier,
        }))
        .send()
        .await
        .map_err(|e| Error::auth(format!("Token exchange failed: {e}")))?;

    if !response.status().is_success() {
        let text = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read body>".to_string());
        return Err(Error::auth(format!("Token exchange failed: {text}")));
    }

    let oauth_response = response
        .json::<OAuthTokenResponse>()
        .await
        .map_err(|e| Error::auth(format!("Invalid token response: {e}")))?;

    Ok(AuthCredential::OAuth {
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
    })
}

async fn refresh_anthropic_oauth_token(refresh_token: &str) -> Result<AuthCredential> {
    let client = reqwest::Client::new();
    let response = client
        .post(ANTHROPIC_OAUTH_TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": ANTHROPIC_OAUTH_CLIENT_ID,
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|e| Error::auth(format!("Anthropic token refresh failed: {e}")))?;

    if !response.status().is_success() {
        let text = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read body>".to_string());
        return Err(Error::auth(format!(
            "Anthropic token refresh failed: {text}"
        )));
    }

    let oauth_response = response
        .json::<OAuthTokenResponse>()
        .await
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

    if let Ok(url) = reqwest::Url::parse(value) {
        let code = url
            .query_pairs()
            .find_map(|(k, v)| (k == "code").then_some(v.into_owned()));
        let state = url
            .query_pairs()
            .find_map(|(k, v)| (k == "state").then_some(v.into_owned()));
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
        let url = reqwest::Url::parse(&info.url).expect("parse url");
        assert_eq!(
            url.as_str().split('?').next().unwrap(),
            ANTHROPIC_OAUTH_AUTHORIZE_URL
        );

        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
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
}
