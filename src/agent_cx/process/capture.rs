//! Bounded pipe collection without blocking readers, spool files or threads.

use super::{AgentChild, cancelled, check_wait};
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::process::Output;
use std::time::{Duration, Instant};

const CAPTURE_POLL: Duration = Duration::from_millis(2);
const READS_PER_TICK: usize = 8;

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "agent process output timed out; side effects may already have occurred",
    )
}

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
                    return Err(io::Error::other(
                        "agent process output exceeded its combined byte limit",
                    ));
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
        drop(self.take_stdin());
        let mut stdout_pipe = self.take_stdout();
        let mut stderr_pipe = self.take_stderr();
        if let Some(pipe) = stdout_pipe.as_ref() {
            nonblocking(pipe)?;
        }
        if let Some(pipe) = stderr_pipe.as_ref() {
            nonblocking(pipe)?;
        }
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut remaining = max_bytes;
        loop {
            self.owner.checkpoint().map_err(|_| cancelled())?;
            if started.elapsed() >= timeout {
                return Err(timed_out());
            }
            drain(&mut stdout_pipe, &mut stdout, &mut remaining)?;
            drain(&mut stderr_pipe, &mut stderr, &mut remaining)?;
            self.owner.checkpoint().map_err(|_| cancelled())?;
            if let Some(status) = self.try_wait()?
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
            self.owner
                .time()
                .sleep(CAPTURE_POLL.min(timeout.saturating_sub(started.elapsed())))
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_cx::{AgentCommand, AgentCx};
    use std::io::Cursor;
    use std::process::{Command, Stdio};

    fn command(owner: AgentCx, script: &str) -> AgentCommand {
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
            command(AgentCx::for_current_or_request(), script)
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
            let mut child = command(AgentCx::for_current_or_request(), "printf saved; exit 4")
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
                AgentCx::for_current_or_request(),
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
            let child = command(AgentCx::for_current_or_request(), "exec sleep 30")
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
            let child = command(owner.clone(), "exec sleep 30").spawn().unwrap();
            let pid = child.id();
            let mut capture = Box::pin(child.wait_with_output_limited(1024, Duration::from_secs(30)));
            assert!(futures::poll!(&mut capture).is_pending());
            owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel capture"));
            assert_eq!(capture.await.unwrap_err().kind(), io::ErrorKind::Interrupted);
            assert_reaped(pid);
        });
    }

    #[test]
    fn dropping_an_unpolled_capture_still_reaps_its_owned_child() {
        let child = command(AgentCx::for_request(), "exec sleep 30")
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
            let child = command(AgentCx::for_current_or_request(), "exec sleep 30")
                .spawn()
                .unwrap();
            let pid = child.id();
            let mut capture = Box::pin(child.wait_with_output_limited(1024, Duration::from_secs(30)));
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
                AgentCx::for_current_or_request(),
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
        drain(&mut pipe, &mut bytes, &mut remaining).unwrap();
        assert_eq!(bytes.len(), READS_PER_TICK * 8192);
        assert!(pipe.is_some());
    }

    #[test]
    fn drain_distinguishes_exact_eof_from_overflow_without_retaining_excess() {
        let mut pipe = Some(Cursor::new(b"exact"));
        let mut bytes = Vec::new();
        let mut remaining = 5;
        drain(&mut pipe, &mut bytes, &mut remaining).unwrap();
        assert!(pipe.is_none());
        assert_eq!(bytes, b"exact");
        assert_eq!(remaining, 0);
        let mut pipe = Some(Cursor::new(b"extra"));
        assert!(drain(&mut pipe, &mut bytes, &mut remaining).is_err());
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
