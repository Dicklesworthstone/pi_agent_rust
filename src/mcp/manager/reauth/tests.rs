//! Exercise the operator path with a real loopback OAuth issuer and MCP
//! server, including disk reload, client-owned definitions, and withdrawal.

use super::*;
use crate::mcp::auth::configured_tests::{
    CapturedRequest, OAuthFixture, browser_callback, fixture_response, http_response,
};
use crate::mcp::config::McpDiscovery;
use crate::mcp::trust::TrustDecision;
use serde_json::{Value, json};
use std::ffi::OsString;

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 3)
        .build()
        .expect("reauthentication runtime")
}

fn json_response(status: &str, value: &Value) -> String {
    http_response(status, "", "application/json", &value.to_string())
}

fn live_response(origin: &str, methods: &[String], request: &CapturedRequest) -> String {
    if request.method == "POST" && request.target == "/token" {
        return json_response("200 OK", &json!({
            "access_token":"managed-access", "refresh_token":"managed-refresh",
            "token_type":"Bearer", "expires_in":3600
        }));
    }
    if request.target != "/mcp" {
        return fixture_response(origin, methods, request);
    }
    if request.headers.get("authorization").map(String::as_str) != Some("Bearer managed-access") {
        if request.method == "GET" {
            return fixture_response(origin, methods, request);
        }
        return json_response("401 Unauthorized", &json!({"error":"expired bearer"}));
    }
    match request.method.as_str() {
        "GET" => http_response("405 Method Not Allowed", "", "text/plain", ""),
        "DELETE" => http_response("204 No Content", "", "text/plain", ""),
        "POST" => {
            let rpc: Value = serde_json::from_slice(&request.body).expect("MCP request");
            let result = match rpc["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion":crate::mcp::transport::MCP_PROTOCOL_VERSION,
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"reauth-fixture","version":"1"}
                }),
                Some("notifications/initialized" | "notifications/cancelled") => {
                    return http_response("202 Accepted", "", "text/plain", "");
                }
                Some("tools/list") => json!({"tools":[{
                    "name":"echo", "description":"Authenticated fixture tool",
                    "inputSchema":{"type":"object"}
                }]}),
                Some("tools/call") => json!({"content":[{"type":"text","text":"authenticated"}]}),
                _ => panic!("unexpected MCP request: {rpc}"),
            };
            json_response("200 OK", &json!({"jsonrpc":"2.0","id":rpc["id"],"result":result}))
        }
        _ => panic!("unexpected HTTP method"),
    }
}

fn server() -> OAuthFixture {
    OAuthFixture::serve(&["none"], live_response)
}

fn browser(fixture: &OAuthFixture) -> Arc<dyn Fn(&str) + Send + Sync> {
    browser_callback(Arc::new(Mutex::new(Vec::new())), format!("{}/", fixture.origin))
}

fn acknowledge(manager: &McpManager) -> Arc<ServerEntry> {
    let entry = manager.entry("remote").expect("selected server");
    TrustStore::load(&manager.inner.trust_path).expect("load trust")
        .acknowledge("remote", &manager.trust_fingerprint_for(&entry), "operator")
        .expect("acknowledge selected server");
    entry
}

fn file_manager(temp: &tempfile::TempDir, url: &str) -> (McpManager, PathBuf, PathBuf) {
    let workspace = temp.path().join("project");
    let global = temp.path().join("global");
    std::fs::create_dir_all(workspace.join(".pi")).expect("project config directory");
    std::fs::create_dir_all(&global).expect("global directory");
    let source = workspace.join(".pi/mcp.json");
    let shadow = global.join("mcp.json");
    std::fs::write(&source, json!({
        "$schema":"fixture-schema", "clientFlags":{"preserve":true},
        "mcpServers":{
            "remote":{"url":url,"cwd":"${workspaceFolder}","timeout":5000,
                "headers":{"aUtHoRiZaTiOn":"Bearer expired","X-Tenant":"workspace"},
                "oauth":{"clientId":"fixture-public-client"}},
            "untouched":{"url":format!("{url}/other"),"headers":{"Authorization":"Bearer other"}}
        }
    }).to_string()).expect("winning project definition");
    std::fs::write(&shadow, json!({"mcpServers":{"remote":{
        "url":"http://127.0.0.1:9/mcp", "headers":{"Authorization":"Bearer global"}
    }}}).to_string()).expect("shadowed global definition");
    let manager = McpManager::bootstrap(&workspace, &global, &[], true).expect("discover manager");
    assert!(manager.warnings().is_empty(), "{:?}", manager.warnings());
    acknowledge(&manager);
    (manager, source, shadow)
}

