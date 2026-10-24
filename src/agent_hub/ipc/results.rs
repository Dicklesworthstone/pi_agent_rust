//! Read-only result access through a native child's existing owned channel.
//!
//! Result envelopes cannot send messages or select a caller identity. The
//! parent reuses its ordinary result acceptance/projection/redaction path.
//! The synchronous client fits the existing synchronous URL-router contract;
//! it uses bounded nonblocking socket loops, not a nested async runtime.

use std::io::{self, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    EXCHANGE_TIMEOUT, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, VERSION, inherited_endpoint,
    ipc_error, matches_token, registry,
};
use crate::agent_cx::AgentCx;
use crate::agent_hub::{AgentHubRegistry, ChildStatus};
use crate::error::Result;
use crate::url_router::{ResolvedDoc, URL_ROUTER_SCHEMA};

mod stream;
pub(super) use stream::Transfer;

const MAX_REFERENCE_BYTES: usize = 4096;
const MAX_DOCUMENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_COMPLETE_BYTES: usize = 128 * 1024;
const IDLE_POLL: Duration = Duration::from_millis(2);
const CONNECT_LIMIT: Duration = Duration::from_millis(100);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    reference: String,
}

// Credentials intentionally have no Debug implementation and are never
// copied into result metadata, errors, argv or diagnostic events.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireRequest {
    version: u8,
    token: String,
    result: ReadRequest,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Document {
    reference: String,
    content: String,
    content_type: String,
    metadata: Value,
}

impl Document {
    fn into_resolved(self) -> ResolvedDoc {
        ResolvedDoc {
            schema: URL_ROUTER_SCHEMA.to_string(),
            scheme: "agent".to_string(),
            reference: self.reference,
            content: self.content,
            content_type: self.content_type,
            metadata: self.metadata,
            line_addressable: true,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "camelCase")]
pub(crate) enum WireResponse {
    Ok(Document),
    Page(stream::Page),
    Error(String),
}

fn reference_valid(reference: &str) -> Result<()> {
    if reference.len() > MAX_REFERENCE_BYTES {
        return Err(ipc_error("INPUT", "agent reference exceeds 4096 bytes"));
    }
    Ok(())
}

fn live_identity(hub: &AgentHubRegistry, sender: &str) -> Result<(PathBuf, u32)> {
    let entry = hub
        .get(sender)
        .filter(|entry| entry.status == ChildStatus::Running)
        .ok_or_else(|| ipc_error("REVOKED", "result reader no longer owns a live child"))?;
    let pid = hub
        .control_pid(sender)
        .ok_or_else(|| ipc_error("REVOKED", "result reader's process was reaped"))?;
    Ok((entry.transcript_path, pid))
}

fn current_identity(sender: &str) -> Result<(PathBuf, u32)> {
    let hub = registry().try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => ipc_error("BUSY", "parent agent registry is busy"),
        std::sync::TryLockError::Poisoned(_) => {
            ipc_error("UNAVAILABLE", "parent agent registry is unavailable")
        }
    })?;
    live_identity(&hub, sender)
}

fn check_owner(owner: &AgentCx) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| ipc_error("CANCELLED", "agent result read was cancelled"))
}

/// Authentication and revocation bracket local resolution. No hub mutex is
/// held while looking up the result cache, decoding or redacting the payload.
/// The resolver is explicitly local even when this host is itself a child.
pub(super) fn dispatch_authenticated(
    sender: &str,
    token: &str,
    request: WireRequest,
    owner: &AgentCx,
) -> Result<Transfer> {
    if request.version != VERSION || !matches_token(token, &request.token) {
        return Err(ipc_error(
            "AUTH",
            "invalid result-read credential or protocol version",
        ));
    }
    reference_valid(&request.result.reference)?;
    check_owner(owner)?;
    let identity = current_identity(sender)?;
    let resolved = crate::url_router::resolve_local_agent(&request.result.reference)?;
    check_owner(owner)?;
    if current_identity(sender)? != identity {
        return Err(ipc_error(
            "REVOKED",
            "result reader identity changed during resolution",
        ));
    }
    let mut metadata = resolved.metadata;
    if !metadata.is_object() {
        return Err(ipc_error(
            "PROTOCOL",
            "local result metadata is not an object",
        ));
    }
    metadata["scope"] = Value::String("parentChannel".to_string());
    let document = Document {
        reference: resolved.reference,
        content: resolved.content,
        content_type: resolved.content_type,
        metadata,
    };
    let transfer = Transfer::new(document, sender, identity)?;
    check_owner(owner)?;
    transfer.check_reader()?;
    Ok(transfer)
}

fn enforce_document_limit(document: &Document) -> Result<()> {
    stream::encoded_size(document, MAX_DOCUMENT_BYTES)
        .map(|_| ())
        .ok_or_else(|| {
            ipc_error(
                "RESULT_LIMIT",
                "agent result exceeds 2 MiB encoded; select a smaller JSON field",
            )
        })
}

