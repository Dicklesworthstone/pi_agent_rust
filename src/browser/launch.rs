//! Owned Chromium launch, isolated from the operator's ordinary browser profile.
//! No shell, arbitrary launch flags, fixed debugging port, or sandbox downgrade.

use super::policy;
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Trusted host configuration. These fields are deliberately not tool arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserLaunchOptions {
    /// An installed Chromium/Chrome executable; None searches standard locations.
    pub executable_path: Option<PathBuf>,
    pub headless: bool,
    pub user_agent: Option<String>,
}

impl Default for BrowserLaunchOptions {
    fn default() -> Self {
        Self {
            executable_path: None,
            headless: true,
            user_agent: None,
        }
    }
}

pub(super) enum Connection {
    Attach(url::Url),
    Managed(BrowserLaunchOptions),
}

impl Connection {
    pub(super) fn resolve(
        endpoint: Option<&str>,
        launch: Option<&BrowserLaunchOptions>,
    ) -> Result<Self> {
        Self::with_environment(endpoint, launch, |name| std::env::var_os(name))
    }

    fn with_environment(
        endpoint: Option<&str>,
        launch: Option<&BrowserLaunchOptions>,
        mut lookup: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self> {
        if let Some(endpoint) = endpoint {
            return Ok(Self::Attach(policy::endpoint(endpoint, false)?));
        }
        if let Some(launch) = launch {
            validate_options(launch)?;
            return Ok(Self::Managed(launch.clone()));
        }
        if let Some(endpoint) = lookup("PI_BROWSER_CDP_URL") {
            let endpoint = endpoint
                .to_str()
                .ok_or_else(|| error("PI_BROWSER_CDP_URL must be UTF-8"))?;
            return Ok(Self::Attach(policy::endpoint(endpoint, false)?));
        }
        let headless = match lookup("PI_BROWSER_HEADLESS") {
            None => true,
            Some(value) => match value.to_str() {
                Some("1" | "true") => true,
                Some("0" | "false") => false,
                _ => return Err(error("PI_BROWSER_HEADLESS must be true, false, 1 or 0")),
            },
        };
        let user_agent = lookup("PI_BROWSER_USER_AGENT")
            .map(|value| {
                value
                    .into_string()
                    .map_err(|_| error("PI_BROWSER_USER_AGENT must be UTF-8"))
            })
            .transpose()?;
        let options = BrowserLaunchOptions {
            executable_path: lookup("PI_BROWSER_EXECUTABLE").map(PathBuf::from),
            headless,
            user_agent,
        };
        validate_options(&options)?;
        Ok(Self::Managed(options))
    }
}

fn validate_options(options: &BrowserLaunchOptions) -> Result<()> {
    if options
        .executable_path
        .as_ref()
        .is_some_and(|path| path.as_os_str().is_empty())
    {
        return Err(error("browser executable path cannot be empty"));
    }
    if options.user_agent.as_ref().is_some_and(|value| {
        value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control)
    }) {
        return Err(error(
            "browser user agent must be nonempty, control-free and at most 1024 bytes",
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct Address {
    pub(super) http: url::Url,
    pub(super) debugger_path: String,
}

pub(super) struct ManagedBrowser {
    process: Process,
    pub(super) address: Address,
}

impl ManagedBrowser {
    pub(super) async fn launch(
        owner: &AgentCx,
        cwd: &Path,
        options: &BrowserLaunchOptions,
    ) -> Result<Self> {
        let mut process = Process::spawn(owner, cwd, options)?;
        // Until ready returns, the process/profile remain local to this future.
        // Dropping a cancelled startup kills the owned process before cleanup.
        let address = process.ready(owner).await?;
        Ok(Self { process, address })
    }

    pub(super) fn id(&self) -> u32 {
        self.process.child.id()
    }

    pub(super) fn running(&mut self) -> Result<bool> {
        self.process
            .child
            .try_wait()
            .map(|status| status.is_none())
            .map_err(|failure| error(format!("could not inspect the owned browser: {failure}")))
    }

    pub(super) fn stop(&mut self) -> Result<()> {
        self.process
            .stop()
            .map_err(|failure| error(format!("could not stop the owned browser: {failure}")))
    }
}

struct Process {
    // The session owns the established browser, not a completed tool call's Cx.
    // Each new operation checks its own owner; startup still checks cancellation.
    child: Child,
    stopped: bool,
    profile: tempfile::TempDir,
}

impl Process {
    fn spawn(owner: &AgentCx, cwd: &Path, options: &BrowserLaunchOptions) -> Result<Self> {
        let caps = owner.capabilities();
        if !caps.io || !caps.spawn || !caps.time || !caps.entropy {
            return Err(error(
                "managed browser launch requires I/O, spawn, timer and entropy capabilities",
            ));
        }
        owner
            .checkpoint()
            .map_err(|_| error("browser launch cancelled before dispatch"))?;
        validate_options(options)?;
        let executable = executable(options.executable_path.as_deref(), cwd)?;
        let profile = tempfile::Builder::new().prefix("pi-browser-").tempdir()?;
        let mut command = Command::new(&executable);
        command
            .current_dir(cwd)
            .args(arguments(profile.path(), options))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let child = owner
            .process()
            .spawn_checked(&mut command)
            .map_err(|failure| {
                error(format!(
                    "could not launch the configured browser: {failure}"
                ))
            })?;
        let process = Self {
            child,
            stopped: false,
            profile,
        };
        crate::tools::attach_child_job_discipline(&process.child);
        // The guard already exists if cancellation races with spawn.
        owner
            .checkpoint()
            .map_err(|_| error("browser launch cancelled after dispatch"))?;
        Ok(process)
    }

    async fn ready(&mut self, owner: &AgentCx) -> Result<Address> {
        let started = Instant::now();
        let port_file = self.profile.path().join("DevToolsActivePort");
        loop {
            owner
                .checkpoint()
                .map_err(|_| error("browser launch cancelled"))?;
            if let Some(status) = self.child.try_wait()? {
                return Err(error(format!(
                    "Chromium exited before remote debugging became ready ({status}); check the executable, display and OS sandbox support. Pi does not disable the browser sandbox"
                )));
            }
            if let Some(address) = read_address(&port_file)? {
                return Ok(address);
            }
            if started.elapsed() >= Duration::from_secs(30) {
                return Err(error(
                    "Chromium did not publish DevToolsActivePort within 30 seconds",
                ));
            }
            owner.time().sleep(Duration::from_millis(25)).await;
        }
    }

    fn stop(&mut self) -> std::io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.child.id())
            && let Some(group) = rustix::process::Pid::from_raw(pid)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        #[cfg(not(unix))]
        crate::tools::kill_process_tree(Some(self.child.id()));
        let _ = self.child.kill();
        self.child.wait()?;
        self.stopped = true;
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Kill/reap before the profile field is dropped. Cleanup uses existing
        // process ownership; it must not require a new capability context.
        let _ = self.stop();
    }
}

fn arguments(profile: &Path, options: &BrowserLaunchOptions) -> Vec<OsString> {
    let mut profile_arg = OsString::from("--user-data-dir=");
    profile_arg.push(profile);
    let mut arguments = vec![
        profile_arg,
        "--remote-debugging-port=0".into(),
        "--remote-debugging-address=127.0.0.1".into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-sync".into(),
    ];
    if options.headless {
        arguments.push("--headless=new".into());
    }
    if let Some(user_agent) = &options.user_agent {
        arguments.push(format!("--user-agent={user_agent}").into());
    }
    arguments.push("about:blank".into());
    arguments
}

fn executable(explicit: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let directories: Vec<_> = std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .collect();
    if let Some(explicit) = explicit {
        let candidate = if explicit.is_absolute() {
            Some(explicit.to_path_buf())
        } else if explicit.components().count() > 1 {
            Some(cwd.join(explicit))
        } else {
            directories
                .iter()
                .map(|directory| directory.join(explicit))
                .find(|path| is_executable(path))
        };
        return candidate.filter(|path| is_executable(path)).ok_or_else(|| {
            error("configured browser executable was not found or is not executable")
        });
    }
    let names: &[&str] = if cfg!(windows) {
        &["chrome.exe", "chromium.exe", "msedge.exe"]
    } else {
        &[
            "chromium",
            "chromium-browser",
            "google-chrome",
            "google-chrome-stable",
            "chrome",
        ]
    };
    let candidates = names.iter().flat_map(|name| {
        directories
            .iter()
            .map(move |directory| directory.join(name))
    });
    if let Some(path) = candidates.into_iter().find(|path| is_executable(path)) {
        return Ok(path);
    }
    #[cfg(target_os = "macos")]
    for path in [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ] {
        if is_executable(Path::new(path)) {
            return Ok(PathBuf::from(path));
        }
    }
    #[cfg(windows)]
    for root in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(root) {
            for suffix in [
                "Google/Chrome/Application/chrome.exe",
                "Microsoft/Edge/Application/msedge.exe",
            ] {
                let path = PathBuf::from(&root).join(suffix);
                if path.is_absolute() && is_executable(&path) {
                    return Ok(path);
                }
            }
        }
    }
    Err(error(
        "no installed Chromium/Chrome executable found; set PI_BROWSER_EXECUTABLE or attach explicitly with PI_BROWSER_CDP_URL",
    ))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn read_address(path: &Path) -> Result<Option<Address>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(failure) => return Err(failure.into()),
    };
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(error("invalid browser readiness file"));
    }
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    let file = std::fs::File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    #[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(error("browser readiness file is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    parse_address(&bytes)
}