fn auth_snapshot(manager: &McpManager) -> Vec<(OsString, Vec<u8>)> {
    let directory = manager.inner.global_dir.join("mcp-auth");
    if !directory.exists() {
        return Vec::new();
    }
    let mut files: Vec<_> = std::fs::read_dir(directory).expect("credential directory")
        .map(|entry| entry.expect("credential entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .map(|path| (path.file_name().expect("credential name").to_os_string(),
            std::fs::read(&path).expect("credential bytes")))
        .collect();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

#[test]
fn reauth_repairs_only_the_winning_source_and_restores_live_and_reloaded_tools() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let fixture = server();
    let (manager, source, shadow) = file_manager(&temp, &fixture.resource());
    let shadow_before = std::fs::read(&shadow).expect("shadow bytes");
    let old = manager.entry("remote").expect("old definition");
    let old_fingerprint = manager.trust_fingerprint_for(&old);
    runtime().block_on(async {
        manager.refresh_tools("remote").await.expect_err("stale bearer is rejected");
        manager.reauthenticate("remote", None, browser(&fixture)).await.expect("explicit reauth");
        let current = manager.entry("remote").expect("repaired definition");
        assert!(!Arc::ptr_eq(&old, &current));
        assert!(!has_authorization(&current.config));
        assert_eq!(manager.mounted_tool_metas().len(), 1);
        let result = manager.call_tool("remote", "echo", json!({})).await.expect("authenticated tool");
        assert_eq!(result["content"][0]["text"], "authenticated");
        let source_value: Value = serde_json::from_slice(&std::fs::read(&source).expect("source")).expect("source JSON");
        assert_eq!(source_value["mcpServers"]["remote"]["headers"], json!({"X-Tenant":"workspace"}));
        assert_eq!(source_value["mcpServers"]["remote"]["cwd"], "${workspaceFolder}");
        assert_eq!(source_value["mcpServers"]["remote"]["oauth"]["clientId"], "fixture-public-client");
        assert_eq!(source_value["mcpServers"]["untouched"]["headers"]["Authorization"], "Bearer other");
        assert_eq!(source_value["clientFlags"]["preserve"], true);
        assert_eq!(std::fs::read(&shadow).expect("shadow after reauth"), shadow_before);
        let trust = TrustStore::load(&manager.inner.trust_path).expect("updated trust");
        assert_eq!(trust.decision("remote", &old_fingerprint), TrustDecision::Pending);
        assert_eq!(trust.decision("remote", &manager.trust_fingerprint_for(&current)), TrustDecision::Acknowledged);
        assert_eq!(trust.records()["remote"].audit.last().expect("repair audit").action, "reauthenticated");
        manager.shutdown_all().await;

        let reloaded = McpManager::bootstrap(&manager.inner.cwd, &manager.inner.global_dir, &[], true)
            .expect("reload source");
        assert_eq!(reloaded.refresh_tools("remote").await.expect("persisted grant after restart").len(), 1);
        reloaded.unauthenticate("remote").await.expect("logout repaired definition");
        reloaded.refresh_tools("remote").await.expect_err("logout cannot revive the removed bearer");
        reloaded.shutdown_all().await;
    });
    let requests = fixture.captured();
    let initializations: Vec<_> = requests.iter().filter(|request| {
        request.target == "/mcp" && request.method == "POST"
            && serde_json::from_slice::<Value>(&request.body).expect("RPC")["method"] == "initialize"
    }).collect();
    assert_eq!(initializations.len(), 3, "stale attempt, repaired session, and disk reload");
    assert_eq!(initializations[0].headers.get("authorization").map(String::as_str), Some("Bearer expired"));
    assert!(initializations[1..].iter().all(|request| {
        request.headers.get("authorization").map(String::as_str) == Some("Bearer managed-access")
            && request.headers.get("x-tenant").map(String::as_str) == Some("workspace")
    }));
    assert_eq!(requests.iter().filter(|request| request.target == "/token").count(), 1);
    assert!(!requests.iter().any(|request| request.target == "/register"));
}

