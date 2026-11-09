//! Discovery-to-execution regressions for imported server settings.

use std::sync::atomic::AtomicUsize;

use async_trait::async_trait;
use serde_json::json;

use super::*;

struct ConfigProbeTransport {
    requests: Arc<Mutex<Vec<(String, Duration)>>>,
    closed: AtomicBool,
    initialize_result: Option<Value>,
    workspace: Option<Arc<Mutex<Option<PathBuf>>>>,
}

#[async_trait]
impl McpTransport for ConfigProbeTransport {
    fn configure_workspace_root(&self, workspace: &Path) -> Result<()> {
        if let Some(selected) = &self.workspace {
            *McpManager::lock(selected) = Some(workspace.to_path_buf());
        }
        Ok(())
    }

    fn client_capabilities(&self) -> Value {
        if self.workspace.is_some() {
            json!({"roots":{"listChanged":false}})
        } else {
            json!({})
        }
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        McpManager::lock(&self.requests).push((method.to_string(), timeout));
        match method {
            "initialize" => {
                assert_eq!(params["protocolVersion"], "2025-11-25");
                assert_eq!(params["capabilities"], self.client_capabilities());
                Ok(self.initialize_result.clone().unwrap_or_else(|| {
                    json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": {},
                        "serverInfo": {"name": "config-fixture", "version": "1"}
                    })
                }))
            }
            "tools/list" => Ok(json!({
                "tools": [{"name":"echo","inputSchema":{"type":"object"}}]
            })),
            "tools/call" => Ok(json!({"content":[]})),
            "resources/list" => Ok(json!({"resources":[]})),
            _ => Err(tool_err("MCP_PROTOCOL", "unexpected probe method")),
        }
    }

    async fn notify(&self, method: &str, _params: Value) -> Result<()> {
        McpManager::lock(&self.requests).push((method.to_string(), Duration::ZERO));
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
const EXECUTION_PEER: &str = r#"#!/bin/sh
printf '%s\n' "$0" > invoked
printf '%s\n' "$PATH" > child-path
printf '%s\n' "$TOKEN" > child-token
while IFS= read -r line; do
    case "$line" in
        *'"id":'*) id=${line#*'"id":'}; id=${id%%,*}; id=${id%%\}*} ;;
        *) continue ;;
    esac
    case "$line" in
        *'"method":"initialize"'*) result='{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"execution-probe","version":"1"}}' ;;
        *'"method":"tools/list"'*) result='{"tools":[{"name":"probe","inputSchema":{"type":"object"}}]}' ;;
        *'"method":"tools/call"'*) result='{"content":[{"type":"text","text":"approved"}]}' ;;
        *) continue ;;
    esac
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#;

#[cfg(unix)]
fn write_execution_peer(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, EXECUTION_PEER).expect("stdio execution peer");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("executable peer");
}

#[cfg(unix)]
#[test]
fn native_stdio_preserves_the_approved_invocation_alias_and_child_environment() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server_cwd = temp.path().join("project/server runtime");
    let shadow = server_cwd.join("shadow");
    std::fs::create_dir_all(&shadow).expect("server cwd and child PATH");
    write_execution_peer(&server_cwd.join("implementation"));
    write_execution_peer(&shadow.join("serve"));
    std::os::unix::fs::symlink("implementation", server_cwd.join("serve"))
        .expect("server invocation alias");
    let manager = imported_manager(
        &temp,
        &json!({"mcpServers":{"fixture":{
            "command":"./serve", "cwd":"server runtime",
            "env":{"PATH":shadow,"TOKEN":"$CMD:printf admitted-token"}
        }}}),
    );
    let entry = manager.entry("fixture").expect("server");
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime");
    let output = runtime
        .block_on(async {
            let result = async {
                manager.trust("fixture").await?;
                manager.call_tool("fixture", "probe", json!({})).await
            }
            .await;
            manager.shutdown_all().await;
            result
        })
        .expect("native server remains usable");
    assert_eq!(output["content"][0]["text"], "approved");
    assert_eq!(
        std::fs::read_to_string(server_cwd.join("invoked"))
            .expect("invocation marker")
            .trim_end(),
        entry
            .effective_cwd(&manager.inner.cwd)
            .join("serve")
            .to_str()
            .expect("UTF-8 alias"),
        "canonicalizing the executable for launch would break alias-sensitive programs",
    );
    assert_eq!(
        std::fs::read_to_string(server_cwd.join("child-path"))
            .expect("child PATH")
            .trim_end(),
        shadow.to_str().expect("UTF-8 child PATH"),
    );
    assert_eq!(
        std::fs::read_to_string(server_cwd.join("child-token"))
            .expect("child token")
            .trim_end(),
        "admitted-token",
    );
}

