//! OS-level sandbox for the bash tool (`sandbox.*`, experimental).
//!
//! Enforcement is delegated to the `sandbox-runtime` crate — the defims fork
//! of `wangyedev/sandbox-runtime-rs`, maintained as a line-level Rust port of
//! `anthropic-experimental/sandbox-runtime` (behavior baseline recorded in the
//! fork's `UPSTREAM_BASE.md`). On macOS commands run under `sandbox-exec`
//! (Seatbelt); on Linux under bubblewrap + seccomp; on Windows via the fork's
//! `srt-win` helper (cross-account model: dedicated local account + WFP fence
//! + session NTFS ACLs — see the fork's UPSTREAM_BASE.md Windows section for
//! the platform gaps). Network egress goes through a local HTTP/SOCKS5
//! filtering proxy owned by the crate.
//!
//! The sandbox is a risk mitigation layer, not a security boundary: it pairs
//! with approval/mediation policy (those classify before spawn, this enforces
//! at the OS level) and inherits the upstream limitations (domain fronting,
//! allowlisted-domain exfiltration, unix sockets, non-proxy-aware tools).
//!
//! Lifecycle: one shared tokio runtime per policy hosts the proxy tasks.
//! On unix, managers are cached per network-policy hash and per-cwd
//! filesystem rules ride on each `wrap` call. On Windows the filesystem
//! rules are SESSION-level (initialize-time ACL grants), so managers are
//! cached per FULL fs+network config key with an idle-LRU bound (in-flight
//! wraps are never evicted — R1); a wrap on a changed cwd re-initializes
//! (revoke+grant) for that key.

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
    /// `sandbox-exec -f <profile> <shell> -c '<command>'`. Unix only —
    /// EMPTY on Windows, where [`Self::windows_spawn`] carries the native
    /// spawn spec instead (fork N3: no outer shell, no MSYS rewriting).
    pub command_line: String,
    /// Proxy environment entries (`HTTP_PROXY`, `ALL_PROXY`, ...) to apply on
    /// top of the child env. Empty when the network layer is off (Linux
    /// bwrap embeds the proxy env in the command line itself; Windows
    /// embeds it in the spawn spec's env overlay).
    pub proxy_env: Vec<(String, String)>,
    /// Environment entries that must be removed so they cannot bypass the
    /// proxy (`NO_PROXY` and friends).
    pub env_removals: Vec<String>,
    /// Windows: native spawn spec for srt-win (`Command::new(program)+
    /// args`), with the `--env` overlay (relay + proxy + git safe.directory)
    /// baked in. `None` on unix.
    pub windows_spawn: Option<WindowsSpawnSpec>,
}

/// Windows native spawn spec (see [`PreparedSandbox::windows_spawn`]).
#[derive(Debug, Clone)]
pub struct WindowsSpawnSpec {
    pub program: std::path::PathBuf,
    pub args: Vec<String>,
    /// KEY=VALUE overlay for the broker spawn (relay + generated).
    pub env: Vec<(String, String)>,
    /// Env vars to strip from the broker spawn env.
    pub env_removals: Vec<String>,
}

impl PreparedSandbox {
    /// Whether this prepare produced a Windows native spawn (no shell line).
    #[must_use]
    pub fn is_windows_spawn(&self) -> bool {
        self.windows_spawn.is_some()
    }

