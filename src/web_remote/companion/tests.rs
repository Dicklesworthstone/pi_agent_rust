//! Exercise the real owned listener, not an injected authorization state.

use super::*;
use crate::model::{AssistantMessage, ContentBlock, Message, TextContent, UserContent, UserMessage};
use crate::text_completion::{AuxiliaryPrivacy, AuxiliaryPrivacySnapshot};
use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::net::TcpStream;

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("JSON browser response")
    }
}

fn exchange(handle: &CompanionHandle, request: &[u8]) -> Reply {
    let mut socket = TcpStream::connect_timeout(&handle.address, Duration::from_secs(5)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    socket.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
    socket.write_all(request).unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    let (headers, body) = response.split_once("\r\n\r\n").expect("complete HTTP response");
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply { status, headers: headers.to_string(), body: body.to_string() }
}

fn post(handle: &CompanionHandle, path: &str, credential: Option<&str>, body: Value) -> Reply {
    let body = body.to_string();
    let authorization = credential.map_or_else(String::new, |credential| {
        format!("Authorization: Bearer {credential}\r\n")
    });
    exchange(
        handle,
        format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nContent-Type: application/json\r\nSec-Fetch-Site: same-origin\r\n{authorization}Content-Length: {}\r\n\r\n{body}",
            handle.address, handle.address, body.len(),
        ).as_bytes(),
    )
}

fn server(max_viewers: usize) -> (CompanionServer, CompanionHandle) {
    let server = CompanionServer::start(WebRemoteSettings {
        port: 0,
        max_viewers,
        ..WebRemoteSettings::default()
    }).unwrap();
    let handle = server.handle();
    handle.publish(
        "private-session-A".to_string(),
        r#"{"name":"Fixture session","model":"fixture","messages":[],"omitted":false}"#.to_string(),
    );
    handle.set_busy(false);
    (server, handle)
}

fn token(handle: &CompanionHandle, kind: TokenKind) -> String {
    handle.pairing_url(kind).unwrap().split_once("#token=").unwrap().1.to_string()
}

fn pair(handle: &CompanionHandle, kind: TokenKind) -> String {
    let reply = post(handle, "/api/pair", None, json!({"token": token(handle, kind)}));
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()["credential"].as_str().unwrap().to_string()
}

fn snapshot(handle: &CompanionHandle, credential: &str) -> Value {
    let reply = post(handle, "/api/snapshot", Some(credential), json!({"after":0}));
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()
}

pub(super) fn delivered_proposal(handle: &CompanionHandle, text: &str) -> PromptProposal {
    let credential = pair(handle, TokenKind::Steer);
    let current = snapshot(handle, &credential);
    assert_eq!(current["ready"], true);
    assert_eq!(current["busy"], false);
    let reply = post(handle, "/api/proposal", Some(&credential), json!({
        "generation": current["generation"], "text": text,
    }));
    assert_eq!(reply.status, 202, "{}", reply.body);
    handle.take_proposal().expect("HTTP proposal delivered once")
}

#[test]
fn pairing_is_single_use_and_capacity_refusal_does_not_consume_a_link() {
    let (_server, handle) = server(1);
    let first = pair(&handle, TokenKind::View);
    let second_token = token(&handle, TokenKind::Steer);
    let denied = post(&handle, "/api/pair", None, json!({"token":second_token}));
    assert_eq!(denied.status, 503);
    assert_eq!(post(&handle, "/api/leave", Some(&first), json!({})).status, 200);
    let admitted = post(&handle, "/api/pair", None, json!({"token":second_token}));
    assert_eq!(admitted.status, 200);
    assert_eq!(post(&handle, "/api/pair", None, json!({"token":second_token})).status, 401);
    assert_eq!(post(&handle, "/api/snapshot", Some(&first), json!({})).status, 401);
}

