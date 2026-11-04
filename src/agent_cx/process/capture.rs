//! Bounded pipe collection without blocking readers or per-child worker threads.

use super::{AgentChild, AgentCommand, cancelled, check_spawn, check_wait};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::AsFd;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

const CAPTURE_POLL: Duration = Duration::from_millis(2);
const READS_PER_TICK: usize = 8;

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "agent process output timed out; side effects may already have occurred",
    )
}

#[cfg(unix)]
fn nonblocking(pipe: &impl AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(pipe).map_err(io::Error::from)?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK).map_err(io::Error::from)
}

// Each stream gets a bounded turn even when the other is a continuous writer.
// Read one extra byte at the limit to distinguish exact-size output from
// overflow without retaining the excess byte or requiring a larger buffer.
fn drain<R: Read>(
    pipe: &mut Option<R>,
    bytes: &mut Vec<u8>,
    remaining: &mut usize,
    per_stream: bool,
) -> io::Result<()> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    let mut buffer = [0_u8; 8192];
    for _ in 0..READS_PER_TICK {
        let capacity = remaining.saturating_add(1).min(buffer.len());
        match reader.read(&mut buffer[..capacity]) {
            Ok(0) => {
                drop(pipe.take());
                return Ok(());
            }
            Ok(count) => {
                if count > *remaining {
                    return Err(if per_stream {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "agent process output exceeded a per-stream byte limit",
                        )
                    } else {
                        io::Error::other("agent process output exceeded its combined byte limit")
                    });
                }
                bytes.extend_from_slice(&buffer[..count]);
                *remaining -= count;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

enum CaptureLimits {
    #[cfg(unix)]
    Combined(usize),
    Separate {
        stdout: usize,
        stderr: usize,
    },
}

async fn collect<O: Read, E: Read>(
    mut child: AgentChild,
    mut stdout_pipe: Option<O>,
    mut stderr_pipe: Option<E>,
    mut limits: CaptureLimits,
    started: Instant,
    timeout: Duration,
) -> io::Result<Output> {
    drop(child.take_stdin());
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        child.owner.checkpoint().map_err(|_| cancelled())?;
        if started.elapsed() >= timeout {
            return Err(timed_out());
        }
        match &mut limits {
            #[cfg(unix)]
            CaptureLimits::Combined(remaining) => {
                drain(&mut stdout_pipe, &mut stdout, remaining, false)?;
                drain(&mut stderr_pipe, &mut stderr, remaining, false)?;
            }
            CaptureLimits::Separate {
                stdout: stdout_remaining,
                stderr: stderr_remaining,
            } => {
                drain(&mut stdout_pipe, &mut stdout, stdout_remaining, true)?;
                drain(&mut stderr_pipe, &mut stderr, stderr_remaining, true)?;
            }
        }
        child.owner.checkpoint().map_err(|_| cancelled())?;
        if let Some(status) = child.try_wait()?
            && stdout_pipe.is_none()
            && stderr_pipe.is_none()
        {
            if started.elapsed() >= timeout {
                return Err(timed_out());
            }
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        child
            .owner
            .time()
            .sleep(CAPTURE_POLL.min(timeout.saturating_sub(started.elapsed())))
            .await;
    }
}

#[cfg(windows)]
struct WindowsReader(crate::jobs::windows_job_pipe::Reader);

#[cfg(windows)]
impl Read for WindowsReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // The ordinary Read implementation waits for a readiness notification.
        // Async capture must only poll the existing IOCP-backed reader.
        self.0.try_read(buffer)
    }
}

