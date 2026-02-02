//! Authentication storage and API key resolution.
//!
//! Auth file: ~/.pi/agent/auth.json

use crate::error::{Error, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Credentials stored in auth.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthCredential {
    ApiKey { key: String },
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
            let _lock = lock_file(&file, Duration::from_secs(30))?;
            let content = fs::read_to_string(&path)?;
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
            .open(&self.path)?;
        let _lock = lock_file(&file, Duration::from_secs(30))?;

        let data = serde_json::to_string_pretty(&AuthFile {
            entries: self.entries.clone(),
        })?;
        fs::write(&self.path, data)?;

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

    /// Get API key for provider from auth.json.
    pub fn api_key(&self, provider: &str) -> Option<String> {
        match self.entries.get(provider) {
            Some(AuthCredential::ApiKey { key }) => Some(key.clone()),
            Some(AuthCredential::OAuth { access_token, expires, .. }) => {
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

fn lock_file(file: &File, timeout: Duration) -> Result<LockGuard<'_>> {
    let start = Instant::now();
    loop {
        match file.try_lock_exclusive() {
            Ok(true) => return Ok(LockGuard { file }),
            Ok(false) | Err(_) => {
                if start.elapsed() >= timeout {
                    return Err(Error::auth(
                        "Timed out waiting for auth lock".to_string()
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

struct LockGuard<'a> {
    file: &'a File,
}

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Convenience to load auth from default path.
pub fn load_default_auth(path: &Path) -> Result<AuthStorage> {
    AuthStorage::load(path.to_path_buf())
}
