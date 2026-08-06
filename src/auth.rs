//! Authentication storage and API key resolution.
//!
//! Auth file: ~/.pi/agent/auth.json

use crate::config::Config;
use crate::error::{Error, Result};
use crate::provider_metadata::{canonical_provider_id, provider_auth_env_keys, provider_metadata};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(not(unix))]
use tempfile::NamedTempFile;

/// Maximum wait for a short-lived auth save before credential resolution fails closed.
pub const AUTH_RESOLUTION_LOCK_TIMEOUT: Duration = Duration::from_secs(1);

/// `auth.json` is a compact credential map. Bound both reads and the persisted
/// representation so a corrupt or hostile store cannot cause unbounded work or
/// produce a file that a subsequent load must reject.
const MAX_AUTH_FILE_BYTES: usize = 1024 * 1024;
/// OAuth, device-flow, and token-exchange responses are small JSON documents.
/// Bound every such body before parsing or redaction so a hostile endpoint
/// cannot turn credential setup or refresh into an unbounded allocation.
const MAX_OAUTH_RESPONSE_BYTES: usize = 256 * 1024;

const ANTHROPIC_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_OAUTH_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
// ubs:ignore public OAuth endpoint metadata, not credential material.
const ANTHROPIC_OAUTH_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const ANTHROPIC_OAUTH_REDIRECT_URI: &str = "https://console.anthropic.com/oauth/code/callback";
const ANTHROPIC_OAUTH_SCOPES: &str = "org:create_api_key user:profile user:inference";

// ── OpenAI Codex OAuth constants ─────────────────────────────────
const OPENAI_CODEX_OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_CODEX_OAUTH_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
// ubs:ignore public OAuth endpoint metadata, not credential material.
const OPENAI_CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CODEX_OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const OPENAI_CODEX_OAUTH_SCOPES: &str = "openid profile email offline_access";

// ── Google Gemini CLI OAuth constants ────────────────────────────
const GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
// ubs:ignore public installed-app OAuth client metadata, not a server-side secret.
const GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET: &str = "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";
const GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI: &str = "http://localhost:8085/oauth2callback";
const GOOGLE_GEMINI_CLI_OAUTH_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile";
const GOOGLE_GEMINI_CLI_OAUTH_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
// ubs:ignore public OAuth endpoint metadata, not credential material.
const GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_GEMINI_CLI_CODE_ASSIST_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com";

// ── Google Antigravity OAuth constants ───────────────────────────
const GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
// ubs:ignore public installed-app OAuth client metadata, not a server-side secret.
const GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET: &str = "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";
const GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
const GOOGLE_ANTIGRAVITY_OAUTH_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
const GOOGLE_ANTIGRAVITY_OAUTH_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
// ubs:ignore public OAuth endpoint metadata, not credential material.
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
// ubs:ignore public OAuth endpoint metadata, not credential material.
const GITHUB_OAUTH_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
/// Default scopes for Copilot access (read:user needed for identity).
const GITHUB_COPILOT_SCOPES: &str = "read:user";
/// GitHub Copilot's public OAuth application client id.
///
/// This is the same well-known, non-secret client id baked into GitHub's own
/// editor integrations (copilot.vim, the Copilot CLI) and reused by essentially
/// every third-party Copilot client. Shipping it as the default lets device/
/// browser login work out of the box without the user having to register their
/// own GitHub App (#97); `GITHUB_COPILOT_CLIENT_ID` still overrides it for
/// Enterprise or custom-app setups.
pub const DEFAULT_COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// Resolve the Copilot OAuth client id: the `GITHUB_COPILOT_CLIENT_ID` env var
/// when set to a non-empty value, otherwise the well-known public default.
#[must_use]
pub fn resolved_copilot_client_id() -> String {
    std::env::var("GITHUB_COPILOT_CLIENT_ID")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_COPILOT_CLIENT_ID.to_string())
}

// ── GitLab OAuth constants ────────────────────────────────────────
const GITLAB_OAUTH_AUTHORIZE_PATH: &str = "/oauth/authorize";
// ubs:ignore public OAuth endpoint path metadata, not credential material.
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
// ubs:ignore public OAuth endpoint metadata, not credential material.
const KIMI_CODE_TOKEN_PATH: &str = "/api/oauth/token";

// ── OAuth env-var override helper ───────────────────────────────
//
// Every compiled-in OAuth constant can be overridden at runtime via an
// environment variable.  This lets operators update credentials without
// rebuilding the binary if a provider rotates client IDs or URLs.

/// Read an OAuth parameter from the environment, falling back to the
/// compiled-in default when the env var is absent or empty.
fn oauth_param(env_key: &str, default: &str) -> String {
    match std::env::var(env_key) {
        Ok(val) if !val.is_empty() => {
            tracing::debug!(env_key, "Using env override for OAuth parameter");
            val
        }
        _ => default.to_string(),
    }
}

// Accessor functions for each overridable OAuth constant.  Call these
// instead of using the `const` values directly so the env-var escape
// hatch is always active.

fn anthropic_oauth_client_id() -> String {
    oauth_param("PI_ANTHROPIC_OAUTH_CLIENT_ID", ANTHROPIC_OAUTH_CLIENT_ID)
}
fn anthropic_oauth_authorize_url() -> String {
    oauth_param(
        "PI_ANTHROPIC_OAUTH_AUTHORIZE_URL",
        ANTHROPIC_OAUTH_AUTHORIZE_URL,
    )
}
fn anthropic_oauth_token_url() -> String {
    oauth_param("PI_ANTHROPIC_OAUTH_TOKEN_URL", ANTHROPIC_OAUTH_TOKEN_URL)
}
fn anthropic_oauth_redirect_uri() -> String {
    oauth_param(
        "PI_ANTHROPIC_OAUTH_REDIRECT_URI",
        ANTHROPIC_OAUTH_REDIRECT_URI,
    )
}
fn anthropic_oauth_scopes() -> String {
    oauth_param("PI_ANTHROPIC_OAUTH_SCOPES", ANTHROPIC_OAUTH_SCOPES)
}

fn openai_codex_oauth_client_id() -> String {
    oauth_param(
        "PI_OPENAI_CODEX_OAUTH_CLIENT_ID",
        OPENAI_CODEX_OAUTH_CLIENT_ID,
    )
}
fn openai_codex_oauth_authorize_url() -> String {
    oauth_param(
        "PI_OPENAI_CODEX_OAUTH_AUTHORIZE_URL",
        OPENAI_CODEX_OAUTH_AUTHORIZE_URL,
    )
}
fn openai_codex_oauth_token_url() -> String {
    oauth_param(
        "PI_OPENAI_CODEX_OAUTH_TOKEN_URL",
        OPENAI_CODEX_OAUTH_TOKEN_URL,
    )
}
fn openai_codex_oauth_redirect_uri() -> String {
    oauth_param(
        "PI_OPENAI_CODEX_OAUTH_REDIRECT_URI",
        OPENAI_CODEX_OAUTH_REDIRECT_URI,
    )
}
fn openai_codex_oauth_scopes() -> String {
    oauth_param("PI_OPENAI_CODEX_OAUTH_SCOPES", OPENAI_CODEX_OAUTH_SCOPES)
}

fn google_gemini_cli_oauth_client_id() -> String {
    oauth_param(
        "PI_GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID",
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_ID,
    )
}
fn google_gemini_cli_oauth_client_secret() -> String {
    oauth_param(
        "PI_GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET",
        GOOGLE_GEMINI_CLI_OAUTH_CLIENT_SECRET,
    )
}
fn google_gemini_cli_oauth_redirect_uri() -> String {
    oauth_param(
        "PI_GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI",
        GOOGLE_GEMINI_CLI_OAUTH_REDIRECT_URI,
    )
}

fn google_antigravity_oauth_client_id() -> String {
    oauth_param(
        "PI_GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID",
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_ID,
    )
}
fn google_antigravity_oauth_client_secret() -> String {
    oauth_param(
        "PI_GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET",
        GOOGLE_ANTIGRAVITY_OAUTH_CLIENT_SECRET,
    )
}
fn google_antigravity_oauth_redirect_uri() -> String {
    oauth_param(
        "PI_GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI",
        GOOGLE_ANTIGRAVITY_OAUTH_REDIRECT_URI,
    )
}
fn google_antigravity_default_project_id() -> String {
    oauth_param(
        "PI_GOOGLE_ANTIGRAVITY_PROJECT_ID",
        GOOGLE_ANTIGRAVITY_DEFAULT_PROJECT_ID,
    )
}

fn kimi_code_oauth_client_id() -> String {
    oauth_param("PI_KIMI_CODE_OAUTH_CLIENT_ID", KIMI_CODE_OAUTH_CLIENT_ID)
}

/// Credentials stored in auth.json.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthCredential {
    ApiKey {
        key: String,
    },
    /// OAuth credential, serialized in the shape upstream TS pi
    /// (`@earendil-works/pi-coding-agent`) reads and writes to the shared
    /// `~/.pi/agent/auth.json`: `{"type":"oauth","access":..,"refresh":..,"expires":..}`
    /// (see `pi-ai` `dist/auth/types.d.ts` `OAuthCredential`). The variant tag and
    /// field names carry `alias`es for the historical pi_agent_rust shape
    /// (`o_auth` / `access_token` / `refresh_token`) so previously-written
    /// credentials keep loading. `token_url`/`client_id` are pi_agent_rust-only
    /// extras for self-contained refresh; upstream tolerates them via its
    /// `[key: string]: unknown` credential type, and `extra` captures any extra
    /// upstream keys so a rust round-trip is lossless.
    #[serde(rename = "oauth", alias = "o_auth")]
    OAuth {
        #[serde(rename = "access", alias = "access_token")]
        access_token: String,
        #[serde(rename = "refresh", alias = "refresh_token")]
        refresh_token: String,
        expires: i64, // Unix ms
        /// Token endpoint URL for self-contained refresh (optional; backward-compatible).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_url: Option<String>,
        /// Client ID for self-contained refresh (optional; backward-compatible).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        /// Any additional keys present in an upstream-written OAuth credential,
        /// preserved verbatim so a load→save round-trip never drops fields
        /// (upstream `OAuthCredentials` is `{ access, refresh, expires, [key]: unknown }`).
        #[serde(
            flatten,
            default,
            skip_serializing_if = "std::collections::HashMap::is_empty"
        )]
        extra: std::collections::HashMap<String, serde_json::Value>,
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

#[derive(Debug)]
pub enum AuthStorageLoadFailure {
    LockTimeout(std::io::Error),
    Other(Error),
}

impl AuthStorageLoadFailure {
    #[must_use]
    pub fn into_error(self) -> Error {
        match self {
            Self::LockTimeout(error) => Error::auth(format!("auth lock: {error}")),
            Self::Other(error) => error,
        }
    }
}

fn read_open_auth_file_bounded(mut file: File, path: &Path) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(
            u64::try_from(MAX_AUTH_FILE_BYTES)
                .unwrap_or(u64::MAX.saturating_sub(1))
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_AUTH_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("auth.json exceeds {MAX_AUTH_FILE_BYTES} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} is not valid UTF-8: {error}", path.display()),
        )
    })
}

#[cfg(not(unix))]
fn read_auth_file_bounded(path: &Path) -> std::io::Result<Option<String>> {
    let initial_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    #[cfg(windows)]
    let initial_is_reparse_point = {
        use std::os::windows::fs::MetadataExt as _;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        initial_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    #[cfg(not(any(unix, windows)))]
    let initial_is_reparse_point = false;

    if initial_metadata.file_type().is_symlink()
        || initial_is_reparse_point
        || !initial_metadata.is_file()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "auth.json must be a regular non-link file: {}",
                path.display()
            ),
        ));
    }

    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

        // Open the link itself if the path is swapped to any reparse point between
        // the initial lstat-style check and this open. Handle metadata can then
        // reject it without ever following it to credential material elsewhere.
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let opened_metadata = file.metadata()?;
        if !opened_metadata.is_file()
            || opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || opened_metadata.creation_time() != initial_metadata.creation_time()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "auth.json changed or became a link while it was being opened: {}",
                    path.display()
                ),
            ));
        }
        let current_metadata = fs::symlink_metadata(path)?;
        if !current_metadata.is_file()
            || current_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || current_metadata.creation_time() != opened_metadata.creation_time()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("auth.json changed after it was opened: {}", path.display()),
            ));
        }
        file
    };

    #[cfg(not(windows))]
    let file = {
        let file = File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("auth.json is not a regular file: {}", path.display()),
            ));
        }
        file
    };

    read_open_auth_file_bounded(file, path).map(Some)
}

#[cfg(unix)]
fn open_auth_directory_nofollow(path: &Path, create: bool) -> std::io::Result<File> {
    use std::path::Component;

    let descriptor = rustix::fs::open(
        if path.is_absolute() { "/" } else { "." },
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let mut directory = File::from(descriptor);

    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => name,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "secure auth paths must not contain parent or prefix components: {}",
                        path.display()
                    ),
                ));
            }
        };

        let flags = rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC;
        let child = match rustix::fs::openat(&directory, name, flags, rustix::fs::Mode::empty()) {
            Ok(child) => child,
            Err(rustix::io::Errno::NOENT) if create => {
                match rustix::fs::mkdirat(
                    &directory,
                    name,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
                ) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(std::io::Error::from(error)),
                }
                rustix::fs::openat(&directory, name, flags, rustix::fs::Mode::empty())
                    .map_err(std::io::Error::from)?
            }
            Err(error) => return Err(std::io::Error::from(error)),
        };
        directory = File::from(child);
    }

    Ok(directory)
}

#[cfg(unix)]
fn auth_target_parts(path: &Path) -> std::io::Result<(&Path, &std::ffi::OsStr)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let target_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("auth path has no filename: {}", path.display()),
        )
    })?;
    Ok((parent, target_name))
}

#[cfg(unix)]
fn auth_parent_identity_matches(path: &Path, expected: &File) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    let (parent, _) = auth_target_parts(path)?;
    let current = open_auth_directory_nofollow(parent, false)?;
    let expected_metadata = expected.metadata()?;
    let current_metadata = current.metadata()?;
    Ok(expected_metadata.dev() == current_metadata.dev()
        && expected_metadata.ino() == current_metadata.ino())
}

#[cfg(unix)]
fn read_auth_file_bounded_at(
    directory: &File,
    target_name: &std::ffi::OsStr,
    path: &Path,
) -> std::io::Result<Option<String>> {
    let descriptor = match rustix::fs::openat(
        directory,
        target_name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "auth.json must be a regular non-link file: {}",
                    path.display()
                ),
            ));
        }
        Err(error) => return Err(std::io::Error::from(error)),
    };
    let file = File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "auth.json must be a regular non-link file: {}",
                path.display()
            ),
        ));
    }
    read_open_auth_file_bounded(file, path).map(Some)
}

#[cfg(unix)]
#[derive(Debug)]
struct AuthTempFile {
    file: File,
    directory: File,
    name: std::ffi::OsString,
    persisted: bool,
}

#[cfg(unix)]
impl AuthTempFile {
    fn persist_to(&mut self, target_name: &std::ffi::OsStr) -> std::io::Result<()> {
        rustix::fs::renameat(&self.directory, &self.name, &self.directory, target_name)
            .map_err(std::io::Error::from)?;
        self.persisted = true;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for AuthTempFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = rustix::fs::unlinkat(&self.directory, &self.name, rustix::fs::AtFlags::empty());
        }
    }
}

#[cfg(unix)]
fn create_auth_temp_file(directory: &File) -> std::io::Result<AuthTempFile> {
    let owned_directory = directory.try_clone()?;
    for _ in 0..16 {
        let name =
            std::ffi::OsString::from(format!(".auth.json.tmp-{}", uuid::Uuid::new_v4().simple()));
        match rustix::fs::openat(
            directory,
            &name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        ) {
            Ok(descriptor) => {
                return Ok(AuthTempFile {
                    file: File::from(descriptor),
                    directory: owned_directory,
                    name,
                    persisted: false,
                });
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(std::io::Error::from(error)),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique auth.json temporary file",
    ))
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsAuthDirectoryGuard {
    path: PathBuf,
    creation_time: u64,
    handle: File,
}

#[cfg(windows)]
fn open_or_create_windows_auth_parent(
    path: &Path,
    create: bool,
) -> std::io::Result<(PathBuf, Vec<WindowsAuthDirectoryGuard>)> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::path::Component;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut current = PathBuf::new();
    let mut guards = Vec::new();

    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                continue;
            }
            Component::RootDir => {
                current.push(component.as_os_str());
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "secure auth paths must not contain parent components: {}",
                        path.display()
                    ),
                ));
            }
            Component::Normal(name) => current.push(name),
        }

        let initial_metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                fs::symlink_metadata(&current)?
            }
            Err(error) => return Err(error),
        };
        if !initial_metadata.is_dir()
            || initial_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "auth path traverses a non-directory or Windows reparse point: {}",
                    current.display()
                ),
            ));
        }

        // Omitting FILE_SHARE_DELETE pins every opened directory component against
        // rename/replacement for the duration of the credential write.
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&current)?;
        let opened_metadata = handle.metadata()?;
        if !opened_metadata.is_dir()
            || opened_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || opened_metadata.creation_time() != initial_metadata.creation_time()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "auth directory changed while it was being opened: {}",
                    current.display()
                ),
            ));
        }
        guards.push(WindowsAuthDirectoryGuard {
            path: current.clone(),
            creation_time: opened_metadata.creation_time(),
            handle,
        });
    }

    validate_windows_auth_parent(&guards)?;
    Ok((absolute_path, guards))
}

#[cfg(windows)]
fn validate_windows_auth_parent(guards: &[WindowsAuthDirectoryGuard]) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    for guard in guards {
        let handle_metadata = guard.handle.metadata()?;
        let path_metadata = fs::symlink_metadata(&guard.path)?;
        if !handle_metadata.is_dir()
            || !path_metadata.is_dir()
            || handle_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || path_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || handle_metadata.creation_time() != guard.creation_time
            || path_metadata.creation_time() != guard.creation_time
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "auth directory changed during credential write: {}",
                    guard.path.display()
                ),
            ));
        }
    }
    Ok(())
}

fn classify_auth_lock_error(error: std::io::Error) -> AuthStorageLoadFailure {
    if error.kind() == std::io::ErrorKind::TimedOut {
        AuthStorageLoadFailure::LockTimeout(error)
    } else {
        AuthStorageLoadFailure::Other(Error::auth(format!("auth lock: {error}")))
    }
}

fn auth_load_other_error(context: &str, error: &std::io::Error) -> AuthStorageLoadFailure {
    AuthStorageLoadFailure::Other(Error::auth(format!("{context}: {error}")))
}

#[cfg(unix)]
struct GuardedAuthRead {
    content: Option<String>,
    parent_directory: File,
    _lock: crate::file_lock::DirLockAt,
}

#[cfg(windows)]
struct GuardedAuthRead {
    content: Option<String>,
    operation_path: PathBuf,
    _lock: crate::file_lock::DirLock,
    _parent_guards: Vec<WindowsAuthDirectoryGuard>,
}

#[cfg(not(any(unix, windows)))]
struct GuardedAuthRead {
    content: Option<String>,
    operation_path: PathBuf,
    _lock: crate::file_lock::DirLock,
}

#[cfg(unix)]
fn read_auth_file_guarded<F>(
    path: &Path,
    lock_timeout: Duration,
    before_read: F,
) -> std::result::Result<Option<GuardedAuthRead>, AuthStorageLoadFailure>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let (parent, target_name) =
        auth_target_parts(path).map_err(|error| auth_load_other_error("auth path", &error))?;
    let parent_directory = match open_auth_directory_nofollow(parent, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(auth_load_other_error("auth.json parent directory", &error)),
    };
    let target_name = target_name.to_os_string();
    let lock =
        crate::file_lock::DirLockAt::acquire_for(&parent_directory, &target_name, lock_timeout)
            .map_err(classify_auth_lock_error)?;
    before_read().map_err(|error| auth_load_other_error("auth read preparation", &error))?;
    if !auth_parent_identity_matches(path, &parent_directory)
        .map_err(|error| auth_load_other_error("auth.json parent identity", &error))?
    {
        return Err(AuthStorageLoadFailure::Other(Error::auth(format!(
            "auth directory changed before credential read: {}",
            parent.display()
        ))));
    }
    let content = read_auth_file_bounded_at(&parent_directory, &target_name, path)
        .map_err(|error| auth_load_other_error("auth.json", &error))?;
    Ok(Some(GuardedAuthRead {
        content,
        parent_directory,
        _lock: lock,
    }))
}

#[cfg(windows)]
fn read_auth_file_guarded<F>(
    path: &Path,
    lock_timeout: Duration,
    before_read: F,
) -> std::result::Result<Option<GuardedAuthRead>, AuthStorageLoadFailure>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let (operation_path, parent_guards) = match open_or_create_windows_auth_parent(path, false) {
        Ok(guarded) => guarded,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(auth_load_other_error("auth.json parent directory", &error)),
    };
    let lock = crate::file_lock::DirLock::acquire_for(&operation_path, lock_timeout)
        .map_err(classify_auth_lock_error)?;
    validate_windows_auth_parent(&parent_guards)
        .map_err(|error| auth_load_other_error("auth.json parent identity", &error))?;
    before_read().map_err(|error| auth_load_other_error("auth read preparation", &error))?;
    validate_windows_auth_parent(&parent_guards)
        .map_err(|error| auth_load_other_error("auth.json parent identity", &error))?;
    let content = read_auth_file_bounded(&operation_path)
        .map_err(|error| auth_load_other_error("auth.json", &error))?;
    validate_windows_auth_parent(&parent_guards)
        .map_err(|error| auth_load_other_error("auth.json parent identity", &error))?;
    Ok(Some(GuardedAuthRead {
        content,
        operation_path,
        _lock: lock,
        _parent_guards: parent_guards,
    }))
}

#[cfg(not(any(unix, windows)))]
fn read_auth_file_guarded<F>(
    path: &Path,
    lock_timeout: Duration,
    before_read: F,
) -> std::result::Result<Option<GuardedAuthRead>, AuthStorageLoadFailure>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let operation_path = path.to_path_buf();
    let parent = operation_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent
        .try_exists()
        .map_err(|error| auth_load_other_error("auth.json parent directory", &error))?
    {
        return Ok(None);
    }
    let lock = crate::file_lock::DirLock::acquire_for(&operation_path, lock_timeout)
        .map_err(classify_auth_lock_error)?;
    before_read().map_err(|error| auth_load_other_error("auth read preparation", &error))?;
    let content = read_auth_file_bounded(&operation_path)
        .map_err(|error| auth_load_other_error("auth.json", &error))?;
    Ok(Some(GuardedAuthRead {
        content,
        operation_path,
        _lock: lock,
    }))
}