impl AgentCommand {
    /// Spawn with separate bounded stdout/stderr capture under this owner.
    /// The consumed builder is dropped immediately after spawn, including its
    /// parent-side Windows writer handles. Cancellation, deadline, overflow or
    /// future drop retires the pipes and kills/reaps the owned child. The
    /// deadline starts before pipe setup/spawn; OS cleanup is synchronous and
    /// is not a hard real-time bound. No partial output is returned on failure.
    pub(crate) async fn output_with_limits(
        mut self,
        stdout_limit: usize,
        stderr_limit: usize,
        timeout: Duration,
    ) -> io::Result<Output> {
        check_spawn(&self.owner)?;
        check_wait(&self.owner)?;
        let started = Instant::now();
        if timeout.is_zero() {
            return Err(timed_out());
        }

        #[cfg(unix)]
        let (child, stdout, stderr) = {
            self.stdout(Stdio::piped()).stderr(Stdio::piped());
            if started.elapsed() >= timeout {
                return Err(timed_out());
            }
            let mut child = self.spawn()?;
            drop(self);
            let stdout = child.take_stdout();
            let stderr = child.take_stderr();
            if let Some(pipe) = stdout.as_ref() {
                nonblocking(pipe)?;
            }
            if let Some(pipe) = stderr.as_ref() {
                nonblocking(pipe)?;
            }
            (child, stdout, stderr)
        };

        #[cfg(windows)]
        let (child, stdout, stderr) = {
            let (stdout, stdout_writer) = crate::jobs::windows_job_pipe::pair()?;
            let (stderr, stderr_writer) = crate::jobs::windows_job_pipe::pair()?;
            self.stdout(Stdio::from(stdout_writer))
                .stderr(Stdio::from(stderr_writer));
            if started.elapsed() >= timeout {
                return Err(timed_out());
            }
            let child = self.spawn()?;
            drop(self);
            (
                child,
                Some(WindowsReader(stdout)),
                Some(WindowsReader(stderr)),
            )
        };

        collect(
            child,
            stdout,
            stderr,
            CaptureLimits::Separate {
                stdout: stdout_limit,
                stderr: stderr_limit,
            },
            started,
            timeout,
        )
        .await
    }
}

#[cfg(unix)]
impl AgentChild {
    /// Drain the remaining piped stdout and stderr while waiting for completion.
    ///
    /// `max_bytes` bounds their combined retained bytes, not each stream
    /// independently. Non-piped or previously extracted streams yield empty
    /// output. Configure pipes on the command before spawning; stdin still
    /// owned by this handle is closed, as in [`Self::wait`]. Output remains raw
    /// bytes and a nonzero exit status is returned rather than hidden as an I/O
    /// error. Collection also works after an earlier successful wait.
    ///
    /// The timeout starts when this future is first polled and covers both
    /// process exit and pipe EOF. Owner cancellation, timeout, overflow, an I/O
    /// error, or dropping this future kills and reaps the owned process through
    /// `AgentChild`'s cleanup guard. No partial output is reported as success.
    /// Cleanup still has the synchronous OS limitations documented on the
    /// process wrapper; this is not a hard real-time deadline.
    ///
    /// Unix-only: this implementation uses nonblocking pipe descriptors, not
    /// detached reader threads that could outlive cancellation.
    pub async fn wait_with_output_limited(
        mut self,
        max_bytes: usize,
        timeout: Duration,
    ) -> io::Result<Output> {
        check_wait(&self.owner)?;
        self.owner.checkpoint().map_err(|_| cancelled())?;
        let started = Instant::now();
        let stdout_pipe = self.take_stdout();
        let stderr_pipe = self.take_stderr();
        if let Some(pipe) = stdout_pipe.as_ref() {
            nonblocking(pipe)?;
        }
        if let Some(pipe) = stderr_pipe.as_ref() {
            nonblocking(pipe)?;
        }
        collect(
            self,
            stdout_pipe,
            stderr_pipe,
            CaptureLimits::Combined(max_bytes),
            started,
            timeout,
        )
        .await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent_cx::{AgentCommand, AgentCx};
    use std::io::Cursor;
    use std::process::{Command, Stdio};

    fn command(owner: &AgentCx, script: &str) -> AgentCommand {
        let mut command = owner.process().command("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn capture(script: &str, limit: usize) -> io::Result<Output> {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            command(&AgentCx::for_current_or_request(), script)
                .spawn()?
                .wait_with_output_limited(limit, Duration::from_secs(10))
                .await
        })
    }

