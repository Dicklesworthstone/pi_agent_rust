//! A bounded, authenticated local channel owned by one native child process.
//!
//! The parent polls this nonblocking listener from its existing child runner.
//! There is no detached listener, per-connection thread, proxy, or HTTP server.
//! A child receives one opaque credential through its environment. It can
//! inspect its parent roster, read its own inbox, and send to its parent or
//! live peers. Process control and transcript access stay with the parent.

use super::{AgentHubRegistry, BusMessage, ChildEntry, ChildStatus, registry};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use asupersync::io::ext::AsyncWriteExt as _;
use asupersync::io::{AsyncRead as _, ReadBuf};
use asupersync::net::tcp::stream::TcpStream as AsyncTcpStream;
use futures::future::{Either, poll_fn, select};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::pin::Pin;
use std::task::Poll;
use std::time::{Duration, Instant};

const ADDRESS_ENV: &str = "PI_SUBAGENT_HUB_ADDRESS";
const TOKEN_ENV: &str = "PI_SUBAGENT_HUB_TOKEN";
const VERSION: u8 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_CONNECTIONS: usize = 8;
const MAX_ACCEPTS_PER_POLL: usize = 4;
const IO_CHUNKS_PER_POLL: usize = 4;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
const CANCEL_POLL: Duration = Duration::from_millis(10);
pub(super) const MAX_INBOX_PAGE_BYTES: usize = 128 * 1024;

/// The complete child-facing authority surface.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
    Roster,
    Send,
    Inbox,
}

/// Public tool inputs, excluding the channel credential.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub action: Action,
    pub name: Option<String>,
    pub text: Option<String>,
    pub claimed_from: Option<String>,
    pub cursor: Option<u64>,
}

/// A successful response; bounded pages retain an explicit continuation cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Reply {
    Roster {
        #[serde(rename = "selfId")]
        self_id: String,
        children: Vec<ChildEntry>,
        truncated: bool,
    },
    Sent {
        message: BusMessage,
    },
    Inbox {
        id: String,
        messages: Vec<BusMessage>,
        #[serde(rename = "nextCursor")]
        next_cursor: u64,
        #[serde(rename = "hasMore")]
        has_more: bool,
        #[serde(rename = "oldestCursor")]
        oldest_cursor: u64,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireRequest {
    version: u8,
    token: String,
    request: Request,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "camelCase")]
enum WireResponse {
    Ok(Reply),
    Error(String),
}

fn ipc_error(code: &str, message: &str) -> Error {
    Error::tool("hub", format!("PI_HUB_IPC_{code}: {message}"))
}

/// Any inherited channel marker activates the restricted child route. Partial
/// or invalid configuration must fail closed instead of reading an empty local
/// registry or accidentally invoking local process-control operations.
#[must_use]
pub fn inherited_channel_present() -> bool {
    std::env::var_os(ADDRESS_ENV).is_some() || std::env::var_os(TOKEN_ENV).is_some()
}

fn inherited_endpoint() -> Result<Option<(SocketAddr, String)>> {
    if !inherited_channel_present() {
        return Ok(None);
    }
    let address = std::env::var(ADDRESS_ENV)
        .ok()
        .and_then(|value| value.parse::<SocketAddr>().ok())
        .filter(|address| address.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) && address.port() != 0)
        .ok_or_else(|| ipc_error("CONFIG", "invalid local parent channel address"))?;
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| ipc_error("CONFIG", "missing or invalid parent channel credential"))?;
    Ok(Some((address, token)))
}

/// Submit exactly one request, without automatic retries. If an acknowledged
/// write is interrupted, the recipient inbox is the delivery record; retrying
/// an uncertain send can otherwise deliver the same message twice.
pub async fn call_from_env(request: Request) -> Result<Option<Reply>> {
    let Some((address, token)) = inherited_endpoint()? else {
        return Ok(None);
    };
    let owner = AgentCx::for_current_or_request();
    call(&owner, address, token, request).await.map(Some)
}