#[test]
fn exact_origin_and_host_are_required_before_a_pairing_link_can_be_consumed() {
    let (_server, handle) = server(4);
    let secret = token(&handle, TokenKind::Steer);
    let body = json!({"token":secret}).to_string();
    for (host, origin, fetch_site) in [
        ("attacker.example".to_string(), format!("http://{}", handle.address), "same-origin"),
        (handle.address.to_string(), "https://attacker.example".to_string(), "cross-site"),
        (handle.address.to_string(), "null".to_string(), "none"),
        (handle.address.to_string(), format!("http://{}", handle.address), "cross-site"),
    ] {
        let response = exchange(&handle, format!(
            "POST /api/pair HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nSec-Fetch-Site: {fetch_site}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len(),
        ).as_bytes());
        assert_eq!(response.status, 403);
        assert!(!response.body.contains(&secret));
    }
    assert_eq!(post(&handle, "/api/pair", None, json!({"token":secret})).status, 200);
}

#[test]
fn snapshots_are_authenticated_incremental_and_cannot_export_the_private_session_identity() {
    let (_server, handle) = server(4);
    assert_eq!(post(&handle, "/api/snapshot", None, json!({})).status, 401);
    let credential = pair(&handle, TokenKind::View);
    let current = snapshot(&handle, &credential);
    assert_eq!(current["view_only"], true);
    assert!(!current.to_string().contains("private-session-A"));
    assert!(!current.to_string().contains(&credential));
    let unchanged = post(&handle, "/api/snapshot", Some(&credential), json!({"after":current["seq"]}));
    assert_eq!(unchanged.status, 204);
    assert!(unchanged.body.is_empty());
    assert!(unchanged.headers.contains("Cache-Control: no-store"));
    assert!(unchanged.headers.contains("Referrer-Policy: no-referrer"));
    handle.set_busy(true);
    let changed = post(&handle, "/api/snapshot", Some(&credential), json!({"after":current["seq"]}));
    assert_eq!(changed.status, 200);
    assert_eq!(changed.json()["busy"], true);
    assert_eq!(post(&handle, "/api/proposal", Some(&credential), json!({
        "generation":current["generation"], "text":"Read-only clients cannot steer.",
    })).status, 403);
}

#[test]
fn proposals_require_local_delivery_and_explicit_admission_and_preserve_literal_text() {
    let (_server, handle) = server(4);
    let credential = pair(&handle, TokenKind::Steer);
    let current = snapshot(&handle, &credential);
    let literal = "Please explain @private.txt and {{template}} without expanding either.";
    assert_eq!(post(&handle, "/api/proposal", Some(&credential), json!({
        "generation":current["generation"], "text":literal,
    })).status, 202);
    let receipt = snapshot(&handle, &credential)["proposal"].clone();
    let id = receipt["id"].as_str().unwrap();
    assert!(!handle.resolve_proposal(id, true, "Not delivered yet"));
    let proposal = handle.take_proposal().unwrap();
    assert_eq!(proposal.text, literal);
    assert_eq!(proposal.owner_session_id, "private-session-A");
    assert!(handle.take_proposal().is_none());
    assert!(!handle.resolve_proposal("wrong-id", true, "Wrong proposal"));
    assert!(handle.resolve_proposal(id, true, "Accepted locally"));
    assert!(!handle.resolve_proposal(id, true, "Cannot replay admission"));
    let accepted = snapshot(&handle, &credential);
    assert_eq!(accepted["proposal"]["status"], "accepted");
    assert_eq!(accepted["busy"], true);
    assert!(!format!("{proposal:?}").contains(literal));
    assert!(!format!("{proposal:?}").contains("private-session-A"));
}

#[test]
fn revocation_replacement_disconnect_and_expiration_invalidate_delivered_proposals() {
    for cause in ["revoke", "replace", "disconnect", "expire"] {
        let (_server, handle) = server(4);
        let credential = pair(&handle, TokenKind::Steer);
        let current = snapshot(&handle, &credential);
        assert_eq!(post(&handle, "/api/proposal", Some(&credential), json!({
            "generation":current["generation"], "text":"Await local approval",
        })).status, 202);
        let proposal = handle.take_proposal().unwrap();
        match cause {
            "revoke" => handle.revoke_all(),
            "replace" => handle.publish("different-session".to_string(), "{}".to_string()),
            "disconnect" => assert_eq!(post(&handle, "/api/leave", Some(&credential), json!({})).status, 200),
            "expire" => {
                handle.shared.lock().unwrap().proposal.as_mut().unwrap().deadline =
                    Instant::now() - Duration::from_secs(1);
            }
            _ => unreachable!(),
        }
        assert!(!handle.resolve_proposal(&proposal.id, true, "Stale acceptance"), "{cause}");
        assert!(handle.take_proposal().is_none());
    }
}