    /// Apply the prepared entries to a `std::process::Command`.
    ///
    /// For a Windows spawn the caller must ALSO rebase the command on
    /// [`Self::windows_spawn`] (program + args) instead of the shell line.
    pub fn apply_to_command(&self, cmd: &mut std::process::Command) {
        for (k, v) in &self.proxy_env {
            cmd.env(k, v);
        }
        for key in &self.env_removals {
            cmd.env_remove(key);
        }
        if let Some(spawn) = &self.windows_spawn {
            for (k, v) in &spawn.env {
                cmd.env(k, v);
            }
            for key in &spawn.env_removals {
                cmd.env_remove(key);
            }
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

/// Manager/proxy state; the `sandbox-runtime` crate compiles on every
/// target now, so the plumbing is target-independent.
struct SandboxRuntime {
    /// Dedicated tokio runtime hosting the proxy tasks; kept alive for the
    /// process lifetime so proxies survive between calls regardless of which
    /// thread drives them.
    tokio: Arc<tokio::runtime::Runtime>,
    manager: Arc<sandbox_runtime::SandboxManager>,
}

/// Cache entry: last-use stamp for idle-LRU eviction (Windows) and the
/// in-flight guard so a wrap in progress is never evicted (R1).
struct RuntimeEntry {
    runtime: Arc<SandboxRuntime>,
    last_used: std::time::Instant,
    in_flight: u32,
}

static RUNTIMES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u64, RuntimeEntry>>> =
    std::sync::OnceLock::new();
static DEGRADE_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
/// Windows install-readiness probe cache (one `srt-win` resolve per
/// process; a failed install stays failed for this process lifetime).
static WIN_SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Max concurrent managers on Windows. The WFP PERMIT range is 10 ports
/// (2 per manager = HTTP+SOCKS), so 5 is the hard ceiling; hold one slot
/// of headroom.
const WINDOWS_MANAGER_CAP: usize = 4;

fn runtimes() -> &'static std::sync::Mutex<std::collections::HashMap<u64, RuntimeEntry>> {
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

    prepare_supported(settings, shell, command, cwd)
}

/// Real wrap path.
fn prepare_supported(
    settings: &SandboxSettings,
    shell: &str,
    command: &str,
    cwd: &Path,
) -> Result<Option<PreparedSandbox>> {
    if cfg!(windows) {
        prepare_supported_windows(settings, shell, command, cwd)
    } else {
        prepare_supported_unix(settings, shell, command, cwd)
    }
}

/// Unix wrap path (macOS Seatbelt / Linux bwrap): managers keyed by network
/// policy; per-cwd fs rules ride on the wrap.
#[cfg(unix)]
fn prepare_supported_unix(
    settings: &SandboxSettings,
    shell: &str,
    command: &str,
    cwd: &Path,
) -> Result<Option<PreparedSandbox>> {
    let entry = get_or_init_runtime(settings)
        .map_err(|e| Error::tool("bash", format!("[SANDBOX] failed to initialize: {e}")))?;
    let runtime = entry;

    let config = to_srt_config(settings, cwd);
    let manager = Arc::clone(&runtime.manager);
    let shell = shell.to_string();
    let command = command.to_string();
    let wrapped = runtime
        .tokio
        .block_on(async move {
            manager
                .wrap_with_sandbox(&command, Some(&shell), Some(config), &[])
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

    let sh = wrapped
        .as_shell()
        .expect("unix wraps are shell-shaped")
        .to_string();

    Ok(Some(PreparedSandbox {
        command_line: sh,
        proxy_env,
        env_removals: vec!["NO_PROXY".to_string(), "no_proxy".to_string()],
        windows_spawn: None,
    }))
}

/// Non-Windows/non-unix stub: `prepare` always degrades before this point.
#[cfg(not(unix))]
fn prepare_supported_unix(
    _settings: &SandboxSettings,
    _shell: &str,
    _command: &str,
    _cwd: &Path,
) -> Result<Option<PreparedSandbox>> {
    unreachable!("sandbox is not supported on this target")
}

/// Windows wrap path: manager keyed by the FULL fs+network config (fs rules
/// are session-level ACLs), wrap returns a native spawn spec. The host env
/// is relayed into the `--env` overlay (wrap strips NO_PROXY/TMPDIR itself)
/// so user tokens survive the fresh-profile env — the N4 decision.
fn prepare_supported_windows(
    settings: &SandboxSettings,
    shell: &str,
    command: &str,
    cwd: &Path,
) -> Result<Option<PreparedSandbox>> {
    let key = windows_config_key(settings, cwd);
    let entry = get_or_init_runtime_keyed(settings, cwd, key)
        .map_err(|e| Error::tool("bash", format!("[SANDBOX] failed to initialize: {e}")))?;
    let runtime = entry;

    // Session-level config = the manager's initialize config (fs grants
    // already applied); the wrap passes no custom config (per-exec allow
    // overrides are unsupported on Windows anyway).
    let manager = Arc::clone(&runtime.manager);
    let shell = shell.to_string();
    let command = command.to_string();
    let relay_env: Vec<(String, String)> = std::env::vars().collect();
    let wrapped = runtime
        .tokio
        .block_on(async move {
            manager
                .wrap_with_sandbox(&command, Some(&shell), None, &relay_env)
                .await
        });
    // Release the in-flight guard on BOTH paths — a leaked guard would
    // pin the manager against idle-LRU eviction forever (R1).
    release_in_flight(key);
    let wrapped = wrapped
        .map_err(|e| Error::tool("bash", format!("[SANDBOX] failed to wrap command: {e}")))?;

    release_in_flight(key);

    let spec = match wrapped {
        sandbox_runtime::manager::WrappedCommand::WindowsSpawn(spec) => spec,
        sandbox_runtime::manager::WrappedCommand::Shell(_) => {
            return Err(Error::tool(
                "bash",
                "[SANDBOX] internal error: Windows wrap returned a shell line".to_string(),
            ));
        }
    };

    Ok(Some(PreparedSandbox {
        command_line: String::new(),
        proxy_env: Vec::new(),
        env_removals: Vec::new(),
        windows_spawn: Some(WindowsSpawnSpec {
            program: spec.program,
            args: spec.args,
            env: spec.env,
            env_removals: spec.env_removals,
        }),
    }))
}

/// Windows readiness: one exact-hash helper resolve per process. Absent
/// install / version drift resolves to `InstallRequired`.
fn platform_supported_windows_probe() -> bool {
    *WIN_SUPPORTED.get_or_init(|| {
        match sandbox_runtime::sandbox::windows::resolve_srt_win_spawn(None) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(
                    event = "pi.bash.sandbox",
                    "windows sandbox helper unavailable: {e}"
                );
                false
            }
        }
    })
}

fn platform_supported() -> bool {
    if cfg!(windows) {
        platform_supported_windows_probe()
    } else {
        #[cfg(unix)]
        {
            if !sandbox_runtime::SandboxManager::is_supported_platform() {
                return false;
            }
            sandbox_runtime::SandboxManager::new()
                .check_dependencies(None)
                .is_ok()
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
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

/// Hash of the settings baked into the shared manager at initialize time.
/// Unix: the network policy only (fs rules are per-wrap). Windows: the
/// FULL fs+network policy — fs grants are session-level ACLs, so a cwd or
/// allowWrite change is a DIFFERENT manager (revoke+grant on init).
fn policy_key(settings: &SandboxSettings, cwd: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    settings.network_restricted().hash(&mut hasher);
    settings.network.hash(&mut hasher);
    settings.allowed_domains.hash(&mut hasher);
    settings.denied_domains.hash(&mut hasher);
    settings.allow_unix_sockets.hash(&mut hasher);
    settings.allow_local_binding.hash(&mut hasher);
    if cfg!(windows) {
        settings.allow_read.hash(&mut hasher);
        settings.allow_write.hash(&mut hasher);
        settings.deny_write.hash(&mut hasher);
        settings.deny_read.hash(&mut hasher);
        cwd.hash(&mut hasher);
    }
    hasher.finish()
}

/// Unix entry point: network-policy key.
#[cfg(unix)]
fn get_or_init_runtime(
    settings: &SandboxSettings,
) -> std::result::Result<Arc<SandboxRuntime>, String> {
    get_or_init_runtime_keyed(settings, &std::env::temp_dir(), network_policy_key(settings))
}

/// Windows entry point: full-config key (kept for the windows arm).
#[cfg(windows)]
fn get_or_init_runtime(
    settings: &SandboxSettings,
) -> std::result::Result<Arc<SandboxRuntime>, String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    get_or_init_runtime_keyed(settings, &cwd, policy_key(settings, &cwd))
}

#[cfg(unix)]
fn network_policy_key(settings: &SandboxSettings) -> u64 {
    policy_key(settings, &std::env::temp_dir())
}

/// Windows settings+cwd cache key.
fn windows_config_key(settings: &SandboxSettings, cwd: &Path) -> u64 {
    policy_key(settings, cwd)
}

fn get_or_init_runtime_keyed(
    settings: &SandboxSettings,
    cwd: &Path,
    key: u64,
) -> std::result::Result<Arc<SandboxRuntime>, String> {
    if let Some(entry) = touch_runtime(key) {
        return Ok(entry);
    }

    let tokio_rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| format!("failed to start sandbox runtime: {e}"))?,
    );

    // The manager keeps the initialize config: unix gets the per-call config
    // shape with a default cwd; Windows gets the session-level fs grants for
    // THIS key's cwd.
    let config = if cfg!(windows) {
        to_srt_config_windows(settings, cwd)
    } else {
        to_srt_config(settings, &std::env::temp_dir())
    };
    let manager = Arc::new(sandbox_runtime::SandboxManager::new());
    let init_manager = Arc::clone(&manager);
    let rt = Arc::clone(&tokio_rt);
    rt.block_on(async move { init_manager.initialize(config).await })
        .map_err(|e| format!("sandbox manager initialize failed: {e}"))?;

    let runtime = Arc::new(SandboxRuntime {
        tokio: tokio_rt,
        manager,
    });
    insert_runtime(key, Arc::clone(&runtime));
    Ok(runtime)
}

/// Bump last-use + in-flight for an existing entry; None when absent.
fn touch_runtime(key: u64) -> Option<Arc<SandboxRuntime>> {
    let mut guards = runtimes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = guards.get_mut(&key)?;
    entry.last_used = std::time::Instant::now();
    entry.in_flight += 1;
    Some(Arc::clone(&entry.runtime))
}

/// Insert a freshly initialized runtime, evicting idle (in_flight == 0)
/// least-recently-used entries beyond the Windows cap. Unix keeps the
/// historical unbounded map (tiny key space: network policies only).
fn insert_runtime(key: u64, runtime: Arc<SandboxRuntime>) {
    let mut guards = runtimes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !guards.contains_key(&key) {
        guards.insert(
            key,
            RuntimeEntry {
                runtime,
                last_used: std::time::Instant::now(),
                in_flight: 1,
            },
        );
    }
    if cfg!(windows) && guards.len() > WINDOWS_MANAGER_CAP {
        // Oldest idle entry first; busy entries are never evicted (R1).
        let victim = guards
            .iter()
            .filter(|(k, e)| e.in_flight == 0 && *k != &key)
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| *k);
        if let Some(victim) = victim {
            if let Some(entry) = guards.remove(&victim) {
                let rt = Arc::clone(&entry.runtime.tokio);
                let manager = Arc::clone(&entry.runtime.manager);
                // Reset OUTSIDE the lock (revokes session ACEs, stops
                // proxies — freeing the port budget for the new manager).
                drop(guards);
                rt.block_on(async move { manager.reset().await });
                return;
            }
        }
    }
}

/// Decrement the in-flight guard after a wrap completes.
fn release_in_flight(key: u64) {
    if let Some(entry) = runtimes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_mut(&key)
    {
        entry.in_flight = entry.in_flight.saturating_sub(1);
        entry.last_used = std::time::Instant::now();
    }
}

/// 通用丢弃设备(N-R2):无条件并入 allow_write
#[cfg(unix)]
const DEVICE_ALLOW_WRITE: [&str; 5] = [
    "/dev/null",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/tty",
    "/dev/zero",
];

/// Map picrab sandbox settings + exec cwd to the crate's runtime config.
///
/// Relative `allowWrite` entries (e.g. the `.` default) are resolved against
/// `cwd` because the crate does not absolutize them and Seatbelt subpath
/// rules need absolute paths.
#[must_use]
#[cfg(unix)]
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
    let mut allow_write: Vec<String> = settings
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
    // N-R2:通用丢弃设备无条件 union(默认清单与显式 allowWrite 都放行)。
    // 写入由内核消化、无落盘副作用,拒绝它只破坏 `2>/dev/null` 类惯用法。
    // 用户否决权经 denyWrite 保留(Seatbelt profile 生成顺序:deny 后置覆盖 allow)。
    for dev in DEVICE_ALLOW_WRITE {
        if !allow_write.iter().any(|p| p == dev) {
            allow_write.push(dev.to_string());
        }
    }

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
            allow_read: Vec::new(),
        },
        ignore_violations: None,
        enable_weaker_nested_sandbox: None,
        ripgrep: None,
        mandatory_deny_search_depth: None,
        // picrab's bash tool may allocate a PTY for isatty-requiring
        // commands on either spawn path; grant it unconditionally.
        allow_pty: Some(true),
        seccomp: None,
        windows: None,
    }
}

