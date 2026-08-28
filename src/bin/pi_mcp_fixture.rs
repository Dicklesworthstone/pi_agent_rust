//! DEV-ONLY MCP fixture server (bd-cv653.6.1 test lanes).
//!
//! A minimal stdio JSON-RPC process speaking enough MCP for the client e2e
//! lanes: initialize → initialized → tools/list → tools/call → ping, with
//! canned tools (`echo`, `env_probe`), a startup stderr marker (stderr
//! capture proof), and an optional crash mode
//! (`PI_MCP_FIXTURE_CRASH_AFTER=<n>` exits after n requests) for the
//! restart/backoff lane. Not shipped to end users (feature-gated binary).
//! This fixture proves Pi's local wire behavior only; it is not evidence that
//! any third-party MCP server is available or interoperable.

use std::io::{BufRead, BufReader, Write};

use serde_json::{Value, json};

const MAX_FIXTURE_MESSAGE_BYTES: usize = 10 * 1024 * 1024;

fn read_message(reader: &mut impl BufRead) -> std::io::Result<Option<Value>> {
    let mut message = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if message.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF before newline terminator",
            ));
        }
        if let Some(newline) = available.iter().position(|byte| *byte == b'\n') {
            if message.len().saturating_add(newline) > MAX_FIXTURE_MESSAGE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "message exceeds fixture limit",
                ));
            }
            message.extend_from_slice(&available[..newline]);
            reader.consume(newline + 1);
            if message.last() == Some(&b'\r') {
                message.pop();
            }
            return serde_json::from_slice(&message).map(Some).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid newline-delimited JSON: {error}"),
                )
            });
        }
        if message.len().saturating_add(available.len()) > MAX_FIXTURE_MESSAGE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "message exceeds fixture limit",
            ));
        }
        let consumed = available.len();
        message.extend_from_slice(available);
        reader.consume(consumed);
    }
}

fn write_message(stdout: &mut impl Write, value: &Value) -> std::io::Result<()> {
    let mut message = serde_json::to_vec(value)?;
    if message.len() > MAX_FIXTURE_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "fixture response exceeds limit",
        ));
    }
    message.push(b'\n');
    stdout.write_all(&message)?;
    stdout.flush()
}

fn write_oversize_line(stdout: &mut impl Write) -> std::io::Result<()> {
    let chunk = [b'x'; 8192];
    let mut remaining = MAX_FIXTURE_MESSAGE_BYTES + 1;
    while remaining > 0 {
        let count = remaining.min(chunk.len());
        stdout.write_all(&chunk[..count])?;
        remaining -= count;
    }
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn validate_client_message(frame: &Value) -> std::io::Result<()> {
    let object = frame.as_object().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "message must be an object")
    })?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message must declare jsonrpc 2.0",
        ));
    }
    if !object.get("method").is_some_and(Value::is_string) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "client message must have a string method",
        ));
    }
    if object
        .get("params")
        .is_some_and(|params| !params.is_object() && !params.is_array())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "params must be an object or array",
        ));
    }
    Ok(())
}

fn tool_list() -> Value {
    json!({
        "tools": [
            {
                "name": "echo",
                "description": "Echo the `text` argument back",
                "inputSchema": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }
            },
            {
                "name": "env_probe",
                "description": "Report which marker env vars the fixture inherited",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ]
    })
}

fn call_tool(params: &Value, requests: u64) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    match name {
        "echo" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("<missing>");
            json!({
                "content": [{ "type": "text", "text": format!("echo: {text} [pid={} req={requests}]", std::process::id()) }],
                "isError": false
            })
        }
        "env_probe" => {
            let present = |var: &str| std::env::var_os(var).is_some();
            json!({
                "content": [{
                    "type": "text",
                    "text": json!({
                        "PATH": present("PATH"),
                        "HOME": present("HOME"),
                        "PI_MCP_SECRET_MARKER": present("PI_MCP_SECRET_MARKER"),
                        "AWS_SECRET_ACCESS_KEY": present("AWS_SECRET_ACCESS_KEY"),
                    })
                    .to_string()
                }],
                "isError": false
            })
        }
        other => json!({
            "content": [{ "type": "text", "text": format!("unknown tool {other}") }],
            "isError": true
        }),
    }
}