#[cfg(unix)]
#[test]
fn native_stdio_bare_command_keeps_its_target_when_child_path_contains_a_shadow() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server_cwd = temp.path().join("project/server runtime");
    let shadow = server_cwd.join("shadow");
    std::fs::create_dir_all(&shadow).expect("server cwd and child PATH");
    let peer = server_cwd
        .canonicalize()
        .expect("canonical server cwd")
        .join("peer script.sh");
    write_execution_peer(&peer);
    write_execution_peer(&shadow.join("sh"));
    let manager = imported_manager(
        &temp,
        &json!({"mcpServers":{"fixture":{
            "command":"sh", "args":[peer], "cwd":"server runtime",
            "env":{"PATH":shadow}
        }}}),
    );
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime");
    let output = runtime
        .block_on(async {
            let result = async {
                manager.trust("fixture").await?;
                manager.call_tool("fixture", "probe", json!({})).await
            }
            .await;
            manager.shutdown_all().await;
            result
        })
        .expect("the trusted shell interprets the selected peer");
    assert_eq!(output["content"][0]["text"], "approved");
    // The real shell sets the script's $0 to the peer argument. A fresh
    // command lookup in the child PATH would run shadow/sh as the script.
    assert_eq!(
        std::fs::read_to_string(server_cwd.join("invoked"))
            .expect("invocation marker")
            .trim_end(),
        peer.to_str().expect("UTF-8 peer path"),
    );
    assert_eq!(
        std::fs::read_to_string(server_cwd.join("child-path"))
            .expect("child PATH")
            .trim_end(),
        shadow.to_str().expect("UTF-8 child PATH"),
    );
}

