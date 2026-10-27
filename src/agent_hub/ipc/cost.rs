//! Session-tree spend accounting over the native child's owned local channel.
//!
//! Every child request consults the same gate as its parent. A completed
//! response is charged before another request may start; uncertain writes
//! are never replayed. The remote budget quarantines itself when an exchange
//! fails, so losing the parent cannot silently create a fresh allowance.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{ChildChannel, EXCHANGE_TIMEOUT, VERSION, ipc_error, matches_token};
use crate::agent_cx::AgentCx;
use crate::agent_hub::{ChildStatus, registry};
use crate::cost::{CostAttempt, CostBudget, CostRemote, CostRemoteAttempt, CostSnapshot};
use crate::error::{Error, Result};

const ADDRESS_ENV: &str = "PI_SUBAGENT_COST_ADDRESS";
const TOKEN_ENV: &str = "PI_SUBAGENT_COST_TOKEN";
const MAX_FRAME_BYTES: usize = 4096;
const CONNECT_LIMIT: Duration = Duration::from_millis(100);
const IDLE_POLL: Duration = Duration::from_millis(2);
const SETTLEMENT_LIMIT: Duration = Duration::from_millis(250);
const MAX_ACTIVE_ATTEMPTS: usize = 128;
const MAX_SETTLED_RECEIPTS: usize = 256;

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase", deny_unknown_fields)]
enum Operation {
    Snapshot,
    Record {
        amount: f64,
    },
    Begin {
        attempt: u64,
    },
    Observe {
        attempt: u64,
        sequence: u64,
        amount: f64,
    },
    Finish {
        attempt: u64,
        sequence: u64,
        amount: Option<f64>,
    },
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
    CostCap(crate::cost::CostCapRefusal),
    Error(String),
}

#[derive(Clone)]
struct RemoteBudget {
    address: SocketAddr,
    token: String,
    unavailable: Arc<AtomicBool>,
}

impl CostRemote for RemoteBudget {
    fn snapshot(&self) -> Result<CostSnapshot> {
        self.exchange(Operation::Snapshot)
    }

    fn record(&self, cost: f64) -> Result<CostSnapshot> {
        validate_amount(cost)?;
        self.exchange(Operation::Record { amount: cost })
    }

    fn start_attempt(&self) -> Result<(CostSnapshot, Box<dyn CostRemoteAttempt>)> {
        let attempt = crate::cost::next_attempt_id()?;
        let snapshot = self.exchange(Operation::Begin { attempt })?;
        Ok((
            snapshot,
            Box::new(RemoteAttempt {
                parent: self.clone(),
                attempt,
                sequence: 0,
                known_cost: 0.0,
                finished: false,
            }),
        ))
    }
}

/// Constructed only after authenticated, live Begin admission. Its one terminal
/// accounting frame is cleanup of that request, not new work under a fresh Cx.
struct RemoteAttempt {
    parent: RemoteBudget,
    attempt: u64,
    sequence: u64,
    known_cost: f64,
    finished: bool,
}

impl CostRemoteAttempt for RemoteAttempt {
    fn observe(&mut self, cost: f64) -> Result<CostSnapshot> {
        if self.finished {
            return Err(ipc_error("COST_SETTLED", "model cost attempt is settled"));
        }
        validate_amount(cost)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| ipc_error("COST_LIMIT", "cost attempt sequence exhausted"))?;
        self.known_cost = self.known_cost.max(cost);
        self.parent.exchange(Operation::Observe {
            attempt: self.attempt,
            sequence: self.sequence,
            amount: cost,
        })
    }

    fn finish(&mut self, cost: Option<f64>, deadline: Option<Instant>) -> Result<CostSnapshot> {
        if self.finished {
            return Err(ipc_error("COST_SETTLED", "model cost attempt is settled"));
        }
        self.finished = true;
        if let Some(cost) = cost {
            validate_amount(cost)?;
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| ipc_error("COST_LIMIT", "cost attempt sequence exhausted"))?;
        // This captured capability permits only one terminal charge for its
        // own attempt. A cancelled task cannot use it for snapshots, another
        // attempt, messaging, or a model call. No new AgentCx is manufactured.
        self.parent.exchange_settlement(
            Operation::Finish {
                attempt: self.attempt,
                sequence: self.sequence,
                amount: cost,
            },
            deadline,
        )
    }
}

impl Drop for RemoteAttempt {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish(Some(self.known_cost), None);
        }
    }
}

