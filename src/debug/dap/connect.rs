//! Explicit connections to pre-existing loopback DAP servers. There is no
//! process discovery, spawn, DNS lookup, protocol probe or automatic retry.

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::{AgentCx, DapTransport, await_completion, tool_err};
use crate::error::Result;
use crate::lsp::jsonrpc::CompletionWaitError;

const MAX_CONNECT_WAIT: Duration = Duration::from_secs(30);
const MAX_CONNECT_WORKERS: usize = 8;
static CONNECT_WORKERS: AtomicUsize = AtomicUsize::new(0);

struct ConnectPermit;

impl ConnectPermit {
    fn acquire() -> Result<Self> {
        CONNECT_WORKERS
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < MAX_CONNECT_WORKERS).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| tool_err("DAP_CONNECT_CAPACITY", "eight debugger connections are already pending"))
    }
}

impl Drop for ConnectPermit {
    fn drop(&mut self) {
        CONNECT_WORKERS.fetch_sub(1, Ordering::SeqCst);
    }
}

fn validate(endpoint: SocketAddr, timeout: Duration) -> Result<()> {
    let plain_address = match endpoint {
        SocketAddr::V4(_) => true,
        SocketAddr::V6(address) => address.flowinfo() == 0 && address.scope_id() == 0,
    };
    if !endpoint.ip().is_loopback() || endpoint.port() == 0 || !plain_address {
        return Err(tool_err(
            "DAP_USAGE",
            "debugger connection requires a loopback IP and nonzero port; forward remote servers through a trusted local tunnel",
        ));
    }
    if timeout.is_zero() || timeout > MAX_CONNECT_WAIT {
        return Err(tool_err("DAP_USAGE", "debugger connection timeout must be positive and at most 30 seconds"));
    }
    Ok(())
}

fn checkpoint(owner: &AgentCx, started: Instant, timeout: Duration) -> Result<()> {
    owner.checkpoint().map_err(|_| tool_err("DAP_CANCELLED", "debugger connection cancelled"))?;
    if started.elapsed() >= timeout {
        return Err(tool_err("DAP_CONNECT_TIMEOUT", "debugger connection exceeded its deadline"));
    }
    Ok(())
}

