//! Delve is a TCP DAP server, not a stdio adapter. Start an owned, same-user
//! server on an ephemeral loopback port and discover that port from its stdout.

use super::{AdapterIo, AgentCx, DapTransport, OutputTail, Stdio, TcpStream};
use super::{await_completion, lock, spawn_output_pump, tool_err};
use crate::error::Result;
use crate::lsp::jsonrpc::CompletionWaitError;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const READY_PREFIX: &str = "DAP server listening at: ";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_STARTUP_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 4096;

type Ready = std::result::Result<SocketAddr, String>;

impl DapTransport {
    /// Start the registered Delve adapter. Only trusted host code supplies the
    /// command/args/env; no tool argument may select a server or relax binding.
    pub async fn spawn_delve(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
    ) -> Result<Self> {
        validate_args(args)?;
        let owner = AgentCx::for_current_or_request();
        let caps = owner.capabilities();
        if !caps.io || !caps.spawn || !caps.time {
            return Err(tool_err(
                "DAP_PERMISSION",
                "Delve requires I/O, spawn and timer capabilities",
            ));
        }
        owner
            .checkpoint()
            .map_err(|_| tool_err("DAP_CANCELLED", "Delve startup cancelled"))?;
        let mut cmd = std::process::Command::new(command);
        cmd.args(args)
            .args(["--listen=127.0.0.1:0", "--only-same-user=true"])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(
                env.iter()
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            )
            .env_remove("CARGO_TARGET_DIR");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let mut child = owner.process().spawn_checked(&mut cmd).map_err(|error| {
            tool_err(
                "DAP_ADAPTER_MISSING",
                format!("failed to start Delve: {error}"),
            )
        })?;
        crate::tools::attach_child_job_discipline(&child);
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(tool_err("DAP_TRANSPORT", "Delve output pipes unavailable"));
        };
        // This guard remains local across every startup await. Cancellation or
        // discovery/connection failure cannot publish or orphan the process.
        let mut child = crate::tools::ProcessGuard::new(
            child,
            crate::tools::ProcessCleanupMode::ProcessGroupTree,
        );
        let tail = Arc::new(Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new()));
        let (ready_tx, ready_rx) = sync_channel(1);
        let stdout_tail = Arc::clone(&tail);
        std::thread::Builder::new()
            .name("pi-delve-stdout".into())
            .spawn(move || drain_stdout(stdout, &stdout_tail, ready_tx))
            .map_err(|error| {
                tool_err(
                    "DAP_TRANSPORT",
                    format!("cannot read Delve stdout: {error}"),
                )
            })?;
        spawn_output_pump(stderr, Arc::clone(&tail));
        let endpoint = await_completion(ready_rx, STARTUP_TIMEOUT, || {})
            .await
            .map_err(startup_error)?
            .map_err(|message| tool_err("DAP_STARTUP", message))?;
        owner
            .checkpoint()
            .map_err(|_| tool_err("DAP_CANCELLED", "Delve startup cancelled"))?;
        if !matches!(child.try_wait_child(), Ok(None)) {
            return Err(tool_err(
                "DAP_STARTUP",
                "Delve exited after announcing its endpoint",
            ));
        }
        let socket = connect_endpoint(&endpoint).await?;
        owner
            .checkpoint()
            .map_err(|_| tool_err("DAP_CANCELLED", "Delve startup cancelled"))?;
        if !matches!(child.try_wait_child(), Ok(None)) {
            return Err(tool_err("DAP_STARTUP", "Delve exited during connection"));
        }
        socket.set_nodelay(true)?;
        let input = socket.try_clone()?;
        let output = socket.try_clone()?;
        Ok(Self::from_io(AdapterIo {
            child,
            input: Box::new(input),
            output: Box::new(output),
            socket: Some(socket),
            tail,
        }))
    }
}

async fn connect_endpoint(endpoint: &SocketAddr) -> Result<TcpStream> {
    let (connected_tx, connected_rx) = sync_channel(1);
    let target = *endpoint;
    std::thread::Builder::new()
        .name("pi-delve-connect".into())
        .spawn(move || {
            let result = TcpStream::connect_timeout(&target, CONNECT_TIMEOUT)
                .map_err(|error| format!("cannot connect to owned Delve endpoint: {error}"));
            let _ = connected_tx.send(result);
        })
        .map_err(|error| tool_err("DAP_TRANSPORT", format!("cannot connect to Delve: {error}")))?;
    await_completion(connected_rx, STARTUP_TIMEOUT, || {})
        .await
        .map_err(startup_error)?
        .map_err(|message| tool_err("DAP_TRANSPORT", message))
}

