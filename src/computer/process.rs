//! Unix helper transport: bounded nonblocking stdout, literal argv/stdin, and
//! AgentChild ownership. Stderr is discarded at spawn: desktop helpers may echo
//! private text, and a long-lived clipboard owner must never block on an unread
//! diagnostic pipe. No shell expansion, reader threads or unbounded spooling.

use super::{error, native};
use crate::agent_cx::{AgentChild, AgentCx};
use crate::error::Result;
use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{ChildStdout, ExitStatus, Stdio};
use std::time::Duration;

pub(super) const TEXT_LIMIT: usize = 256 * 1024;

pub(super) struct Running {
    pub(super) child: AgentChild,
    stdout: ChildStdout,
    bytes: Vec<u8>,
    stdout_eof: bool,
}

fn nonblocking(fd: &impl AsFd) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK).map_err(std::io::Error::from)
}

fn spawn(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
    name: &str,
    args: &[String],
    input: &[u8],
) -> Result<AgentChild> {
    native::check_owner(owner)?;
    if input.len() > 128 * 1024 {
        return Err(error("desktop helper input exceeds 128 KiB"));
    }
    // Anonymous file stdin cannot deadlock against a helper writing stdout
    // before reading. The file is private, bounded and never given a pathname.
    let mut stdin = tempfile::tempfile()?;
    stdin.write_all(input)?;
    stdin.rewind()?;
    let program = helpers
        .get(name)
        .map_or_else(|| Path::new(name), PathBuf::as_path);
    let mut command = owner.process().command(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_remove("XDOTOOL_DEBUG")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command.spawn().map_err(|failure| error(format!(
        "cannot start desktop helper {name}: {failure}; install the required OS helper or configure its trusted path",
    )))
}

pub(super) fn start(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
    name: &str,
    args: &[String],
    input: &[u8],
) -> Result<Running> {
    let mut child = spawn(owner, cwd, helpers, name, args, input)?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| error("missing helper stdout pipe"))?;
    nonblocking(&stdout)?;
    Ok(Running {
        child,
        stdout,
        bytes: Vec::new(),
        stdout_eof: false,
    })
}

// Bound work per tick as well as retained bytes, so a busy writer cannot starve
// owner cancellation. Exact-limit output is allowed if the next read is EOF.
fn drain(reader: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut buffer = [0_u8; 8192];
    for _ in 0..8 {
        let capacity = limit
            .saturating_sub(bytes.len())
            .saturating_add(1)
            .min(buffer.len());
        match reader.read(&mut buffer[..capacity]) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if count > limit.saturating_sub(bytes.len()) {
                    return Err(error("desktop helper output exceeded its byte limit"));
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(failure) if failure.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(failure) if failure.kind() == std::io::ErrorKind::Interrupted => {}
            Err(failure) => return Err(failure.into()),
        }
    }
    Ok(false)
}

impl Running {
    pub(super) fn poll(&mut self, limit: usize) -> Result<Option<ExitStatus>> {
        if !self.stdout_eof {
            self.stdout_eof = drain(&mut self.stdout, &mut self.bytes, limit)?;
        }
        Ok(self.child.try_wait()?)
    }
}

pub(super) async fn run(
    owner: &AgentCx,
    cwd: &Path,
    helpers: &BTreeMap<String, PathBuf>,
    name: &str,
    args: &[String],
    input: &[u8],
    limit: usize,
) -> Result<Vec<u8>> {
    let output = spawn(owner, cwd, helpers, name, args, input)?
        .wait_with_output_limited(limit, Duration::from_secs(30))
        .await
        .map_err(|failure| error(format!("desktop helper {name}: {failure}")))?;
    if !output.status.success() {
        return Err(error(format!(
            "desktop helper {name} failed ({}); check the display session and OS permissions",
            output.status,
        )));
    }
    Ok(output.stdout)
}

pub(super) fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

pub(super) fn text(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| error("desktop helper returned non-UTF-8 text"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run_script(script: &str, input: &[u8], limit: usize) -> Result<Vec<u8>> {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            run(
                &owner,
                Path::new("/"),
                &BTreeMap::new(),
                "/bin/sh",
                &strings(&["-c", script]),
                input,
                limit,
            )
            .await
        })
    }
    #[test]
    fn helper_transport_preserves_literal_stdin_including_shell_syntax() {
        let input = b"line one\n$(touch never-created); 'quoted'\n";
        assert_eq!(run_script("cat", input, 4096).unwrap(), input);
    }
    #[test]
    fn output_limits_and_failure_status_are_not_success() {
        assert_eq!(run_script("printf 1234", b"", 4).unwrap(), b"1234");
        assert!(run_script("printf 12345", b"", 4).is_err());
        let failure = run_script("printf private-secret >&2; exit 7", b"", 4).unwrap_err();
        assert!(!failure.to_string().contains("private-secret"));
        assert!(failure.to_string().contains("failed"));
    }
    #[test]
    fn denied_owner_never_spawns_a_helper() {
        let inner = asupersync::Cx::for_request();
        let owner = {
            let _guard = inner
                .restrict::<asupersync::cx::cap::None>()
                .set_current_restricted();
            AgentCx::for_current_or_request()
        };
        assert!(
            start(
                &owner,
                Path::new("/"),
                &BTreeMap::new(),
                "/bin/sh",
                &strings(&["-c", "exit 0"]),
                b""
            )
            .is_err()
        );
    }
    #[test]
    fn helper_diagnostics_cannot_fill_a_pipe_or_leak_private_text() {
        let bytes = run_script("i=0; while [ $i -lt 20000 ]; do printf private-secret >&2; i=$((i+1)); done; printf done", b"", 1024).unwrap();
        assert_eq!(bytes, b"done");
    }

    #[test]
    fn helper_can_fill_stdout_before_reading_large_literal_input() {
        let input = vec![37; 65_536];
        let bytes = run_script("head -c 131072 /dev/zero; cat", &input, 196_608).unwrap();
        assert_eq!(&bytes[..131_072], vec![0; 131_072]);
        assert_eq!(&bytes[131_072..], input);
    }

    #[test]
    fn oversized_stdout_is_not_embedded_in_the_reported_error() {
        let failure = run_script("printf private-output-secret", b"", 1).unwrap_err();
        assert!(failure.to_string().contains("byte limit"));
        assert!(!failure.to_string().contains("private-output-secret"));
    }
}
