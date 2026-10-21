//! Discovery-to-execution regressions for imported server settings.

use std::sync::atomic::AtomicUsize;

use async_trait::async_trait;
use serde_json::json;

use super::*;

struct ConfigProbeTransport {
    requests: Arc<Mutex<Vec<(String, Duration)>>>,
    closed: AtomicBool,
}

#[async_trait]
impl McpTransport for ConfigProbeTransport {
    async fn request(&self, method: &str, _params: Value, timeout: Duration) -> Result<Value> {
        McpManager::lock(&self.requests).push((method.to_string(), timeout));
        match method {
            "initialize" => Ok(json!({})),
            "tools/list" => Ok(json!({
                "tools": [{"name":"echo","inputSchema":{"type":"object"}}]
            })),
            "tools/call" => Ok(json!({"content":[]})),
            "resources/list" => Ok(json!({"resources":[]})),
            _ => Err(tool_err("MCP_PROTOCOL", "unexpected probe method")),
        }
    }

    async fn notify(&self, _method: &str, _params: Value) -> Result<()> {
        Ok(())
    }

    fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    fn abort(&self) {
        self.closed.store(true, Ordering::Release);
    }

    async fn close(&self) {
        self.abort();
    }

    fn diagnostics_tail(&self) -> String {
        String::new()
    }
}

fn imported_manager(temp: &tempfile::TempDir, config: &Value) -> McpManager {
    let cwd = temp.path().join("project");
    let global = temp.path().join("global");
    std::fs::create_dir_all(cwd.join(".omp")).expect("project configuration directory");
    std::fs::create_dir_all(&global).expect("global directory");
    std::fs::write(cwd.join(".omp/mcp.json"), config.to_string()).expect("OMP configuration");
    let discovery = crate::mcp::config::discover(&cwd, &global, &[], true);
    assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
    McpManager::new(&cwd, &global, discovery)
}

#[cfg(unix)]
#[test]
fn imported_cwd_and_timeout_reach_trust_setup_tools_and_context() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let server_cwd = temp.path().join("project/server");
    std::fs::create_dir_all(&server_cwd).expect("server cwd");
    let executable = server_cwd.join("serve");
    std::fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("server executable");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))
        .expect("executable permissions");
    let manager = imported_manager(
        &temp,
        &json!({"mcpServers":{"fixture":{
            "command":"./serve", "cwd":"server", "timeout":120000
        }}}),
    );
    let entry = manager.entry("fixture").expect("imported server");
    assert_eq!(
        entry.effective_cwd(&manager.inner.cwd),
        server_cwd.canonicalize().expect("canonical cwd")
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let probe_requests = Arc::clone(&requests);
    *McpManager::lock(&manager.inner.transport_factory) = Some(Arc::new(move || {
        Box::new(ConfigProbeTransport {
            requests: Arc::clone(&probe_requests),
            closed: AtomicBool::new(false),
        }) as Box<dyn McpTransport>
    }));
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        // Trust and transport construction both resolve ./serve in server/;
        // no such executable exists in the session workspace itself.
        manager
            .trust("fixture")
            .await
            .expect("trust selected executable");
        manager
            .refresh_tools("fixture")
            .await
            .expect("refresh catalog");
        manager
            .call_tool("fixture", "echo", json!({}))
            .await
            .expect("tool call");
        manager
            .list_resources("fixture", None)
            .await
            .expect("context call");
        manager.shutdown_all().await;
    });
    let recorded = McpManager::lock(&requests);
    for method in ["initialize", "tools/list", "tools/call", "resources/list"] {
        let timeouts: Vec<_> = recorded
            .iter()
            .filter(|(name, _)| name == method)
            .map(|(_, timeout)| *timeout)
            .collect();
        assert!(!timeouts.is_empty(), "missing {method}");
        for timeout in timeouts {
            assert!(
                timeout > DEFAULT_MCP_TIMEOUT && timeout <= Duration::from_secs(120),
                "{method}: {timeout:?}"
            );
        }
    }
}

#[test]
fn disabled_import_cannot_be_trusted_started_mounted_or_replaced_by_an_extension() {
    let temp = tempfile::tempdir().expect("tempdir");
    let manager = imported_manager(
        &temp,
        &json!({"mcpServers":{"fixture":{
            "url":"https://fixture.invalid/mcp", "enabled":false
        }}}),
    );
    let entry = manager.entry("fixture").expect("disabled entry");
    // Even a persisted acknowledgment must not override disablement.
    TrustStore::load(&manager.inner.trust_path)
        .expect("trust store")
        .acknowledge(
            "fixture",
            &manager.trust_fingerprint_for(&entry),
            "operator",
        )
        .expect("preexisting acknowledgment");
    let constructions = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&constructions);
    *McpManager::lock(&manager.inner.transport_factory) = Some(Arc::new(move || {
        count.fetch_add(1, Ordering::AcqRel);
        Box::new(ConfigProbeTransport {
            requests: Arc::new(Mutex::new(Vec::new())),
            closed: AtomicBool::new(false),
        }) as Box<dyn McpTransport>
    }));
    manager.register_extension_server("fixture", &json!({"url":"https://other.invalid/mcp"}));
    assert!(
        !manager
            .entry("fixture")
            .expect("original entry")
            .config
            .enabled
    );
    assert_eq!(manager.list()[0].health, "disabled");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        manager.connect_trusted().await;
        for result in [
            manager.trust("fixture").await.map(|_| ()),
            manager.refresh_tools("fixture").await.map(|_| ()),
            manager
                .call_tool("fixture", "echo", json!({}))
                .await
                .map(|_| ()),
            manager.list_resources("fixture", None).await.map(|_| ()),
        ] {
            let error = result.expect_err("disabled server must be refused");
            assert!(error.to_string().contains("MCP_SERVER_DISABLED"), "{error}");
        }
    });
    assert_eq!(constructions.load(Ordering::Acquire), 0);
    assert!(manager.mounted_tool_metas().is_empty());
    assert!(manager.context_server_names(None).is_empty());
}

#[test]
fn workspace_templates_are_literal_data_and_do_not_follow_server_cwd() {
    let workspace = Path::new("/session/project");
    let server_cwd = Path::new("/session/project/server");
    let resolved = resolve_server_secrets(
        &[("ROOT".to_string(), "${workspaceFolder}".to_string())],
        Provenance::Foreign,
        crate::mcp::config::validate_env_value,
        server_cwd,
        workspace,
    )
    .expect("literal template expansion");
    assert_eq!(resolved[0].1, "/session/project");
    let literal = resolve_server_secrets(
        &[("LITERAL".to_string(), "${workspaceFolder}".to_string())],
        Provenance::Foreign,
        crate::mcp::config::validate_env_value,
        server_cwd,
        Path::new("$CMD:exit 97"),
    )
    .expect("expanded values never become credential commands");
    assert_eq!(literal[0].1, "$CMD:exit 97");
}