fn startup_error(error: CompletionWaitError) -> crate::error::Error {
    match error {
        CompletionWaitError::Cancelled => tool_err("DAP_CANCELLED", "Delve startup cancelled"),
        CompletionWaitError::Timeout => tool_err(
            "DAP_STARTUP_TIMEOUT",
            "Delve did not become ready before the startup deadline",
        ),
        CompletionWaitError::Closed => tool_err("DAP_STARTUP", "Delve startup channel closed"),
    }
}

fn validate_args(args: &[String]) -> Result<()> {
    for argument in args {
        let flag = argument.split('=').next().unwrap_or(argument);
        if argument == "--"
            || (argument.starts_with("-l") && !argument.starts_with("--"))
            || matches!(
                flag,
                "--listen"
                    | "--client-addr"
                    | "--only-same-user"
                    | "--headless"
                    | "--accept-multiclient"
                    | "--log-dest"
            )
        {
            return Err(tool_err(
                "DAP_USAGE",
                "Delve adapter arguments cannot override the owned loopback transport or startup output",
            ));
        }
    }
    Ok(())
}

fn announcement(line: &[u8]) -> Option<Ready> {
    let line = std::str::from_utf8(line).ok()?;
    let address = line.trim_end_matches('\r').strip_prefix(READY_PREFIX)?;
    Some(
        address
            .parse::<SocketAddrV4>()
            .ok()
            .filter(|address| *address.ip() == Ipv4Addr::LOCALHOST && address.port() != 0)
            .map(SocketAddr::V4)
            .ok_or_else(|| "Delve announced an invalid or non-loopback endpoint".to_string()),
    )
}

fn drain_stdout(mut reader: impl Read, tail: &OutputTail, sender: SyncSender<Ready>) {
    let mut sender = Some(sender);
    let mut line = Vec::new();
    let mut total = 0_usize;
    let mut buffer = [0_u8; 4096];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        lock(tail).push(&String::from_utf8_lossy(&buffer[..count]));
        if sender.is_none() {
            continue;
        }
        for byte in &buffer[..count] {
            total += 1;
            if total > MAX_STARTUP_BYTES || line.len() >= MAX_LINE_BYTES {
                if let Some(sender) = sender.take() {
                    let _ = sender.send(Err("Delve startup output exceeded its byte limit".into()));
                }
                return;
            }
            if *byte == b'\n' {
                if let Some(result) = announcement(&line) {
                    if let Some(sender) = sender.take() {
                        let _ = sender.send(result);
                    }
                    line.clear();
                    break;
                }
                line.clear();
            } else {
                line.push(*byte);
            }
        }
    }
    if let Some(sender) = sender {
        let _ = sender.send(Err(
            "Delve exited or closed stdout before announcing its endpoint".into(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_accepts_only_the_owned_ipv4_loopback_binding() {
        let good = announcement(b"DAP server listening at: 127.0.0.1:12345\r")
            .unwrap()
            .unwrap();
        assert_eq!(good.to_string(), "127.0.0.1:12345");
        for address in [
            "0.0.0.0:1",
            "192.0.2.1:1",
            "localhost:1",
            "127.0.0.1:0",
            "[::1]:1",
            "127.0.0.1:1/path",
        ] {
            assert!(
                announcement(format!("{READY_PREFIX}{address}").as_bytes())
                    .unwrap()
                    .is_err()
            );
        }
        assert!(announcement(b"ordinary program output").is_none());
    }

    #[test]
    fn stdout_discovery_is_bounded_and_output_is_not_dap_framing() {
        let tail = Arc::new(Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new()));
        let (tx, rx) = sync_channel(1);
        drain_stdout(
            std::io::Cursor::new(
                b"startup log\nDAP server listening at: 127.0.0.1:23456\nprogram output\n",
            ),
            &tail,
            tx,
        );
        assert_eq!(rx.recv().unwrap().unwrap().port(), 23456);
        assert!(lock(&tail).tail().contains("program output"));
        let (tx, rx) = sync_channel(1);
        drain_stdout(
            std::io::Cursor::new(vec![b'x'; MAX_LINE_BYTES + 1]),
            &tail,
            tx,
        );
        assert!(rx.recv().unwrap().unwrap_err().contains("byte limit"));
        let (tx, rx) = sync_channel(1);
        drain_stdout(std::io::Cursor::new(b"no announcement\n"), &tail, tx);
        assert!(rx.recv().unwrap().is_err());
    }

    #[test]
    fn trusted_adapter_overrides_cannot_relax_transport_restrictions() {
        assert!(validate_args(&["dap".into(), "--log".into()]).is_ok());
        for arg in [
            "--listen=:1",
            "-l",
            "-l:1",
            "--client-addr",
            "--only-same-user=false",
            "--headless",
            "--",
            "--log-dest=elsewhere",
        ] {
            assert!(validate_args(&["dap".into(), arg.into()]).is_err());
        }
    }
}