#[cfg(unix)]
#[test]
fn executable_changes_before_or_during_credentials_never_start_the_stdio_server() {
    for before_helper in [true, false] {
        let temp = tempfile::tempdir().expect("tempdir");
        let server_cwd = temp.path().join("project/server");
        std::fs::create_dir_all(&server_cwd).expect("server cwd");
        let executable = server_cwd.join("serve");
        let replacement = server_cwd.join("replacement");
        write_execution_peer(&executable);
        std::fs::write(
            &replacement,
            format!("{EXECUTION_PEER}\n# changed executable\n"),
        )
        .expect("replacement peer");
        let manager = imported_manager(
            &temp,
            &json!({"mcpServers":{"fixture":{
                "command":"./serve", "cwd":"server",
                "env":{"TOKEN":"$CMD:cat replacement > serve; printf ready > helper-ran; printf token"}
            }}}),
        );
        let entry = manager.entry("fixture").expect("server");
        let identity = entry
            .config
            .execution_identity(&entry.effective_cwd(&manager.inner.cwd))
            .expect("executable identity")
            .expect("stdio identity");
        TrustStore::load(&manager.inner.trust_path)
            .expect("trust store")
            .acknowledge_execution(
                "fixture",
                &manager.trust_fingerprint_for(&entry),
                "operator",
                identity,
            )
            .expect("acknowledge original executable");
        if before_helper {
            std::fs::write(
                &executable,
                std::fs::read(&replacement).expect("replacement bytes"),
            )
            .expect("change after acknowledgment");
        }
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let outcome = runtime.block_on(async {
            let result = manager.ensure_ready(&entry).await;
            manager.shutdown_all().await;
            result
        });
        let error = outcome.expect_err("changed executable must not be launched");
        assert!(
            error.to_string().contains("MCP_TRUST_IDENTITY_MISMATCH"),
            "{error}"
        );
        assert_eq!(server_cwd.join("helper-ran").exists(), !before_helper);
        assert!(
            !server_cwd.join("invoked").exists(),
            "no server side effect"
        );
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(manager.mounted_tool_metas().is_empty());
    }
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
            "command":"./serve", "cwd":"server", "timeout":120_000
        }}}),
    );
    let entry = manager.entry("fixture").expect("imported server");
    assert_eq!(
        entry.effective_cwd(&manager.inner.cwd),
        server_cwd.canonicalize().expect("canonical cwd")
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let probe_requests = Arc::clone(&requests);
    let workspace = Arc::new(Mutex::new(None));
    let probe_workspace = Arc::clone(&workspace);
    *McpManager::lock(&manager.inner.transport_factory) = Some(Arc::new(move || {
        Box::new(ConfigProbeTransport {
            requests: Arc::clone(&probe_requests),
            closed: AtomicBool::new(false),
            initialize_result: None,
            workspace: Some(Arc::clone(&probe_workspace)),
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
    assert_eq!(
        McpManager::lock(&workspace).as_ref(),
        Some(&manager.inner.cwd)
    );
    assert_ne!(McpManager::lock(&workspace).as_ref(), Some(&server_cwd));
    for method in ["initialize", "tools/list", "tools/call", "resources/list"] {
        let timeouts: Vec<_> = McpManager::lock(&requests)
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
            initialize_result: None,
            workspace: None,
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

#[test]
fn unsupported_or_malformed_handshakes_never_initialize_or_publish_a_connection() {
    for response in [
        json!({}),
        json!({"protocolVersion":"2099-01-01","capabilities":{},"serverInfo":{"name":"future","version":"1"}}),
        json!({"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{}}),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let manager = imported_manager(
            &temp,
            &json!({"mcpServers":{"fixture":{
                "url":"https://fixture.invalid/mcp"
            }}}),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let probe_requests = Arc::clone(&requests);
        *McpManager::lock(&manager.inner.transport_factory) = Some(Arc::new(move || {
            Box::new(ConfigProbeTransport {
                requests: Arc::clone(&probe_requests),
                closed: AtomicBool::new(false),
                initialize_result: Some(response.clone()),
                workspace: None,
            }) as Box<dyn McpTransport>
        }));
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(manager.trust("fixture"))
            .expect_err("invalid handshake");
        assert!(error.to_string().contains("MCP_PROTOCOL"), "{error}");
        let entry = manager.entry("fixture").expect("entry");
        assert!(McpManager::lock(&entry.transport).is_none());
        assert!(McpManager::lock(&entry.tools_cache).is_none());
        assert!(manager.mounted_tool_metas().is_empty());
        assert!(matches!(
            &*McpManager::lock(&entry.health),
            ServerHealth::Unhealthy { reason, retries: 1 } if reason.contains("MCP_PROTOCOL")
        ));
        let recorded = McpManager::lock(&requests);
        assert_eq!(
            recorded.len(),
            1,
            "no initialized notification or tool discovery after rejection"
        );
        assert_eq!(recorded[0].0, "initialize");
    }
}

#[test]
fn fixed_server_cwd_does_not_transfer_root_trust_to_another_workspace() {
    let temp = tempfile::tempdir().expect("tempdir");
    let server_cwd = temp.path().join("server");
    let second_workspace = temp.path().join("other-project");
    std::fs::create_dir(&server_cwd).expect("server cwd");
    std::fs::create_dir(&second_workspace).expect("second workspace");
    let manager = imported_manager(
        &temp,
        &json!({"mcpServers":{"fixture":{
            "url":"https://fixture.invalid/mcp", "cwd":server_cwd
        }}}),
    );
    let entry = manager.entry("fixture").expect("first entry");
    let other = McpManager::new(
        &second_workspace,
        &temp.path().join("global"),
        McpDiscovery {
            servers: vec![entry.config.clone()],
            warnings: Vec::new(),
        },
    );
    let other_entry = other.entry("fixture").expect("other entry");
    assert_eq!(
        entry.effective_cwd(&manager.inner.cwd),
        other_entry.effective_cwd(&other.inner.cwd)
    );
    assert_ne!(
        manager.trust_fingerprint_for(&entry),
        other.trust_fingerprint_for(&other_entry)
    );
    TrustStore::load(&manager.inner.trust_path)
        .expect("trust store")
        .acknowledge(
            "fixture",
            &manager.trust_fingerprint_for(&entry),
            "operator",
        )
        .expect("first workspace acknowledgment");
    manager
        .check_trust(&entry)
        .expect("selected workspace trusted");
    let error = other
        .check_trust(&other_entry)
        .expect_err("different root needs its own acknowledgment");
    assert!(error.to_string().contains("MCP_TRUST_PENDING"), "{error}");
    assert!(
        McpManager::lock_trust_for_execution(
            &other_entry.config,
            &other_entry.effective_cwd(&other.inner.cwd),
            &other.inner.cwd,
            &other.inner.trust_path,
        )
        .is_err(),
        "the final construction gate must bind the same root"
    );
}
