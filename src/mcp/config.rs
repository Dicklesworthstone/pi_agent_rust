//! MCP server configuration: discovery, parsing, merge, and precedence.
//!
//! Sources in precedence order (highest first, bd-cv653.6.1):
//!
//! 1. `--mcp-config <path>` (CLI, repeatable)
//! 2. `.pi/mcp.json` (project native)
//! 3. `.agents/mcp.json` (project cross-agent convention)
//! 4. `~/.pi/agent/mcp.json` (global native)
//! 5. Foreign files (`.claude/mcp.json`, `.cursor/mcp.json`,
//!    `.windsurf/mcp.json`, `.gemini/settings.json`, `.codex/config.toml`
//!    under the project, including OMP and VS Code) — marked `provenance=foreign`.
//! 6. Project-root `mcp.json` / `.mcp.json` portable fallbacks.
//!
//! Merge semantics: per server name, the highest-precedence source wins the
//! whole definition; every server records where it came from. Malformed
//! entries are skipped with a warning record. A whole-file failure in a native
//! or explicitly selected source blocks all lower-precedence sources so a
//! configuration error cannot revive an older trusted execution target.

mod acp;
pub(crate) use acp::parse_acp_servers;

use std::collections::HashMap;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const TRUST_FINGERPRINT_DOMAIN: &[u8] = b"pi_agent_rust:mcp-trust-surface:v2";
const MAX_MCP_CONFIG_BYTES: usize = 1024 * 1024;
/// Keep every MCP operation bounded, including imported OMP configurations.
pub(super) const MAX_MCP_TIMEOUT_MS: u64 = 3_600_000;

/// `HttpTransport` always installs Content-Type + Accept and may add an MCP
/// session and protocol-version headers after initialize. Leave room for all
/// four transport-owned headers inside the HTTP
/// client's hard 100-header ceiling so no trusted custom definition is ever
/// silently dropped.
const MAX_MCP_CUSTOM_HTTP_HEADERS: usize = 96;
const MAX_MCP_HEADER_NAME_BYTES: usize = 128;
const MAX_MCP_HEADER_VALUE_BYTES: usize = 16 * 1024;
const MAX_MCP_ENV_ENTRIES: usize = 256;
const MAX_MCP_ENV_NAME_BYTES: usize = 128;
const MAX_MCP_ENV_VALUE_BYTES: usize = 64 * 1024;

fn is_terminal_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        )
}

const fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

pub(super) fn validate_http_header_value(value: &str) -> std::result::Result<(), String> {
    if value.len() > MAX_MCP_HEADER_VALUE_BYTES {
        return Err(format!(
            "HTTP header value exceeds {MAX_MCP_HEADER_VALUE_BYTES} bytes"
        ));
    }
    if value.chars().any(is_terminal_control) {
        return Err(
            "HTTP header value contains terminal or protocol control characters".to_string(),
        );
    }
    Ok(())
}

pub(super) fn validate_env_value(value: &str) -> std::result::Result<(), String> {
    if value.len() > MAX_MCP_ENV_VALUE_BYTES {
        return Err(format!(
            "environment value exceeds {MAX_MCP_ENV_VALUE_BYTES} bytes"
        ));
    }
    if value.chars().any(is_terminal_control) {
        return Err(
            "environment value contains terminal or process control characters".to_string(),
        );
    }
    Ok(())
}

/// Where a server definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// `--mcp-config` CLI file.
    Cli,
    /// `.pi/mcp.json` in the project.
    ProjectPi,
    /// `.agents/mcp.json` in the project.
    ProjectAgents,
    /// `~/.pi/agent/mcp.json`.
    GlobalPi,
    /// A foreign tool's config file (`.claude/`, `.cursor/`, ...).
    Foreign,
    /// Contributed by an installed extension via `registerMcpServer`.
    Extension,
    /// Client-supplied ACP session definition. Env/header values are literal.
    Acp,
}

impl Provenance {
    /// Display label for the `/mcp` view.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::ProjectPi => ".pi",
            Self::ProjectAgents => ".agents",
            Self::GlobalPi => "global",
            Self::Foreign => "foreign",
            Self::Extension => "extension",
            Self::Acp => "acp",
        }
    }

    /// Whether this provenance is one of pi's native files.
    #[must_use]
    pub const fn is_native(self) -> bool {
        !matches!(self, Self::Foreign | Self::Extension | Self::Acp)
    }
}

/// One server definition after merging.
#[derive(Debug, Clone)]
pub struct ConfiguredServer {
    /// Server name (config map key).
    pub name: String,
    /// Spawn command (stdio servers).
    pub command: Option<String>,
    /// argv for the command.
    pub args: Vec<String>,
    /// Extra environment entries (values may use `$ENV:`/`$CMD:`).
    pub env: Vec<(String, String)>,
    /// Endpoint URL (HTTP servers).
    pub url: Option<String>,
    /// Extra HTTP headers (values may use `$ENV:`/`$CMD:`).
    pub headers: Vec<(String, String)>,
    /// Explicit transport hint (`"stdio"` / `"http"` / `"sse"`).
    pub transport_hint: Option<String>,
    /// Explicit configuration disablement is independent of operator trust.
    pub enabled: bool,
    /// Working directory, anchored to the session workspace during discovery.
    pub cwd: Option<PathBuf>,
    /// Per-operation timeout in milliseconds; absent means the native default.
    pub timeout_ms: Option<u64>,
    /// Where the definition came from.
    pub provenance: Provenance,
    /// Source file it was read from.
    pub source_file: PathBuf,
}

impl ConfiguredServer {
    /// Resolve execution paths consistently for trust, identity, and spawn.
    #[must_use]
    pub fn effective_cwd(&self, workspace: &Path) -> PathBuf {
        self.cwd.as_ref().map_or_else(
            || canonical_cwd(workspace),
            |cwd| {
                let path = if cwd.is_absolute() {
                    cwd.clone()
                } else {
                    workspace.join(cwd)
                };
                canonical_cwd(&path)
            },
        )
    }

    /// Versioned cryptographic fingerprint of the complete execution surface.
    ///
    /// Trust is global on disk, so the digest binds not only the target but
    /// also its name, raw secret definitions, provenance/source identity, and
    /// the effective working directory for local process resolution. Secret
    /// references are intentionally hashed before resolution: environment
    /// rotation does not re-prompt, while changing `$ENV:`/`$CMD:` definitions
    /// does.
    #[must_use]
    pub fn fingerprint(&self, effective_cwd: &Path) -> String {
        let mut hasher = Sha256::new();
        hash_part(&mut hasher, "domain", TRUST_FINGERPRINT_DOMAIN);
        hash_part(&mut hasher, "name", self.name.as_bytes());
        hash_optional(&mut hasher, "command", self.command.as_deref());
        hash_strings(&mut hasher, "args", &self.args);
        hash_definitions(&mut hasher, "env", &self.env, false);
        hash_optional(&mut hasher, "url", self.url.as_deref());
        hash_definitions(&mut hasher, "headers", &self.headers, true);
        hash_optional(
            &mut hasher,
            "transport_hint",
            self.transport_hint.as_deref(),
        );
        hash_part(
            &mut hasher,
            "provenance",
            self.provenance.label().as_bytes(),
        );

        let source = canonical_identity(&self.source_file, effective_cwd);
        hash_part(
            &mut hasher,
            "source_file",
            source.as_os_str().as_encoded_bytes(),
        );
        // HTTP headers can contain `$CMD:` references too, and their helper
        // processes run in the server's selected working directory. Bind cwd for all
        // transports so global/CLI trust cannot authorize a different local
        // helper merely because Pi was launched from another project.
        let cwd = canonical_cwd(effective_cwd);
        hash_part(
            &mut hasher,
            "effective_cwd",
            cwd.as_os_str().as_encoded_bytes(),
        );

        crate::package_manager::hex_encode(&hasher.finalize())
    }

    /// Whether this is an HTTP(-family) server.
    #[must_use]
    pub fn is_http(&self) -> bool {
        self.url.is_some()
            || matches!(
                self.transport_hint.as_deref(),
                Some("http" | "sse" | "streamable-http")
            )
    }
}

/// Canonical identity of the executable a stdio MCP server will actually
/// run (bd-sp5o3).
///
/// `resolved_path` is the symlink-collapsed absolute path selected for the
/// raw command (PATH search for bare names, cwd-anchored resolution for
/// relative paths), and `content_sha256` binds the bytes that path pointed
/// at when the operator acknowledged trust.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredExecutionIdentity {
    pub resolved_path: String,
    pub content_sha256: String,
}

