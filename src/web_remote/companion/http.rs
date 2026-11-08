//! Small, bounded HTTP/1.1 transport for a loopback-only application. There is
//! no proxying, filesystem serving, request pipelining, or remote input stream.

use super::{
    CompanionHandle, MAX_PROMPT_BYTES, PROPOSAL_INTERVAL, PROPOSAL_LIFETIME, PromptProposal,
    ProposalReceipt, ProposalSlot, TokenKind, Viewer, random_token, token_hash,
};
use serde::Deserialize;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 32;
const MAX_HEADERS: usize = 8 * 1024;
// JSON may escape each permitted byte (for example newlines or tabs). The
// decoded prompt has its own smaller limit, checked before queue admission.
const MAX_BODY: usize = MAX_PROMPT_BYTES * 6 + 1024;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const HTML: &str = include_str!("client.html");
const JAVASCRIPT: &str = include_str!("client.js");
const CSS: &str = include_str!("client.css");

pub(super) fn serve(listener: TcpListener, handle: &CompanionHandle, stop: &AtomicBool) {
    let mut connections: Vec<Connection> = Vec::new();
    while !stop.load(Ordering::Acquire) {
        for _ in 0..8 {
            match listener.accept() {
                Ok((socket, address)) => {
                    if address.ip().is_loopback()
                        && connections.len() < MAX_CONNECTIONS
                        && socket.set_nonblocking(true).is_ok()
                    {
                        connections.push(Connection::new(socket));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
        connections.retain_mut(|connection| connection.poll(handle));
        std::thread::park_timeout(POLL_INTERVAL);
    }
}

struct Connection {
    socket: TcpStream,
    input: Vec<u8>,
    response: Option<Response>,
    written: usize,
    deadline: Instant,
}

impl Connection {
    fn new(socket: TcpStream) -> Self {
        Self {
            socket,
            input: Vec::new(),
            response: None,
            written: 0,
            deadline: Instant::now() + EXCHANGE_TIMEOUT,
        }
    }

    fn poll(&mut self, handle: &CompanionHandle) -> bool {
        if Instant::now() >= self.deadline {
            return false;
        }
        if self.response.is_none() {
            let mut buffer = [0_u8; 4096];
            // Fairness is per connection as well as per accept pass.
            for _ in 0..4 {
                match self.socket.read(&mut buffer) {
                    Ok(0) => return false,
                    Ok(count) => {
                        if self.input.len().saturating_add(count) > MAX_HEADERS + MAX_BODY {
                            self.response = Some(Response::error(413, "Request is too large."));
                            break;
                        }
                        self.input.extend_from_slice(&buffer[..count]);
                        match parse(&self.input) {
                            Ok(Some(request)) => {
                                self.response = Some(dispatch(handle, request));
                                break;
                            }
                            Ok(None) => {}
                            Err(response) => {
                                self.response = Some(response);
                                break;
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => return false,
                }
            }
        }
        let Some(response) = &self.response else {
            return true;
        };
        // Recheck before each write: revocation also closes slow responses,
        // rather than leaving a previously authenticated socket with authority.
        if let Some(client) = response.client {
            let mut state = handle
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.expire(Instant::now());
            if !state.active || !state.viewers.contains_key(&client) {
                return false;
            }
        }
        self.input.clear();
        let end = response.bytes.len().min(self.written.saturating_add(16 * 1024));
        match self.socket.write(&response.bytes[self.written..end]) {
            Ok(0) => false,
            Ok(count) => {
                self.written += count;
                self.written < response.bytes.len()
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => true,
            Err(_) => false,
        }
    }
}

struct Request<'a> {
    method: &'a str,
    path: &'a str,
    host: &'a str,
    origin: Option<&'a str>,
    authorization: Option<&'a str>,
    content_type: Option<&'a str>,
    fetch_site: Option<&'a str>,
    body: &'a [u8],
}

fn parse(input: &[u8]) -> Result<Option<Request<'_>>, Response> {
    let invalid = || Response::error(400, "Invalid HTTP request.");
    let Some(header_end) = input.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
        return if input.len() > MAX_HEADERS {
            Err(Response::error(431, "Request headers are too large."))
        } else {
            Ok(None)
        };
    };
    if header_end + 4 > MAX_HEADERS {
        return Err(Response::error(431, "Request headers are too large."));
    }
    let headers = std::str::from_utf8(&input[..header_end]).map_err(|_| invalid())?;
    if !headers.is_ascii() {
        return Err(invalid());
    }
    let mut lines = headers.split("\r\n");
    let mut start = lines.next().ok_or_else(invalid)?.split(' ');
    let (Some(method), Some(path), Some(version), None) =
        (start.next(), start.next(), start.next(), start.next())
    else {
        return Err(invalid());
    };
    if version != "HTTP/1.1" || !path.starts_with('/') || path.contains(['#', '?']) {
        return Err(invalid());
    }
    let mut request = Request {
        method,
        path,
        host: "",
        origin: None,
        authorization: None,
        content_type: None,
        fetch_site: None,
        body: &[],
    };
    let mut content_length = None;
    for line in lines {
        let (name, value) = line.split_once(':').ok_or_else(invalid)?;
        if name.is_empty()
            || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value.bytes().any(|byte| byte < b' ' && byte != b'\t')
        {
            return Err(invalid());
        }
        let value = value.trim();
        let slot = match name.to_ascii_lowercase().as_str() {
            "host" => {
                if !request.host.is_empty() || value.is_empty() {
                    return Err(invalid());
                }
                request.host = value;
                continue;
            }
            "origin" => &mut request.origin,
            "authorization" => &mut request.authorization,
            "content-type" => &mut request.content_type,
            "sec-fetch-site" => &mut request.fetch_site,
            "content-length" => {
                if content_length.is_some() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(invalid());
                }
                content_length = Some(value.parse::<usize>().map_err(|_| invalid())?);
                continue;
            }
            "transfer-encoding" | "expect" => return Err(invalid()),
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return Err(invalid());
        }
    }
    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_BODY {
        return Err(Response::error(413, "Request body is too large."));
    }
    let expected = header_end + 4 + content_length;
    if input.len() < expected {
        return Ok(None);
    }
    if input.len() != expected || request.host.is_empty() {
        return Err(invalid());
    }
    request.body = &input[header_end + 4..];
    Ok(Some(request))
}

struct Response {
    bytes: Vec<u8>,
    client: Option<[u8; 32]>,
}

impl Response {
    fn new(status: u16, content_type: &str, body: &[u8]) -> Self {
        let reason = match status {
            200 => "OK",
            202 => "Accepted",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Payload Too Large",
            422 => "Unprocessable Content",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            503 => "Service Unavailable",
            _ => "Internal Server Error",
        };
        let mut bytes = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'none'; font-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\r\n",
            body.len(),
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        Self {
            bytes,
            client: None,
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Self::json(status, &serde_json::json!({"error": message}))
    }

    fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(status, "application/json", value.to_string().as_bytes())
    }

    fn for_client(mut self, client: [u8; 32]) -> Self {
        self.client = Some(client);
        self
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairRequest {
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRequest {
    #[serde(default)]
    after: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalRequest {
    generation: u64,
    text: String,
}

fn dispatch(handle: &CompanionHandle, request: Request<'_>) -> Response {
    let authority = handle.address.to_string();
    let origin = format!("http://{authority}");
    // An exact Host also prevents DNS rebinding into this loopback service.
    if request.host != authority
        || request.origin.is_some_and(|value| value != origin)
    {
        return Response::error(403, "This request does not belong to the local browser origin.");
    }
    if request.method == "GET" && request.body.is_empty() {
        return match request.path {
            "/" => Response::new(200, "text/html; charset=utf-8", HTML.as_bytes()),
            "/client.js" => Response::new(200, "text/javascript; charset=utf-8", JAVASCRIPT.as_bytes()),
            "/client.css" => Response::new(200, "text/css; charset=utf-8", CSS.as_bytes()),
            _ => Response::error(404, "Unknown browser companion route."),
        };
    }
    if request.method != "POST" {
        return Response::error(405, "Only application GET and POST requests are supported.");
    }
    if request.origin != Some(origin.as_str())
        || request.fetch_site.is_some_and(|site| site != "same-origin")
        || request.content_type.map(|value| value.split(';').next().unwrap_or("").trim())
            != Some("application/json")
    {
        return Response::error(403, "Browser API requests require the exact local origin.");
    }
    if request.path == "/api/pair" {
        return pair(handle, request.body);
    }
    let Some(credential) = request.authorization.and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Response::error(401, "Pair with a fresh link from the local terminal.");
    };
    if credential.len() != 64 || !credential.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Response::error(401, "The browser credential is invalid.");
    }
    let client = token_hash(credential);
    let now = Instant::now();
    let mut state = handle
        .shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.expire(now);
    if !state.active || !state.viewers.contains_key(&client) {
        return Response::error(401, "Browser access expired or was revoked. Pair again locally.");
    }
    if let Some(viewer) = state.viewers.get_mut(&client) {
        viewer.last_seen = now;
    }
    match request.path {
        "/api/snapshot" => {
            let Ok(query) = serde_json::from_slice::<SnapshotRequest>(request.body) else {
                return Response::error(400, "Invalid snapshot request.");
            };
            if query.after == state.seq {
                return Response::new(204, "application/json", &[]).for_client(client);
            }
            let viewer = &state.viewers[&client];
            let receipt = serde_json::to_string(&viewer.receipt).unwrap_or_else(|_| "null".into());
            let view_only = state.settings.view_only || viewer.kind == TokenKind::View;
            let snapshot = Arc::clone(&state.snapshot);
            let (seq, generation, busy, ready) = (
                state.seq,
                state.generation,
                state.busy,
                state.owner_session_id.is_some(),
            );
            drop(state);
            let body = format!(
                "{{\"seq\":{seq},\"generation\":{generation},\"busy\":{busy},\"ready\":{ready},\"view_only\":{view_only},\"proposal\":{receipt},\"snapshot\":{snapshot}}}"
            );
            Response::new(200, "application/json", body.as_bytes()).for_client(client)
        }
        "/api/proposal" => {
            let Ok(proposal) = serde_json::from_slice::<ProposalRequest>(request.body) else {
                return Response::error(400, "Invalid prompt proposal.");
            };
            if proposal.text.is_empty()
                || proposal.text.len() > MAX_PROMPT_BYTES
                || proposal.text.trim().is_empty()
                || proposal.text.trim_start().starts_with(['/', '!'])
                || proposal.text.chars().any(|character| {
                    character.is_control() && character != '\n' && character != '\t'
                })
            {
                return Response::error(
                    422,
                    "Propose ordinary text up to 8 KiB. Terminal commands and control characters stay local.",
                );
            }
            let viewer = &state.viewers[&client];
            if state.settings.view_only || viewer.kind == TokenKind::View {
                return Response::error(403, "This browser has view-only access.");
            }
            if now < viewer.next_proposal {
                return Response::error(429, "Wait briefly before proposing another prompt.");
            }
            let Some(owner_session_id) = state.owner_session_id.clone() else {
                return Response::error(409, "The local session is not ready.");
            };
            if state.busy || proposal.generation != state.generation || state.proposal.is_some() {
                return Response::error(409, "Wait for the local terminal and any pending proposal.");
            }
            let Ok(id) = random_token() else {
                return Response::error(503, "Proposal identity is temporarily unavailable.");
            };
            if let Some(viewer) = state.viewers.get_mut(&client) {
                viewer.next_proposal = now + PROPOSAL_INTERVAL;
                viewer.receipt = Some(ProposalReceipt {
                    id: id.clone(),
                    status: "pending",
                    reason: "Waiting for explicit acceptance in the local terminal.".to_string(),
                });
            }
            state.proposal = Some(ProposalSlot {
                proposal: PromptProposal {
                    id,
                    owner_session_id,
                    text: proposal.text,
                },
                client,
                delivered: false,
                deadline: now + PROPOSAL_LIFETIME,
            });
            state.changed();
            state.record("proposal_received");
            Response::json(202, &serde_json::json!({"status":"pending"})).for_client(client)
        }
        "/api/leave" => {
            if request.body != b"{}" {
                return Response::error(400, "Invalid disconnect request.");
            }
            state.viewers.remove(&client);
            if state.proposal.as_ref().is_some_and(|slot| slot.client == client) {
                state.reject_proposal("The browser disconnected.");
            }
            state.changed();
            state.record("viewer_disconnected");
            Response::json(200, &serde_json::json!({"status":"disconnected"}))
        }
        _ => Response::error(404, "Unknown browser companion route."),
    }
}

fn pair(handle: &CompanionHandle, body: &[u8]) -> Response {
    let Ok(request) = serde_json::from_slice::<PairRequest>(body) else {
        return Response::error(400, "Invalid pairing request.");
    };
    if request.token.len() != 64 || !request.token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Response::error(401, "Use a fresh pairing link from the local terminal.");
    }
    let token = token_hash(&request.token);
    let Ok(credential) = random_token() else {
        return Response::error(503, "Browser pairing is temporarily unavailable.");
    };
    let client = token_hash(&credential);
    let now = Instant::now();
    let mut state = handle
        .shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.expire(now);
    let Some(grant) = state.tokens.get(&token).filter(|_| state.active) else {
        return Response::error(401, "This pairing link expired, was used, or was revoked.");
    };
    let kind = grant.kind;
    // Capacity is checked before consumption; a rejected viewer can retry
    // its still-valid one-time link after a slot becomes available.
    if state.viewers.len() >= state.settings.max_viewers || state.viewers.contains_key(&client) {
        return Response::error(503, "Browser viewer capacity is full. Disconnect a viewer or revoke locally.");
    }
    state.tokens.remove(&token);
    state.viewers.insert(client, Viewer {
        kind,
        last_seen: now,
        next_proposal: now,
        receipt: None,
    });
    state.changed();
    state.record("viewer_paired");
    Response::json(200, &serde_json::json!({
        "credential": credential,
        "view_only": state.settings.view_only || kind == TokenKind::View,
    })).for_client(client)
}