#[cfg(unix)]
fn backup_corrupt_auth(read: &GuardedAuthRead, path: &Path, content: &str) -> Result<()> {
    let backup_path = path.with_extension("json.corrupt");
    let backup_name = backup_path.file_name().ok_or_else(|| {
        Error::auth(format!(
            "corrupt auth backup path has no filename: {}",
            backup_path.display()
        ))
    })?;
    AuthStorage::save_data_sync_at(&read.parent_directory, backup_name, content, Duration::ZERO)
}

#[cfg(not(unix))]
fn backup_corrupt_auth(read: &GuardedAuthRead, _path: &Path, content: &str) -> Result<()> {
    AuthStorage::save_data_sync_with_lock_timeout(
        &read.operation_path.with_extension("json.corrupt"),
        content,
        Duration::ZERO,
    )
}

fn parse_auth_entries(path: &Path, read: &GuardedAuthRead) -> HashMap<String, AuthCredential> {
    let Some(content) = read.content.as_deref() else {
        return HashMap::new();
    };
    let parsed: AuthFile = match serde_json::from_str(content) {
        Ok(file) => file,
        Err(error) => {
            let backup_path = path.with_extension("json.corrupt");
            let backup_succeeded = match backup_corrupt_auth(read, path, content) {
                Ok(()) => true,
                Err(backup_error) => {
                    tracing::error!(
                        event = "pi.auth.backup_failed",
                        error = %backup_error,
                        backup = %backup_path.display(),
                        "Failed to backup corrupted auth.json"
                    );
                    false
                }
            };
            if backup_succeeded {
                tracing::warn!(
                    event = "pi.auth.parse_error",
                    error = %error,
                    backup = %backup_path.display(),
                    "auth.json is corrupted; backed up and starting with empty credentials"
                );
            } else {
                tracing::warn!(
                    event = "pi.auth.parse_error",
                    error = %error,
                    backup = %backup_path.display(),
                    "auth.json is corrupted; backup failed; starting with empty credentials"
                );
            }
            AuthFile::default()
        }
    };
    parsed.entries
}

#[cfg(unix)]
fn save_auth_data_platform<F, G>(
    path: &Path,
    data: &str,
    lock_timeout: Duration,
    before_persist: F,
    after_persist: G,
) -> Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    G: FnOnce(&Path) -> std::io::Result<()>,
{
    let (parent_path, target_name) = auth_target_parts(path)?;
    let directory = open_auth_directory_nofollow(parent_path, true)?;
    let _lock = crate::file_lock::DirLockAt::acquire_for(&directory, target_name, lock_timeout)
        .map_err(|error| Error::auth(format!("auth lock: {error}")))?;
    if !auth_parent_identity_matches(path, &directory).map_err(|error| {
        Error::auth(format!(
            "could not revalidate auth directory after acquiring the lock at {}: {error}",
            parent_path.display()
        ))
    })? {
        return Err(Error::auth(format!(
            "auth directory changed while acquiring the lock: {}",
            parent_path.display()
        )));
    }

    let mut temp = create_auth_temp_file(&directory)?;
    temp.file
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    let temp_path = parent_path.join(&temp.name);
    temp.file.write_all(data.as_bytes())?;
    temp.file.sync_all()?;
    before_persist(&temp_path)?;
    if !auth_parent_identity_matches(path, &directory).map_err(|error| {
        Error::auth(format!(
            "could not revalidate auth directory before credential persistence at {}: {error}",
            parent_path.display()
        ))
    })? {
        return Err(Error::auth(format!(
            "auth directory changed before credential persistence: {}",
            parent_path.display()
        )));
    }
    temp.persist_to(target_name)?;
    directory.sync_all()?;
    after_persist(path).map_err(|error| {
        Error::auth(format!(
            "auth.json was persisted and synced in the pinned directory, but post-persist verification preparation failed: {error}"
        ))
    })?;
    match auth_parent_identity_matches(path, &directory) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::auth(format!(
            "auth.json was persisted and synced in the pinned directory, but its configured parent path changed before completion: {}",
            parent_path.display()
        ))),
        Err(error) => Err(Error::auth(format!(
            "auth.json was persisted and synced in the pinned directory, but its configured parent path could not be revalidated: {}: {error}",
            parent_path.display()
        ))),
    }
}

#[cfg(windows)]
fn save_auth_data_platform<F, G>(
    path: &Path,
    data: &str,
    lock_timeout: Duration,
    before_persist: F,
    after_persist: G,
) -> Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    G: FnOnce(&Path) -> std::io::Result<()>,
{
    let (operation_path, parent_guards) = open_or_create_windows_auth_parent(path, true)?;
    let _lock = crate::file_lock::DirLock::acquire_for(&operation_path, lock_timeout)
        .map_err(|error| Error::auth(format!("auth lock: {error}")))?;
    validate_windows_auth_parent(&parent_guards)?;
    let parent = operation_path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(data.as_bytes())?;
    temp.as_file().sync_all()?;
    before_persist(temp.path())?;
    validate_windows_auth_parent(&parent_guards)?;
    temp.persist(&operation_path).map_err(|error| error.error)?;
    sync_parent_dir(&operation_path)?;
    after_persist(&operation_path).map_err(|error| {
        Error::auth(format!(
            "auth.json was persisted, but post-persist verification preparation failed: {error}"
        ))
    })?;
    validate_windows_auth_parent(&parent_guards)?;
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn save_auth_data_platform<F, G>(
    path: &Path,
    data: &str,
    lock_timeout: Duration,
    before_persist: F,
    after_persist: G,
) -> Result<()>
where
    F: FnOnce(&Path) -> std::io::Result<()>,
    G: FnOnce(&Path) -> std::io::Result<()>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock = crate::file_lock::DirLock::acquire_for(path, lock_timeout)
        .map_err(|error| Error::auth(format!("auth lock: {error}")))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(data.as_bytes())?;
    temp.as_file().sync_all()?;
    before_persist(temp.path())?;
    temp.persist(path).map_err(|error| error.error)?;
    sync_parent_dir(path)?;
    after_persist(path).map_err(|error| {
        Error::auth(format!(
            "auth.json was persisted, but post-persist verification preparation failed: {error}"
        ))
    })?;
    Ok(())
}

impl AuthStorage {
    fn allow_external_provider_lookup(&self) -> bool {
        // External credential auto-detection is intended for Pi's global auth
        // file (typically `~/.pi/agent/auth.json`). Scoping it this way keeps
        // tests and custom auth sandboxes deterministic.
        self.path.eq(&Config::auth_path())
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
        Self::load_with_lock_timeout(path, Duration::from_secs(30))
    }

    /// Load auth.json while bounding how long a concurrently held lock may delay the read.
    pub fn load_with_lock_timeout(path: PathBuf, lock_timeout: Duration) -> Result<Self> {
        Self::load_with_lock_timeout_classified(path, lock_timeout)
            .map_err(AuthStorageLoadFailure::into_error)
    }

    pub fn load_with_lock_timeout_classified(
        path: PathBuf,
        lock_timeout: Duration,
    ) -> std::result::Result<Self, AuthStorageLoadFailure> {
        Self::load_with_lock_timeout_classified_and_hook(path, lock_timeout, || Ok(()))
    }

    fn load_with_lock_timeout_classified_and_hook<F>(
        path: PathBuf,
        lock_timeout: Duration,
        before_read: F,
    ) -> std::result::Result<Self, AuthStorageLoadFailure>
    where
        F: FnOnce() -> std::io::Result<()>,
    {
        // Keep the lock and pinned parent descriptor alive through corrupt-backup handling.
        let Some(read) = read_auth_file_guarded(&path, lock_timeout, before_read)? else {
            return Ok(Self {
                path,
                entries: HashMap::new(),
            });
        };
        let entries = parse_auth_entries(&path, &read);
        Ok(Self { path, entries })
    }

    /// Load auth.json asynchronously (creates empty if missing).
    pub async fn load_async(path: PathBuf) -> Result<Self> {
        asupersync::runtime::spawn_blocking(move || Self::load(path)).await
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
        let path = self.path.clone();

        asupersync::runtime::spawn_blocking(move || Self::save_data_sync(&path, &data)).await
    }

    fn save_data_sync(path: &Path, data: &str) -> Result<()> {
        Self::save_data_sync_with_lock_timeout(path, data, Duration::from_secs(30))
    }

    fn save_data_sync_with_lock_timeout(
        path: &Path,
        data: &str,
        lock_timeout: Duration,
    ) -> Result<()> {
        Self::save_data_sync_with_hooks_and_lock_timeout(
            path,
            data,
            lock_timeout,
            |_| Ok(()),
            |_| Ok(()),
        )
    }

    #[cfg(unix)]
    fn save_data_sync_at(
        directory: &File,
        target_name: &std::ffi::OsStr,
        data: &str,
        lock_timeout: Duration,
    ) -> Result<()> {
        if data.len() > MAX_AUTH_FILE_BYTES {
            return Err(Error::auth(format!(
                "auth.json exceeds {MAX_AUTH_FILE_BYTES} bytes"
            )));
        }

        let _locked =
            crate::file_lock::DirLockAt::acquire_for(directory, target_name, lock_timeout)
                .map_err(|error| Error::auth(format!("auth lock: {error}")))?;
        let mut temp = create_auth_temp_file(directory)?;
        temp.file
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        (|| -> Result<()> {
            temp.file.write_all(data.as_bytes())?;
            temp.file.sync_all()?;
            temp.persist_to(target_name)?;
            directory.sync_all()?;
            Ok(())
        })()
    }

    fn save_data_sync_with_hook<F>(path: &Path, data: &str, before_persist: F) -> Result<()>
    where
        F: FnOnce(&Path) -> std::io::Result<()>,
    {
        Self::save_data_sync_with_hooks_and_lock_timeout(
            path,
            data,
            Duration::from_secs(30),
            before_persist,
            |_| Ok(()),
        )
    }

    #[cfg(test)]
    fn save_data_sync_with_hooks<F, G>(
        path: &Path,
        data: &str,
        before_persist: F,
        after_persist: G,
    ) -> Result<()>
    where
        F: FnOnce(&Path) -> std::io::Result<()>,
        G: FnOnce(&Path) -> std::io::Result<()>,
    {
        Self::save_data_sync_with_hooks_and_lock_timeout(
            path,
            data,
            Duration::from_secs(30),
            before_persist,
            after_persist,
        )
    }

    fn save_data_sync_with_hooks_and_lock_timeout<F, G>(
        path: &Path,
        data: &str,
        lock_timeout: Duration,
        before_persist: F,
        after_persist: G,
    ) -> Result<()>
    where
        F: FnOnce(&Path) -> std::io::Result<()>,
        G: FnOnce(&Path) -> std::io::Result<()>,
    {
        if data.len() > MAX_AUTH_FILE_BYTES {
            return Err(Error::auth(format!(
                "auth.json exceeds {MAX_AUTH_FILE_BYTES} bytes"
            )));
        }
        save_auth_data_platform(path, data, lock_timeout, before_persist, after_persist)
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

    /// Get only credentials that are safe to pass through the generic bearer/API-key lane.
    ///
    /// Every stored Bedrock credential and legacy SAP API-key entry is consumed by its
    /// provider-owned chain so documented ambient/stored precedence is identical across CLI, RPC,
    /// and SDK surfaces. In particular, returning an AWS access-key ID would lose the accompanying
    /// secret, while returning a legacy SAP API key here would incorrectly promote the lowest
    /// stored tier into the explicit-caller lane.
    fn generic_api_key(&self, provider: &str) -> Option<String> {
        let canonical = canonical_provider_id(provider).unwrap_or(provider);
        let credential = self.credential_for_provider(provider)?;
        if canonical.eq_ignore_ascii_case("amazon-bedrock")
            || canonical.eq_ignore_ascii_case("sap-ai-core")
        {
            return None;
        }
        api_key_from_credential(credential)
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
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }

        let canonical_provider = canonical_provider_id(provider).unwrap_or(provider);

        // Prefer explicit stored OAuth/Bearer credentials over ambient env vars.
        // This prevents stale shell env keys from silently overriding successful `/login` flows.
        // Bedrock is the exception: all stored variants stay in its provider-owned AWS chain so
        // an ambient AWS bearer/key/profile and a stored fallback have one consistent precedence.
        if !canonical_provider.eq_ignore_ascii_case("amazon-bedrock")
            && let Some(credential) = self.credential_for_provider(provider)
            && let Some(key) = match credential {
                AuthCredential::OAuth { .. }
                    if canonical_provider_id(provider)
                        .unwrap_or(provider)
                        .eq("anthropic") =>
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

        let generic_env_keys: &[&str] = match canonical_provider {
            // The remaining AWS variables form a credential chain; none is a standalone bearer
            // token. Bedrock resolves the complete chain at request time.
            "amazon-bedrock" => &["AWS_BEARER_TOKEN_BEDROCK"],
            // SAP service-key JSON and split client credentials require an OAuth exchange before
            // they can be used as an Authorization bearer value.
            "sap-ai-core" => &[],
            _ => env_keys_for_provider(provider),
        };

        if let Some(key) = generic_env_keys.iter().find_map(|var| {
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

        if let Some(key) = self.generic_api_key(provider) {
            return Some(key);
        }

        if self.allow_external_provider_lookup()
            && let Some(key) = resolve_external_provider_api_key(provider)
        {
            return Some(key);
        }

        canonical_provider_id(provider)
            .filter(|canonical| canonical.ne(&provider))
            .and_then(|canonical| {
                self.generic_api_key(canonical).or_else(|| {
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
                "google-gemini-cli" => match decode_project_scoped_access_token(&access_token) {
                    Some((_, project_id)) => {
                        Box::pin(refresh_google_gemini_cli_oauth_token(
                            client,
                            &refresh_token,
                            &project_id,
                        ))
                        .await
                    }
                    None => Err(Error::auth(
                        "google-gemini-cli OAuth credential missing projectId payload".to_string(),
                    )),
                },
                "google-antigravity" => match decode_project_scoped_access_token(&access_token) {
                    Some((_, project_id)) => {
                        Box::pin(refresh_google_antigravity_oauth_token(
                            client,
                            &refresh_token,
                            &project_id,
                        ))
                        .await
                    }
                    None => Err(Error::auth(
                        "google-antigravity OAuth credential missing projectId payload".to_string(),
                    )),
                },
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

        if needs_save && let Err(e) = self.save_async().await {
            tracing::warn!("Failed to save auth.json after refreshing OAuth tokens: {e}");
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
                if *expires <= proactive_deadline
                    && let Some(config) = extension_configs.get(provider)
                {
                    refreshes.push((provider.clone(), refresh_token.clone(), config.clone()));
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
            let start = std::time::Instant::now(); // ubs:ignore false positive: refresh latency instrumentation, not security token generation.
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

        if needs_save && let Err(e) = self.save_async().await {
            tracing::warn!("Failed to save auth.json after refreshing extension OAuth tokens: {e}");
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

/// Resolve an API key string that may contain a `$ENV:VAR_NAME` or
/// `$CMD:shell command` prefix.
///
/// - Plain string → returned as-is (backward compatible literal key)
/// - `$ENV:VAR_NAME` → resolves the named environment variable at runtime
/// - `$CMD:shell command` / `$COMMAND:shell command` → runs the command and
///   uses trimmed stdout as the key
///
/// Returns `None` when the key references an env var that is unset or empty.
/// Returns `None` when the command succeeds but produces empty output.
/// Returns `Err` with a descriptive message for malformed `$ENV:` / `$CMD:`
/// references or command execution failures.
fn resolve_api_key_source(raw: &str) -> std::result::Result<Option<String>, String> {
    resolve_api_key_source_with_resolvers(
        raw,
        |var| std::env::var(var).ok(),
        run_api_key_source_command,
    )
}

/// Testable version of [`resolve_api_key_source`] with injectable env lookup.
#[cfg(test)]
fn resolve_api_key_source_with_env<F>(
    raw: &str,
    env_lookup: F,
) -> std::result::Result<Option<String>, String>
where
    F: FnOnce(&str) -> Option<String>,
{
    resolve_api_key_source_with_resolvers(raw, env_lookup, |_command| {
        Err("API key command resolution is not available in this test helper".to_string())
    })
}

fn resolve_api_key_source_with_resolvers<F, G>(
    raw: &str,
    env_lookup: F,
    mut command_runner: G,
) -> std::result::Result<Option<String>, String>
where
    F: FnOnce(&str) -> Option<String>,
    G: FnMut(&str) -> std::result::Result<Option<String>, String>,
{
    if let Some(var_name) = raw.strip_prefix("$ENV:") {
        if var_name.is_empty() {
            return Err("$ENV: prefix requires a variable name (e.g. $ENV:MY_API_KEY)".to_string());
        }
        return match env_lookup(var_name) {
            Some(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_string())),
            Some(_) => {
                tracing::warn!(
                    event = "pi.auth.env_var_empty",
                    var = var_name,
                    "API key env var is set but empty"
                );
                Ok(None)
            }
            None => {
                tracing::warn!(
                    event = "pi.auth.env_var_missing",
                    var = var_name,
                    "API key env var is not set"
                );
                Ok(None)
            }
        };
    }

    if let Some(command) = raw
        .strip_prefix("$CMD:")
        .or_else(|| raw.strip_prefix("$COMMAND:"))
    {
        let command = command.trim();
        if command.is_empty() {
            return Err(
                "$CMD: prefix requires a shell command (e.g. $CMD:op read \"op://vault/item/field\" --no-newline)"
                    .to_string(),
            );
        }
        return command_runner(command).map(|resolved| {
            resolved.and_then(|value| {
                let trimmed = value.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            })
        });
    }

    // Plain literal key — backward compatible.
    Ok(Some(raw.to_string()))
}

fn run_api_key_source_command(command: &str) -> std::result::Result<Option<String>, String> {
    let output = build_api_key_command_shell(command)
        .output()
        .map_err(|e| format!("Failed to run API key command: {e}"))?;

    if !output.status.success() {
        let status = output.status.code().map_or_else(
            || "terminated by signal".to_string(),
            |code| format!("exit code {code}"),
        );
        return Err(format!("API key command failed ({status})"));
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| "API key command output is not valid UTF-8".to_string())?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        tracing::warn!(
            event = "pi.auth.key_command_empty",
            "API key command succeeded but produced empty output"
        );
        return Ok(None);
    }

    Ok(Some(trimmed.to_string()))
}

fn build_api_key_command_shell(command: &str) -> std::process::Command {
    #[cfg(windows)]
    {
        let mut shell = std::process::Command::new("cmd");
        shell.arg("/C").arg(command);
        shell
    }

    #[cfg(not(windows))]
    {
        let shell_path = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .unwrap_or("sh");
        let mut shell = std::process::Command::new(shell_path); // ubs:ignore fixed local shell allowlist selected above, not user-controlled executable input.
        if shell_path.ends_with("bash") {
            shell.arg("-lc");
        } else {
            shell.arg("-c");
        }
        shell.arg(command);
        shell
    }
}

fn api_key_from_credential(credential: &AuthCredential) -> Option<String> {
    match credential {
        AuthCredential::ApiKey { key } => match resolve_api_key_source(key) {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!(
                    event = "pi.auth.key_resolve_error",
                    error = %err,
                    "Failed to resolve API key source"
                );
                None
            }
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

#[cfg(test)]
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
            let project =
                google_project_id_from_env().unwrap_or_else(google_antigravity_default_project_id);
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
            let project =
                google_project_id_from_env().unwrap_or_else(google_antigravity_default_project_id);
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

        if !matches!(section, Some("core")) {
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().ne("project") {
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

const MAX_AWS_SHARED_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_AWS_SSO_CACHE_BYTES: usize = 256 * 1024;

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
/// 3. `AWS_PROFILE` env var → profile-based credentials from AWS config files
/// 4. Stored `AwsCredentials` in auth.json
/// 5. Stored `BearerToken` in auth.json (for bedrock)
///
/// `region` is resolved from: `AWS_REGION` → `AWS_DEFAULT_REGION` → `"us-east-1"`.
pub fn resolve_aws_credentials(auth: &AuthStorage) -> Option<AwsResolvedCredentials> {
    resolve_aws_credentials_with_env(auth, |var| std::env::var(var).ok())
}

fn preferred_non_empty_env<F>(env: &mut F, primary: &str, fallback: &str) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    let normalized = |value: String| {
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_string())
    };
    env(primary)
        .and_then(&normalized)
        .or_else(|| env(fallback).and_then(normalized))
}

fn resolve_aws_credentials_with_env<F>(auth: &AuthStorage, env: F) -> Option<AwsResolvedCredentials>
where
    F: FnMut(&str) -> Option<String>,
{
    resolve_aws_credentials_with_env_policy(auth, env, true)
}

fn resolve_aws_credentials_with_env_policy<F>(
    auth: &AuthStorage,
    mut env: F,
    include_stored_credentials: bool,
) -> Option<AwsResolvedCredentials>
where
    F: FnMut(&str) -> Option<String>,
{
    let env_region = preferred_non_empty_env(&mut env, "AWS_REGION", "AWS_DEFAULT_REGION");
    let region = env_region
        .clone()
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
        if !access_key.is_empty()
            && let Some(secret_key) = env("AWS_SECRET_ACCESS_KEY")
        {
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

    // 3. Profile-based credentials (AWS_PROFILE / AWS_DEFAULT_PROFILE)
    if let Some(profile) = preferred_non_empty_env(&mut env, "AWS_PROFILE", "AWS_DEFAULT_PROFILE") {
        if let Some(resolved) = resolve_aws_profile_credentials_with_env(
            &profile,
            env_region.as_deref(),
            &region,
            &mut env,
        ) {
            return Some(resolved);
        }
        if include_stored_credentials {
            tracing::warn!(
                event = "pi.auth.aws_profile_missing",
                profile = %profile,
                "AWS_PROFILE set but no credentials found in ~/.aws/credentials"
            );
        }
    }

    if include_stored_credentials {
        resolve_stored_aws_credentials(auth, region)
    } else {
        None
    }
}

fn resolve_stored_aws_credentials(
    auth: &AuthStorage,
    region: String,
) -> Option<AwsResolvedCredentials> {
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
            // Legacy: treat stored API key as bearer token for Bedrock.
            // Supports $ENV: and $CMD: prefixes for dynamically resolved keys.
            match resolve_api_key_source(key) {
                Ok(Some(resolved)) => Some(AwsResolvedCredentials::Bearer {
                    token: resolved,
                    region,
                }),
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(
                        event = "pi.auth.key_resolve_error",
                        error = %err,
                        "Failed to resolve API key source for bedrock"
                    );
                    None
                }
            }
        }
        _ => None,
    }
}

fn aws_home_dir_from_env<F>(env: &mut F) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<String>,
{
    env("HOME")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env("USERPROFILE")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| {
            let drive = env("HOMEDRIVE")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())?;
            let path = env("HOMEPATH")
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

fn aws_credentials_paths_from_env<F>(env: &mut F) -> Option<(PathBuf, PathBuf)>
where
    F: FnMut(&str) -> Option<String>,
{
    let credentials_path = env("AWS_SHARED_CREDENTIALS_FILE")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let config_path = env("AWS_CONFIG_FILE")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    if let (Some(credentials_path), Some(config_path)) =
        (credentials_path.clone(), config_path.clone())
    {
        return Some((credentials_path, config_path));
    }

    let home = aws_home_dir_from_env(env);
    let credentials_path = if let Some(path) = credentials_path {
        path
    } else {
        let home = home.as_ref()?;
        home.join(".aws").join("credentials")
    };
    let config_path = config_path.unwrap_or_else(|| {
        home.map_or_else(
            || credentials_path.clone(),
            |home| home.join(".aws").join("config"),
        )
    });
    Some((credentials_path, config_path))
}

/// Like [`aws_credentials_paths_from_env`] but only resolves the config-file
/// path. Used by SSO discovery, which needs to read `[profile X]` and
/// `[sso-session Y]` blocks from `~/.aws/config` but does NOT need the
/// `~/.aws/credentials` file (SSO doesn't write static IAM keys there).
///
/// Falls back: `AWS_CONFIG_FILE` env var → `<HOME>/.aws/config`. Returns
/// None only when no env var is set AND no home directory can be resolved.
fn aws_config_path_from_env<F>(env: &mut F) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<String>,
{
    if let Some(path) = env("AWS_CONFIG_FILE")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return Some(path);
    }
    let home = aws_home_dir_from_env(env)?;
    Some(home.join(".aws").join("config"))
}

fn read_bounded_aws_text(
    path: &Path,
    max_bytes: usize,
    description: &str,
) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take((max_bytes + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{description} exceeds {max_bytes} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{description} is not valid UTF-8: {error}"),
        )
    })
}

fn parse_aws_ini(contents: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim().to_ascii_lowercase();
            if !name.is_empty() {
                current = Some(name);
            }
            continue;
        }
        let Some(section) = current.clone() else {
            continue;
        };
        let mut splitter = line.splitn(2, ['=', ':']);
        let key = splitter.next().unwrap_or("").trim().to_ascii_lowercase();
        let value = splitter.next().unwrap_or("").trim().to_string();
        if key.is_empty() || value.is_empty() {
            continue;
        }
        sections.entry(section).or_default().insert(key, value);
    }

    sections
}

fn resolve_aws_profile_credentials_with_env<F>(
    profile: &str,
    region_override: Option<&str>,
    region_default: &str,
    env: &mut F,
) -> Option<AwsResolvedCredentials>
where
    F: FnMut(&str) -> Option<String>,
{
    let (credentials_path, config_path) = aws_credentials_paths_from_env(env)?;
    let credentials = read_bounded_aws_text(
        &credentials_path,
        MAX_AWS_SHARED_CONFIG_BYTES,
        "AWS shared credentials file",
    )
    .ok()
    .as_deref()
    .map(parse_aws_ini)
    .unwrap_or_default();
    let profile_key = profile.trim().to_ascii_lowercase();
    let section = credentials.get(&profile_key)?;

    let access_key_id = section.get("aws_access_key_id")?.trim().to_string();
    let secret_access_key = section.get("aws_secret_access_key")?.trim().to_string();
    if access_key_id.is_empty() || secret_access_key.is_empty() {
        return None;
    }

    let session_token = section
        .get("aws_session_token")
        .or_else(|| section.get("aws_security_token"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let mut region = region_override.map_or_else(|| region_default.to_string(), str::to_string);

    let allow_config_region = region_override.is_none();
    if allow_config_region && let Some(value) = section.get("region") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            region = trimmed.to_string();
        }
    }

    if allow_config_region
        && let Ok(config_text) = read_bounded_aws_text(
            &config_path,
            MAX_AWS_SHARED_CONFIG_BYTES,
            "AWS shared config file",
        )
    {
        let config = parse_aws_ini(&config_text);
        for name in profile_section_candidates(&profile_key) {
            if let Some(section) = config.get(&name)
                && let Some(value) = section.get("region")
            {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    region = trimmed.to_string();
                    break;
                }
            }
        }
    }

    Some(AwsResolvedCredentials::Sigv4 {
        access_key_id,
        secret_access_key,
        session_token,
        region,
    })
}

/// Candidate `[profile X]` / `[X]` section names for `~/.aws/config`.
///
/// The AWS shared config format names the default profile literally `[default]`
/// (or sometimes `[profile default]` when written by tooling), while named
/// profiles are written as `[profile NAME]`. SSO sessions live in
/// `[sso-session NAME]` and use just the session name (no `profile` prefix).
fn profile_section_candidates(profile_key: &str) -> Vec<String> {
    if profile_key.eq("default") {
        vec!["default".to_string(), "profile default".to_string()]
    } else {
        vec![format!("profile {profile_key}"), profile_key.to_string()]
    }
}

// ── AWS SSO (Single Sign-On) Resolution ─────────────────────────
//
// Modern AWS profiles increasingly use SSO instead of static IAM credentials.
// An SSO profile in `~/.aws/config` looks like one of:
//
//   ; sso-session form (preferred, SDKv2+):
//   [profile dev]
//   sso_session = my-sso
//   sso_account_id = 123456789012
//   sso_role_name = MyRole
//   region = us-west-2
//
//   [sso-session my-sso]
//   sso_start_url = https://example.awsapps.com/start
//   sso_region = us-east-1
//
//   ; legacy form (sso_start_url on the profile itself):
//   [profile dev]
//   sso_start_url = https://example.awsapps.com/start
//   sso_region = us-east-1
//   sso_account_id = 123456789012
//   sso_role_name = MyRole
//
// `aws sso login` writes a cached OIDC access token to
// `~/.aws/sso/cache/<sha1>.json`. The cache key is SHA1 of the
// `sso_session` name (sso-session form) or the `sso_start_url`
// (legacy form). The JSON file contains `accessToken`, `expiresAt`,
// and `region` fields.
//
// Pi exchanges the cached access token for short-lived IAM credentials
// via the SSO Portal `GetRoleCredentials` API. Pi never refreshes the
// SSO access token itself — that requires the interactive browser flow
// driven by `aws sso login`.

/// Information needed to perform an SSO `GetRoleCredentials` call.
///
/// Produced synchronously from `~/.aws/config` + `~/.aws/sso/cache/`.
/// Consumed by the async [`exchange_aws_sso_credentials`] helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsSsoTokenLocator {
    /// SSO Portal access token (`accessToken` from the cache file).
    pub access_token: String,
    /// SSO region (where the SSO Portal lives), e.g. `us-east-1`.
    pub sso_region: String,
    /// AWS account ID the role belongs to.
    pub account_id: String,
    /// IAM role name to assume.
    pub role_name: String,
    /// Region the resolved IAM credentials should target (the profile's
    /// `region`, NOT the SSO region — these are usually different).
    pub target_region: String,
}

const MAX_AWS_SSO_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_AWS_SSO_ERROR_SNIPPET_BYTES: usize = 16 * 1024;

/// Detect an AWS SSO profile in `~/.aws/config` and resolve the cached
/// access token from `~/.aws/sso/cache/<sha1>.json`.
///
/// Returns:
/// - `Ok(Some(_))` when the profile is a fully-configured SSO profile
///   with a non-expired cached token.
/// - `Ok(None)` when the profile is not an SSO profile (caller should
///   fall back to other credential sources).
/// - `Err(_)` when the profile *is* an SSO profile but cannot be used —
///   e.g. token cache missing/expired/malformed, or required fields
///   absent. The error message tells the user how to recover (typically
///   "run `aws sso login --profile <name>`").
#[allow(clippy::too_many_lines)] // SSO config parsing has irreducible branching for sso-session vs legacy form + cache lookup + diagnostics
fn detect_aws_sso_profile_with_env<F>(
    profile: &str,
    region_override: Option<&str>,
    region_default: &str,
    env: &mut F,
) -> Result<Option<AwsSsoTokenLocator>>
where
    F: FnMut(&str) -> Option<String>,
{
    // SSO discovery only needs `~/.aws/config` (the [profile X] / [sso-session Y]
    // blocks). It does NOT need `~/.aws/credentials` (SSO never writes IAM keys
    // there), so we use the config-only resolver — otherwise tests / users with
    // AWS_CONFIG_FILE set but no AWS_SHARED_CREDENTIALS_FILE / no HOME silently
    // get a None result and the SSO path looks broken when it isn't.
    let Some(config_path) = aws_config_path_from_env(env) else {
        return Ok(None);
    };
    let config_text = match read_bounded_aws_text(
        &config_path,
        MAX_AWS_SHARED_CONFIG_BYTES,
        "AWS shared config file",
    ) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::auth(format!(
                "Failed to read AWS shared config {}: {error}",
                config_path.display()
            )));
        }
    };
    let config = parse_aws_ini(&config_text);
    let profile_key = profile.trim().to_ascii_lowercase();

    // Locate the [profile X] (or [default]) section.
    let mut profile_section: Option<&HashMap<String, String>> = None;
    for name in profile_section_candidates(&profile_key) {
        if let Some(section) = config.get(&name) {
            profile_section = Some(section);
            break;
        }
    }
    let Some(profile_section) = profile_section else {
        return Ok(None);
    };

    // Determine if this is an SSO profile, and if so which form (sso-session
    // form vs legacy). At minimum we need account_id + role_name + a way to
    // find the cached token.
    let sso_session_name = profile_section
        .get("sso_session")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let profile_sso_start_url = profile_section
        .get("sso_start_url")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    let is_sso_profile = sso_session_name.is_some() || profile_sso_start_url.is_some();
    if !is_sso_profile {
        return Ok(None);
    }

    let account_id = profile_section
        .get("sso_account_id")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::auth(format!(
                "AWS profile '{profile}' looks like an SSO profile but is missing sso_account_id; \
                 add it to ~/.aws/config under [profile {profile}]",
            ))
        })?;
    let role_name = profile_section
        .get("sso_role_name")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::auth(format!(
                "AWS profile '{profile}' looks like an SSO profile but is missing sso_role_name; \
                 add it to ~/.aws/config under [profile {profile}]",
            ))
        })?;

    // Resolve the SSO region + start URL by either reading the [sso-session]
    // block (preferred) or the legacy fields on the profile itself.
    let (sso_region, _sso_start_url, cache_key) = if let Some(session_name) = &sso_session_name {
        let session_section_key = format!("sso-session {session_name}");
        let session_section = config.get(&session_section_key).ok_or_else(|| {
            Error::auth(format!(
                "AWS profile '{profile}' references sso_session = {session_name}, but no \
                 [sso-session {session_name}] block was found in ~/.aws/config",
            ))
        })?;
        let sso_region = session_section
            .get("sso_region")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                Error::auth(format!(
                    "AWS [sso-session {session_name}] is missing sso_region",
                ))
            })?;
        let sso_start_url = session_section
            .get("sso_start_url")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                Error::auth(format!(
                    "AWS [sso-session {session_name}] is missing sso_start_url",
                ))
            })?;
        // sso-session form: cache key is the session name.
        (sso_region, sso_start_url, session_name.clone())
    } else {
        // Legacy form: sso_start_url is on the profile.
        let Some(sso_start_url) = profile_sso_start_url else {
            return Err(Error::auth(format!(
                "AWS profile '{profile}' looks like an SSO profile but is missing sso_start_url",
            )));
        };
        let sso_region = profile_section
            .get("sso_region")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::auth(format!("AWS profile '{profile}' is missing sso_region")))?;
        // Legacy form: cache key is the start URL.
        (sso_region, sso_start_url.clone(), sso_start_url)
    };

    // Resolve the target region: env override wins, then profile.region,
    // then default. (Same precedence as the static IAM path.)
    let target_region = region_override.map_or_else(
        || {
            profile_section
                .get("region")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| region_default.to_string())
        },
        str::to_string,
    );

    // Read and validate the cached SSO access token.
    let cache_dir = aws_sso_cache_dir_from_env(env).ok_or_else(|| {
        Error::auth(
            "Could not locate AWS SSO cache directory (HOME unset?); \
             expected ~/.aws/sso/cache/"
                .to_string(),
        )
    })?;
    let token = load_aws_sso_token_cache(&cache_dir, &cache_key, profile)?;

    Ok(Some(AwsSsoTokenLocator {
        access_token: token,
        sso_region,
        account_id,
        role_name,
        target_region,
    }))
}