/// Resolve from the configured immediate parent, never a local fallback.
/// Ordinary ReadTool line selection applies after a complete response arrives.
pub(crate) fn resolve_from_env(reference: &str) -> Result<ResolvedDoc> {
    reference_valid(reference)?;
    let (address, token) = inherited_endpoint()?
        .ok_or_else(|| ipc_error("CONFIG", "parent result channel is unavailable"))?;
    let owner = AgentCx::for_current_or_request();
    let deadline = read_deadline(&owner)?;
    resolve_at(&owner, address, token, reference, deadline)
}

fn read_deadline(owner: &AgentCx) -> Result<Instant> {
    check_owner(owner)?;
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.time {
        return Err(ipc_error(
            "PERMISSION",
            "result reads require I/O and timer authority",
        ));
    }
    let now = owner
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now());
    let remaining = owner
        .budget()
        .deadline
        .map_or(EXCHANGE_TIMEOUT, |deadline| {
            Duration::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
        })
        .min(EXCHANGE_TIMEOUT);
    if remaining.is_zero() {
        return Err(ipc_error(
            "TIMEOUT",
            "agent result read deadline has expired",
        ));
    }
    Ok(Instant::now() + remaining)
}

fn remaining(owner: &AgentCx, deadline: Instant) -> Result<Duration> {
    check_owner(owner)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ipc_error(
            "TIMEOUT",
            "agent result read deadline has expired",
        ));
    }
    Ok(remaining)
}

fn resolve_at(
    owner: &AgentCx,
    address: SocketAddr,
    token: String,
    reference: &str,
    deadline: Instant,
) -> Result<ResolvedDoc> {
    reference_valid(reference)?;
    if address.ip() != std::net::IpAddr::V4(Ipv4Addr::LOCALHOST) || address.port() == 0 {
        return Err(ipc_error("CONFIG", "result channel must be IPv4 localhost"));
    }
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err(ipc_error(
            "PERMISSION",
            "result reads require I/O and timer authority",
        ));
    }
    remaining(owner, deadline)?;
    let mut frame = serde_json::to_vec(&WireRequest {
        version: VERSION,
        token,
        result: ReadRequest {
            reference: reference.to_string(),
        },
    })?;
    frame.push(b'\n');
    if frame.len() > MAX_REQUEST_BYTES {
        return Err(ipc_error(
            "LIMIT",
            "result read request exceeds the frame limit",
        ));
    }
    let wait = remaining(owner, deadline)?.min(CONNECT_LIMIT);
    let mut socket = TcpStream::connect_timeout(&address, wait)
        .map_err(|_| ipc_error("UNAVAILABLE", "parent result channel is unavailable"))?;
    socket
        .set_nonblocking(true)
        .map_err(|_| ipc_error("UNAVAILABLE", "cannot configure parent result channel"))?;
    let mut written = 0;
    let mut response = Vec::new();
    let mut collector = stream::Collector::new(reference);
    loop {
        remaining(owner, deadline)?;
        let mut progressed = false;
        if written < frame.len() {
            match socket.write(&frame[written..]) {
                Ok(0) => {
                    return Err(ipc_error(
                        "WRITE",
                        "parent result channel closed during request",
                    ));
                }
                Ok(count) => {
                    written += count;
                    progressed = true;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(ipc_error("WRITE", "parent result request was interrupted")),
            }
        }
        let mut chunk = [0_u8; 4096];
        match socket.read(&mut chunk) {
            Ok(0) => {
                return Err(ipc_error(
                    "CLOSED",
                    "parent closed without a complete result",
                ));
            }
            Ok(count) => {
                progressed = true;
                if response.len().saturating_add(count) > MAX_RESPONSE_BYTES {
                    return Err(ipc_error(
                        "LIMIT",
                        "parent result response exceeds the frame limit",
                    ));
                }
                response.extend_from_slice(&chunk[..count]);
                while let Some(end) = response.iter().position(|byte| *byte == b'\n') {
                    remaining(owner, deadline)?;
                    if written != frame.len() {
                        return Err(ipc_error("PROTOCOL", "invalid result response framing"));
                    }
                    let message: WireResponse =
                        serde_json::from_slice(&response[..end]).map_err(|_| {
                            ipc_error("PROTOCOL", "parent returned an invalid result document")
                        })?;
                    response.drain(..=end);
                    if let Some(document) = collector.push(message)? {
                        if !response.is_empty() {
                            return Err(ipc_error(
                                "PROTOCOL",
                                "trailing data after the completed agent result",
                            ));
                        }
                        remaining(owner, deadline)?;
                        return Ok(document.into_resolved());
                    }
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Err(ipc_error("READ", "parent result response was interrupted")),
        }
        if !progressed {
            std::thread::sleep(IDLE_POLL.min(remaining(owner, deadline)?));
        }
    }
}

#[cfg(test)]
mod tests;
