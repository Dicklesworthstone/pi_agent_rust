//! Session-tree spend accounting over the native child's owned local channel.
//!
//! Every child request consults the same gate as its parent. A completed
//! response is charged before another request may start; uncertain writes
//! are never replayed. The remote budget quarantines itself when an exchange
//! fails, so losing the parent cannot silently create a fresh allowance.

use std::ffi::OsString;
use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{ChildChannel, EXCHANGE_TIMEOUT, VERSION, ipc_error, matches_token};
use crate::agent_cx::AgentCx;
use crate::agent_hub::{ChildStatus, registry};
use crate::cost::{CostBudget, CostRemote, CostSnapshot};
use crate::error::{Error, Result};

const ADDRESS_ENV: &str = "PI_SUBAGENT_COST_ADDRESS";
const TOKEN_ENV: &str = "PI_SUBAGENT_COST_TOKEN";
const MAX_FRAME_BYTES: usize = 4096;
const CONNECT_LIMIT: Duration = Duration::from_millis(100);
const IDLE_POLL: Duration = Duration::from_millis(2);

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase", deny_unknown_fields)]
enum Operation {
    Snapshot,
    Record { amount: f64 },
}

// Separate from messaging: no caller-selected identity, model, cap, path or
// parent total can be supplied by a child. Credentials never implement Debug.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireRequest {
    version: u8,
    token: String,
    cost: Operation,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "camelCase")]
pub(super) enum WireResponse {
    Ok(CostSnapshot),
    Error(String),
}

struct RemoteBudget {
    address: SocketAddr,
    token: String,
}

impl CostRemote for RemoteBudget {
    fn snapshot(&self) -> Result<CostSnapshot> {
        self.exchange(Operation::Snapshot)
    }

    fn record(&self, cost: f64) -> Result<CostSnapshot> {
        validate_amount(cost)?;
        self.exchange(Operation::Record { amount: cost })
    }
}

fn parse_endpoint(
    address: Option<OsString>,
    token: Option<OsString>,
) -> Result<Option<RemoteBudget>> {
    if address.is_none() && token.is_none() {
        return Ok(None);
    }
    let address = address
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse::<SocketAddr>().ok())
        .filter(|address| address.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) && address.port() != 0)
        .ok_or_else(|| ipc_error("COST_CONFIG", "invalid local parent cost address"))?;
    let token = token
        .and_then(|value| value.into_string().ok())
        .filter(|token| token.len() == 64 && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| ipc_error("COST_CONFIG", "missing or invalid parent cost credential"))?;
    Ok(Some(RemoteBudget { address, token }))
}

/// Join the parent's live budget, if this is a native delegated process.
///
/// Callers must prefer this over creating a new local budget from settings.
/// Any inherited marker requires a complete authenticated exchange; invalid
/// or stale configuration is an error, never an unlimited local fallback.
pub fn inherited_budget() -> Result<Option<CostBudget>> {
    parse_endpoint(std::env::var_os(ADDRESS_ENV), std::env::var_os(TOKEN_ENV))?
        .map(|remote| CostBudget::remote(Arc::new(remote)))
        .transpose()
}

pub(super) fn configure_child(command: &mut std::process::Command, channel: Option<&ChildChannel>) {
    command.env_remove(ADDRESS_ENV).env_remove(TOKEN_ENV);
    if let Some(channel) = channel.filter(|channel| channel.cost_budget.is_some()) {
        command
            .env(ADDRESS_ENV, channel.address.to_string())
            .env(TOKEN_ENV, &channel.token);
    }
}

fn validate_amount(amount: f64) -> Result<()> {
    if !amount.is_finite() || amount < 0.0 {
        return Err(ipc_error(
            "COST_INPUT",
            "recorded provider cost must be a finite nonnegative amount",
        ));
    }
    Ok(())
}

fn validate_snapshot(snapshot: &CostSnapshot) -> Result<()> {
    validate_amount(snapshot.spent)?;
    if [snapshot.warn_cost, snapshot.max_cost]
        .into_iter()
        .flatten()
        .any(|value| !value.is_finite() || value <= 0.0)
    {
        return Err(ipc_error(
            "COST_PROTOCOL",
            "parent returned invalid cost thresholds",
        ));
    }
    Ok(())
}