impl std::fmt::Display for StoredExecutionIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (sha256:{})",
            self.resolved_path,
            &self.content_sha256[..16.min(self.content_sha256.len())]
        )
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path)
        .map_err(|err| format!("cannot open {:?} for hashing: {err}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|err| format!("cannot read {:?}: {err}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(crate::package_manager::hex_encode(&hasher.finalize()))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Pure resolver for one raw command against explicit PATH contents.
///
/// Semantics mirror ambient spawn behavior: bare names are searched in every
/// non-empty `path_env` directory in order (the current directory is NOT
/// implicitly searched), relative paths containing a separator anchor to
/// `cwd`, absolute paths pass through. The result collapses symlinks so a
/// retargeted link changes identity, and hashes the executable's content so
/// an in-place replacement is detected even when size/mtime lie.
// Debug-quoting paths in these errors is deliberate (quotes delimit untrusted input).
#[allow(clippy::unnecessary_debug_formatting)]
pub fn resolve_command_identity(
    raw_command: &str,
    cwd: &Path,
    path_env: Option<&std::ffi::OsStr>,
) -> Result<StoredExecutionIdentity, String> {
    let trimmed = raw_command.trim();
    if trimmed.is_empty() {
        return Err("command is empty".to_string());
    }
    let raw_path = Path::new(trimmed);
    let candidate = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else if trimmed.contains('/') || trimmed.contains('\\') {
        cwd.join(raw_path)
    } else {
        let dirs = path_env.map_or_else(Vec::new, |os_str| {
            std::env::split_paths(os_str).collect::<Vec<_>>()
        });
        dirs.iter()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(raw_path))
            .find(|candidate| is_executable_file(candidate))
            .ok_or_else(|| format!("executable {trimmed:?} was not found on the current PATH"))?
    };

    let candidate: PathBuf = candidate
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    let resolved = candidate
        .canonicalize()
        .map_err(|err| format!("cannot resolve command target {candidate:?}: {err}"))?;
    let meta = std::fs::metadata(&resolved)
        .map_err(|err| format!("cannot inspect resolved command {resolved:?}: {err}"))?;
    if !meta.is_file() {
        return Err(format!(
            "resolved command {resolved:?} is not a regular file"
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&resolved).map_or(0, |m| m.permissions().mode());
        if mode & 0o111 == 0 {
            return Err(format!(
                "resolved command {resolved:?} exists but is not executable"
            ));
        }
    }

    Ok(StoredExecutionIdentity {
        resolved_path: resolved.display().to_string(),
        content_sha256: sha256_file(&resolved)?,
    })
}

impl ConfiguredServer {
    /// Derive the execution identity this configuration will run RIGHT NOW.
    ///
    /// Returns `Ok(None)` for HTTP servers (nothing local executes), and an
    /// error when a stdio command cannot be resolved to exactly one regular
    /// executable (bd-sp5o3).
    ///
    /// # Errors
    ///
    /// Fails when a stdio command resolves to nothing executable or its
    /// canonical target/content cannot be hashed.
    pub fn execution_identity(
        &self,
        effective_cwd: &Path,
    ) -> Result<Option<StoredExecutionIdentity>, String> {
        let Some(command) = self.command.as_deref() else {
            return Ok(None);
        };
        resolve_command_identity(command, effective_cwd, std::env::var_os("PATH").as_deref())
            .map(Some)
    }
}

pub(super) fn validate_transport_shape(
    config: &ConfiguredServer,
) -> std::result::Result<(), String> {
    validate_timeout(config.timeout_ms)?;
    if let Some(cwd) = &config.cwd
        && (cwd.as_os_str().is_empty() || cwd.to_string_lossy().chars().any(is_terminal_control))
    {
        return Err("cwd must be a nonempty path without control characters".to_string());
    }
    let hint = config.transport_hint.as_deref().map(str::trim);
    if hint.is_some_and(str::is_empty) {
        return Err("transport type must not be empty".to_string());
    }
    match (config.command.as_deref(), config.url.as_deref()) {
        (Some(_), Some(_)) => Err(
            "defines both command and url; exactly one transport target is required".to_string(),
        ),
        (None, None) => Err("must define exactly one of command or url".to_string()),
        (Some(command), None) => {
            if command.trim().is_empty() {
                return Err("stdio command must not be empty".to_string());
            }
            if !matches!(hint, None | Some("stdio")) {
                return Err(format!(
                    "stdio command is incompatible with transport type {:?}",
                    config.transport_hint.as_deref().unwrap_or_default()
                ));
            }
            if !config.headers.is_empty() {
                return Err("stdio transport must not define HTTP headers".to_string());
            }
            Ok(())
        }
        (None, Some(url)) => {
            if url.trim().is_empty() {
                return Err("HTTP url must not be empty".to_string());
            }
            if !matches!(hint, None | Some("http" | "sse" | "streamable-http")) {
                return Err(format!(
                    "HTTP url is incompatible with transport type {:?}",
                    config.transport_hint.as_deref().unwrap_or_default()
                ));
            }
            if !config.args.is_empty() || !config.env.is_empty() {
                return Err("HTTP transport must not define stdio args or env".to_string());
            }
            Ok(())
        }
    }
}

fn canonical_identity(path: &Path, base: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            canonical_cwd(base).join(path)
        }
    })
}

fn canonical_cwd(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_or_else(|_| path.to_path_buf(), |current| current.join(path))
        }
    })
}

pub(super) fn normalize_http_headers(
    mut headers: Vec<(String, String)>,
) -> std::result::Result<Vec<(String, String)>, String> {
    if headers.len() > MAX_MCP_CUSTOM_HTTP_HEADERS {
        return Err(format!(
            "defines {} custom HTTP headers; at most {MAX_MCP_CUSTOM_HTTP_HEADERS} are allowed",
            headers.len()
        ));
    }
    for (name, value) in &headers {
        if name.is_empty()
            || name.len() > MAX_MCP_HEADER_NAME_BYTES
            || !name.bytes().all(is_http_token_byte)
        {
            return Err(format!(
                "defines invalid HTTP header name {name:?}; names must be 1 to {MAX_MCP_HEADER_NAME_BYTES} ASCII token bytes"
            ));
        }
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "accept" | "content-type" | "mcp-protocol-version" | "mcp-session-id"
        ) {
            return Err(format!(
                "defines transport-owned HTTP header {name:?}; Accept, Content-Type, Mcp-Protocol-Version, and Mcp-Session-Id are managed by the MCP transport"
            ));
        }
        validate_http_header_value(value).map_err(|reason| format!("header {name:?}: {reason}"))?;
    }
    headers.sort_by(|left, right| {
        left.0
            .to_ascii_lowercase()
            .cmp(&right.0.to_ascii_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });
    for duplicate in headers.windows(2) {
        if duplicate[0].0.eq_ignore_ascii_case(&duplicate[1].0) {
            return Err(format!(
                "defines duplicate HTTP header names {:?} and {:?} (header names are case-insensitive)",
                duplicate[0].0, duplicate[1].0
            ));
        }
    }
    Ok(headers)
}

pub(super) fn normalize_env(
    mut env: Vec<(String, String)>,
) -> std::result::Result<Vec<(String, String)>, String> {
    if env.len() > MAX_MCP_ENV_ENTRIES {
        return Err(format!(
            "defines {} environment entries; at most {MAX_MCP_ENV_ENTRIES} are allowed",
            env.len()
        ));
    }
    for (name, value) in &env {
        let mut bytes = name.bytes();
        let valid_start = bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
        if !valid_start
            || name.len() > MAX_MCP_ENV_NAME_BYTES
            || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(format!(
                "defines invalid environment name {name:?}; names must be 1 to {MAX_MCP_ENV_NAME_BYTES} ASCII identifier bytes"
            ));
        }
        validate_env_value(value).map_err(|reason| format!("environment {name:?}: {reason}"))?;
    }
    env.sort_by(|left, right| {
        left.0
            .to_ascii_lowercase()
            .cmp(&right.0.to_ascii_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });
    for duplicate in env.windows(2) {
        if duplicate[0].0.eq_ignore_ascii_case(&duplicate[1].0) {
            return Err(format!(
                "defines duplicate environment names {:?} and {:?} (environment names alias case-insensitively on Windows)",
                duplicate[0].0, duplicate[1].0
            ));
        }
    }
    Ok(env)
}

pub(super) fn validate_server_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() || name.len() > 128 {
        return Err("server name must contain 1 to 128 ASCII characters".to_string());
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(
            "server name may contain only ASCII letters, digits, '.', '-', and '_'".to_string(),
        );
    }
    Ok(())
}

fn hash_part(hasher: &mut Sha256, label: &str, value: &[u8]) {
    hasher.update(u64::try_from(label.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(label.as_bytes());
    hasher.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    hasher.update(value);
}

fn hash_optional(hasher: &mut Sha256, label: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            hash_part(hasher, &format!("{label}.state"), b"present");
            hash_part(hasher, label, value.as_bytes());
        }
        None => hash_part(hasher, &format!("{label}.state"), b"absent"),
    }
}

fn hash_strings(hasher: &mut Sha256, label: &str, values: &[String]) {
    hash_part(
        hasher,
        &format!("{label}.count"),
        &u64::try_from(values.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for value in values {
        hash_part(hasher, &format!("{label}.item"), value.as_bytes());
    }
}

// if let reads clearer here
#[allow(clippy::option_if_let_else)]
fn hash_definitions(
    hasher: &mut Sha256,
    label: &str,
    entries: &[(String, String)],
    normalize_names: bool,
) {
    let mut sorted: Vec<_> = entries
        .iter()
        .map(|(name, value)| {
            let name = if normalize_names {
                name.to_ascii_lowercase()
            } else {
                name.clone()
            };
            (name, value.as_str())
        })
        .collect();
    sorted.sort_unstable();
    hash_part(
        hasher,
        &format!("{label}.count"),
        &u64::try_from(sorted.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for (name, raw) in sorted {
        let (kind, definition) = if let Some(name) = raw.strip_prefix("$ENV:") {
            ("env_ref", name)
        } else if let Some(command) = raw.strip_prefix("$CMD:") {
            ("command_ref", command)
        } else {
            ("literal", raw)
        };
        hash_part(hasher, &format!("{label}.name"), name.as_bytes());
        hash_part(hasher, &format!("{label}.value_kind"), kind.as_bytes());
        hash_part(
            hasher,
            &format!("{label}.value_definition"),
            definition.as_bytes(),
        );
    }
}

/// A skipped entry, surfaced in `/mcp` and logs instead of aborting.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWarning {
    pub source_file: PathBuf,
    pub entry: String,
    pub reason: String,
}

/// Every field can carry workspace-controlled text (a rejected server name,
/// a parser message quoting file contents), so the rendering escapes control
/// and bidi characters instead of letting them reach the terminal.
impl std::fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} ({})",
            display_safe(&self.source_file.display().to_string()),
            display_safe(&self.entry),
            display_safe(&self.reason)
        )
    }
}