async fn call(
    owner: &AgentCx,
    address: SocketAddr,
    token: String,
    request: Request,
) -> Result<Reply> {
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err(ipc_error(
            "PERMISSION",
            "parent messaging requires I/O and timer authority",
        ));
    }
    owner
        .checkpoint()
        .map_err(|_| ipc_error("CANCELLED", "parent messaging was cancelled before dispatch"))?;
    let mut frame = serde_json::to_vec(&WireRequest {
        version: VERSION,
        token,
        request,
    })?;
    frame.push(b'\n');
    if frame.len() > MAX_REQUEST_BYTES {
        return Err(ipc_error("LIMIT", "request exceeds the 64 KiB frame limit"));
    }
    let now = owner
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let inherited = owner.budget().deadline.map_or(EXCHANGE_TIMEOUT, |deadline| {
        Duration::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
    });
    let timeout = EXCHANGE_TIMEOUT.min(inherited);
    if timeout.is_zero() {
        return Err(ipc_error("TIMEOUT", "parent messaging deadline has expired"));
    }
    let exchange = async {
        match select(
            Box::pin(exchange(address, frame)),
            Box::pin(wait_for_cancellation(owner)),
        )
        .await
        {
            Either::Left((reply, _)) => reply,
            Either::Right((error, _)) => Err(error),
        }
    };
    owner
        .with_current(asupersync::time::timeout(now, timeout, exchange))
        .await
        .map_err(|_| {
            ipc_error(
                "TIMEOUT",
                "parent messaging timed out; a submitted send may already be in the recipient inbox",
            )
        })?
}

async fn wait_for_cancellation(owner: &AgentCx) -> Error {
    loop {
        if owner.checkpoint().is_err() {
            return ipc_error(
                "CANCELLED",
                "parent messaging was cancelled; a submitted send may already be in the recipient inbox",
            );
        }
        owner.time().sleep(CANCEL_POLL).await;
    }
}

async fn exchange(address: SocketAddr, frame: Vec<u8>) -> Result<Reply> {
    // SocketAddr is validated before this function; no name resolution or
    // ambient proxy configuration participates in this local transport.
    let mut socket = AsyncTcpStream::connect(address)
        .await
        .map_err(|_| ipc_error("UNAVAILABLE", "parent messaging channel is unavailable"))?;
    socket
        .write_all(&frame)
        .await
        .map_err(|_| ipc_error("WRITE", "parent messaging request could not be written"))?;
    socket
        .flush()
        .await
        .map_err(|_| ipc_error("WRITE", "parent messaging request could not be flushed"))?;
    let mut response = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let count = poll_fn(|cx| {
            let mut buffer = ReadBuf::new(&mut chunk);
            match Pin::new(&mut socket).poll_read(cx, &mut buffer) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(buffer.filled().len())),
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await
        .map_err(|_| ipc_error("READ", "parent messaging response was interrupted"))?;
        if count == 0 {
            return Err(ipc_error(
                "CLOSED",
                "parent messaging channel closed without a complete reply",
            ));
        }
        if response.len().saturating_add(count) > MAX_RESPONSE_BYTES {
            return Err(ipc_error("LIMIT", "parent messaging response exceeds 256 KiB"));
        }
        response.extend_from_slice(&chunk[..count]);
        if let Some(end) = response.iter().position(|byte| *byte == b'\n') {
            if end + 1 != response.len() {
                return Err(ipc_error("PROTOCOL", "parent returned trailing response data"));
            }
            let response: WireResponse = serde_json::from_slice(&response[..end])
                .map_err(|_| ipc_error("PROTOCOL", "parent returned an invalid response"))?;
            return match response {
                WireResponse::Ok(reply) => Ok(reply),
                WireResponse::Error(message) => Err(Error::tool("hub", message)),
            };
        }
    }
}

/// Owned by the same guard as the native process, never by the global registry.
/// Revocation closes all sockets immediately; the secret is never serialized
/// into a launch result, transcript, command argument, or diagnostic.
pub(crate) struct ChildChannel {
    listener: Option<TcpListener>,
    address: SocketAddr,
    token: String,
    sender: String,
    owner: AgentCx,
    deadline: Instant,
    connections: Vec<Connection>,
}