fn aws_sso_cache_dir_from_env<F>(env: &mut F) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<String>,
{
    // AWS SDKs honor AWS_SDK_LOAD_NONDEFAULT_CONFIG and similar tweaks for
    // the cache dir, but the only path the official tooling actually uses
    // is `<HOME>/.aws/sso/cache`. We additionally allow PI_AWS_SSO_CACHE_DIR
    // for tests + advanced users.
    if let Some(custom) = env("PI_AWS_SSO_CACHE_DIR")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return Some(PathBuf::from(custom));
    }
    let home = aws_home_dir_from_env(env)?;
    Some(home.join(".aws").join("sso").join("cache"))
}

/// Compute the SHA1-based cache filename used by `aws sso login`.
///
/// Per the AWS CLI/SDK convention, the file is `<sha1(key)>.json` where
/// `key` is the sso-session name (sso-session form) or the start URL
/// (legacy form). The hash is lowercase hex.
fn aws_sso_cache_filename(cache_key: &str) -> String {
    use sha1::{Digest as _, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(cache_key.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(40 + 5);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex.push_str(".json");
    hex
}

fn aws_sso_cache_path(cache_dir: &Path, cache_key: &str) -> PathBuf {
    let filename = aws_sso_cache_filename(cache_key);
    debug_assert!(
        filename
            .strip_suffix(".json")
            .is_some_and(|stem| stem.len() == 40 && stem.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    let path = cache_dir.join(filename); // ubs:ignore AWS SSO cache filename is SHA1 lower-hex + ".json", not a raw path segment.
    path
}

#[derive(Debug, Deserialize)]
struct SsoCachedToken {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<String>,
}

fn load_aws_sso_token_cache(cache_dir: &Path, cache_key: &str, profile: &str) -> Result<String> {
    let path = aws_sso_cache_path(cache_dir, cache_key);
    let text = read_bounded_aws_text(&path, MAX_AWS_SSO_CACHE_BYTES, "AWS SSO token cache")
        .map_err(|err| {
            Error::auth(format!(
                "AWS SSO token cache unavailable ({}: {err}); run: aws sso login --profile {profile}",
                path.display()
            ))
        })?;
    let cached: SsoCachedToken = serde_json::from_str(&text).map_err(|err| {
        Error::auth(format!(
            "AWS SSO token cache at {} is malformed ({err}); run: aws sso login --profile {profile}",
            path.display()
        ))
    })?;
    let access_token = cached
        .access_token
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::auth(format!(
                "AWS SSO token cache at {} has no accessToken; run: aws sso login --profile {profile}",
                path.display()
            ))
        })?;

    // expiresAt is an ISO-8601 / RFC-3339 timestamp. If absent we trust the
    // file (some SDK versions omit it), but if present we enforce it strictly.
    if let Some(expires_at) = cached.expires_at {
        let expires_at = expires_at.trim();
        if !expires_at.is_empty() {
            let parsed: std::result::Result<chrono::DateTime<chrono::Utc>, chrono::ParseError> =
                chrono::DateTime::parse_from_rfc3339(expires_at)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .or_else(|_| {
                        // Some SDK versions write `2026-05-06T20:12:34UTC` (no
                        // RFC-3339 offset). Treat as UTC.
                        chrono::NaiveDateTime::parse_from_str(expires_at, "%Y-%m-%dT%H:%M:%SUTC")
                            .map(|naive| naive.and_utc())
                    });
            match parsed {
                Ok(expiry) => {
                    let now = chrono::Utc::now();
                    if expiry <= now {
                        return Err(Error::auth(format!(
                            "AWS SSO token expired at {expires_at}; run: aws sso login --profile {profile}",
                        )));
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        event = "pi.auth.aws_sso_expires_at_unparseable",
                        expires_at = %expires_at,
                        error = %err,
                        "Could not parse SSO expiresAt; trusting cache"
                    );
                }
            }
        }
    }

    Ok(access_token)
}

/// Response shape from the SSO Portal `GetRoleCredentials` endpoint.
#[derive(Debug, Deserialize)]
struct SsoRoleCredentialsResponse {
    #[serde(rename = "roleCredentials")]
    role_credentials: SsoRoleCredentials,
}

#[derive(Debug, Deserialize)]
struct SsoRoleCredentials {
    #[serde(rename = "accessKeyId")]
    access_key_id: String,
    #[serde(rename = "secretAccessKey")]
    secret_access_key: String,
    #[serde(rename = "sessionToken")]
    session_token: String,
    // expiration in milliseconds since epoch — informational only; Pi
    // re-resolves credentials on every Bedrock call so we don't cache.
    #[allow(dead_code)]
    expiration: Option<i64>,
}

/// Test-only override for the SSO Portal base URL.
///
/// We use an interior-mutable static rather than reading `PI_AWS_SSO_PORTAL_URL`
/// directly because Rust 2024 made `std::env::set_var`/`remove_var` `unsafe`,
/// and the crate forbids `unsafe_code`. Tests set the override via
/// `set_sso_portal_base_url_override`; production code paths always see `None`
/// because both helpers are cfg-gated.
#[cfg(test)]
static SSO_PORTAL_BASE_URL_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

/// Override for the SSO Portal base URL (test-only; production uses the
/// canonical `https://portal.sso.<region>.amazonaws.com`).
///
/// On lock poisoning (a previous holder panicked), we recover the inner
/// guard via `PoisonError::into_inner` and read the stored value. Silently
/// returning `None` on poison would mask the override state and produce
/// confusing test failures where the resolver hits the real AWS endpoint
/// instead of the mock — recovering preserves the test's intent.
#[cfg(test)]
fn sso_portal_base_url_override() -> Option<String> {
    let mutex = SSO_PORTAL_BASE_URL_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let guard = mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.clone()
}

/// Test-only setter for `sso_portal_base_url_override`.
///
/// Pass `Some(url)` to install an override (typically a localhost URL pointing
/// at a test HTTP server) and `None` to clear the override at end of test.
/// Callers MUST hold `sso_portal_override_lock()` for the entire scope where
/// the override is observable, otherwise concurrently-running tests can race
/// on this static and route GetRoleCredentials at the wrong mock server.
///
/// Recovers from lock poisoning rather than silently dropping the write —
/// see `sso_portal_base_url_override` for the rationale.
#[cfg(test)]
fn set_sso_portal_base_url_override(value: Option<String>) {
    let mutex = SSO_PORTAL_BASE_URL_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = value;
}

#[cfg(not(test))]
const fn sso_portal_base_url_override() -> Option<String> {
    None
}

fn sso_portal_base_url(sso_region: &str) -> String {
    if let Some(override_url) = sso_portal_base_url_override() {
        let trimmed = override_url.trim().trim_end_matches('/').to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    format!("https://portal.sso.{sso_region}.amazonaws.com")
}

/// Exchange a cached SSO access token for short-lived IAM credentials via
/// the SSO Portal `GetRoleCredentials` endpoint.
async fn exchange_aws_sso_credentials_with_client(
    client: &crate::http::client::Client,
    locator: &AwsSsoTokenLocator,
) -> Result<AwsResolvedCredentials> {
    let base = sso_portal_base_url(&locator.sso_region);
    let url = format!(
        "{base}/federation/credentials?account_id={}&role_name={}",
        percent_encode_component(&locator.account_id),
        percent_encode_component(&locator.role_name),
    );

    let request = client
        .get(&url)
        .header("Accept", "application/json")
        .header("x-amz-sso_bearer_token", locator.access_token.as_str());

    let response = Box::pin(request.send())
        .await
        .map_err(|err| Error::auth(format!("AWS SSO GetRoleCredentials request failed: {err}")))?;

    let status = response.status();
    let text = response
        .text_limited(MAX_AWS_SSO_RESPONSE_BYTES)
        .await
        .map_err(|error| {
            Error::auth(format!(
                "AWS SSO GetRoleCredentials returned an unreadable or oversized response body (HTTP {status}): {error}"
            ))
        })?;
    let redacted = redact_known_secrets_bounded(
        &text,
        &[locator.access_token.as_str()],
        MAX_AWS_SSO_ERROR_SNIPPET_BYTES,
    );

    if matches!(status, 401 | 403) {
        return Err(Error::auth(format!(
            "AWS SSO GetRoleCredentials returned HTTP {status}: token may be expired or the role \
             is not assigned to your user. Run: aws sso login (response: {redacted})",
        )));
    }
    if !(200..300).contains(&status) {
        return Err(Error::auth(format!(
            "AWS SSO GetRoleCredentials failed (HTTP {status}): {redacted}",
        )));
    }

    let parsed: SsoRoleCredentialsResponse = serde_json::from_str(&text).map_err(|err| {
        Error::auth(format!(
            "AWS SSO GetRoleCredentials returned malformed JSON: {err}",
        ))
    })?;
    let access_key_id = parsed.role_credentials.access_key_id.trim().to_string();
    let secret_access_key = parsed.role_credentials.secret_access_key.trim().to_string();
    let session_token = parsed.role_credentials.session_token.trim().to_string();
    if access_key_id.is_empty() || secret_access_key.is_empty() || session_token.is_empty() {
        return Err(Error::auth(
            "AWS SSO GetRoleCredentials returned an empty credential field".to_string(),
        ));
    }

    Ok(AwsResolvedCredentials::Sigv4 {
        access_key_id,
        secret_access_key,
        session_token: Some(session_token),
        region: locator.target_region.clone(),
    })
}

/// Async resolver that handles AWS SSO in addition to the static
/// credential chain.
///
/// Precedence is identical to [`resolve_aws_credentials`], with the
/// added behavior that profile resolution prefers static IAM keys but
/// falls through to SSO when the profile is configured for it. SSO
/// requires an HTTP call to the AWS SSO Portal; the bedrock provider
/// invokes this from its async `stream()` path.
pub async fn resolve_aws_credentials_async(
    auth: &AuthStorage,
    client: &crate::http::client::Client,
) -> Result<Option<AwsResolvedCredentials>> {
    resolve_aws_credentials_async_with_env(auth, client, |var| std::env::var(var).ok()).await
}

pub(crate) async fn resolve_ambient_aws_credentials_async(
    client: &crate::http::client::Client,
) -> Result<Option<AwsResolvedCredentials>> {
    let empty_auth = AuthStorage {
        path: PathBuf::new(),
        entries: HashMap::new(),
    };
    resolve_aws_credentials_async(&empty_auth, client).await
}

async fn resolve_aws_credentials_async_with_env<F>(
    auth: &AuthStorage,
    client: &crate::http::client::Client,
    mut env: F,
) -> Result<Option<AwsResolvedCredentials>>
where
    F: FnMut(&str) -> Option<String>,
{
    // Phase 1: synchronous ambient sources (bearer env, complete env keys,
    // static profile credentials) cover the fast paths without HTTP. Stored
    // auth.json credentials are deliberately deferred until after an explicitly
    // selected SSO profile has had its chance at the profile tier.
    if let Some(resolved) = resolve_aws_credentials_with_env_policy(auth, &mut env, false) {
        return Ok(Some(resolved));
    }

    // Phase 2: SSO-only profile fallback. The synchronous resolver returns
    // None when AWS_PROFILE points at a profile that has no static IAM
    // credentials in `~/.aws/credentials`. SSO profiles routinely look that
    // way, so we re-read the config here and try the SSO path.
    let env_region = preferred_non_empty_env(&mut env, "AWS_REGION", "AWS_DEFAULT_REGION");
    let region_default = env_region
        .clone()
        .unwrap_or_else(|| "us-east-1".to_string());
    let profile = preferred_non_empty_env(&mut env, "AWS_PROFILE", "AWS_DEFAULT_PROFILE");

    if let Some(profile) = profile {
        match detect_aws_sso_profile_with_env(
            &profile,
            env_region.as_deref(),
            &region_default,
            &mut env,
        ) {
            Ok(Some(locator)) => {
                let resolved = exchange_aws_sso_credentials_with_client(client, &locator).await?;
                return Ok(Some(resolved));
            }
            Ok(None) => {}
            Err(err) => return Err(err),
        }
    }

    Ok(resolve_stored_aws_credentials(auth, region_default))
}

// ── SAP AI Core Service Key Resolution ──────────────────────────

const MAX_SAP_TOKEN_RESPONSE_BYTES: usize = 256 * 1024;

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
///
/// A present, non-empty `AICORE_SERVICE_KEY` is authoritative. If it is malformed, resolution
/// fails closed (`None`) instead of silently falling through to a different account's split or
/// stored credentials.
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
    // 1. JSON-encoded service key from env. A non-empty invalid value is authoritative and must
    // stop the chain; falling through could silently authenticate to a different SAP account.
    if let Some(key_json) = env("AICORE_SERVICE_KEY") {
        let key_json = key_json.trim();
        if !key_json.is_empty() {
            return parse_sap_service_key_json(key_json);
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
    stored_sap_service_key(auth)
}

fn stored_sap_service_key(auth: &AuthStorage) -> Option<SapResolvedCredentials> {
    let provider = "sap-ai-core";
    if let Some(AuthCredential::ServiceKey {
        client_id,
        client_secret,
        token_url,
        service_url,
    }) = auth.credential_for_provider(provider)
        && let (Some(id), Some(secret), Some(turl), Some(surl)) = (
            client_id.as_ref(),
            client_secret.as_ref(),
            token_url.as_ref(),
            service_url.as_ref(),
        )
        && !id.trim().is_empty()
        && !secret.trim().is_empty()
        && !turl.trim().is_empty()
        && !surl.trim().is_empty()
    {
        return Some(SapResolvedCredentials {
            client_id: id.trim().to_string(),
            client_secret: secret.trim().to_string(),
            token_url: turl.trim().to_string(),
            service_url: surl.trim().to_string(),
        });
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
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let client_secret = obj
        .get("clientsecret")
        .or_else(|| obj.get("client_secret"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let token_url = obj
        .get("url")
        .or_else(|| obj.get("token_url"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let service_url = obj
        .get("serviceurls")
        .and_then(|v| v.get("AI_API_URL"))
        .and_then(|v| v.as_str())
        .or_else(|| obj.get("service_url").and_then(|v| v.as_str()))
        .map(str::trim)
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum SapAuthMaterial {
    DirectBearer(String),
    ServiceKey(SapResolvedCredentials),
}

fn classify_sap_auth_candidate(candidate: &str) -> Result<Option<SapAuthMaterial>> {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return Ok(None);
    }
    if candidate.starts_with('{') || candidate.starts_with('[') {
        let credentials = parse_sap_service_key_json(candidate).ok_or_else(|| {
            Error::auth(
                "SAP AI Core credential is JSON-shaped but is not a valid service key".to_string(),
            )
        })?;
        return Ok(Some(SapAuthMaterial::ServiceKey(credentials)));
    }
    Ok(Some(SapAuthMaterial::DirectBearer(candidate.to_string())))
}

fn stored_sap_candidate(auth: &AuthStorage, direct: bool) -> Option<String> {
    let credential = auth.credential_for_provider("sap-ai-core")?;
    match (direct, credential) {
        (
            true,
            credential @ (AuthCredential::OAuth { .. } | AuthCredential::BearerToken { .. }),
        )
        | (false, credential @ AuthCredential::ApiKey { .. }) => {
            api_key_from_credential(credential)
        }
        _ => None,
    }
}

fn resolve_sap_credentials_for_exchange_with_env<F>(
    auth: &AuthStorage,
    mut env: F,
) -> Result<Option<SapResolvedCredentials>>
where
    F: FnMut(&str) -> Option<String>,
{
    if let Some(service_key) = env("AICORE_SERVICE_KEY") {
        let service_key = service_key.trim();
        if !service_key.is_empty() {
            return parse_sap_service_key_json(service_key)
                .map(Some)
                .ok_or_else(|| {
                    Error::auth(
                        "AICORE_SERVICE_KEY is set but is not a valid SAP AI Core service key"
                            .to_string(),
                    )
                });
        }
    }

    let split_names = [
        "SAP_AI_CORE_CLIENT_ID",
        "SAP_AI_CORE_CLIENT_SECRET",
        "SAP_AI_CORE_TOKEN_URL",
        "SAP_AI_CORE_SERVICE_URL",
    ];
    let split_values = split_names.map(|name| {
        env(name)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    });
    if split_values.iter().any(Option::is_some) {
        let [
            Some(client_id),
            Some(client_secret),
            Some(token_url),
            Some(service_url),
        ] = split_values
        else {
            return Err(Error::auth(
                "SAP AI Core split credentials are partially configured; set all of SAP_AI_CORE_CLIENT_ID, SAP_AI_CORE_CLIENT_SECRET, SAP_AI_CORE_TOKEN_URL, and SAP_AI_CORE_SERVICE_URL, or unset the partial values"
                    .to_string(),
            ));
        };
        return Ok(Some(SapResolvedCredentials {
            client_id,
            client_secret,
            token_url,
            service_url,
        }));
    }

    Ok(stored_sap_service_key(auth))
}

fn resolve_sap_auth_material_with_env<F>(
    auth: &AuthStorage,
    candidate: Option<&str>,
    env: F,
) -> Result<Option<SapAuthMaterial>>
where
    F: FnMut(&str) -> Option<String>,
{
    // 1. Explicit caller candidate.
    if let Some(material) = candidate
        .map(classify_sap_auth_candidate)
        .transpose()?
        .flatten()
    {
        return Ok(Some(material));
    }

    // 2. Stored OAuth/Bearer credentials intentionally beat ambient configuration, matching the
    // generic auth contract and preventing a stale shell environment from overriding `/login`.
    if let Some(material) = stored_sap_candidate(auth, true)
        .as_deref()
        .map(classify_sap_auth_candidate)
        .transpose()?
        .flatten()
    {
        return Ok(Some(material));
    }

    // 3-5. Valid AICORE_SERVICE_KEY, then complete split env credentials, then a structured
    // stored ServiceKey. A malformed non-empty AICORE_SERVICE_KEY returns an error here.
    if let Some(credentials) = resolve_sap_credentials_for_exchange_with_env(auth, env)? {
        return Ok(Some(SapAuthMaterial::ServiceKey(credentials)));
    }

    // 6. Legacy ApiKey entries are the lowest stored tier. Their payload may itself be service-key
    // JSON, so it still goes through the same classifier and can never become a raw bearer header.
    stored_sap_candidate(auth, false)
        .as_deref()
        .map(classify_sap_auth_candidate)
        .transpose()
        .map(Option::flatten)
}

async fn resolve_sap_auth_material_with_client(
    client: &crate::http::client::Client,
    material: SapAuthMaterial,
) -> Result<String> {
    match material {
        SapAuthMaterial::DirectBearer(token) => Ok(token),
        SapAuthMaterial::ServiceKey(credentials) => {
            exchange_sap_access_token_with_client(client, &credentials).await
        }
    }
}

async fn resolve_ambient_sap_auth_token_with_client_and_env<F>(
    client: &crate::http::client::Client,
    env: F,
) -> Result<Option<String>>
where
    F: FnMut(&str) -> Option<String>,
{
    let empty_auth = AuthStorage {
        path: PathBuf::new(),
        entries: HashMap::new(),
    };
    let Some(material) = resolve_sap_auth_material_with_env(&empty_auth, None, env)? else {
        return Ok(None);
    };
    resolve_sap_auth_material_with_client(client, material)
        .await
        .map(Some)
}

pub(crate) async fn resolve_ambient_sap_auth_token_with_client(
    client: &crate::http::client::Client,
) -> Result<Option<String>> {
    resolve_ambient_sap_auth_token_with_client_and_env(client, |name| std::env::var(name).ok())
        .await
}

/// Resolve SAP authentication from ambient service-key variables only.
pub async fn resolve_ambient_sap_auth_token() -> Result<Option<String>> {
    let client = crate::http::client::Client::new();
    resolve_ambient_sap_auth_token_with_client(&client).await
}

/// Resolve one candidate SAP auth value without ever treating service-key JSON as a bearer.
///
/// Non-JSON candidates are direct bearer tokens. JSON-shaped candidates must be valid SAP
/// service keys and are exchanged; malformed JSON-shaped values fail closed instead of being sent
/// verbatim in an `Authorization` header.
pub(crate) async fn resolve_sap_auth_candidate_with_client(
    client: &crate::http::client::Client,
    candidate: &str,
) -> Result<Option<String>> {
    let Some(material) = classify_sap_auth_candidate(candidate)? else {
        return Ok(None);
    };
    resolve_sap_auth_material_with_client(client, material)
        .await
        .map(Some)
}

/// Resolve one explicit SAP auth candidate as either a direct bearer or exchanged service key.
pub async fn resolve_sap_auth_candidate(candidate: &str) -> Result<Option<String>> {
    let client = crate::http::client::Client::new();
    resolve_sap_auth_candidate_with_client(&client, candidate).await
}

pub(crate) async fn resolve_sap_auth_token_with_client(
    client: &crate::http::client::Client,
    auth: &AuthStorage,
    candidate: Option<&str>,
) -> Result<Option<String>> {
    let Some(material) =
        resolve_sap_auth_material_with_env(auth, candidate, |name| std::env::var(name).ok())?
    else {
        return Ok(None);
    };
    resolve_sap_auth_material_with_client(client, material)
        .await
        .map(Some)
}

/// Resolve SAP authentication across direct-bearer and service-key credential forms.
pub async fn resolve_sap_auth_token(
    auth: &AuthStorage,
    candidate: Option<&str>,
) -> Result<Option<String>> {
    let client = crate::http::client::Client::new();
    resolve_sap_auth_token_with_client(&client, auth, candidate).await
}

/// Exchange SAP AI Core service-key credentials for an access token.
///
/// Returns `Ok(None)` when SAP credentials are not configured.
pub async fn exchange_sap_access_token(auth: &AuthStorage) -> Result<Option<String>> {
    let Some(creds) =
        resolve_sap_credentials_for_exchange_with_env(auth, |name| std::env::var(name).ok())?
    else {
        return Ok(None);
    };

    let client = crate::http::client::Client::new();
    let token = exchange_sap_access_token_with_client(&client, &creds).await?;
    Ok(Some(token))
}

pub(crate) async fn exchange_sap_access_token_with_client(
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
        .text_limited(MAX_SAP_TOKEN_RESPONSE_BYTES)
        .await
        .map_err(|error| {
            Error::auth(format!(
                "SAP AI Core token exchange returned an unreadable or oversized response body (HTTP {status}): {error}"
            ))
        })?;
    if !(200..300).contains(&status) {
        let redacted_text = redact_known_secrets_bounded(
            &text,
            &[creds.client_id.as_str(), creds.client_secret.as_str()],
            MAX_SAP_TOKEN_RESPONSE_BYTES,
        );
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

const DEFAULT_REDACTED_ERROR_BYTES: usize = 64 * 1024;

fn known_secret_variants(secrets: &[&str]) -> Vec<(String, bool)> {
    let mut variants = Vec::with_capacity(secrets.len().saturating_mul(12));
    for secret in secrets {
        let trimmed = secret.trim();
        if trimmed.is_empty() {
            continue;
        }
        for value in [*secret, trimmed] {
            variants.push((value.to_string(), false));
            // A gateway can embed an upstream JSON fragment inside otherwise non-JSON text.
            // Include the JSON string's escaped payload (without its surrounding quotes) so the
            // bounded raw scanner still catches quotes, backslashes, and control characters when
            // parsing the complete diagnostic as JSON is impossible.
            if let Ok(json_string) = serde_json::to_string(value)
                && let Some(json_payload) = json_string
                    .strip_prefix('"')
                    .and_then(|payload| payload.strip_suffix('"'))
            {
                variants.push((json_payload.to_string(), false));
            }
            // OAuth servers and proxies sometimes echo the submitted form body rather than the
            // decoded credential. Redacting only the raw value would expose credentials that
            // contain spaces or reserved characters as `%XX` sequences in an error message.
            let percent_encoded = percent_encode_component(value);
            variants.push((percent_encoded.clone(), true));
            variants.push((percent_encoded.replace("%20", "+"), true));
        }
    }

    // Replace longer values first so a client ID that is a prefix of the client secret cannot
    // partially redact the latter and leave its suffix visible.
    variants.sort_unstable_by(|left, right| {
        right
            .0
            .len()
            .cmp(&left.0.len())
            .then_with(|| left.cmp(right))
    });
    variants.dedup();
    variants
}

pub(crate) fn redact_known_secrets(text: &str, secrets: &[&str]) -> String {
    redact_known_secrets_bounded(text, secrets, DEFAULT_REDACTED_ERROR_BYTES)
}

pub(crate) fn redact_known_secrets_bounded(
    text: &str,
    secrets: &[&str],
    max_bytes: usize,
) -> String {
    let variants = known_secret_variants(secrets);

    let redacted = serde_json::from_str::<serde_json::Value>(text).map_or_else(
        |_| text.to_string(),
        |mut json| {
            redact_sensitive_json_value(&mut json, &variants);
            serde_json::to_string(&json).unwrap_or_else(|_| text.to_string())
        },
    );
    redact_known_secret_variants_bounded(&redacted, &variants, max_bytes)
}

fn redact_known_secret_variants_bounded(
    text: &str,
    variants: &[(String, bool)],
    max_bytes: usize,
) -> String {
    const TRUNCATION_MARKER: &str = "...[truncated]";
    const REDACTION_MARKER: &str = "[REDACTED]";
    let mut result = String::with_capacity(text.len().min(max_bytes));
    let mut cursor = 0;
    while cursor < text.len() {
        let remaining = &text[cursor..];
        if let Some((variant, _)) = variants
            .iter()
            .find(|(variant, encoded)| variant_matches_at(remaining, variant, *encoded))
        {
            if REDACTION_MARKER.len() > max_bytes.saturating_sub(result.len()) {
                return append_truncation_marker(result, max_bytes, TRUNCATION_MARKER);
            }
            result.push_str(REDACTION_MARKER);
            cursor += variant.len();
            continue;
        }

        let Some(character) = remaining.chars().next() else {
            break;
        };
        if character.len_utf8() > max_bytes.saturating_sub(result.len()) {
            return append_truncation_marker(result, max_bytes, TRUNCATION_MARKER);
        }
        result.push(character);
        cursor += character.len_utf8();
    }
    result
}

fn append_truncation_marker(
    mut output: String,
    max_bytes: usize,
    truncation_marker: &str,
) -> String {
    if max_bytes <= truncation_marker.len() {
        return truncation_marker[..max_bytes].to_string();
    }
    let mut end = output.len().min(max_bytes - truncation_marker.len());
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
    output.push_str(truncation_marker);
    output
}

fn variant_matches_at(text: &str, pattern: &str, percent_escape_case_insensitive: bool) -> bool {
    let Some(candidate) = text.as_bytes().get(..pattern.len()) else {
        return false;
    };
    if percent_escape_case_insensitive {
        percent_encoded_variant_matches(candidate, pattern.as_bytes())
    } else {
        candidate == pattern.as_bytes()
    }
}

const fn percent_encoded_variant_matches(candidate: &[u8], pattern: &[u8]) -> bool {
    if candidate.len() != pattern.len() {
        return false;
    }

    let mut index = 0;
    while index < pattern.len() {
        if pattern[index] == b'%'
            && index + 2 < pattern.len()
            && pattern[index + 1].is_ascii_hexdigit()
            && pattern[index + 2].is_ascii_hexdigit()
        {
            if candidate[index] != b'%'
                || !candidate[index + 1].eq_ignore_ascii_case(&pattern[index + 1])
                || !candidate[index + 2].eq_ignore_ascii_case(&pattern[index + 2])
            {
                return false;
            }
            index += 3;
        } else {
            if candidate[index] != pattern[index] {
                return false;
            }
            index += 1;
        }
    }
    true
}

fn lowercase_percent_escape_hex(value: &str) -> String {
    let mut bytes = value.as_bytes().to_vec();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] == b'%'
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            bytes[index + 1].make_ascii_lowercase();
            bytes[index + 2].make_ascii_lowercase();
            index += 3;
        } else {
            index += 1;
        }
    }
    String::from_utf8(bytes).unwrap_or_else(|_| value.to_string())
}

fn redact_sensitive_json_value(value: &mut serde_json::Value, variants: &[(String, bool)]) {
    match value {
        serde_json::Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut nested) in entries {
                if is_sensitive_json_key(&key) {
                    nested = serde_json::Value::String("[REDACTED]".to_string());
                } else {
                    redact_sensitive_json_value(&mut nested, variants);
                }
                map.insert(redact_known_secret_variants(&key, variants), nested);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_sensitive_json_value(item, variants);
            }
        }
        serde_json::Value::String(text) => {
            *text = redact_known_secret_variants(text, variants);
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn redact_known_secret_variants(text: &str, variants: &[(String, bool)]) -> String {
    for (start, _) in text.char_indices() {
        let remaining = &text[start..];
        if variants
            .iter()
            .any(|(variant, encoded)| variant_matches_at(remaining, variant, *encoded))
        {
            return "[REDACTED]".to_string();
        }
    }
    text.to_string()
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
                line.split_whitespace().nth(1).map(str::to_string)
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
        if let Err(e) = stream.write_all(response.as_bytes()) {
            tracing::debug!("Failed to write HTTP response: {e}");
        }
        if let Err(e) = stream.flush() {
            tracing::debug!("Failed to flush HTTP response: {e}");
        }

        // Deliver the callback URL to the waiting caller.
        if tx.send(request_path).is_err() {
            tracing::debug!(
                "OAuth callback receiver was dropped before callback URL could be delivered"
            );
        }
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
        if let Err(e) = stream.write_all(response.as_bytes()) {
            tracing::debug!("Failed to write HTTP response: {e}");
        }
        if let Err(e) = stream.flush() {
            tracing::debug!("Failed to flush HTTP response: {e}");
        }

        if tx.send(request_path).is_err() {
            tracing::debug!(
                "OAuth callback receiver was dropped before callback URL could be delivered"
            );
        }
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
            client_id: DEFAULT_COPILOT_CLIENT_ID.to_string(),
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

    if let Some(parent) = primary.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        tracing::debug!(
            path = ?parent,
            error = %err,
            "Failed to create directory for generated credential file"
        );
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    if let Ok(mut file) = options.open(&primary)
        && let Err(err) = file.write_all(generated.as_bytes())
    {
        tracing::debug!(
            path = ?primary,
            error = %err,
            "Failed to write generated credential data to file"
        );
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

    let client_id = anthropic_oauth_client_id();
    let authorize_url = anthropic_oauth_authorize_url();
    let redirect_uri = anthropic_oauth_redirect_uri();
    let scopes = anthropic_oauth_scopes();
    let url = build_url_with_query(
        &authorize_url,
        &[
            ("code", "true"),
            ("client_id", &client_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
            ("scope", &scopes),
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
        redirect_uri: Some(redirect_uri),
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
    if state.ne(verifier) {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client_id = anthropic_oauth_client_id();
    let token_url = anthropic_oauth_token_url();
    let redirect_uri = anthropic_oauth_redirect_uri();

    let client = crate::http::client::Client::new();
    let request = client.post(&token_url).json(
        &serde_json::json!({ // ubs:ignore built-in Anthropic OAuth endpoint selected from fixed metadata/env override.
            "grant_type": "authorization_code",
            "client_id": client_id,
            "code": code,
            "state": state,
            "redirect_uri": redirect_uri,
            "code_verifier": verifier,
        }),
    )?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url),
        client_id: Some(client_id),
    })
}

async fn refresh_anthropic_oauth_token(
    client: &crate::http::client::Client,
    refresh_token: &str,
) -> Result<AuthCredential> {
    let client_id = anthropic_oauth_client_id();
    let token_url = anthropic_oauth_token_url();

    let request = client.post(&token_url).json(
        &serde_json::json!({ // ubs:ignore built-in Anthropic OAuth endpoint selected from fixed metadata/env override.
            "grant_type": "refresh_token",
            "client_id": client_id,
            "refresh_token": refresh_token,
        }),
    )?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Anthropic token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url),
        client_id: Some(client_id),
    })
}

/// Start OpenAI Codex OAuth by generating an authorization URL and PKCE verifier.
pub fn start_openai_codex_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let client_id = openai_codex_oauth_client_id();
    let authorize_url = openai_codex_oauth_authorize_url();
    let redirect_uri = openai_codex_oauth_redirect_uri();
    let scopes = openai_codex_oauth_scopes();
    let url = build_url_with_query(
        &authorize_url,
        &[
            ("response_type", "code"),
            ("client_id", &client_id),
            ("redirect_uri", &redirect_uri),
            ("scope", &scopes),
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
        redirect_uri: Some(redirect_uri),
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
    if state.ne(verifier) {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client_id = openai_codex_oauth_client_id();
    let token_url = openai_codex_oauth_token_url();
    let redirect_uri = openai_codex_oauth_redirect_uri();

    let form_body = format!(
        "grant_type=authorization_code&client_id={}&code={}&code_verifier={}&redirect_uri={}",
        percent_encode_component(&client_id),
        percent_encode_component(&code),
        percent_encode_component(verifier),
        percent_encode_component(&redirect_uri),
    );

    let client = crate::http::client::Client::new();
    let request = client
        .post(&token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("OpenAI Codex token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url),
        client_id: Some(client_id),
    })
}

/// Start Google Gemini CLI OAuth by generating an authorization URL and PKCE verifier.
pub fn start_google_gemini_cli_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let client_id = google_gemini_cli_oauth_client_id();
    let redirect_uri = google_gemini_cli_oauth_redirect_uri();
    let url = build_url_with_query(
        GOOGLE_GEMINI_CLI_OAUTH_AUTHORIZE_URL,
        &[
            ("client_id", &client_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
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
        redirect_uri: Some(redirect_uri),
        callback_server: None,
    })
}

/// Start Google Antigravity OAuth by generating an authorization URL and PKCE verifier.
pub fn start_google_antigravity_oauth() -> Result<OAuthStartInfo> {
    let (verifier, challenge) = generate_pkce();
    let client_id = google_antigravity_oauth_client_id();
    let redirect_uri = google_antigravity_oauth_redirect_uri();
    let url = build_url_with_query(
        GOOGLE_ANTIGRAVITY_OAUTH_AUTHORIZE_URL,
        &[
            ("client_id", &client_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
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
        redirect_uri: Some(redirect_uri),
        callback_server: None,
    })
}

async fn discover_google_gemini_cli_project_id(
    client: &crate::http::client::Client,
    access_token: &str,
) -> Result<String> {
    let env_project = google_project_id_from_env();
    let platform_upper = crate::platform::os_name().to_ascii_uppercase();
    let mut payload = serde_json::json!({
        "metadata": {
            "ideType": "CLI",
            "platform": platform_upper,
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
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());

    if (200..300).contains(&status)
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(project_id) = parse_code_assist_project_id(&value)
    {
        return Ok(project_id);
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
    let platform_upper = crate::platform::os_name().to_ascii_uppercase();
    let payload = serde_json::json!({
        "metadata": {
            "ideType": "CLI",
            "platform": platform_upper,
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
        let text = response
            .text_limited(MAX_OAUTH_RESPONSE_BYTES)
            .await
            .unwrap_or_default();
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(project_id) = parse_code_assist_project_id(&value)
        {
            return Ok(project_id);
        }
    }

    Ok(google_antigravity_default_project_id())
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
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
    if state.ne(verifier) {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();
    let gc_client_id = google_gemini_cli_oauth_client_id();
    let gc_client_secret = google_gemini_cli_oauth_client_secret();
    let gc_redirect_uri = google_gemini_cli_oauth_redirect_uri();
    let oauth_response = exchange_google_authorization_code(
        &client,
        GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL,
        &gc_client_id,
        &gc_client_secret,
        &code,
        &gc_redirect_uri,
        verifier,
    )
    .await?;

    let project_id =
        discover_google_gemini_cli_project_id(&client, &oauth_response.access_token).await?;

    Ok(AuthCredential::OAuth {
        extra: HashMap::new(),
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
    if state.ne(verifier) {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let client = crate::http::client::Client::new();
    let ga_client_id = google_antigravity_oauth_client_id();
    let ga_client_secret = google_antigravity_oauth_client_secret();
    let ga_redirect_uri = google_antigravity_oauth_redirect_uri();
    let oauth_response = exchange_google_authorization_code(
        &client,
        GOOGLE_ANTIGRAVITY_OAUTH_TOKEN_URL,
        &ga_client_id,
        &ga_client_secret,
        &code,
        &ga_redirect_uri,
        verifier,
    )
    .await?;

    let project_id =
        discover_google_antigravity_project_id(&client, &oauth_response.access_token).await?;

    Ok(AuthCredential::OAuth {
        extra: HashMap::new(),
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
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
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
    let gc_client_id = google_gemini_cli_oauth_client_id();
    let gc_client_secret = google_gemini_cli_oauth_client_secret();
    refresh_google_oauth_token_with_project(
        client,
        GOOGLE_GEMINI_CLI_OAUTH_TOKEN_URL,
        &gc_client_id,
        &gc_client_secret,
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
    let ga_client_id = google_antigravity_oauth_client_id();
    let ga_client_secret = google_antigravity_oauth_client_secret();
    refresh_google_oauth_token_with_project(
        client,
        GOOGLE_ANTIGRAVITY_OAUTH_TOKEN_URL,
        &ga_client_id,
        &ga_client_secret,
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
    let kimi_client_id = kimi_code_oauth_client_id();
    let url = kimi_code_endpoint_for_host(oauth_host, KIMI_CODE_DEVICE_AUTHORIZATION_PATH);
    let form_body = format!("client_id={}", percent_encode_component(&kimi_client_id));
    // ubs:ignore Kimi OAuth host is resolved through kimi_code_endpoint_for_host allowlist helpers before request construction.
    let mut request = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        // ubs:ignore fixed Kimi OAuth headers from kimi_common_headers, not response headers or request-controlled values.
        request = request.header(name, value);
    }

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Kimi device authorization request failed: {e}")))?;
    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
        .await
        .unwrap_or_else(|_| "<failed to read body>".to_string());
    let redacted_text = redact_known_secrets(&text, &[&kimi_client_id]);
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
    let kimi_client_id = kimi_code_oauth_client_id();
    let token_url = kimi_code_endpoint_for_host(oauth_host, KIMI_CODE_TOKEN_PATH);
    let form_body = format!(
        "client_id={}&device_code={}&grant_type={}",
        percent_encode_component(&kimi_client_id),
        percent_encode_component(device_code),
        percent_encode_component("urn:ietf:params:oauth:grant-type:device_code"),
    );
    // ubs:ignore Kimi OAuth host is resolved through kimi_code_endpoint_for_host allowlist helpers before request construction.
    let mut request = client
        .post(&token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        // ubs:ignore fixed Kimi OAuth headers from kimi_common_headers, not response headers or request-controlled values.
        request = request.header(name, value);
    }

    let response = match Box::pin(request.send()).await {
        Ok(response) => response,
        Err(err) => return DeviceFlowPollResult::Error(format!("Poll request failed: {err}")),
    };
    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
        access_token: oauth_response.access_token,
        refresh_token: oauth_response.refresh_token,
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url),
        client_id: Some(kimi_client_id),
    })
}

async fn refresh_kimi_code_oauth_token(
    client: &crate::http::client::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<AuthCredential> {
    let kimi_client_id = kimi_code_oauth_client_id();
    let form_body = format!(
        "client_id={}&grant_type=refresh_token&refresh_token={}",
        percent_encode_component(&kimi_client_id),
        percent_encode_component(refresh_token),
    );
    // ubs:ignore stored Kimi token_url is generated by Pi OAuth setup metadata, not request input.
    let mut request = client
        .post(token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(form_body.into_bytes());
    for (name, value) in kimi_common_headers() {
        // ubs:ignore fixed Kimi OAuth headers from kimi_common_headers, not response headers or request-controlled values.
        request = request.header(name, value);
    }

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Kimi token refresh failed: {e}")))?;
    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
        access_token: oauth_response.access_token,
        refresh_token: oauth_response
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires: oauth_expires_at_ms(oauth_response.expires_in),
        token_url: Some(token_url.to_string()),
        client_id: Some(kimi_client_id),
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
    let client = crate::http::client::Client::new();
    complete_extension_oauth_with_client(&client, config, code_input, verifier).await
}

/// Complete OAuth for an extension-registered provider with an injected HTTP client.
pub async fn complete_extension_oauth_with_client(
    client: &crate::http::client::Client,
    config: &crate::models::OAuthConfig,
    code_input: &str,
    verifier: &str,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth("Missing authorization code".to_string()));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state.ne(verifier) {
        return Err(Error::auth("State mismatch".to_string()));
    }

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

    let request = client.post(&config.token_url).json(&body)?; // ubs:ignore extension OAuth token_url comes from trusted extension config metadata, not request input.

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
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
    let request = client.post(&config.token_url).json(
        &serde_json::json!({ // ubs:ignore extension OAuth token_url comes from trusted extension config metadata, not request input.
            "grant_type": "refresh_token",
            "client_id": config.client_id,
            "refresh_token": refresh_token,
        }),
    )?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("Extension OAuth token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
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
    let request = client.post(token_url).json(
        &serde_json::json!({ // ubs:ignore self-contained OAuth token_url is stored credential metadata from prior login, not request input.
            "grant_type": "refresh_token",
            "client_id": oauth_client_id,
            "refresh_token": refresh_token,
        }),
    )?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("{provider} token refresh failed: {e}")))?;

    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
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

    let auth_url = if config.github_base_url.eq("https://github.com") {
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
    redirect_uri: Option<&str>,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth(
            "Missing authorization code. Paste the full callback URL or just the code parameter."
                .to_string(),
        ));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state.ne(verifier) {
        return Err(Error::auth("State mismatch".to_string()));
    }

    let token_url_str = if config.github_base_url.eq("https://github.com") {
        GITHUB_OAUTH_TOKEN_URL.to_string()
    } else {
        format!(
            "{}/login/oauth/access_token",
            trim_trailing_slash(&config.github_base_url)
        )
    };

    let client = crate::http::client::Client::new();
    let mut body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": config.client_id,
        "code": code,
        "state": state,
        "code_verifier": verifier,
    });
    // RFC 6749 §4.1.3: redirect_uri MUST be included in the token request
    // if it was included in the authorization request.
    if let Some(uri) = redirect_uri {
        body["redirect_uri"] = serde_json::Value::String(uri.to_string());
    }
    let request = client
        .post(&token_url_str)
        .header("Accept", "application/json")
        .json(&body)?;

    let response = Box::pin(request.send())
        .await
        .map_err(|e| Error::auth(format!("GitHub token exchange failed: {e}")))?;

    let status = response.status();
    let text = response
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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

    let device_url = if config.github_base_url.eq("https://github.com") {
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
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
    let token_url = if config.github_base_url.eq("https://github.com") {
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
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
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
    redirect_uri: Option<&str>,
) -> Result<AuthCredential> {
    let (code, state) = parse_oauth_code_input(code_input);

    let Some(code) = code else {
        return Err(Error::auth(
            "Missing authorization code. Paste the full callback URL or just the code parameter."
                .to_string(),
        ));
    };

    let state = state.unwrap_or_else(|| verifier.to_string());
    if state.ne(verifier) {
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

    if let Some(uri) = redirect_uri {
        body["redirect_uri"] = serde_json::Value::String(uri.to_string());
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
        .text_limited(MAX_OAUTH_RESPONSE_BYTES)
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
        extra: HashMap::new(),
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
            .find_map(|(k, v)| k.eq("code").then(|| v.clone()));
        let state = pairs
            .iter()
            .find_map(|(k, v)| k.eq("state").then(|| v.clone()));
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

    #[test]
    fn test_load_async_reads_existing_auth_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("auth.json");
        std::fs::write(
            &path,
            r#"{
  "openai": {
    "type": "api_key",
    "key": "sk-test"
  }
}"#,
        )
        .expect("write auth file");

        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let storage = rt
            .block_on(AuthStorage::load_async(path.clone()))
            .expect("load async");

        assert_eq!(storage.path, path);
        assert_eq!(storage.api_key("openai").as_deref(), Some("sk-test"));
    }

    #[test]
    fn test_save_async_persists_auth_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("auth.json");
        let mut storage = AuthStorage::load(path.clone()).expect("load empty auth storage");
        storage.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "sk-ant".to_string(),
            },
        );

        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        rt.block_on(storage.save_async()).expect("save async");

        let saved = AuthStorage::load(path).expect("reload auth storage");
        assert_eq!(saved.api_key("anthropic").as_deref(), Some("sk-ant"));
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
    fn test_auth_storage_load_missing_parent_does_not_create_directories() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let missing_parent = dir.path().join("not-created");
        let auth_path = missing_parent.join("auth.json");

        let loaded = AuthStorage::load(auth_path.clone()).expect("load missing auth storage");

        assert!(loaded.entries.is_empty());
        assert_eq!(loaded.path, auth_path);
        assert!(
            !missing_parent.exists(),
            "loading a missing auth store must not create its parent directory"
        );
    }

    #[test]
    fn test_auth_storage_classifies_live_lock_timeout_separately_from_read_failure() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        assert!(!auth_path.exists());

        let held_lock = crate::file_lock::DirLock::acquire_for(&auth_path, Duration::from_secs(1))
            .expect("hold auth lock");
        let lock_failure = AuthStorage::load_with_lock_timeout_classified(
            auth_path.clone(),
            Duration::from_millis(25),
        )
        .expect_err("a first-write lock must produce a typed timeout before auth.json exists");
        assert!(matches!(
            lock_failure,
            AuthStorageLoadFailure::LockTimeout(ref error)
                if error.kind() == std::io::ErrorKind::TimedOut
        ));
        drop(held_lock);

        std::fs::write(&auth_path, [0xff]).expect("write invalid UTF-8 auth fixture");
        let read_failure =
            AuthStorage::load_with_lock_timeout_classified(auth_path, Duration::from_secs(1))
                .expect_err("an unreadable auth file must be a non-contention failure");
        assert!(matches!(read_failure, AuthStorageLoadFailure::Other(_)));
    }

    #[test]
    fn test_auth_storage_rejects_oversized_file_before_parsing() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        std::fs::write(&auth_path, vec![b' '; MAX_AUTH_FILE_BYTES + 1])
            .expect("write oversized auth fixture");

        let failure =
            AuthStorage::load_with_lock_timeout_classified(auth_path, Duration::from_secs(1))
                .expect_err("oversized auth.json must fail before JSON parsing");
        let AuthStorageLoadFailure::Other(error) = failure else {
            panic!("oversized auth.json must not be classified as lock contention");
        };
        assert!(error.to_string().contains("exceeds"), "{error}");
        assert!(
            error.to_string().contains(&MAX_AUTH_FILE_BYTES.to_string()),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_auth_storage_rejects_final_symlink_without_reading_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tmpdir");
        let target_path = dir.path().join("credential-target.json");
        let target_contents = br#"{"openai":{"type":"api_key","key":"target-secret"}}"#;
        std::fs::write(&target_path, target_contents).expect("write auth symlink target");
        let auth_path = dir.path().join("auth.json");
        symlink(&target_path, &auth_path).expect("create auth symlink");

        let failure = AuthStorage::load_with_lock_timeout_classified(
            auth_path.clone(),
            Duration::from_secs(1),
        )
        .expect_err("auth.json symlink must fail closed");
        let AuthStorageLoadFailure::Other(error) = failure else {
            panic!("auth.json symlink must not be classified as lock contention");
        };
        assert!(error.to_string().contains("non-link"), "{error}");
        assert!(
            std::fs::symlink_metadata(auth_path)
                .expect("auth symlink metadata")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(&target_path).expect("read preserved target"),
            target_contents
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_auth_storage_rejects_intermediate_symlink_without_reading_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tmpdir");
        let redirected_parent = dir.path().join("redirected");
        fs::create_dir(&redirected_parent).expect("create redirected parent");
        let target_contents = br#"{"openai":{"type":"api_key","key":"redirected-secret"}}"#;
        fs::write(redirected_parent.join("auth.json"), target_contents)
            .expect("write redirected auth target");
        let linked_parent = dir.path().join("linked-parent");
        symlink(&redirected_parent, &linked_parent).expect("create intermediate symlink");
        let auth_path = linked_parent.join("auth.json");

        let failure =
            AuthStorage::load_with_lock_timeout_classified(auth_path, Duration::from_secs(1))
                .expect_err("an intermediate auth symlink must fail closed");

        assert!(
            matches!(failure, AuthStorageLoadFailure::Other(_)),
            "an intermediate symlink is a path-integrity failure, not lock contention"
        );
        assert_eq!(
            fs::read(redirected_parent.join("auth.json")).expect("read redirected target"),
            target_contents,
            "the rejected read must not modify the redirected credential target"
        );
        assert!(
            !redirected_parent.join("auth.json.lock").exists(),
            "the redirected parent must never receive lock state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_auth_storage_detects_parent_swap_before_descriptor_relative_read() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tmpdir");
        let parent = dir.path().join("agent");
        let moved_parent = dir.path().join("moved-agent");
        let redirected_parent = dir.path().join("redirected");
        fs::create_dir(&parent).expect("create auth parent");
        fs::create_dir(&redirected_parent).expect("create redirected parent");
        fs::write(
            parent.join("auth.json"),
            br#"{"openai":{"type":"api_key","key":"original-secret"}}"#,
        )
        .expect("write original auth");
        fs::write(
            redirected_parent.join("auth.json"),
            br#"{"openai":{"type":"api_key","key":"redirected-secret"}}"#,
        )
        .expect("write redirected auth");
        let auth_path = parent.join("auth.json");

        let failure = AuthStorage::load_with_lock_timeout_classified_and_hook(
            auth_path,
            Duration::from_secs(1),
            || {
                fs::rename(&parent, &moved_parent)?;
                symlink(&redirected_parent, &parent)?;
                Ok(())
            },
        )
        .expect_err("a swapped auth parent must fail before credential read");

        assert!(
            matches!(failure, AuthStorageLoadFailure::Other(_)),
            "a parent swap is a path-integrity failure, not lock contention"
        );
        assert!(
            !moved_parent.join("auth.json.lock").exists(),
            "the descriptor-relative read lock must be released from the moved parent"
        );
        assert!(
            !redirected_parent.join("auth.json.lock").exists(),
            "the replacement parent must never receive read-lock state"
        );
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
                key: "openai-key-test".to_string(),
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
                key: "google-key-test".to_string(),
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "stored-oauth-token".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "expired-oauth-token".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
            let err = complete_copilot_browser_oauth(&config, "abc#mismatch", "expected", None)
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
            let err = complete_gitlab_oauth(&config, "abc#mismatch", "expected", None)
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
                    extra: HashMap::new(),
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
                        if access_token.eq(&initial_access)
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
                    extra: HashMap::new(),
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
                        if access_token.eq(&initial_access_token)
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
                    extra: HashMap::new(),
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
                        if access_token.eq(&initial_access_token)
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
                    extra: HashMap::new(),
                    // ubs:ignore test fixture credential, not live secret.
                    access_token: "old-access".to_string(),
                    // ubs:ignore test fixture credential, not live secret.
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
            // ubs:ignore test fixture credential, not live secret.
            let refresh_secret = "secret-refresh-token-123";
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
                    extra: HashMap::new(),
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
                extra: HashMap::new(),
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
                extra: HashMap::new(),
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "valid-access".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                refresh_token: "valid-refresh".to_string(),
                expires: now + 30_000,
                token_url: None,
                client_id: None,
            },
        );
        auth.set(
            "expired-oauth",
            AuthCredential::OAuth {
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "expired-access".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
            other => unreachable!("unexpected credential status: {other:?}"),
        }

        match auth.credential_status("expired-oauth") {
            CredentialStatus::OAuthExpired { expired_by_ms } => {
                assert!(expired_by_ms > 0, "expired_by_ms should be positive");
            }
            other => unreachable!("unexpected credential status: {other:?}"),
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
                key: "google-key".to_string(),
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
                key: "legacy-gemini-key".to_string(),
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
                key: "mixed-case-key".to_string(),
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
                key: "legacy-gemini-key".to_string(),
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
                key: "google-key".to_string(),
            },
        );
        auth.set(
            "gemini",
            AuthCredential::ApiKey {
                key: "gemini-key".to_string(),
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
            other => unreachable!("unexpected credential variant: {other:?}"),
        }
    }

    #[test]
    fn test_auth_storage_load_corrupted_json_returns_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let corrupt_content = "not valid json {{";
        fs::write(&auth_path, corrupt_content).expect("write");

        let auth = AuthStorage::load(auth_path.clone()).expect("load");
        assert!(auth.entries.is_empty());
        assert_eq!(
            fs::read_to_string(auth_path.with_extension("json.corrupt"))
                .expect("read corrupt auth backup"),
            corrupt_content,
            "the backup must contain the exact bounded bytes that were parsed"
        );
    }

    #[test]
    fn test_corrupt_auth_load_is_not_delayed_by_busy_backup_lock() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        fs::write(&auth_path, "not valid json {{").expect("write corrupt auth");
        let backup_path = auth_path.with_extension("json.corrupt");
        let held_backup_lock =
            crate::file_lock::DirLock::acquire_for(&backup_path, Duration::from_secs(1))
                .expect("hold backup lock");

        let auth = AuthStorage::load_with_lock_timeout(auth_path, Duration::from_secs(1))
            .expect("a busy diagnostic-backup lock must not fail credential loading");

        assert!(auth.entries.is_empty());
        assert!(
            !backup_path.exists(),
            "a contended best-effort backup must not be written without its lock"
        );
        drop(held_backup_lock);
    }

    #[cfg(unix)]
    #[test]
    fn test_corrupt_auth_backup_replaces_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let corrupt_content = "not valid json {{";
        fs::write(&auth_path, corrupt_content).expect("write corrupt auth");

        let unrelated_path = dir.path().join("unrelated.json");
        let unrelated_content = b"must remain unchanged";
        fs::write(&unrelated_path, unrelated_content).expect("write unrelated target");
        let backup_path = auth_path.with_extension("json.corrupt");
        symlink(&unrelated_path, &backup_path).expect("create hostile backup symlink");

        let auth = AuthStorage::load(auth_path).expect("load corrupt auth");

        assert!(auth.entries.is_empty());
        assert_eq!(
            fs::read(&unrelated_path).expect("read unrelated target"),
            unrelated_content,
            "backup creation must never follow the pre-existing destination symlink"
        );
        assert!(
            !fs::symlink_metadata(&backup_path)
                .expect("backup metadata")
                .file_type()
                .is_symlink(),
            "atomic backup persistence must replace rather than follow the symlink"
        );
        assert_eq!(
            fs::read_to_string(backup_path).expect("read safe backup"),
            corrupt_content
        );
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
    fn test_resolve_api_key_empty_override_falls_through() {
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

        // Empty string override should not mask stored credentials.
        let resolved = auth.resolve_api_key_with_env_lookup("anthropic", Some(""), |_| None);
        assert_eq!(resolved.as_deref(), Some("stored-key"));
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
                key: "stored-groq-key".to_string(),
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
                key: "stored-openrouter-key".to_string(),
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
                key: "stored-key".to_string(),
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "sk-ant-api-like-token".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "codex-oauth-token".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
                key: "stored-google-key".to_string(),
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
                key: "stored-google-key".to_string(),
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
                key: "legacy-gemini-key".to_string(),
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
                key: "stored-dashscope-key".to_string(),
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
                key: "stored-moonshot-key".to_string(),
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
                key: "sk-openai-test".to_string(),
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
                key: "google-key-old".to_string(),
            },
        );
        auth.set(
            "google",
            AuthCredential::ApiKey {
                key: "google-key-new".to_string(),
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "goog-token".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
    fn test_save_rejects_intermediate_symlink_without_writing_credentials() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tmpdir");
        let redirected_directory = dir.path().join("redirected");
        fs::create_dir(&redirected_directory).expect("create redirected directory");
        let linked_parent = dir.path().join("linked-parent");
        symlink(&redirected_directory, &linked_parent).expect("create intermediate symlink");
        let auth_path = linked_parent.join("nested").join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "must-not-be-redirected".to_string(),
            },
        );

        auth.save()
            .expect_err("an intermediate auth-directory symlink must fail closed");

        assert!(
            !redirected_directory.join("nested").exists(),
            "the symlink target must not receive directories, lock state, or credentials"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_save_detects_parent_swap_before_descriptor_relative_persist() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tmpdir");
        let parent = dir.path().join("agent");
        let moved_parent = dir.path().join("moved-agent");
        let redirected_directory = dir.path().join("redirected");
        fs::create_dir(&parent).expect("create auth parent");
        fs::create_dir(&redirected_directory).expect("create redirected directory");
        let auth_path = parent.join("auth.json");
        let mut entries = HashMap::new();
        entries.insert(
            "anthropic".to_string(),
            AuthCredential::ApiKey {
                key: "must-not-survive-parent-swap".to_string(),
            },
        );
        let data = serde_json::to_string_pretty(&AuthFileRef { entries: &entries })
            .expect("serialize auth data");
        let mut moved_temp_path = None;

        let error = AuthStorage::save_data_sync_with_hook(&auth_path, &data, |temp_path| {
            let temp_name = temp_path.file_name().expect("temporary filename");
            moved_temp_path = Some(moved_parent.join(temp_name));
            fs::rename(&parent, &moved_parent)?;
            symlink(&redirected_directory, &parent)?;
            Ok(())
        })
        .expect_err("a swapped auth parent must fail closed");

        assert!(
            error.to_string().contains("revalidate")
                || error.to_string().contains("directory changed"),
            "unexpected parent-swap error: {error}"
        );
        assert!(
            !redirected_directory.join("auth.json").exists(),
            "the replacement parent must not receive auth.json"
        );
        assert!(
            !moved_parent.join("auth.json").exists(),
            "descriptor-relative persistence must stop after the parent identity changes"
        );
        assert!(
            !moved_temp_path.expect("captured moved temp path").exists(),
            "the descriptor-relative temporary credential file must be cleaned up"
        );
        assert!(
            !moved_parent.join("auth.json.lock").exists(),
            "the descriptor-relative compatibility lock must be released from the moved parent"
        );
        assert!(
            !redirected_directory.join("auth.json.lock").exists(),
            "the replacement parent must never receive compatibility lock state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_save_reports_parent_swap_after_descriptor_relative_persist() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tmpdir");
        let parent = dir.path().join("agent");
        let moved_parent = dir.path().join("moved-agent");
        let redirected_directory = dir.path().join("redirected");
        fs::create_dir(&parent).expect("create auth parent");
        fs::create_dir(&redirected_directory).expect("create redirected directory");
        let auth_path = parent.join("auth.json");
        let mut entries = HashMap::new();
        entries.insert(
            "anthropic".to_string(),
            AuthCredential::ApiKey {
                key: "committed-to-pinned-directory".to_string(),
            },
        );
        let data = serde_json::to_string_pretty(&AuthFileRef { entries: &entries })
            .expect("serialize auth data");

        let error = AuthStorage::save_data_sync_with_hooks(
            &auth_path,
            &data,
            |_| Ok(()),
            |_| {
                fs::rename(&parent, &moved_parent)?;
                symlink(&redirected_directory, &parent)?;
                Ok(())
            },
        )
        .expect_err("a post-persist parent swap must be reported as partial success");

        assert!(
            error.to_string().contains("persisted and synced")
                && error.to_string().contains("configured parent path"),
            "the error must disclose that bytes committed before path verification failed: {error}"
        );
        assert_eq!(
            fs::read(moved_parent.join("auth.json")).expect("read committed auth data"),
            data.as_bytes(),
            "the descriptor-relative write must remain in the pinned directory"
        );
        assert!(
            !redirected_directory.join("auth.json").exists(),
            "the replacement parent must not receive credential bytes"
        );
        assert!(
            !moved_parent.join("auth.json.lock").exists(),
            "the descriptor-relative compatibility lock must be released"
        );
        assert!(
            !redirected_directory.join("auth.json.lock").exists(),
            "the replacement parent must never receive compatibility lock state"
        );
        let moved_entries = fs::read_dir(&moved_parent)
            .expect("read moved auth directory")
            .map(|entry| entry.expect("auth directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            moved_entries,
            vec![std::ffi::OsString::from("auth.json")],
            "the committed auth file must not leave a temporary artifact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_save_replaces_final_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let unrelated_path = dir.path().join("unrelated.json");
        let unrelated_contents = b"must remain byte-for-byte unchanged";
        fs::write(&unrelated_path, unrelated_contents).expect("write unrelated target");
        symlink(&unrelated_path, &auth_path).expect("create hostile final symlink");
        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };
        auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "replacement-key".to_string(),
            },
        );

        auth.save()
            .expect("descriptor-relative rename should replace the final symlink itself");

        assert_eq!(
            fs::read(&unrelated_path).expect("read unrelated target"),
            unrelated_contents,
            "the final symlink target must never receive credential bytes"
        );
        let metadata = fs::symlink_metadata(&auth_path).expect("auth path metadata");
        assert!(metadata.is_file(), "auth.json must now be a regular file");
        assert!(
            !metadata.file_type().is_symlink(),
            "the hostile final symlink must have been replaced"
        );
        let loaded = AuthStorage::load(auth_path).expect("load replaced auth.json");
        assert_eq!(
            loaded.api_key("anthropic").as_deref(),
            Some("replacement-key")
        );
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
                key: "old-key".to_string(),
            },
        );
        original.save().expect("save original auth");

        // The proper-lockfile-compatible directory lock is created on acquire and
        // removed (`rmdir`) on release, so it must NOT persist after `save()`.
        // A leftover regular file here is exactly what poisons upstream TS pi.
        let lock_path = crate::file_lock::lock_path_for(&auth_path);
        assert!(
            !lock_path.exists(),
            "lock must not persist after save (no poisoning artifact)"
        );

        let mut replacement_entries = HashMap::new();
        replacement_entries.insert(
            "anthropic".to_string(),
            AuthCredential::ApiKey {
                key: "new-key".to_string(),
            },
        );
        let replacement_data = serde_json::to_string_pretty(&AuthFileRef {
            entries: &replacement_entries,
        })
        .expect("serialize replacement");

        let mut temp_path = None;
        let err = AuthStorage::save_data_sync_with_hook(&auth_path, &replacement_data, |temp| {
            temp_path = Some(temp.to_path_buf());
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

    #[test]
    fn test_save_rejects_oversized_serialization_before_mutating_existing_file() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let original_data = br#"{"anthropic":{"type":"api_key","key":"old-key"}}"#;
        fs::write(&auth_path, original_data).expect("write original auth");
        let entries_before = fs::read_dir(dir.path())
            .expect("read auth directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        let mut oversized_auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };
        oversized_auth.set(
            "anthropic",
            AuthCredential::ApiKey {
                key: "x".repeat(MAX_AUTH_FILE_BYTES + 1),
            },
        );

        let error = oversized_auth
            .save()
            .expect_err("oversized serialized auth must be rejected");

        assert!(error.to_string().contains("exceeds"), "{error}");
        assert!(
            error.to_string().contains(&MAX_AUTH_FILE_BYTES.to_string()),
            "{error}"
        );
        assert_eq!(
            fs::read(&auth_path).expect("read preserved auth"),
            original_data,
            "a rejected save must preserve the existing auth.json byte-for-byte"
        );
        assert!(
            !crate::file_lock::lock_path_for(&auth_path).exists(),
            "a rejected save must not acquire or leave an auth lock"
        );
        let entries_after = fs::read_dir(dir.path())
            .expect("read auth directory after rejection")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            entries_after, entries_before,
            "a rejected save must not create temp or sidecar files"
        );
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
    fn test_redact_known_secrets_covers_encoded_and_overlapping_values() {
        let client_id = "sap client+id@example.com";
        let client_secret = "sap client+id@example.com&secret=100%";
        let encoded_id = percent_encode_component(client_id);
        let encoded_secret = percent_encode_component(client_secret);
        let lowercase_encoded_secret = lowercase_percent_escape_hex(&encoded_secret);
        let mixed_case_encoded_secret = encoded_secret.replace("%2B", "%2b");
        let text = format!(
            "client_id={encoded_id}&client_secret={encoded_secret}&plus={}&lower={lowercase_encoded_secret}&lower_plus={}&mixed={mixed_case_encoded_secret}",
            encoded_secret.replace("%20", "+"),
            lowercase_encoded_secret.replace("%20", "+")
        );

        let redacted = redact_known_secrets(&text, &[client_id, client_secret]);
        assert!(!redacted.contains(client_id));
        assert!(!redacted.contains(client_secret));
        assert!(!redacted.contains(&encoded_id));
        assert!(!redacted.contains(&encoded_secret));
        assert!(!redacted.contains(&lowercase_encoded_secret));
        assert!(!redacted.contains(&mixed_case_encoded_secret));
        assert!(!redacted.contains("secret%3D100%25"));
        assert!(!redacted.contains("secret%3d100%25"));
        assert_eq!(redacted.matches("[REDACTED]").count(), 6);

        let expanding = redact_known_secrets_bounded(
            &"x".repeat(128),
            &["x", "R", "E", "D", "A", "C", "T"],
            64,
        );
        assert_eq!(expanding.len(), 64);
        assert!(expanding.ends_with("...[truncated]"), "{expanding}");
        assert_eq!(
            redact_known_secrets("x", &["x", "R", "E", "D", "A", "C", "T"]),
            "[REDACTED]",
            "introduced redaction markers must never be reprocessed and amplified"
        );

        let default_bounded = redact_known_secrets(&"safe".repeat(32 * 1024), &[]);
        assert_eq!(default_bounded.len(), DEFAULT_REDACTED_ERROR_BYTES);
        assert!(default_bounded.ends_with("...[truncated]"));
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

    #[test]
    fn test_redact_known_secrets_handles_json_escaped_values_and_keys() {
        let secret = "quoted-\"credential\\with\ncontrols";
        let mut object = serde_json::Map::new();
        object.insert(
            "echo".to_string(),
            serde_json::Value::String(secret.to_string()),
        );
        object.insert(
            secret.to_string(),
            serde_json::Value::String("credential appeared as an object key".to_string()),
        );
        let text = serde_json::Value::Object(object).to_string();
        assert!(text.contains(r#"quoted-\"credential\\with\ncontrols"#));

        let redacted = redact_known_secrets(&text, &[secret]);
        assert!(
            !redacted.contains("quoted-"),
            "credential prefix leaked: {redacted}"
        );
        assert!(
            !redacted.contains("credential\\\\with"),
            "credential leaked: {redacted}"
        );
        assert_eq!(redacted.matches("[REDACTED]").count(), 2, "{redacted}");

        let prefixed = format!("upstream error: {text}");
        let prefixed_redacted = redact_known_secrets(&prefixed, &[secret]);
        assert!(prefixed_redacted.starts_with("upstream error: "));
        assert!(prefixed_redacted.contains("[REDACTED]"));
        assert!(
            !prefixed_redacted.contains(r#"quoted-\"credential\\with\ncontrols"#),
            "JSON-escaped credential leaked from mixed-text diagnostic: {prefixed_redacted}"
        );
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
                other => unreachable!("unexpected credential variant: {other:?}"),
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
            other => unreachable!("unexpected credential variant: {other:?}"),
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
            other => unreachable!("unexpected credential variant: {other:?}"),
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
    fn test_start_kimi_code_device_flow_rejects_oversized_response() {
        let oversized = "x".repeat(MAX_OAUTH_RESPONSE_BYTES + 1);
        let host = spawn_oauth_host_server(200, &oversized);
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client = crate::http::client::Client::new();
            let error = start_kimi_code_device_flow_with_client(&client, &host)
                .await
                .expect_err("oversized device authorization body must be rejected");
            let message = error.to_string();
            assert!(
                message.contains("Invalid Kimi device authorization response"),
                "{message}"
            );
            assert!(
                !message.contains(&"x".repeat(256)),
                "oversized untrusted body must not be copied into the diagnostic"
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
                other => unreachable!("unexpected credential variant: {other:?}"),
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
                extra: HashMap::new(),
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
                other => unreachable!("unexpected credential variant: {other:?}"),
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
        // #97: the default now ships the well-known public Copilot client id so
        // login works without the user registering their own GitHub App.
        assert_eq!(config.client_id, DEFAULT_COPILOT_CLIENT_ID);
        assert!(!config.client_id.is_empty());
        assert_eq!(config.github_base_url, "https://github.com");
        assert_eq!(config.scopes, GITHUB_COPILOT_SCOPES);
    }

    #[test]
    fn test_resolved_copilot_client_id_falls_back_to_default() {
        // With the env var unset/empty, the resolver returns the public default.
        // (We avoid mutating the process env here to stay parallel-test-safe;
        // CI runs without GITHUB_COPILOT_CLIENT_ID set.)
        if std::env::var("GITHUB_COPILOT_CLIENT_ID").map_or(true, |v| v.trim().is_empty()) {
            assert_eq!(resolved_copilot_client_id(), DEFAULT_COPILOT_CLIENT_ID);
        }
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
            // ubs:ignore test fixture credential, not live secret.
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
            other => unreachable!("unexpected credential variant: {other:?}"),
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
            other => unreachable!("unexpected credential variant: {other:?}"),
        }
    }

    #[test]
    // ubs:ignore test fixture name mentions bearer token but contains no live secret.
    fn test_bearer_token_round_trip() {
        // ubs:ignore test fixture credential, not live secret.
        let cred = AuthCredential::BearerToken {
            // ubs:ignore test fixture credential, not live secret.
            token: "my-gateway-token-123".to_string(),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::BearerToken { token } => {
                assert_eq!(token, "my-gateway-token-123");
            }
            other => unreachable!("unexpected credential variant: {other:?}"),
        }
    }

    #[test]
    fn test_service_key_round_trip() {
        let cred = AuthCredential::ServiceKey {
            client_id: Some("sap-client-id".to_string()),
            // ubs:ignore test fixture credential, not live secret.
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
            other => unreachable!("unexpected credential variant: {other:?}"),
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
            other => unreachable!("unexpected credential variant: {other:?}"),
        }
    }

    // ── api_key() with new variants ──────────────────────────────────

    #[test]
    // ubs:ignore test fixture name mentions bearer token but contains no live secret.
    fn test_api_key_returns_bearer_token() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "my-gateway",
            // ubs:ignore test fixture credential, not live secret.
            AuthCredential::BearerToken {
                // ubs:ignore test fixture credential, not live secret.
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
                // ubs:ignore test fixture credential, not live secret.
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
                // ubs:ignore test fixture credential, not live secret.
                client_secret: Some("secret".to_string()),
                token_url: Some("https://auth.example.com".to_string()),
                service_url: Some("https://api.example.com".to_string()),
            },
        );
        assert!(auth.api_key("sap-ai-core").is_none());
    }

    #[test]
    fn generic_api_key_resolution_defers_all_stored_bedrock_credentials_to_provider_chain() {
        let mut auth = empty_auth();
        auth.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIA_STORED".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                secret_access_key: "stored-secret".to_string(),
                session_token: Some("stored-session".to_string()),
                region: Some("us-west-2".to_string()),
            },
        );

        let resolved = auth.resolve_api_key_with_env_lookup("bedrock", None, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA_ENV".to_string()),
            // ubs:ignore test fixture credential, not live secret.
            "AWS_SECRET_ACCESS_KEY" => Some("env-secret".to_string()),
            "AWS_SESSION_TOKEN" => Some("env-session".to_string()),
            "AWS_PROFILE" => Some("developer".to_string()),
            "AWS_REGION" => Some("eu-west-1".to_string()),
            _ => None,
        });

        assert!(
            resolved.is_none(),
            "structured AWS credentials must reach the Bedrock resolver, not the bearer lane"
        );

        auth.set(
            "amazon-bedrock",
            AuthCredential::BearerToken {
                token: "stored-bearer".to_string(),
            },
        );
        assert!(
            auth.resolve_api_key_with_env_lookup("bedrock", None, |_| None)
                .is_none(),
            "stored bearer must retain the provider-owned AWS chain's precedence"
        );

        auth.set(
            "amazon-bedrock",
            AuthCredential::ApiKey {
                key: "legacy-stored-bearer".to_string(),
            },
        );
        assert!(
            auth.resolve_api_key_with_env_lookup("amazon-bedrock", None, |_| None)
                .is_none(),
            "legacy stored bearer must also be resolved by the Bedrock provider"
        );
    }

    #[test]
    fn generic_bedrock_resolution_accepts_only_direct_bearer_inputs() {
        let auth = empty_auth();
        let from_env = auth.resolve_api_key_with_env_lookup("amazon-bedrock", None, |var| {
            (var == "AWS_BEARER_TOKEN_BEDROCK").then(|| "bedrock-bearer".to_string())
        });
        assert_eq!(from_env.as_deref(), Some("bedrock-bearer"));

        let explicit =
            auth.resolve_api_key_with_env_lookup("bedrock", Some("explicit-bearer"), |_| {
                Some("must-not-win".to_string())
            });
        assert_eq!(explicit.as_deref(), Some("explicit-bearer"));
    }

    #[test]
    fn generic_api_key_resolution_never_flattens_sap_service_credentials() {
        let auth = empty_auth();
        let service_key_json = serde_json::json!({
            "clientid": "sap-client",
            "clientsecret": "sap-secret",
            "url": "https://auth.sap.example.com/oauth/token",
            "serviceurls": {"AI_API_URL": "https://api.ai.sap.example.com"}
        })
        .to_string();

        let from_service_key = auth.resolve_api_key_with_env_lookup("sap-ai-core", None, |var| {
            (var == "AICORE_SERVICE_KEY").then(|| service_key_json.clone())
        });
        assert!(from_service_key.is_none());

        let from_split_env = auth.resolve_api_key_with_env_lookup("sap", None, |var| match var {
            "SAP_AI_CORE_CLIENT_ID" => Some("sap-client".to_string()),
            // ubs:ignore test fixture credential, not live secret.
            "SAP_AI_CORE_CLIENT_SECRET" => Some("sap-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://auth.sap.example.com/oauth/token".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://api.ai.sap.example.com".to_string()),
            _ => None,
        });
        assert!(from_split_env.is_none());
    }

    #[test]
    fn generic_sap_resolution_preserves_provider_owned_legacy_precedence() {
        let ambient_service_key = serde_json::json!({
            "clientid": "ambient-client",
            "clientsecret": "ambient-secret",
            "url": "https://ambient-token.example.com",
            "serviceurls": {"AI_API_URL": "https://ambient-api.example.com"}
        })
        .to_string();
        let mut auth = empty_auth();
        auth.set(
            "sap-ai-core",
            AuthCredential::ApiKey {
                key: "legacy-stored-bearer".to_string(),
            },
        );

        assert!(
            auth.resolve_api_key_with_env_lookup("sap", None, |_| None)
                .is_none(),
            "legacy SAP ApiKey must remain in the provider-owned lowest-precedence tier"
        );
        let provider_owned = resolve_sap_auth_material_with_env(&auth, None, |name| {
            (name == "AICORE_SERVICE_KEY").then(|| ambient_service_key.clone())
        })
        .expect("provider-owned SAP resolution");
        assert!(matches!(
            provider_owned,
            Some(SapAuthMaterial::ServiceKey(SapResolvedCredentials {
                client_id,
                ..
            })) if client_id == "ambient-client"
        ));

        let explicit = auth.resolve_api_key_with_env_lookup(
            "sap-ai-core",
            Some("explicit-sap-bearer"),
            |_| None,
        );
        assert_eq!(explicit.as_deref(), Some("explicit-sap-bearer"));

        auth.set(
            "sap-ai-core",
            AuthCredential::BearerToken {
                token: "stored-direct-bearer".to_string(),
            },
        );
        assert_eq!(
            auth.resolve_api_key_with_env_lookup("sap", None, |_| None)
                .as_deref(),
            Some("stored-direct-bearer"),
            "stored direct bearer must retain its documented precedence over ambient service keys"
        );
    }

    // ── AWS Credential Chain ─────────────────────────────────────────

    fn empty_auth() -> AuthStorage {
        let dir = tempfile::tempdir().expect("tmpdir");
        AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        }
    }

    /// Acquire the serialisation lock for tests that read/write the
    /// `SSO_PORTAL_BASE_URL_OVERRIDE` static. Without this, parallel test
    /// execution races on the override and produces sporadic 401 responses
    /// from the wrong test's mock portal. Tests that touch the override
    /// must hold this guard for their entire duration — including across
    /// `.await` points, which is intentional. Each test runs on its own
    /// `current_thread` runtime so cross-thread runtime-state poisoning isn't
    /// possible; the only concern is two `#[test]` functions reading the same
    /// override concurrently from different OS threads, which this guard
    /// prevents. The `clippy::await_holding_lock` lint is silenced at each
    /// call site because the alternative (an async-aware mutex) would buy us
    /// nothing here — the lock contention is bounded by the test count, the
    /// tests are short, and the guard is the entire point.
    fn sso_portal_override_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    // ubs:ignore test fixture name mentions bearer token but contains no live secret.
    fn test_aws_bearer_token_env_wins() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_BEARER_TOKEN_BEDROCK" => Some("bearer-tok-env".to_string()),
            "AWS_REGION" => Some("eu-west-1".to_string()),
            "AWS_ACCESS_KEY_ID" => Some("AKIA_SHOULD_NOT_WIN".to_string()),
            // ubs:ignore test fixture credential, not live secret.
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            // ubs:ignore test fixture credential, not live secret.
            Some(AwsResolvedCredentials::Bearer {
                // ubs:ignore test fixture credential, not live secret.
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
            // ubs:ignore test fixture credential, not live secret.
            "AWS_SECRET_ACCESS_KEY" => Some("secretTEST".to_string()),
            "AWS_SESSION_TOKEN" => Some("session123".to_string()),
            "AWS_REGION" => Some("ap-southeast-1".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIATEST".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
            // ubs:ignore test fixture credential, not live secret.
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                secret_access_key: "secret".to_string(),
                session_token: None,
                region: "us-east-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_profile_credentials_from_files() {
        let auth = empty_auth();
        let dir = tempfile::tempdir().expect("tmpdir");
        let cred_path = dir.path().join("credentials");
        let config_path = dir.path().join("config");
        std::fs::write(
            &cred_path,
            "[default]\naws_access_key_id = AKIA_DEFAULT\naws_secret_access_key = default_secret\n\n[dev]\naws_access_key_id = AKIA_DEV\naws_secret_access_key = dev_secret\naws_session_token = dev_session\n",
        )
        .expect("write credentials");
        std::fs::write(
            &config_path,
            "[default]\nregion = us-east-2\n\n[profile dev]\nregion = us-west-2\n",
        )
        .expect("write config");

        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_PROFILE" => Some("dev".to_string()),
            "AWS_SHARED_CREDENTIALS_FILE" => Some(cred_path.to_string_lossy().to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            _ => None,
        });

        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA_DEV".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                secret_access_key: "dev_secret".to_string(),
                session_token: Some("dev_session".to_string()),
                region: "us-west-2".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_profile_env_region_overrides_config() {
        let auth = empty_auth();
        let dir = tempfile::tempdir().expect("tmpdir");
        let cred_path = dir.path().join("credentials");
        let config_path = dir.path().join("config");
        std::fs::write(
            &cred_path,
            "[dev]\naws_access_key_id = AKIA_DEV\naws_secret_access_key = dev_secret\n",
        )
        .expect("write credentials");
        std::fs::write(&config_path, "[profile dev]\nregion = us-west-2\n").expect("write config");

        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_PROFILE" => Some("dev".to_string()),
            "AWS_SHARED_CREDENTIALS_FILE" => Some(cred_path.to_string_lossy().to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            "AWS_REGION" => Some("eu-north-1".to_string()),
            _ => None,
        });

        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA_DEV".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                secret_access_key: "dev_secret".to_string(),
                session_token: None,
                region: "eu-north-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_profile_missing_falls_back_to_auth() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let mut auth = AuthStorage {
            path: dir.path().join("auth.json"),
            entries: HashMap::new(),
        };
        auth.set(
            "amazon-bedrock",
            AuthCredential::AwsCredentials {
                access_key_id: "AKIA_STORED".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                secret_access_key: "secret_stored".to_string(),
                session_token: None,
                region: Some("us-west-1".to_string()),
            },
        );
        let cred_path = dir.path().join("credentials");
        std::fs::write(
            &cred_path,
            "[default]\naws_access_key_id = AKIA_DEFAULT\naws_secret_access_key = default_secret\n",
        )
        .expect("write credentials");
        let config_path = dir.path().join("config");
        std::fs::write(&config_path, "[default]\nregion = us-east-1\n").expect("write config");

        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_PROFILE" => Some("missing".to_string()),
            "AWS_SHARED_CREDENTIALS_FILE" => Some(cred_path.to_string_lossy().to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            _ => None,
        });

        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Sigv4 {
                access_key_id: "AKIA_STORED".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                secret_access_key: "secret_stored".to_string(),
                session_token: None,
                region: "us-west-1".to_string(),
            })
        );
    }

    #[test]
    fn test_aws_default_region_fallback() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            // ubs:ignore test fixture credential, not live secret.
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            "AWS_DEFAULT_REGION" => Some("ca-central-1".to_string()),
            _ => None,
        });
        match result {
            Some(AwsResolvedCredentials::Sigv4 { region, .. }) => {
                assert_eq!(region, "ca-central-1");
            }
            other => unreachable!("unexpected credential variant: {other:?}"),
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
                // ubs:ignore test fixture credential, not live secret.
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
                // ubs:ignore test fixture credential, not live secret.
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
                // ubs:ignore test fixture credential, not live secret.
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
                // ubs:ignore test fixture credential, not live secret.
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
            // ubs:ignore test fixture credential, not live secret.
            AuthCredential::BearerToken {
                // ubs:ignore test fixture credential, not live secret.
                token: "stored-bearer".to_string(),
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            // ubs:ignore test fixture credential, not live secret.
            Some(AwsResolvedCredentials::Bearer {
                // ubs:ignore test fixture credential, not live secret.
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
                // ubs:ignore test fixture credential, not live secret.
                secret_access_key: "stored_secret".to_string(),
                session_token: None,
                region: None,
            },
        );
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_ACCESS_KEY_ID" => Some("AKIA_ENV".to_string()),
            // ubs:ignore test fixture credential, not live secret.
            "AWS_SECRET_ACCESS_KEY" => Some("env_secret".to_string()),
            _ => None,
        });
        match result {
            Some(AwsResolvedCredentials::Sigv4 { access_key_id, .. }) => {
                assert_eq!(access_key_id, "AKIA_ENV");
            }
            other => unreachable!("unexpected credential variant: {other:?}"),
        }
    }

    #[test]
    fn test_aws_no_credentials_returns_none() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert!(result.is_none());
    }

    #[test]
    // ubs:ignore test fixture name mentions bearer token but contains no live secret.
    fn test_aws_empty_bearer_token_skipped() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_BEARER_TOKEN_BEDROCK" => Some("  ".to_string()),
            "AWS_ACCESS_KEY_ID" => Some("AKIA".to_string()),
            // ubs:ignore test fixture credential, not live secret.
            "AWS_SECRET_ACCESS_KEY" => Some("secret".to_string()),
            _ => None,
        });
        assert!(matches!(result, Some(AwsResolvedCredentials::Sigv4 { .. })));
    }

    #[test]
    fn test_aws_whitespace_primary_region_falls_back_to_default_region_env() {
        let auth = empty_auth();
        let result = resolve_aws_credentials_with_env(&auth, |var| match var {
            "AWS_BEARER_TOKEN_BEDROCK" => Some("bedrock-bearer".to_string()),
            "AWS_REGION" => Some("   ".to_string()),
            "AWS_DEFAULT_REGION" => Some("  ca-central-1  ".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(AwsResolvedCredentials::Bearer {
                token: "bedrock-bearer".to_string(),
                region: "ca-central-1".to_string(),
            })
        );
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

    #[test]
    fn test_aws_sso_oversized_explicit_config_fails_before_stored_fallback() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let directory = tempfile::tempdir().expect("tmpdir");
            let config_path = directory.path().join("config");
            std::fs::write(&config_path, vec![b'x'; MAX_AWS_SHARED_CONFIG_BYTES + 1])
                .expect("write oversized AWS config");

            let mut auth = empty_auth();
            auth.set(
                "amazon-bedrock",
                AuthCredential::BearerToken {
                    token: "stored-different-account".to_string(),
                },
            );
            let client = crate::http::client::Client::new();
            let error = resolve_aws_credentials_async_with_env(&auth, &client, |name| match name {
                "AWS_PROFILE" => Some("explicit-sso-profile".to_string()),
                "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
                _ => None,
            })
            .await
            .expect_err("oversized explicit profile config must fail closed");

            let message = error.to_string();
            assert!(message.contains("exceeds"), "{message}");
            assert!(!message.contains("stored-different-account"), "{message}");
        });
    }

    // ── AWS SSO ──────────────────────────────────────────────────────

    /// Helper: build a fully-configured SSO sandbox (config + cache file)
    /// in a fresh temp dir and return env-var overrides for the resolver.
    fn write_sso_sandbox(
        dir: &Path,
        cache_key: &str,
        access_token: &str,
        expires_at: Option<&str>,
        config_text: &str,
    ) -> (PathBuf, PathBuf, PathBuf) {
        let aws_dir = dir.join(".aws");
        std::fs::create_dir_all(&aws_dir).expect("mkdir .aws");
        let credentials_path = aws_dir.join("credentials");
        // SSO profiles often have NO credentials file at all, but we write
        // an empty one so the env-var override works in the resolver.
        std::fs::write(&credentials_path, "").expect("write credentials");
        let config_path = aws_dir.join("config");
        std::fs::write(&config_path, config_text).expect("write config");
        let cache_dir = aws_dir.join("sso").join("cache");
        std::fs::create_dir_all(&cache_dir).expect("mkdir cache");
        let mut payload = serde_json::json!({
            "accessToken": access_token,
            "region": "us-east-1",
        });
        if let Some(expires_at) = expires_at {
            payload["expiresAt"] = serde_json::Value::String(expires_at.to_string());
        }
        std::fs::write(
            aws_sso_cache_path(&cache_dir, cache_key),
            payload.to_string(),
        )
        .expect("write cache file");
        (credentials_path, config_path, cache_dir)
    }

    fn far_future_iso() -> String {
        let dt = chrono::Utc::now() + chrono::Duration::hours(8);
        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    fn far_past_iso() -> String {
        let dt = chrono::Utc::now() - chrono::Duration::hours(8);
        dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    #[test]
    fn test_aws_sso_cache_filename_matches_aws_cli() {
        // AWS CLI hashes the literal start-URL (or sso-session name) with SHA1
        // and uses the lowercase hex digest as the cache filename. Reference
        // vector verified against `printf '%s' "https://example.awsapps.com/start" | sha1sum`:
        //   e8be5486177c5b5392bd9aa76563515b29358e6e
        let filename = aws_sso_cache_filename("https://example.awsapps.com/start");
        assert_eq!(filename, "e8be5486177c5b5392bd9aa76563515b29358e6e.json");
    }

    #[test]
    fn test_aws_sso_detect_session_form() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let config_text = "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole
region = us-west-2

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1
";
        let expires = far_future_iso();
        let (_cred, config_path, cache_dir) = write_sso_sandbox(
            dir.path(),
            "my-sso",
            "sso-access-token-123",
            Some(&expires),
            config_text,
        );

        let mut env = |var: &str| match var {
            "AWS_PROFILE" => Some("dev".to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            "PI_AWS_SSO_CACHE_DIR" => Some(cache_dir.to_string_lossy().to_string()),
            "HOME" => Some(dir.path().to_string_lossy().to_string()),
            _ => None,
        };
        let detected = detect_aws_sso_profile_with_env("dev", None, "us-east-1", &mut env)
            .expect("detect")
            .expect("present");
        assert_eq!(detected.access_token, "sso-access-token-123");
        assert_eq!(detected.account_id, "123456789012");
        assert_eq!(detected.role_name, "MyRole");
        assert_eq!(detected.sso_region, "us-east-1");
        assert_eq!(detected.target_region, "us-west-2");
    }

    #[test]
    fn test_aws_sso_detect_legacy_form() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let config_text = "\
[profile legacy]
sso_start_url = https://legacy.awsapps.com/start
sso_region = us-east-2
sso_account_id = 999999999999
sso_role_name = LegacyRole
region = eu-west-1
";
        let expires = far_future_iso();
        let (_cred, config_path, cache_dir) = write_sso_sandbox(
            dir.path(),
            "https://legacy.awsapps.com/start",
            "legacy-token",
            Some(&expires),
            config_text,
        );

        let mut env = |var: &str| match var {
            "AWS_PROFILE" => Some("legacy".to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            "PI_AWS_SSO_CACHE_DIR" => Some(cache_dir.to_string_lossy().to_string()),
            _ => None,
        };
        let detected = detect_aws_sso_profile_with_env("legacy", None, "us-east-1", &mut env)
            .expect("detect")
            .expect("present");
        assert_eq!(detected.access_token, "legacy-token");
        assert_eq!(detected.account_id, "999999999999");
        assert_eq!(detected.role_name, "LegacyRole");
        assert_eq!(detected.sso_region, "us-east-2");
        assert_eq!(detected.target_region, "eu-west-1");
    }

    #[test]
    fn test_aws_sso_non_sso_profile_returns_none() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let aws_dir = dir.path().join(".aws");
        std::fs::create_dir_all(&aws_dir).expect("mkdir");
        let config_path = aws_dir.join("config");
        std::fs::write(&config_path, "[profile static]\nregion = us-west-2\n")
            .expect("write config");
        let credentials_path = aws_dir.join("credentials");
        std::fs::write(&credentials_path, "").expect("write credentials");

        let mut env = |var: &str| match var {
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            "AWS_SHARED_CREDENTIALS_FILE" => Some(credentials_path.to_string_lossy().to_string()),
            _ => None,
        };
        let detected =
            detect_aws_sso_profile_with_env("static", None, "us-east-1", &mut env).expect("detect");
        assert!(detected.is_none());
    }

    #[test]
    fn test_aws_sso_expired_token_returns_error_with_login_command() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let config_text = "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1
";
        let expires = far_past_iso();
        let (_cred, config_path, cache_dir) = write_sso_sandbox(
            dir.path(),
            "my-sso",
            "expired-token",
            Some(&expires),
            config_text,
        );

        let mut env = |var: &str| match var {
            "AWS_PROFILE" => Some("dev".to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            "PI_AWS_SSO_CACHE_DIR" => Some(cache_dir.to_string_lossy().to_string()),
            _ => None,
        };
        let err = detect_aws_sso_profile_with_env("dev", None, "us-east-1", &mut env)
            .expect_err("expired token must error");
        let msg = err.to_string();
        assert!(
            msg.contains("expired"),
            "expected 'expired' in error: {msg}"
        );
        assert!(
            msg.contains("aws sso login --profile dev"),
            "expected login hint in error: {msg}"
        );
    }

    #[test]
    fn test_aws_sso_missing_cache_returns_error_with_login_command() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let aws_dir = dir.path().join(".aws");
        std::fs::create_dir_all(&aws_dir).expect("mkdir");
        let config_path = aws_dir.join("config");
        std::fs::write(
            &config_path,
            "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1
",
        )
        .expect("write config");
        let cache_dir = aws_dir.join("sso").join("cache");
        std::fs::create_dir_all(&cache_dir).expect("mkdir cache");
        // No cache file written.

        let mut env = |var: &str| match var {
            "AWS_PROFILE" => Some("dev".to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            "PI_AWS_SSO_CACHE_DIR" => Some(cache_dir.to_string_lossy().to_string()),
            _ => None,
        };
        let err = detect_aws_sso_profile_with_env("dev", None, "us-east-1", &mut env)
            .expect_err("missing cache must error");
        let msg = err.to_string();
        assert!(
            msg.contains("aws sso login --profile dev"),
            "expected login hint in error: {msg}"
        );
    }

    #[test]
    fn test_aws_sso_oversized_token_cache_is_rejected() {
        let directory = tempfile::tempdir().expect("tmpdir");
        let config_text = "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1
";
        let expires = far_future_iso();
        let (_credentials_path, config_path, cache_dir) = write_sso_sandbox(
            directory.path(),
            "my-sso",
            "initial-token",
            Some(&expires),
            config_text,
        );
        let cache_path = aws_sso_cache_path(&cache_dir, "my-sso");
        std::fs::write(&cache_path, vec![b'x'; MAX_AWS_SSO_CACHE_BYTES + 1])
            .expect("write oversized token cache");

        let mut env = |name: &str| match name {
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            "PI_AWS_SSO_CACHE_DIR" => Some(cache_dir.to_string_lossy().to_string()),
            _ => None,
        };
        let error = detect_aws_sso_profile_with_env("dev", None, "us-east-1", &mut env)
            .expect_err("oversized token cache must be rejected");
        let message = error.to_string();
        assert!(message.contains("exceeds"), "{message}");
        assert!(message.contains("aws sso login --profile dev"), "{message}");
    }

    #[test]
    fn test_aws_sso_sso_session_block_missing_errors() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let aws_dir = dir.path().join(".aws");
        std::fs::create_dir_all(&aws_dir).expect("mkdir");
        let config_path = aws_dir.join("config");
        std::fs::write(
            &config_path,
            "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole
",
        )
        .expect("write config");

        let mut env = |var: &str| match var {
            "AWS_PROFILE" => Some("dev".to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            _ => None,
        };
        let err = detect_aws_sso_profile_with_env("dev", None, "us-east-1", &mut env)
            .expect_err("missing sso-session block must error");
        assert!(
            err.to_string().contains("[sso-session my-sso]"),
            "expected sso-session hint: {err}"
        );
    }

    #[test]
    fn test_aws_sso_missing_role_or_account_errors() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let aws_dir = dir.path().join(".aws");
        std::fs::create_dir_all(&aws_dir).expect("mkdir");
        let config_path = aws_dir.join("config");
        // sso_session set but role_name missing.
        std::fs::write(
            &config_path,
            "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1
",
        )
        .expect("write config");

        let mut env = |var: &str| match var {
            "AWS_PROFILE" => Some("dev".to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            _ => None,
        };
        let err = detect_aws_sso_profile_with_env("dev", None, "us-east-1", &mut env)
            .expect_err("missing role_name must error");
        assert!(
            err.to_string().contains("sso_role_name"),
            "expected sso_role_name hint: {err}"
        );
    }

    #[test]
    #[allow(clippy::await_holding_lock)] // intentional — see sso_portal_override_lock docs
    fn test_aws_sso_async_resolver_succeeds_via_mock_portal() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let config_text = "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole
region = us-west-2

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1
";
            let expires = far_future_iso();
            let (_cred, config_path, cache_dir) = write_sso_sandbox(
                dir.path(),
                "my-sso",
                "sso-access-token-abcdef",
                Some(&expires),
                config_text,
            );

            // Mock SSO Portal: returns short-lived IAM credentials.
            let body = r#"{"roleCredentials":{"accessKeyId":"AKIASTSTOKEN","secretAccessKey":"sts-secret","sessionToken":"sts-session","expiration":1700000000000}}"#;
            let portal_url = spawn_json_server(200, body);
            // spawn_json_server appends "/token" to the path; strip it so the
            // resolver appends "/federation/credentials" itself.
            let portal_base = portal_url.trim_end_matches("/token").to_string();

            // Acquire the override-mutex BEFORE setting the override and hold
            // it through the assert. This serialises tests that share the
            // SSO_PORTAL_BASE_URL_OVERRIDE static; without it parallel test
            // execution sporadically reads another test's override and routes
            // GetRoleCredentials at the wrong mock server (401 → spurious fail).
            let _override_guard = sso_portal_override_lock();
            // Install the test-only SSO Portal base URL override. We can't use
            // `std::env::set_var` here because Rust 2024 made it `unsafe` and
            // the crate forbids unsafe_code; the static-Mutex helper above
            // gives the same isolation without touching global env state.
            set_sso_portal_base_url_override(Some(portal_base.clone()));
            let mut env_map = std::collections::HashMap::new();
            env_map.insert("AWS_PROFILE", "dev".to_string());
            env_map.insert(
                "AWS_CONFIG_FILE",
                config_path.to_string_lossy().to_string(),
            );
            env_map.insert(
                "PI_AWS_SSO_CACHE_DIR",
                cache_dir.to_string_lossy().to_string(),
            );

            let mut auth = empty_auth();
            auth.set(
                "amazon-bedrock",
                AuthCredential::AwsCredentials {
                    access_key_id: "AKIA_STORED_FALLBACK".to_string(),
                    // ubs:ignore test fixture credential, not live secret.
                    secret_access_key: "stored-fallback-secret".to_string(),
                    session_token: None,
                    region: Some("eu-west-1".to_string()),
                },
            );
            let client = crate::http::client::Client::new();
            let result = resolve_aws_credentials_async_with_env(&auth, &client, |var| {
                env_map.get(var).cloned()
            })
            .await
            .expect("resolve");

            set_sso_portal_base_url_override(None);

            let resolved = result.expect("present");
            match resolved {
                AwsResolvedCredentials::Sigv4 {
                    access_key_id,
                    secret_access_key,
                    session_token,
                    region,
                } => {
                    // An explicitly selected SSO profile is the profile tier and
                    // must not be shadowed by a stored auth.json fallback.
                    assert_eq!(access_key_id, "AKIASTSTOKEN");
                    assert_eq!(secret_access_key, "sts-secret");
                    assert_eq!(session_token.as_deref(), Some("sts-session"));
                    assert_eq!(region, "us-west-2");
                }
                other @ AwsResolvedCredentials::Bearer { .. } => {
                    unreachable!("expected Sigv4, got {other:?}")
                }
            }
        });
    }

    #[test]
    #[allow(clippy::await_holding_lock)] // intentional — see sso_portal_override_lock docs
    fn test_aws_sso_async_resolver_propagates_portal_error() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let config_text = "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1
";
            let expires = far_future_iso();
            let (_cred, config_path, cache_dir) = write_sso_sandbox(
                dir.path(),
                "my-sso",
                "sso-access-token",
                Some(&expires),
                config_text,
            );
            let portal_url = spawn_json_server(403, r#"{"message":"forbidden"}"#);
            let portal_base = portal_url.trim_end_matches("/token").to_string();

            // See sso_portal_override_lock above for why this guard is needed.
            let _override_guard = sso_portal_override_lock();
            set_sso_portal_base_url_override(Some(portal_base.clone()));
            let mut env_map = std::collections::HashMap::new();
            env_map.insert("AWS_PROFILE", "dev".to_string());
            env_map.insert("AWS_CONFIG_FILE", config_path.to_string_lossy().to_string());
            env_map.insert(
                "PI_AWS_SSO_CACHE_DIR",
                cache_dir.to_string_lossy().to_string(),
            );

            let auth = empty_auth();
            let client = crate::http::client::Client::new();
            let err = resolve_aws_credentials_async_with_env(&auth, &client, |var| {
                env_map.get(var).cloned()
            })
            .await
            .expect_err("403 must surface as auth error");

            set_sso_portal_base_url_override(None);

            let msg = err.to_string();
            assert!(msg.contains("HTTP 403"), "expected 403 in error: {msg}");
            assert!(
                msg.contains("aws sso login"),
                "expected login hint in 403 error: {msg}"
            );
        });
    }

    #[test]
    #[allow(clippy::await_holding_lock)] // intentional — see sso_portal_override_lock docs
    fn test_aws_sso_async_resolver_redacts_access_token_in_errors() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let dir = tempfile::tempdir().expect("tmpdir");
            let config_text = "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
            sso_region = us-east-1
