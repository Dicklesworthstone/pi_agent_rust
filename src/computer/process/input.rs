//! Owned desktop input transport. A single writer keeps stdin open as a lease.
//!
//! On cancellation/drop the pipe closes before process teardown, giving the
//! already-running helper a bounded chance to release its held keys/buttons.
//! No fresh spawn authority, detached pump, or blocking stdin write is needed.

use super::{error, native};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const INPUT_LIMIT: usize = 32 * 1024;
const OUTPUT_LIMIT: usize = 4096;
const CLEANUP_GRACE: Duration = Duration::from_millis(500);

struct InputChild {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
}

fn nonblocking(fd: &impl AsFd) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(std::io::Error::from)
}

impl InputChild {
    fn spawn(owner: &AgentCx, program: &Path, args: &[String]) -> Result<Self> {
        native::check_owner(owner)?;
        let mut command = Command::new(program);
        command.args(args).current_dir("/").process_group(0)
            .env("PI_COMPUTER_OWNED_INPUT", "1")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        // Enter cleanup ownership immediately, before any fallible pipe setup.
        let child = owner.process().spawn_checked(&mut command)
            .map_err(|_| error("cannot start owned desktop input helper"))?;
        let mut owned = Self { child, input: None, output: None };
        owned.input = owned.child.stdin.take();
        owned.output = owned.child.stdout.take();
        nonblocking(owned.input.as_ref().ok_or_else(|| error("missing input lease"))?)?;
        nonblocking(owned.output.as_ref().ok_or_else(|| error("missing input reply"))?)?;
        native::check_owner(owner)?;
        Ok(owned)
    }

    fn stop_group(&mut self) {
        if let Ok(pid) = i32::try_from(self.child.id())
            && let Some(group) = rustix::process::Pid::from_raw(pid)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

impl Drop for InputChild {
    fn drop(&mut self) {
        // EOF is cancellation, not success. The helper is the only emitter and
        // releases before replying/exiting. Keep stdout open during this grace
        // so a small terminal reply cannot prevent its normal exit.
        drop(self.input.take());
        drop(self.child.stdin.take());
        let deadline = Instant::now() + CLEANUP_GRACE;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => { self.stop_group(); return; }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(None) | Err(_) => break,
            }
        }
        self.stop_group();
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Reaping/foreign filesystem operations are not hard real-time. A
        // crashed or uncooperative helper cannot guarantee OS input release.
    }
}

fn write_frame(pipe: &mut ChildStdin, frame: &[u8], written: &mut usize) -> Result<()> {
    for _ in 0..8 {
        if *written == frame.len() { break; }
        let end = frame.len().min(*written + 4096);
        match pipe.write(&frame[*written..end]) {
            Ok(0) => return Err(error("desktop input helper closed its request channel")),
            Ok(count) => *written += count,
            Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(failure) if failure.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(error("desktop input helper request channel failed")),
        }
    }
    Ok(())
}