#[test]
fn client_owned_headers_are_repaired_without_creating_or_rewriting_a_config() {
    for provenance in [Provenance::Acp, Provenance::Extension] {
        let temp = tempfile::tempdir().expect("fixture directory");
        let fixture = server();
        let workspace = temp.path().join("project");
        let global = temp.path().join("global");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&global).expect("global");
        let manager = if provenance == Provenance::Acp {
            let servers = crate::mcp::config::parse_acp_servers(&json!({"mcpServers":[{
                "name":"remote", "type":"http", "url":fixture.resource(),
                "headers":[{"name":"Authorization","value":"Bearer expired"}]
            }]}), &workspace).expect("ACP definitions").expect("ACP array");
            McpManager::new(&workspace, &global, McpDiscovery { servers, warnings:Vec::new() })
        } else {
            let manager = McpManager::new(&workspace, &global, McpDiscovery::default());
            manager.register_extension_server("remote", &json!({
                "url":fixture.resource(), "headers":{"Authorization":"Bearer expired"},
                "extension_id":"fixture"
            }));
            manager
        };
        let entry = acknowledge(&manager);
        let original_trust = std::fs::read(&manager.inner.trust_path).expect("trust snapshot");
        runtime().block_on(async {
            manager.reauthenticate("remote", None, browser(&fixture)).await.expect("client-owned reauth");
            assert!(Arc::ptr_eq(&entry, &manager.entry("remote").expect("same definition")));
            assert!(has_authorization(&entry.config), "client remains owner of its definition");
            assert_eq!(manager.mounted_tool_metas().len(), 1);
            assert_eq!(std::fs::read(&manager.inner.trust_path).expect("trust after login"), original_trust);
            manager.unauthenticate("remote").await.expect("client-owned logout");
            let count = fixture.captured().len();
            manager.refresh_tools("remote").await.expect_err("tombstone suppresses authored bearer");
            assert_eq!(fixture.captured().len(), count);
            manager.shutdown_all().await;
        });
        assert!(!workspace.join(".pi/mcp.json").exists());
        assert!(!global.join("mcp.json").exists());
        assert!(!entry.config.source_file.exists());
    }
}

#[test]
fn editing_the_source_during_browser_login_preserves_the_edit_and_previous_trust() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let fixture = server();
    let (manager, source, _) = file_manager(&temp, &fixture.resource());
    let trust_before = std::fs::read(&manager.inner.trust_path).expect("trust");
    let auth_before = auth_snapshot(&manager);
    let mut edited: Value = serde_json::from_slice(&std::fs::read(&source).expect("source")).expect("source JSON");
    edited["clientFlags"]["editedDuringLogin"] = json!(true);
    let edited = edited.to_string();
    let edit_path = source.clone();
    let write = edited.clone();
    let callback = browser(&fixture);
    let callback = Arc::new(move |url: &str| {
        std::fs::write(&edit_path, &write).expect("concurrent operator edit");
        callback(url);
    });
    runtime().block_on(async {
        let error = manager.reauthenticate("remote", None, callback).await.expect_err("source changed during login");
        assert!(error.to_string().contains("MCP_AUTH_SOURCE_CHANGED"), "{error}");
        manager.shutdown_all().await;
    });
    assert_eq!(std::fs::read_to_string(source).expect("preserved edit"), edited);
    assert_eq!(std::fs::read(&manager.inner.trust_path).expect("trust"), trust_before);
    assert_eq!(auth_snapshot(&manager), auth_before);
    assert!(!fixture.captured().iter().any(|request| request.target == "/token"));
}