/// Escape control characters and Unicode bidi controls so remote or
/// workspace-controlled text cannot move the cursor, recolor, or visually
/// reorder the surrounding terminal output.
pub(crate) fn display_safe(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for ch in text.chars() {
        let bidi_control = matches!(ch, '\u{061C}' | '\u{200E}' | '\u{200F}')
            || ('\u{202A}'..='\u{202E}').contains(&ch)
            || ('\u{2066}'..='\u{2069}').contains(&ch);
        if ch.is_control() || bidi_control {
            output.extend(ch.escape_unicode());
        } else {
            output.push(ch);
        }
    }
    output
}

/// One raw server entry (tolerant: unknown fields ignored).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawServer {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    #[serde(default, rename = "type")]
    transport: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "timeout", alias = "timeoutMs", alias = "timeout_ms")]
    timeout_ms: Option<u64>,
}

/// Parse one server entry; `Err` carries the skip reason.
fn parse_server(name: &str, raw: &Value) -> std::result::Result<RawServer, String> {
    for key in [
        "enabled",
        "disabled",
        "cwd",
        "timeout",
        "timeoutMs",
        "timeout_ms",
    ] {
        if raw.get(key).is_some_and(Value::is_null) {
            return Err(format!("entry {name:?}: field {key:?} must not be null"));
        }
    }
    let server: RawServer =
        serde_json::from_value(raw.clone()).map_err(|err| format!("entry {name:?}: {err}"))?;
    validate_timeout(server.timeout_ms)?;
    Ok(server)
}

pub(super) fn validate_timeout(timeout_ms: Option<u64>) -> std::result::Result<(), String> {
    if timeout_ms.is_some_and(|value| !(1..=MAX_MCP_TIMEOUT_MS).contains(&value)) {
        return Err(format!(
            "timeout must be an integer from 1 to {MAX_MCP_TIMEOUT_MS} milliseconds; zero/unbounded timeouts are not supported"
        ));
    }
    Ok(())
}

pub(super) fn configured_enabled(value: &Value) -> Result<bool, String> {
    let flag = |key: &str| -> Result<Option<bool>, String> {
        value
            .get(key)
            .map(|flag| {
                flag.as_bool()
                    .ok_or_else(|| format!("field {key:?} must be a boolean"))
            })
            .transpose()
    };
    Ok(flag("enabled")? != Some(false) && flag("disabled")? != Some(true))
}

pub(super) fn configured_timeout(value: &Value) -> Result<Option<u64>, String> {
    let mut timeout = None;
    for key in ["timeout", "timeoutMs", "timeout_ms"] {
        if let Some(value) = value.get(key) {
            if timeout.is_some() {
                return Err("timeout aliases must not be combined".to_string());
            }
            timeout = Some(
                value
                    .as_u64()
                    .ok_or_else(|| "timeout must be an integer in milliseconds".to_string())?,
            );
        }
    }
    validate_timeout(timeout)?;
    Ok(timeout)
}

/// Expand OMP `${VAR}` / `${VAR:-default}` and VS Code `${env:VAR}`.
/// Non-secret fields expand before trust; env/header values use this same
/// parser only at the already-trusted transport-construction boundary.
pub(super) fn expand_config_value(raw: &str, cwd: &Path) -> Result<String, String> {
    expand_config_value_with(raw, cwd, |name| std::env::var(name).ok())
}

fn expand_config_value_with(
    raw: &str,
    cwd: &Path,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Result<String, String> {
    let mut output = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let expression = &rest[start + 2..];
        let Some(end) = expression.find('}') else {
            return Err("unterminated MCP configuration variable".to_string());
        };
        let expression = &expression[..end];
        let value = if expression == "workspaceFolder" {
            cwd.to_string_lossy().into_owned()
        } else {
            let expression = expression.strip_prefix("env:").unwrap_or(expression);
            let (name, fallback) = expression
                .split_once(":-")
                .map_or((expression, None), |(name, fallback)| {
                    (name, Some(fallback))
                });
            let mut bytes = name.bytes();
            if !bytes
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == b'_')
                || !bytes.all(|ch| ch.is_ascii_alphanumeric() || ch == b'_')
            {
                return Err("unsupported MCP configuration variable; use ${VAR}, ${VAR:-default}, ${env:VAR}, or ${workspaceFolder}".to_string());
            }
            lookup(name)
                .filter(|value| !value.is_empty())
                .or_else(|| fallback.map(str::to_string))
                .ok_or_else(|| {
                    format!("MCP configuration environment variable {name:?} is unset or empty")
                })?
        };
        if output.len().saturating_add(value.len()) > MAX_MCP_ENV_VALUE_BYTES {
            return Err("expanded MCP configuration value exceeds 65536 bytes".to_string());
        }
        output.push_str(&value);
        rest = &rest[start + 2 + end + 1..];
    }
    if output.len().saturating_add(rest.len()) > MAX_MCP_ENV_VALUE_BYTES {
        return Err("expanded MCP configuration value exceeds 65536 bytes".to_string());
    }
    output.push_str(rest);
    Ok(output)
}

#[derive(Default)]
struct ServerSelection {
    disabled: std::collections::HashSet<String>,
    disabled_sources: HashMap<String, (Provenance, PathBuf)>,
    enabled: std::collections::HashSet<String>,
    invalid: bool,
}

impl ServerSelection {
    fn load(
        &mut self,
        root: &serde_json::Map<String, Value>,
        provenance: Provenance,
        path: &Path,
    ) -> Result<(), String> {
        for (key, destination) in [
            ("disabledServers", &mut self.disabled),
            ("enabledServers", &mut self.enabled),
        ] {
            let Some(value) = root.get(key) else {
                continue;
            };
            let names = value
                .as_array()
                .ok_or_else(|| format!("{key} must be an array of server names"))?;
            for name in names {
                let name = name
                    .as_str()
                    .ok_or_else(|| format!("{key} must contain only server names"))?;
                validate_server_name(name)?;
                destination.insert(name.to_string());
                if key == "disabledServers" {
                    self.disabled_sources
                        .entry(name.to_string())
                        .or_insert_with(|| (provenance, path.to_path_buf()));
                }
            }
        }
        Ok(())
    }
}

fn read_bounded_config(path: &Path) -> std::io::Result<String> {
    let not_regular = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "MCP config path is not a regular file",
        )
    };
    if !std::fs::metadata(path)?.is_file() {
        return Err(not_regular());
    }
    // The path can be replaced between the metadata check and the open.
    // NONBLOCK keeps a swapped-in FIFO from hanging startup before the
    // descriptor itself is checked below.
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    let file = {
        use rustix::fs::{Mode, OFlags};
        File::from(rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?)
    };
    #[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(not_regular());
    }
    let mut bytes = Vec::new();
    file.take((MAX_MCP_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_MCP_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("MCP config exceeds {MAX_MCP_CONFIG_BYTES} bytes"),
        ));
    }
    String::from_utf8(bytes).map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("MCP config is not valid UTF-8: {err}"),
        )
    })
}

/// Parse the file envelope without interpreting individual server definitions.
fn parse_server_map(
    path: &Path,
    provenance: Provenance,
    content: &str,
    selection: &mut ServerSelection,
) -> Result<HashMap<String, Value>, String> {
    // Use the TOML parser so booleans, nested env/header tables, quoted names,
    // multiline arrays and comments keep their meaning during Codex import.
    let value = if path.extension().is_some_and(|e| e == "toml") {
        parse_codex_toml(content)?
    } else {
        serde_json::from_str(content).map_err(|err| format!("invalid JSON: {err}"))?
    };
    // Accept both `{"mcpServers": {...}}` and a bare `{name: {...}}` map.
    // Presence of the wrapper is authoritative even when it is empty; never
    // reinterpret unrelated top-level settings as a fallback server map.
    let root = value
        .as_object()
        .ok_or_else(|| "MCP config root must be an object".to_string())?;
    // Only operator-selected and native user files may override another
    // source's enable flag. A lower-priority project's enable list cannot
    // authorize a disabled global definition. Deny always wins over enable.
    if matches!(provenance, Provenance::Cli | Provenance::GlobalPi)
        && let Err(reason) = selection.load(root, provenance, path)
    {
        selection.invalid = true;
        return Err(reason);
    }
    let vscode = path.ends_with(Path::new(".vscode/mcp.json"));
    let wrapped = root.get("mcpServers").or_else(|| {
        if vscode {
            root.get("servers")
                .or_else(|| root.get("mcp").and_then(|mcp| mcp.get("servers")))
        } else {
            None
        }
    });
    if let Some(wrapped) = wrapped {
        let wrapped = wrapped.as_object().ok_or_else(|| {
            "MCP server map must be an object (mcpServers, or VS Code servers)".to_string()
        })?;
        Ok(wrapped.clone().into_iter().collect())
    } else if provenance.is_native() {
        // Native and explicit CLI files are MCP-only surfaces. Claim every
        // bare-map key before parsing so a malformed high-precedence override
        // cannot fall through to an older, already-trusted definition.
        Ok(root
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "$schema" | "disabledServers" | "enabledServers"
                )
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect())
    } else {
        // Foreign settings files share their root with unrelated settings, so
        // only object values carrying an MCP execution field are candidates.
        Ok(root
            .iter()
            .filter(|(_, value)| value.get("command").is_some() || value.get("url").is_some())
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect())
    }
}