struct ActiveAttempt {
    charge: CostAttempt,
    sequence: u64,
    amount: f64,
}

struct SettledReceipt {
    attempt: u64,
    sequence: u64,
    amount: Option<f64>,
}

/// The channel, rather than the child's last packet, owns unfinalized usage.
/// Dropping a channel settles every acknowledged partial charge locally (or
/// through the already-owned ancestor attempt for a nested delegation).
#[derive(Default)]
pub(super) struct AttemptLedger {
    active: BTreeMap<u64, ActiveAttempt>,
    settled: VecDeque<SettledReceipt>,
    retired_through: u64,
}

impl AttemptLedger {
    pub(super) fn retire(&mut self) {
        let deadline = Instant::now() + SETTLEMENT_LIMIT;
        for active in self.active.values_mut() {
            if let Err(error) = active.charge.finish_known_before(deadline) {
                tracing::warn!(%error, "child cost attempt accounting could not settle");
            }
        }
        self.active.clear();
    }

    fn begin(&mut self, budget: &CostBudget, attempt: u64) -> Result<CostSnapshot> {
        if attempt == 0 {
            return Err(ipc_error("COST_INPUT", "invalid cost attempt identity"));
        }
        if self.active.contains_key(&attempt) {
            return Ok(budget.snapshot());
        }
        if attempt <= self.retired_through
            || self.settled.iter().any(|receipt| receipt.attempt == attempt)
        {
            return Err(ipc_error("COST_SETTLED", "cost attempt is already settled"));
        }
        if self.active.len() >= MAX_ACTIVE_ATTEMPTS {
            return Err(ipc_error("COST_LIMIT", "too many active child cost attempts"));
        }
        let charge = budget.start_attempt()?;
        self.active.insert(
            attempt,
            ActiveAttempt {
                charge,
                sequence: 0,
                amount: 0.0,
            },
        );
        Ok(budget.snapshot())
    }

    #[allow(clippy::float_cmp)] // Identical receipts must replay exactly, without charging again.
    fn observe(
        &mut self,
        budget: &CostBudget,
        attempt: u64,
        sequence: u64,
        amount: f64,
    ) -> Result<CostSnapshot> {
        validate_amount(amount)?;
        if sequence == 0 {
            return Err(ipc_error("COST_INPUT", "invalid cost attempt sequence"));
        }
        if let Some(receipt) = self.settled.iter().find(|receipt| receipt.attempt == attempt) {
            if sequence < receipt.sequence {
                return Ok(budget.snapshot());
            }
            return Err(ipc_error("COST_SETTLED", "cost attempt is already settled"));
        }
        let active = self
            .active
            .get_mut(&attempt)
            .ok_or_else(|| ipc_error("COST_UNKNOWN", "cost attempt is not owned by this child"))?;
        if sequence < active.sequence || (sequence == active.sequence && amount == active.amount) {
            return Ok(budget.snapshot());
        }
        if sequence == active.sequence || amount < active.amount {
            return Err(ipc_error("COST_PROTOCOL", "conflicting partial cost receipt"));
        }
        active.charge.observe(amount)?;
        active.sequence = sequence;
        active.amount = amount;
        Ok(budget.snapshot())
    }