#[test]
fn denial_during_browser_login_prevents_grant_and_source_publication() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let fixture = server();
    let (manager, source, _) = file_manager(&temp, &fixture.resource());
    let source_before = std::fs::read(&source).expect("source");
    let entry = manager.entry("remote").expect("entry");
    let fingerprint = manager.trust_fingerprint_for(&entry);
    let trust_path = manager.inner.trust_path.clone();
    let denied_fingerprint = fingerprint.clone();
    let callback = browser(&fixture);
    let callback = Arc::new(move |url: &str| {
        TrustStore::load(&trust_path).expect("peer trust store")
            .deny("remote", &denied_fingerprint, "operator").expect("peer denial");
        callback(url);
    });
    runtime().block_on(async {
        let error = manager.reauthenticate("remote", None, callback).await.expect_err("denied during browser flow");
        assert!(error.to_string().contains("MCP_TRUST_DENIED"), "{error}");
        manager.shutdown_all().await;
    });
    assert_eq!(std::fs::read(source).expect("source after denial"), source_before);
    assert!(auth_snapshot(&manager).is_empty());
    assert_eq!(TrustStore::load(&manager.inner.trust_path).expect("trust")
        .decision("remote", &fingerprint), TrustDecision::Denied);
}

fn observed_url() -> (
    Arc<dyn Fn(&str) + Send + Sync>,
    futures::channel::oneshot::Receiver<String>,
) {
    let (sender, receiver) = futures::channel::oneshot::channel();
    let sender = Mutex::new(Some(sender));
    (Arc::new(move |url: &str| {
        if let Some(sender) = sender.lock().expect("URL sender").take() {
            sender.send(url.to_string()).expect("browser observer");
        }
    }), receiver)
}

#[test]
fn cancelling_a_pending_browser_flow_leaves_source_trust_and_credentials_unchanged() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let fixture = server();
    let (manager, source, _) = file_manager(&temp, &fixture.resource());
    let source_before = std::fs::read(&source).expect("source");
    let trust_before = std::fs::read(&manager.inner.trust_path).expect("trust");
    let auth_before = auth_snapshot(&manager);
    runtime().block_on(async {
        let (observe, received) = observed_url();
        let login = Box::pin(manager.reauthenticate("remote", None, observe));
        match futures::future::select(received, login).await {
            futures::future::Either::Left((Ok(_), login)) => drop(login),
            _ => panic!("browser wait must be reached before cancellation"),
        }
        manager.shutdown_all().await;
    });
    assert_eq!(std::fs::read(source).expect("source"), source_before);
    assert_eq!(std::fs::read(&manager.inner.trust_path).expect("trust"), trust_before);
    assert_eq!(auth_snapshot(&manager), auth_before);
    assert!(!fixture.captured().iter().any(|request| request.target == "/token"));
}

#[test]
fn logout_while_the_browser_waits_fences_late_login_and_never_restores_static_auth() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let fixture = server();
    let (manager, source, _) = file_manager(&temp, &fixture.resource());
    let source_before = std::fs::read(&source).expect("source");
    let trust_before = std::fs::read(&manager.inner.trust_path).expect("trust");
    runtime().block_on(async {
        let (observe, received) = observed_url();
        let login = Box::pin(manager.reauthenticate("remote", None, observe));
        let (url, login) = match futures::future::select(received, login).await {
            futures::future::Either::Left((Ok(url), login)) => (url, login),
            _ => panic!("browser wait must precede logout"),
        };
        manager.unauthenticate("remote").await.expect("explicit logout during pending login");
        let logged_out = auth_snapshot(&manager);
        assert_eq!(logged_out.len(), 1, "logout persists a durable tombstone");
        browser(&fixture)(&url);
        let error = login.await.expect_err("late browser login cannot overwrite logout");
        assert!(error.to_string().contains("MCP_AUTH_SUPERSEDED"), "{error}");
        assert_eq!(auth_snapshot(&manager), logged_out);
        let before = fixture.captured().len();
        manager.refresh_tools("remote").await.expect_err("static bearer remains revoked");
        assert_eq!(fixture.captured().len(), before);
        manager.shutdown_all().await;
    });
    assert_eq!(std::fs::read(source).expect("source"), source_before);
    assert_eq!(std::fs::read(&manager.inner.trust_path).expect("trust"), trust_before);
}

