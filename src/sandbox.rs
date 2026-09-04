//! OS-level sandbox for the bash tool (`sandbox.*`, experimental).
//!
//! Enforcement is delegated to the `sandbox-runtime` crate — the defims fork
//! of `wangyedev/sandbox-runtime-rs`, maintained as a line-level Rust port of
//! `anthropic-experimental/sandbox-runtime` (behavior baseline recorded in the
//! fork's `UPSTREAM_BASE.md`). On macOS commands run under `sandbox-exec`
//! (Seatbelt); on Linux under bubblewrap + seccomp. Network egress goes
//! through a local HTTP/SOCKS5 filtering proxy owned by the crate.
//!
//! The sandbox is a risk mitigation layer, not a security boundary: it pairs
//! with approval/mediation policy (those classify before spawn, this enforces
//! at the OS level) and inherits the upstream limitations (domain fronting,
//! allowlisted-domain exfiltration, unix sockets, non-proxy-aware tools).
//!
//! Lifecycle: one shared tokio runtime per network policy hosts the proxy
//! tasks; managers are cached per network-policy hash so distinct
//! `sandbox.allowedDomains` configurations get distinct proxies while
//! sessions sharing a policy share one proxy pair. Per-cwd filesystem rules
//! ride on each `wrap` call as a custom config and never require
//! re-initializing the manager.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

use crate::config::SandboxSettings;
use crate::error::{Error, Result};

/// `sandbox.mode` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// No sandbox (default; byte-identical to the pre-sandbox path).
    Off,
    /// Sandbox when the platform supports it; degrade with a one-time
    /// warning otherwise.
    Auto,
    /// Sandbox or fail the tool call closed.
    On,
}

impl SandboxMode {
    /// Parse `sandbox.mode` (default [`SandboxMode::Off`]).
    #[must_use]
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("auto") => Self::Auto,
            Some("on") => Self::On,
            _ => Self::Off,
        }
    }
}

/// A command prepared for sandboxed execution.
#[derive(Debug, Clone)]
pub struct PreparedSandbox {
    /// Full command line to run through the shell: e.g.
    /// `sandbox-exec -f <profile> <shell> -c '<command>'`.
    pub command_line: String,
    /// Proxy environment entries (`HTTP_PROXY`, `ALL_PROXY`, ...) to apply on
    /// top of the child env. Empty when the network layer is off (Linux
    /// bwrap embeds the proxy env in the command line itself).
    pub proxy_env: Vec<(String, String)>,
    /// Environment entries that must be removed so they cannot bypass the
    /// proxy (`NO_PROXY` and friends).
    pub env_removals: Vec<String>,
}

impl PreparedSandbox {
    /// Apply the prepared entries to a `std::process::Command`.
    pub fn apply_to_command(&self, cmd: &mut std::process::Command) {
        for (k, v) in &self.proxy_env {
            cmd.env(k, v);
        }
        for key in &self.env_removals {
            cmd.env_remove(key);
        }
    }

    /// Apply the prepared entries to a portable-pty `CommandBuilder`.
    pub fn apply_to_pty_command(&self, cmd: &mut portable_pty::CommandBuilder) {
        for (k, v) in &self.proxy_env {
            cmd.env(k, v);
        }
        for key in &self.env_removals {
            cmd.env_remove(key);
        }
    }
}

struct SandboxRuntime {
    /// Dedicated tokio runtime hosting the proxy tasks; kept alive for the
    /// process lifetime so proxies survive between calls regardless of which
    /// thread drives them.
    tokio: Arc<tokio::runtime::Runtime>,
    manager: Arc<sandbox_runtime::SandboxManager>,
}

static RUNTIMES: std::sync::OnceLock<std::sync::Mutex<HashMap<u64, Arc<SandboxRuntime>>>> =
    std::sync::OnceLock::new();
static DEGRADE_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn runtimes() -> &'static std::sync::Mutex<HashMap<u64, Arc<SandboxRuntime>>> {
    RUNTIMES.get_or_init(std::sync::Mutex::default)
}