/// Load one config file.
///
/// Returns `true` when a whole-file failure in a native or explicitly selected
/// source must block all lower-precedence sources. Missing default files are
/// normal; missing explicit CLI files fail closed.
fn load_file(
    path: &Path,
    provenance: Provenance,
    out: &mut Vec<(String, RawServer, Provenance, PathBuf)>,
    warnings: &mut Vec<ConfigWarning>,
    claimed_names: &mut std::collections::HashSet<String>,
    selection: &mut ServerSelection,
) -> bool {
    let content = match read_bounded_config(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && provenance != Provenance::Cli => {
            return false; // absent default files are normal
        }
        Err(err) => {
            warnings.push(ConfigWarning {
                source_file: path.to_path_buf(),
                entry: "<file>".to_string(),
                reason: format!("cannot read MCP config: {err}"),
            });
            return provenance.is_native();
        }
    };
    let servers = match parse_server_map(path, provenance, &content, selection) {
        Ok(servers) => servers,
        Err(reason) => {
            warnings.push(ConfigWarning {
                source_file: path.to_path_buf(),
                entry: "<file>".to_string(),
                reason,
            });
            return provenance.is_native();
        }
    };
    for (name, raw) in servers {
        // A higher-precedence definition owns its name even when malformed.
        // Falling through to an older, already-trusted lower definition would
        // turn a configuration error into unexpected code execution.
        if !claimed_names.insert(name.clone()) {
            continue;
        }
        match parse_server(&name, &raw) {
            Ok(server) => out.push((name, server, provenance, path.to_path_buf())),
            Err(reason) => {
                // A disabled entry still owns its name when another field is
                // malformed. Keep an inert reservation so later extension
                // registration cannot bypass that explicit disablement.
                if raw.get("enabled").and_then(Value::as_bool) == Some(false)
                    || raw.get("disabled").and_then(Value::as_bool) == Some(true)
                {
                    out.push((
                        name.clone(),
                        RawServer {
                            disabled: Some(true),
                            ..RawServer::default()
                        },
                        provenance,
                        path.to_path_buf(),
                    ));
                }
                warnings.push(ConfigWarning {
                    source_file: path.to_path_buf(),
                    entry: name,
                    reason,
                });
            }
        }
    }
    false
}

/// Extract only Codex's MCP tables from a fully parsed TOML document.
fn parse_codex_toml(content: &str) -> Result<Value, String> {
    let value: toml::Value =
        toml::from_str(content).map_err(|_| "invalid TOML in MCP configuration".to_string())?;
    let servers = value
        .get("mcp_servers")
        .cloned()
        .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
    let mut servers = serde_json::to_value(servers)
        .map_err(|_| "cannot decode Codex MCP configuration".to_string())?;
    if let Some(servers) = servers.as_object_mut() {
        for server in servers.values_mut().filter_map(Value::as_object_mut) {
            if let Some(headers) = server.remove("http_headers") {
                server.entry("headers").or_insert(headers);
            }
        }
    }
    Ok(serde_json::json!({"mcpServers": servers}))
}

/// Foreign discovery candidates under the project root.
const FOREIGN_PROJECT_FILES: &[&str] = &[
    ".omp/mcp.json",
    ".claude/mcp.json",
    ".claude/.mcp.json",
    ".cursor/mcp.json",
    ".windsurf/mcp.json",
    ".windsurf/mcp_config.json",
    ".gemini/settings.json",
    ".codex/config.toml",
    ".vscode/mcp.json",
    "mcp.json",
    ".mcp.json",
];

/// The merged discovery result.
#[derive(Debug, Default)]
pub struct McpDiscovery {
    /// Servers keyed by name, highest-precedence definition per name.
    pub servers: Vec<ConfiguredServer>,
    /// Non-fatal load problems (malformed entries/files).
    pub warnings: Vec<ConfigWarning>,
}

/// Discover and merge MCP server configs under the established
/// workspace-trust decision.
///
/// `cli_paths`: `--mcp-config` files (repeatable, highest precedence).
/// `global_dir`: the pi global agent dir (`~/.pi/agent`).
///
/// When `project_trusted` is false, project-native and foreign project files
/// are skipped without being opened. Explicit `--mcp-config` paths and the
/// global Pi config remain eligible because neither is discovered from the
/// untrusted workspace. The decision is a required argument so no caller can
/// reach project configuration by omitting it.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn discover(
    cwd: &Path,
    global_dir: &Path,
    cli_paths: &[PathBuf],
    project_trusted: bool,
) -> McpDiscovery {
    let mut layered: Vec<(String, RawServer, Provenance, PathBuf)> = Vec::new();
    let mut warnings = Vec::new();
    let mut claimed_names = std::collections::HashSet::new();
    let mut selection = ServerSelection::default();

    // Precedence high → low. Later layers only fill names not already set. A
    // whole-file failure in a native or explicit source blocks every lower
    // layer; otherwise an invalid override could revive an older trusted
    // command or URL.
    let mut lower_layers_blocked = false;
    for path in cli_paths {
        if lower_layers_blocked {
            break;
        }
        lower_layers_blocked = load_file(
            path,
            Provenance::Cli,
            &mut layered,
            &mut warnings,
            &mut claimed_names,
            &mut selection,
        );
    }
    if !lower_layers_blocked && project_trusted {
        lower_layers_blocked = load_file(
            &cwd.join(".pi/mcp.json"),
            Provenance::ProjectPi,
            &mut layered,
            &mut warnings,
            &mut claimed_names,
            &mut selection,
        );
    }
    if !lower_layers_blocked && project_trusted {
        lower_layers_blocked = load_file(
            &cwd.join(".agents/mcp.json"),
            Provenance::ProjectAgents,
            &mut layered,
            &mut warnings,
            &mut claimed_names,
            &mut selection,
        );
    }
    if !lower_layers_blocked {
        lower_layers_blocked = load_file(
            &global_dir.join("mcp.json"),
            Provenance::GlobalPi,
            &mut layered,
            &mut warnings,
            &mut claimed_names,
            &mut selection,
        );
    }
    if !lower_layers_blocked && project_trusted {
        for foreign in FOREIGN_PROJECT_FILES {
            load_file(
                &cwd.join(foreign),
                Provenance::Foreign,
                &mut layered,
                &mut warnings,
                &mut claimed_names,
                &mut selection,
            );
        }
    }

    let mut servers = Vec::new();
    if selection.invalid {
        return McpDiscovery { servers, warnings };
    }
    for (name, raw, provenance, source_file) in layered {
        if let Err(reason) = validate_server_name(&name) {
            warnings.push(ConfigWarning {
                source_file,
                entry: name,
                reason,
            });
            continue;
        }
        let enabled = !selection.disabled.contains(&name)
            && raw.disabled != Some(true)
            && (raw.enabled != Some(false) || selection.enabled.contains(&name));
        if !enabled {
            // Keep the name occupied even without an executable, credentials,
            // or a currently available working directory. Neither a lower
            // file nor an extension may revive a disabled definition.
            servers.push(ConfiguredServer {
                name,
                command: raw.command,
                args: raw.args.unwrap_or_default(),
                env: Vec::new(),
                url: raw.url,
                headers: Vec::new(),
                transport_hint: raw.transport,
                enabled: false,
                cwd: None,
                timeout_ms: raw.timeout_ms,
                provenance,
                source_file,
            });
            continue;
        }
        let headers =
            match normalize_http_headers(raw.headers.unwrap_or_default().into_iter().collect()) {
                Ok(headers) => headers,
                Err(reason) => {
                    warnings.push(ConfigWarning {
                        source_file,
                        entry: name,
                        reason,
                    });
                    continue;
                }
            };
        let env = match normalize_env(raw.env.unwrap_or_default().into_iter().collect()) {
            Ok(env) => env,
            Err(reason) => {
                warnings.push(ConfigWarning {
                    source_file,
                    entry: name,
                    reason,
                });
                continue;
            }
        };
        let execution = (|| -> Result<_, String> {
            let command = raw
                .command
                .as_deref()
                .map(|value| expand_config_value(value, cwd))
                .transpose()?;
            let args = raw
                .args
                .unwrap_or_default()
                .iter()
                .map(|value| expand_config_value(value, cwd))
                .collect::<Result<Vec<_>, _>>()?;
            let url = raw
                .url
                .as_deref()
                .map(|value| expand_config_value(value, cwd))
                .transpose()?;
            let server_cwd = raw
                .cwd
                .as_deref()
                .map(|value| expand_config_value(value, cwd))
                .transpose()?;
            let server_cwd = server_cwd
                .map(|value| {
                    if value.is_empty() {
                        return Err("cwd must not be empty".to_string());
                    }
                    let path = PathBuf::from(value);
                    Ok(canonical_cwd(&if path.is_absolute() {
                        path
                    } else {
                        cwd.join(path)
                    }))
                })
                .transpose()?;
            Ok((command, args, url, server_cwd))
        })();
        let (command, args, url, server_cwd) = match execution {
            Ok(values) => values,
            Err(reason) => {
                warnings.push(ConfigWarning {
                    source_file,
                    entry: name,
                    reason,
                });
                continue;
            }
        };
        let config = ConfiguredServer {
            name,
            command,
            args,
            env,
            url,
            headers,
            transport_hint: raw.transport,
            enabled,
            cwd: server_cwd,
            timeout_ms: raw.timeout_ms,
            provenance,
            source_file,
        };
        if let Err(reason) = validate_transport_shape(&config) {
            warnings.push(ConfigWarning {
                source_file: config.source_file.clone(),
                entry: config.name.clone(),
                reason,
            });
            continue;
        }
        servers.push(config);
    }
    // The denylist also applies to extension definitions registered after
    // discovery, even when no file currently defines that server name.
    for (name, (provenance, source_file)) in selection.disabled_sources {
        if !servers.iter().any(|server| server.name == name) {
            servers.push(ConfiguredServer {
                name,
                command: None,
                args: Vec::new(),
                env: Vec::new(),
                url: None,
                headers: Vec::new(),
                transport_hint: None,
                enabled: false,
                cwd: None,
                timeout_ms: None,
                provenance,
                source_file,
            });
        }
    }
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    McpDiscovery { servers, warnings }
}

