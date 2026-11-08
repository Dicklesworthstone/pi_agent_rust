//! Owned HTTP/WebSocket transport for the native terminal's remote bridge.
//!
//! The listener owns every upgraded socket. Authentication precedes access to
//! the latest frame, and browser input only enters the bridge's provenance-aware
//! queue; the transport never receives an agent command sender.

use super::{
    BindMode, ControlMode, EMBEDDED_WEB_CLIENT_HTML, EMBEDDED_WEB_CLIENT_JS, TokenKind,
    WebRemoteBridge, WebRemoteInput, WebRemoteSettings,
};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use asupersync::Cx;
use asupersync::http::h1::server::HostPolicy;
use asupersync::http::h1::{
    Http1Config, Http1Listener, Http1ListenerConfig, Http1Response, Http1Upgrade, Method, Request,
    Response,
};
use asupersync::net::websocket::{
    HttpRequest as UpgradeRequest, Message, ServerHandshake, ServerWebSocket, SlowConsumerPolicy,
    WebSocketAcceptor,
};
use asupersync::net::{TcpListener, TcpStream};
use asupersync::runtime::RuntimeBuilder;
use asupersync::server::shutdown::ShutdownSignal;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::process::Stdio;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_INBOUND_BYTES: usize = 32 * 1024;
// A bounded one-MiB terminal frame can expand sixfold when JSON escapes it.
const MAX_OUTBOUND_BYTES: usize = 6 * super::bridge::MAX_FRAME_BYTES + 4096;
const MAX_HTTP_CONNECTIONS: usize = 96;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const SEND_TIMEOUT: Duration = Duration::from_secs(2);
const FRAME_INTERVAL: Duration = Duration::from_millis(34);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_COMMANDS_PER_SECOND: usize = 120;
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; connect-src 'self'";