/// Default sandbox applied when `sandbox.*` is absent from settings:
/// filesystem protection on (`auto`), network layer off (the domain
/// allowlist proxy would 403 unlisted hosts — opt-in, not default).
#[must_use]
pub fn default_settings() -> crate::config::SandboxSettings {
    crate::config::SandboxSettings {
        mode: Some("auto".to_string()),
        network: Some("off".to_string()),
        ..Default::default()
    }
}

/// Prepare `command` for sandboxed execution.
///
/// Returns `Ok(None)` when the sandbox is not active for this call — either
/// `mode: off` or an `auto`-mode degradation (platform unavailable). Returns
/// an error only for `mode: on` with an unavailable platform or an actual
/// wrap failure (fail closed).
pub fn prepare(
    settings: &SandboxSettings,
    shell: &str,
    command: &str,
    cwd: &Path,
) -> Result<Option<PreparedSandbox>> {
    if settings.mode() == SandboxMode::Off {
        return Ok(None);
    }

    if !platform_supported() {
        return if settings.mode() == SandboxMode::On {
            Err(Error::tool(
                "bash",
                "[SANDBOX] sandbox.mode is 'on' but the platform is unsupported \
                 (sandboxing requires macOS or Linux with bubblewrap). Refusing to \
                 run the command unsandboxed."
                    .to_string(),
            ))
        } else {
            warn_degraded_once("platform unsupported");
            Ok(None)
        };
    }

    let runtime = get_or_init_runtime(settings)
        .map_err(|e| Error::tool("bash", format!("[SANDBOX] failed to initialize: {e}")))?;

    let config = to_srt_config(settings, cwd);
    let manager = Arc::clone(&runtime.manager);
    let shell = shell.to_string();
    let command = command.to_string();
    let wrapped = runtime
        .tokio
        .block_on(async move {
            manager
                .wrap_with_sandbox(&command, Some(&shell), Some(config))
                .await
        })
        .map_err(|e| Error::tool("bash", format!("[SANDBOX] failed to wrap command: {e}")))?;

    // On macOS the proxy env must be injected per child; on Linux the bwrap
    // command line already embeds it (see sandbox-runtime's linux module).
    let proxy_env = if cfg!(target_os = "macos") && settings.network_restricted() {
        let http = runtime.manager.get_proxy_port().unwrap_or(0);
        let socks = runtime.manager.get_socks_proxy_port().unwrap_or(0);
        sandbox_runtime::sandbox::macos::generate_proxy_env(http, socks)
    } else {
        Vec::new()
    };

    Ok(Some(PreparedSandbox {
        command_line: wrapped,
        proxy_env,
        env_removals: vec!["NO_PROXY".to_string(), "no_proxy".to_string()],
    }))
}

fn platform_supported() -> bool {
    if cfg!(target_os = "windows") {
        return false;
    }
    if !sandbox_runtime::SandboxManager::is_supported_platform() {
        return false;
    }
    sandbox_runtime::SandboxManager::new()
        .check_dependencies(None)
        .is_ok()
}

fn warn_degraded_once(reason: &str) {
    if DEGRADE_WARNED.set(()).is_ok() {
        tracing::warn!(
            event = "pi.bash.sandbox",
            reason = %reason,
            "sandbox.mode is 'auto' but the sandbox is unavailable; \
             running bash commands WITHOUT the sandbox"
        );
    }
}

/// Hash of the network policy: the only settings baked into the shared
/// manager (domain filter) at initialize time.
fn network_policy_key(settings: &SandboxSettings) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    settings.network_restricted().hash(&mut hasher);
    settings.network.hash(&mut hasher);
    settings.allowed_domains.hash(&mut hasher);
    settings.denied_domains.hash(&mut hasher);
    settings.allow_unix_sockets.hash(&mut hasher);
    settings.allow_local_binding.hash(&mut hasher);
    hasher.finish()
}

