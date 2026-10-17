//! Decode ACP's session-scoped MCP server list without granting execution.
//!
//! ACP supplies literal environment/header values, not pi's `$ENV:`/`$CMD:`
//! reference language. A distinct provenance binds that interpretation into
//! the native trust fingerprint. Definitions are never written to a config or
//! conversation file; clients supply them again when reopening a session.

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use super::{
    ConfiguredServer, MAX_MCP_CONFIG_BYTES, Provenance, normalize_env,
    normalize_http_headers, validate_server_name, validate_transport_shape,
};

const MAX_ACP_MCP_SERVERS: usize = 32;
const MAX_ACP_MCP_ARGS: usize = 256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NameValue {
    name: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireServer {
    name: String,
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<String>>,
    #[serde(default)]
    env: Option<Vec<NameValue>>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<Vec<NameValue>>,
    #[serde(rename = "_meta", default)]
    _meta: Option<Value>,
}

// Measure before cloning/deserializing the list. Do not duplicate an oversized
// credential-bearing request in memory, nor echo serde's offending values.
struct SizeLimit(usize);

impl Write for SizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_add(bytes.len()).ok_or_else(|| {
            std::io::Error::other("ACP MCP configuration size overflow")
        })?;
        if self.0 > MAX_MCP_CONFIG_BYTES {
            return Err(std::io::Error::other("ACP MCP configuration size limit"));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `None` means the client omitted the field, while `Some([])` explicitly
/// selects no servers. That distinction matters when reattaching a live
/// session: an omitted field must not disconnect its existing tools.
pub(crate) fn parse_acp_servers(
    params: &Value,
    cwd: &Path,
) -> Result<Option<Vec<ConfiguredServer>>, String> {
    let Some(raw) = params.get("mcpServers") else {
        return Ok(None);
    };
    let servers = raw
        .as_array()
        .ok_or_else(|| "mcpServers must be an array".to_string())?;
    if servers.len() > MAX_ACP_MCP_SERVERS {
        return Err(format!("mcpServers supports at most {MAX_ACP_MCP_SERVERS} servers"));
    }
    serde_json::to_writer(&mut SizeLimit(0), raw).map_err(|_| {
        format!("mcpServers exceeds the {MAX_MCP_CONFIG_BYTES}-byte configuration limit")
    })?;

    let mut names = HashSet::with_capacity(servers.len());
    let mut configured = Vec::with_capacity(servers.len());
    for (index, raw) in servers.iter().enumerate() {
        // Option fields otherwise accept JSON null as absence. ACP requires
        // the declared field's actual type; never reinterpret malformed input.
        for field in ["type", "command", "args", "env", "url", "headers"] {
            if raw.get(field).is_some_and(Value::is_null) {
                return Err(format!("mcpServers[{index}].{field} must not be null"));
            }
        }
        let wire: WireServer = serde_json::from_value(raw.clone())
            .map_err(|_| format!("Invalid MCP server definition at mcpServers[{index}]"))?;
        validate_server_name(&wire.name)
            .map_err(|_| format!("Invalid server name at mcpServers[{index}]"))?;
        if !names.insert(wire.name.clone()) {
            return Err(format!("Duplicate server name at mcpServers[{index}]"));
        }

        let kind = wire.kind.as_deref().unwrap_or("stdio");
        match kind {
            "stdio" => {
                if wire.url.is_some() || wire.headers.is_some() {
                    return Err(format!("Stdio server at mcpServers[{index}] cannot define url or headers"));
                }
                let command = wire.command.as_deref().ok_or_else(|| {
                    format!("Stdio server at mcpServers[{index}] requires command")
                })?;
                if command.is_empty()
                    || command.trim() != command
                    || command.contains('\0')
                    || !Path::new(command).is_absolute()
                {
                    return Err(format!("Invalid command at mcpServers[{index}]"));
                }
                if let Some(args) = wire.args.as_ref()
                    && (args.len() > MAX_ACP_MCP_ARGS || args.iter().any(|arg| arg.contains('\0')))
                {
                    return Err(format!("Invalid args at mcpServers[{index}]"));
                }
            }
            "http" => {
                if wire.command.is_some() || wire.args.is_some() || wire.env.is_some() {
                    return Err(format!("HTTP server at mcpServers[{index}] cannot define command, args, or env"));
                }
                let raw_url = wire.url.as_deref().ok_or_else(|| {
                    format!("HTTP server at mcpServers[{index}] requires url")
                })?;
                let parsed = url::Url::parse(raw_url)
                    .map_err(|_| format!("Invalid HTTP URL at mcpServers[{index}]"))?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.host_str().is_none()
                    || parsed.fragment().is_some()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || raw_url.chars().any(char::is_control)
                    || raw_url.trim() != raw_url
                {
                    return Err(format!("Invalid HTTP URL at mcpServers[{index}]; use HTTP(S) with credentials in headers"));
                }
            }
            "sse" => return Err("ACP MCP SSE transport is not supported; use stdio or HTTP".to_string()),
            _ => return Err(format!("Unsupported MCP transport at mcpServers[{index}]")),
        }

        let pairs = |entries: Option<Vec<NameValue>>| {
            entries.unwrap_or_default().into_iter()
                .map(|entry| (entry.name, entry.value)).collect()
        };
        let server = ConfiguredServer {
            name: wire.name,
            command: wire.command,
            args: wire.args.unwrap_or_default(),
            env: normalize_env(pairs(wire.env))
                .map_err(|_| format!("Invalid environment at mcpServers[{index}]"))?,
            url: wire.url,
            headers: normalize_http_headers(pairs(wire.headers))
                .map_err(|_| format!("Invalid HTTP headers at mcpServers[{index}]"))?,
            transport_hint: Some(kind.to_string()),
            provenance: Provenance::Acp,
            // A stable workspace-scoped source identity, not a file we create
            // or read. Provenance prevents aliasing a real CLI/project file.
            source_file: cwd.join(".pi/acp-mcp-session"),
        };
        validate_transport_shape(&server)
            .map_err(|_| format!("Invalid transport shape at mcpServers[{index}]"))?;
        configured.push(server);
    }
    configured.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(Some(configured))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command() -> String {
        std::env::current_exe().unwrap().display().to_string()
    }

    fn parse(servers: Value) -> Result<Vec<ConfiguredServer>, String> {
        parse_acp_servers(&json!({"mcpServers": servers}), Path::new("/workspace"))
            .map(|value| value.expect("field supplied"))
    }

    #[test]
    fn preserves_omission_and_explicit_empty_list() {
        assert!(parse_acp_servers(&json!({}), Path::new("/workspace")).unwrap().is_none());
        assert!(parse(json!([])).unwrap().is_empty());
        for invalid in [Value::Null, json!({}), json!(false)] {
            assert!(parse(invalid).is_err());
        }
    }

    #[test]
    fn decodes_stdio_and_http_without_resolving_literal_values() {
        let servers = parse(json!([
            {"name":"stdio","command":command(),"args":["--flag","a b"],
             "env":[{"name":"TOKEN","value":"$CMD:must-not-run"}]},
            {"type":"http","name":"remote","url":"https://example.invalid/mcp",
             "headers":[{"name":"Authorization","value":"$ENV:NOT_A_LOOKUP"}]}
        ])).unwrap();
        assert_eq!(servers[0].name, "remote");
        assert_eq!(servers[0].headers[0].1, "$ENV:NOT_A_LOOKUP");
        assert_eq!(servers[1].args, ["--flag", "a b"]);
        assert_eq!(servers[1].env[0].1, "$CMD:must-not-run");
        assert!(servers.iter().all(|server| server.provenance == Provenance::Acp));
        assert!(!Provenance::Acp.is_native());
    }

    #[test]
    fn rejects_ambiguous_servers_transports_and_malformed_arrays() {
        for servers in [
            json!([{"name":"one","command":command()},{"name":"one","command":command()}]),
            json!([{"name":"one","command":command(),"url":"https://example.invalid"}]),
            json!([{"name":"one","command":command(),"env":{"TOKEN":"value"}}]),
            json!([{"name":"one","command":command(),"args":[4]}]),
            json!([{"name":"one","command":command(),"env":null}]),
            json!([{"name":"one","command":command(),"type":null}]),
            json!([{"name":"one","command":command(),"surprise":"value"}]),
            json!([{"name":"one","type":"sse","url":"https://example.invalid"}]),
            json!([{"name":"one","type":"http","url":"https://example.invalid","args":[]}]),
            json!([{"name":"one","command":command(),"headers":[]}]),
            json!([{"name":"one","command":"relative-command"}]),
        ] {
            assert!(parse(servers).is_err());
        }
    }

    #[test]
    fn rejects_duplicates_reserved_headers_controls_and_unsafe_urls() {
        for headers in [
            json!([{"name":"TOKEN","value":"a"},{"name":"token","value":"b"}]),
            json!([{"name":"Mcp-Session-Id","value":"forged"}]),
            json!([{"name":"Authorization","value":"a\r\nb"}]),
        ] {
            assert!(parse(json!([{"type":"http","name":"remote",
                "url":"https://example.invalid/mcp","headers":headers}])).is_err());
        }
        for url in ["file:///secret", "https://user:secret@example.invalid", "https://example.invalid/#x"] {
            let error = parse(json!([{"type":"http","name":"remote","url":url}])).unwrap_err();
            assert!(!error.contains("secret"));
        }
        assert!(parse(json!([{"name":"one","command":command(),"env":[
            {"name":"PATH","value":"a"},{"name":"Path","value":"b"}]}])).is_err());
        assert!(parse(json!([{"name":"bad\nname","command":"x"}])).is_err());
    }

    #[test]
    fn bounds_count_and_total_input_before_cloning() {
        let repeated = vec![json!({"name":"one","command":"x"}); MAX_ACP_MCP_SERVERS + 1];
        assert!(parse(json!(repeated)).unwrap_err().contains("at most"));
        assert!(parse(json!([{"name":"one","command":"x",
            "args":["x".repeat(MAX_MCP_CONFIG_BYTES)]}])).unwrap_err().contains("byte"));
    }

    #[test]
    fn trust_identity_is_workspace_definition_and_provenance_bound() {
        let raw = json!({"mcpServers":[{"name":"one","command":command()}]});
        let mut servers = parse_acp_servers(&raw, Path::new("/workspace")).unwrap().unwrap();
        let server = &mut servers[0];
        let first = server.fingerprint(Path::new("/workspace"));
        assert_ne!(first, server.fingerprint(Path::new("/other-workspace")));
        server.provenance = Provenance::Cli;
        assert_ne!(first, server.fingerprint(Path::new("/workspace")));
        server.provenance = Provenance::Acp;
        server.args.push("changed".into());
        assert_ne!(first, server.fingerprint(Path::new("/workspace")));
    }
}