/// A bound listener and the thread that owns all of its connection tasks.
pub struct WebRemoteServer {
    bridge: WebRemoteBridge,
    local_addr: SocketAddr,
    pairing_url: String,
    shutdown: ShutdownSignal,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for WebRemoteServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The pairing URL contains a credential; it must never enter Debug logs.
        formatter
            .debug_struct("WebRemoteServer")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl WebRemoteServer {
    /// Bind before returning, so agent/session startup cannot race a failed port.
    pub fn start(settings: WebRemoteSettings) -> Result<Self> {
        settings.validate().map_err(Error::Config)?;
        let bridge = WebRemoteBridge::new(settings.clone());
        let kind = if settings.view_only {
            TokenKind::View
        } else {
            TokenKind::Steer
        };
        let token = bridge.manager().generate_token(kind)?;
        let thread_bridge = bridge.clone();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("pi-web-remote".to_owned())
            .spawn(move || {
                let _owner = ListenerGuard(thread_bridge.clone());
                if let Err(error) = run_listener(settings, &thread_bridge, &ready_tx) {
                    // Before startup this reports the bind failure. Afterwards
                    // the receiver is gone; do not log requests or credentials.
                    let _ = ready_tx.try_send(Err(error));
                }
            })?;
        match ready_rx.recv() {
            Ok(Ok(ready)) => Ok(Self {
                bridge,
                local_addr: ready.local_addr,
                pairing_url: format!("http://{}/#token={token}", ready.authority),
                shutdown: ready.shutdown,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error.into())
            }
            Err(error) => {
                let _ = thread.join();
                Err(io::Error::other(error).into())
            }
        }
    }

    #[must_use]
    pub fn bridge(&self) -> WebRemoteBridge {
        self.bridge.clone()
    }

    #[must_use]
    pub fn pairing_url(&self) -> &str {
        &self.pairing_url
    }

    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Stop admission, retire every socket, and join the listener's owner.
    pub fn shutdown(&mut self) {
        self.bridge.close();
        // Http1Listener's upgrade supervisor drops even a pending recv/send on
        // force-close, then joins all connection tasks before run() returns.
        let _ = self.shutdown.begin_drain(Duration::ZERO);
        let _ = self.shutdown.begin_force_close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for WebRemoteServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Ready {
    local_addr: SocketAddr,
    authority: String,
    shutdown: ShutdownSignal,
}

struct ServerState {
    bridge: WebRemoteBridge,
    authorities: Vec<String>,
    content_security_policy: String,
}

struct ListenerGuard(WebRemoteBridge);

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn run_listener(
    settings: WebRemoteSettings,
    bridge: &WebRemoteBridge,
    ready: &mpsc::SyncSender<io::Result<Ready>>,
) -> io::Result<()> {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .map_err(io::Error::other)?;
    let handle = runtime.handle();
    runtime.block_on(async {
        let listener = bind_listener(&settings).await?;
        let local_addr = listener.local_addr()?;
        let (authority, authorities) = bound_authorities(local_addr);
        let http_config = Http1Config::default()
            .max_headers_size(8192)
            .max_body_size(0)
            .keep_alive(false)
            .idle_timeout(Some(HTTP_TIMEOUT))
            .request_timeout(Some(HTTP_TIMEOUT))
            .request_drain_grace(Duration::from_millis(100))
            .host_policy(HostPolicy::AllowList(authorities.clone()));
        let config = Http1ListenerConfig::default()
            .http_config(http_config)
            // Incomplete HTTP/authentication requests do not consume the much
            // smaller authenticated viewer allowance.
            .max_connections(Some(MAX_HTTP_CONNECTIONS))
            .drain_timeout(Duration::from_millis(100))
            .hard_drain_timeout(Duration::from_secs(2));
        let state = Arc::new(ServerState {
            bridge: bridge.clone(),
            content_security_policy: content_security_policy(&authorities),
            authorities,
        });
        let listener = Http1Listener::from_listener_upgradeable(
            listener,
            move |request| {
                let state = Arc::clone(&state);
                async move { route_request(request, state) }
            },
            config,
        );
        ready
            .send(Ok(Ready {
                local_addr,
                authority,
                shutdown: listener.shutdown_signal(),
            }))
            .map_err(|_| io::Error::other("web listener owner disappeared"))?;
        listener.run(&handle).await.map(|_| ())
    })
}

async fn bind_listener(settings: &WebRemoteSettings) -> io::Result<TcpListener> {
    match settings.bind_mode {
        BindMode::Loopback => TcpListener::bind((Ipv4Addr::LOCALHOST, settings.port)).await,
        BindMode::Lan => {
            eprintln!("Web remote LAN mode: accepting connections on local network interfaces.");
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, settings.port)).await
        }
        BindMode::Tailscale => {
            if let Some(address) = tailscale_address().await {
                match TcpListener::bind((address, settings.port)).await {
                    Ok(listener) => return Ok(listener),
                    Err(error) if error.kind() == io::ErrorKind::AddrNotAvailable => {}
                    Err(error) => return Err(error),
                }
            }
            eprintln!(
                "No usable local Tailscale IPv4 address was found; web remote is using loopback."
            );
            TcpListener::bind((Ipv4Addr::LOCALHOST, settings.port)).await
        }
    }
}

async fn tailscale_address() -> Option<Ipv4Addr> {
    let owner = AgentCx::for_current_or_request();
    let mut command = owner.process().command("tailscale");
    command.args(["ip", "-4"]).stdin(Stdio::null());
    let output = command
        .output_with_limits(4096, 1024, Duration::from_secs(2))
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let address = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse::<Ipv4Addr>()
        .ok()?;
    // Only the tailnet's CGNAT range is eligible. Binding below independently
    // verifies that the address belongs to this machine.
    (u32::from(address) & 0xffc0_0000 == 0x6440_0000).then_some(address)
}

fn bound_authorities(address: SocketAddr) -> (String, Vec<String>) {
    if !address.ip().is_unspecified() && !address.ip().is_loopback() {
        let authority = address.to_string();
        return (authority.clone(), http_authorities(vec![authority]));
    }
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, address.port())).to_string();
    let mut authorities = vec![loopback.clone(), format!("localhost:{}", address.port())];
    let authority = if address.ip().is_unspecified() {
        primary_lan_address().map_or(loopback, |ip| {
            let authority = SocketAddr::new(ip, address.port()).to_string();
            authorities.push(authority.clone());
            authority
        })
    } else {
        loopback
    };
    (authority, http_authorities(authorities))
}

fn http_authorities(mut authorities: Vec<String>) -> Vec<String> {
    // Browsers omit HTTP's default port from Host and Origin even when the
    // pairing URL explicitly contains it. Both spellings identify this socket.
    let default_port_aliases = authorities
        .iter()
        .filter_map(|authority| authority.strip_suffix(":80").map(str::to_owned))
        .collect::<Vec<_>>();
    authorities.extend(default_port_aliases);
    authorities
}

fn content_security_policy(authorities: &[String]) -> String {
    let mut policy = CONTENT_SECURITY_POLICY.to_owned();
    // Some browsers do not include ws:// in an HTTP page's 'self' source.
    // These literal authorities come from the bound socket/local route, never
    // from a request header, so granting them does not broaden the trust boundary.
    for authority in authorities {
        let _ = write!(policy, " ws://{authority}");
    }
    policy
}

fn primary_lan_address() -> Option<IpAddr> {
    // UDP connect only consults the local routing table; no datagram is sent and
    // no external service is contacted. Fail closed to the loopback authority.
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let address = socket.local_addr().ok()?.ip();
    if address.is_unspecified() || address.is_loopback() || address.is_multicast() {
        return None;
    }
    // Do not admit a merely claimed source address into the Host allowlist.
    let probe = std::net::TcpListener::bind(SocketAddr::new(address, 0)).ok()?;
    drop(probe);
    Some(address)
}