fn main() {
    if std::env::var_os("PI_MCP_FIXTURE_CHILD_SENTINEL").is_some() {
        loop {
            std::thread::park_timeout(std::time::Duration::from_secs(60));
        }
    }

    // Startup stderr marker: the client surfaces this in diagnostics.
    eprintln!("pi_mcp_fixture: ready marker 7f3a9c-v2");
    let crash_after = std::env::var("PI_MCP_FIXTURE_CRASH_AFTER")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    let response_mode = std::env::var("PI_MCP_FIXTURE_RESPONSE_MODE").unwrap_or_default();
    let _descendant = if std::env::var_os("PI_MCP_FIXTURE_SPAWN_DESCENDANT").is_some() {
        let mut descendant =
            std::process::Command::new(std::env::current_exe().expect("fixture executable path"));
        descendant
            .env("PI_MCP_FIXTURE_CHILD_SENTINEL", "1")
            .stdin(std::process::Stdio::null());
        if std::env::var_os("PI_MCP_FIXTURE_DESCENDANT_INHERIT_OUTPUT").is_none() {
            descendant
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        let child = descendant.spawn().expect("spawn fixture descendant");
        eprintln!("pi_mcp_fixture: descendant pid={}", child.id());
        Some(child)
    } else {
        None
    };
    if response_mode == "root-exit" {
        return;
    }
    if response_mode == "no-read" {
        loop {
            std::thread::park_timeout(std::time::Duration::from_secs(60));
        }
    }

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    let mut requests: u64 = 0;
    let mut cancellation_target: Option<Value> = None;

    loop {
        let frame = match read_message(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                eprintln!("pi_mcp_fixture: protocol input rejected: {error}");
                break;
            }
        };
        if let Err(error) = validate_client_message(&frame) {
            eprintln!("pi_mcp_fixture: protocol input rejected: {error}");
            break;
        }
        let id = frame.get("id").cloned();
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        if id.is_none() {
            if method == "notifications/cancelled"
                && cancellation_target.as_ref()
                    == frame
                        .get("params")
                        .and_then(|params| params.get("requestId"))
            {
                eprintln!("pi_mcp_fixture: observed cancellation for pending request");
                cancellation_target = None;
            }
            continue; // notification
        }
        requests += 1;
        eprintln!("pi_mcp_fixture: request {requests} method={method}");
        if crash_after.is_some_and(|limit| requests > limit) {
            eprintln!("pi_mcp_fixture: crashing after {requests} requests (fixture mode)");
            std::process::exit(1);
        }
        if method == "fixture/await-cancellation"
            || (method == "initialize" && response_mode == "hang-initialize")
        {
            cancellation_target = id;
            continue;
        }
        if method == "tools/list" {
            match response_mode.as_str() {
                "malformed" => {
                    let _ = stdout.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\n");
                    let _ = stdout.flush();
                    break;
                }
                "oversize" => {
                    let _ = write_oversize_line(&mut stdout);
                    break;
                }
                "eof" => break,
                "wrong-id" => {
                    let wrong_id = id.as_ref().and_then(Value::as_u64).unwrap_or(0) + 1000;
                    let _ = write_message(
                        &mut stdout,
                        &json!({ "jsonrpc": "2.0", "id": wrong_id, "result": tool_list() }),
                    );
                    break;
                }
                _ => {}
            }
        }
        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "pi_mcp_fixture", "version": "0.1.0" }
            }),
            "tools/list" => tool_list(),
            "tools/call" => call_tool(frame.get("params").unwrap_or(&Value::Null), requests),
            "ping" => json!({}),
            _ => {
                let _ = write_message(
                    &mut stdout,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("unknown method {method}") }
                    }),
                );
                continue;
            }
        };
        if write_message(
            &mut stdout,
            &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        )
        .is_err()
        {
            break;
        }
    }
}