#[test]
fn source_compare_and_swap_checks_identity_bytes_and_final_withdrawal() {
    for change in ["same-bytes-new-file", "edited-in-place", "withdrawn-at-commit"] {
        let temp = tempfile::tempdir().expect("fixture directory");
        let (manager, source, _) = file_manager(&temp, "http://127.0.0.1:9/mcp");
        let entry = manager.entry("remote").expect("entry");
        let repair = SourceRepair::prepare(&entry.config, &manager.inner.cwd).expect("capture source");
        let original = std::fs::read(&source).expect("original source");
        let expected = if change == "same-bytes-new-file" {
            std::fs::rename(&source, source.with_extension("original.json")).expect("move original inode");
            std::fs::write(&source, &original).expect("replacement with identical bytes");
            original
        } else if change == "edited-in-place" {
            let mut edited = original;
            edited.push(b' ');
            std::fs::write(&source, &edited).expect("editor write");
            edited
        } else {
            original
        };
        let admissions = std::sync::atomic::AtomicUsize::new(0);
        let result = repair.commit(&|| {
            if admissions.fetch_add(1, Ordering::AcqRel) == 1 {
                return Err(super::super::tool_err("MCP_CANCELLED", "fixture withdrawal"));
            }
            Ok(())
        });
        assert!(result.is_err(), "{change} must refuse source publication");
        assert!(!repair.committed.load(Ordering::Acquire));
        assert_eq!(std::fs::read(source).expect("source after CAS refusal"), expected);
    }
}

#[cfg(unix)]
#[test]
fn replacing_the_parent_directory_cannot_redirect_source_publication() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let (manager, source, _) = file_manager(&temp, "http://127.0.0.1:9/mcp");
    let entry = manager.entry("remote").expect("entry");
    let repair = SourceRepair::prepare(&entry.config, &manager.inner.cwd).expect("capture source");
    let original = std::fs::read(&source).expect("source");
    let parent = source.parent().expect("source parent");
    let moved = parent.with_file_name("original-parent");
    std::fs::rename(parent, &moved).expect("displace parent");
    std::fs::create_dir(parent).expect("replacement parent");
    std::fs::write(&source, &original).expect("replacement source");
    assert!(repair.commit(&|| Ok(())).is_err());
    assert_eq!(std::fs::read(source).expect("replacement unchanged"), original);
    assert_eq!(std::fs::read(moved.join("mcp.json")).expect("pinned original unchanged"), original);
}

