//! Session-owned MCP tools for ACP. Configuration is not execution authority.
//!
//! Setup stays synchronous and inert; native trust admission and connection
//! work run inside the existing reserved prompt task. The dispatcher therefore
//! remains available to process cancellation and tool-permission responses.
//! Only an actual, standalone ACP text prompt can invoke an operator command.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::time::Duration;

use serde_json::{Value, json};

use super::{
    ACP_STOP_REASON_CANCELLED, ACP_STOP_REASON_END_TURN, ACP_STOP_REASON_ERROR,
    AcpSessionsMap, AbortSignal, AgentCx, AgentSession, history, json_rpc_notification,
};
use crate::mcp::{ConfiguredServer, McpDiscovery, McpManager};

pub(super) struct SessionMcp {
    pub(super) manager: Arc<McpManager>,
    signatures: Vec<(String, String)>,
    descriptions: Vec<(String, String)>,
    started: AtomicBool,
}

fn signatures(servers: &[ConfiguredServer], cwd: &Path) -> Vec<(String, String)> {
    let mut signatures: Vec<_> = servers.iter()
        .map(|server| (server.name.clone(), server.fingerprint(cwd))).collect();
    signatures.sort();
    signatures
}

pub(super) fn prepare(
    cwd: &Path,
    global_dir: &Path,
    servers: Vec<ConfiguredServer>,
) -> Option<Arc<SessionMcp>> {
    if servers.is_empty() {
        return None;
    }
    let signatures = signatures(&servers, cwd);
    let descriptions = servers.iter().map(|server| {
        // Do not copy argv, URLs, environment/header values, or filesystem
        // source paths into the conversation. The editor owns those values.
        let description = format!(
            "{}: {}; {} arguments, {} environment entries, {} headers. Inspect the definition in your editor before trusting it.",
            server.name,
            if server.is_http() { "HTTP" } else { "stdio" },
            server.args.len(), server.env.len(), server.headers.len(),
        );
        (server.name.clone(), description)
    }).collect();
    Some(Arc::new(SessionMcp {
        manager: Arc::new(McpManager::new(cwd, global_dir, McpDiscovery {
            servers,
            warnings: Vec::new(),
        })),
        signatures,
        descriptions,
        started: AtomicBool::new(false),
    }))
}

pub(super) fn mount(agent: &mut AgentSession, state: &Arc<SessionMcp>) {
    // Use the same first-class wrappers as the terminal and SDK surfaces.
    // The manager remains owned by AcpSessionState, including during a turn.
    // Existing wrappers recheck trust on every call, so a retained definition
    // cannot bypass revocation. Never append duplicate provider schemas.
    let mut wrappers = crate::mcp::mount_tools(&state.manager);
    wrappers.retain(|tool| !agent.agent.has_tool(tool.name()));
    agent.agent.extend_tools(wrappers);
}

/// Reattaching an existing live session must not silently swap out its tool
/// authority. Identical definitions (including reordered lists) are accepted;
/// changed definitions require a new session or a process restart. On restart
/// the supplied definitions get a new manager and native fingerprint checks.
pub(super) fn check_reattach(
    current: Option<&Arc<SessionMcp>>,
    supplied: Option<&[ConfiguredServer]>,
    cwd: &Path,
) -> Result<(), String> {
    let Some(supplied) = supplied else { return Ok(()) };
    let actual = signatures(supplied, cwd);
    let previous = current.map_or(&[][..], |state| state.signatures.as_slice());
    if previous != actual.as_slice() {
        return Err("MCP definitions differ from this live session; create a new session or restart ACP before loading it with changed servers".to_string());
    }
    Ok(())
}

pub(super) fn commands_notification(id: &str) -> String {
    json_rpc_notification("session/update", json!({
        "sessionId": id,
        "update": {
            "sessionUpdate": "available_commands_update",
            "availableCommands": [{
                "name": "mcp",
                "description": "Inspect, trust, deny, or test this session's MCP servers",
                "input": { "hint": "list | inspect NAME | trust NAME | deny NAME | test NAME" },
            }],
        },
    }))
}