    #[allow(clippy::float_cmp)] // Terminal receipt identity includes the exact reported bill.
    fn finish(
        &mut self,
        budget: &CostBudget,
        attempt: u64,
        sequence: u64,
        amount: Option<f64>,
    ) -> Result<CostSnapshot> {
        if let Some(amount) = amount {
            validate_amount(amount)?;
        }
        if let Some(receipt) = self.settled.iter().find(|receipt| receipt.attempt == attempt) {
            if sequence < receipt.sequence
                || (sequence == receipt.sequence && amount == receipt.amount)
            {
                return Ok(budget.snapshot());
            }
            return Err(ipc_error("COST_PROTOCOL", "conflicting final cost receipt"));
        }
        let active = self
            .active
            .get(&attempt)
            .ok_or_else(|| ipc_error("COST_UNKNOWN", "cost attempt is not owned by this child"))?;
        if sequence <= active.sequence {
            return Err(ipc_error("COST_PROTOCOL", "stale final cost receipt"));
        }
        let mut active = self
            .active
            .remove(&attempt)
            .ok_or_else(|| ipc_error("COST_UNKNOWN", "cost attempt is not owned by this child"))?;
        let result = active.charge.finish(amount.unwrap_or(f64::NAN));
        self.settled.push_back(SettledReceipt {
            attempt,
            sequence,
            amount,
        });
        if self.settled.len() > MAX_SETTLED_RECEIPTS
            && let Some(retired) = self.settled.pop_front()
        {
            // Never reopen an old identity after its exact receipt ages out.
            // Already-active requests remain owned even below this watermark;
            // a very old, never-admitted Begin must fail closed instead.
            self.retired_through = self.retired_through.max(retired.attempt);
        }
        result?;
        Ok(budget.snapshot())
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
    Ok(Some(RemoteBudget {
        address,
        token,
        unavailable: Arc::new(AtomicBool::new(false)),
    }))
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
    validate_amount(snapshot.pending)?;
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
    attempts: &mut AttemptLedger,
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
            Operation::Begin { attempt } => attempts.begin(budget, attempt),
            Operation::Observe {
                attempt,
                sequence,
                amount,
            } => attempts.observe(budget, attempt, sequence, amount),
            Operation::Finish {
                attempt,
                sequence,
                amount,
            } => attempts.finish(budget, attempt, sequence, amount),
        }
    })();
    match result {
        Ok(snapshot) => WireResponse::Ok(snapshot),
        Err(Error::CostCap { spent, cap }) => WireResponse::CostCap(crate::cost::CostCapRefusal {
            spent,
            cap,
            message: Error::CostCap { spent, cap }.to_string(),
        }),
        Err(error) => WireResponse::Error(error.to_string()),
    }
}

fn check_remaining(owner: &AgentCx, deadline: Instant) -> Result<Duration> {
    check_exchange_remaining(Some(owner), deadline)
}