impl ChildChannel {
    pub(crate) fn bind(sender: &str, owner: &AgentCx, remaining: Duration) -> Result<Self> {
        owner
            .checkpoint()
            .map_err(|_| ipc_error("CANCELLED", "parent messaging owner is cancelled"))?;
        let capabilities = owner.capabilities();
        if !capabilities.io
            || !capabilities.time
            || !capabilities.entropy
            || remaining.is_zero()
        {
            return Err(ipc_error(
                "PERMISSION",
                "parent messaging requires live I/O, timer and entropy authority",
            ));
        }
        let mut entropy = [0_u8; 32];
        getrandom::fill(&mut entropy)
            .map_err(|_| ipc_error("START", "secure parent messaging identity is unavailable"))?;
        let mut token = String::with_capacity(64);
        for byte in entropy {
            let _ = write!(token, "{byte:02x}");
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| ipc_error("START", "could not bind the local parent messaging channel"))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ipc_error("START", "could not make the local channel nonblocking"))?;
        let address = listener
            .local_addr()
            .map_err(|_| ipc_error("START", "could not resolve the local channel address"))?;
        let deadline = Instant::now()
            .checked_add(remaining)
            .ok_or_else(|| ipc_error("START", "parent messaging deadline is out of range"))?;
        Ok(Self {
            listener: Some(listener),
            address,
            token,
            sender: sender.to_string(),
            owner: owner.clone(),
            deadline,
            connections: Vec::new(),
        })
    }

    pub(crate) fn configure_child(
        command: &mut std::process::Command,
        channel: Option<&Self>,
    ) {
        // An explicit tool allowlist without hub needs no new channel and
        // must never borrow this process's identity in its own parent hub.
        command.env_remove(ADDRESS_ENV).env_remove(TOKEN_ENV);
        if let Some(channel) = channel {
            command
                .env(ADDRESS_ENV, channel.address.to_string())
                .env(TOKEN_ENV, &channel.token);
        }
    }

    pub(crate) fn deactivate(&mut self) {
        self.connections.clear();
        self.listener = None;
        self.token.clear();
    }

    pub(crate) fn poll(&mut self) {
        if self.owner.checkpoint().is_err() || Instant::now() >= self.deadline {
            self.deactivate();
            return;
        }
        let Some(listener) = self.listener.as_ref() else {
            return;
        };
        for _ in 0..MAX_ACCEPTS_PER_POLL {
            if self.connections.len() >= MAX_CONNECTIONS {
                break;
            }
            match listener.accept() {
                Ok((socket, address)) => {
                    if !address.ip().is_loopback() || socket.set_nonblocking(true).is_err() {
                        continue;
                    }
                    self.connections.push(Connection::new(socket));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        self.connections.retain_mut(|connection| {
            connection.poll(&self.sender, &self.token, &self.owner, self.deadline)
        });
    }
}

struct Connection {
    socket: TcpStream,
    input: Vec<u8>,
    output: Option<Vec<u8>>,
    written: usize,
    deadline: Instant,
}

impl Connection {
    fn new(socket: TcpStream) -> Self {
        Self {
            socket,
            input: Vec::new(),
            output: None,
            written: 0,
            deadline: Instant::now() + EXCHANGE_TIMEOUT,
        }
    }

    fn set_response(&mut self, response: WireResponse) {
        let encoded = serde_json::to_vec(&response)
            .ok()
            .filter(|bytes| bytes.len() < MAX_RESPONSE_BYTES)
            .unwrap_or_else(|| {
                br#"{"status":"error","payload":"PI_HUB_IPC_LIMIT: reply exceeds 256 KiB"}"#
                    .to_vec()
            });
        self.input.clear();
        let mut encoded = encoded;
        encoded.push(b'\n');
        self.output = Some(encoded);
    }

    fn refuse(&mut self, code: &str, message: &str) {
        self.set_response(WireResponse::Error(ipc_error(code, message).to_string()));
    }

    fn poll(&mut self, sender: &str, token: &str, owner: &AgentCx, deadline: Instant) -> bool {
        if Instant::now() >= self.deadline.min(deadline) || owner.checkpoint().is_err() {
            return false;
        }
        if self.output.is_none() {
            for _ in 0..IO_CHUNKS_PER_POLL {
                let mut chunk = [0_u8; 4096];
                match self.socket.read(&mut chunk) {
                    Ok(0) => return false,
                    Ok(count) => {
                        if self.input.len().saturating_add(count) > MAX_REQUEST_BYTES {
                            self.refuse("LIMIT", "request exceeds the 64 KiB frame limit");
                            break;
                        }
                        self.input.extend_from_slice(&chunk[..count]);
                        if let Some(end) = self.input.iter().position(|byte| *byte == b'\n') {
                            if end + 1 != self.input.len() {
                                self.refuse("PROTOCOL", "only one request frame is permitted");
                                break;
                            }
                            if Instant::now() >= self.deadline.min(deadline)
                                || owner.checkpoint().is_err()
                            {
                                return false;
                            }
                            let request = serde_json::from_slice::<WireRequest>(&self.input[..end]);
                            let response = match request {
                                Ok(request) => dispatch_authenticated(sender, token, request, owner),
                                Err(_) => Err(ipc_error("PROTOCOL", "invalid request frame")),
                            };
                            self.set_response(match response {
                                Ok(reply) => WireResponse::Ok(reply),
                                Err(error) => WireResponse::Error(error.to_string()),
                            });
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return false,
                }
            }
        }
        if let Some(output) = &self.output {
            for _ in 0..IO_CHUNKS_PER_POLL {
                let end = self.written.saturating_add(4096).min(output.len());
                match self.socket.write(&output[self.written..end]) {
                    Ok(0) => return false,
                    Ok(count) => {
                        self.written += count;
                        if self.written == output.len() {
                            let _ = self.socket.shutdown(Shutdown::Both);
                            return false;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => return false,
                }
            }
        }
        true
    }
}

fn matches_token(expected: &str, supplied: &str) -> bool {
    // Check the fixed public length first, then visit every credential byte.
    expected.len() == 64
        && supplied.len() == 64
        && expected
            .bytes()
            .zip(supplied.bytes())
            .fold(0_u8, |different, (left, right)| different | (left ^ right))
            == 0
}

fn dispatch_authenticated(
    sender: &str,
    token: &str,
    request: WireRequest,
    owner: &AgentCx,
) -> Result<Reply> {
    if request.version != VERSION || !matches_token(token, &request.token) {
        return Err(ipc_error(
            "AUTH",
            "invalid parent messaging credential or protocol version",
        ));
    }
    if request
        .request
        .claimed_from
        .as_deref()
        .is_some_and(|claimed| claimed != sender)
    {
        return Err(ipc_error(
            "SENDER",
            "sender label does not match the authenticated child",
        ));
    }
    owner
        .checkpoint()
        .map_err(|_| ipc_error("CANCELLED", "parent messaging owner is cancelled"))?;
    let mut hub = registry().try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => ipc_error(
            "BUSY",
            "parent agent registry is busy; request was not accepted",
        ),
        std::sync::TryLockError::Poisoned(_) => {
            ipc_error("UNAVAILABLE", "parent agent registry is unavailable")
        }
    })?;
    dispatch(&mut hub, sender, request.request)
}

fn dispatch(hub: &mut AgentHubRegistry, sender: &str, request: Request) -> Result<Reply> {
    if hub.control_pid(sender).is_none()
        || hub
            .get(sender)
            .is_none_or(|entry| entry.status != ChildStatus::Running)
    {
        return Err(ipc_error(
            "REVOKED",
            "the sending child no longer owns a live process",
        ));
    }
    match request.action {
        Action::Roster => {
            let mut children = Vec::new();
            let mut bytes = 0_usize;
            let mut truncated = false;
            for child in hub.entries.values() {
                let size = serde_json::to_vec(&child)?.len();
                if bytes.saturating_add(size) > MAX_INBOX_PAGE_BYTES {
                    truncated = true;
                    break;
                }
                bytes += size;
                children.push(child.clone());
            }
            Ok(Reply::Roster {
                self_id: sender.to_string(),
                children,
                truncated,
            })
        }
        Action::Send => {
            let recipient = request
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| ipc_error("INPUT", "send requires a recipient run id or parent"))?;
            if recipient != "parent"
                && (hub.control_pid(recipient).is_none()
                    || hub
                        .get(recipient)
                        .is_none_or(|entry| entry.status != ChildStatus::Running))
            {
                return Err(ipc_error("RECIPIENT", "recipient is not a live sibling"));
            }
            let body = request
                .text
                .as_deref()
                .filter(|body| !body.trim().is_empty())
                .ok_or_else(|| ipc_error("INPUT", "send requires non-empty text"))?;
            let message = hub.bus_send(recipient, sender, body)?;
            Ok(Reply::Sent { message })
        }
        Action::Inbox => {
            let recipient = request.name.as_deref().unwrap_or(sender);
            if recipient != sender && recipient != "self" {
                return Err(ipc_error("INBOX", "a child can read only its own inbox"));
            }
            hub.inbox_page(sender, request.cursor)
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn request(action: Action) -> Request {
        Request {
            action,
            name: None,
            text: None,
            claimed_from: None,
            cursor: None,
        }
    }

    fn wire(token: &str, request: Request) -> Vec<u8> {
        let mut frame = serde_json::to_vec(&WireRequest {
            version: VERSION,
            token: token.to_string(),
            request,
        })
        .expect("encode fixture");
        frame.push(b'\n');
        frame
    }

    fn channel() -> (AgentCx, ChildChannel) {
        let owner = AgentCx::for_testing_with_io();
        let channel = ChildChannel::bind(
            "unregistered-transport-fixture",
            &owner,
            Duration::from_secs(30),
        )
        .expect("bind owned channel");
        (owner, channel)
    }

    /// Exercise the actual nonblocking TCP transport without a detached
    /// server thread. Both sides advance under one finite test deadline.
    fn round_trip(channel: &mut ChildChannel, frame: &[u8]) -> Vec<u8> {
        let mut client = TcpStream::connect_timeout(&channel.address, Duration::from_secs(1))
            .expect("connect local fixture");
        client.set_nonblocking(true).expect("nonblocking client");
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut written = 0_usize;
        let mut response = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "local exchange did not settle");
            if written < frame.len() {
                let end = written.saturating_add(4096).min(frame.len());
                match client.write(&frame[written..end]) {
                    Ok(0) => panic!("fixture write closed"),
                    Ok(count) => written += count,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("fixture write failed: {error}"),
                }
            }
            channel.poll();
            let mut chunk = [0_u8; 4096];
            match client.read(&mut chunk) {
                Ok(0) => {
                    assert!(response.ends_with(b"\n"), "reply closed before its boundary");
                    return response;
                }
                Ok(count) => {
                    response.extend_from_slice(&chunk[..count]);
                    assert!(response.len() <= MAX_RESPONSE_BYTES);
                    if response.ends_with(b"\n") {
                        return response;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("fixture read failed: {error}"),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn refusal(bytes: &[u8]) -> String {
        match serde_json::from_slice::<WireResponse>(bytes).expect("decode reply") {
            WireResponse::Error(error) => error,
            WireResponse::Ok(_) => panic!("expected refusal"),
        }
    }

    #[test]
    fn child_environment_never_reuses_an_inherited_channel() {
        let (_owner, channel) = channel();
        let mut command = std::process::Command::new("pi");
        command
            .env(ADDRESS_ENV, "127.0.0.1:1")
            .env(TOKEN_ENV, "older-generation-credential");
        ChildChannel::configure_child(&mut command, None);
        for key in [ADDRESS_ENV, TOKEN_ENV] {
            let (_, value) = command
                .get_envs()
                .find(|(name, _)| *name == std::ffi::OsStr::new(key))
                .expect("explicitly removed inherited marker");
            assert!(value.is_none());
        }
        ChildChannel::configure_child(&mut command, Some(&channel));
        let (_, address) = command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new(ADDRESS_ENV))
            .expect("new address");
        assert_eq!(
            address.and_then(std::ffi::OsStr::to_str),
            Some(channel.address.to_string().as_str())
        );
        let (_, token) = command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new(TOKEN_ENV))
            .expect("new credential");
        assert!(token.is_some_and(|value| value == std::ffi::OsStr::new(&channel.token)));
    }

    #[test]
    fn native_socket_authentication_and_limits_fail_without_disabling_the_channel() {
        let (_owner, mut channel) = channel();
        let token = channel.token.clone();
        let wrong_token = if token.starts_with('a') {
            "b".repeat(64)
        } else {
            "a".repeat(64)
        };
        let invalid = wire(&wrong_token, request(Action::Roster));
        let error = refusal(&round_trip(&mut channel, &invalid));
        assert!(error.contains("PI_HUB_IPC_AUTH"));
        assert!(!error.contains(&token));
        assert!(!error.contains(&wrong_token));

        let oversized = vec![b'x'; MAX_REQUEST_BYTES + 1];
        assert!(
            refusal(&round_trip(&mut channel, &oversized)).contains("PI_HUB_IPC_LIMIT")
        );

        let mut spoofed = request(Action::Roster);
        spoofed.claimed_from = Some("parent".to_string());
        let spoofed = wire(&token, spoofed);
        assert!(
            refusal(&round_trip(&mut channel, &spoofed)).contains("PI_HUB_IPC_SENDER")
        );

        let valid = wire(&token, request(Action::Roster));
        let error = refusal(&round_trip(&mut channel, &valid));
        assert!(
            error.contains("PI_HUB_IPC_REVOKED") || error.contains("PI_HUB_IPC_BUSY"),
            "valid credentials reached registry validation: {error}"
        );
        assert!(channel.listener.is_some());
        assert!(channel.connections.is_empty());
    }

    #[test]
    fn partial_frames_and_disconnects_do_not_become_requests() {
        let (_owner, mut channel) = channel();
        let mut client = TcpStream::connect_timeout(&channel.address, Duration::from_secs(1))
            .expect("connect");
        let mut frame = wire(&channel.token, request(Action::Roster));
        frame.pop();
        client.write_all(&frame).expect("write incomplete frame");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            channel.poll();
            assert_eq!(channel.connections.len(), 1);
            assert!(channel.connections[0].output.is_none());
            if channel.connections[0].input == frame {
                break;
            }
            assert!(Instant::now() < deadline, "partial input did not arrive");
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(client);
        while !channel.connections.is_empty() {
            channel.poll();
            assert!(Instant::now() < deadline, "disconnected client was retained");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn channel_cancellation_revokes_every_connection_and_the_listener() {
        let (owner, mut channel) = channel();
        let mut clients = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            clients.push(
                TcpStream::connect_timeout(&channel.address, Duration::from_secs(1))
                    .expect("connect"),
            );
            channel.poll();
        }
        assert_eq!(channel.connections.len(), MAX_CONNECTIONS);
        owner.set_cancel_requested(true);
        channel.poll();
        assert!(channel.connections.is_empty());
        assert!(channel.listener.is_none());
        assert!(channel.token.is_empty());
        assert!(
            TcpStream::connect_timeout(&channel.address, Duration::from_millis(100)).is_err()
        );
        drop(clients);
    }

    #[test]
    fn channel_deadline_and_drop_close_the_endpoint() {
        let (_owner, mut channel) = channel();
        let address = channel.address;
        channel.deadline = Instant::now();
        channel.poll();
        assert!(channel.listener.is_none());
        assert!(
            TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err()
        );

        let (_owner, owned) = self::channel();
        let address = owned.address;
        drop(owned);
        assert!(
            TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err()
        );
    }

    #[test]
    fn cancelled_client_never_connects_or_submits_a_frame() {
        let (owner, mut channel) = channel();
        owner.set_cancel_requested(true);
        let error = futures::executor::block_on(call(
            &owner,
            channel.address,
            channel.token.clone(),
            request(Action::Roster),
        ))
        .expect_err("cancelled client");
        assert!(error.to_string().contains("PI_HUB_IPC_CANCELLED"));
        assert!(
            channel
                .listener
                .as_ref()
                .expect("listener remains unpolled")
                .accept()
                .is_err()
        );
        channel.poll();
        assert!(channel.listener.is_none());
    }

    #[test]
    fn authenticated_dispatch_reaches_peers_and_parent_but_never_foreign_inboxes() {
        let temp = tempfile::tempdir().expect("hub directory");
        let mut hub = AgentHubRegistry::default();
        hub.set_dir_for_tests(temp.path().to_path_buf());
        let sender = hub.register("sender", "send message").expect("sender");
        let peer = hub.register("receiver", "receive message").expect("receiver");
        hub.mark_running(&sender.id, 10);
        hub.mark_running(&peer.id, 20);
        let mut send = request(Action::Send);
        send.name = Some(peer.id.clone());
        send.text = Some("first native message".to_string());
        let first = match dispatch(&mut hub, &sender.id, send).expect("send peer") {
            Reply::Sent { message } => message,
            _ => panic!("expected sent reply"),
        };
        assert_eq!(first.from, sender.id);
        assert_eq!(first.to, peer.id);
        let mut inbox = request(Action::Inbox);
        inbox.name = Some("self".to_string());
        let messages = match dispatch(&mut hub, &peer.id, inbox).expect("own inbox") {
            Reply::Inbox { messages, .. } => messages,
            _ => panic!("expected inbox"),
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].body, "first native message");
        assert!(
            std::fs::read_to_string(&peer.steer_path)
                .expect("persisted steering delivery")
                .contains("first native message")
        );

        let mut report = request(Action::Send);
        report.name = Some("parent".to_string());
        report.text = Some("child report".to_string());
        let message = match dispatch(&mut hub, &sender.id, report).expect("parent send") {
            Reply::Sent { message } => message,
            _ => panic!("expected sent reply"),
        };
        assert!(message.seq > first.seq);
        assert_eq!(hub.inbox("parent")[0].from, sender.id);
        assert_eq!(hub.inbox("parent")[0].body, "child report");

        for foreign in [sender.id.as_str(), "parent"] {
            let mut inbox = request(Action::Inbox);
            inbox.name = Some(foreign.to_string());
            assert!(
                dispatch(&mut hub, &peer.id, inbox)
                    .expect_err("foreign inbox must fail")
                    .to_string()
                    .contains("PI_HUB_IPC_INBOX")
            );
        }

        hub.mark_process_reaped(&peer.id);
        let mut late_send = request(Action::Send);
        late_send.name = Some(peer.id.clone());
        late_send.text = Some("must not queue after reap".to_string());
        assert!(
            dispatch(&mut hub, &sender.id, late_send)
                .expect_err("reaped recipient")
                .to_string()
                .contains("PI_HUB_IPC_RECIPIENT")
        );
        assert_eq!(hub.inbox(&peer.id).len(), 1);
        assert!(
            dispatch(&mut hub, &peer.id, request(Action::Roster))
                .expect_err("reaped sender")
                .to_string()
                .contains("PI_HUB_IPC_REVOKED")
        );
    }

    #[test]
    fn inbox_pages_are_bounded_ordered_and_expose_retention() {
        let mut hub = AgentHubRegistry::default();
        let body = "a".repeat(55 * 1024);
        for _ in 0..3 {
            hub.bus_send("parent", "sender-1", &body).expect("send");
        }
        let (messages, cursor, oldest) = match hub.inbox_page("parent", None).expect("page") {
            Reply::Inbox {
                messages,
                next_cursor,
                has_more,
                oldest_cursor,
                ..
            } => {
                assert!(has_more);
                assert_eq!(messages.len(), 2);
                (messages, next_cursor, oldest_cursor)
            }
            _ => panic!("expected inbox"),
        };
        assert!(messages[0].seq < messages[1].seq);
        assert_eq!(oldest, messages[0].seq);
        let page = hub.inbox_page("parent", Some(cursor)).expect("next page");
        assert!(serde_json::to_vec(&page).expect("encode page").len() < MAX_RESPONSE_BYTES);
        match page {
            Reply::Inbox {
                messages,
                next_cursor,
                has_more,
                ..
            } => {
                assert!(!has_more);
                assert_eq!(messages.len(), 1);
                assert!(next_cursor > cursor);
            }
            _ => panic!("expected inbox"),
        }
        for _ in 0..64 {
            hub.bus_send("parent", "sender-1", "newer").expect("send");
        }
        match hub.inbox_page("parent", None).expect("retained page") {
            Reply::Inbox {
                messages,
                oldest_cursor,
                has_more,
                ..
            } => {
                assert_eq!(messages.len(), 64);
                assert!(oldest_cursor > cursor);
                assert!(!has_more);
            }
            _ => panic!("expected inbox"),
        }
    }
}