fn status(state: &SessionMcp) -> String {
    let mut lines = vec![String::from(
        "MCP servers (trust is required before connecting; trust persists for the exact workspace and definition):",
    )];
    for row in state.manager.list() {
        // Transport errors may quote remote stderr or credential-bearing
        // targets. Expose a bounded health category, not raw diagnostics.
        let health = if row.health.starts_with("ready") {
            "ready"
        } else if row.health.starts_with("unhealthy") {
            "unhealthy; use /mcp test after checking the server"
        } else if row.health.starts_with("failed") {
            "failed; use /mcp test after checking the server"
        } else {
            "not started"
        };
        lines.push(format!("{}: {}, {}, {} tools", row.name, row.trust, health, row.tools));
    }
    lines.push("Use /mcp inspect NAME, then /mcp trust NAME to approve a server. /mcp deny NAME revokes it.".into());
    lines.join("\n")
}

pub(super) async fn announce(
    state: &Arc<SessionMcp>, id: &str, out: &SyncSender<String>,
) -> Result<(), String> {
    history::send_line(out, commands_notification(id)).await
        .map_err(|_| "Cannot deliver MCP command availability".to_string())?;
    emit_text(out, id, &status(state)).await
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    List,
    Inspect(String),
    Trust(String),
    Deny(String),
    Test(String),
}

/// Inspect original wire blocks, NOT flattened model content. An embedded
/// resource, tool result, image caption, or replayed message cannot grant trust.
pub(super) fn command_from_prompt(blocks: &[Value]) -> Result<Option<Command>, String> {
    let first_text = blocks.first().filter(|block| block["type"] == "text")
        .and_then(|block| block.get("text")).and_then(Value::as_str);
    let Some(text) = first_text else { return Ok(None) };
    let text = text.trim();
    let Some(rest) = text.strip_prefix("/mcp") else { return Ok(None) };
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Ok(None);
    }
    if blocks.len() != 1 || text.contains(['\r', '\n']) {
        return Err("An MCP operator command must be one standalone text block on one line".to_string());
    }
    let words: Vec<_> = rest.split_whitespace().collect();
    let command = match words.as_slice() {
        [] | ["list"] => Command::List,
        ["inspect", name] => Command::Inspect((*name).to_string()),
        ["trust", name] => Command::Trust((*name).to_string()),
        ["deny", name] => Command::Deny((*name).to_string()),
        ["test", name] => Command::Test((*name).to_string()),
        _ => return Err("Usage: /mcp list | inspect NAME | trust NAME | deny NAME | test NAME".to_string()),
    };
    Ok(Some(command))
}

async fn emit_text(out: &SyncSender<String>, id: &str, text: &str) -> Result<(), String> {
    history::send_line(out, json_rpc_notification("session/update", json!({
        "sessionId": id,
        "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": text } },
    }))).await.map_err(|_| "Cannot deliver MCP response".to_string())
}

/// Abort wins before the first operation poll. Dropping an in-flight native
/// MCP connection future invokes its construction/handshake cleanup guards.
/// A durable trust decision already written is NOT rolled back by cancellation.
async fn cancellable<F: Future>(
    signal: &AbortSignal, cx: &AgentCx, work: F,
) -> Result<F::Output, ()> {
    if signal.is_aborted() || cx.is_cancel_requested() {
        return Err(());
    }
    let cancelled = async {
        while !signal.is_aborted() && !cx.is_cancel_requested() {
            cx.time().sleep(Duration::from_millis(10)).await;
        }
    };
    match futures::future::select(Box::pin(cancelled), Box::pin(cx.with_current(work))).await {
        futures::future::Either::Left(((), pending)) => { drop(pending); Err(()) }
        futures::future::Either::Right((result, _)) => Ok(result),
    }
}