fn check_exchange_remaining(owner: Option<&AgentCx>, deadline: Instant) -> Result<Duration> {
    if let Some(owner) = owner {
        owner
            .checkpoint()
            .map_err(|_| ipc_error("COST_CANCELLED", "parent cost exchange was cancelled"))?;
    }
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
        self.exchange_owned(Some(&owner), operation, deadline)
    }

    fn exchange_settlement(
        &self,
        operation: Operation,
        deadline: Option<Instant>,
    ) -> Result<CostSnapshot> {
        if !matches!(&operation, Operation::Finish { .. }) {
            return Err(ipc_error(
                "COST_PERMISSION",
                "cleanup permits only attempt settlement",
            ));
        }
        if self.unavailable.load(Ordering::Acquire) {
            return Err(ipc_error(
                "COST_UNAVAILABLE",
                "parent cost settlement was interrupted",
            ));
        }
        let limit = Instant::now() + SETTLEMENT_LIMIT;
        let result = self.exchange_owned(None, operation, deadline.map_or(limit, |end| end.min(limit)));
        if result.is_err() {
            // One failed bounded cleanup exchange is enough. Other attempts
            // retain their parent-owned partial charges until channel teardown.
            self.unavailable.store(true, Ordering::Release);
        }
        result
    }

    #[allow(clippy::too_many_lines)] // One bounded nonblocking exchange, with no retry after dispatch.
    fn exchange_owned(
        &self,
        owner: Option<&AgentCx>,
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
        let wait = check_exchange_remaining(owner, deadline)?.min(CONNECT_LIMIT);
        let mut socket = TcpStream::connect_timeout(&self.address, wait)
            .map_err(|_| ipc_error("COST_UNAVAILABLE", "parent cost channel is unavailable"))?;
        socket
            .set_nonblocking(true)
            .map_err(|_| ipc_error("COST_UNAVAILABLE", "cannot configure parent cost channel"))?;
        let mut written = 0;
        let mut response = Vec::new();
        loop {
            check_exchange_remaining(owner, deadline)?;
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
                        check_exchange_remaining(owner, deadline)?;
                        return match reply {
                            WireResponse::Ok(snapshot) => {
                                validate_snapshot(&snapshot)?;
                                Ok(snapshot)
                            }
                            WireResponse::CostCap(refusal) => Err(Error::CostCap {
                                spent: refusal.spent,
                                cap: refusal.cap,
                            }),
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
                std::thread::sleep(IDLE_POLL.min(check_exchange_remaining(owner, deadline)?));
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Quarter-dollar fixtures require exact replacement and replay invariants.
mod tests {
    use super::*;
    use futures::StreamExt as _;
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
            unavailable: Arc::new(AtomicBool::new(false)),
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

    struct PartialCostProvider;

    #[async_trait::async_trait]
    impl crate::provider::Provider for PartialCostProvider {
        fn name(&self) -> &str {
            "partial-cost-fixture"
        }

        fn api(&self) -> &str {
            "test"
        }

        fn model_id(&self) -> &str {
            "metered"
        }

        async fn stream(
            &self,
            _: &crate::provider::Context<'_>,
            _: &crate::provider::StreamOptions,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<crate::model::StreamEvent>> + Send>>,
        > {
            let mut partial = crate::model::AssistantMessage::default();
            partial.usage.cost.total = 0.75;
            Ok(Box::pin(
                futures::stream::iter([Ok(crate::model::StreamEvent::Start { partial })])
                    .chain(futures::stream::pending()),
            ))
        }
    }

    #[test]
    fn retiring_a_child_channel_preserves_streamed_partial_cost_before_child_drop() {
        let owner = AgentCx::for_testing_with_io();
        let _current = owner.cx().clone().set_current_restricted();
        let parent = CostBudget::new(None, Some(0.5), 0.0).unwrap();
        let (remote, server) = serve_budget(&parent);
        let child = CostBudget::remote(Arc::new(remote)).unwrap();
        let provider =
            crate::provider::guard_with_cost_budget(Arc::new(PartialCostProvider), child.clone());
        let mut stream = futures::executor::block_on(provider.stream(
            &crate::provider::Context::default(),
            &crate::provider::StreamOptions::default(),
        ))
        .unwrap();
        assert!(matches!(
            futures::executor::block_on(stream.next()).unwrap().unwrap(),
            crate::model::StreamEvent::Start { .. }
        ));
        assert_eq!(parent.spent(), 0.0);
        assert_eq!(parent.snapshot().pending, 0.75);
        assert!(
            matches!(parent.admit(), Err(Error::CostCap { .. })),
            "siblings must see known partial usage immediately"
        );

        // Retire the actual listener without any terminal packet or child-side
        // destructor. This is the ordering imposed by native process reaping.
        drop(server);
        assert_eq!(parent.spent(), 0.75);
        assert_eq!(parent.snapshot().pending, 0.0);
        owner.set_cancel_requested(true);
        drop(stream);
        assert!(
            child.start_attempt().is_err(),
            "cancelled work cannot borrow settlement authority"
        );
        assert_eq!(
            parent.spent(),
            0.75,
            "a late failed child settlement cannot refund or duplicate spend"
        );
    }

    #[test]
    fn cancelled_work_can_only_settle_its_previously_admitted_attempt() {
        let owner = AgentCx::for_testing_with_io();
        let _current = owner.cx().clone().set_current_restricted();
        let parent = CostBudget::new(None, Some(2.0), 0.0).unwrap();
        let (remote, _server) = serve_budget(&parent);
        let child = CostBudget::remote(Arc::new(remote)).unwrap();
        let mut attempt = child.start_attempt().unwrap();
        attempt.observe(1.0).unwrap();
        owner.set_cancel_requested(true);
        attempt.finish(0.25).unwrap();
        assert_eq!(
            parent.spent(),
            0.25,
            "final billing replaces the provisional estimate during cleanup"
        );
        assert_eq!(parent.snapshot().pending, 0.0);
        assert!(
            child.admit().is_err(),
            "cleanup cannot refresh ordinary admission under cancellation"
        );
        assert!(child.start_attempt().is_err());
        assert_eq!(parent.spent(), 0.25);
    }

    #[test]
    fn retiring_a_nested_child_settles_known_partial_usage_at_the_root() {
        let owner = AgentCx::for_testing_with_io();
        let _current = owner.cx().clone().set_current_restricted();
        let root = CostBudget::new(None, Some(0.5), 0.0).unwrap();
        let (middle_remote, _middle_server) = serve_budget(&root);
        let middle = CostBudget::remote(Arc::new(middle_remote)).unwrap();
        let (leaf_remote, leaf_server) = serve_budget(&middle);
        let leaf = CostBudget::remote(Arc::new(leaf_remote)).unwrap();
        let mut attempt = leaf.start_attempt().unwrap();
        attempt.observe(0.75).unwrap();
        assert_eq!(root.snapshot().pending, 0.75);
        drop(leaf_server);
        assert_eq!(root.spent(), 0.75);
        assert_eq!(root.snapshot().pending, 0.0);
        assert!(matches!(root.admit(), Err(Error::CostCap { .. })));
        drop(attempt);
        assert_eq!(root.spent(), 0.75);
    }

    #[test]
    fn socket_attempt_receipts_are_idempotent_and_cannot_reopen_settled_charges() {
        let parent = CostBudget::new(None, Some(10.0), 0.0).unwrap();
        let (remote, _server) = serve_budget(&parent);
        let send = |operation: serde_json::Value| {
            exchange_frame(
                remote.address,
                &json!({"version": VERSION, "token": remote.token, "cost": operation}),
            )
        };
        for _ in 0..2 {
            assert_eq!(
                send(json!({"operation":"begin", "attempt":7}))["status"],
                "ok"
            );
        }
        for _ in 0..2 {
            assert_eq!(
                send(json!({"operation":"observe", "attempt":7, "sequence":1, "amount":2.0}))
                    ["status"],
                "ok"
            );
        }
        send(json!({"operation":"observe", "attempt":7, "sequence":2, "amount":3.0}));
        send(json!({"operation":"observe", "attempt":7, "sequence":1, "amount":2.0}));
        assert_eq!(parent.snapshot().pending, 3.0);
        let finished = json!({"operation":"finish", "attempt":7, "sequence":3, "amount":0.75});
        for _ in 0..2 {
            assert_eq!(send(finished.clone())["status"], "ok");
        }
        send(json!({"operation":"observe", "attempt":7, "sequence":2, "amount":3.0}));
        assert_eq!(
            send(json!({"operation":"finish", "attempt":7, "sequence":3, "amount":8.0}))
                ["status"],
            "error"
        );
        assert_eq!(
            send(json!({"operation":"begin", "attempt":7}))["status"],
            "error"
        );
        assert_eq!(
            send(json!({"operation":"finish", "attempt":99, "sequence":1, "amount":4.0}))
                ["status"],
            "error"
        );
        assert_eq!(parent.spent(), 0.75);
        assert_eq!(parent.snapshot().pending, 0.0);
    }

    #[test]
    fn invalid_terminal_billing_quarantines_parent_without_losing_partial_usage() {
        let owner = AgentCx::for_testing_with_io();
        let _current = owner.cx().clone().set_current_restricted();
        let parent = CostBudget::new(None, Some(5.0), 0.0).unwrap();
        let (remote, _server) = serve_budget(&parent);
        let child = CostBudget::remote(Arc::new(remote)).unwrap();
        let mut attempt = child.start_attempt().unwrap();
        attempt.observe(0.5).unwrap();
        assert!(attempt.finish(f64::NAN).is_err());
        assert_eq!(parent.spent(), 0.5);
        assert_eq!(parent.snapshot().pending, 0.0);
        assert!(
            parent.admit().is_err(),
            "invalid child billing must quarantine the shared account"
        );
    }

    #[test]
    fn bounded_receipt_history_never_reopens_old_attempts_or_drops_active_charges() {
        let parent = CostBudget::new(None, None, 0.0).unwrap();
        let mut attempts = AttemptLedger::default();
        attempts.begin(&parent, 1).unwrap();
        attempts.observe(&parent, 1, 1, 0.5).unwrap();

        // Keep an early request live while later requests exhaust the exact
        // replay window. Eviction must not release or reopen either identity.
        let last = u64::try_from(MAX_SETTLED_RECEIPTS).unwrap() + 2;
        for attempt in 2..=last {
            attempts.begin(&parent, attempt).unwrap();
            attempts.finish(&parent, attempt, 1, Some(0.25)).unwrap();
        }
        assert_eq!(attempts.settled.len(), MAX_SETTLED_RECEIPTS);
        assert_eq!(attempts.retired_through, 2);
        assert!(attempts.begin(&parent, 2).is_err());
        assert!(attempts.finish(&parent, 2, 1, Some(0.25)).is_err());
        attempts.begin(&parent, 1).unwrap();
        attempts.finish(&parent, 1, 2, Some(0.25)).unwrap();
        assert_eq!(parent.spent(), 64.5);
        assert_eq!(parent.snapshot().pending, 0.0);

        for attempt in 1000..1000 + u64::try_from(MAX_ACTIVE_ATTEMPTS).unwrap() {
            attempts.begin(&parent, attempt).unwrap();
            attempts.observe(&parent, attempt, 1, 0.25).unwrap();
        }
        assert!(attempts.begin(&parent, 2000).is_err());
        attempts.retire();
        assert_eq!(parent.snapshot().pending, 0.0);
        assert_eq!(parent.spent(), 96.5);
    }

    struct DeadlineRecordingRemote {
        budget: CostBudget,
        deadlines: Arc<std::sync::Mutex<Vec<Option<Instant>>>>,
    }

    struct DeadlineRecordingAttempt {
        charge: CostAttempt,
        budget: CostBudget,
        deadlines: Arc<std::sync::Mutex<Vec<Option<Instant>>>>,
    }

    impl CostRemote for DeadlineRecordingRemote {
        fn snapshot(&self) -> Result<CostSnapshot> {
            Ok(self.budget.snapshot())
        }

        fn record(&self, cost: f64) -> Result<CostSnapshot> {
            self.budget.record(cost)?;
            Ok(self.budget.snapshot())
        }

        fn start_attempt(&self) -> Result<(CostSnapshot, Box<dyn CostRemoteAttempt>)> {
            let charge = self.budget.start_attempt()?;
            Ok((
                self.budget.snapshot(),
                Box::new(DeadlineRecordingAttempt {
                    charge,
                    budget: self.budget.clone(),
                    deadlines: Arc::clone(&self.deadlines),
                }),
            ))
        }
    }

    impl CostRemoteAttempt for DeadlineRecordingAttempt {
        fn observe(&mut self, cost: f64) -> Result<CostSnapshot> {
            self.charge.observe(cost)?;
            Ok(self.budget.snapshot())
        }

        fn finish(&mut self, cost: Option<f64>, deadline: Option<Instant>) -> Result<CostSnapshot> {
            self.deadlines.lock().unwrap().push(deadline);
            self.charge.finish(cost.unwrap_or(f64::NAN))?;
            Ok(self.budget.snapshot())
        }
    }

    #[test]
    fn channel_retirement_shares_one_deadline_across_all_ancestor_settlements() {
        let root = CostBudget::new(None, None, 0.0).unwrap();
        let deadlines = Arc::new(std::sync::Mutex::new(Vec::new()));
        let parent = CostBudget::remote(Arc::new(DeadlineRecordingRemote {
            budget: root.clone(),
            deadlines: Arc::clone(&deadlines),
        }))
        .unwrap();
        let mut attempts = AttemptLedger::default();
        for attempt in 1..=4 {
            attempts.begin(&parent, attempt).unwrap();
            attempts.observe(&parent, attempt, 1, 0.25).unwrap();
        }
        assert_eq!(root.snapshot().pending, 1.0);
        attempts.retire();
        let deadlines = deadlines.lock().unwrap();
        assert_eq!(deadlines.len(), 4);
        assert!(deadlines[0].is_some());
        assert!(deadlines.iter().all(|deadline| *deadline == deadlines[0]));
        assert_eq!(root.spent(), 1.0);
        assert_eq!(root.snapshot().pending, 0.0);
    }

    #[test]
    fn expired_cleanup_deadline_cannot_write_or_release_an_ancestor_charge() {
        let root = CostBudget::new(None, None, 0.0).unwrap();
        let mut ancestor_charge = root.start_attempt().unwrap();
        ancestor_charge.observe(0.5).unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut attempt = RemoteAttempt {
            parent: RemoteBudget {
                address: listener.local_addr().unwrap(),
                token: "a".repeat(64),
                unavailable: Arc::new(AtomicBool::new(false)),
            },
            attempt: 1,
            sequence: 1,
            known_cost: 0.5,
            finished: false,
        };
        let error = attempt.finish(Some(0.5), Some(Instant::now())).unwrap_err();
        assert!(error.to_string().contains("PI_HUB_IPC_COST_TIMEOUT"));
        drop(attempt);
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
            "expired cleanup must not connect or retry from Drop"
        );
        assert_eq!(root.snapshot().pending, 0.5);
        drop(ancestor_charge);
        assert_eq!(root.spent(), 0.5);
        assert_eq!(root.snapshot().pending, 0.0);
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
            unavailable: Arc::new(AtomicBool::new(false)),
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
                    _ => panic!("direct-charge fixture received an attempt operation"),
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
            pending: 0.0,
            revision: 0,
            warn_cost: Some(0.5),
            max_cost: Some(1.0),
            warned: true,
        };
        assert!(validate_snapshot(&snapshot).is_ok());
        snapshot.max_cost = Some(0.0);
        assert!(validate_snapshot(&snapshot).is_err());
    }
}