fn live_child(sender: &str, owner: &AgentCx) -> Result<()> {
    // Contention from an unrelated transcript update must not quarantine a
    // healthy child's account. Wait only before any mutation, with a bound
    // and cancellation checks; never hold the registry while forwarding.
    let deadline = exchange_deadline(owner)?.min(Instant::now() + CONNECT_LIMIT);
    loop {
        check_remaining(owner, deadline)?;
        let hub = match registry().try_lock() {
            Ok(hub) => hub,
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(IDLE_POLL.min(check_remaining(owner, deadline)?));
                continue;
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(ipc_error(
                    "UNAVAILABLE",
                    "parent agent registry is unavailable",
                ));
            }
        };
        if !hub
            .get(sender)
            .is_some_and(|entry| entry.status == ChildStatus::Running)
            || hub.control_pid(sender).is_none()
        {
            return Err(ipc_error(
                "REVOKED",
                "cost caller no longer owns a live child",
            ));
        }
        return Ok(());
    }
}

pub(super) fn dispatch_authenticated(
    sender: &str,
    token: &str,
    request: WireRequest,
    owner: &AgentCx,
    budget: Option<&CostBudget>,
) -> WireResponse {
    let result = (|| {
        if request.version != VERSION || !matches_token(token, &request.token) {
            return Err(ipc_error(
                "AUTH",
                "invalid cost credential or protocol version",
            ));
        }
        owner
            .checkpoint()
            .map_err(|_| ipc_error("CANCELLED", "parent cost owner is cancelled"))?;
        let budget = budget
            .ok_or_else(|| ipc_error("PERMISSION", "this channel has no parent cost authority"))?;
        live_child(sender, owner)?;
        // No registry lock is held across accounting. A parent that is itself
        // a native child forwards through its own captured budget, so nested
        // delegations share the root allowance instead of independent caps.
        match request.cost {
            Operation::Snapshot => budget.refresh(),
            Operation::Record { amount } => {
                validate_amount(amount)?;
                budget.record(amount)?;
                Ok(budget.snapshot())
            }
        }
    })();
    match result {
        Ok(snapshot) => WireResponse::Ok(snapshot),
        Err(error) => WireResponse::Error(error.to_string()),
    }
}

fn check_remaining(owner: &AgentCx, deadline: Instant) -> Result<Duration> {
    owner
        .checkpoint()
        .map_err(|_| ipc_error("COST_CANCELLED", "parent cost exchange was cancelled"))?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ipc_error(
            "COST_TIMEOUT",
            "parent cost exchange expired; a submitted charge may already be recorded",
        ));
    }
    Ok(remaining)
}

