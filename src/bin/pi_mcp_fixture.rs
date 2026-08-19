//! DEV-ONLY MCP fixture server (bd-cv653.6.1 test lanes).
//!
//! A minimal stdio JSON-RPC process speaking enough MCP for the client e2e
//! lanes: initialize → initialized → tools/list → tools/call → ping, with
//! canned tools (`echo`, `env_probe`), a startup stderr marker (stderr
//! capture proof), and an optional crash mode
//! (`PI_MCP_FIXTURE_CRASH_AFTER=<n>` exits after n requests) for the
//! restart/backoff lane. Not shipped to end users (feature-gated binary).

use std::io::{BufRead, BufReader, Read, Write};

use serde_json::{Value, json};

fn read_frame(reader: &mut BufReader<impl Read>) -> std::io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed
            .split_once(':')
            .map(|(k, v)| (k.trim(), v.trim()))
            .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v)
        {
            content_length = value.parse::<usize>().ok();
        }
    }
    let length = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let value = serde_json::from_slice(&body)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    Ok(Some(value))
}

fn write_frame(stdout: &mut impl Write, value: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(stdout, "Content-Length: {}\r\n\r\n", body.len())?;
    stdout.write_all(&body)?;
    stdout.flush()
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
    // Startup stderr marker: the client surfaces this in diagnostics.
    eprintln!("pi_mcp_fixture: ready marker 7f3a9c-v2");
    let crash_after = std::env::var("PI_MCP_FIXTURE_CRASH_AFTER")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = std::io::stdout();
    let mut requests: u64 = 0;

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(frame)) => frame,
            Ok(None) | Err(_) => break,
        };
        let id = frame.get("id").cloned();
        let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
        if id.is_none() {
            continue; // notification
        }
        requests += 1;
        eprintln!("pi_mcp_fixture: request {requests} method={method}");
        if crash_after.is_some_and(|limit| requests > limit) {
            eprintln!("pi_mcp_fixture: crashing after {requests} requests (fixture mode)");
            std::process::exit(1);
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
                let _ = write_frame(
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
        if write_frame(
            &mut stdout,
            &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        )
        .is_err()
        {
            break;
        }
    }
}