fn route_request(request: Request, state: Arc<ServerState>) -> Http1Response {
    if state.bridge.is_closed() {
        return plain_response(503, "Service Unavailable", "Session closed.");
    }
    let Some(host) = unique_header(&request, "host") else {
        return plain_response(403, "Forbidden", "Invalid host.");
    };
    if !state
        .authorities
        .iter()
        .any(|authority| authority.eq_ignore_ascii_case(host))
    {
        return plain_response(403, "Forbidden", "Invalid host.");
    }
    let origin_count = request
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("origin"))
        .count();
    if origin_count > 1
        || unique_header(&request, "origin")
            .is_some_and(|origin| origin != format!("http://{host}"))
    {
        return plain_response(403, "Forbidden", "Invalid origin.");
    }
    if request.method != Method::Get {
        let mut response = plain_response(405, "Method Not Allowed", "Only GET is supported.");
        response
            .response
            .headers
            .push(("Allow".to_owned(), "GET".to_owned()));
        return response;
    }
    if !request.body.is_empty() || !request.trailers.is_empty() {
        return plain_response(400, "Bad Request", "Request bodies are not supported.");
    }
    // Exact paths also reject credential-bearing queries and fragments before
    // routing. Authentication only happens inside the WebSocket hello message.
    match request.uri.as_str() {
        "/" => Http1Response::new(secure_response(
            200,
            "OK",
            "text/html; charset=utf-8",
            EMBEDDED_WEB_CLIENT_HTML,
            &state.content_security_policy,
        )),
        "/client.js" => Http1Response::new(secure_response(
            200,
            "OK",
            "text/javascript; charset=utf-8",
            EMBEDDED_WEB_CLIENT_JS,
            &state.content_security_policy,
        )),
        "/ws" if origin_count == 1 => upgrade_response(&request, state),
        "/ws" => plain_response(403, "Forbidden", "An origin is required."),
        _ => plain_response(404, "Not Found", "Not found."),
    }
}

fn unique_header<'a>(request: &'a Request, wanted: &str) -> Option<&'a str> {
    let mut values = request
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.as_str());
    let first = values.next()?;
    values.next().is_none().then_some(first)
}

fn secure_response(
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
    content_security_policy: &str,
) -> Response {
    Response::new(status, reason, body.as_bytes().to_vec())
        .with_header("Content-Type", content_type)
        .with_header("Cache-Control", "no-store")
        .with_header("Pragma", "no-cache")
        .with_header("Content-Security-Policy", content_security_policy)
        .with_header("Referrer-Policy", "no-referrer")
        .with_header("X-Content-Type-Options", "nosniff")
        .with_header("Cross-Origin-Resource-Policy", "same-origin")
        .with_header("Cross-Origin-Opener-Policy", "same-origin")
}

fn plain_response(status: u16, reason: &str, body: &str) -> Http1Response {
    Http1Response::new(secure_response(
        status,
        reason,
        "text/plain; charset=utf-8",
        body,
        CONTENT_SECURITY_POLICY,
    ))
}