fn get_or_init_runtime(
    settings: &SandboxSettings,
) -> std::result::Result<Arc<SandboxRuntime>, String> {
    let key = network_policy_key(settings);
    if let Some(existing) = runtimes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Ok(Arc::clone(existing));
    }

    let tokio_rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| format!("failed to start sandbox runtime: {e}"))?,
    );

    // The manager keeps the domain filter from the initialize config; give it
    // this settings' network policy. Filesystem rules are per-cwd and always
    // passed per-wrap, so the cwd below is only a default.
    let config = to_srt_config(settings, &std::env::temp_dir());
    let manager = Arc::new(sandbox_runtime::SandboxManager::new());
    let init_manager = Arc::clone(&manager);
    let rt = Arc::clone(&tokio_rt);
    rt.block_on(async move { init_manager.initialize(config).await })
        .map_err(|e| format!("sandbox manager initialize failed: {e}"))?;

    let entry = Arc::new(SandboxRuntime {
        tokio: tokio_rt,
        manager,
    });
    let mut guards = runtimes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = guards.get(&key) {
        // Another thread initialized the same policy concurrently.
        return Ok(Arc::clone(existing));
    }
    guards.insert(key, Arc::clone(&entry));
    drop(guards);
    Ok(entry)
}

/// Map picrab sandbox settings + exec cwd to the crate's runtime config.
///
/// Relative `allowWrite` entries (e.g. the `.` default) are resolved against
/// `cwd` because the crate does not absolutize them and Seatbelt subpath
/// rules need absolute paths.
#[must_use]
pub fn to_srt_config(
    settings: &SandboxSettings,
    cwd: &Path,
) -> sandbox_runtime::config::SandboxRuntimeConfig {
    let restricted = settings.network_restricted();
    let (allowed, denied) = if restricted {
        (
            settings
                .allowed_domains
                .clone()
                .unwrap_or_else(|| vec!["*".to_string()]),
            settings.denied_domains.clone().unwrap_or_default(),
        )
    } else {
        // Empty domain lists = unrestricted egress, no proxy filtering.
        (Vec::new(), Vec::new())
    };

    let global_dir = crate::config::Config::global_dir();
    let allow_write: Vec<String> = settings
        .allow_write
        .clone()
        .unwrap_or_else(|| {
            vec![
                ".".to_string(),
                "/tmp".to_string(),
                "/private/tmp".to_string(),
            ]
        })
        .into_iter()
        .map(|p| absolutize_allow_write(&p, cwd, &global_dir))
        .collect();

    sandbox_runtime::config::SandboxRuntimeConfig {
        network: sandbox_runtime::config::NetworkConfig {
            allowed_domains: allowed,
            denied_domains: denied,
            allow_unix_sockets: settings.allow_unix_sockets.clone(),
            allow_all_unix_sockets: None,
            allow_local_binding: settings.allow_local_binding,
            http_proxy_port: None,
            socks_proxy_port: None,
            mitm_proxy: None,
        },
        filesystem: sandbox_runtime::config::FilesystemConfig {
            deny_read: settings.deny_read.clone().unwrap_or_else(|| {
                vec![
                    "~/.ssh".to_string(),
                    "~/.gnupg".to_string(),
                    "~/.aws".to_string(),
                ]
            }),
            allow_write,
            deny_write: settings.deny_write.clone().unwrap_or_default(),
            allow_git_config: None,
        },
        ignore_violations: None,
        enable_weaker_nested_sandbox: None,
        ripgrep: None,
        mandatory_deny_search_depth: None,
        // picrab's bash tool may allocate a PTY for isatty-requiring
        // commands on either spawn path; grant it unconditionally.
        allow_pty: Some(true),
        seccomp: None,
    }
}