    fn assert_reaped(pid: u32) {
        let status = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "owned process {pid} was not reaped");
    }

    #[test]
    fn both_pipes_can_exceed_pipe_capacity_without_deadlocking() {
        let output = capture(
            "head -c 131072 /dev/zero; head -c 131072 /dev/zero >&2; exit 7",
            262_144,
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, vec![0; 131_072]);
        assert_eq!(output.stderr, vec![0; 131_072]);
    }

    #[test]
    fn byte_limit_is_shared_and_exact_limit_output_is_accepted() {
        let output = capture("printf abc; printf xyz >&2", 6).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"abc");
        assert_eq!(output.stderr, b"xyz");
        let error = capture("printf abc; printf xyz >&2", 5).unwrap_err();
        assert!(error.to_string().contains("combined byte limit"));
        assert!(!error.to_string().contains("abc"));
        assert!(!error.to_string().contains("xyz"));
    }

    #[test]
    fn consumed_command_enforces_independent_stream_limits() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let output = command(&owner, "printf abc; printf xyz >&2; exit 7")
                .output_with_limits(3, 3, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(output.status.code(), Some(7));
            assert_eq!(output.stdout, b"abc");
            assert_eq!(output.stderr, b"xyz");
            for script in ["printf abcd", "printf xyzw >&2"] {
                let error = command(&owner, script)
                    .output_with_limits(3, 3, Duration::from_secs(5))
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(!error.to_string().contains("abcd"));
                assert!(!error.to_string().contains("xyzw"));
            }
        });
    }

    #[test]
    fn consumed_capture_refuses_cancellation_capability_and_deadline_before_spawn() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for mode in ["cancelled", "denied", "expired"] {
            let directory = tempfile::tempdir().unwrap();
            let owner = if mode == "denied" {
                let restricted =
                    asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
                let _guard = restricted.set_current_restricted();
                AgentCx::for_current_or_request()
            } else {
                AgentCx::for_request()
            };
            if mode == "cancelled" {
                owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel helper"));
            }
            let mut command = command(&owner, "printf started > helper-started");
            command.current_dir(directory.path());
            let timeout = if mode == "expired" {
                Duration::ZERO
            } else {
                Duration::from_secs(5)
            };
            let error = runtime
                .block_on(command.output_with_limits(64, 64, timeout))
                .unwrap_err();
            assert_eq!(
                error.kind(),
                match mode {
                    "cancelled" => io::ErrorKind::Interrupted,
                    "denied" => io::ErrorKind::PermissionDenied,
                    _ => io::ErrorKind::TimedOut,
                }
            );
            assert!(!directory.path().join("helper-started").exists());
        }
    }

    #[test]
    fn consumed_capture_cancel_and_drop_reap_a_started_helper() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        for cancel in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
            runtime.block_on(async {
                let mut command = command(&owner, "echo $$ > helper.pid; exec sleep 30");
                command.current_dir(directory.path());
                let mut pending =
                    Box::pin(command.output_with_limits(64, 64, Duration::from_secs(20)));
                let deadline = Instant::now() + Duration::from_secs(10);
                let pid = loop {
                    assert!(futures::poll!(&mut pending).is_pending());
                    if let Ok(text) = std::fs::read_to_string(directory.path().join("helper.pid"))
                        && let Ok(pid) = text.trim().parse::<u32>()
                    {
                        break pid;
                    }
                    assert!(Instant::now() < deadline, "helper did not become ready");
                    owner.time().sleep(Duration::from_millis(2)).await;
                };
                if cancel {
                    owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel capture"));
                    assert_eq!(
                        pending.await.unwrap_err().kind(),
                        io::ErrorKind::Interrupted
                    );
                } else {
                    drop(pending);
                }
                assert_reaped(pid);
            });
        }
    }

    #[test]
    fn closes_unclaimed_stdin_and_accepts_empty_output_at_zero_limit() {
        let output = capture("cat", 0).unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert!(capture("printf x", 0).is_err());
    }

    #[test]
    fn output_collection_can_follow_an_earlier_wait() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut child = command(&AgentCx::for_current_or_request(), "printf saved; exit 4")
                .spawn()
                .unwrap();
            assert_eq!(child.wait().await.unwrap().code(), Some(4));
            let output = child
                .wait_with_output_limited(5, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(output.status.code(), Some(4));
            assert_eq!(output.stdout, b"saved");
        });
    }

    #[test]
    fn discarded_stderr_is_not_captured_or_charged_to_the_limit() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let output = command(
                &AgentCx::for_current_or_request(),
                "head -c 131072 /dev/zero >&2; printf ok",
            )
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
            .wait_with_output_limited(2, Duration::from_secs(5))
            .await
            .unwrap();
            assert_eq!(output.stdout, b"ok");
            assert!(output.stderr.is_empty());
        });
    }

    #[test]
    fn timeout_kills_and_reaps_instead_of_returning_partial_success() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let child = command(&AgentCx::for_current_or_request(), "exec sleep 30")
                .spawn()
                .unwrap();
            let pid = child.id();
            let error = child
                .wait_with_output_limited(1024, Duration::ZERO)
                .await
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert_reaped(pid);
        });
    }

    #[test]
    fn owner_cancellation_retires_an_idle_pending_capture() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        runtime.block_on(async {
            let child = command(&owner, "exec sleep 30").spawn().unwrap();
            let pid = child.id();
            let mut capture =
                Box::pin(child.wait_with_output_limited(1024, Duration::from_secs(30)));
            assert!(futures::poll!(&mut capture).is_pending());
            owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel capture"));
            assert_eq!(
                capture.await.unwrap_err().kind(),
                io::ErrorKind::Interrupted
            );
            assert_reaped(pid);
        });
    }

    #[test]
    fn dropping_an_unpolled_capture_still_reaps_its_owned_child() {
        let child = command(&AgentCx::for_request(), "exec sleep 30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let capture = child.wait_with_output_limited(1024, Duration::from_secs(30));
        drop(capture);
        assert_reaped(pid);
    }

    #[test]
    fn dropping_a_pending_capture_still_reaps_its_owned_child() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let child = command(&AgentCx::for_current_or_request(), "exec sleep 30")
                .spawn()
                .unwrap();
            let pid = child.id();
            let mut capture =
                Box::pin(child.wait_with_output_limited(1024, Duration::from_secs(30)));
            assert!(futures::poll!(&mut capture).is_pending());
            drop(capture);
            assert_reaped(pid);
        });
    }

    #[test]
    fn overflow_reaps_a_child_that_would_otherwise_keep_running() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let child = command(
                &AgentCx::for_current_or_request(),
                "printf overflow; exec sleep 30",
            )
            .spawn()
            .unwrap();
            let pid = child.id();
            let error = child
                .wait_with_output_limited(1, Duration::from_secs(5))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("combined byte limit"));
            assert_reaped(pid);
        });
    }

    #[test]
    fn drain_bounds_work_even_for_a_reader_that_never_blocks() {
        let mut pipe = Some(io::repeat(42));
        let mut bytes = Vec::new();
        let mut remaining = usize::MAX;
        drain(&mut pipe, &mut bytes, &mut remaining, false).unwrap();
        assert_eq!(bytes.len(), READS_PER_TICK * 8192);
        assert!(pipe.is_some());
    }

    #[test]
    fn drain_distinguishes_exact_eof_from_overflow_without_retaining_excess() {
        let mut pipe = Some(Cursor::new(b"exact"));
        let mut bytes = Vec::new();
        let mut remaining = 5;
        drain(&mut pipe, &mut bytes, &mut remaining, false).unwrap();
        assert!(pipe.is_none());
        assert_eq!(bytes, b"exact");
        assert_eq!(remaining, 0);
        let mut pipe = Some(Cursor::new(b"extra"));
        assert!(drain(&mut pipe, &mut bytes, &mut remaining, false).is_err());
        assert_eq!(bytes, b"exact");
    }

    #[test]
    fn exited_shell_cannot_leave_a_background_writer_holding_capture_open() {
        let output = capture("sleep 30 & printf parent-done", 64).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"parent-done");
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn denied_wait_authority_cleans_up_without_using_the_pollers_authority() {
        let raw = Command::new("/bin/sh")
            .args(["-c", "exec sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = raw.id();
        let owner = {
            let _guard = asupersync::Cx::for_request()
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            AgentCx::for_current_or_request()
        };
        // This private constructor models transferring an already-created
        // child into a timerless scope; the public spawn API denies that owner.
        let child = AgentChild::new(owner, raw);
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let error = runtime
            .block_on(child.wait_with_output_limited(1024, Duration::from_secs(5)))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_reaped(pid);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::agent_cx::AgentCx;

    #[test]
    fn consumed_windows_capture_releases_writer_handles_and_limits_both_streams() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let mut command = owner.process().command("cmd.exe");
            command
                .args(["/D", "/C", "echo out&echo err 1>&2&exit /B 7"])
                .stdin(Stdio::null());
            let output = command
                .output_with_limits(32, 32, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(output.status.code(), Some(7));
            assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "out");
            assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), "err");
            for script in ["echo private-stdout", "echo private-stderr 1>&2"] {
                let mut command = owner.process().command("cmd.exe");
                command.args(["/D", "/C", script]).stdin(Stdio::null());
                let error = command
                    .output_with_limits(3, 3, Duration::from_secs(5))
                    .await
                    .unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                assert!(!error.to_string().contains("private-stdout"));
                assert!(!error.to_string().contains("private-stderr"));
            }
        });
    }
}
