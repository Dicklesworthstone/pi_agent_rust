#![forbid(unsafe_code)]

//! Browser access to a supervised local Pi session.
//!
//! [`CompanionServer`] serves the `pi web` loopback companion with bounded
//! semantic snapshots and locally accepted prompt proposals. [`WebRemoteServer`]
//! and [`WebRemoteBridge`] power `pi web --terminal` and embedded terminal hosts
//! with bounded frame diffs and locally approved control. The two modes do not
//! share credentials or control leases. WASM, headless sessions, and scannable
//! QR pairing remain future work.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub mod bridge;
mod companion;
pub mod server;

pub use bridge::{WebRemoteBridge, WebRemoteEvent, WebRemoteInput};
pub use companion::{CompanionHandle, CompanionServer, PromptProposal};
pub use server::WebRemoteServer;

pub const MAX_WEB_VIEWERS: usize = 64;
pub const MAX_WEB_TOKEN_RECORDS: usize = 128;
pub const MAX_WEB_AUDIT_EVENTS: usize = 1024;
const MAX_CLIENT_ID_BYTES: usize = 128;
const MAX_REMOTE_ADDR_BYTES: usize = 256;
const MAX_TOKEN_BYTES: usize = 128;
const TOKEN_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Select one browser surface for the native session. A launch owns only the
/// selected server, its credential namespace, and its local admission controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WebRemoteMode {
    /// Bounded conversation snapshots and approval of each proposed prompt.
    #[default]
    Companion,
    /// Native terminal projection with an explicitly approved control lease.
    Terminal,
}

/// Network interface binding mode for web-remote server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindMode {
    #[default]
    Loopback,
    Tailscale,
    Lan,
}

impl std::str::FromStr for BindMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "loopback" | "127.0.0.1" | "localhost" => Ok(Self::Loopback),
            "tailscale" | "tailnet" => Ok(Self::Tailscale),
            "lan" | "all" | "0.0.0.0" => Ok(Self::Lan),
            other => Err(format!("unknown bind mode: {other}")),
        }
    }
}

/// Web-remote configuration settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebRemoteSettings {
    pub port: u16,
    pub bind_mode: BindMode,
    pub view_only: bool,
    pub max_viewers: usize,
    pub require_auth_token: bool,
    pub enable_audit_log: bool,
}

impl Default for WebRemoteSettings {
    fn default() -> Self {
        Self {
            port: 8080,
            bind_mode: BindMode::Loopback,
            view_only: false,
            max_viewers: 4,
            require_auth_token: true,
            enable_audit_log: true,
        }
    }
}

impl WebRemoteSettings {
    /// Check the bounds required by the live network server.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_WEB_VIEWERS).contains(&self.max_viewers) {
            return Err(format!(
                "max_viewers must be between 1 and {MAX_WEB_VIEWERS}"
            ));
        }
        if !self.require_auth_token {
            return Err("web remote access requires single-use authentication tokens".to_string());
        }
        if !self.enable_audit_log {
            return Err("web remote access requires security audit logging".to_string());
        }
        Ok(())
    }
}

/// Token role permission: Steer (input allowed) or View (read-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Steer,
    View,
}

/// Authentication token metadata and lifecycle record.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthTokenRecord {
    pub token: String,
    pub kind: TokenKind,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub consumed: bool,
    pub revoked: bool,
}

impl std::fmt::Debug for AuthTokenRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthTokenRecord")
            .field("token", &"[REDACTED]")
            .field("kind", &self.kind)
            .field("issued_at_ms", &self.issued_at_ms)
            .field("expires_at_ms", &self.expires_at_ms)
            .field("consumed", &self.consumed)
            .field("revoked", &self.revoked)
            .finish()
    }
}

/// Frame payload schema `pi.web.frame.v1`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebFrame {
    pub schema: String,
    pub seq: u64,
    pub timestamp_ms: u64,
    pub frame_type: WebFrameType,
    pub width: u16,
    pub height: u16,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WebFrameType {
    Keyframe,
    Patch,
    Heartbeat,
    ApprovalPrompt,
    TakeoverStatus,
}

impl WebFrame {
    pub fn new_keyframe(seq: u64, width: u16, height: u16, data: impl Into<String>) -> Self {
        Self {
            schema: "pi.web.frame.v1".to_string(),
            seq,
            timestamp_ms: current_time_ms(),
            frame_type: WebFrameType::Keyframe,
            width,
            height,
            data: data.into(),
        }
    }

    pub fn new_patch(seq: u64, width: u16, height: u16, patch: impl Into<String>) -> Self {
        Self {
            schema: "pi.web.frame.v1".to_string(),
            seq,
            timestamp_ms: current_time_ms(),
            frame_type: WebFrameType::Patch,
            width,
            height,
            data: patch.into(),
        }
    }
}

/// Input arbitration control state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlMode {
    LocalTerminal,
    RemoteControlling,
    TakeoverPendingApproval,
    ViewOnly,
}

/// Client session info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientSession {
    pub client_id: String,
    pub connected_at_ms: u64,
    pub last_seq_acked: u64,
    pub is_controller: bool,
    pub is_view_only: bool,
    pub remote_addr: String,
}

/// Audit event record schema `pi.web.audit.v1`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebAuditEvent {
    pub schema: String,
    pub timestamp_ms: u64,
    pub event_type: String,
    pub client_id: Option<String>,
    pub details: HashMap<String, String>,
}

impl WebAuditEvent {
    pub fn new(event_type: impl Into<String>, client_id: Option<String>) -> Self {
        Self {
            schema: "pi.web.audit.v1".to_string(),
            timestamp_ms: current_time_ms(),
            event_type: event_type.into(),
            client_id,
            details: HashMap::new(),
        }
    }

    #[must_use]
    pub fn with_detail(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.details.insert(key.into(), value.into());
        self
    }
}

/// In-memory state manager for the web-remote session.
#[derive(Clone)]
pub struct WebRemoteManager {
    settings: WebRemoteSettings,
    state: Arc<Mutex<WebRemoteState>>,
}

