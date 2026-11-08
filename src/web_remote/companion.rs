//! An owned loopback service. Network clients can observe screened snapshots
//! and propose ordinary prompts; only the terminal can admit a proposal.

use super::{BindMode, TokenKind, WebAuditEvent, WebRemoteSettings};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod http;
mod projection;

const TOKEN_LIFETIME: Duration = Duration::from_secs(10 * 60);
const VIEWER_IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);
const PROPOSAL_LIFETIME: Duration = Duration::from_secs(2 * 60);
const PROPOSAL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_PENDING_TOKENS: usize = 16;
const MAX_VIEWERS: usize = 16;
const MAX_PROMPT_BYTES: usize = 8 * 1024;
const MAX_AUDIT_EVENTS: usize = 128;

/// A proposal has no execution authority until the local terminal accepts it
/// and its driver resolves the still-live ID against the current session.
#[derive(Clone, PartialEq, Eq)]
pub struct PromptProposal {
    pub id: String,
    pub owner_session_id: String,
    pub text: String,
}

impl std::fmt::Debug for PromptProposal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PromptProposal")
            .field("id", &self.id)
            .field("text_bytes", &self.text.len())
            .finish_non_exhaustive()
    }
}

/// A handle cannot keep the listener alive after its owning server is dropped.
#[derive(Clone)]
pub struct CompanionHandle {
    shared: Arc<Mutex<State>>,
    address: SocketAddr,
}

impl std::fmt::Debug for CompanionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompanionHandle")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