fn upgrade_response(request: &Request, state: Arc<ServerState>) -> Http1Response {
    let mut raw = format!("GET /ws {}\r\n", request.version.as_str());
    for (name, value) in &request.headers {
        let _ = write!(raw, "{name}: {value}\r\n");
    }
    raw.push_str("\r\n");
    let accepted = UpgradeRequest::parse(raw.as_bytes())
        .and_then(|request| ServerHandshake::new().accept(&request));
    let Ok(accepted) = accepted else {
        return plain_response(400, "Bad Request", "Invalid WebSocket upgrade.");
    };
    let response = secure_response(
        101,
        "Switching Protocols",
        "application/octet-stream",
        "",
        &state.content_security_policy,
    )
    .with_header("Upgrade", "websocket")
    .with_header("Connection", "Upgrade")
    .with_header("Sec-WebSocket-Accept", &accepted.accept_key);
    let peer = request
        .peer_addr
        .map_or_else(|| "unknown".to_owned(), |peer| peer.to_string());
    let upgrade = Http1Upgrade::new(move |cx, stream, read_ahead| async move {
        let acceptor = WebSocketAcceptor::new()
            .max_frame_size(MAX_INBOUND_BYTES)
            .max_message_size(MAX_INBOUND_BYTES)
            .max_pending_write_bytes(MAX_OUTBOUND_BYTES + 14)
            .slow_consumer_policy(SlowConsumerPolicy::CloseWithTryAgainLater)
            .ping_interval(None)
            .close_timeout(Duration::from_millis(100));
        let socket = acceptor.finish_upgrade(stream, accepted, &read_ahead);
        serve_websocket(&cx, socket, state.bridge.clone(), peer).await;
    });
    Http1Response::new(response).with_upgrade(upgrade)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Hello {
    Hello { token: String },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ClientCommand {
    Takeover,
    Release,
    Input { seq: u64, input: WebRemoteInput },
    Resync,
    Ack { seq: u64 },
}

struct ClientGuard {
    bridge: WebRemoteBridge,
    client_id: String,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.bridge.manager().disconnect_client(&self.client_id);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ControlStatus {
    mode: ControlMode,
    is_controller: bool,
    view_only: bool,
}

fn control_status(bridge: &WebRemoteBridge, client_id: &str) -> Option<ControlStatus> {
    let client = bridge
        .manager()
        .clients()
        .into_iter()
        .find(|client| client.client_id == client_id)?;
    let mode = if client.is_view_only {
        ControlMode::ViewOnly
    } else if client.is_controller {
        ControlMode::RemoteControlling
    } else if bridge
        .manager()
        .pending_takeovers()
        .iter()
        .any(|id| id == client_id)
    {
        ControlMode::TakeoverPendingApproval
    } else {
        ControlMode::LocalTerminal
    };
    Some(ControlStatus {
        mode,
        is_controller: client.is_controller,
        view_only: client.is_view_only,
    })
}

async fn serve_websocket(
    cx: &Cx,
    mut socket: ServerWebSocket<TcpStream>,
    bridge: WebRemoteBridge,
    peer: String,
) {
    let first = asupersync::time::timeout(now(cx), HELLO_TIMEOUT, socket.recv(cx)).await;
    let Ok(Ok(Some(first))) = first else { return };
    let Some(Hello::Hello { token }) = decode_message(&first) else {
        let _ = send_error(cx, &mut socket, "hello_required").await;
        return;
    };
    let client_id = uuid::Uuid::new_v4().to_string();
    let Ok(client) = bridge
        .manager()
        .connect_client(&client_id, &peer, Some(&token))
    else {
        let _ = send_error(cx, &mut socket, "authentication_or_capacity").await;
        return;
    };
    drop(token);
    let _guard = ClientGuard {
        bridge: bridge.clone(),
        client_id: client_id.clone(),
    };
    if !send_json(
        cx,
        &mut socket,
        &serde_json::json!({
            "type": "hello", "client_id": client_id, "view_only": client.is_view_only,
        }),
    )
    .await
    {
        return;
    }
    let mut progress = ClientProgress::new();
    while !bridge.is_closed() {
        if Instant::now() >= progress.next_frame {
            if !send_updates(cx, &mut socket, &bridge, &client_id, &mut progress).await {
                break;
            }
            progress.next_frame = Instant::now() + FRAME_INTERVAL;
        }
        let wait = progress
            .next_frame
            .saturating_duration_since(Instant::now());
        // ServerWebSocket::recv retains partial frames, fragmented messages and
        // close state in self and is explicitly cancel-safe. A frame tick can
        // drop this recv future without losing bytes; send timeouts instead
        // retire the entire socket because partially written sends are not safe
        // to resume as a different message.
        let received = asupersync::time::timeout(now(cx), wait, socket.recv(cx)).await;
        let message = match received {
            Err(_) => continue,
            Ok(Ok(Some(Message::Close(_)) | None) | Err(_)) => break,
            Ok(Ok(Some(message))) => message,
        };
        if !progress.admit_command() {
            let _ = send_error(cx, &mut socket, "rate_limit").await;
            break;
        }
        let Some(command) = decode_message(&message) else {
            let _ = send_error(cx, &mut socket, "invalid_command").await;
            break;
        };
        if let Err(code) = apply_command(&bridge, &client_id, command, &mut progress)
            && !send_error(cx, &mut socket, code).await
        {
            break;
        }
    }
}

struct ClientProgress {
    sent: u64,
    acknowledged: u64,
    control: Option<ControlStatus>,
    next_frame: Instant,
    rate_started: Instant,
    command_count: usize,
}

impl ClientProgress {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            sent: 0,
            acknowledged: 0,
            control: None,
            next_frame: now,
            rate_started: now,
            command_count: 0,
        }
    }

    fn admit_command(&mut self) -> bool {
        if self.rate_started.elapsed() >= Duration::from_secs(1) {
            self.rate_started = Instant::now();
            self.command_count = 0;
        }
        self.command_count += 1;
        self.command_count <= MAX_COMMANDS_PER_SECOND
    }
}

async fn send_updates(
    cx: &Cx,
    socket: &mut ServerWebSocket<TcpStream>,
    bridge: &WebRemoteBridge,
    client_id: &str,
    progress: &mut ClientProgress,
) -> bool {
    let Some(control) = control_status(bridge, client_id) else {
        return false;
    };
    if progress.control != Some(control) {
        if !send_json(
            cx,
            socket,
            &serde_json::json!({
                "type": "control", "mode": control.mode,
                "is_controller": control.is_controller, "view_only": control.view_only,
            }),
        )
        .await
        {
            return false;
        }
        progress.control = Some(control);
    }
    if let Some(frame) = bridge.frame_after(progress.acknowledged)
        && frame.seq != progress.sent
    {
        if !send_json(cx, socket, frame.as_ref()).await {
            return false;
        }
        progress.sent = frame.seq;
    }
    true
}

fn apply_command(
    bridge: &WebRemoteBridge,
    client_id: &str,
    command: ClientCommand,
    progress: &mut ClientProgress,
) -> std::result::Result<(), &'static str> {
    match command {
        ClientCommand::Takeover => bridge
            .queue_takeover(client_id)
            .map(|_| ())
            .map_err(|_| "takeover_denied"),
        ClientCommand::Release => {
            bridge.release_control(client_id);
            Ok(())
        }
        ClientCommand::Input { seq, input } => bridge
            .queue_input(client_id, seq, input)
            .map_err(|_| "input_denied"),
        ClientCommand::Resync => {
            progress.sent = 0;
            progress.acknowledged = 0;
            bridge.request_refresh();
            Ok(())
        }
        ClientCommand::Ack { seq } if seq >= progress.acknowledged && seq <= progress.sent => {
            progress.acknowledged = seq;
            Ok(())
        }
        ClientCommand::Ack { .. } => Err("invalid_ack"),
    }
}