impl DapTransport {
    /// Connect to a DAP server the caller has already started or forwarded.
    /// No local adapter executable or subprocess capability is required.
    ///
    /// Loopback is not authentication: the caller must trust this endpoint.
    /// Cancellation drops the wait; a bounded OS connect may finish afterwards,
    /// but its unclaimed socket closes without sending any DAP messages. At
    /// most eight connect workers exist, including abandoned pending connects.
    /// Dropping the returned transport closes only this connection; it never
    /// signals the external adapter or its debuggee.
    pub async fn connect_loopback(endpoint: SocketAddr, timeout: Duration) -> Result<Self> {
        validate(endpoint, timeout)?;
        let owner = AgentCx::for_current_or_request();
        let started = Instant::now();
        checkpoint(&owner, started, timeout)?;
        if !owner.capabilities().io || !owner.capabilities().time {
            return Err(tool_err("DAP_PERMISSION", "debugger connection requires I/O and timer capabilities"));
        }
        let permit = ConnectPermit::acquire()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("pi-dap-connect".into())
            .spawn(move || {
                let _permit = permit;
                // Sending into a dropped receiver drops the contained stream.
                // Neither this worker nor a late socket can initialize/attach.
                let result = TcpStream::connect_timeout(&endpoint, timeout);
                let _ = sender.send(result);
            })
            .map_err(|error| tool_err("DAP_CONNECT", format!("cannot start connection worker: {error}")))?;
        let connected = owner.with_current(await_completion(receiver, timeout, || {})).await;
        checkpoint(&owner, started, timeout)?;
        let socket = connected.map_err(|error| match error {
            CompletionWaitError::Cancelled => tool_err("DAP_CANCELLED", "debugger connection cancelled"),
            CompletionWaitError::Timeout => tool_err("DAP_CONNECT_TIMEOUT", "debugger connection exceeded its deadline"),
            CompletionWaitError::Closed => tool_err("DAP_CONNECT", "debugger connection worker closed"),
        })?.map_err(|error| {
            let code = if error.kind() == std::io::ErrorKind::TimedOut {
                "DAP_CONNECT_TIMEOUT"
            } else {
                "DAP_CONNECT"
            };
            tool_err(code, format!("cannot connect to {endpoint}: {error}"))
        })?;
        socket.set_nodelay(true)?;
        let input = socket.try_clone()?;
        let output = socket.try_clone()?;
        checkpoint(&owner, started, timeout)?;
        let transport = Self::from_lanes(
            None,
            Box::new(input),
            Box::new(output),
            Some(socket),
            Arc::new(Mutex::new(crate::lsp::jsonrpc::PublicTailBuffer::new())),
        );
        checkpoint(&owner, started, timeout)?;
        Ok(transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::debug::dap::{DapError, encode_frame, lock, read_frame_with_scratch};
    use serde_json::{Value, json};
    use std::io::{BufReader, Read, Write};
    use std::net::TcpListener;

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap().block_on(future)
    }

    fn accept(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let started = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(started.elapsed() < Duration::from_secs(3), "client never connected");
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
    }

    fn request(reader: &mut BufReader<TcpStream>) -> Value {
        read_frame_with_scratch(reader, &mut Vec::new()).unwrap().unwrap()
    }

    #[test]
    fn non_loopback_zero_port_and_invalid_budgets_are_rejected() {
        for endpoint in ["0.0.0.0:1", "192.0.2.1:1", "127.0.0.1:0", "[::]:1", "[::ffff:127.0.0.1]:1"] {
            assert!(validate(endpoint.parse().unwrap(), Duration::from_secs(1)).is_err());
        }
        for endpoint in ["127.0.0.1:1", "[::1]:1"] {
            let endpoint = endpoint.parse().unwrap();
            assert!(validate(endpoint, Duration::from_secs(1)).is_ok());
            assert!(validate(endpoint, Duration::ZERO).is_err());
            assert!(validate(endpoint, Duration::from_secs(31)).is_err());
        }
    }

    #[test]
    fn cancelled_owner_never_opens_the_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let owner = AgentCx::for_request();
        owner.set_cancel_requested(true);
        let result = run(owner.with_current(DapTransport::connect_loopback(
            listener.local_addr().unwrap(), Duration::from_secs(1),
        )));
        assert!(result.err().unwrap().to_string().contains("DAP_CANCELLED"));
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn external_connection_uses_real_framing_and_drop_preserves_the_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut stream = accept(&listener);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let input = request(&mut reader);
            assert_eq!(input["command"], "threads");
            stream.write_all(&encode_frame(&json!({"seq":1,"type":"event","event":"output","body":{"output":"external-marker"}}))).unwrap();
            stream.write_all(&encode_frame(&json!({"seq":2,"type":"response","request_seq":input["seq"],"command":"threads","success":true,"body":{"threads":[{"id":7,"name":"external"}]}}))).unwrap();
            assert_eq!(reader.read(&mut [0_u8; 1]).unwrap(), 0, "drop must close the socket, not send a terminate command");
            // The external service is still accepting clients after retirement.
            let mut second = accept(&listener);
            second.write_all(b"alive").unwrap();
        });
        run(async {
            let transport = DapTransport::connect_loopback(endpoint, Duration::from_secs(1)).await.unwrap();
            assert!(lock(&transport.life.child).is_none());
            let reply = transport.request("threads", json!({}), Duration::from_secs(1)).await.unwrap();
            assert_eq!(reply["threads"][0]["name"], "external");
            assert!(transport.stderr_tail().contains("external-marker"));
            drop(transport);
        });
        let mut second = TcpStream::connect_timeout(&endpoint, Duration::from_secs(1)).unwrap();
        second.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut alive = [0; 5];
        second.read_exact(&mut alive).unwrap();
        assert_eq!(&alive, b"alive");
        peer.join().unwrap();
    }

    #[test]
    fn external_eof_fails_pending_request_without_a_process_handle() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut reader = BufReader::new(accept(&listener));
            assert_eq!(request(&mut reader)["command"], "threads");
            // Close without a reply: this is a transport failure, not success.
        });
        run(async {
            let transport = DapTransport::connect_loopback(endpoint, Duration::from_secs(1)).await.unwrap();
            let result = transport.request("threads", json!({}), Duration::from_secs(1)).await;
            assert!(matches!(result, Err(DapError::Transport(_))), "{result:?}");
            assert!(!transport.is_alive());
            assert!(lock(&transport.pending).is_empty());
        });
        peer.join().unwrap();
    }

    #[test]
    fn cancellation_releases_a_sent_external_request_and_socket_cleanup() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = listener.local_addr().unwrap();
        let (ready, received) = mpsc::sync_channel(1);
        let peer = std::thread::spawn(move || {
            let mut reader = BufReader::new(accept(&listener));
            assert_eq!(request(&mut reader)["command"], "stall");
            ready.send(()).unwrap();
            assert_eq!(reader.read(&mut [0_u8; 1]).unwrap(), 0);
        });
        let owner = AgentCx::for_request();
        run(owner.with_current(async {
            let transport = DapTransport::connect_loopback(endpoint, Duration::from_secs(1)).await.unwrap();
            let mut pending = Box::pin(transport.request("stall", json!({}), Duration::from_secs(2)));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            await_completion(received, Duration::from_secs(1), || {}).await.unwrap();
            owner.set_cancel_requested(true);
            assert!(matches!(pending.await, Err(DapError::Cancelled)));
            assert!(lock(&transport.pending).is_empty());
            transport.kill();
            assert!(!transport.is_alive());
        }));
        peer.join().unwrap();
    }
}