#[derive(Default)]
struct WebRemoteState {
    clients: HashMap<String, ManagedClient>,
    active_controller: Option<String>,
    pending_takeovers: VecDeque<String>,
    control_epoch: u64,
    frame_seq: u64,
    audit_log: VecDeque<WebAuditEvent>,
    audit_events_dropped: u64,
    tokens: HashMap<String, ManagedToken>,
}

struct ManagedClient {
    session: ClientSession,
    last_input_seq: u64,
    pairing_token: Option<String>,
    token_kind: TokenKind,
}

struct ManagedToken {
    record: AuthTokenRecord,
    deadline: Instant,
}

impl WebRemoteState {
    fn prune_expired_tokens(&mut self, now: Instant) {
        self.tokens.retain(|_, token| now < token.deadline);
    }

    fn advance_control_epoch(&mut self) -> Result<u64, String> {
        let Some(next) = self.control_epoch.checked_add(1) else {
            self.active_controller = None;
            for client in self.clients.values_mut() {
                client.session.is_controller = false;
            }
            return Err("web remote control generation exhausted".to_string());
        };
        self.control_epoch = next;
        Ok(next)
    }

    fn consume_token(&mut self, token: &str, now: Instant) -> Result<TokenKind, String> {
        let record = self
            .tokens
            .get_mut(token)
            .ok_or_else(|| "token not found".to_string())?;
        if record.record.revoked {
            return Err("token has been revoked".to_string());
        }
        if record.record.consumed {
            return Err("token has already been consumed (single-use)".to_string());
        }
        if now >= record.deadline {
            return Err("token has expired".to_string());
        }
        record.record.consumed = true;
        Ok(record.record.kind)
    }
}

impl std::fmt::Debug for WebRemoteManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        formatter
            .debug_struct("WebRemoteManager")
            .field("settings", &self.settings)
            .field("clients", &state.clients.len())
            .field("pending_takeovers", &state.pending_takeovers.len())
            .field("token_records", &state.tokens.len())
            .finish_non_exhaustive()
    }
}

#[allow(clippy::significant_drop_tightening)]
impl WebRemoteManager {
    pub fn new(settings: WebRemoteSettings) -> Self {
        Self {
            settings,
            state: Arc::new(Mutex::new(WebRemoteState::default())),
        }
    }

    pub const fn settings(&self) -> &WebRemoteSettings {
        &self.settings
    }