fn decode_message<T: for<'de> Deserialize<'de>>(message: &Message) -> Option<T> {
    let bytes = match message {
        Message::Text(text) => text.as_bytes(),
        Message::Binary(bytes) => bytes.as_ref(),
        _ => return None,
    };
    if bytes.len() > MAX_INBOUND_BYTES {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

fn now(cx: &Cx) -> asupersync::types::Time {
    cx.timer_driver()
        .map_or_else(asupersync::time::wall_now, |driver| driver.now())
}

async fn send_json<T: Serialize + ?Sized>(
    cx: &Cx,
    socket: &mut ServerWebSocket<TcpStream>,
    value: &T,
) -> bool {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return false;
    };
    if bytes.len() > MAX_OUTBOUND_BYTES {
        return false;
    }
    matches!(
        asupersync::time::timeout(
            now(cx),
            SEND_TIMEOUT,
            socket.send(cx, Message::binary(bytes))
        )
        .await,
        Ok(Ok(()))
    )
}

async fn send_error(cx: &Cx, socket: &mut ServerWebSocket<TcpStream>, code: &str) -> bool {
    send_json(
        cx,
        socket,
        &serde_json::json!({ "type": "error", "code": code }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_remote::{WebRemoteEvent, WebRemoteInput};
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::TcpStream as BlockingStream;

    fn server(max_viewers: usize) -> WebRemoteServer {
        WebRemoteServer::start(WebRemoteSettings {
            port: 0,
            max_viewers,
            ..WebRemoteSettings::default()
        })
        .expect("bind live web listener")
    }

    fn token(server: &WebRemoteServer) -> &str {
        server.pairing_url().split_once("#token=").unwrap().1
    }

    fn connect(address: SocketAddr) -> BlockingStream {
        let socket = BlockingStream::connect(address).expect("connect live listener");
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket.set_nodelay(true).unwrap();
        socket
    }

    fn read_headers(socket: &mut BlockingStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 16 * 1024, "bounded HTTP response headers");
            let mut byte = [0];
            socket.read_exact(&mut byte).expect("HTTP response header");
            bytes.push(byte[0]);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn upgrade_bytes(address: SocketAddr, origin: &str) -> Vec<u8> {
        format!(
            "GET /ws HTTP/1.1\r\nHost: {address}\r\nOrigin: {origin}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        ).into_bytes()
    }

    fn websocket(server: &WebRemoteServer) -> BlockingStream {
        let address = server.local_addr();
        let mut socket = connect(address);
        socket
            .write_all(&upgrade_bytes(address, &format!("http://{address}")))
            .unwrap();
        assert!(read_headers(&mut socket).starts_with("HTTP/1.1 101 "));
        socket
    }

    fn masked_frame(opcode: u8, final_frame: bool, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![opcode | if final_frame { 0x80 } else { 0 }];
        if payload.len() < 126 {
            bytes.push(0x80 | u8::try_from(payload.len()).unwrap());
        } else {
            bytes.push(0x80 | 126);
            bytes.extend_from_slice(&u16::try_from(payload.len()).unwrap().to_be_bytes());
        }
        let mask = [0x12, 0x34, 0x56, 0x78];
        bytes.extend_from_slice(&mask);
        bytes.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        bytes
    }

    fn send(socket: &mut BlockingStream, value: &Value) {
        socket
            .write_all(&masked_frame(2, true, &serde_json::to_vec(value).unwrap()))
            .unwrap();
    }

    fn read_frame(socket: &mut BlockingStream) -> io::Result<(u8, Vec<u8>)> {
        let mut header = [0; 2];
        socket.read_exact(&mut header)?;
        assert_ne!(header[0] & 0x80, 0, "complete server frame");
        assert_eq!(header[1] & 0x80, 0, "server frames are not masked");
        let length = match header[1] & 0x7f {
            126 => {
                let mut bytes = [0; 2];
                socket.read_exact(&mut bytes)?;
                usize::from(u16::from_be_bytes(bytes))
            }
            127 => {
                let mut bytes = [0; 8];
                socket.read_exact(&mut bytes)?;
                usize::try_from(u64::from_be_bytes(bytes)).unwrap()
            }
            length => usize::from(length),
        };
        assert!(length <= MAX_OUTBOUND_BYTES, "bounded server frame");
        let mut payload = vec![0; length];
        socket.read_exact(&mut payload)?;
        Ok((header[0] & 0x0f, payload))
    }

    fn receive(socket: &mut BlockingStream) -> Value {
        let (opcode, payload) = read_frame(socket).expect("WebSocket response");
        assert_eq!(opcode, 2, "application protocol uses binary UTF-8 JSON");
        serde_json::from_slice(&payload).unwrap()
    }

    fn receive_matching(socket: &mut BlockingStream, predicate: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..16 {
            let value = receive(socket);
            if predicate(&value) {
                return value;
            }
        }
        panic!("expected response was not received");
    }

    fn authenticate(socket: &mut BlockingStream, token: &str) -> Value {
        send(socket, &json!({ "type": "hello", "token": token }));
        let response = receive(socket);
        assert_eq!(response["type"], "hello");
        response
    }

    fn next_event(bridge: &WebRemoteBridge) -> WebRemoteEvent {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(event) = bridge.recv_event_timeout(Duration::from_millis(100)) {
                return event;
            }
            assert!(Instant::now() < deadline, "expected native bridge event");
        }
    }

    #[test]
    fn live_http_assets_are_private_and_bound_to_the_listener_authority() {
        let server = server(4);
        let address = server.local_addr();
        let same_origin = format!("http://{address}");
        for path in ["/", "/client.js"] {
            let mut socket = connect(address);
            write!(socket, "GET {path} HTTP/1.1\r\nHost: {address}\r\n\r\n").unwrap();
            let headers = read_headers(&mut socket);
            assert!(headers.starts_with("HTTP/1.1 200 "));
            let normalized = headers.to_ascii_lowercase();
            assert!(normalized.contains("cache-control: no-store"));
            assert!(normalized.contains("script-src 'self'"));
            assert!(normalized.contains(&format!("connect-src 'self' ws://{address}")));
            assert!(normalized.contains("frame-ancestors 'none'"));
            assert!(normalized.contains("referrer-policy: no-referrer"));
            let mut body = String::new();
            socket.read_to_string(&mut body).unwrap();
            assert!(!body.contains(token(&server)));
        }
        for (path, host, origin) in [
            ("/", format!("attacker.example:{}", address.port()), None),
            ("/", address.to_string(), Some("http://attacker.example")),
            ("/ws", address.to_string(), None),
            (
                "/ws?token=never-accept-query-tokens",
                address.to_string(),
                Some(same_origin.as_str()),
            ),
        ] {
            let mut socket = connect(address);
            write!(socket, "GET {path} HTTP/1.1\r\nHost: {host}\r\n").unwrap();
            if let Some(origin) = origin {
                write!(socket, "Origin: {origin}\r\n").unwrap();
            }
            socket.write_all(b"\r\n").unwrap();
            let headers = read_headers(&mut socket);
            assert!(!headers.starts_with("HTTP/1.1 200 "));
            assert!(!headers.starts_with("HTTP/1.1 101 "));
        }
        assert_eq!(server.bridge().manager().clients().len(), 0);
    }

    #[test]
    fn live_websocket_sends_no_frames_until_hello_and_preserves_upgrade_read_ahead() {
        let server = server(1);
        let bridge = server.bridge();
        bridge.set_secrets(vec!["live-network-secret".to_owned()]);
        assert!(bridge.publish_text(
            80,
            24,
            format!(
                "private live terminal live-network-secret {}",
                token(&server)
            )
        ));
        let mut waiting = websocket(&server);
        waiting
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut probe = [0];
        let error = waiting
            .read(&mut probe)
            .expect_err("no unauthenticated application data");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
        assert_eq!(bridge.manager().clients().len(), 0);

        // A pending anonymous socket must not occupy the sole viewer slot. The
        // hello is pipelined after HTTP headers in the same network write.
        let address = server.local_addr();
        let mut socket = connect(address);
        let mut request = upgrade_bytes(address, &format!("http://{address}"));
        request.extend(masked_frame(
            2,
            true,
            &serde_json::to_vec(&json!({
                "type": "hello", "token": token(&server),
            }))
            .unwrap(),
        ));
        socket.write_all(&request).unwrap();
        assert!(read_headers(&mut socket).starts_with("HTTP/1.1 101 "));
        assert_eq!(receive(&mut socket)["type"], "hello");
        let frame = receive_matching(&mut socket, |value| value["frame_type"] == "keyframe");
        let terminal = frame["data"].as_str().unwrap();
        assert!(terminal.starts_with("private live terminal"));
        assert!(!terminal.contains("live-network-secret"));
        assert!(!terminal.contains(token(&server)));
        assert_eq!(bridge.manager().clients().len(), 1);

        let mut replay = websocket(&server);
        send(
            &mut replay,
            &json!({ "type": "hello", "token": token(&server) }),
        );
        assert_eq!(receive(&mut replay)["type"], "error");
        assert_eq!(bridge.manager().clients().len(), 1);
    }

    #[test]
    fn live_admission_preserves_a_token_when_viewer_capacity_is_full() {
        let server = server(1);
        let bridge = server.bridge();
        let second_token = bridge.manager().generate_token(TokenKind::View).unwrap();
        let mut first = websocket(&server);
        let hello = authenticate(&mut first, token(&server));
        let first_id = hello["client_id"].as_str().unwrap();
        let mut denied = websocket(&server);
        send(
            &mut denied,
            &json!({ "type": "hello", "token": second_token }),
        );
        assert_eq!(receive(&mut denied)["code"], "authentication_or_capacity");
        bridge.manager().disconnect_client(first_id);
        let mut accepted = websocket(&server);
        assert_eq!(
            authenticate(&mut accepted, &second_token)["view_only"],
            true
        );
        send(&mut accepted, &json!({ "type": "takeover" }));
        assert_eq!(
            receive_matching(&mut accepted, |v| v["type"] == "error")["code"],
            "takeover_denied"
        );
        send(
            &mut accepted,
            &json!({ "type": "input", "seq": 1, "input": { "kind": "abort" } }),
        );
        assert_eq!(
            receive_matching(&mut accepted, |v| v["type"] == "error")["code"],
            "input_denied"
        );
        assert_eq!(bridge.manager().pending_takeovers().len(), 0);
    }

    #[test]
    fn fragmented_commands_survive_frame_ticks_and_input_needs_local_approval() {
        let server = server(4);
        let bridge = server.bridge();
        let mut socket = websocket(&server);
        let hello = authenticate(&mut socket, token(&server));
        let client_id = hello["client_id"].as_str().unwrap();
        send(
            &mut socket,
            &json!({ "type": "input", "seq": 1, "input": { "kind": "paste", "text": "blocked" } }),
        );
        assert_eq!(
            receive_matching(&mut socket, |v| v["type"] == "error")["code"],
            "input_denied"
        );

        socket
            .write_all(&masked_frame(1, false, br#"{"type":"take"#))
            .unwrap();
        // Observing a frame tick while the inbound JSON remains fragmented is
        // the synchronization barrier; no arbitrary scheduling sleep is needed.
        assert!(bridge.publish_text(80, 24, "still rendering".to_owned()));
        receive_matching(&mut socket, |value| value["frame_type"] == "keyframe");
        socket
            .write_all(&masked_frame(0, true, br#"over"}"#))
            .unwrap();
        assert!(
            matches!(next_event(&bridge), WebRemoteEvent::TakeoverRequested { client_id: id } if id == client_id)
        );
        assert!(!bridge.manager().clients()[0].is_controller);
        receive_matching(&mut socket, |value| {
            value["mode"] == "takeover_pending_approval"
        });

        bridge.manager().approve_takeover(client_id).unwrap();
        receive_matching(&mut socket, |value| value["mode"] == "remote_controlling");
        send(
            &mut socket,
            &json!({ "type": "input", "seq": 1, "input": { "kind": "paste", "text": "approved" } }),
        );
        let epoch = match next_event(&bridge) {
            WebRemoteEvent::Input {
                client_id: id,
                epoch,
                input: WebRemoteInput::Paste { text },
            } => {
                assert_eq!(id, client_id);
                assert_eq!(text, "approved");
                epoch
            }
            event => panic!("unexpected event: {event:?}"),
        };
        assert!(bridge.manager().input_authorized(client_id, epoch));
        send(&mut socket, &json!({ "type": "release" }));
        receive_matching(&mut socket, |value| value["mode"] == "local_terminal");
        assert!(!bridge.manager().input_authorized(client_id, epoch));
        send(
            &mut socket,
            &json!({ "type": "input", "seq": 2, "input": { "kind": "abort" } }),
        );
        assert_eq!(
            receive_matching(&mut socket, |v| v["type"] == "error")["code"],
            "input_denied"
        );
    }

    #[test]
    fn live_frame_delivery_uses_acknowledged_bases_and_recovers_lag_with_keyframes() {
        let server = server(4);
        let bridge = server.bridge();
        let mut socket = websocket(&server);
        authenticate(&mut socket, token(&server));
        let stable = "unchanged line ".repeat(40);
        assert!(bridge.publish_text(100, 24, format!("first\n{stable}\n{stable}")));
        let first = receive_matching(&mut socket, |v| v["frame_type"] == "keyframe");
        assert!(bridge.publish_text(100, 24, format!("second\n{stable}\n{stable}")));
        let second = receive_matching(&mut socket, |v| v["seq"].as_u64() > first["seq"].as_u64());
        assert_eq!(
            second["frame_type"], "keyframe",
            "an unacknowledged base cannot receive a patch"
        );
        send(&mut socket, &json!({ "type": "ack", "seq": second["seq"] }));
        // TCP ordering plus the denial response proves the preceding ack was
        // processed before publishing a patch that depends on it.
        send(
            &mut socket,
            &json!({ "type": "input", "seq": 1, "input": { "kind": "abort" } }),
        );
        receive_matching(&mut socket, |v| v["type"] == "error");
        assert!(bridge.publish_text(100, 24, format!("third\n{stable}\n{stable}")));
        let third = receive_matching(&mut socket, |v| v["frame_type"] == "patch");
        let patch: Value = serde_json::from_str(third["data"].as_str().unwrap()).unwrap();
        assert_eq!(patch["base_seq"], second["seq"]);
        assert_eq!(patch["lines"][0], json!([0, "third"]));
        send(&mut socket, &json!({ "type": "resync" }));
        let recovered = receive_matching(&mut socket, |v| v["frame_type"] == "keyframe");
        assert_eq!(recovered["seq"], third["seq"]);
        assert_eq!(recovered["data"], format!("third\n{stable}\n{stable}"));
    }

    #[test]
    fn oversized_inbound_frame_is_rejected_before_its_payload_arrives() {
        let server = server(4);
        let mut socket = websocket(&server);
        authenticate(&mut socket, token(&server));
        receive_matching(&mut socket, |v| v["type"] == "control");
        let length = u16::try_from(MAX_INBOUND_BYTES + 1).unwrap();
        let mut header = vec![0x82, 0xfe];
        header.extend_from_slice(&length.to_be_bytes());
        header.extend_from_slice(&[1, 2, 3, 4]);
        socket.write_all(&header).unwrap();
        match read_frame(&mut socket) {
            Ok((opcode, _)) => assert_eq!(opcode, 8, "only a close is permitted"),
            Err(error) => assert!(matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            )),
        }
    }

    #[test]
    fn unauthenticated_websocket_is_retired_at_the_hello_deadline() {
        let server = server(1);
        let mut socket = websocket(&server);
        socket
            .set_read_timeout(Some(HELLO_TIMEOUT + Duration::from_secs(3)))
            .unwrap();
        let mut byte = [0];
        match socket.read(&mut byte) {
            Ok(count) => assert_eq!(count, 0, "no application bytes before authentication"),
            Err(error) => assert!(matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            )),
        }
        assert_eq!(server.bridge().manager().clients().len(), 0);
    }

    #[test]
    fn token_revocation_retires_the_live_websocket() {
        let server = server(1);
        let bridge = server.bridge();
        let mut socket = websocket(&server);
        authenticate(&mut socket, token(&server));
        receive_matching(&mut socket, |value| value["type"] == "control");
        assert!(bridge.manager().revoke_token(token(&server)));
        match read_frame(&mut socket) {
            Ok((opcode, _)) => assert_eq!(opcode, 8, "revoked viewers cannot receive data"),
            Err(error) => assert!(matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
            )),
        }
        assert_eq!(bridge.manager().clients().len(), 0);
    }

    #[test]
    fn server_drop_joins_authenticated_and_incomplete_connections_and_releases_port() {
        let server = server(4);
        let address = server.local_addr();
        let bridge = server.bridge();
        let mut authenticated = websocket(&server);
        authenticate(&mut authenticated, token(&server));
        let _awaiting_hello = websocket(&server);
        let mut partial_http = connect(address);
        partial_http.write_all(b"GET / HTTP/1.1\r\nHost: ").unwrap();
        let started = Instant::now();
        drop(server);
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(bridge.is_closed());
        assert_eq!(bridge.manager().clients().len(), 0);
        let rebound = std::net::TcpListener::bind(address)
            .expect("server port released after joined shutdown");
        drop(rebound);
    }

    #[test]
    fn startup_reports_bind_failure_before_creating_a_server() {
        let occupied = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let error = WebRemoteServer::start(WebRemoteSettings {
            port: occupied.local_addr().unwrap().port(),
            ..WebRemoteSettings::default()
        })
        .expect_err("occupied port must fail startup");
        assert!(matches!(error, Error::Io(error) if error.kind() == io::ErrorKind::AddrInUse));
    }
}