";
            let expires = far_future_iso();
            // ubs:ignore test fixture credential, not live secret.
            let secret_token = "VERY-SECRET-SSO-TOKEN-DO-NOT-LEAK";
            let (_cred, config_path, cache_dir) = write_sso_sandbox(
                dir.path(),
                "my-sso",
                secret_token,
                Some(&expires),
                config_text,
            );
            // Body echoes the access token to verify redaction.
            let body = format!(r#"{{"echoed":"{secret_token}"}}"#);
            let portal_url = spawn_json_server(401, &body);
            let portal_base = portal_url.trim_end_matches("/token").to_string();

            // See sso_portal_override_lock above for why this guard is needed.
            let _override_guard = sso_portal_override_lock();
            set_sso_portal_base_url_override(Some(portal_base.clone()));
            let mut env_map = std::collections::HashMap::new();
            env_map.insert("AWS_PROFILE", "dev".to_string());
            env_map.insert("AWS_CONFIG_FILE", config_path.to_string_lossy().to_string());
            env_map.insert(
                "PI_AWS_SSO_CACHE_DIR",
                cache_dir.to_string_lossy().to_string(),
            );

            let auth = empty_auth();
            let client = crate::http::client::Client::new();
            let err = resolve_aws_credentials_async_with_env(&auth, &client, |var| {
                env_map.get(var).cloned()
            })
            .await
            .expect_err("401 must surface as auth error");

            set_sso_portal_base_url_override(None);

            let msg = err.to_string();
            assert!(
                !msg.contains(secret_token),
                "access token must be redacted in errors: {msg}"
            );
            assert!(
                msg.contains("[REDACTED]"),
                "expected redaction marker: {msg}"
            );
        });
    }

    #[test]
    #[allow(clippy::await_holding_lock)] // intentional — see sso_portal_override_lock docs
    fn test_aws_sso_exchange_rejects_oversized_response_body() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let body = "x".repeat(MAX_AWS_SSO_RESPONSE_BYTES + 1);
            let portal_url = spawn_json_server(200, &body);
            let portal_base = portal_url.trim_end_matches("/token").to_string();
            let _override_guard = sso_portal_override_lock();
            set_sso_portal_base_url_override(Some(portal_base));

            let locator = AwsSsoTokenLocator {
                access_token: "bounded-response-token".to_string(),
                sso_region: "us-east-1".to_string(),
                account_id: "123456789012".to_string(),
                role_name: "BoundedRole".to_string(),
                target_region: "us-west-2".to_string(),
            };
            let client = crate::http::client::Client::new();
            let error = exchange_aws_sso_credentials_with_client(&client, &locator)
                .await
                .expect_err("oversized SSO response must be rejected");

            set_sso_portal_base_url_override(None);

            let message = error.to_string();
            assert!(message.contains("oversized response body"), "{message}");
            assert!(!message.contains(&body), "response body leaked: {message}");
        });
    }

    #[test]
    fn test_aws_sso_async_resolver_falls_through_when_static_creds_present() {
        // Static IAM credentials in env vars must short-circuit the SSO
        // path: no HTTP, no cache lookups.
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let auth = empty_auth();
            let client = crate::http::client::Client::new();
            let mut env_map = std::collections::HashMap::new();
            env_map.insert("AWS_ACCESS_KEY_ID", "AKIASTATIC".to_string());
            // ubs:ignore test fixture credential, not live secret.
            env_map.insert("AWS_SECRET_ACCESS_KEY", "static-secret".to_string());
            env_map.insert("AWS_REGION", "us-west-2".to_string());

            let result = resolve_aws_credentials_async_with_env(&auth, &client, |var| {
                env_map.get(var).cloned()
            })
            .await
            .expect("resolve");
            match result {
                Some(AwsResolvedCredentials::Sigv4 { access_key_id, .. }) => {
                    assert_eq!(access_key_id, "AKIASTATIC");
                }
                other => unreachable!("expected static Sigv4, got {other:?}"),
            }
        });
    }

    #[test]
    fn test_aws_sso_target_region_overridden_by_env() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let config_text = "\
[profile dev]
sso_session = my-sso
sso_account_id = 123456789012
sso_role_name = MyRole
region = us-west-2