/// Resolve one `allowWrite` entry to an absolute path (unix shape).
#[cfg(unix)]
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

/// Windows session config (fs rules are initialize-time ACL grants):
/// - allowWrite = user list or `[cwd]` (NO user %TEMP%: msys `/tmp` maps to
///   the sandbox account's OWN temp — N5); `/tmp`-style unix defaults do
///   not apply;
/// - `DEVICE_ALLOW_WRITE` is NOT merged (NUL is shell semantics on
///   Windows, no /dev devices);
/// - allowRead = user extensions (the escape hatch for ~/.gitconfig etc.);
/// - denyRead defaults mirror unix (`~` expands via %USERPROFILE% in the
///   crate);
/// - port range / sublayer GUID: fork defaults (60080-60089), overridable
///   later via a config pass-through if needed.
/// Windows session config (fs rules are initialize-time ACL grants):
/// (windows + unix both compile this — the prepare_supported windows arm
/// is a runtime branch, not a cfg branch).
pub fn to_srt_config_windows(
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
        (Vec::new(), Vec::new())
    };

    let global_dir = crate::config::Config::global_dir();
    let abs = |entry: &str| -> String {
        match entry {
            "." => cwd.display().to_string(),
            "$PI_DIR" | "~/.pi" => global_dir.display().to_string(),
            p if p.len() >= 2 && p.as_bytes()[1] == b':' => p.to_string(),
            p if p.starts_with("\\\\") => p.to_string(),
            p => cwd.join(p).display().to_string(),
        }
    };

    let allow_write: Vec<String> = settings
        .allow_write
        .clone()
        .unwrap_or_else(|| vec![".".to_string()])
        .iter()
        .map(|p| abs(p))
        .collect();
    let allow_read: Vec<String> = settings
        .allow_read
        .clone()
        .unwrap_or_default()
        .iter()
        .map(|p| abs(p))
        .collect();

    sandbox_runtime::config::SandboxRuntimeConfig {
        network: sandbox_runtime::config::NetworkConfig {
            allowed_domains: allowed,
            denied_domains: denied,
            allow_unix_sockets: None,
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
            allow_read,
        },
        ignore_violations: None,
        enable_weaker_nested_sandbox: None,
        ripgrep: None,
        mandatory_deny_search_depth: None,
        allow_pty: None,
        seccomp: None,
        windows: None,
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
    #[cfg(unix)]
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
        // N-R2: 显式 allowWrite 也 union 丢弃设备
        assert!(config.filesystem.allow_write.iter().any(|p| p == "/dev/null"));
        // default deny_read applied
        assert!(config.filesystem.deny_read.iter().any(|p| p == "~/.ssh"));
        assert_eq!(config.allow_pty, Some(true));
    }

    #[test]
    #[cfg(unix)]
    fn allow_write_defaults_include_devices() {
        let s = settings(r#"{"mode": "auto"}"#);
        let config = to_srt_config(&s, Path::new("/workspace/proj"));
        for dev in DEVICE_ALLOW_WRITE {
            assert!(
                config.filesystem.allow_write.iter().any(|p| p == dev),
                "missing {dev}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
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
    #[cfg(unix)]
    fn network_policy_key_distinguishes_domains() {
        let a = settings(r#"{"mode": "auto", "allowedDomains": ["a.com"]}"#);
        let b = settings(r#"{"mode": "auto", "allowedDomains": ["b.com"]}"#);
        let a2 = settings(r#"{"mode": "on", "allowedDomains": ["a.com"]}"#);
        assert_ne!(network_policy_key(&a), network_policy_key(&b));
        // mode/network-mode differences that don't change the policy hash equal
        assert_eq!(network_policy_key(&a), network_policy_key(&a2));
    }
}