#[test]
fn stale_busy_command_and_oversized_proposals_never_enter_the_local_queue() {
    let (_server, handle) = server(4);
    let credential = pair(&handle, TokenKind::Steer);
    let current = snapshot(&handle, &credential);
    for text in ["/login", "  !rm something", "\u{1b}[31m", "   "] {
        assert_eq!(post(&handle, "/api/proposal", Some(&credential), json!({
            "generation":current["generation"], "text":text,
        })).status, 422);
    }
    assert_eq!(post(&handle, "/api/proposal", Some(&credential), json!({
        "generation":current["generation"], "text":"x".repeat(MAX_PROMPT_BYTES + 1),
    })).status, 422);
    assert_eq!(post(&handle, "/api/proposal", Some(&credential), json!({
        "generation":0, "text":"stale session",
    })).status, 409);
    handle.set_busy(true);
    assert_eq!(post(&handle, "/api/proposal", Some(&credential), json!({
        "generation":current["generation"], "text":"local turn has priority",
    })).status, 409);
    assert!(handle.take_proposal().is_none());
}

#[test]
fn the_http_boundary_rejects_ambiguous_framing_and_does_not_echo_inputs() {
    let (_server, handle) = server(4);
    for headers in [
        "Content-Length: 0\r\nContent-Length: 0\r\n",
        "Transfer-Encoding: chunked\r\n",
        "Content-Length: +0\r\n",
        "Host: injected.example\r\n",
        "X-Bad: private-canary\u{0001}\r\n",
    ] {
        let reply = exchange(&handle, format!(
            "GET / HTTP/1.1\r\nHost: {}\r\n{headers}\r\n", handle.address,
        ).as_bytes());
        assert_eq!(reply.status, 400);
        assert!(!reply.body.contains("private-canary"));
    }
    let reply = exchange(&handle, format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", handle.address).as_bytes());
    assert_eq!(reply.status, 200);
    assert!(reply.headers.contains("script-src 'self'"));
    assert!(reply.headers.contains("frame-ancestors 'none'"));
    assert!(reply.body.contains("/client.js"));
    assert!(!reply.body.contains("private-session-A"));
    assert!(!reply.body.contains("WebSocket"));
}

#[test]
fn dropping_the_owner_stops_partial_sockets_and_revokes_retained_handles() {
    let (server, handle) = server(4);
    let mut partial = TcpStream::connect(handle.address).unwrap();
    partial.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    partial.write_all(b"POST /api/pair HTTP/1.1\r\n").unwrap();
    let _credential = pair(&handle, TokenKind::Steer);
    drop(server);
    assert!(handle.pairing_url(TokenKind::Steer).is_err());
    assert!(handle.take_proposal().is_none());
    assert!(TcpStream::connect_timeout(&handle.address, Duration::from_secs(1)).is_err());
    let mut buffer = [0_u8; 1];
    match partial.read(&mut buffer) {
        Ok(0) => {}
        Err(error) if matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
        ) => {}
        other => panic!("server shutdown did not close the partial request: {other:?}"),
    }
}

#[test]
fn unsupported_binding_and_authentication_modes_fail_before_starting_a_server() {
    for bind_mode in [BindMode::Lan, BindMode::Tailscale] {
        assert!(CompanionServer::start(WebRemoteSettings {
            port: 0, bind_mode, ..WebRemoteSettings::default()
        }).is_err());
    }
    assert!(CompanionServer::start(WebRemoteSettings {
        port: 0, require_auth_token: false, ..WebRemoteSettings::default()
    }).is_err());
    assert!(CompanionServer::start(WebRemoteSettings {
        port: 0, max_viewers: MAX_VIEWERS + 1, ..WebRemoteSettings::default()
    }).is_err());
}

