//! Capability-checked process dispatch and an owned subprocess lifetime.
//!
//! Constructing a command performs no process I/O. Spawn checks the captured
//! owner's I/O and spawn capabilities, never the polling caller's authority.
//! Owned children are killed and reaped on cancellation or drop. Cleanup is
//! synchronous and is not a hard real-time bound on foreign OS operations.

#[cfg(unix)]
mod capture;

use super::{AgentCx, AgentProcess};
use std::ffi::OsStr;
use std::io;
use std::ops::Deref;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::Duration;

fn cancelled() -> io::Error {
    io::Error::new(
        io::ErrorKind::Interrupted,
        "agent process operation cancelled",
    )
}

fn check_spawn(owner: &AgentCx) -> io::Result<()> {
    if !owner.capabilities().io || !owner.capabilities().spawn {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "agent process spawn requires I/O and spawn capabilities",
        ));
    }
    owner.checkpoint().map_err(|_| cancelled())
}

fn check_wait(owner: &AgentCx) -> io::Result<()> {
    if !owner.capabilities().time {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "waiting for an agent process requires timer capability",
        ));
    }
    Ok(())
}

impl AgentProcess<'_> {
    /// Dispatch for internal runners that already own process-group cleanup,
    /// pipe draining and reaping. The returned child must enter that guard
    /// immediately; public callers use `command(...).spawn()` instead.
    pub(crate) fn spawn_checked(&self, command: &mut Command) -> io::Result<Child> {
        check_spawn(self.cx)?;
        let _guard = self.cx.cx().clone().set_current_restricted();
        command.spawn()
    }
}

/// A command builder that cannot lose its captured owner through `DerefMut`.
/// Read-only `Command` inspection remains available through `Deref`.
pub struct AgentCommand {
    owner: AgentCx,
    command: Command,
}

impl AgentCommand {
    pub(super) fn new(owner: AgentCx, program: impl AsRef<OsStr>) -> Self {
        Self {
            owner,
            command: Command::new(program),
        }
    }

    pub fn arg(&mut self, argument: impl AsRef<OsStr>) -> &mut Self {
        self.command.arg(argument);
        self
    }

    pub fn args<I, S>(&mut self, arguments: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command.args(arguments);
        self
    }

    pub fn current_dir(&mut self, directory: impl AsRef<Path>) -> &mut Self {
        self.command.current_dir(directory);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.command.env(key, value);
        self
    }

    pub fn envs<I, K, V>(&mut self, variables: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.command.envs(variables);
        self
    }

    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.command.env_remove(key);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.command.env_clear();
        self
    }

    pub fn stdin(&mut self, input: Stdio) -> &mut Self {
        self.command.stdin(input);
        self
    }

    pub fn stdout(&mut self, output: Stdio) -> &mut Self {
        self.command.stdout(output);
        self
    }

    pub fn stderr(&mut self, output: Stdio) -> &mut Self {
        self.command.stderr(output);
        self
    }

    pub fn spawn(&mut self) -> io::Result<AgentChild> {
        check_spawn(&self.owner)?;
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            self.command.process_group(0);
        }
        let child = self.owner.process().spawn_checked(&mut self.command)?;
        let mut child = AgentChild::new(self.owner.clone(), child);
        crate::tools::attach_child_job_discipline(child.child.as_ref().expect("owned child"));
        // A cancellation concurrent with spawn cannot leave an unowned process.
        if self.owner.checkpoint().is_err() {
            let _ = child.terminate();
            return Err(cancelled());
        }
        Ok(child)
    }

    /// Spawn and wait, retaining cancellation and cleanup ownership throughout.
    /// Waiting requires timer capability; check that before starting a process.
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        check_spawn(&self.owner)?;
        check_wait(&self.owner)?;
        self.spawn()?.wait().await
    }
}

impl Deref for AgentCommand {
    type Target = Command;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

/// Owns a subprocess until reaped.
///
/// Extracted pipe handles are ordinary OS handles; they do not transfer or
/// disable this child's cleanup ownership. Unclaimed stdout and stderr remain
/// available after reaping or killing the process. Waiting only closes stdin;
/// it must not discard unread output.
pub struct AgentChild {
    owner: AgentCx,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    id: u32,
    status: Option<ExitStatus>,
    descendants_stopped: bool,
    cancelled: bool,
}

impl AgentChild {
    fn new(owner: AgentCx, mut child: Child) -> Self {
        Self {
            owner,
            id: child.id(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child: Some(child),
            status: None,
            descendants_stopped: false,
            cancelled: false,
        }
    }

    #[must_use]
    pub const fn id(&self) -> u32 {
        self.id
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut().and_then(|child| child.stdin.take())
    }

    pub const fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    pub const fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.cancelled {
            return Err(cancelled());
        }
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        if self.owner.checkpoint().is_err() {
            self.cancelled = true;
            self.terminate()?;
            return Err(cancelled());
        }
        let status = match self.child.as_mut() {
            Some(child) => child.try_wait()?,
            None => return Err(io::Error::other("agent process has no waitable child")),
        };
        if let Some(status) = status {
            self.stop_descendants();
            self.status = Some(status);
            drop(self.child.take());
        }
        Ok(status)
    }