/// Send exactly one bounded JSON line, then retain stdin until helper exit.
/// The helper must check for EOF between input events and release on unwind.
/// This is deliberately separate from finite helpers that read stdin to EOF.
pub(in crate::computer) async fn run(
    owner: &AgentCx,
    helpers: &BTreeMap<String, PathBuf>,
    args: &[String],
    input: &[u8],
) -> Result<Vec<u8>> {
    native::check_owner(owner)?;
    if input.is_empty() || input.len() > INPUT_LIMIT || input.contains(&b'\n') {
        return Err(error("desktop input requires one bounded JSON frame"));
    }
    let mut frame = input.to_vec();
    frame.push(b'\n');
    let program = helpers.get("swift").map_or(Path::new("/usr/bin/swift"), PathBuf::as_path);
    let mut owned = InputChild::spawn(owner, program, args)?;
    let mut written = 0;
    let mut bytes = Vec::new();
    let mut eof = false;
    let mut status: Option<ExitStatus> = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        native::check_owner(owner)?;
        if Instant::now() >= deadline {
            return Err(error("desktop input helper timed out; side effects may already have occurred"));
        }
        if written < frame.len() {
            write_frame(owned.input.as_mut().expect("owned input lease"), &frame, &mut written)?;
        }
        if !eof {
            eof = super::drain(owned.output.as_mut().expect("owned input reply"), &mut bytes, OUTPUT_LIMIT)?;
        }
        if status.is_none() {
            status = owned.child.try_wait()?;
        }
        if let Some(status) = status {
            if !status.success() || written != frame.len() {
                return Err(error("desktop input helper failed; release was attempted but OS effects may remain"));
            }
            if eof { return Ok(bytes); }
        }
        // Bound ready work, input writes and output reads on every iteration.
        owner.time().sleep(Duration::from_millis(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll};

    fn shell() -> BTreeMap<String, PathBuf> {
        BTreeMap::from([("swift".into(), PathBuf::from("/bin/sh"))])
    }

    #[test]
    fn one_literal_frame_is_delivered_without_waiting_for_eof() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let input = br#"{"text":"$(touch never); \"quoted\"\\n"}"#;
        let result = runtime.block_on(async {
            run(&AgentCx::for_current_or_request(), &shell(),
                &super::super::strings(&["-c", "IFS= read -r frame; printf '%s' \"$frame\""]), input).await
        }).unwrap();
        assert_eq!(result, input);
    }

    #[test]
    fn output_limits_and_failed_helpers_never_echo_private_output() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        for script in ["IFS= read -r frame; printf PRIVATE-CANARY; exit 7",
            "IFS= read -r frame; head -c 4097 /dev/zero"]
        {
            let failure = runtime.block_on(async {
                run(&AgentCx::for_current_or_request(), &shell(),
                    &super::super::strings(&["-c", script]), b"{}").await.unwrap_err()
            });
            assert!(!failure.to_string().contains("PRIVATE-CANARY"));
        }
    }

    #[test]
    fn invalid_frames_and_denied_owners_do_not_spawn() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let helpers = BTreeMap::from([("swift".into(), PathBuf::from("/must-not-run"))]);
        for input in [vec![], b"{}\n{}".to_vec(), vec![b'x'; INPUT_LIMIT + 1]] {
            let failure = runtime.block_on(async {
                run(&AgentCx::for_current_or_request(), &helpers, &[], &input).await.unwrap_err()
            });
            assert!(failure.to_string().contains("bounded JSON frame"));
        }
        let cx = asupersync::Cx::for_request();
        let owner = {
            let _guard = cx.restrict::<asupersync::cx::cap::None>().set_current_restricted();
            AgentCx::for_current_or_request()
        };
        assert!(runtime.block_on(run(&owner, &helpers, &[], b"{}")).unwrap_err()
            .to_string().contains("capabilities"));
    }

    #[test]
    fn dropped_operation_closes_lease_before_forced_teardown() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let armed = dir.path().join("armed");
        let released = dir.path().join("released");
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let helpers = shell();
            // Paths are positional shell operands, never interpolated source.
            let args = vec!["-c".into(),
                "IFS= read -r frame; printf armed > \"$1\"; IFS= read -r control; printf released > \"$2\"".into(),
                "input-lease-test".into(), armed.to_string_lossy().into_owned(),
                released.to_string_lossy().into_owned()];
            let mut operation = Box::pin(run(&owner, &helpers, &args, b"{}"));
            let deadline = Instant::now() + Duration::from_secs(5);
            let waker = futures::task::noop_waker();
            while !armed.exists() {
                assert!(Instant::now() < deadline, "fixture never armed");
                assert!(matches!(operation.as_mut().poll(&mut Context::from_waker(&waker)), Poll::Pending));
                owner.time().sleep(Duration::from_millis(2)).await;
            }
            assert!(!released.exists());
            drop(operation);
            assert_eq!(std::fs::read(&released).unwrap(), b"released");
        });
    }
}