/// Write-side view of a project-native config file (for `/mcp add|remove`):
/// read-modify-write `.pi/mcp.json` preserving unrelated content.
///
/// # Errors
///
/// Returns an error when the file exists but is not valid JSON.
pub fn read_project_config(path: &Path) -> Result<Value, crate::error::Error> {
    match read_bounded_config(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|err| {
            crate::error::Error::tool(
                "mcp",
                format!("[MCP_CONFIG_INVALID] {}: {err}", path.display()),
            )
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::json!({ "mcpServers": {} }))
        }
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
            Err(crate::error::Error::tool(
                "mcp",
                format!("[MCP_CONFIG_INVALID] {}: {err}", path.display()),
            ))
        }
        Err(err) => Err(crate::error::Error::tool(
            "mcp",
            format!("[MCP_CONFIG_IO] cannot read {}: {err}", path.display()),
        )),
    }
}

/// Insert or replace `name` in a project config's `mcpServers` object.
///
/// A missing or `null` `mcpServers` member (and a `null` document) starts
/// empty, as before.
///
/// # Errors
///
/// Refuses a config whose top level or whose `mcpServers` member is some
/// other non-object value. Indexing such a [`Value`] by key panics, and
/// replacing it would discard what the user wrote.
pub fn insert_project_server(
    config: &mut Value,
    name: &str,
    entry: Value,
) -> Result<(), crate::error::Error> {
    let not_an_object = |what: &str| {
        crate::error::Error::tool(
            "mcp",
            format!("[MCP_CONFIG_INVALID] {what} must be a JSON object"),
        )
    };
    if config.is_null() {
        *config = Value::Object(serde_json::Map::new());
    }
    let servers = config
        .as_object_mut()
        .ok_or_else(|| not_an_object("the project MCP config"))?
        .entry("mcpServers")
        .or_insert(Value::Null);
    if servers.is_null() {
        *servers = Value::Object(serde_json::Map::new());
    }
    servers
        .as_object_mut()
        .ok_or_else(|| not_an_object("`mcpServers`"))?
        .insert(name.to_string(), entry);
    Ok(())
}

/// Remove `name` from a project config's `mcpServers` object.
///
/// Returns whether an entry was removed. A config whose top level or whose
/// `mcpServers` member is not an object has no such server and is left
/// untouched: it is never indexed, because indexing a non-object [`Value`]
/// by key panics.
pub fn remove_project_server(config: &mut Value, name: &str) -> bool {
    config
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .is_some_and(|servers| servers.remove(name).is_some())
}