    /// Wait for completion, closing stdin still owned by this handle first.
    /// A caller that extracted stdin must close that separate handle itself.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(child) = self.child.as_mut() {
            drop(child.stdin.take());
        }
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            check_wait(&self.owner)?;
            self.owner.time().sleep(Duration::from_millis(10)).await;
        }
    }

    /// Kill the owned process tree and reap its root, even when the owner has
    /// already been cancelled. Cleanup must not require renewed authority.
    pub fn kill(&mut self) -> io::Result<()> {
        self.terminate()
    }

    fn stop_descendants(&mut self) {
        if self.descendants_stopped {
            return;
        }
        self.descendants_stopped = true;
        #[cfg(unix)]
        if let Ok(pid) = i32::try_from(self.id)
            && let Some(group) = rustix::process::Pid::from_raw(pid)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        #[cfg(not(unix))]
        crate::tools::kill_process_tree(Some(self.id));
    }

    fn terminate(&mut self) -> io::Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        self.stop_descendants();
        let child = self.child.as_mut().expect("owned child");
        let _ = child.kill();
        // Retain the handle if wait fails so Drop can still attempt cleanup.
        self.status = Some(child.wait()?);
        drop(self.child.take());
        Ok(())
    }
}

impl Drop for AgentChild {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use asupersync::Budget;
    use asupersync::Cx;

    fn restricted_owner() -> AgentCx {
        let restricted = Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        AgentCx::for_current_or_request()
    }

    #[test]
    fn command_cannot_use_the_callers_spawn_authority() {
        let owner = restricted_owner();
        let mut command = AgentCommand::new(owner, "must-not-be-executed");
        let _caller = Cx::for_request().set_current_restricted();
        let error = command.spawn().err().expect("spawn denied");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn cancellation_between_construction_and_spawn_prevents_dispatch() {
        let owner = AgentCx::for_request();
        let mut command = AgentCommand::new(owner.clone(), "must-not-be-executed");
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel before spawn"),
        );
        let error = command.spawn().err().expect("cancelled spawn");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn command_inspection_and_configuration_preserve_owner() {
        let owner = restricted_owner();
        let mut command = AgentCommand::new(owner, "fixture");
        command
            .args(["one", "two"])
            .env_clear()
            .env("PI_TEST", "value");
        assert_eq!(command.get_program(), OsStr::new("fixture"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [OsStr::new("one"), OsStr::new("two")]
        );
        assert_eq!(
            command.spawn().err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[test]
    fn wait_returns_real_status_and_keeps_it_after_reaping() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let mut child = AgentCommand::new(owner, "sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        let status = runtime.block_on(child.wait()).unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(child.try_wait().unwrap(), Some(status));
        assert!(child.child.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_wait_terminates_and_reaps_the_child() {
        let owner = AgentCx::for_request();
        let mut child = AgentCommand::new(owner.clone(), "sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel running child"),
        );
        assert_eq!(
            child.try_wait().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(child.child.is_none());
        assert!(child.status.is_some());
        assert!(child.descendants_stopped);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_an_owned_child_does_not_leave_its_root_running() {
        let child = AgentCommand::new(AgentCx::for_request(), "sh")
            .args(["-c", "exec sleep 30"])
            .spawn()
            .unwrap();
        let pid = child.id().to_string();
        drop(child);
        let status = Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "the owned root must already be reaped");
    }

    #[cfg(unix)]
    #[test]
    fn wait_closes_unclaimed_stdin_before_waiting_for_eof() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let mut child = AgentCommand::new(owner, "sh")
            .args(["-c", "read value; test $? -ne 0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        assert!(child.child.as_ref().unwrap().stdin.is_some());
        assert!(runtime.block_on(child.wait()).unwrap().success());
        assert!(child.child.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn wait_preserves_both_output_pipes_and_nonzero_status() {
        use std::io::Read as _;

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let mut child = AgentCommand::new(owner, "sh")
            .args(["-c", "printf 'out\\000tail'; printf err >&2; exit 7"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let status = runtime.block_on(child.wait()).unwrap();
        assert_eq!(status.code(), Some(7));
        assert_eq!(runtime.block_on(child.wait()).unwrap(), status);
        assert!(child.child.is_none());
        child.kill().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        child
            .take_stdout()
            .unwrap()
            .read_to_end(&mut stdout)
            .unwrap();
        child
            .take_stderr()
            .unwrap()
            .read_to_end(&mut stderr)
            .unwrap();
        assert_eq!(stdout, b"out\0tail");
        assert_eq!(stderr, b"err");
        assert!(child.take_stdout().is_none());
        assert!(child.take_stderr().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn try_wait_preserves_unclaimed_output_after_reaping() {
        use std::io::Read as _;
        use std::time::Instant;

        let mut child = AgentCommand::new(AgentCx::for_request(), "sh")
            .args(["-c", "printf complete"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "child did not exit");
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut stdout = String::new();
        child
            .take_stdout()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        assert_eq!(stdout, "complete");
    }

    #[cfg(unix)]
    #[test]
    fn extracted_output_survives_wait_without_duplication() {
        use std::io::Read as _;

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let mut child = AgentCommand::new(owner, "sh")
            .args(["-c", "printf complete"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut pipe = child.take_stdout().unwrap();
        assert!(runtime.block_on(child.wait()).unwrap().success());
        assert!(child.take_stdout().is_none());
        let mut stdout = String::new();
        pipe.read_to_string(&mut stdout).unwrap();
        assert_eq!(stdout, "complete");
    }

    #[cfg(unix)]
    #[test]
    fn reaping_does_not_invent_output_for_unpiped_streams() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(Budget::new()));
        let mut child = AgentCommand::new(owner, "sh")
            .args(["-c", "exit 0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        assert!(runtime.block_on(child.wait()).unwrap().success());
        assert!(child.take_stdout().is_none());
        assert!(child.take_stderr().is_none());
    }
}