fn parse_address(bytes: &[u8]) -> Result<Option<Address>> {
    if bytes.len() > 4096 {
        return Err(error("browser readiness file exceeds its byte limit"));
    }
    let text =
        std::str::from_utf8(bytes).map_err(|_| error("browser readiness file is not UTF-8"))?;
    let mut lines = text.lines();
    let Some(port) = lines.next() else {
        return Ok(None);
    };
    let Some(debugger_path) = lines.next() else {
        return Ok(None);
    };
    if debugger_path.is_empty() {
        return Ok(None);
    }
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| error("invalid browser debugging port"))?;
    let id = debugger_path
        .strip_prefix("/devtools/browser/")
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-')
        })
        .ok_or_else(|| error("invalid browser debugging path"))?;
    if lines.any(|line| !line.is_empty()) {
        return Err(error("unexpected data in browser readiness file"));
    }
    Ok(Some(Address {
        http: policy::endpoint(&format!("http://127.0.0.1:{port}"), false)?,
        debugger_path: format!("/devtools/browser/{id}"),
    }))
}

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn explicit_host_choices_win_without_reading_ambient_configuration() {
        let forbidden = |_: &str| -> Option<OsString> {
            panic!("explicit settings must not read the environment")
        };
        assert!(matches!(
            Connection::with_environment(Some("http://127.0.0.1:9222"), None, forbidden).unwrap(),
            Connection::Attach(_)
        ));
        let launch = BrowserLaunchOptions::default();
        assert!(matches!(
            Connection::with_environment(None, Some(&launch), forbidden).unwrap(),
            Connection::Managed(_)
        ));
        assert!(matches!(
            Connection::with_environment(None, None, |_| None).unwrap(),
            Connection::Managed(_)
        ));
    }

    #[test]
    fn launch_arguments_preserve_profiles_and_do_not_disable_security() {
        let options = BrowserLaunchOptions {
            user_agent: Some("Pi test --no-sandbox".into()),
            ..Default::default()
        };
        let args = arguments(Path::new("/tmp/profile with spaces"), &options);
        assert_eq!(
            args[0],
            OsStr::new("--user-data-dir=/tmp/profile with spaces")
        );
        assert!(args.contains(&OsString::from("--remote-debugging-port=0")));
        assert!(args.contains(&OsString::from("--user-agent=Pi test --no-sandbox")));
        for forbidden in [
            "--no-sandbox",
            "--disable-web-security",
            "--remote-allow-origins=*",
        ] {
            assert!(!args.contains(&OsString::from(forbidden)));
        }
        assert!(
            validate_options(&BrowserLaunchOptions {
                user_agent: Some("bad\nagent".into()),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn readiness_is_bounded_and_cannot_choose_a_remote_host_or_path() {
        let address = parse_address(b"43123\n/devtools/browser/abc-123")
            .unwrap()
            .unwrap();
        assert_eq!(address.http.as_str(), "http://127.0.0.1:43123/");
        assert_eq!(address.debugger_path, "/devtools/browser/abc-123");
        assert!(parse_address(b"43123\n").unwrap().is_none());
        for value in [
            "0\n/devtools/browser/id",
            "65536\n/devtools/browser/id",
            "42\nws://remote/id",
            "42\n/devtools/browser/../id",
            "42\n/devtools/browser/id?secret",
            "42\n/devtools/browser/id\nextra",
        ] {
            assert!(parse_address(value.as_bytes()).is_err(), "{value}");
        }
        assert!(parse_address(&vec![b'x'; 4097]).is_err());
    }

    #[cfg(unix)]
    fn fixture(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("test browser");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn real_process_readiness_and_owned_profile_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let executable = fixture(
            dir.path(),
            "for arg in \"$@\"; do case \"$arg\" in --user-data-dir=*) profile=${arg#--user-data-dir=};; esac; done\nprintf '43123\\n/devtools/browser/fixture-id' > \"$profile/DevToolsActivePort\"\nexec sleep 30",
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let options = BrowserLaunchOptions {
            executable_path: Some(executable),
            ..Default::default()
        };
        let mut browser = runtime
            .block_on(ManagedBrowser::launch(&owner, dir.path(), &options))
            .unwrap();
        let profile = browser.process.profile.path().to_path_buf();
        assert!(profile.is_dir());
        assert!(browser.running().unwrap());
        assert_eq!(
            browser.address.debugger_path,
            "/devtools/browser/fixture-id"
        );
        // Completing/cancelling the launching call must not lend that old Cx to
        // the next call. The session owns the established process until stop/drop.
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("launching call finished"),
        );
        assert!(browser.running().unwrap());
        browser.stop().unwrap();
        assert!(!browser.running().unwrap());
        drop(browser);
        assert!(!profile.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_and_early_exit_do_not_become_a_ready_browser() {
        let dir = tempfile::tempdir().unwrap();
        let executable = fixture(dir.path(), "exit 7");
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let options = BrowserLaunchOptions {
            executable_path: Some(executable),
            ..Default::default()
        };
        assert!(
            runtime
                .block_on(ManagedBrowser::launch(&owner, dir.path(), &options))
                .is_err()
        );
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel before launch"),
        );
        assert!(Process::spawn(&owner, dir.path(), &options).is_err());
    }
}