fn exchange_deadline(owner: &AgentCx) -> Result<Instant> {
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.time {
        return Err(ipc_error(
            "COST_PERMISSION",
            "cost exchange requires I/O and timer authority",
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
    let deadline = Instant::now() + remaining;
    check_remaining(owner, deadline)?;
    Ok(deadline)
}

impl RemoteBudget {
    fn exchange(&self, operation: Operation) -> Result<CostSnapshot> {
        let owner = AgentCx::for_current_or_request();
        let deadline = exchange_deadline(&owner)?;
        self.exchange_owned(&owner, operation, deadline)
    }

    #[allow(clippy::too_many_lines)] // One bounded nonblocking exchange, with no retry after dispatch.
    fn exchange_owned(
        &self,
        owner: &AgentCx,
        operation: Operation,
        deadline: Instant,
    ) -> Result<CostSnapshot> {
        let mut frame = serde_json::to_vec(&WireRequest {
            version: VERSION,
            token: self.token.clone(),
            cost: operation,
        })?;
        frame.push(b'\n');
        if frame.len() > MAX_FRAME_BYTES {
            return Err(ipc_error(
                "COST_LIMIT",
                "cost request exceeds its frame limit",
            ));
        }
        let wait = check_remaining(owner, deadline)?.min(CONNECT_LIMIT);
        let mut socket = TcpStream::connect_timeout(&self.address, wait)
            .map_err(|_| ipc_error("COST_UNAVAILABLE", "parent cost channel is unavailable"))?;
        socket
            .set_nonblocking(true)
            .map_err(|_| ipc_error("COST_UNAVAILABLE", "cannot configure parent cost channel"))?;
        let mut written = 0;
        let mut response = Vec::new();
        loop {
            check_remaining(owner, deadline)?;
            let mut progressed = false;
            if written < frame.len() {
                match socket.write(&frame[written..]) {
                    Ok(0) => {
                        return Err(ipc_error("COST_WRITE", "parent closed during cost request"));
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
                    Err(_) => {
                        return Err(ipc_error(
                            "COST_WRITE",
                            "parent cost request was interrupted",
                        ));
                    }
                }
            }
            let mut chunk = [0_u8; 1024];
            match socket.read(&mut chunk) {
                Ok(0) => {
                    return Err(ipc_error(
                        "COST_CLOSED",
                        "parent closed without a complete cost acknowledgment",
                    ));
                }
                Ok(count) => {
                    progressed = true;
                    if response.len().saturating_add(count) > MAX_FRAME_BYTES {
                        return Err(ipc_error(
                            "COST_LIMIT",
                            "parent cost response exceeds its frame limit",
                        ));
                    }
                    response.extend_from_slice(&chunk[..count]);
                    if let Some(end) = response.iter().position(|byte| *byte == b'\n') {
                        if written != frame.len() || end + 1 != response.len() {
                            return Err(ipc_error(
                                "COST_PROTOCOL",
                                "invalid cost response framing",
                            ));
                        }
                        let reply: WireResponse = serde_json::from_slice(&response[..end])
                            .map_err(|_| {
                                ipc_error("COST_PROTOCOL", "invalid parent cost acknowledgment")
                            })?;
                        check_remaining(owner, deadline)?;
                        return match reply {
                            WireResponse::Ok(snapshot) => {
                                validate_snapshot(&snapshot)?;
                                Ok(snapshot)
                            }
                            WireResponse::Error(error) => Err(Error::tool("cost", error)),
                        };
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => {
                    return Err(ipc_error(
                        "COST_READ",
                        "parent cost acknowledgment was interrupted",
                    ));
                }
            }
            if !progressed {
                std::thread::sleep(IDLE_POLL.min(check_remaining(owner, deadline)?));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct ChannelServer {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
        child_id: String,
    }

    impl Drop for ChannelServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            registry()
                .lock()
                .unwrap()
                .settle(&self.child_id, ChildStatus::Done);
        }
    }

    /// The actual parent listener, polled by one bounded test owner. Tests
    /// drive the production synchronous client on the calling thread.
    fn serve_budget(budget: &CostBudget) -> (RemoteBudget, ChannelServer) {
        let owner = AgentCx::for_testing_with_io();
        let child_id = {
            let mut hub = registry().lock().unwrap();
            let entry = hub
                .register("cost-socket-fixture", "share parent spend")
                .unwrap();
            // A registry liveness marker only: never publish the test runner's
            // real PID as a controllable delegated process.
            hub.mark_running(&entry.id, 2_147_483_647);
            entry.id
        };
        let mut channel = ChildChannel::bind(&child_id, &owner, Duration::from_secs(5))
            .unwrap()
            .with_cost_budget(Some(budget.clone()), false);
        let remote = RemoteBudget {
            address: channel.address,
            token: channel.token.clone(),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !thread_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                channel.poll();
                std::thread::sleep(IDLE_POLL);
            }
        });
        (
            remote,
            ChannelServer {
                stop,
                thread: Some(thread),
                child_id,
            },
        )
    }

    fn exchange_frame(address: SocketAddr, request: &serde_json::Value) -> serde_json::Value {
        let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut frame = serde_json::to_vec(request).unwrap();
        frame.push(b'\n');
        socket.write_all(&frame).unwrap();
        let mut reply = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let count = socket.read(&mut chunk).unwrap();
            assert!(count > 0, "parent closed before its acknowledgment");
            reply.extend_from_slice(&chunk[..count]);
            assert!(reply.len() <= MAX_FRAME_BYTES);
            if reply.ends_with(b"\n") {
                return serde_json::from_slice(&reply).unwrap();
            }
        }
    }

    #[test]
    fn child_cost_environment_is_separate_from_messaging_and_cleared_without_a_gate() {
        let owner = AgentCx::for_testing_with_io();
        let budget = CostBudget::new(None, Some(1.0), 0.0).unwrap();
        let channel = ChildChannel::bind("cost-only", &owner, Duration::from_secs(5))
            .unwrap()
            .with_cost_budget(Some(budget), false);
        let mut command = std::process::Command::new("pi");
        for key in [
            ADDRESS_ENV,
            TOKEN_ENV,
            super::super::ADDRESS_ENV,
            super::super::TOKEN_ENV,
        ] {
            command.env(key, "inherited-marker");
        }
        ChildChannel::configure_child(&mut command, Some(&channel));
        for key in [ADDRESS_ENV, TOKEN_ENV] {
            let (_, value) = command.get_envs().find(|(name, _)| *name == key).unwrap();
            assert!(value.is_some_and(|value| value != "inherited-marker"));
        }
        for key in [super::super::ADDRESS_ENV, super::super::TOKEN_ENV] {
            let (_, value) = command.get_envs().find(|(name, _)| *name == key).unwrap();
            assert!(value.is_none(), "a cost gate does not grant hub access");
        }
        ChildChannel::configure_child(&mut command, None);
        for key in [ADDRESS_ENV, TOKEN_ENV] {
            let (_, value) = command.get_envs().find(|(name, _)| *name == key).unwrap();
            assert!(
                value.is_none(),
                "a child cannot borrow its parent's credential"
            );
        }
    }

    #[test]
    fn socket_children_share_parent_and_sibling_cost_before_the_next_request() {
        let owner = AgentCx::for_testing_with_io();
        let _current = owner.cx().clone().set_current_restricted();
        let parent = CostBudget::new(Some(0.5), Some(1.0), 0.125).unwrap();
        let (first, _first_server) = serve_budget(&parent);
        let (second, _second_server) = serve_budget(&parent);
        let first = CostBudget::remote(Arc::new(first)).unwrap();
        let second = CostBudget::remote(Arc::new(second)).unwrap();
        first.admit().unwrap();
        second.admit().unwrap();
        first.record(0.5).unwrap();
        assert!((second.refresh().unwrap().spent - 0.625).abs() < f64::EPSILON);
        second.record(0.5).unwrap();
        assert!((parent.spent() - 1.125).abs() < f64::EPSILON);
        for budget in [&parent, &first, &second] {
            assert!(matches!(budget.admit(), Err(Error::CostCap { spent, cap })
                if (spent - 1.125).abs() < f64::EPSILON && (cap - 1.0).abs() < f64::EPSILON));
        }
        assert!(parent.take_warning().is_some());
        assert!(parent.take_warning().is_none());
    }

    #[test]
    fn cost_only_socket_rejects_wrong_credentials_messaging_and_reaped_children() {
        let parent = CostBudget::new(None, Some(1.0), 0.0).unwrap();
        let (remote, server) = serve_budget(&parent);
        let mut wrong_token = remote.token.clone().into_bytes();
        wrong_token[0] = if wrong_token[0] == b'a' { b'b' } else { b'a' };
        let response = exchange_frame(
            remote.address,
            &json!({
                "version":VERSION, "token":String::from_utf8(wrong_token).unwrap(),
                "cost":{"operation":"record", "amount":0.5}
            }),
        );
        assert!(
            response["payload"]
                .as_str()
                .unwrap()
                .contains("PI_HUB_IPC_AUTH")
        );
        let response = exchange_frame(
            remote.address,
            &json!({
                "version":VERSION, "token":remote.token,
                "request":{"action":"roster"}
            }),
        );
        assert!(
            response["payload"]
                .as_str()
                .unwrap()
                .contains("PI_HUB_IPC_PERMISSION")
        );
        registry()
            .lock()
            .unwrap()
            .mark_process_reaped(&server.child_id);
        let response = exchange_frame(
            remote.address,
            &json!({
                "version":VERSION, "token":remote.token,
                "cost":{"operation":"record", "amount":0.5}
            }),
        );
        assert!(
            response["payload"]
                .as_str()
                .unwrap()
                .contains("PI_HUB_IPC_REVOKED")
        );
        assert!(parent.spent().abs() < f64::EPSILON);
    }

    #[test]
    fn a_lost_charge_acknowledgment_quarantines_the_child_without_replaying_cost() {
        let owner = AgentCx::for_testing_with_io();
        let _current = owner.cx().clone().set_current_restricted();
        let parent = CostBudget::new(None, Some(1.0), 0.0).unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let remote = RemoteBudget {
            address: listener.local_addr().unwrap(),
            token: "a".repeat(64),
        };
        let recorded = parent.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            for expected_record in [false, true] {
                let mut socket = loop {
                    match listener.accept() {
                        Ok((socket, _)) => break socket,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "cost request did not arrive");
                            std::thread::sleep(IDLE_POLL);
                        }
                        Err(error) => panic!("cost fixture accept: {error}"),
                    }
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut input = Vec::new();
                while !input.ends_with(b"\n") {
                    let mut chunk = [0_u8; 1024];
                    let count = socket.read(&mut chunk).unwrap();
                    assert!(count > 0);
                    input.extend_from_slice(&chunk[..count]);
                    assert!(input.len() <= MAX_FRAME_BYTES);
                }
                let request: WireRequest = serde_json::from_slice(&input).unwrap();
                match request.cost {
                    Operation::Snapshot => {
                        assert!(!expected_record);
                        let mut reply =
                            serde_json::to_vec(&WireResponse::Ok(recorded.snapshot())).unwrap();
                        reply.push(b'\n');
                        for fragment in reply.chunks(7) {
                            socket.write_all(fragment).unwrap();
                        }
                    }
                    Operation::Record { amount } => {
                        assert!(expected_record);
                        recorded.record(amount).unwrap();
                        // The provider response was charged, but the connection
                        // closes before an ACK: replay would double the spend.
                    }
                }
            }
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline {
                assert!(
                    matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
                    "quarantined accounting unexpectedly retried a request"
                );
                std::thread::sleep(IDLE_POLL);
            }
        });
        let child = CostBudget::remote(Arc::new(remote)).unwrap();
        let error = child.record(0.5).unwrap_err().to_string();
        assert!(error.contains("PI_HUB_IPC_COST_CLOSED"), "{error}");
        let error = child.admit().unwrap_err().to_string();
        assert!(error.contains("accounting unavailable"), "{error}");
        assert!(child.record(0.5).is_err());
        thread.join().unwrap();
        assert!((parent.spent() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_and_invalid_parent_markers_never_create_an_unlimited_budget() {
        assert!(parse_endpoint(None, None).unwrap().is_none());
        let token = Some(OsString::from("a".repeat(64)));
        for address in [
            None,
            Some("127.0.0.1:0"),
            Some("192.0.2.1:4000"),
            Some("localhost:4000"),
        ] {
            assert!(parse_endpoint(address.map(OsString::from), token.clone()).is_err());
        }
        let address = Some(OsString::from("127.0.0.1:4000"));
        for token in [None, Some("short"), Some("not-a-credential")] {
            assert!(parse_endpoint(address.clone(), token.map(OsString::from)).is_err());
        }
        assert!(parse_endpoint(address, token).unwrap().is_some());
    }

    #[test]
    fn cost_envelopes_cannot_change_caps_or_borrow_a_sender() {
        let valid =
            json!({"version":1,"token":"a".repeat(64),"cost":{"operation":"record","amount":0.4}});
        assert!(serde_json::from_value::<super::super::Incoming>(valid.clone()).is_ok());
        for (field, value) in [
            ("sender", json!("parent")),
            ("maxCost", json!(10000)),
            ("request", json!({"action":"roster"})),
        ] {
            let mut request = valid.clone();
            request[field] = value;
            assert!(serde_json::from_value::<super::super::Incoming>(request).is_err());
        }
        let mut request = valid;
        request["cost"]["spent"] = json!(0);
        assert!(serde_json::from_value::<super::super::Incoming>(request).is_err());
    }

    #[test]
    fn malformed_charges_and_snapshots_are_rejected() {
        for amount in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(validate_amount(amount).is_err());
        }
        assert!(validate_amount(0.0).is_ok());
        let mut snapshot = CostSnapshot {
            spent: 0.5,
            warn_cost: Some(0.5),
            max_cost: Some(1.0),
            warned: true,
        };
        assert!(validate_snapshot(&snapshot).is_ok());
        snapshot.max_cost = Some(0.0);
        assert!(validate_snapshot(&snapshot).is_err());
    }
}