#[test]
fn source_repair_preserves_authored_values_across_native_editor_and_codex_formats() {
    let spec = json!({
        "url":"http://127.0.0.1:9/mcp", "cwd":"${workspaceFolder}",
        "headers":{"Authorization":"Bearer stale","X-Keep":"${workspaceFolder}"},
        "oauth":{"clientId":"${CLIENT_ID:-fixture}"}
    });
    let servers = json!({"remote":spec,"untouched":{"url":"http://127.0.0.1:9/other"}});
    let cases = [
        (".pi/mcp.json", json!({"mcpServers":servers,"editorSetting":"preserved"}).to_string()),
        (".agents/mcp.json", servers.to_string()),
        (".vscode/mcp.json", json!({"servers":servers,"inputs":[{"id":"preserved"}]}).to_string()),
        (".vscode/mcp.json", json!({"mcp":{"servers":servers},"inputs":[{"id":"preserved"}]}).to_string()),
        (".codex/config.toml", concat!(
            "model = \"preserved\"\n",
            "[mcp_servers.remote]\nurl = \"http://127.0.0.1:9/mcp\"\ncwd = \"${workspaceFolder}\"\n",
            "[mcp_servers.remote.headers]\nAuthorization = \"Bearer stale\"\nX-Keep = \"${workspaceFolder}\"\n",
            "[mcp_servers.remote.http_headers]\nauthorization = \"Bearer shadowed-stale\"\nX-Alias = \"preserved\"\n",
            "[mcp_servers.untouched]\nurl = \"http://127.0.0.1:9/other\"\n"
        ).to_string()),
    ];
    for (relative, content) in cases {
        let temp = tempfile::tempdir().expect("source fixture");
        let workspace = temp.path().join("project");
        let global = temp.path().join("global");
        let source = workspace.join(relative);
        std::fs::create_dir_all(source.parent().expect("source parent")).expect("source directory");
        std::fs::create_dir_all(&global).expect("global directory");
        std::fs::write(&source, &content).expect("authored source");
        let manager = McpManager::bootstrap(&workspace, &global, &[], true).expect("discovery");
        assert!(manager.warnings().is_empty(), "{relative}: {:?}", manager.warnings());
        let selected = manager.entry("remote").expect("selected source");
        let repair = SourceRepair::prepare(&selected.config, &manager.inner.cwd).expect("source repair");
        let rendered = std::str::from_utf8(&repair.rendered).expect("rendered source");
        assert!(!has_authorization(&repair.repaired));
        if relative.ends_with(".toml") {
            let value: toml::Value = toml::from_str(rendered).expect("repaired TOML");
            assert_eq!(value["model"].as_str(), Some("preserved"));
            let server = &value["mcp_servers"]["remote"];
            assert_eq!(server["cwd"].as_str(), Some("${workspaceFolder}"));
            assert_eq!(server["headers"]["X-Keep"].as_str(), Some("${workspaceFolder}"));
            assert!(server["headers"].get("Authorization").is_none());
            assert!(server["http_headers"].get("authorization").is_none());
            assert_eq!(server["http_headers"]["X-Alias"].as_str(), Some("preserved"));
            assert_eq!(value["mcp_servers"]["untouched"]["url"].as_str(), Some("http://127.0.0.1:9/other"));
        } else {
            let value: Value = serde_json::from_str(rendered).expect("repaired JSON");
            let servers = value.get("mcpServers").or_else(|| value.get("servers"))
                .or_else(|| value.get("mcp").and_then(|mcp| mcp.get("servers")))
                .unwrap_or(&value);
            assert_eq!(servers["remote"]["headers"], json!({"X-Keep":"${workspaceFolder}"}));
            assert_eq!(servers["remote"]["cwd"], "${workspaceFolder}");
            assert_eq!(servers["remote"]["oauth"]["clientId"], "${CLIENT_ID:-fixture}");
            assert_eq!(servers["untouched"]["url"], "http://127.0.0.1:9/other");
            if relative.starts_with(".vscode") {
                assert_eq!(value["inputs"], json!([{"id":"preserved"}]));
            } else if relative.starts_with(".pi") {
                assert_eq!(value["editorSetting"], "preserved");
            }
        }
        assert_eq!(std::fs::read_to_string(source).expect("capture is read-only"), content);
    }
}

#[test]
fn a_source_disabled_or_reconfigured_before_login_must_be_reloaded() {
    for (field, replacement) in [("enabled", json!(false)), ("timeout", json!(6000))] {
        let temp = tempfile::tempdir().expect("source fixture");
        let (manager, source, _) = file_manager(&temp, "http://127.0.0.1:9/mcp");
        let entry = manager.entry("remote").expect("original entry");
        let mut value: Value = serde_json::from_slice(&std::fs::read(&source).expect("source")).expect("JSON");
        value["mcpServers"]["remote"][field] = replacement;
        std::fs::write(&source, value.to_string()).expect("operator edit before login");
        assert!(SourceRepair::prepare(&entry.config, &manager.inner.cwd).is_err(), "{field} changed");
    }
}