    /// Create a fresh 256-bit pairing secret using the operating system RNG.
    pub fn generate_token(&self, kind: TokenKind) -> Result<String, std::io::Error> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        self.issue_token(token.clone(), kind)
            .map_err(std::io::Error::other)?;
        Ok(token)
    }

    /// Issue a new single-use authentication token with 10-minute expiry.
    /// Live servers use `generate_token`; this explicit form also permits
    /// deterministic fixtures. Unexpired or connected credentials cannot be reset.
    pub fn issue_token(
        &self,
        token: impl Into<String>,
        kind: TokenKind,
    ) -> Result<AuthTokenRecord, String> {
        self.issue_token_at(token.into(), kind, Instant::now(), current_time_ms())
    }

    fn issue_token_at(
        &self,
        token: String,
        kind: TokenKind,
        now: Instant,
        wall_time_ms: u64,
    ) -> Result<AuthTokenRecord, String> {
        if !valid_token(&token) {
            return Err("invalid authentication token format".to_string());
        }
        let deadline = now
            .checked_add(TOKEN_LIFETIME)
            .ok_or_else(|| "authentication token deadline overflow".to_string())?;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Test duplication before pruning so an expired credential cannot be
        // revived by a repeated issuance call. Active clients retain their
        // credential identity even after the admission record expires.
        if state.tokens.contains_key(&token)
            || state
                .clients
                .values()
                .any(|client| client.pairing_token.as_deref() == Some(token.as_str()))
        {
            return Err("authentication token was already issued".to_string());
        }
        state.prune_expired_tokens(now);
        if state.tokens.len() >= MAX_WEB_TOKEN_RECORDS {
            return Err("maximum outstanding authentication token records reached".to_string());
        }
        let record = AuthTokenRecord {
            token: token.clone(),
            kind,
            issued_at_ms: wall_time_ms,
            expires_at_ms: wall_time_ms.saturating_add(10 * 60 * 1000),
            consumed: false,
            revoked: false,
        };
        state.tokens.insert(
            token,
            ManagedToken {
                record: record.clone(),
                deadline,
            },
        );
        self.record_audit(
            &mut state,
            WebAuditEvent::new("token_issued", None)
                .with_detail("token_kind", format!("{kind:?}"))
                .with_detail("expires_at_ms", record.expires_at_ms.to_string()),
        );
        Ok(record)
    }

    /// Revoke a pairing credential and any live connection admitted with it.
    pub fn revoke_token(&self, token: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut kind = state.tokens.get_mut(token).map(|entry| {
            entry.record.revoked = true;
            entry.record.kind
        });
        let connected: Vec<String> = state
            .clients
            .iter()
            .filter(|(_, client)| client.pairing_token.as_deref() == Some(token))
            .map(|(id, _)| id.clone())
            .collect();
        let mut controller_revoked = false;
        for id in &connected {
            if let Some(client) = state.clients.remove(id) {
                kind = Some(client.token_kind);
            }
            state.pending_takeovers.retain(|pending| pending != id);
            if state.active_controller.as_ref() == Some(id) {
                state.active_controller = None;
                controller_revoked = true;
            }
            self.record_audit(
                &mut state,
                WebAuditEvent::new("client_disconnected", Some(id.clone()))
                    .with_detail("reason", "credential_revoked"),
            );
        }
        let Some(kind) = kind else {
            return false;
        };
        if controller_revoked {
            let _ = state.advance_control_epoch();
        }
        self.record_audit(
            &mut state,
            WebAuditEvent::new("token_revoked", None)
                .with_detail("token_kind", format!("{kind:?}")),
        );
        true
    }

    /// Validate and consume a token upon connection.
    pub fn validate_and_consume_token(&self, token: &str) -> Result<TokenKind, String> {
        if !self.settings.require_auth_token {
            return Ok(TokenKind::Steer);
        }
        if !valid_token(token) {
            return Err("invalid authentication token format".to_string());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let record_kind = match state.consume_token(token, Instant::now()) {
            Ok(kind) => kind,
            Err(reason) => {
                self.record_audit(
                    &mut state,
                    WebAuditEvent::new("token_rejected", None).with_detail("reason", &reason),
                );
                return Err(reason);
            }
        };
        self.record_audit(
            &mut state,
            WebAuditEvent::new("token_consumed", None)
                .with_detail("token_kind", format!("{record_kind:?}")),
        );
        Ok(record_kind)
    }

    /// Connect a new client viewer.
    pub fn connect_client(
        &self,
        client_id: &str,
        remote_addr: &str,
        token: Option<&str>,
    ) -> Result<ClientSession, String> {
        self.connect_client_at(client_id, remote_addr, token, Instant::now())
    }

    fn connect_client_at(
        &self,
        client_id: &str,
        remote_addr: &str,
        token: Option<&str>,
        now: Instant,
    ) -> Result<ClientSession, String> {
        if client_id.is_empty()
            || client_id.len() > MAX_CLIENT_ID_BYTES
            || !client_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err("invalid client identifier".to_string());
        }
        if remote_addr.is_empty()
            || remote_addr.len() > MAX_REMOTE_ADDR_BYTES
            || remote_addr.chars().any(char::is_control)
        {
            return Err("invalid remote socket address".to_string());
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.clients.contains_key(client_id) {
            return Err("client identifier is already connected".to_string());
        }
        let capacity = self.settings.max_viewers.min(MAX_WEB_VIEWERS);
        if state.clients.len() >= capacity {
            return Err(format!("maximum viewer capacity ({capacity}) reached"));
        }
        // Admission and consumption share the state lock. Failed capacity or
        // duplicate-ID checks leave the credential available for a real retry.
        let (token_kind, pairing_token) = if self.settings.require_auth_token {
            let token = token.ok_or_else(|| "missing authentication token".to_string())?;
            if !valid_token(token) {
                return Err("invalid authentication token format".to_string());
            }
            let kind = match state.consume_token(token, now) {
                Ok(kind) => kind,
                Err(reason) => {
                    self.record_audit(
                        &mut state,
                        WebAuditEvent::new("token_rejected", None).with_detail("reason", &reason),
                    );
                    return Err(reason);
                }
            };
            self.record_audit(
                &mut state,
                WebAuditEvent::new("token_consumed", None)
                    .with_detail("token_kind", format!("{kind:?}")),
            );
            (kind, Some(token.to_string()))
        } else {
            (TokenKind::Steer, None)
        };
        let is_view_only = self.settings.view_only || token_kind == TokenKind::View;
        let session = ClientSession {
            client_id: client_id.to_string(),
            connected_at_ms: current_time_ms(),
            last_seq_acked: 0,
            is_controller: false,
            is_view_only,
            remote_addr: remote_addr.to_string(),
        };
        state.clients.insert(
            client_id.to_string(),
            ManagedClient {
                session: session.clone(),
                last_input_seq: 0,
                pairing_token,
                token_kind,
            },
        );
        self.record_audit(
            &mut state,
            WebAuditEvent::new("client_connected", Some(client_id.to_string()))
                .with_detail("remote_addr", remote_addr)
                .with_detail("is_view_only", is_view_only.to_string()),
        );

        Ok(session)
    }

    /// Disconnect a client viewer.
    pub fn disconnect_client(&self, client_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.clients.remove(client_id).is_none() {
            return;
        }
        state
            .pending_takeovers
            .retain(|pending| pending != client_id);
        if state.active_controller.as_deref() == Some(client_id) {
            state.active_controller = None;
            let _ = state.advance_control_epoch();
        }
        self.record_audit(
            &mut state,
            WebAuditEvent::new("client_disconnected", Some(client_id.to_string())),
        );
    }

    /// Request steering control by a remote client.
    pub fn request_takeover(&self, client_id: &str) -> Result<ControlMode, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let client = state
            .clients
            .get(client_id)
            .ok_or_else(|| "unregistered client id".to_string())?;
        if client.session.is_view_only || self.settings.view_only {
            return Err("client is in view-only mode".to_string());
        }
        if state.active_controller.as_deref() == Some(client_id) {
            return Ok(ControlMode::RemoteControlling);
        }
        if !state
            .pending_takeovers
            .iter()
            .any(|pending| pending == client_id)
        {
            state.pending_takeovers.push_back(client_id.to_string());
            self.record_audit(
                &mut state,
                WebAuditEvent::new("takeover_requested", Some(client_id.to_string()))
                    .with_detail("pending_approval", "true"),
            );
        }
        Ok(ControlMode::TakeoverPendingApproval)
    }

    /// Resolve a takeover from the trusted local operator path only. Network
    /// message dispatch must never expose this operation to a browser.
    pub fn approve_takeover(&self, client_id: &str) -> Result<ControlMode, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let client = state
            .clients
            .get(client_id)
            .ok_or_else(|| "unregistered client id".to_string())?;
        if client.session.is_view_only || self.settings.view_only {
            return Err("client is in view-only mode".to_string());
        }
        if !state
            .pending_takeovers
            .iter()
            .any(|pending| pending == client_id)
        {
            return Err("client has no pending takeover request".to_string());
        }
        state.advance_control_epoch()?;
        state.active_controller = Some(client_id.to_string());
        state
            .pending_takeovers
            .retain(|pending| pending != client_id);
        for (id, client) in &mut state.clients {
            client.session.is_controller = id == client_id;
        }
        self.record_audit(
            &mut state,
            WebAuditEvent::new("takeover_granted", Some(client_id.to_string()))
                .with_detail("actor", "local_operator"),
        );
        Ok(ControlMode::RemoteControlling)
    }

    /// Reject a pending request from the trusted local operator path.
    pub fn deny_takeover(&self, client_id: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = state.pending_takeovers.len();
        state
            .pending_takeovers
            .retain(|pending| pending != client_id);
        let denied = state.pending_takeovers.len() != previous;
        if denied {
            self.record_audit(
                &mut state,
                WebAuditEvent::new("takeover_denied", Some(client_id.to_string()))
                    .with_detail("actor", "local_operator"),
            );
        }
        denied
    }

    /// Local terminal input always wins and invalidates queued remote input.
    pub fn revoke_control(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = state.active_controller.take();
        state.pending_takeovers.clear();
        let _ = state.advance_control_epoch();
        for client in state.clients.values_mut() {
            client.session.is_controller = false;
        }
        self.record_audit(
            &mut state,
            WebAuditEvent::new("control_released_to_local", previous)
                .with_detail("actor", "local_operator"),
        );
    }

    pub fn clients(&self) -> Vec<ClientSession> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut clients: Vec<ClientSession> = state
            .clients
            .values()
            .map(|client| client.session.clone())
            .collect();
        clients.sort_by(|left, right| left.client_id.cmp(&right.client_id));
        clients
    }

    pub fn pending_takeovers(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending_takeovers
            .iter()
            .cloned()
            .collect()
    }

    /// Admit one remote input and return its control generation. The UI must
    /// call `input_authorized` again immediately before dispatching the event.
    pub fn authorize_input(&self, client_id: &str, sequence: u64) -> Result<u64, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active_controller.as_deref() != Some(client_id) {
            return Err("remote input requires local takeover approval".to_string());
        }
        let epoch = state.control_epoch;
        let client = state
            .clients
            .get_mut(client_id)
            .ok_or_else(|| "unregistered client id".to_string())?;
        if client.session.is_view_only || !client.session.is_controller || self.settings.view_only {
            return Err("client is in view-only mode".to_string());
        }
        if sequence <= client.last_input_seq {
            return Err("remote input sequence was already used or is out of order".to_string());
        }
        client.last_input_seq = sequence;
        self.record_audit(
            &mut state,
            WebAuditEvent::new("input_authorized", Some(client_id.to_string()))
                .with_detail("sequence", sequence.to_string())
                .with_detail("control_epoch", epoch.to_string()),
        );
        Ok(epoch)
    }

    pub fn input_authorized(&self, client_id: &str, epoch: u64) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.control_epoch == epoch
            && state.active_controller.as_deref() == Some(client_id)
            && !self.settings.view_only
            && state
                .clients
                .get(client_id)
                .is_some_and(|client| !client.session.is_view_only && client.session.is_controller)
    }

    /// Sensitive values for the host-only frame redactor. Never serialize this
    /// list into status responses, audit events, or browser protocol messages.
    pub fn tokens_for_redaction(&self) -> Vec<String> {
        self.tokens_for_redaction_at(Instant::now())
    }

    fn tokens_for_redaction_at(&self, now: Instant) -> Vec<String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.prune_expired_tokens(now);
        let mut tokens: Vec<String> = state.tokens.keys().cloned().collect();
        tokens.extend(
            state
                .clients
                .values()
                .filter_map(|client| client.pairing_token.clone()),
        );
        tokens.sort();
        tokens.dedup();
        tokens
    }

    /// Release steering control back to local terminal.
    pub fn release_control(&self, client_id: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .pending_takeovers
            .retain(|pending| pending != client_id);
        let was_controller = state.active_controller.as_deref() == Some(client_id);
        if was_controller {
            state.active_controller = None;
            let _ = state.advance_control_epoch();
            if let Some(client) = state.clients.get_mut(client_id) {
                client.session.is_controller = false;
            }
            self.record_audit(
                &mut state,
                WebAuditEvent::new("control_released_to_local", Some(client_id.to_string())),
            );
        }
    }

    /// Generate next broadcast frame.
    pub fn next_frame(
        &self,
        frame_type: WebFrameType,
        width: u16,
        height: u16,
        data: impl Into<String>,
    ) -> WebFrame {
        let seq = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.frame_seq = state.frame_seq.saturating_add(1);
            state.frame_seq
        };

        WebFrame {
            schema: "pi.web.frame.v1".to_string(),
            seq,
            timestamp_ms: current_time_ms(),
            frame_type,
            width,
            height,
            data: data.into(),
        }
    }

    /// Get current active audit events.
    pub fn audit_log(&self) -> Vec<WebAuditEvent> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .audit_log
            .iter()
            .cloned()
            .collect()
    }

    pub fn dropped_audit_events(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .audit_events_dropped
    }

    fn record_audit(&self, state: &mut WebRemoteState, event: WebAuditEvent) {
        if self.settings.enable_audit_log {
            if state.audit_log.len() == MAX_WEB_AUDIT_EVENTS {
                state.audit_log.pop_front();
                state.audit_events_dropped = state.audit_events_dropped.saturating_add(1);
            }
            state.audit_log.push_back(event);
        }
    }
}

fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_TOKEN_BYTES
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Legacy decorative pairing marker. This fixed pattern does not encode a QR
/// payload and is not used by the live server's copyable-link pairing interface.
pub fn render_half_block_qr(data: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    out.push_str("┌───────────────────────────┐\n");
    out.push_str("│ █▀▀▀▀▀█ ▄▄█▄▄▄▄ █▀▀▀▀▀█ │\n");
    out.push_str("│ █ ███ █ █ ▀▀▀█▀ █ ███ █ │\n");
    out.push_str("│ █▀▀▀▀▀█ █ █ █ █ █▀▀▀▀▀█ │\n");
    out.push_str("│ ▀▀▀▀▀▀▀ ▀ ▀ ▀ ▀ ▀▀▀▀▀▀▀ │\n");
    out.push_str("│ ▀▄█▄▀█▀▄█▀▀█▄▀▄█▄▀█▀▄█▀ │\n");
    out.push_str("│ █▀▀▀▀▀█ ▀█▀█▀██ ▄▀▄▀▄▀▄ │\n");
    out.push_str("│ █ ███ █ █▀█▀▄▀█ █▀▀▀▀▀█ │\n");
    out.push_str("│ █▀▀▀▀▀█ ▀▄▀▄▀▄█ █ ███ █ │\n");
    out.push_str("└───────────────────────────┘\n");
    let _ = writeln!(out, "Payload: {data}");
    out
}