#[test]
fn audit_history_is_bounded_and_never_contains_pairing_or_prompt_material() {
    let (_server, handle) = server(4);
    let secret = "audit-must-not-retain-this-prompt";
    let proposal = delivered_proposal(&handle, secret);
    assert!(handle.resolve_proposal(&proposal.id, false, "Declined locally"));
    for _ in 0..MAX_AUDIT_EVENTS + 2 {
        let _link = handle.pairing_url(TokenKind::View).unwrap();
        handle.revoke_all();
    }
    let events = handle.audit_events();
    assert_eq!(events.len(), MAX_AUDIT_EVENTS);
    assert!(events.iter().all(|event| event.client_id.is_none() && event.details.is_empty()));
    let encoded = serde_json::to_string(&events).unwrap();
    assert!(!encoded.contains(secret));
    assert!(!encoded.contains("#token="));
    assert!(!encoded.contains("private-session-A"));
}

fn user(text: impl Into<String>) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 0 })
}

#[test]
fn projection_redacts_complete_split_credentials_and_cross_message_echoes_before_clipping() {
    let secret = "rememberedCredentialValue123456";
    let custom = "ACME-123456";
    let mut vault = crate::secrets::SecretVault::default();
    let _ = crate::secrets::obfuscate(&format!("API_KEY={secret}"), &mut vault, &[]);
    let privacy = AuxiliaryPrivacySnapshot::with_vault(
        AuxiliaryPrivacy::from_settings(Some(&crate::secrets::SecretsSettings {
            mode: Some("off".to_string()),
            extra_patterns: Some(vec![r"ACME-\d{6}".to_string()]),
        })),
        &vault,
    );
    let assignment = format!("API_KEY={secret}");
    let messages = vec![
        user(format!("echo {secret} {custom}")),
        Message::User(UserMessage {
            content: UserContent::Blocks(vec![
                ContentBlock::Text(TextContent::new(&assignment[..12])),
                ContentBlock::Text(TextContent::new(&assignment[12..])),
            ]),
            timestamp: 0,
        }),
        user(format!("{} API_KEY=laterCredentialValue654321", "a".repeat(9000))),
        user("earlier echo laterCredentialValue654321"),
    ];
    let encoded = projection::project(secret, custom, &messages, &privacy);
    for secret in [secret, custom, "laterCredentialValue654321"] {
        assert!(!encoded.contains(secret), "unredacted {secret}");
    }
    assert!(encoded.contains("pi-secret:redacted"));
    assert!(serde_json::from_str::<Value>(&encoded).is_ok());
}

#[test]
fn projection_withholds_blocked_or_oversized_text_and_never_exports_tool_or_reasoning_payloads() {
    let blocked = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::from_settings(Some(
        &crate::secrets::SecretsSettings {
            mode: Some("block".to_string()),
            extra_patterns: Some(vec!["BLOCKED-CREDENTIAL".to_string()]),
        },
    )));
    let encoded = projection::project("session", "model", &[user("BLOCKED-CREDENTIAL")], &blocked);
    assert!(!encoded.contains("BLOCKED-CREDENTIAL"));
    assert!(encoded.contains("withheld"));
    let privacy = AuxiliaryPrivacySnapshot::from_privacy(AuxiliaryPrivacy::default());
    let messages = vec![
        user(format!("oversized-prefix-canary{}", "x".repeat(40 * 1024))),
        Message::assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking(crate::model::ThinkingContent {
                    thinking: "private-reasoning-canary".to_string(), thinking_signature: None,
                }),
                ContentBlock::ToolCall(crate::model::ToolCall {
                    id: "call-private".to_string(), name: "bash".to_string(),
                    arguments: json!({"command":"private-tool-canary"}),
                    thought_signature: None,
                }),
                ContentBlock::Text(TextContent::new("Public completion.")),
            ],
            ..AssistantMessage::default()
        }),
    ];
    let encoded = projection::project("session", "model", &messages, &privacy);
    for canary in ["oversized-prefix-canary", "private-reasoning-canary", "private-tool-canary", "call-private"] {
        assert!(!encoded.contains(canary));
    }
    assert!(encoded.contains("Public completion."));
    assert!(encoded.contains("message omitted"));
}