#[test]
fn explicit_cli_source_repair_leaves_project_and_user_definitions_untouched() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let fixture = server();
    let (base, project, global) = file_manager(&temp, &fixture.resource());
    let project_before = std::fs::read(&project).expect("project snapshot");
    let global_before = std::fs::read(&global).expect("global snapshot");
    let cli = temp.path().join("selected-mcp.json");
    std::fs::write(&cli, &project_before).expect("explicit CLI config");
    let manager = McpManager::bootstrap(&base.inner.cwd, &base.inner.global_dir, std::slice::from_ref(&cli), true)
        .expect("CLI discovery");
    let entry = acknowledge(&manager);
    assert_eq!(entry.config.provenance, Provenance::Cli);
    runtime().block_on(async {
        manager.reauthenticate("remote", None, browser(&fixture)).await.expect("CLI source repair");
        assert_eq!(manager.mounted_tool_metas().len(), 1);
        manager.shutdown_all().await;
        base.shutdown_all().await;
    });
    let repaired: Value = serde_json::from_slice(&std::fs::read(cli).expect("CLI source")).expect("JSON");
    assert_eq!(repaired["mcpServers"]["remote"]["headers"], json!({"X-Tenant":"workspace"}));
    assert_eq!(std::fs::read(project).expect("project after reauth"), project_before);
    assert_eq!(std::fs::read(global).expect("global after reauth"), global_before);
}

#[cfg(unix)]
#[test]
fn explicit_reauth_and_logout_never_execute_the_discarded_authorization_helper() {
    for login in [true, false] {
        let temp = tempfile::tempdir().expect("fixture directory");
        let fixture = server();
        let (base, source, _) = file_manager(&temp, &fixture.resource());
        let mut value: Value = serde_json::from_slice(&std::fs::read(&source).expect("source")).expect("JSON");
        value["mcpServers"]["remote"]["headers"]["aUtHoRiZaTiOn"] =
            json!("$CMD:printf invoked > obsolete-helper-ran; printf 'Bearer expired'");
        std::fs::write(&source, value.to_string()).expect("authored helper definition");
        let manager = McpManager::bootstrap(&base.inner.cwd, &base.inner.global_dir, &[], true)
            .expect("helper discovery");
        acknowledge(&manager);
        let marker = manager.inner.cwd.join("obsolete-helper-ran");
        runtime().block_on(async {
            if login {
                manager.reauthenticate("remote", None, browser(&fixture)).await.expect("explicit reauth");
                assert_eq!(manager.mounted_tool_metas().len(), 1);
            } else {
                manager.unauthenticate("remote").await.expect("explicit logout");
                manager.refresh_tools("remote").await.expect_err("revoked static credential");
                assert!(fixture.captured().is_empty(), "logout must precede any HTTP request");
            }
            assert!(!marker.exists(), "discarded Authorization helper must never execute");
            manager.shutdown_all().await;
            base.shutdown_all().await;
        });
    }
}

async fn wait_until(owner: &AgentCx, ready: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "fixture synchronization deadline");
        owner.time().sleep(Duration::from_millis(5)).await;
    }
}