/// Write the project config back (pretty JSON, parent dirs created).
///
/// # Errors
///
/// Returns an error on I/O failure.
pub fn write_project_config(path: &Path, value: &Value) -> Result<(), crate::error::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            crate::error::Error::tool(
                "mcp",
                format!("[MCP_CONFIG_IO] cannot create {}: {err}", parent.display()),
            )
        })?;
    }
    let rendered = serde_json::to_string_pretty(value).map_err(|err| {
        crate::error::Error::tool("mcp", format!("[MCP_CONFIG_IO] serialize failed: {err}"))
    })?;
    if rendered.len().saturating_add(1) > MAX_MCP_CONFIG_BYTES {
        return Err(crate::error::Error::tool(
            "mcp",
            format!(
                "[MCP_CONFIG_INVALID] rendered MCP config exceeds {MAX_MCP_CONFIG_BYTES} bytes"
            ),
        ));
    }
    std::fs::write(path, format!("{rendered}\n")).map_err(|err| {
        crate::error::Error::tool(
            "mcp",
            format!("[MCP_CONFIG_IO] cannot write {}: {err}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("dirs");
        }
        std::fs::write(path, content).expect("write");
    }

    #[test]
    fn project_beats_global_and_foreign_fills_gaps() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"shared": {"command": "project-cmd"}, "only_project": {"command": "p2"}}}"#,
        );
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers": {"shared": {"command": "global-cmd"}, "only_global": {"command": "g2"}}}"#,
        );
        write(
            &cwd.join(".claude/mcp.json"),
            r#"{"mcpServers": {"foreign_one": {"command": "f1"}, "only_project": {"command": "shadowed"}}}"#,
        );
        let discovery = discover(&cwd, &global, &[], true);
        let by_name: HashMap<_, _> = discovery
            .servers
            .iter()
            .map(|s| (s.name.as_str(), s))
            .collect();
        assert_eq!(
            by_name["shared"].command.as_deref(),
            Some("project-cmd"),
            "project must beat global"
        );
        assert_eq!(by_name["shared"].provenance, Provenance::ProjectPi);
        assert_eq!(by_name["only_global"].command.as_deref(), Some("g2"));
        assert_eq!(by_name["foreign_one"].provenance, Provenance::Foreign);
        // Native definition shadows the foreign duplicate entirely.
        assert_eq!(
            by_name["only_project"].command.as_deref(),
            Some("p2"),
            "native must shadow foreign"
        );
    }

    #[test]
    fn cli_beats_everything() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        let cli = temp.path().join("cli.json");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"s": {"command": "project"}}}"#,
        );
        write(&cli, r#"{"mcpServers": {"s": {"command": "cli"}}}"#);
        let discovery = discover(&cwd, &global, &[cli], true);
        assert_eq!(discovery.servers.len(), 1);
        assert_eq!(discovery.servers[0].command.as_deref(), Some("cli"));
        assert_eq!(discovery.servers[0].provenance, Provenance::Cli);
    }

    #[test]
    fn malformed_entries_skip_and_warn_never_abort() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"good": {"command": "ok"}, "bad": 42}}"#,
        );
        let discovery = discover(&cwd, &global, &[], true);
        assert_eq!(discovery.servers.len(), 1, "good entry survives");
        assert_eq!(discovery.warnings.len(), 1, "bad entry warned");
        assert!(discovery.warnings[0].reason.contains("\"bad\""));
    }

    #[test]
    fn malformed_higher_precedence_entries_shadow_lower_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers":{
                "bad-shape":{"command":7},
                "bad-header":{"url":"https://project.invalid","headers":{"Bad Header":"x"}},
                "bad-env":{"command":"project","env":{"BAD-NAME":"x"}}
            }}"#,
        );
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers":{
                "bad-shape":{"command":"global"},
                "bad-header":{"command":"global"},
                "bad-env":{"command":"global"}
            }}"#,
        );

        let discovery = discover(&cwd, &global, &[], true);
        assert!(
            discovery.servers.is_empty(),
            "a lower-precedence trusted target must not replace a malformed override"
        );
        assert_eq!(discovery.warnings.len(), 3);
    }

    #[test]
    fn malformed_native_bare_map_entry_shadows_lower_server() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        write(&cwd.join(".pi/mcp.json"), r#"{"shared":42}"#);
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers":{"shared":{"command":"global"}}}"#,
        );

        let discovery = discover(&cwd, &global, &[], true);
        assert!(
            discovery.servers.is_empty(),
            "a malformed bare-map override must still own its server name"
        );
        assert_eq!(discovery.warnings.len(), 1);
        assert_eq!(discovery.warnings[0].entry, "shared");
    }

    #[test]
    fn ambiguous_transport_shapes_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers":{
                "both":{"command":"server","url":"https://example.invalid"},
                "url-stdio":{"url":"https://example.invalid","type":"stdio"},
                "command-http":{"command":"server","type":"http"},
                "unknown":{"command":"server","type":"other"},
                "http-args":{"url":"https://example.invalid","args":["ignored"]},
                "stdio-headers":{"command":"server","headers":{"X-Test":"ignored"}}
            }}"#,
        );

        let discovery = discover(&cwd, &temp.path().join("global"), &[], true);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.warnings.len(), 6);
    }

    #[test]
    fn malformed_mcp_servers_wrapper_warns_instead_of_becoming_a_bare_map() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers":[],"unrelated":{"command":"must-not-run"}}"#,
        );

        let discovery = discover(&cwd, &temp.path().join("global"), &[], true);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert!(discovery.warnings[0].reason.contains("mcpServers"));
    }

    #[test]
    fn malformed_native_file_blocks_lower_precedence_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        write(&cwd.join(".pi/mcp.json"), "{not json");
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers": {"g": {"command": "ok"}}}"#,
        );
        let discovery = discover(&cwd, &global, &[], true);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
    }

    #[test]
    fn non_regular_native_config_blocks_lower_precedence_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        std::fs::create_dir_all(cwd.join(".pi/mcp.json")).expect("non-regular config fixture");
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers":{"fallback":{"command":"ok"}}}"#,
        );

        let discovery = discover(&cwd, &global, &[], true);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert_eq!(discovery.warnings[0].source_file, cwd.join(".pi/mcp.json"));
        assert!(
            discovery.warnings[0]
                .reason
                .contains("cannot read MCP config")
        );
    }

    #[test]
    fn oversized_native_config_blocks_lower_precedence_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        let project_config = cwd.join(".pi/mcp.json");
        std::fs::create_dir_all(project_config.parent().expect("config parent"))
            .expect("project config directory");
        std::fs::write(&project_config, vec![b' '; MAX_MCP_CONFIG_BYTES + 1])
            .expect("oversized config fixture");
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers":{"fallback":{"command":"ok"}}}"#,
        );

        let discovery = discover(&cwd, &global, &[], true);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert!(
            discovery.warnings[0]
                .reason
                .contains("exceeds 1048576 bytes")
        );
    }

    #[test]
    fn missing_explicit_cli_config_blocks_lower_precedence_servers() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let global = temp.path().join("global");
        let missing = temp.path().join("explicit-missing.json");
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers":{"fallback":{"command":"must-not-run"}}}"#,
        );

        let discovery = discover(&cwd, &global, std::slice::from_ref(&missing), true);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert_eq!(discovery.warnings[0].source_file, missing);
        assert!(
            discovery.warnings[0]
                .reason
                .contains("cannot read MCP config")
        );
    }

    #[test]
    fn bare_map_form_accepted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"myserver": {"command": "bare-form"}}"#,
        );
        let discovery = discover(&cwd, &temp.path().join("g"), &[], true);
        assert_eq!(discovery.servers.len(), 1);
        assert_eq!(discovery.servers[0].command.as_deref(), Some("bare-form"));
    }

    #[test]
    fn codex_toml_tables_parsed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = \"docs-mcp\"\nargs = [\"--port\", \"8080\"]\n",
        );
        let discovery = discover(&cwd, &temp.path().join("g"), &[], true);
        assert_eq!(discovery.servers.len(), 1);
        let server = &discovery.servers[0];
        assert_eq!(server.name, "docs");
        assert_eq!(server.command.as_deref(), Some("docs-mcp"));
        assert_eq!(server.args, vec!["--port", "8080"]);
        assert_eq!(server.provenance, Provenance::Foreign);
    }

    #[test]
    fn http_shape_detected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"remote": {"url": "https://mcp.example.com/sse", "headers": {"Authorization": "$ENV:MCP_TOKEN"}}}}"#,
        );
        let discovery = discover(&cwd, &temp.path().join("g"), &[], true);
        assert!(discovery.servers[0].is_http());
        assert_eq!(discovery.servers[0].headers.len(), 1);
    }

    #[test]
    fn duplicate_case_insensitive_http_headers_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers":{"remote":{"url":"https://mcp.example.test","headers":{"Authorization":"first","authorization":"second"}}}}"#,
        );

        let discovery = discover(&cwd, &temp.path().join("g"), &[], true);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
        assert!(discovery.warnings[0].reason.contains("case-insensitive"));
    }

    #[test]
    fn terminal_control_server_names_are_rejected_during_discovery() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        write(
            &cwd.join(".pi/mcp.json"),
            &serde_json::json!({
                "mcpServers": {
                    "hostile\u{202e}name": {"command": "server"}
                }
            })
            .to_string(),
        );

        let discovery = discover(&cwd, &temp.path().join("g"), &[], true);
        assert!(discovery.servers.is_empty());
        assert!(discovery.warnings[0].reason.contains("ASCII"));
    }

    #[test]
    fn config_warning_display_escapes_terminal_and_bidi_controls() {
        let warning = ConfigWarning {
            source_file: PathBuf::from("/p/.pi/mcp.json"),
            entry: "evil\u{1b}[2J\u{202e}name".to_string(),
            reason: "line\nbreak".to_string(),
        };
        let rendered = warning.to_string();
        assert!(!rendered.chars().any(char::is_control), "{rendered:?}");
        assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
        assert!(rendered.contains(r"\u{1b}[2J\u{202e}name"), "{rendered}");
        assert!(rendered.contains(r"line\u{a}break"), "{rendered}");
        assert_eq!(
            display_safe("plain ascii — ünïcode"),
            "plain ascii — ünïcode"
        );
    }

    #[test]
    fn over_limit_http_headers_are_rejected_before_trust() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        let at_limit: Vec<(String, String)> = (0..MAX_MCP_CUSTOM_HTTP_HEADERS)
            .map(|index| (format!("X-Limit-{index}"), "value".to_string()))
            .collect();
        assert_eq!(
            normalize_http_headers(at_limit)
                .expect("the exact custom-header limit must be accepted")
                .len(),
            MAX_MCP_CUSTOM_HTTP_HEADERS
        );
        let headers: serde_json::Map<String, Value> = (0..=MAX_MCP_CUSTOM_HTTP_HEADERS)
            .map(|index| (format!("X-MCP-{index}"), Value::String("value".to_string())))
            .collect();
        write(
            &cwd.join(".pi/mcp.json"),
            &serde_json::json!({
                "mcpServers": {
                    "remote": {
                        "url": "https://mcp.example.test",
                        "headers": headers
                    }
                }
            })
            .to_string(),
        );

        let discovery = discover(&cwd, &temp.path().join("g"), &[], true);
        assert!(discovery.servers.is_empty());
        assert!(discovery.warnings[0].reason.contains("at most 96"));
    }

    #[test]
    fn invalid_reserved_or_control_bearing_definitions_are_rejected() {
        for headers in [
            serde_json::json!({"Bad\nName": "value"}),
            serde_json::json!({"Accept": "application/json"}),
            serde_json::json!({"Mcp-Protocol-Version": "2025-06-18"}),
            serde_json::json!({"X-Test": "line\nforge"}),
        ] {
            let values = headers
                .as_object()
                .expect("header object")
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        value.as_str().expect("header string").to_string(),
                    )
                })
                .collect();
            assert!(normalize_http_headers(values).is_err());
        }

        for env in [
            vec![("BAD\nNAME".to_string(), "value".to_string())],
            vec![("9INVALID".to_string(), "value".to_string())],
            vec![("VALID".to_string(), "line\nforge".to_string())],
            vec![
                ("PATH".to_string(), "first".to_string()),
                ("Path".to_string(), "second".to_string()),
            ],
        ] {
            assert!(normalize_env(env).is_err());
        }
    }

    #[test]
    fn fingerprint_binds_complete_execution_surface_and_is_order_stable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("proj");
        std::fs::create_dir_all(&cwd).expect("cwd");
        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"s": {"command": "a", "args": ["one"], "env": {"B": "$ENV:TOKEN", "A": "literal"}}}}"#,
        );
        let server = discover(&cwd, &temp.path().join("g"), &[], true)
            .servers
            .remove(0);
        let fingerprint = server.fingerprint(&cwd);
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));

        let mut reordered_env = server.clone();
        reordered_env.env.reverse();
        assert_eq!(fingerprint, reordered_env.fingerprint(&cwd));

        let mut changed_env = server.clone();
        changed_env
            .env
            .iter_mut()
            .find(|(name, _)| name == "B")
            .expect("B env definition")
            .1 = "$CMD:new-helper".to_string();
        assert_ne!(fingerprint, changed_env.fingerprint(&cwd));

        let mut changed_name = server.clone();
        changed_name.name = "other-name".to_string();
        assert_ne!(fingerprint, changed_name.fingerprint(&cwd));

        let mut changed_command = server.clone();
        changed_command.command = Some("other-command".to_string());
        assert_ne!(fingerprint, changed_command.fingerprint(&cwd));

        let mut changed_args = server.clone();
        changed_args.args.push("two".to_string());
        assert_ne!(fingerprint, changed_args.fingerprint(&cwd));

        write(
            &cwd.join(".pi/mcp.json"),
            r#"{"mcpServers": {"s": {"url": "https://mcp.example.test", "headers": {"X-Token": "$CMD:token-helper", "X-Accept-Mode": "application/json"}}}}"#,
        );
        let http_server = discover(&cwd, &temp.path().join("g"), &[], true)
            .servers
            .remove(0);
        let http_fingerprint = http_server.fingerprint(&cwd);

        let mut reordered_headers = http_server.clone();
        reordered_headers.headers.reverse();
        assert_eq!(http_fingerprint, reordered_headers.fingerprint(&cwd));

        let mut changed_header = http_server.clone();
        changed_header
            .headers
            .first_mut()
            .expect("header definition")
            .1 = "$ENV:OTHER_TOKEN".to_string();
        assert_ne!(http_fingerprint, changed_header.fingerprint(&cwd));

        let mut changed_url = http_server;
        changed_url.url = Some("https://other.example.test".to_string());
        assert_ne!(http_fingerprint, changed_url.fingerprint(&cwd));

        let mut changed_transport = server.clone();
        changed_transport.transport_hint = Some("stdio".to_string());
        assert_ne!(fingerprint, changed_transport.fingerprint(&cwd));

        let mut changed_provenance = server.clone();
        changed_provenance.provenance = Provenance::Foreign;
        assert_ne!(fingerprint, changed_provenance.fingerprint(&cwd));

        let other_source = cwd.join("other-mcp.json");
        write(&other_source, "{}");
        let mut changed_source = server.clone();
        changed_source.source_file = other_source;
        assert_ne!(fingerprint, changed_source.fingerprint(&cwd));

        let other_cwd = temp.path().join("other-project");
        std::fs::create_dir_all(&other_cwd).expect("other cwd");
        assert_ne!(fingerprint, server.fingerprint(&other_cwd));
    }

    /// bd-sp5o3 fixtures: an executable script with the unix exec bit set.
    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, body).expect("write fixture");
        let mut permissions = std::fs::metadata(path)
            .expect("fixture metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod fixture");
    }

    #[cfg(unix)]
    fn stdio_fixture(name: &str, command: &str) -> ConfiguredServer {
        ConfiguredServer {
            name: name.to_string(),
            command: Some(command.to_string()),
            args: vec!["--serve".to_string()],
            env: Vec::new(),
            url: None,
            headers: Vec::new(),
            transport_hint: None,
            enabled: true,
            cwd: None,
            timeout_ms: None,
            provenance: Provenance::GlobalPi,
            source_file: PathBuf::from("/tmp/whatever.json"),
        }
    }

    /// The path `resolve_command_identity` would report for `path`.
    ///
    /// Resolution canonicalizes, and on macOS a tempdir lives under `/var`,
    /// which is a symlink into `/private/var`, so comparing against the raw
    /// path fails there and passes on Linux — the assertion was testing the
    /// host's filesystem layout rather than PATH order.
    #[cfg(unix)]
    fn canonical_string(path: &std::path::Path) -> String {
        let resolved = std::fs::canonicalize(path).expect("canonicalize fixture path"); // ubs:ignore a panic is how a test reports a broken fixture
        resolved.display().to_string()
    }

    /// Bare commands resolve through explicit PATH contents in order; the
    /// current directory is never implicitly searched. Unix-only: the
    /// fixtures rely on the exec bit.
    #[cfg(unix)]
    #[test]
    fn bare_command_resolution_follows_path_order_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first_dir = temp.path().join("first");
        let second_dir = temp.path().join("second");
        let project_cwd = temp.path().join("project");
        for directory in [&first_dir, &second_dir, &project_cwd] {
            std::fs::create_dir_all(directory).expect("mkdir");
        }
        write_executable(&first_dir.join("svc"), "#!/bin/sh\necho first\n");
        write_executable(&second_dir.join("svc"), "#!/bin/sh\necho second\n");
        // Same name in the cwd must be ignored for bare lookups.
        write_executable(&project_cwd.join("svc"), "#!/bin/sh\necho cwd\n");

        let path_env = std::env::join_paths([&first_dir, &second_dir]).expect("join PATH");
        let identity = resolve_command_identity("svc", &project_cwd, Some(path_env.as_os_str()))
            .expect("bare resolution");
        assert_eq!(
            identity.resolved_path,
            canonical_string(&first_dir.join("svc")),
            "earlier PATH entry wins"
        );

        // Reordering the PATH selects different code under identical config.
        let flipped = std::env::join_paths([&second_dir, &first_dir]).expect("join PATH");
        let identity = resolve_command_identity("svc", &project_cwd, Some(flipped.as_os_str()))
            .expect("reordered resolution");
        assert_eq!(
            identity.resolved_path,
            canonical_string(&second_dir.join("svc"))
        );
    }

    /// Relative commands anchor to the effective cwd and FAIL when nothing
    /// executable is there.
    #[cfg(unix)]
    #[test]
    fn relative_command_resolution_anchors_to_cwd() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().join("proj");
        std::fs::create_dir_all(project.join("bin")).expect("mkdir bin");
        write_executable(&project.join("bin").join("tool"), "#!/bin/sh\nexit 0\n");

        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("mkdir elsewhere");

        let resolved_here =
            resolve_command_identity("./bin/tool", &project, Some(std::ffi::OsStr::new("")))
                .expect("relative resolution anchored to project cwd")
                .resolved_path;
        assert_eq!(
            resolved_here,
            std::fs::canonicalize(project.join("bin/tool"))
                .expect("canonical")
                .display()
                .to_string()
        );

        let error = resolve_command_identity("./bin/tool", &elsewhere, None)
            .expect_err("must not resolve outside the effective cwd");
        assert!(
            error.contains("cannot resolve") || error.contains("not a regular"),
            "{error}"
        );
    }

    /// In-place content replacement (same filename, similar size) MUST
    /// change the identity digest — the exact substitution the bead exists
    /// to catch between acknowledgement and spawn.
    #[cfg(unix)]
    #[test]
    fn content_mutation_changes_identity_even_in_place() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let target = dir.join("tool");
        write_executable(&target, "#!/bin/sh\necho ORIGINAL_SAME_LENGTH_AAAA\n");

        let path_env = std::env::join_paths([&dir]).expect("join PATH");
        let before = resolve_command_identity("tool", temp.path(), Some(path_env.as_os_str()))
            .expect("initial");

        write_executable(&target, "#!/bin/sh\necho REPLACED_SAME_LENGTH_BBBB\n");
        let after = resolve_command_identity("tool", temp.path(), Some(path_env.as_os_str()))
            .expect("after rewrite");

        assert_eq!(before.resolved_path, after.resolved_path);
        assert_ne!(
            before.content_sha256, after.content_sha256,
            "content binding must expose in-place swaps"
        );
    }

    /// A symlink retarget changes the canonical path (and therefore the
    /// bound identity), independent of content hashing.
    #[cfg(unix)]
    #[test]
    fn symlink_retarget_changes_resolved_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let real_a = bin.join("real-a");
        let real_b = bin.join("real-b");
        write_executable(&real_a, "#!/bin/sh\necho A\n");
        write_executable(&real_b, "#!/bin/sh\necho B\n");
        std::os::unix::fs::symlink(&real_a, bin.join("link")).expect("symlink a");

        let path_env = std::env::join_paths([&bin]).expect("join PATH");
        let before = resolve_command_identity("link", temp.path(), Some(path_env.as_os_str()))
            .expect("via link->a");

        std::fs::remove_file(bin.join("link")).expect("remove link");
        std::os::unix::fs::symlink(&real_b, bin.join("link")).expect("symlink b");
        let after = resolve_command_identity("link", temp.path(), Some(path_env.as_os_str()))
            .expect("via link->b");

        assert_ne!(
            before.resolved_path, after.resolved_path,
            "canonicalization exposes retargeted symlinks"
        );
    }

    /// Non-executable and missing targets fail closed with actionable text;
    /// HTTP servers carry no execution identity at all.
    #[cfg(unix)]
    #[test]
    fn unresolvable_commands_fail_closed_and_http_is_none() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("d");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("lazy"), "#!/bin/sh\nexit 0\n").expect("no exec bit");

        let path_env = std::env::join_paths([&dir]).expect("join PATH");
        let error = resolve_command_identity("lazy", temp.path(), Some(path_env.as_os_str()))
            .expect_err("missing exec bit fails closed");
        assert!(
            error.contains("not executable") || error.contains("not found"),
            "{error}"
        );

        let missing = resolve_command_identity("never-installed-binary", temp.path(), None)
            .expect_err("absent from PATH fails closed");
        assert!(
            missing.contains("not found on the current PATH"),
            "{missing}"
        );

        let mut http = ConfiguredServer {
            name: "remote".to_string(),
            command: None,
            args: Vec::new(),
            env: Vec::new(),
            url: Some("https://example.invalid/mcp".to_string()),
            headers: Vec::new(),
            transport_hint: None,
            enabled: true,
            cwd: None,
            timeout_ms: None,
            provenance: Provenance::GlobalPi,
            source_file: PathBuf::from("/tmp/remote.json"),
        };
        assert!(
            http.execution_identity(temp.path())
                .expect("http has no local surface")
                .is_none()
        );
        http.command = Some("./nowhere/tool".to_string());
        assert!(
            http.execution_identity(temp.path()).is_err(),
            "stdio-with-missing-target still fails closed"
        );
    }

    #[test]
    fn write_then_read_project_config_roundtrip() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".pi/mcp.json");
        let mut value = read_project_config(&path).expect("read absent");
        value["mcpServers"]["added"] = serde_json::json!({"command": "new-cmd"});
        write_project_config(&path, &value).expect("write");
        let reread = read_project_config(&path).expect("reread");
        assert_eq!(
            reread["mcpServers"]["added"]["command"].as_str(),
            Some("new-cmd")
        );
    }

    #[test]
    fn project_config_read_rejects_oversized_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".pi/mcp.json");
        std::fs::create_dir_all(path.parent().expect("config parent")).expect("config directory");
        std::fs::write(&path, vec![b' '; MAX_MCP_CONFIG_BYTES + 1])
            .expect("oversized config fixture");

        let error = read_project_config(&path).expect_err("oversized config must fail closed");
        assert!(error.to_string().contains("MCP_CONFIG_INVALID"), "{error}");
        assert!(
            error.to_string().contains("exceeds 1048576 bytes"),
            "{error}"
        );
    }

    #[test]
    fn project_config_write_rejects_output_above_read_limit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(".pi/mcp.json");
        let value = serde_json::json!({
            "mcpServers": {
                "oversized": {"command": "x".repeat(MAX_MCP_CONFIG_BYTES)}
            }
        });

        let error = write_project_config(&path, &value)
            .expect_err("writer must not create a config the bounded reader rejects");
        assert!(error.to_string().contains("MCP_CONFIG_INVALID"), "{error}");
        assert!(
            !path.exists(),
            "rejected output must not create the config file"
        );
    }

    #[test]
    fn project_server_edits_never_index_a_non_object_config() {
        use serde_json::json;

        // `/mcp add` and `/mcp remove` used to index these by key, which
        // panics for every shape below except a bare `null`.
        for config in [
            json!([]),
            json!("text"),
            json!(7),
            json!({"mcpServers": []}),
            json!({"mcpServers": "text"}),
        ] {
            let mut removed = config.clone();
            assert!(!remove_project_server(&mut removed, "docs"), "{config}");
            assert_eq!(removed, config, "a refused removal must not edit");

            let mut added = config.clone();
            let error = insert_project_server(&mut added, "docs", json!({"command": "x"}))
                .expect_err("a non-object config is refused");
            assert!(error.to_string().contains("MCP_CONFIG_INVALID"), "{error}");
            assert_eq!(added, config, "a refused insertion must not edit");
        }

        for config in [json!(null), json!({}), json!({"mcpServers": null})] {
            let mut removed = config.clone();
            assert!(!remove_project_server(&mut removed, "docs"), "{config}");
            assert_eq!(removed, config, "an unknown server must not edit");

            let mut added = config;
            insert_project_server(&mut added, "docs", json!({"command": "x"}))
                .expect("an empty config accepts a server");
            assert_eq!(added, json!({"mcpServers": {"docs": {"command": "x"}}}));
        }

        let mut config = json!({"mcpServers": {"docs": {"command": "x"}, "db": {}}, "other": 1});
        insert_project_server(&mut config, "docs", json!({"url": "https://example.test"}))
            .expect("replace");
        assert!(remove_project_server(&mut config, "db"));
        assert!(!remove_project_server(&mut config, "db"));
        assert_eq!(
            config,
            json!({"mcpServers": {"docs": {"url": "https://example.test"}}, "other": 1})
        );
    }

    #[test]
    fn omp_vscode_and_portable_configs_follow_native_precedence() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        for (file, contents) in [
            (
                ".pi/mcp.json",
                r#"{"mcpServers":{"shared":{"command":"native"}}}"#,
            ),
            (
                ".omp/mcp.json",
                r#"{"mcpServers":{"shared":{"command":"omp"},"omp":{"command":"omp"}}}"#,
            ),
            (
                ".vscode/mcp.json",
                r#"{"servers":{"shared":{"command":"vscode"},"vscode":{"command":"code"}},"inputs":[{}]}"#,
            ),
            (
                "mcp.json",
                r#"{"mcpServers":{"shared":{"command":"portable"},"portable":{"command":"root"}}}"#,
            ),
            (
                ".mcp.json",
                r#"{"mcpServers":{"portable":{"command":"hidden"},"hidden":{"command":"hidden"}}}"#,
            ),
        ] {
            write(&cwd.join(file), contents);
        }
        let discovery = discover(&cwd, &temp.path().join("global"), &[], true);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.servers.len(), 5);
        let server = |name| {
            discovery
                .servers
                .iter()
                .find(|server| server.name == name)
                .unwrap()
        };
        assert_eq!(server("shared").command.as_deref(), Some("native"));
        assert_eq!(server("portable").command.as_deref(), Some("root"));
        for name in ["omp", "vscode", "portable", "hidden"] {
            assert_eq!(server(name).provenance, Provenance::Foreign);
        }
    }

    #[test]
    fn vscode_nested_servers_are_not_confused_with_inputs() {
        let temp = tempfile::tempdir().expect("tempdir");
        write(
            &temp.path().join(".vscode/mcp.json"),
            r#"{
            "mcp":{"servers":{"docs":{"command":"docs-server","enabled":false}}},
            "inputs":[{"id":"token","type":"promptString"}]
        }"#,
        );
        let discovery = discover(temp.path(), &temp.path().join("global"), &[], true);
        assert!(discovery.warnings.is_empty());
        assert_eq!(discovery.servers.len(), 1);
        assert!(!discovery.servers[0].enabled);
    }

    #[test]
    fn imported_configs_are_not_read_without_workspace_trust() {
        let temp = tempfile::tempdir().expect("tempdir");
        for file in [".omp/mcp.json", ".vscode/mcp.json", "mcp.json", ".mcp.json"] {
            write(&temp.path().join(file), "{malformed must never be read");
        }
        let discovery = discover(temp.path(), &temp.path().join("global"), &[], false);
        assert!(discovery.servers.is_empty());
        assert!(discovery.warnings.is_empty());
    }

    #[test]
    fn disabled_override_keeps_its_name_without_resolving_credentials() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join("global");
        write(
            &temp.path().join(".pi/mcp.json"),
            r#"{"mcpServers":{
            "docs":{"enabled":false,"command":"${PI_MCP_MISSING_DISABLED_COMMAND}"},
            "absent":{"disabled":true}
        }}"#,
        );
        write(
            &global.join("mcp.json"),
            r#"{"mcpServers":{"docs":{"command":"fallback"},"absent":{"command":"fallback"}}}"#,
        );
        let discovery = discover(temp.path(), &global, &[], true);
        assert!(discovery.warnings.is_empty());
        assert_eq!(discovery.servers.len(), 2);
        assert!(discovery.servers.iter().all(|server| !server.enabled));
        assert!(
            discovery
                .servers
                .iter()
                .all(|server| server.provenance == Provenance::ProjectPi)
        );
    }

    #[test]
    fn native_user_selection_deny_wins_and_can_enable_foreign_definitions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join("global");
        write(
            &temp.path().join(".omp/mcp.json"),
            r#"{"mcpServers":{
            "allowed":{"command":"server","enabled":false},
            "blocked":{"command":"server","enabled":false},
            "explicit":{"command":"server","disabled":true}
        }}"#,
        );
        write(
            &global.join("mcp.json"),
            r#"{
            "enabledServers":["allowed","blocked","explicit"],
            "disabledServers":["blocked","extension-only"]
        }"#,
        );
        let discovery = discover(temp.path(), &global, &[], true);
        assert!(discovery.warnings.is_empty());
        assert_eq!(discovery.servers.len(), 4);
        let enabled: Vec<_> = discovery
            .servers
            .iter()
            .filter(|server| server.enabled)
            .map(|server| server.name.as_str())
            .collect();
        assert_eq!(enabled, ["allowed"]);
        let placeholder = discovery
            .servers
            .iter()
            .find(|server| server.name == "extension-only")
            .unwrap();
        assert_eq!(placeholder.source_file, global.join("mcp.json"));
        assert!(placeholder.command.is_none());
    }

    #[test]
    fn malformed_enable_and_timeout_fields_cannot_fall_back_to_trusted_definitions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join("global");
        for invalid in [
            serde_json::json!({"enabled":"false"}),
            serde_json::json!({"enabled":null}),
            serde_json::json!({"disabled":"true"}),
            serde_json::json!({"timeout":0}),
            serde_json::json!({"timeout":-1}),
            serde_json::json!({"timeout":1.5}),
            serde_json::json!({"timeout":3_600_001}),
            serde_json::json!({"timeout":1000,"timeout_ms":1000}),
            serde_json::json!({"cwd":null}),
        ] {
            write(
                &temp.path().join(".pi/mcp.json"),
                &serde_json::json!({"mcpServers":{"docs":invalid}}).to_string(),
            );
            write(
                &global.join("mcp.json"),
                r#"{"mcpServers":{"docs":{"command":"fallback"}}}"#,
            );
            let discovery = discover(temp.path(), &global, &[], true);
            assert!(discovery.servers.is_empty(), "{invalid}");
            assert_eq!(discovery.warnings.len(), 1, "{invalid}");
        }
    }

    #[test]
    fn malformed_user_denylist_cannot_leave_project_servers_enabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let global = temp.path().join("global");
        write(
            &temp.path().join(".pi/mcp.json"),
            r#"{"mcpServers":{"docs":{"command":"server"}}}"#,
        );
        write(&global.join("mcp.json"), r#"{"disabledServers":"docs"}"#);
        let discovery = discover(temp.path(), &global, &[], true);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.warnings.len(), 1);
    }

    #[test]
    fn malformed_disabled_entry_still_reserves_the_name_for_later_extensions() {
        let temp = tempfile::tempdir().expect("tempdir");
        write(
            &temp.path().join(".omp/mcp.json"),
            r#"{"mcpServers":{
            "docs":{"enabled":false,"timeout":0,"command":"server"}
        }}"#,
        );
        let discovery = discover(temp.path(), &temp.path().join("global"), &[], true);
        assert_eq!(discovery.warnings.len(), 1);
        assert_eq!(discovery.servers.len(), 1);
        assert_eq!(discovery.servers[0].name, "docs");
        assert!(!discovery.servers[0].enabled);
        assert!(discovery.servers[0].command.is_none());
    }

    #[test]
    fn file_cwd_expansion_and_timeout_survive_discovery_and_bind_trust() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(cwd.join("server")).unwrap();
        write(
            &cwd.join(".omp/mcp.json"),
            r#"{"mcpServers":{"docs":{
            "command":"./serve","args":["${workspaceFolder}/input"],
            "cwd":"server","timeout":120000,
            "env":{"TOKEN":"${PI_MCP_DEFERRED_TOKEN}"}
        }}}"#,
        );
        let discovery = discover(&cwd, &temp.path().join("global"), &[], true);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        let server = &discovery.servers[0];
        assert_eq!(server.cwd, Some(cwd.join("server").canonicalize().unwrap()));
        assert_eq!(server.timeout_ms, Some(120_000));
        assert_eq!(
            server.args,
            [cwd.join("input").to_string_lossy().into_owned()]
        );
        assert_eq!(
            server.env,
            [("TOKEN".to_string(), "${PI_MCP_DEFERRED_TOKEN}".to_string())]
        );
        let mut changed = server.clone();
        changed.cwd = Some(cwd.clone());
        assert_ne!(
            server.fingerprint(&server.effective_cwd(&cwd)),
            changed.fingerprint(&changed.effective_cwd(&cwd))
        );
    }

    #[test]
    fn config_variables_expand_once_with_no_command_interpretation() {
        let workspace = Path::new("/workspace/project");
        let lookup = |name: &str| match name {
            "TOKEN" => Some("secret-token".to_string()),
            "DATA" => Some("$CMD:do-not-run".to_string()),
            "EMPTY" => Some(String::new()),
            _ => None,
        };
        assert_eq!(
            expand_config_value_with("Bearer ${TOKEN}", workspace, lookup).unwrap(),
            "Bearer secret-token"
        );
        assert_eq!(
            expand_config_value_with("${env:TOKEN}", workspace, lookup).unwrap(),
            "secret-token"
        );
        assert_eq!(
            expand_config_value_with("${MISSING:-fallback}/${EMPTY:-value}", workspace, lookup)
                .unwrap(),
            "fallback/value"
        );
        assert_eq!(
            expand_config_value_with("${DATA}", workspace, lookup).unwrap(),
            "$CMD:do-not-run"
        );
        assert_eq!(
            expand_config_value_with("${workspaceFolder}/src", workspace, lookup).unwrap(),
            "/workspace/project/src"
        );
        for value in ["${MISSING}", "${input:secret}", "${TOKEN", "${}"] {
            assert!(
                expand_config_value_with(value, workspace, lookup).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn codex_toml_preserves_disabled_flags_nested_env_and_multiline_arguments() {
        let temp = tempfile::tempdir().expect("tempdir");
        write(
            &temp.path().join(".codex/config.toml"),
            r#"
            [mcp_servers."docs.api"]
            command = 'server'
            args = [
                'first', # comments are TOML, not argv
                'second',
            ]
            enabled = true
            cwd = 'service'
            [mcp_servers."docs.api".env]
            TOKEN = '${env:TOKEN}'
            [mcp_servers.disabled]
            command = 'never'
            enabled = false
        "#,
        );
        let discovery = discover(temp.path(), &temp.path().join("global"), &[], true);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.servers.len(), 2);
        assert!(!discovery.servers[0].enabled);
        let server = &discovery.servers[1];
        assert_eq!(server.name, "docs.api");
        assert_eq!(server.args, ["first", "second"]);
        assert_eq!(
            server.env,
            [("TOKEN".to_string(), "${env:TOKEN}".to_string())]
        );
    }
}