/// Drop revokes all capabilities, closes sockets, and joins the single bounded
/// nonblocking I/O worker. No per-client task or detached thread is created.
pub struct CompanionServer {
    handle: CompanionHandle,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for CompanionServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompanionServer")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl CompanionServer {
    pub fn start(settings: WebRemoteSettings) -> io::Result<Self> {
        if settings.bind_mode != BindMode::Loopback {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the supervised browser companion supports loopback binding only",
            ));
        }
        if !settings.require_auth_token || !(1..=MAX_VIEWERS).contains(&settings.max_viewers) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "browser access requires authentication and between 1 and 16 viewers",
            ));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, settings.port))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let handle = CompanionHandle {
            shared: Arc::new(Mutex::new(State::new(settings))),
            address,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_handle = handle.clone();
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("pi-browser-companion".to_string())
            .spawn(move || {
                http::serve(listener, &worker_handle, &worker_stop);
                let mut state = worker_handle
                    .shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.active = false;
                state.revoke_all();
                state.snapshot = Arc::from("{}");
                state.owner_session_id = None;
            })?;
        Ok(Self {
            handle,
            stop,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn handle(&self) -> CompanionHandle {
        self.handle.clone()
    }
}

impl Drop for CompanionServer {
    fn drop(&mut self) {
        {
            let mut state = self
                .handle
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = false;
            state.revoke_all();
            state.snapshot = Arc::from("{}");
            state.owner_session_id = None;
        }
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

struct TokenGrant {
    kind: TokenKind,
    deadline: Instant,
}

struct Viewer {
    kind: TokenKind,
    last_seen: Instant,
    next_proposal: Instant,
    receipt: Option<ProposalReceipt>,
}

#[derive(Clone, Serialize)]
struct ProposalReceipt {
    id: String,
    status: &'static str,
    reason: String,
}

struct ProposalSlot {
    proposal: PromptProposal,
    client: [u8; 32],
    delivered: bool,
    deadline: Instant,
}

struct State {
    active: bool,
    settings: WebRemoteSettings,
    tokens: HashMap<[u8; 32], TokenGrant>,
    viewers: HashMap<[u8; 32], Viewer>,
    proposal: Option<ProposalSlot>,
    owner_session_id: Option<String>,
    generation: u64,
    seq: u64,
    busy: bool,
    snapshot: Arc<str>,
    audit: VecDeque<WebAuditEvent>,
}

impl State {
    fn new(settings: WebRemoteSettings) -> Self {
        Self {
            active: true,
            settings,
            tokens: HashMap::new(),
            viewers: HashMap::new(),
            proposal: None,
            owner_session_id: None,
            generation: 0,
            seq: 1,
            busy: true,
            snapshot: Arc::from(
                r#"{"name":"Starting Pi","model":"","messages":[],"omitted":false}"#,
            ),
            audit: VecDeque::new(),
        }
    }

    fn changed(&mut self) {
        if let Some(next) = self.seq.checked_add(1) {
            self.seq = next;
        } else {
            self.active = false;
            self.tokens.clear();
            self.viewers.clear();
            self.proposal = None;
        }
    }

    fn record(&mut self, event: &'static str) {
        if self.settings.enable_audit_log {
            if self.audit.len() == MAX_AUDIT_EVENTS {
                self.audit.pop_front();
            }
            // Metadata only: never retain a credential, prompt, session ID,
            // transcript, or browser-supplied field in the audit trail.
            self.audit.push_back(WebAuditEvent::new(event, None));
        }
    }

    fn revoke_all(&mut self) {
        self.tokens.clear();
        self.viewers.clear();
        self.proposal = None;
        self.changed();
        self.record("access_revoked");
    }

    fn reject_proposal(&mut self, reason: &'static str) {
        if let Some(slot) = self.proposal.take() {
            if let Some(viewer) = self.viewers.get_mut(&slot.client) {
                viewer.receipt = Some(ProposalReceipt {
                    id: slot.proposal.id,
                    status: "rejected",
                    reason: reason.to_string(),
                });
            }
            self.changed();
        }
    }

    fn expire(&mut self, now: Instant) {
        self.tokens.retain(|_, grant| now < grant.deadline);
        let previous_viewers = self.viewers.len();
        self.viewers
            .retain(|_, viewer| now.saturating_duration_since(viewer.last_seen) < VIEWER_IDLE_LIMIT);
        if self.proposal.as_ref().is_some_and(|slot| {
            now >= slot.deadline || !self.viewers.contains_key(&slot.client)
        }) {
            self.reject_proposal("The browser proposal expired. Submit it again when ready.");
            self.record("proposal_expired");
        }
        if self.viewers.len() != previous_viewers {
            self.changed();
            self.record("viewer_expired");
        }
    }
}

impl CompanionHandle {
    /// Issue locally. The returned secret belongs only in a local overlay or
    /// terminal notice, never in a transcript, audit record, or request URL.
    pub fn pairing_url(&self, kind: TokenKind) -> io::Result<String> {
        let token = random_token()?;
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        state.expire(now);
        if !state.active {
            return Err(io::Error::other("the browser companion has stopped"));
        }
        if state.tokens.len() >= MAX_PENDING_TOKENS {
            return Err(io::Error::other(
                "too many unused browser links; revoke them before creating another",
            ));
        }
        let kind = if state.settings.view_only {
            TokenKind::View
        } else {
            kind
        };
        state.tokens.insert(
            token_hash(&token),
            TokenGrant {
                kind,
                deadline: now + TOKEN_LIFETIME,
            },
        );
        state.record("pairing_issued");
        Ok(format!("http://{}/#token={token}", self.address))
    }

    pub fn revoke_all(&self) {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revoke_all();
    }

    #[must_use]
    pub fn take_proposal(&self) -> Option<PromptProposal> {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire(Instant::now());
        if !state.active {
            return None;
        }
        let slot = state.proposal.as_mut()?;
        if slot.delivered {
            return None;
        }
        slot.delivered = true;
        Some(slot.proposal.clone())
    }

    /// The driver's final admission point. A revoked, expired, or replaced
    /// proposal can never be resurrected by a delayed UI acceptance.
    #[must_use]
    pub fn resolve_proposal(&self, id: &str, accepted: bool, reason: &str) -> bool {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.expire(Instant::now());
        if !state.active
            || !state.proposal.as_ref().is_some_and(|slot| {
                slot.proposal.id == id
                    && slot.delivered
                    && state.owner_session_id.as_deref()
                        == Some(slot.proposal.owner_session_id.as_str())
                    && state.viewers.contains_key(&slot.client)
            })
        {
            return false;
        }
        let Some(slot) = state.proposal.take() else {
            return false;
        };
        if let Some(viewer) = state.viewers.get_mut(&slot.client) {
            // Reasons are host-authored, but do not let a caller accidentally
            // turn an error payload into an unscreened browser response.
            let reason = if reason.len() <= 1024 {
                crate::crash::redact_text(reason)
            } else {
                "The local terminal resolved this proposal.".to_string()
            };
            viewer.receipt = Some(ProposalReceipt {
                id: slot.proposal.id,
                status: if accepted { "accepted" } else { "rejected" },
                reason,
            });
        }
        if accepted {
            state.busy = true;
        }
        state.changed();
        state.record(if accepted { "proposal_admitted" } else { "proposal_rejected" });
        true
    }

    pub fn set_busy(&self, busy: bool) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active && state.busy != busy {
            state.busy = busy;
            state.changed();
        }
    }

    /// Refresh only at a driver boundary after the operation has completed.
    /// The projection omits incomplete deltas and never exports FTUI drafts,
    /// login text, permission cards, tool arguments, or binary attachments.
    pub async fn refresh(&self, handle: &crate::sdk::AgentSessionHandle) -> crate::Result<()> {
        let identity = handle
            .with_session(|session| {
                let name = session.get_name_ref().unwrap_or("Pi session");
                if name.len() > crate::text_completion::MAX_INPUT_BYTES
                    || session.header.id.len() > 4096
                {
                    return None;
                }
                Some((session.header.id.clone(), name.to_string()))
            })
            .await;
        let (owner, name) = match identity {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                self.clear_view();
                return Ok(());
            }
            Err(error) => {
                self.clear_view();
                return Err(error);
            }
        };
        let agent = &handle.session().agent;
        let provider = agent.provider();
        if provider.name().len().saturating_add(provider.model_id().len())
            >= crate::text_completion::MAX_INPUT_BYTES
        {
            self.clear_view();
            return Ok(());
        }
        let model = format!("{}/{}", provider.name(), provider.model_id());
        let snapshot = projection::project(
            &name,
            &model,
            agent.messages(),
            &agent.auxiliary_privacy_snapshot(),
        );
        self.publish(owner, snapshot);
        Ok(())
    }

    fn clear_view(&self) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.reject_proposal("The current session is unavailable.");
        state.owner_session_id = None;
        state.generation = state.generation.saturating_add(1);
        state.snapshot = Arc::from(
            r#"{"name":"Session unavailable","model":"","messages":[],"omitted":true}"#,
        );
        state.changed();
    }

    fn publish(&self, owner: String, snapshot: String) {
        let mut state = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.active {
            return;
        }
        if state.owner_session_id.as_deref() != Some(owner.as_str()) {
            state.reject_proposal("The local terminal changed sessions.");
            state.owner_session_id = Some(owner);
            state.generation = state.generation.saturating_add(1);
        }
        state.snapshot = Arc::from(snapshot);
        state.changed();
    }

    #[must_use]
    pub fn audit_events(&self) -> Vec<WebAuditEvent> {
        self.shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .audit
            .iter()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn test_delivered_proposal(&self, text: &str) -> PromptProposal {
        tests::delivered_proposal(self, text)
    }
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn random_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| io::Error::other("secure browser pairing entropy is unavailable"))?;
    Ok(crate::package_manager::hex_encode(&bytes))
}

#[cfg(test)]
mod tests;