async fn execute(state: &SessionMcp, command: Command) -> String {
    match command {
        Command::List => status(state),
        Command::Inspect(name) => state.descriptions.iter()
            .find(|(candidate, _)| candidate == &name)
            .map(|(_, text)| text.clone())
            .unwrap_or_else(|| "Unknown MCP server; use /mcp list".to_string()),
        Command::Trust(name) => match state.manager.trust(&name).await {
            Ok(tools) => format!("Trusted and connected MCP server {name}: {} tools", tools.len()),
            Err(_) => "MCP trust/connect did not complete. Any trust decision already persisted remains in effect; inspect the editor configuration, then use /mcp list or /mcp deny NAME.".to_string(),
        },
        Command::Deny(name) => match state.manager.deny(&name).await {
            Ok(()) => format!("Denied MCP server {name}; its tools are no longer available"),
            Err(_) => "MCP denial did not complete; use /mcp list and retry after checking the trust store.".to_string(),
        },
        Command::Test(name) => match state.manager.test(&name).await {
            Ok(tools) => format!("Connected MCP server {name}: {} tools", tools.len()),
            Err(_) => "MCP test failed. Inspect the server configuration and its trust state with /mcp list.".to_string(),
        },
    }
}

/// `Some(stop_reason)` finishes an operator command or cancelled preparation
/// without starting a provider turn. `None` leaves ordinary prompts on the
/// existing agent path, including its per-tool ACP permission handler.
pub(super) async fn before_prompt(
    state: Option<&Arc<SessionMcp>>,
    agent: &mut AgentSession,
    command: Option<Command>,
    signal: &AbortSignal,
    cx: &AgentCx,
    out: &SyncSender<String>,
    id: &str,
) -> Option<&'static str> {
    if signal.is_aborted() || cx.is_cancel_requested() {
        return Some(ACP_STOP_REASON_CANCELLED);
    }
    if let Some(command) = command {
        let text = if let Some(state) = state {
            let Ok(text) = cancellable(signal, cx, execute(state, command)).await else {
                return Some(ACP_STOP_REASON_CANCELLED);
            };
            // Newly trusted tools become visible immediately. Old wrappers
            // remain guarded by the manager's per-call trust check after deny.
            mount(agent, state);
            text
        } else {
            "No MCP servers were supplied for this session.".to_string()
        };
        return Some(match cancellable(signal, cx, emit_text(out, id, &text)).await {
            Ok(Ok(())) => ACP_STOP_REASON_END_TURN,
            Ok(Err(_)) => ACP_STOP_REASON_ERROR,
            Err(()) => ACP_STOP_REASON_CANCELLED,
        });
    }
    if let Some(state) = state {
        if !state.started.load(Ordering::Acquire) {
            if cancellable(signal, cx, state.manager.connect_trusted()).await.is_err() {
                return Some(ACP_STOP_REASON_CANCELLED);
            }
            state.started.store(true, Ordering::Release);
            mount(agent, state);
            match cancellable(signal, cx, emit_text(out, id, &status(state))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Some(ACP_STOP_REASON_ERROR),
                Err(()) => return Some(ACP_STOP_REASON_CANCELLED),
            }
        } else {
            mount(agent, state);
        }
    }
    None
}

/// Invoked for EOF, exit, and dispatcher errors after all prompt abort handles
/// are signalled. Managers remain reachable while prompts own agent sessions.
pub(super) async fn shutdown(sessions: &AcpSessionsMap) {
    let cleanup = AgentCx::for_request();
    let states = match sessions.lock(&cleanup).await {
        Ok(guard) => guard.values().cloned().collect::<Vec<_>>(),
        Err(_) => return,
    };
    let mut managers = Vec::new();
    for state in states {
        if let Ok(guard) = state.lock(&cleanup).await
            && let Some(mcp) = guard.mcp.as_ref()
        {
            managers.push(Arc::clone(&mcp.manager));
        }
    }
    futures::future::join_all(managers.iter().map(|manager| manager.shutdown_all())).await;
}