#[test]
fn queued_trust_deny_and_logout_act_on_the_repaired_entry_and_its_live_catalog() {
    for action in ["trust", "deny", "logout"] {
        let temp = tempfile::tempdir().expect("fixture directory");
        let fixture = server();
        let (manager, _, _) = file_manager(&temp, &fixture.resource());
        let original = manager.entry("remote").expect("original entry");
        runtime().block_on(async {
            let owner = AgentCx::for_current_or_request();
            let repair = Repair::prepare(&original, &manager.inner.cwd).await.expect("source capture");
            let lane = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&original.connect_lane), owner.cx())
                .await.expect("hold publication lane");
            let mut pending: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + '_>> =
                match action {
                    "trust" => Box::pin(async { manager.trust("remote").await.map(|_| ()) }),
                    "deny" => Box::pin(manager.deny("remote")),
                    "logout" => Box::pin(manager.unauthenticate("remote")),
                    _ => unreachable!(),
                };
            assert!(futures::poll!(&mut pending).is_pending(), "operation queues behind the held lane");
            if action == "logout" {
                let saved = Box::pin(wait_until(&owner, || !auth_snapshot(&manager).is_empty()));
                pending = match futures::future::select(saved, pending).await {
                    futures::future::Either::Left(((), pending)) => pending,
                    _ => panic!("logout must persist before waiting for the connection lane"),
                };
            }
            // Simulate the admitted final phase of reauth while an operator
            // command has already captured the original immutable entry.
            let (current, lane) = repair.publish(&manager, &original, &owner, lane).await
                .expect("publish repaired definition");
            let transport: Arc<dyn crate::mcp::transport::McpTransport> = Arc::new(
                crate::mcp::transport::HttpTransport::new(
                    &fixture.resource(), vec![("Authorization".to_string(), "Bearer managed-access".to_string())],
                ).expect("already-authenticated transport"),
            );
            *McpManager::lock(&current.transport) = Some(Arc::clone(&transport));
            *McpManager::lock(&current.tools_cache) = Some((std::time::Instant::now(), vec![
                super::super::McpToolMeta {
                    name:"echo".to_string(), description:String::new(),
                    input_schema:json!({"type":"object"}), output_schema:None,
                    task_support:crate::mcp::tasks::TaskSupport::Forbidden,
                },
            ]));
            *McpManager::lock(&current.health) = ServerHealth::Ready { tools:1 };
            drop(lane);
            pending.await.expect("queued lifecycle command");
            let trust = TrustStore::load(&manager.inner.trust_path).expect("current trust");
            let decision = trust.decision("remote", &manager.trust_fingerprint_for(&current));
            assert_eq!(decision, if action == "deny" { TrustDecision::Denied } else { TrustDecision::Acknowledged });
            if action == "trust" {
                assert!(transport.is_alive());
                assert_eq!(manager.mounted_tool_metas().len(), 1);
            } else {
                assert!(!transport.is_alive(), "{action} must retire the replacement transport");
                assert!(McpManager::lock(&current.tools_cache).is_none());
                assert!(manager.mounted_tool_metas().is_empty());
                if action == "logout" {
                    assert!(matches!(&*McpManager::lock(&current.health), ServerHealth::AuthenticationRequired { .. }));
                }
            }
            manager.shutdown_all().await;
        });
    }
}

#[cfg(unix)]
#[test]
fn cancelled_publication_retains_its_connection_lane_until_the_blocking_worker_retires() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let (manager, source_path, _) = file_manager(&temp, "http://127.0.0.1:9/mcp");
    let original = manager.entry("remote").expect("original entry");
    let source_before = std::fs::read(&source_path).expect("source snapshot");
    let trust_before = std::fs::read(&manager.inner.trust_path).expect("trust snapshot");
    runtime().block_on(async {
        let owner = AgentCx::for_current_or_request();
        let repair = Repair::prepare(&original, &manager.inner.cwd).await.expect("source capture");
        let source = repair.source.as_ref().expect("file-backed repair");
        let source_lock = crate::file_lock::DirLockAt::acquire_for(
            &source.directory, &source.name, Duration::from_secs(1),
        ).expect("hold source publication");
        let lane = asupersync::sync::OwnedMutexGuard::lock(Arc::clone(&original.connect_lane), owner.cx())
            .await.expect("connection lane");
        let publication = Box::pin(repair.publish(&manager, &original, &owner, lane));
        let worker_started = Box::pin(wait_until(&owner, || {
            match manager.inner.trust_lock.try_lock() {
                Ok(guard) => { drop(guard); false },
                Err(std::sync::TryLockError::WouldBlock) => true,
                Err(std::sync::TryLockError::Poisoned(_)) => panic!("unpoisoned fixture trust lock"),
            }
        }));
        let publication = match futures::future::select(worker_started, publication).await {
            futures::future::Either::Left(((), publication)) => publication,
            _ => panic!("source lock must hold the blocking publication"),
        };
        drop(publication);
        assert!(original.connect_lane.try_lock_owned().is_err(),
            "cancelling the waiter must not release a worker-owned connection lane");
        drop(source_lock);
        wait_until(&owner, || original.connect_lane.try_lock_owned().is_ok()).await;
        assert!(Arc::ptr_eq(&original, &manager.entry("remote").expect("unpublished original")));
        manager.shutdown_all().await;
    });
    assert_eq!(std::fs::read(source_path).expect("unchanged source"), source_before);
    assert_eq!(std::fs::read(&manager.inner.trust_path).expect("unchanged trust"), trust_before);
    assert!(auth_snapshot(&manager).is_empty());
}
