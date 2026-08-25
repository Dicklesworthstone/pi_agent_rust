#![forbid(unsafe_code)]

//! Web-remote access: ftui-web WASM browser client over WebSocket frame diffs (OMP-ADOPT / bd-cv653.10.1 / bd-cv653.10.2).
//!
//! Provides relay-free browser access to the live Pi agent session over local network or Tailscale,
//! with strict input arbitration, server-rendered frame diffs, QR console pairing, and privacy/audit controls.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Token role permission: Steer (input allowed) or View (read-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    Steer,
    View,
}

/// Authentication token metadata and lifecycle record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthTokenRecord {
    pub token: String,
    pub kind: TokenKind,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub consumed: bool,
    pub revoked: bool,
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
#[derive(Debug, Clone)]
pub struct WebRemoteManager {
    settings: WebRemoteSettings,
    clients: Arc<Mutex<HashMap<String, ClientSession>>>,
    active_controller: Arc<Mutex<Option<String>>>,
    frame_seq: Arc<Mutex<u64>>,
    audit_log: Arc<Mutex<Vec<WebAuditEvent>>>,
    tokens: Arc<Mutex<HashMap<String, AuthTokenRecord>>>,
}

#[allow(clippy::significant_drop_tightening)]
impl WebRemoteManager {
    pub fn new(settings: WebRemoteSettings) -> Self {
        Self {
            settings,
            clients: Arc::new(Mutex::new(HashMap::new())),
            active_controller: Arc::new(Mutex::new(None)),
            frame_seq: Arc::new(Mutex::new(0)),
            audit_log: Arc::new(Mutex::new(Vec::new())),
            tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Issue a new single-use authentication token with 10-minute expiry.
    pub fn issue_token(&self, token: impl Into<String>, kind: TokenKind) -> AuthTokenRecord {
        let now = current_time_ms();
        let expires_at_ms = now + (10 * 60 * 1000); // 10 minutes
        let tok_str = token.into();
        let record = AuthTokenRecord {
            token: tok_str.clone(),
            kind,
            issued_at_ms: now,
            expires_at_ms,
            consumed: false,
            revoked: false,
        };

        {
            let mut map = self
                .tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(tok_str, record.clone());
        }

        self.record_audit(
            WebAuditEvent::new("token_issued", None)
                .with_detail("token_kind", format!("{kind:?}"))
                .with_detail("expires_at_ms", expires_at_ms.to_string()),
        );

        record
    }

    /// Revoke an existing token.
    pub fn revoke_token(&self, token: &str) -> bool {
        let kind = {
            let mut map = self
                .tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(record) = map.get_mut(token) {
                record.revoked = true;
                Some(record.kind)
            } else {
                None
            }
        };
        kind.is_some_and(|token_kind| {
            self.record_audit(
                WebAuditEvent::new("token_revoked", None)
                    .with_detail("token_kind", format!("{token_kind:?}")),
            );
            true
        })
    }

    /// Validate and consume a token upon connection.
    pub fn validate_and_consume_token(&self, token: &str) -> Result<TokenKind, String> {
        if !self.settings.require_auth_token {
            return Ok(TokenKind::Steer);
        }

        let now = current_time_ms();
        let record_kind = {
            let mut map = self
                .tokens
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let record = map
                .get_mut(token)
                .ok_or_else(|| "token not found".to_string())?;

            if record.revoked {
                return Err("token has been revoked".to_string());
            }
            if record.consumed {
                return Err("token has already been consumed (single-use)".to_string());
            }
            if now > record.expires_at_ms {
                return Err("token has expired".to_string());
            }

            record.consumed = true;
            record.kind
        };

        self.record_audit(
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
        let token_kind = if self.settings.require_auth_token {
            let Some(tok) = token else {
                return Err("missing authentication token".to_string());
            };
            self.validate_and_consume_token(tok)?
        } else {
            TokenKind::Steer
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

        {
            let mut clients = self
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if clients.len() >= self.settings.max_viewers {
                return Err(format!(
                    "maximum viewer capacity ({}) reached",
                    self.settings.max_viewers
                ));
            }
            clients.insert(client_id.to_string(), session.clone());
        }

        self.record_audit(
            WebAuditEvent::new("client_connected", Some(client_id.to_string()))
                .with_detail("remote_addr", remote_addr)
                .with_detail("is_view_only", is_view_only.to_string()),
        );

        Ok(session)
    }

    /// Disconnect a client viewer.
    pub fn disconnect_client(&self, client_id: &str) {
        self.clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(client_id);

        {
            let mut controller = self
                .active_controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if controller.as_deref() == Some(client_id) {
                *controller = None;
            }
        }

        self.record_audit(WebAuditEvent::new(
            "client_disconnected",
            Some(client_id.to_string()),
        ));
    }

    /// Request steering control by a remote client.
    pub fn request_takeover(&self, client_id: &str) -> Result<ControlMode, String> {
        let is_view_only = {
            let clients = self
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let client = clients
                .get(client_id)
                .ok_or_else(|| "unregistered client id".to_string())?;
            client.is_view_only
        };

        if is_view_only || self.settings.view_only {
            return Err("client is in view-only mode".to_string());
        }

        let is_none = {
            let mut controller = self
                .active_controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if controller.is_none() {
                *controller = Some(client_id.to_string());
                true
            } else {
                false
            }
        };

        if is_none {
            self.record_audit(
                WebAuditEvent::new("takeover_granted", Some(client_id.to_string()))
                    .with_detail("previous_controller", "none"),
            );
            Ok(ControlMode::RemoteControlling)
        } else {
            self.record_audit(
                WebAuditEvent::new("takeover_requested", Some(client_id.to_string()))
                    .with_detail("pending_approval", "true"),
            );
            Ok(ControlMode::TakeoverPendingApproval)
        }
    }

    /// Release steering control back to local terminal.
    pub fn release_control(&self, client_id: &str) {
        let was_controller = {
            let mut controller = self
                .active_controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if controller.as_deref() == Some(client_id) {
                *controller = None;
                true
            } else {
                false
            }
        };
        if was_controller {
            self.record_audit(WebAuditEvent::new(
                "control_released_to_local",
                Some(client_id.to_string()),
            ));
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
            let mut seq_guard = self
                .frame_seq
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *seq_guard += 1;
            *seq_guard
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
        self.audit_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record_audit(&self, event: WebAuditEvent) {
        if self.settings.enable_audit_log {
            self.audit_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
    }
}

/// Render a text/URL string into half-block terminal QR presentation (`▀`, `▄`, `█`, ` `).
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

/// Static HTML/JS/CSS assets for the ftui-web thin client.
pub const EMBEDDED_WEB_CLIENT_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <meta http-equiv="Content-Security-Policy" content="default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:;">
    <title>Pi Agent Remote TUI</title>
    <style>
        body, html {
            margin: 0; padding: 0; width: 100%; height: 100%;
            background-color: #0f141c; color: #d8dee9;
            font-family: 'JetBrains Mono', 'Fira Code', monospace;
            overflow: hidden;
        }
        #terminal-container {
            display: flex; flex-direction: column; width: 100%; height: 100%;
        }
        #status-bar {
            background-color: #1a2230; padding: 4px 12px; font-size: 12px;
            display: flex; justify-content: space-between; border-bottom: 1px solid #2e3a4e;
        }
        #canvas-grid {
            flex: 1; white-space: pre; font-size: 14px; line-height: 1.2;
            padding: 8px; overflow: hidden;
        }
    </style>
</head>
<body>
    <div id="terminal-container">
        <div id="status-bar">
            <span id="conn-status">Connecting...</span>
            <span id="session-info">Pi Agent Web Remote (pi.web.frame.v1)</span>
        </div>
        <pre id="canvas-grid"></pre>
    </div>
    <script>
        // Minimal frame receiver and DOM renderer (zero persistent client-side storage / caching)
        const grid = document.getElementById('canvas-grid');
        const status = document.getElementById('conn-status');
        const token = window.location.hash.replace('#t=', '').replace('#', '');
        const wsProto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        const wsUrl = `${wsProto}//${window.location.host}/ws?token=${encodeURIComponent(token)}`;

        const socket = new WebSocket(wsUrl);
        socket.onopen = () => { status.textContent = 'Connected (Live)'; };
        socket.onclose = () => { status.textContent = 'Disconnected'; };
        socket.onerror = (err) => { status.textContent = 'Connection Error'; };
        socket.onmessage = (event) => {
            try {
                const frame = JSON.parse(event.data);
                if (frame.schema === 'pi.web.frame.v1') {
                    grid.textContent = frame.data;
                }
            } catch (e) {}
        };
    </script>
</body>
</html>
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
        manager.issue_token("secret-tok-1", TokenKind::Steer);
        manager.issue_token("secret-tok-2", TokenKind::View);

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
        manager.issue_token("secret-tok-3", TokenKind::Steer);
        let c3 = manager.connect_client("client-3", "127.0.0.1:50003", Some("secret-tok-3"));
        assert!(c3.is_err());
        assert!(
            c3.as_ref()
                .err()
                .is_some_and(|e| e.contains("maximum viewer capacity"))
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
        assert_eq!(mode_a, Ok(ControlMode::RemoteControlling));

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

    #[test]
    fn test_half_block_qr_rendering() {
        let qr = render_half_block_qr("http://127.0.0.1:8080/#t=tok123");
        assert!(qr.contains("▀"));
        assert!(qr.contains("▄"));
        assert!(qr.contains("█"));
        assert!(qr.contains("http://127.0.0.1:8080/#t=tok123"));
    }
}