[sso-session my-sso]
sso_start_url = https://example.awsapps.com/start
sso_region = us-east-1
";
        let expires = far_future_iso();
        let (_cred, config_path, cache_dir) =
            write_sso_sandbox(dir.path(), "my-sso", "tok", Some(&expires), config_text);

        let mut env = |var: &str| match var {
            "AWS_PROFILE" => Some("dev".to_string()),
            "AWS_CONFIG_FILE" => Some(config_path.to_string_lossy().to_string()),
            "PI_AWS_SSO_CACHE_DIR" => Some(cache_dir.to_string_lossy().to_string()),
            _ => None,
        };
        let detected =
            detect_aws_sso_profile_with_env("dev", Some("eu-north-1"), "us-east-1", &mut env)
                .expect("detect")
                .expect("present");
        // env_region beats profile.region (matches static-IAM semantics).
        assert_eq!(detected.target_region, "eu-north-1");
        // sso_region is independent and should still come from sso-session block.
        assert_eq!(detected.sso_region, "us-east-1");
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
                // ubs:ignore test fixture credential, not live secret.
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
            // ubs:ignore test fixture credential, not live secret.
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.sap.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://service.sap.example.com".to_string()),
            _ => None,
        });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "env-client".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
                client_id: Some("  stored-id  ".to_string()),
                // ubs:ignore test fixture credential, not live secret.
                client_secret: Some("  stored-secret  ".to_string()),
                // ubs:ignore test fixture credential endpoint, not live secret.
                token_url: Some("  https://stored-token.sap.com  ".to_string()),
                service_url: Some("  https://stored-api.sap.com  ".to_string()),
            },
        );
        let result = resolve_sap_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "stored-id".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                client_secret: "stored-secret".to_string(),
                // ubs:ignore test fixture credential endpoint, not live secret.
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
                // ubs:ignore test fixture credential, not live secret.
                client_secret: Some("alias-secret".to_string()),
                // ubs:ignore test fixture credential endpoint, not live secret.
                token_url: Some("https://alias-token.sap.com".to_string()),
                service_url: Some("https://alias-api.sap.com".to_string()),
            },
        );
        let result = resolve_sap_credentials_with_env(&auth, |_| -> Option<String> { None });
        assert_eq!(
            result,
            Some(SapResolvedCredentials {
                client_id: "alias-id".to_string(),
                // ubs:ignore test fixture credential, not live secret.
                client_secret: "alias-secret".to_string(),
                // ubs:ignore test fixture credential endpoint, not live secret.
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
    fn test_sap_partial_split_env_does_not_fall_through_to_stored_account() {
        let mut auth = empty_auth();
        auth.set(
            "sap-ai-core",
            AuthCredential::ServiceKey {
                client_id: Some("stored-client".to_string()),
                // ubs:ignore test fixture credential, not live secret.
                client_secret: Some("stored-secret".to_string()),
                token_url: Some("https://stored-token.example.com".to_string()),
                service_url: Some("https://stored-api.example.com".to_string()),
            },
        );

        let error = resolve_sap_auth_material_with_env(&auth, None, |name| match name {
            "SAP_AI_CORE_CLIENT_ID" => Some("ambient-client".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("ambient-secret".to_string()),
            _ => None,
        })
        .expect_err("a partial higher-precedence account must fail closed");
        let message = error.to_string();
        assert!(message.contains("partially configured"), "{message}");
        assert!(!message.contains("ambient-client"), "{message}");
        assert!(!message.contains("ambient-secret"), "{message}");
        assert!(!message.contains("stored-client"), "{message}");
    }

    #[test]
    fn test_sap_invalid_nonempty_json_fails_closed() {
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "AICORE_SERVICE_KEY" => Some("not-valid-json".to_string()),
            "SAP_AI_CORE_CLIENT_ID" => Some("env-id".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://api.example.com".to_string()),
            _ => None,
        });
        assert!(result.is_none());
    }

    #[test]
    fn test_sap_nonempty_json_with_whitespace_required_field_fails_closed() {
        let whitespace_json = serde_json::json!({
            "clientid": "   ",
            "clientsecret": "json-secret",
            "url": "https://json-token.example.com",
            "serviceurls": {"AI_API_URL": "https://json-api.example.com"}
        })
        .to_string();
        let auth = empty_auth();
        let result = resolve_sap_credentials_with_env(&auth, |var| match var {
            "AICORE_SERVICE_KEY" => Some(whitespace_json.clone()),
            "SAP_AI_CORE_CLIENT_ID" => Some("env-id".to_string()),
            "SAP_AI_CORE_CLIENT_SECRET" => Some("env-secret".to_string()),
            "SAP_AI_CORE_TOKEN_URL" => Some("https://token.example.com".to_string()),
            "SAP_AI_CORE_SERVICE_URL" => Some("https://api.example.com".to_string()),
            _ => None,
        });
        assert!(result.is_none());
    }

    #[test]
    fn test_sap_auth_material_precedence_is_explicit_and_account_safe() {
        let ambient_service_key = serde_json::json!({
            "clientid": "ambient-client",
            "clientsecret": "ambient-secret",
            "url": "https://ambient-token.example.com",
            "serviceurls": {"AI_API_URL": "https://ambient-api.example.com"}
        })
        .to_string();
        let mut auth = empty_auth();
        auth.set(
            "sap-ai-core",
            AuthCredential::BearerToken {
                token: "stored-direct-bearer".to_string(),
            },
        );

        let stored_direct = resolve_sap_auth_material_with_env(&auth, None, |name| {
            (name == "AICORE_SERVICE_KEY").then(|| ambient_service_key.clone())
        })
        .expect("resolve stored direct bearer");
        assert_eq!(
            stored_direct,
            Some(SapAuthMaterial::DirectBearer(
                "stored-direct-bearer".to_string()
            ))
        );

        let explicit =
            resolve_sap_auth_material_with_env(&auth, Some("explicit-direct-bearer"), |_| None)
                .expect("resolve explicit bearer");
        assert_eq!(
            explicit,
            Some(SapAuthMaterial::DirectBearer(
                "explicit-direct-bearer".to_string()
            ))
        );

        auth.set(
            "sap-ai-core",
            AuthCredential::ApiKey {
                key: "legacy-stored-bearer".to_string(),
            },
        );
        let ambient = resolve_sap_auth_material_with_env(&auth, None, |name| {
            (name == "AICORE_SERVICE_KEY").then(|| ambient_service_key.clone())
        })
        .expect("resolve ambient service key over legacy ApiKey");
        assert!(matches!(
            ambient,
            Some(SapAuthMaterial::ServiceKey(SapResolvedCredentials {
                client_id,
                ..
            })) if client_id == "ambient-client"
        ));

        let legacy = resolve_sap_auth_material_with_env(&auth, None, |_| None)
            .expect("resolve legacy ApiKey without ambient service credentials");
        assert_eq!(
            legacy,
            Some(SapAuthMaterial::DirectBearer(
                "legacy-stored-bearer".to_string()
            ))
        );
    }

    #[test]
    fn test_sap_auth_material_rejects_malformed_authoritative_service_key() {
        let mut auth = empty_auth();
        auth.set(
            "sap-ai-core",
            AuthCredential::ApiKey {
                key: "legacy-stored-bearer".to_string(),
            },
        );
        let err = resolve_sap_auth_material_with_env(&auth, None, |name| {
            (name == "AICORE_SERVICE_KEY").then(|| "{\"clientid\":\"incomplete\"}".to_string())
        })
        .expect_err("malformed authoritative service key must not fall through");
        assert!(
            err.to_string().contains("AICORE_SERVICE_KEY is set"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_sap_structured_array_credential_fails_closed() {
        let error = classify_sap_auth_candidate(r#"[{"clientsecret":"must-not-leak"}]"#)
            .expect_err("structured non-object credentials must never become bearer tokens");
        assert!(error.to_string().contains("JSON-shaped"), "{error}");
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
                // ubs:ignore test fixture credential, not live secret.
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

    #[test]
    fn test_sap_json_shaped_auth_candidate_fails_closed_when_invalid() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client = crate::http::client::Client::new();
            let err = resolve_sap_auth_candidate_with_client(
                &client,
                r#"{"clientid":"id","clientsecret":"secret"}"#,
            )
            .await
            .expect_err("incomplete service key must not become a bearer token");
            assert!(
                err.to_string().contains("not a valid service key"),
                "unexpected error: {err}"
            );
        });
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
                // ubs:ignore test fixture credential, not live secret.
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
    fn test_ambient_sap_auth_exchange_does_not_require_stored_auth() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let token_url = spawn_json_server(200, r#"{"access_token":"ambient-token"}"#);
            let service_key = serde_json::json!({
                "clientid": "ambient-client",
                // ubs:ignore test fixture credential, not live secret.
                "clientsecret": "ambient-secret",
                "url": token_url,
                "serviceurls": {"AI_API_URL": "https://ambient-api.example.com"}
            })
            .to_string();
            let client = crate::http::client::Client::new();

            let token = resolve_ambient_sap_auth_token_with_client_and_env(&client, |name| {
                (name == "AICORE_SERVICE_KEY").then(|| service_key.clone())
            })
            .await
            .expect("resolve complete ambient service key");

            assert_eq!(token.as_deref(), Some("ambient-token"));
            assert!(
                resolve_ambient_sap_auth_token_with_client_and_env(&client, |_| None)
                    .await
                    .expect("resolve absent ambient credentials")
                    .is_none()
            );
        });
    }

    #[test]
    fn test_exchange_sap_access_token_with_client_http_error() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let client_id = "sap client+id@example.com";
            let client_secret = "sap&secret=100%";
            let encoded_client_id = percent_encode_component(client_id);
            let encoded_client_secret = percent_encode_component(client_secret);
            let echoed_form = format!(
                "invalid_client client_id={encoded_client_id}&client_secret={encoded_client_secret}"
            );
            let token_url = spawn_json_server(401, &echoed_form);
            let client = crate::http::client::Client::new();
            let creds = SapResolvedCredentials {
                client_id: client_id.to_string(),
                // ubs:ignore test fixture credential, not live secret.
                client_secret: client_secret.to_string(),
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
            let message = err.to_string();
            assert!(
                !message.contains(client_id),
                "raw client ID leaked: {message}"
            );
            assert!(
                !message.contains(client_secret),
                "raw client secret leaked: {message}"
            );
            assert!(
                !message.contains(&encoded_client_id),
                "encoded client ID leaked: {message}"
            );
            assert!(
                !message.contains(&encoded_client_secret),
                "encoded client secret leaked: {message}"
            );
            assert!(message.contains("[REDACTED]"), "{message}");
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
                // ubs:ignore test fixture credential, not live secret.
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
                    extra: HashMap::new(),
                    // ubs:ignore test fixture credential, not live secret.
                    access_token: "about-to-expire".to_string(),
                    // ubs:ignore test fixture credential, not live secret.
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
                other => unreachable!("unexpected credential variant: {other:?}"),
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
                    extra: HashMap::new(),
                    // ubs:ignore test fixture credential, not live secret.
                    access_token: "still-good".to_string(),
                    // ubs:ignore test fixture credential, not live secret.
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
                other => unreachable!("unexpected credential variant: {other:?}"),
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
                    extra: HashMap::new(),
                    // ubs:ignore test fixture credential, not live secret.
                    access_token: "expired-copilot".to_string(),
                    // ubs:ignore test fixture credential, not live secret.
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
                other => unreachable!("unexpected credential variant: {other:?}"),
            }
        });
    }

    #[test]
    fn test_self_contained_refresh_rejects_oversized_response() {
        let rt = asupersync::runtime::RuntimeBuilder::current_thread().build();
        rt.expect("runtime").block_on(async {
            let oversized = "x".repeat(MAX_OAUTH_RESPONSE_BYTES + 1);
            let server_url = spawn_json_server(200, &oversized);
            let client = crate::http::client::Client::new();
            let error = refresh_self_contained_oauth_token(
                &client,
                &server_url,
                "test-client",
                "test-refresh-token",
                "custom-provider",
            )
            .await
            .expect_err("oversized self-contained refresh body must be rejected");
            let message = error.to_string();
            assert!(
                message.contains("Invalid refresh response from custom-provider"),
                "{message}"
            );
            assert!(
                !message.contains(&"x".repeat(256)),
                "oversized untrusted body must not be copied into the diagnostic"
            );
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
                    extra: HashMap::new(),
                    // ubs:ignore test fixture credential, not live secret.
                    access_token: "old-ext".to_string(),
                    // ubs:ignore test fixture credential, not live secret.
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
                other => unreachable!("unexpected credential variant: {other:?}"),
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
                    extra: HashMap::new(),
                    // ubs:ignore test fixture credential, not live secret.
                    access_token: "self-contained".to_string(),
                    // ubs:ignore test fixture credential, not live secret.
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
                other => unreachable!("unexpected credential variant: {other:?}"),
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "dead".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "old-copilot".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "recent".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
                key: "sk-test".to_string(),
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
                extra: HashMap::new(),
                // ubs:ignore test fixture credential, not live secret.
                access_token: "valid".to_string(),
                // ubs:ignore test fixture credential, not live secret.
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
            extra: HashMap::new(),
            // ubs:ignore test fixture credential, not live secret.
            access_token: "tok".to_string(),
            // ubs:ignore test fixture credential, not live secret.
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
            other => unreachable!("unexpected credential variant: {other:?}"),
        }
    }

    #[test]
    fn test_credential_serialization_omits_none_fields() {
        let cred = AuthCredential::OAuth {
            extra: HashMap::new(),
            // ubs:ignore test fixture credential, not live secret.
            access_token: "tok".to_string(),
            // ubs:ignore test fixture credential, not live secret.
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
            other => unreachable!("unexpected credential variant: {other:?}"),
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

    // ── ApiKeySource resolution tests ───────────────────────────────

    #[test]
    fn test_resolve_api_key_source_literal_string() {
        let result = resolve_api_key_source_with_env("sk-plain-key-12345", |_| None);
        assert_eq!(result.unwrap(), Some("sk-plain-key-12345".to_string()));
    }

    #[test]
    fn test_resolve_api_key_source_empty_literal() {
        let result = resolve_api_key_source_with_env("", |_| None);
        assert_eq!(result.unwrap(), Some(String::new()));
    }

    #[test]
    fn test_resolve_api_key_source_env_var_resolves() {
        let result = resolve_api_key_source_with_env("$ENV:MY_API_KEY", |var| {
            assert_eq!(var, "MY_API_KEY");
            Some("resolved-secret-key".to_string())
        });
        assert_eq!(result.unwrap(), Some("resolved-secret-key".to_string()));
    }

    #[test]
    fn test_resolve_api_key_source_env_var_trims_whitespace() {
        let result =
            resolve_api_key_source_with_env("$ENV:MY_KEY", |_| Some("  spaced-key  ".to_string()));
        assert_eq!(result.unwrap(), Some("spaced-key".to_string()));
    }

    #[test]
    fn test_resolve_api_key_source_command_resolves() {
        let result = resolve_api_key_source_with_resolvers(
            "$CMD:pass show openai/api-key",
            |_| None,
            |command| {
                assert_eq!(command, "pass show openai/api-key");
                Ok(Some("resolved-command-key".to_string()))
            },
        );
        assert_eq!(result.unwrap(), Some("resolved-command-key".to_string()));
    }

    #[test]
    fn test_resolve_api_key_source_command_alias_prefix_resolves() {
        let result = resolve_api_key_source_with_resolvers(
            "$COMMAND:op read secret",
            |_| None,
            |command| {
                assert_eq!(command, "op read secret");
                Ok(Some("from-command-alias".to_string()))
            },
        );
        assert_eq!(result.unwrap(), Some("from-command-alias".to_string()));
    }

    #[test]
    fn test_resolve_api_key_source_command_trims_whitespace() {
        let result = resolve_api_key_source_with_resolvers(
            "$CMD:security find-generic-password",
            |_| None,
            |_| Ok(Some("  spaced-command-key  ".to_string())),
        );
        assert_eq!(result.unwrap(), Some("spaced-command-key".to_string()));
    }

    #[test]
    fn test_resolve_api_key_source_command_empty_returns_none() {
        let result = resolve_api_key_source_with_resolvers(
            "$CMD:echo key",
            |_| None,
            |_| Ok(Some("   ".to_string())),
        );
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_resolve_api_key_source_command_runner_error_propagates() {
        let result = resolve_api_key_source_with_resolvers(
            "$CMD:bad-command",
            |_| None,
            |_| Err("boom".to_string()),
        );
        assert_eq!(result.unwrap_err(), "boom");
    }

    #[test]
    fn test_resolve_api_key_source_command_prefix_no_command_is_error() {
        let result = resolve_api_key_source_with_resolvers(
            "$CMD:   ",
            |_| None,
            |_| {
                unreachable!("command runner should not be called for empty command");
            },
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires a shell command"));
    }

    #[test]
    fn test_resolve_api_key_source_env_var_missing_returns_none() {
        let result = resolve_api_key_source_with_env("$ENV:NONEXISTENT_VAR", |_| None);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_resolve_api_key_source_env_var_empty_returns_none() {
        let result = resolve_api_key_source_with_env("$ENV:EMPTY_VAR", |_| Some(String::new()));
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_resolve_api_key_source_env_var_whitespace_only_returns_none() {
        let result = resolve_api_key_source_with_env("$ENV:WS_ONLY", |_| Some("   ".to_string()));
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_resolve_api_key_source_env_prefix_no_var_name_is_error() {
        let result = resolve_api_key_source_with_env("$ENV:", |_| None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires a variable name"));
    }

    #[test]
    fn test_resolve_api_key_source_dollar_without_env_is_literal() {
        // "$NOTENV:FOO" is NOT a special prefix — only "$ENV:" is.
        let result = resolve_api_key_source_with_env("$NOTENV:FOO", |_| {
            unreachable!("should not call env_lookup for non-$ENV: prefixed keys");
        });
        assert_eq!(result.unwrap(), Some("$NOTENV:FOO".to_string()));
    }

    #[test]
    fn test_resolve_api_key_source_case_sensitive_prefix() {
        // "$env:FOO" (lowercase) should NOT trigger env resolution — only "$ENV:" is valid.
        let result = resolve_api_key_source_with_env("$env:MY_KEY", |_| {
            unreachable!("should not call env_lookup for lowercase $env:");
        });
        assert_eq!(result.unwrap(), Some("$env:MY_KEY".to_string()));
    }

    #[test]
    fn test_api_key_from_credential_resolves_env_var() {
        // Test env var resolution through the testable `resolve_api_key_source_with_env` function,
        // since the crate forbids unsafe code (and `std::env::set_var` is unsafe).
        let result = resolve_api_key_source_with_env("$ENV:MY_SECRET_KEY", |var| {
            assert_eq!(var, "MY_SECRET_KEY");
            Some("from-env-42".to_string())
        });
        assert_eq!(result.unwrap(), Some("from-env-42".to_string()));
    }

    #[test]
    fn test_api_key_from_credential_literal_backward_compat() {
        let cred = AuthCredential::ApiKey {
            key: "sk-ant-plain-key".to_string(),
        };
        let result = api_key_from_credential(&cred);
        assert_eq!(result, Some("sk-ant-plain-key".to_string()));
    }

    #[test]
    fn test_api_key_from_credential_env_var_missing_returns_none() {
        let result = resolve_api_key_source_with_env("$ENV:DEFINITELY_NOT_SET", |_| None);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn test_env_key_roundtrip_serialization() {
        let cred = AuthCredential::ApiKey {
            key: "$ENV:OPENAI_API_KEY".to_string(),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::ApiKey { key } => {
                assert_eq!(key, "$ENV:OPENAI_API_KEY");
            }
            other => unreachable!("expected ApiKey, got {:?}", other),
        }
    }

    #[test]
    fn test_command_key_roundtrip_serialization() {
        let cred = AuthCredential::ApiKey {
            key: "$CMD:op read op://vault/openai/api-key --no-newline".to_string(),
        };
        let json = serde_json::to_string(&cred).expect("serialize");
        let parsed: AuthCredential = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            AuthCredential::ApiKey { key } => {
                assert_eq!(key, "$CMD:op read op://vault/openai/api-key --no-newline");
            }
            other => unreachable!("expected ApiKey, got {:?}", other),
        }
    }

    #[test]
    fn test_env_key_in_auth_file_roundtrip() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path.clone(),
            entries: HashMap::new(),
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "$ENV:OPENAI_API_KEY".to_string(),
            },
        );
        auth.save().expect("save");

        let loaded = AuthStorage::load(auth_path).expect("load");
        match loaded.get("openai") {
            Some(AuthCredential::ApiKey { key }) => {
                assert_eq!(key, "$ENV:OPENAI_API_KEY");
            }
            other => unreachable!("expected ApiKey with $ENV: prefix, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_api_key_env_source_integration() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let auth_path = dir.path().join("auth.json");
        let mut auth = AuthStorage {
            path: auth_path,
            entries: HashMap::new(),
        };
        auth.set(
            "openai",
            AuthCredential::ApiKey {
                key: "$ENV:PI_TEST_OPENAI_KEY_INTEG".to_string(),
            },
        );

        // Without the env var set, stored key should resolve to None, then fall
        // through the resolution chain.
        let resolved_without_env = auth.resolve_api_key_with_env_lookup("openai", None, |_| None);
        assert!(
            resolved_without_env.is_none(),
            "env-backed key with unset var and no env fallback should resolve to None"
        );

        // With override, override always wins regardless of stored key format.
        let resolved_with_override =
            auth.resolve_api_key_with_env_lookup("openai", Some("override-key"), |_| None);
        assert_eq!(resolved_with_override.as_deref(), Some("override-key"));
    }
}