/// Resolve one `allowWrite` entry to an absolute path.
fn absolutize_allow_write(entry: &str, cwd: &Path, global_dir: &Path) -> String {
    match entry {
        "." => cwd.display().to_string(),
        "$PI_DIR" | "~/.pi" => global_dir.display().to_string(),
        p if p.starts_with('/') => p.to_string(),
        // `~`-prefixed paths are expanded by the crate; everything else
        // relative resolves against the exec cwd.
        p => cwd.join(p).display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(json: &str) -> SandboxSettings {
        serde_json::from_str(json).expect("parse")
    }

    #[test]
    fn mode_defaults_to_off() {
        assert_eq!(SandboxMode::from_setting(None), SandboxMode::Off);
        assert_eq!(SandboxMode::from_setting(Some("off")), SandboxMode::Off);
        assert_eq!(SandboxMode::from_setting(Some("auto")), SandboxMode::Auto);
        assert_eq!(SandboxMode::from_setting(Some("on")), SandboxMode::On);
        assert_eq!(SandboxMode::from_setting(Some("bogus")), SandboxMode::Off);
    }

    #[test]
    fn network_defaults_follow_mode() {
        // mode absent -> off -> network not restricted
        assert!(!settings("{}").network_restricted());
        // auto implies allowlist
        assert!(settings(r#"{"mode": "auto"}"#).network_restricted());
        assert!(settings(r#"{"mode": "on"}"#).network_restricted());
        // explicit off wins over mode
        assert!(!settings(r#"{"mode": "auto", "network": "off"}"#).network_restricted());
        // explicit allowlist with off mode still restricts
        assert!(settings(r#"{"mode": "off", "network": "allowlist"}"#).network_restricted());
    }

    #[test]
    fn off_mode_prepares_nothing() {
        let s = settings("{}");
        assert!(
            prepare(&s, "/bin/bash", "echo hi", Path::new("/tmp"))
                .is_ok_and(|p| p.is_none())
        );
    }

    #[test]
    fn srt_config_maps_domains_and_paths() {
        let s = settings(
            r#"{"mode": "auto", "allowedDomains": ["github.com"], "allowWrite": [".", "/tmp"]}"#,
        );
        let cwd = Path::new("/workspace/proj");
        let config = to_srt_config(&s, cwd);
        assert_eq!(config.network.allowed_domains, vec!["github.com"]);
        assert!(config
            .filesystem
            .allow_write
            .iter()
            .any(|p| p == "/workspace/proj"));
        assert!(config.filesystem.allow_write.iter().any(|p| p == "/tmp"));
        // default deny_read applied
        assert!(config.filesystem.deny_read.iter().any(|p| p == "~/.ssh"));
        assert_eq!(config.allow_pty, Some(true));
    }

    #[test]
    fn network_off_maps_to_empty_domain_lists() {
        let s = settings(r#"{"mode": "auto", "network": "off"}"#);
        let config = to_srt_config(&s, Path::new("/tmp"));
        assert!(config.network.allowed_domains.is_empty());
        assert!(config.network.denied_domains.is_empty());
    }

    #[test]
    fn sandbox_settings_camel_case_aliases() {
        let s = settings(
            r#"{"mode": "on", "allowedDomains": ["a.com"], "denyRead": ["~/.x"], "allowLocalBinding": true}"#,
        );
        assert_eq!(s.allowed_domains, Some(vec!["a.com".to_string()]));
        assert_eq!(s.deny_read, Some(vec!["~/.x".to_string()]));
        assert_eq!(s.allow_local_binding, Some(true));
    }

    #[test]
    fn network_policy_key_distinguishes_domains() {
        let a = settings(r#"{"mode": "auto", "allowedDomains": ["a.com"]}"#);
        let b = settings(r#"{"mode": "auto", "allowedDomains": ["b.com"]}"#);
        let a2 = settings(r#"{"mode": "on", "allowedDomains": ["a.com"]}"#);
        assert_ne!(network_policy_key(&a), network_policy_key(&b));
        // mode/network-mode differences that don't change the policy hash equal
        assert_eq!(network_policy_key(&a), network_policy_key(&a2));
    }
}
