//! Bounded reply waits over the same inbox and authenticated child channel as
//! ordinary hub messages. Waiting never sends, consumes, or replays a message.

use std::collections::BTreeSet;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{AgentHubRegistry, BusMessage, ChildStatus, ipc};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};

const MAX_WAIT_MS: u64 = 60_000;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_TRACKED_PEERS: usize = 256;
const MAX_TRACKED_PEER_BYTES: usize = 32 * 1024;
const MAX_WAIT_HOLDS: usize = 1024;

struct ReplyWatch {
    cursor: u64,
    all_senders: bool,
}

/// Only the waiting operation owns a strong reference. Dropping a wait never
/// needs the registry mutex, even when a process is reaped under that mutex.
pub struct DependencyHold {
    waiter: String,
    peers: Vec<String>,
    deadline: Instant,
    reply: Option<ReplyWatch>,
    /// A nonblocking clone observes EOF before the channel's next poll.
    /// The client socket's lifetime is the authenticated release token.
    channel: Option<TcpStream>,
}

impl DependencyHold {
    pub(super) fn active(&self) -> bool {
        if Instant::now() >= self.deadline {
            return false;
        }
        self.channel.as_ref().is_none_or(|socket| {
            let mut byte = [0_u8; 1];
            matches!(socket.peek(&mut byte), Err(error) if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InboxWaitStatus {
    Pending,
    Messages,
    PeerStopped,
    PeerBlocked,
    HistoryLost,
    TimedOut,
    NothingPending,
}

/// An unanswered successful send. Any later message from the peer answers it;
/// message bodies are never interpreted to guess whether they are questions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingReply {
    pub peer: String,
    pub sent_cursor: u64,
    pub peer_status: Option<ChildStatus>,
    pub can_reply: bool,
    /// A live peer depends directly or transitively on this caller.
    #[serde(default)]
    pub blocked_on_caller: bool,
}

/// A cursor belongs to this recipient/peer selection. An inbox read remains
/// independent: returning messages here does not consume their steering queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxWaitResult {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    pub status: InboxWaitStatus,
    pub messages: Vec<BusMessage>,
    pub next_cursor: u64,
    pub has_more: bool,
    pub oldest_cursor: u64,
    /// Nonzero means inbox eviction passed the requested cursor. A reply may
    /// have been lost; a timeout must never hide this fact.
    pub dropped_through_cursor: u64,
    pub peer_status: Option<ChildStatus>,
    /// Present for automatic waits. Settled peers are included so an empty
    /// reply cannot be mistaken for work that is still running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_replies: Vec<PendingReply>,
    /// Tracking is bounded separately from inbox retention. A lost pending
    /// send must not become an invented claim that nobody owes a reply.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dropped_tracking_through_cursor: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_peers: Vec<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes `&T`.
const fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn wait_error(code: &str, message: &str) -> Error {
    Error::tool("hub", format!("PI_HUB_WAIT_{code}: {message}"))
}

fn has_code(error: &Error, code: &str) -> bool {
    matches!(error, Error::Tool { tool, message } if tool == "hub"
        && message.strip_prefix("Tool error: hub: ").unwrap_or(message).starts_with(code))
}

fn expired_probe(
    error: Error,
    remaining: Duration,
    last_pending: Option<InboxWaitResult>,
) -> Result<InboxWaitResult> {
    if remaining.is_zero()
        && has_code(&error, "PI_HUB_IPC_TIMEOUT:")
        && let Some(mut result) = last_pending
    {
        result.status = InboxWaitStatus::TimedOut;
        return Ok(result);
    }
    Err(error)
}

impl AgentHubRegistry {
    /// A foreground native call awaits the child's result, including accepted
    /// writeback. Background jobs must not install this execution dependency.
    pub(crate) fn hold_child_result(
        &mut self,
        child: &str,
        remaining: Duration,
    ) -> Result<Arc<DependencyHold>> {
        if self
            .entries
            .get(child)
            .is_none_or(|entry| entry.status.settled())
        {
            return Err(wait_error(
                "PEER",
                "the awaited native child is unavailable",
            ));
        }
        self.prune_wait_holds()?;
        let deadline = Instant::now()
            .checked_add(remaining)
            .ok_or_else(|| wait_error("INPUT", "child result deadline is out of range"))?;
        let hold = Arc::new(DependencyHold {
            waiter: "parent".to_string(),
            peers: vec![child.to_string()],
            deadline,
            reply: None,
            channel: None,
        });
        self.wait_holds.push(Arc::downgrade(&hold));
        Ok(hold)
    }

    fn prune_wait_holds(&mut self) -> Result<()> {
        self.wait_holds
            .retain(|hold| hold.upgrade().is_some_and(|hold| hold.active()));
        if self.wait_holds.len() >= MAX_WAIT_HOLDS {
            return Err(wait_error("LIMIT", "too many active dependency waits"));
        }
        Ok(())
    }

    pub(super) fn begin_reply_wait(
        &mut self,
        recipient: &str,
        peer: Option<&str>,
        cursor: u64,
        remaining: Duration,
        channel: Option<TcpStream>,
    ) -> Result<Arc<DependencyHold>> {
        // Validate identities and readiness before publishing a dependency.
        let snapshot = self.wait_snapshot(recipient, peer, cursor)?;
        self.prune_wait_holds()?;
        let peers = if snapshot.status != InboxWaitStatus::Pending {
            Vec::new()
        } else if let Some(peer) = peer {
            vec![peer.to_string()]
        } else {
            snapshot
                .pending_replies
                .into_iter()
                .filter(|reply| reply.can_reply)
                .map(|reply| reply.peer)
                .collect()
        };
        let hold = Arc::new(DependencyHold {
            waiter: recipient.to_string(),
            peers,
            deadline: Instant::now() + remaining.min(Duration::from_millis(MAX_WAIT_MS)),
            reply: Some(ReplyWatch {
                cursor,
                all_senders: peer.is_none(),
            }),
            channel,
        });
        self.wait_holds.push(Arc::downgrade(&hold));
        Ok(hold)
    }

    fn peer_can_reply(&self, peer: &str) -> bool {
        let status = self.entries.get(peer).map(|entry| entry.status);
        peer == "parent"
            || status == Some(ChildStatus::Starting)
            || (status == Some(ChildStatus::Running) && self.control_pid(peer).is_some())
    }

    /// Reply holds wake for messages, history loss or peer completion. A
    /// delivered reply must release its dependency before the wait polls again.
    fn hold_peers(&self, hold: &DependencyHold) -> Vec<String> {
        // A lease opened after a terminal snapshot remains inert, even if a
        // concurrent send later adds a new pending recipient.
        if hold.peers.is_empty() {
            return Vec::new();
        }
        let Some(reply) = &hold.reply else {
            return hold.peers.clone();
        };
        let ready = self.bus.get(&hold.waiter).is_some_and(|messages| {
            messages.iter().any(|message| {
                message.seq > reply.cursor
                    && (reply.all_senders || hold.peers.contains(&message.from))
            })
        });
        let history_lost = self
            .dropped_bus_through
            .get(&hold.waiter)
            .is_some_and(|cursor| *cursor > reply.cursor)
            || (reply.all_senders
                && self
                    .dropped_reply_tracking_through
                    .get(&hold.waiter)
                    .is_some_and(|cursor| *cursor > reply.cursor));
        if ready || history_lost {
            return Vec::new();
        }
        if reply.all_senders {
            // Other concurrent tools may add unanswered sends while this
            // wait is open. Every current live alternative can wake it.
            return self
                .pending_replies
                .get(&hold.waiter)
                .into_iter()
                .flat_map(|pending| pending.keys())
                .filter(|peer| self.peer_can_reply(peer))
                .cloned()
                .collect();
        }
        hold.peers
            .iter()
            .filter(|peer| self.peer_can_reply(peer))
            .cloned()
            .collect()
    }

    /// Every alternative of a reply hold must depend on the caller. Separate
    /// holds (including foreground children) are conjunctive obligations:
    /// any one blocked hold prevents that agent's completion. Fixed-point
    /// propagation handles transitive dependencies without recursive searches.
    fn blocked_on(&self, caller: &str) -> BTreeSet<String> {
        let holds = self
            .wait_holds
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .filter(|hold| {
                hold.active()
                    && (hold.waiter == "parent" || self.control_pid(&hold.waiter).is_some())
            })
            .map(|hold| (hold.waiter.clone(), self.hold_peers(&hold)))
            .collect::<Vec<_>>();
        let mut blocked = BTreeSet::from([caller.to_string()]);
        loop {
            let prior = blocked.len();
            for (waiter, peers) in &holds {
                if !peers.is_empty() && peers.iter().all(|peer| blocked.contains(peer)) {
                    blocked.insert(waiter.clone());
                }
            }
            if blocked.len() == prior {
                return blocked;
            }
        }
    }

    pub(super) fn record_reply_delivery(&mut self, message: &BusMessage) {
        let known = |id: &str| id == "parent" || self.entries.contains_key(id);
        if message.from == message.to || !known(&message.from) || !known(&message.to) {
            return;
        }
        if let Some(pending) = self.pending_replies.get_mut(&message.to) {
            pending.remove(&message.from);
        }
        let pending = self
            .pending_replies
            .entry(message.from.clone())
            .or_default();
        pending.insert(message.to.clone(), message.seq);
        let mut bytes = pending.keys().map(String::len).sum::<usize>();
        while pending.len() > MAX_TRACKED_PEERS || bytes > MAX_TRACKED_PEER_BYTES {
            let oldest = pending
                .iter()
                .min_by_key(|(_, seq)| **seq)
                .map(|(peer, _)| peer.clone());
            if let Some(peer) = oldest {
                bytes = bytes.saturating_sub(peer.len());
                pending.remove(&peer);
                self.dropped_reply_tracking_through
                    .entry(message.from.clone())
                    // Loss happens now, not at the old question's sequence.
                    // A reader may already have acknowledged later inbox
                    // messages while that earlier question stayed pending.
                    .and_modify(|dropped| *dropped = (*dropped).max(message.seq))
                    .or_insert(message.seq);
            } else {
                break;
            }
        }
    }

    fn pending_reply_snapshot(
        &self,
        recipient: &str,
        blocked: &BTreeSet<String>,
    ) -> Vec<PendingReply> {
        let Some(pending) = self.pending_replies.get(recipient) else {
            return Vec::new();
        };
        let mut replies = pending
            .iter()
            .map(|(peer, sent_cursor)| {
                let peer_status = self.entries.get(peer).map(|entry| entry.status);
                let live = self.peer_can_reply(peer);
                let blocked_on_caller = live && blocked.contains(peer);
                PendingReply {
                    peer: peer.clone(),
                    sent_cursor: *sent_cursor,
                    peer_status,
                    can_reply: live && !blocked_on_caller,
                    blocked_on_caller,
                }
            })
            .collect::<Vec<_>>();
        replies.sort_unstable_by_key(|reply| reply.sent_cursor);
        replies
    }

    #[allow(clippy::too_many_lines)] // One locked snapshot of inbox, peers and pending replies.
    pub(super) fn wait_snapshot(
        &self,
        recipient: &str,
        peer: Option<&str>,
        cursor: u64,
    ) -> Result<InboxWaitResult> {
        if peer == Some(recipient) {
            return Err(wait_error("INPUT", "an agent cannot wait for itself"));
        }
        if recipient != "parent" && !self.entries.contains_key(recipient) {
            return Err(wait_error("RECIPIENT", "the waiting agent is unknown"));
        }
        let peer_status = if let Some(peer) = peer.filter(|peer| *peer != "parent") {
            Some(
                self.entries
                    .get(peer)
                    .ok_or_else(|| wait_error("PEER", "the named peer is unknown"))?
                    .status,
            )
        } else {
            None
        };
        let blocked = self.blocked_on(recipient);
        let pending_replies = if peer.is_none() {
            self.pending_reply_snapshot(recipient, &blocked)
        } else {
            Vec::new()
        };
        let blocked_peers = peer.map_or_else(
            || {
                pending_replies
                    .iter()
                    .filter(|reply| reply.blocked_on_caller)
                    .map(|reply| reply.peer.clone())
                    .collect::<Vec<_>>()
            },
            |peer| {
                if blocked.contains(peer) {
                    vec![peer.to_string()]
                } else {
                    Vec::new()
                }
            },
        );
        let dropped_tracking_through_cursor = if peer.is_none() {
            self.dropped_reply_tracking_through
                .get(recipient)
                .copied()
                .filter(|dropped| *dropped > cursor)
                .unwrap_or(0)
        } else {
            0
        };
        let queue = self.bus.get(recipient);
        let oldest_cursor = queue.and_then(|queue| queue.front()).map_or(0, |m| m.seq);
        let dropped_through_cursor = self
            .dropped_bus_through
            .get(recipient)
            .copied()
            .filter(|dropped| *dropped > cursor)
            .unwrap_or(0);
        let mut messages = Vec::new();
        let mut bytes = 0_usize;
        let mut next_cursor = cursor;
        let mut has_more = false;
        if let Some(queue) = queue {
            for message in queue.iter().filter(|message| message.seq > cursor) {
                if peer.is_none_or(|peer| message.from == peer) {
                    let size = serde_json::to_vec(message)?.len();
                    if bytes.saturating_add(size) > ipc::MAX_INBOX_PAGE_BYTES {
                        has_more = true;
                        break;
                    }
                    bytes += size;
                    // ubs:ignore[rust.collections.clone-in-loop] Owned reply retains the source inbox.
                    messages.push(message.clone());
                }
                next_cursor = message.seq;
            }
        }
        if !has_more {
            // Acknowledge reported tracking loss even when the caller has
            // received no messages. Never skip an unreturned inbox page.
            next_cursor = next_cursor.max(dropped_tracking_through_cursor);
        }
        let status = if !messages.is_empty() {
            InboxWaitStatus::Messages
        } else if dropped_through_cursor != 0 || dropped_tracking_through_cursor != 0 {
            InboxWaitStatus::HistoryLost
        } else if peer_status.is_some_and(ChildStatus::settled) {
            InboxWaitStatus::PeerStopped
        } else if !blocked_peers.is_empty()
            && (peer.is_some() || !pending_replies.iter().any(|reply| reply.can_reply))
        {
            InboxWaitStatus::PeerBlocked
        } else if peer.is_none() && pending_replies.is_empty() {
            InboxWaitStatus::NothingPending
        } else if peer.is_none() && !pending_replies.iter().any(|reply| reply.can_reply) {
            InboxWaitStatus::PeerStopped
        } else {
            InboxWaitStatus::Pending
        };
        Ok(InboxWaitResult {
            id: recipient.to_string(),
            peer: peer.map(str::to_string),
            status,
            messages,
            next_cursor,
            has_more,
            oldest_cursor,
            dropped_through_cursor,
            peer_status,
            pending_replies,
            dropped_tracking_through_cursor,
            blocked_peers,
        })
    }
}

/// One budget spans every probe and idle interval, including each IPC exchange.
/// On each wake, honor the earlier wall/runtime deadline. A host-supplied timer
/// must still make progress; this does not install an independent watchdog.
struct WaitBudget {
    wall_start: Instant,
    duration: Duration,
    deadline: asupersync::Time,
}

fn now(owner: &AgentCx) -> asupersync::Time {
    owner
        .timer_driver()
        .map_or_else(asupersync::time::wall_now, |timer| timer.now())
}

impl WaitBudget {
    fn new(owner: &AgentCx, wait_ms: u64) -> Result<Self> {
        if !(1..=MAX_WAIT_MS).contains(&wait_ms) {
            return Err(wait_error("INPUT", "waitMs must be between 1 and 60000"));
        }
        Ok(Self {
            wall_start: Instant::now(),
            duration: Duration::from_millis(wait_ms),
            deadline: now(owner).saturating_add_nanos(wait_ms * 1_000_000),
        })
    }

    fn remaining(&self, owner: &AgentCx) -> Result<Duration> {
        owner
            .checkpoint()
            .map_err(|_| wait_error("CANCELLED", "reply wait was cancelled"))?;
        let now = now(owner);
        let inherited = owner.budget().deadline.map_or(Duration::MAX, |deadline| {
            Duration::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
        });
        if inherited.is_zero() {
            return Err(wait_error("DEADLINE", "the caller's deadline has expired"));
        }
        Ok(self
            .duration
            .saturating_sub(self.wall_start.elapsed())
            .min(Duration::from_nanos(
                self.deadline.as_nanos().saturating_sub(now.as_nanos()),
            ))
            .min(inherited))
    }
}

async fn probe_inbox(
    owner: &AgentCx,
    inherited: bool,
    peer: Option<&str>,
    cursor: u64,
    claimed_from: Option<&str>,
    remaining: Duration,
) -> Result<InboxWaitResult> {
    if inherited {
        let reply = ipc::call_from_env_bounded(
            owner,
            ipc::Request {
                action: ipc::Action::Wait,
                name: peer.map(str::to_string),
                text: None,
                claimed_from: claimed_from.map(str::to_string),
                cursor: Some(cursor),
            },
            remaining,
        )
        .await?
        .ok_or_else(|| wait_error("CHANNEL", "the inherited parent channel is unavailable"))?;
        let ipc::Reply::Wait { result } = reply else {
            return Err(wait_error(
                "PROTOCOL",
                "parent returned a different reply type",
            ));
        };
        if result.peer.as_deref() != peer || result.next_cursor < cursor {
            return Err(wait_error(
                "PROTOCOL",
                "parent returned an inconsistent reply cursor",
            ));
        }
        Ok(result)
    } else {
        match super::registry().try_lock() {
            Ok(hub) => hub.wait_snapshot("parent", peer, cursor),
            Err(std::sync::TryLockError::WouldBlock) => {
                Err(wait_error("BUSY", "the parent agent registry is busy"))
            }
            Err(std::sync::TryLockError::Poisoned(_)) => Err(wait_error(
                "UNAVAILABLE",
                "the parent agent registry is unavailable",
            )),
        }
    }
}

/// Wait for messages from one named run, or automatically await unanswered
/// sends when no peer is named.
///
/// Other live agents never sustain an automatic wait. A child can only read
/// its own authenticated inbox; a root caller reads the parent inbox. An
/// inherited channel never falls back to a different local registry.
#[allow(clippy::too_many_lines)] // Keep lease ownership and probes in one budgeted scope.
pub async fn wait_for_replies(
    peer: Option<&str>,
    cursor: Option<u64>,
    wait_ms: u64,
    claimed_from: Option<&str>,
) -> Result<InboxWaitResult> {
    if peer.is_some_and(|peer| peer.trim().is_empty()) {
        return Err(wait_error(
            "INPUT",
            "name must be a peer run id or parent, or omitted for unanswered sends",
        ));
    }
    let owner = AgentCx::for_current_or_request();
    if !owner.capabilities().io || !owner.capabilities().time {
        return Err(wait_error(
            "PERMISSION",
            "reply wait requires I/O and timer authority",
        ));
    }
    let budget = WaitBudget::new(&owner, wait_ms)?;
    let inherited = ipc::inherited_channel_present();
    if !inherited && claimed_from.is_some_and(|from| from != "parent") {
        return Err(wait_error(
            "SENDER",
            "a root reply wait reads only the parent inbox",
        ));
    }
    let mut cursor = cursor.unwrap_or(0);
    let mut last_pending: Option<InboxWaitResult> = None;
    // Every return, cancellation and dropped future releases these owners.
    // Neither cleanup path acquires the registry lock or cancels a child.
    let mut local_hold = None;
    let mut inherited_hold = None;
    loop {
        let remaining = budget.remaining(&owner)?;
        if inherited && remaining.is_zero() {
            if let Some(mut result) = last_pending {
                result.status = InboxWaitStatus::TimedOut;
                return Ok(result);
            }
            return Err(wait_error(
                "TIMEOUT",
                "reply wait expired before a complete inbox probe",
            ));
        }
        let probe = probe_inbox(&owner, inherited, peer, cursor, claimed_from, remaining).await;
        let mut result = match probe {
            Ok(result) => result,
            Err(error) => {
                // Only read-only contention is retryable. A send is never
                // replayed, and cancellation/revocation/auth failures remain
                // terminal even at the end of the requested wait budget.
                let remaining = budget.remaining(&owner)?;
                if !remaining.is_zero()
                    && (has_code(&error, "PI_HUB_IPC_BUSY:")
                        || has_code(&error, "PI_HUB_WAIT_BUSY:"))
                {
                    owner
                        .time()
                        .sleep(Duration::from_millis(10).min(remaining))
                        .await;
                    continue;
                }
                return expired_probe(error, remaining, last_pending);
            }
        };
        let remaining = budget.remaining(&owner)?;
        if result.status != InboxWaitStatus::Pending {
            return Ok(result);
        }
        if remaining.is_zero() {
            result.status = InboxWaitStatus::TimedOut;
            return Ok(result);
        }
        cursor = result.next_cursor;
        last_pending = Some(result);
        if (inherited && inherited_hold.is_none()) || (!inherited && local_hold.is_none()) {
            let acquired = if inherited {
                match ipc::open_wait_channel(&owner, peer, cursor, claimed_from, remaining).await {
                    Ok((snapshot, channel)) => {
                        inherited_hold = Some(channel);
                        if snapshot.status != InboxWaitStatus::Pending {
                            return Ok(snapshot);
                        }
                        cursor = snapshot.next_cursor;
                        last_pending = Some(snapshot);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            } else {
                match super::registry().try_lock() {
                    Ok(mut hub) => hub
                        .begin_reply_wait("parent", peer, cursor, remaining, None)
                        .map(|hold| local_hold = Some(hold)),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        Err(wait_error("BUSY", "the parent agent registry is busy"))
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => Err(wait_error(
                        "UNAVAILABLE",
                        "the parent agent registry is unavailable",
                    )),
                }
            };
            if let Err(error) = acquired {
                let remaining = budget.remaining(&owner)?;
                if !remaining.is_zero()
                    && (has_code(&error, "PI_HUB_IPC_BUSY:")
                        || has_code(&error, "PI_HUB_WAIT_BUSY:"))
                {
                    owner
                        .time()
                        .sleep(Duration::from_millis(10).min(remaining))
                        .await;
                    continue;
                }
                return expired_probe(error, remaining, last_pending);
            }
            // A new edge can close a cycle, or race a final reply. Probe
            // immediately; lease acquisition never adds an idle interval.
            continue;
        }
        // A local final probe is nonblocking and observes messages/exit/loss
        // that arrived in the last slice. For IPC, leave half the remaining
        // budget for another exchange rather than sleeping through it.
        let sleep_for = if inherited {
            POLL_INTERVAL.min(remaining / 2)
        } else {
            POLL_INTERVAL.min(remaining)
        };
        owner.time().sleep(sleep_for).await;
    }
}

#[cfg(test)]
mod tests;