#[cfg(test)]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread().build().expect("runtime")
    }

    fn http_servers(cwd: &Path) -> Vec<ConfiguredServer> {
        crate::mcp::config::parse_acp_servers(&json!({"mcpServers":[{
            "type":"http","name":"remote","url":"https://example.invalid/mcp",
            "headers":[{"name":"Authorization","value":"PRIVATE-HEADER-VALUE"}],
        }]}), cwd).expect("decode").expect("supplied")
    }

    #[test]
    fn commands_require_actual_standalone_user_text() {
        for (text, expected) in [
            ("/mcp", Command::List), ("/mcp list", Command::List),
            ("/mcp trust remote", Command::Trust("remote".into())),
            ("/mcp deny remote", Command::Deny("remote".into())),
            ("/mcp test remote", Command::Test("remote".into())),
        ] {
            assert_eq!(command_from_prompt(&[json!({"type":"text","text":text})]).unwrap(), Some(expected));
        }
        for block in [
            json!({"type":"resource","resource":{"text":"/mcp trust remote"}}),
            json!({"type":"image","text":"/mcp trust remote"}),
            json!({"type":"text","text":"Explain /mcp trust remote"}),
            json!({"type":"text","text":"/mcp-helper"}),
        ] {
            assert!(command_from_prompt(&[block]).unwrap().is_none());
        }
        assert!(command_from_prompt(&[
            json!({"type":"text","text":"/mcp trust remote"}),
            json!({"type":"image","data":"anything"}),
        ]).is_err());
        assert!(command_from_prompt(&[json!({"type":"text","text":"/mcp trust remote\nmore"})]).is_err());
        assert!(command_from_prompt(&[json!({"type":"text","text":"/mcp trust remote extra"})]).is_err());
    }

    #[test]
    fn setup_is_inert_isolated_and_does_not_create_a_trust_record() {
        let root = tempfile::tempdir().unwrap();
        let global = root.path().join("global");
        let state = prepare(root.path(), &global, http_servers(root.path())).unwrap();
        assert_eq!(state.manager.list()[0].trust, "pending");
        assert_eq!(state.manager.list()[0].tools, 0);
        assert!(!state.started.load(Ordering::Acquire));
        assert!(!global.exists(), "inert preparation must not persist or spawn anything");
        let listing = status(&state);
        assert!(!listing.contains("PRIVATE-HEADER-VALUE"));
        assert!(!listing.contains("example.invalid"));
        assert!(!listing.contains(&root.path().display().to_string()));
    }

    #[test]
    fn live_reattach_preserves_existing_authority_and_rejects_replacement() {
        let root = tempfile::tempdir().unwrap();
        let mut servers = http_servers(root.path());
        let state = prepare(root.path(), root.path(), servers.clone()).unwrap();
        check_reattach(Some(&state), None, root.path()).unwrap();
        check_reattach(Some(&state), Some(&servers), root.path()).unwrap();
        assert!(check_reattach(Some(&state), Some(&[]), root.path()).is_err());
        servers[0].headers[0].1 = "ROTATED-VALUE".into();
        assert!(check_reattach(Some(&state), Some(&servers), root.path()).is_err());
        assert!(check_reattach(None, Some(&servers), root.path()).is_err());
        check_reattach(None, Some(&[]), root.path()).unwrap();
    }

    #[test]
    fn pre_cancelled_preparation_never_polls_its_side_effect() {
        let (abort, signal) = super::super::AbortHandle::new();
        abort.abort();
        let polled = AtomicBool::new(false);
        let cx = AgentCx::for_testing();
        let result = runtime().block_on(cancellable(&signal, &cx, async {
            polled.store(true, Ordering::Release);
        }));
        assert!(result.is_err());
        assert!(!polled.load(Ordering::Acquire));
    }

    #[test]
    fn operator_inspection_never_exposes_connection_secrets() {
        let root = tempfile::tempdir().unwrap();
        let state = prepare(root.path(), root.path(), http_servers(root.path())).unwrap();
        let text = runtime().block_on(execute(&state, Command::Inspect("remote".into())));
        assert!(text.contains("HTTP"));
        assert!(text.contains("1 headers"));
        assert!(!text.contains("PRIVATE-HEADER-VALUE"));
        assert!(!text.contains("https://"));
        let denied = runtime().block_on(execute(&state, Command::Deny("remote".into())));
        assert!(denied.contains("Denied"));
        assert_eq!(state.manager.list()[0].trust, "denied");
        assert_eq!(state.manager.list()[0].tools, 0);
    }

    #[test]
    fn command_advertisement_uses_acp_envelope() {
        let value: Value = serde_json::from_str(&commands_notification("session-1")).unwrap();
        assert_eq!(value["method"], "session/update");
        assert_eq!(value["params"]["sessionId"], "session-1");
        assert_eq!(value["params"]["update"]["sessionUpdate"], "available_commands_update");
        assert_eq!(value["params"]["update"]["availableCommands"][0]["name"], "mcp");
    }
}