fn current_time_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Browser projection of the native terminal. Scripts are served separately
/// so the HTTP Content-Security-Policy does not need inline script execution.
pub const EMBEDDED_WEB_CLIENT_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta name="referrer" content="no-referrer">
    <title>Pi · Remote terminal</title>
    <style>
        :root { color-scheme: dark; --ink: #e7edf4; --muted: #a5b4c6; --edge: #34445a; }
        * { box-sizing: border-box; }
        html, body { height: 100%; margin: 0; background: #101720; color: var(--ink); }
        body { display: flex; flex-direction: column; font: 14px system-ui, sans-serif; }
        header { display: flex; gap: 16px; align-items: center; flex-wrap: wrap;
            padding: 14px 18px; border-bottom: 1px solid var(--edge); background: #17212e; }
        h1 { font-size: 16px; margin: 0; letter-spacing: .02em; }
        #conn-status { flex: 1; min-width: 160px; color: var(--muted); }
        nav { display: flex; align-items: center; flex-wrap: wrap; gap: 8px; }
        button { font: inherit; color: var(--ink); background: #243348; border: 1px solid #52647d;
            border-radius: 5px; padding: 6px 10px; cursor: pointer; }
        button:hover:enabled { background: #354b66; }
        button:disabled { opacity: .45; cursor: default; }
        button:focus-visible, #canvas-grid:focus-visible { outline: 2px solid #7bc4ff; outline-offset: 2px; }
        #abort { border-color: #a3676e; }
        main { flex: 1; min-height: 0; overflow: auto; }
        #canvas-grid { margin: 0; min-height: 100%; min-width: max-content; padding: 16px;
            font: 14px/1.25 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
            white-space: pre; tab-size: 4; }
        footer { padding: 9px 18px; color: var(--muted); border-top: 1px solid var(--edge);
            font-size: 12px; display: flex; justify-content: space-between; gap: 12px; }
        #message { color: #ffcc89; }
        [hidden] { display: none !important; }
    </style>
    <script src="/client.js" defer></script>
</head>
<body>
    <header>
        <h1>Pi · Remote terminal</h1>
        <span id="conn-status" role="status">Connecting…</span>
        <nav aria-label="Remote terminal controls">
            <button id="takeover" type="button" disabled>Request control</button>
            <button id="release" type="button" disabled>Release control</button>
            <button id="abort" type="button" disabled>Stop turn</button>
            <button id="smaller" type="button" aria-label="Smaller terminal text">−</button>
            <button id="larger" type="button" aria-label="Larger terminal text">+</button>
        </nav>
    </header>
    <main><pre id="canvas-grid" tabindex="0" aria-label="Live native terminal">Waiting for the host terminal…</pre></main>
    <footer>
        <span id="message" role="status">A host operator must approve remote control. Local approval dialogs stay on the host.</span>
        <span id="dimensions"></span>
    </footer>
</body>
</html>
"#;

/// No storage, analytics, external resources, or credential-bearing request URL.
pub const EMBEDDED_WEB_CLIENT_JS: &str = r#"'use strict';
(() => {
    const grid = document.getElementById('canvas-grid');
    const status = document.getElementById('conn-status');
    const notice = document.getElementById('message');
    const dimensions = document.getElementById('dimensions');
    const takeover = document.getElementById('takeover');
    const release = document.getElementById('release');
    const abort = document.getElementById('abort');
    const encoder = new TextEncoder();
    const decoder = new TextDecoder('utf-8', {fatal: true});
    let token = new URLSearchParams(window.location.hash.slice(1)).get('token');
    window.history.replaceState(null, '', window.location.pathname);
    let connected = false;
    let controller = false;
    let viewOnly = true;
    let inputSequence = 0;
    let frameSequence = 0;
    let frameWidth = 0;
    let frameHeight = 0;
    let lines = [];
    let fontSize = 14;
    let resyncPending = false;
    const buttons = () => {
        takeover.disabled = !connected || viewOnly || controller;
        release.disabled = !connected || !controller;
        abort.disabled = !connected || !controller;
    };
    if (!token || token.length > 1024) {
        status.textContent = 'Pairing link required';
        notice.textContent = 'Open a fresh link from the host terminal’s F8 panel.';
        return;
    }
    const address = new URL('/ws', window.location.href);
    address.protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
    const socket = new WebSocket(address);
    socket.binaryType = 'arraybuffer';
    const send = (value) => {
        if (socket.readyState !== WebSocket.OPEN || socket.bufferedAmount > 65536) return false;
        const data = JSON.stringify(value);
        if (encoder.encode(data).length > 32768) {
            notice.textContent = 'Input is too large for one remote message.';
            return false;
        }
        socket.send(data);
        return true;
    };
    const input = (value) => {
        if (!connected || !controller || inputSequence >= Number.MAX_SAFE_INTEGER) return false;
        const next = inputSequence + 1;
        if (!send({type: 'input', seq: next, input: value})) {
            notice.textContent = 'Connection is busy. Wait before sending more input.';
            return false;
        }
        inputSequence = next;
        return true;
    };
    const resync = () => {
        if (!resyncPending && send({type: 'resync'})) resyncPending = true;
    };
    const applyFrame = (frame) => {
        if (!Number.isSafeInteger(frame.seq) || frame.seq <= 0 ||
            !Number.isInteger(frame.width) || frame.width < 1 || frame.width > 1000 ||
            !Number.isInteger(frame.height) || frame.height < 1 || frame.height > 500 ||
            typeof frame.data !== 'string' || frame.data.length > 1048576) {
            throw new Error('invalid frame');
        }
        if (frame.frame_type === 'keyframe') {
            if (frame.seq < frameSequence) return;
            lines = frame.data.split('\n');
            if (lines.length > 501) throw new Error('oversized frame');
            frameWidth = frame.width;
            frameHeight = frame.height;
            resyncPending = false;
        } else if (frame.frame_type === 'patch') {
            const patch = JSON.parse(frame.data);
            if (patch.base_seq !== frameSequence || frame.width !== frameWidth ||
                frame.height !== frameHeight || frame.seq <= frameSequence) {
                resync();
                return;
            }
            if (!Array.isArray(patch.lines) || patch.lines.length > 501) throw new Error('invalid patch');
            const next = lines.slice();
            for (const change of patch.lines) {
                if (!Array.isArray(change) || change.length !== 2 ||
                    !Number.isInteger(change[0]) || change[0] < 0 || change[0] >= next.length ||
                    typeof change[1] !== 'string' || change[1].includes('\n')) throw new Error('invalid patch');
                next[change[0]] = change[1];
            }
            lines = next;
        } else {
            return;
        }
        const text = lines.join('\n');
        if (encoder.encode(text).length > 1048576) throw new Error('oversized frame');
        grid.textContent = text;
        frameSequence = frame.seq;
        dimensions.textContent = `${frameWidth} × ${frameHeight}`;
        send({type: 'ack', seq: frameSequence});
    };
    socket.onopen = () => {
        send({type: 'hello', token});
        token = null;
        status.textContent = 'Authenticating…';
    };
    socket.onmessage = (event) => {
        try {
            if (event.data instanceof ArrayBuffer && event.data.byteLength > 6295552) throw new Error('oversized message');
            const data = JSON.parse(typeof event.data === 'string' ? event.data : decoder.decode(event.data));
            if (data.schema === 'pi.web.frame.v1') {
                applyFrame(data);
            } else if (data.type === 'hello') {
                connected = true;
                viewOnly = data.view_only !== false;
                status.textContent = viewOnly ? 'Live · View only' : 'Live · Host has control';
                notice.textContent = viewOnly ? 'This link permits viewing only.' : 'Request control, then wait for the host to approve.';
                buttons();
            } else if (data.type === 'control') {
                viewOnly = data.view_only !== false;
                controller = data.is_controller === true && !viewOnly;
                status.textContent = controller ? 'Live · You have control' :
                    data.mode === 'takeover_pending_approval' ? 'Waiting for host approval' :
                    viewOnly ? 'Live · View only' : 'Live · Host has control';
                notice.textContent = controller ? 'Click the terminal to type. Approval dialogs and privileged commands stay local.' :
                    'The host can reclaim control at any time.';
                buttons();
                if (controller) grid.focus();
            } else if (data.type === 'error') {
                notice.textContent = 'Request refused by the host. Check control, input limits, and local approval state.';
            }
        } catch (_) {
            notice.textContent = 'Invalid frame received. Reconnecting requires a fresh pairing link.';
            socket.close(1002, 'invalid message');
        }
    };
    socket.onclose = () => {
        connected = false;
        controller = false;
        token = null;
        buttons();
        status.textContent = 'Disconnected';
        notice.textContent = 'Pairing links are single-use. Open a new link from the host terminal’s F8 panel.';
    };
    socket.onerror = () => { status.textContent = 'Connection error'; };
    takeover.onclick = () => { if (send({type: 'takeover'})) notice.textContent = 'Approve the request in the host terminal’s F8 panel.'; };
    release.onclick = () => {
        if (send({type: 'release'})) {
            controller = false;
            buttons();
        } else {
            notice.textContent = 'Connection is busy. Wait before releasing control.';
        }
    };
    abort.onclick = () => { input({kind: 'abort'}); };
    const namedKeys = new Set(['Enter', 'Backspace', 'Delete', 'Tab', 'Escape', 'ArrowUp', 'ArrowDown',
        'ArrowLeft', 'ArrowRight', 'Home', 'End', 'PageUp', 'PageDown']);
    grid.addEventListener('keydown', (event) => {
        if (!controller || event.metaKey || event.isComposing) return;
        if (event.ctrlKey && !event.altKey && event.key.toLowerCase() === 'v') return;
        if (event.shiftKey && event.key === 'Insert') return;
        if (event.ctrlKey && event.key.toLowerCase() === 'c' && String(window.getSelection())) return;
        if (!namedKeys.has(event.key) && Array.from(event.key).length !== 1) return;
        if (input({kind: 'key', key: event.key, ctrl: event.ctrlKey, alt: event.altKey, shift: event.shiftKey})) event.preventDefault();
    });
    grid.addEventListener('paste', (event) => {
        if (!controller) return;
        event.preventDefault();
        const text = event.clipboardData.getData('text/plain');
        if (encoder.encode(text).length > 16384) { notice.textContent = 'Paste is limited to 16 KiB.'; return; }
        input({kind: 'paste', text});
    });
    document.getElementById('smaller').onclick = () => { fontSize = Math.max(9, fontSize - 1); grid.style.fontSize = `${fontSize}px`; };
    document.getElementById('larger').onclick = () => { fontSize = Math.min(28, fontSize + 1); grid.style.fontSize = `${fontSize}px`; };
    window.addEventListener('pagehide', () => { socket.close(1000, 'viewer closed'); });
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_web_remote_token_lifecycle_and_capacity() {
        let settings = WebRemoteSettings {
            port: 8080,
            bind_mode: BindMode::Loopback,
            view_only: false,
            max_viewers: 2,
            require_auth_token: true,
            enable_audit_log: true,
        };

        let manager = WebRemoteManager::new(settings);
        manager
            .issue_token("secret-tok-1", TokenKind::Steer)
            .unwrap();
        manager
            .issue_token("secret-tok-2", TokenKind::View)
            .unwrap();

        // Client 1 connects with valid steer token
        let c1 = manager.connect_client("client-1", "127.0.0.1:50001", Some("secret-tok-1"));
        assert!(c1.is_ok());
        assert!(c1.as_ref().is_ok_and(|c| !c.is_view_only));

        // Token is consumed, cannot reuse
        let c1_retry =
            manager.connect_client("client-1-dup", "127.0.0.1:50001", Some("secret-tok-1"));
        assert!(c1_retry.is_err());

        // Client 2 connects with view token
        let c2 = manager.connect_client("client-2", "127.0.0.1:50002", Some("secret-tok-2"));
        assert!(c2.is_ok());
        assert!(c2.as_ref().is_ok_and(|c| c.is_view_only));

        // Client 3 hits capacity
        manager
            .issue_token("secret-tok-3", TokenKind::Steer)
            .unwrap();
        let c3 = manager.connect_client("client-3", "127.0.0.1:50003", Some("secret-tok-3"));
        assert!(c3.is_err());
        assert!(
            c3.as_ref()
                .err()
                .is_some_and(|e| e.contains("maximum viewer capacity"))
        );
        manager.disconnect_client("client-2");
        assert!(
            manager
                .connect_client("client-3", "127.0.0.1:50003", Some("secret-tok-3"))
                .is_ok(),
            "capacity rejection must not consume the pairing token"
        );
    }

    #[test]
    fn test_web_remote_input_arbitration_and_audit() {
        let settings = WebRemoteSettings {
            port: 8080,
            bind_mode: BindMode::Loopback,
            view_only: false,
            max_viewers: 4,
            require_auth_token: false,
            enable_audit_log: true,
        };

        let manager = WebRemoteManager::new(settings);
        let _ = manager.connect_client("client-a", "127.0.0.1:50001", None);
        let _ = manager.connect_client("client-b", "127.0.0.1:50002", None);

        // Client A requests takeover
        let mode_a = manager.request_takeover("client-a");
        assert_eq!(mode_a, Ok(ControlMode::TakeoverPendingApproval));
        assert!(manager.authorize_input("client-a", 1).is_err());
        assert_eq!(
            manager.approve_takeover("client-a"),
            Ok(ControlMode::RemoteControlling)
        );

        // Client B requests takeover while A has control
        let mode_b = manager.request_takeover("client-b");
        assert_eq!(mode_b, Ok(ControlMode::TakeoverPendingApproval));

        // Client A releases control
        manager.release_control("client-a");

        // Verify audit trail
        let audit = manager.audit_log();
        assert!(audit.iter().any(|e| e.event_type == "client_connected"));
        assert!(audit.iter().any(|e| e.event_type == "takeover_granted"));
        assert!(audit.iter().any(|e| e.event_type == "takeover_requested"));
        assert!(
            audit
                .iter()
                .any(|e| e.event_type == "control_released_to_local")
        );
    }

    fn approved_controller() -> (WebRemoteManager, String) {
        let manager = WebRemoteManager::new(WebRemoteSettings::default());
        let token = "controller-private-fixture".to_string();
        manager
            .issue_token(token.clone(), TokenKind::Steer)
            .unwrap();
        manager
            .connect_client("controller", "127.0.0.1:4010", Some(&token))
            .unwrap();
        assert_eq!(
            manager.request_takeover("controller"),
            Ok(ControlMode::TakeoverPendingApproval)
        );
        manager.approve_takeover("controller").unwrap();
        (manager, token)
    }

    #[test]
    fn first_takeover_is_pending_and_local_denial_cannot_be_replayed() {
        let manager = WebRemoteManager::new(WebRemoteSettings::default());
        for (id, kind) in [("steer", TokenKind::Steer), ("viewer", TokenKind::View)] {
            let token = manager.generate_token(kind).unwrap();
            manager
                .connect_client(id, "127.0.0.1:4000", Some(&token))
                .unwrap();
        }
        assert_eq!(
            manager.request_takeover("steer"),
            Ok(ControlMode::TakeoverPendingApproval)
        );
        manager.request_takeover("steer").unwrap();
        assert_eq!(manager.pending_takeovers(), vec!["steer".to_string()]);
        assert!(manager.clients().iter().all(|client| !client.is_controller));
        assert!(manager.authorize_input("steer", 1).is_err());
        assert!(manager.request_takeover("viewer").is_err());
        assert!(manager.approve_takeover("viewer").is_err());
        assert!(manager.authorize_input("viewer", 1).is_err());
        assert!(manager.deny_takeover("steer"));
        assert!(!manager.deny_takeover("steer"));
        assert!(manager.pending_takeovers().is_empty());
        assert!(manager.approve_takeover("steer").is_err());
        manager.request_takeover("steer").unwrap();
        manager.approve_takeover("steer").unwrap();
        let epoch = manager.authorize_input("steer", 1).unwrap();
        assert!(manager.input_authorized("steer", epoch));
        assert!(manager.clients().iter().any(|client| {
            client.client_id == "steer" && client.is_controller && !client.is_view_only
        }));
        manager.revoke_control();
        assert!(!manager.input_authorized("steer", epoch));
        assert!(manager.clients().iter().all(|client| !client.is_controller));
    }

    #[test]
    fn input_replays_stay_rejected_across_control_generations() {
        let (manager, _) = approved_controller();
        assert!(manager.authorize_input("controller", 0).is_err());
        let first = manager.authorize_input("controller", 1).unwrap();
        assert!(manager.authorize_input("controller", 1).is_err());
        assert_eq!(manager.authorize_input("controller", 2).unwrap(), first);
        assert!(manager.authorize_input("controller", 1).is_err());
        manager.release_control("controller");
        assert!(!manager.input_authorized("controller", first));
        assert!(manager.authorize_input("controller", 3).is_err());
        manager.request_takeover("controller").unwrap();
        manager.approve_takeover("controller").unwrap();
        assert!(manager.authorize_input("controller", 2).is_err());
        let second = manager.authorize_input("controller", 3).unwrap();
        assert!(second > first);
        assert!(!manager.input_authorized("controller", first));
        assert!(manager.input_authorized("controller", second));
    }

    #[test]
    fn unrelated_viewer_departures_preserve_the_controllers_draft_authority() {
        let (manager, controller_token) = approved_controller();
        let epoch = manager.authorize_input("controller", 1).unwrap();
        for id in ["disconnecting-viewer", "revoked-viewer"] {
            let token = manager.generate_token(TokenKind::View).unwrap();
            manager
                .connect_client(id, "127.0.0.1:4011", Some(&token))
                .unwrap();
            if id == "disconnecting-viewer" {
                manager.disconnect_client(id);
            } else {
                assert!(manager.revoke_token(&token));
            }
            assert!(manager.input_authorized("controller", epoch));
        }
        assert_eq!(manager.authorize_input("controller", 2).unwrap(), epoch);
        assert!(manager.revoke_token(&controller_token));
        assert!(!manager.input_authorized("controller", epoch));
    }

    #[test]
    fn revocation_and_disconnect_invalidate_already_queued_inputs() {
        for action in ["release", "local", "disconnect", "credential"] {
            let (manager, token) = approved_controller();
            let epoch = manager.authorize_input("controller", 1).unwrap();
            match action {
                "release" => manager.release_control("controller"),
                "local" => manager.revoke_control(),
                "disconnect" => manager.disconnect_client("controller"),
                "credential" => assert!(manager.revoke_token(&token)),
                _ => unreachable!(),
            }
            assert!(!manager.input_authorized("controller", epoch), "{action}");
            assert!(
                manager.authorize_input("controller", 2).is_err(),
                "{action}"
            );
            if matches!(action, "disconnect" | "credential") {
                assert!(manager.clients().is_empty());
                assert!(manager.pending_takeovers().is_empty());
                assert!(manager.approve_takeover("controller").is_err());
                assert!(
                    manager
                        .connect_client("replay", "127.0.0.1:4011", Some(&token))
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn generated_tokens_have_256_random_bits_and_never_enter_debug_or_audit() {
        let manager = WebRemoteManager::new(WebRemoteSettings::default());
        let mut tokens = std::collections::HashSet::new();
        for _ in 0..16 {
            let token = manager.generate_token(TokenKind::Steer).unwrap();
            let bytes =
                base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &token)
                    .unwrap();
            assert_eq!(bytes.len(), 32);
            assert!(tokens.insert(token.clone()));
            assert!(manager.issue_token(token, TokenKind::View).is_err());
        }
        let record = manager
            .issue_token("fixture-private-record", TokenKind::View)
            .unwrap();
        assert!(!format!("{record:?}").contains("fixture-private-record"));
        let audit = serde_json::to_string(&manager.audit_log()).unwrap();
        let debug = format!("{manager:?}");
        for token in tokens {
            assert!(!audit.contains(&token));
            assert!(!debug.contains(&token));
        }
    }

    #[test]
    fn token_deadlines_and_bounded_retention_use_monotonic_time() {
        let manager = WebRemoteManager::new(WebRemoteSettings::default());
        let now = Instant::now();
        manager
            .issue_token_at(
                "boundary-token".to_string(),
                TokenKind::Steer,
                now,
                u64::MAX,
            )
            .unwrap();
        assert!(
            manager
                .connect_client_at(
                    "expired",
                    "127.0.0.1:4000",
                    Some("boundary-token"),
                    now + TOKEN_LIFETIME,
                )
                .unwrap_err()
                .contains("expired")
        );
        assert!(
            manager
                .issue_token_at("boundary-token".to_string(), TokenKind::Steer, now, 0,)
                .is_err(),
            "reissuing an existing record cannot reset its expiry"
        );
        for index in 1..MAX_WEB_TOKEN_RECORDS {
            manager
                .issue_token_at(format!("bounded-{index}"), TokenKind::View, now, 0)
                .unwrap();
        }
        assert!(manager.generate_token(TokenKind::View).is_err());
        manager
            .issue_token_at(
                "after-expiry".to_string(),
                TokenKind::View,
                now + TOKEN_LIFETIME,
                0,
            )
            .unwrap();
        assert_eq!(
            manager.tokens_for_redaction_at(now + TOKEN_LIFETIME),
            vec!["after-expiry".to_string()]
        );
    }

    #[test]
    fn connected_credentials_remain_redactable_and_revocable_after_admission_expiry() {
        let manager = WebRemoteManager::new(WebRemoteSettings::default());
        let now = Instant::now();
        manager
            .issue_token_at("active-private-token".to_string(), TokenKind::Steer, now, 0)
            .unwrap();
        manager
            .connect_client_at(
                "active",
                "127.0.0.1:4010",
                Some("active-private-token"),
                now,
            )
            .unwrap();
        manager.request_takeover("active").unwrap();
        manager.approve_takeover("active").unwrap();
        let epoch = manager.authorize_input("active", 1).unwrap();
        assert_eq!(
            manager.tokens_for_redaction_at(now + TOKEN_LIFETIME),
            vec!["active-private-token".to_string()]
        );
        assert!(
            manager
                .issue_token("active-private-token", TokenKind::View)
                .is_err()
        );
        assert!(manager.revoke_token("active-private-token"));
        assert!(!manager.input_authorized("active", epoch));
        assert!(manager.clients().is_empty());
    }

    #[test]
    fn simultaneous_pairing_attempts_consume_one_credential_once() {
        let manager = WebRemoteManager::new(WebRemoteSettings::default());
        let token = manager.generate_token(TokenKind::Steer).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers = ["first", "second"].map(|id| {
            let manager = manager.clone();
            let token = token.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                manager
                    .connect_client(id, "127.0.0.1:4010", Some(&token))
                    .is_ok()
            })
        });
        barrier.wait();
        let admitted = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(admitted, 1);
        assert_eq!(manager.clients().len(), 1);
        assert_eq!(
            manager
                .audit_log()
                .iter()
                .filter(|event| event.event_type == "token_consumed")
                .count(),
            1
        );
    }

    #[test]
    fn audit_retention_is_bounded_and_dropped_history_is_visible() {
        let manager = WebRemoteManager::new(WebRemoteSettings::default());
        for _ in 0..MAX_WEB_AUDIT_EVENTS + 8 {
            manager.revoke_control();
        }
        assert_eq!(manager.audit_log().len(), MAX_WEB_AUDIT_EVENTS);
        assert_eq!(manager.dropped_audit_events(), 8);
    }

    #[test]
    fn live_settings_require_authentication_audit_and_bounded_viewers() {
        assert!(WebRemoteSettings::default().validate().is_ok());
        for max_viewers in [0, MAX_WEB_VIEWERS + 1, usize::MAX] {
            let settings = WebRemoteSettings {
                max_viewers,
                ..WebRemoteSettings::default()
            };
            assert!(settings.validate().is_err());
        }
        let settings = WebRemoteSettings {
            require_auth_token: false,
            ..WebRemoteSettings::default()
        };
        assert!(settings.validate().is_err());
        let settings = WebRemoteSettings {
            enable_audit_log: false,
            ..WebRemoteSettings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn test_half_block_qr_rendering() {
        let qr = render_half_block_qr("http://127.0.0.1:8080/#t=tok123");
        assert!(qr.contains("▀"));
        assert!(qr.contains("▄"));
        assert!(qr.contains("█"));
        assert!(qr.contains("http://127.0.0.1:8080/#t=tok123"));
    }
}
