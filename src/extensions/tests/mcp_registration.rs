//! Runtime MCP registry publication and its owner/trust boundaries.
#![cfg(test)]

use super::*;

async fn mcp_session_fixture(
    directory: &Path,
    sources: &[(&str, &str)],
) -> (crate::sdk::AgentSessionHandle, Vec<String>) {
    let mut paths = Vec::new();
    let mut owners = Vec::new();
    for (name, source) in sources {
        let extension_dir = directory.join(Path::new(name).file_stem().unwrap_or_default());
        let _ = std::fs::create_dir_all(&extension_dir);
        let path = extension_dir.join(name);
        std::fs::write(&path, source).expect("write MCP extension fixture");
        owners.push(
            JsExtensionLoadSpec::from_entry_path(&path)
                .expect("MCP extension load spec")
                .extension_id,
        );
        paths.push(path);
    }
    let handle = crate::sdk::create_agent_session(crate::sdk::SessionOptions {
        provider: Some("openai".to_string()),
        model: Some("gpt-4o".to_string()),
        api_key: Some("dummy-key".to_string()),
        working_directory: Some(directory.to_path_buf()),
        no_session: true,
        enabled_tools: Some(Vec::new()),
        extension_paths: paths,
        mcp: Some(crate::sdk::McpSessionOptions {
            config_paths: Vec::new(),
            global_dir: Some(directory.join("mcp-global")),
        }),
        ..crate::sdk::SessionOptions::default()
    })
    .await
    .expect("create MCP-enabled extension session");
    (handle, owners)
}

fn server_names(manager: &ExtensionManager) -> Vec<String> {
    let mut names = manager
        .extension_mcp_servers()
        .iter()
        .map(|spec| {
            spec.get("name")
                .and_then(Value::as_str)
                .expect("server name")
                .trim()
                .to_string()
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
#[allow(clippy::too_many_lines)]
fn runtime_mcp_callbacks_reach_the_host_and_live_pending_trust_registry() {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build")
        .block_on(async {
            let directory = tempdir().expect("fixture directory");
            let (mut handle, owners) = mcp_session_fixture(
                directory.path(),
                &[(
                    "late-mcp.mjs",
                    r#"
                    export default function init(pi) {
                        const install = name => pi.registerMcpServer(name, {
                            command: "pi-mcp-untrusted-fixture-must-not-run",
                            extension_id: "forged-owner"
                        });
                        pi.registerCommand("install-mcp", {
                            handler: async () => { install("late-command"); }
                        });
                        pi.on("turn_start", () => { install("late-event"); });
                        pi.registerTool({
                            name: "install_mcp_tool",
                            label: "Install MCP fixture",
                            description: "Register a fixture server from a tool callback",
                            parameters: { type: "object", properties: {} },
                            async execute() {
                                install("late-tool");
                                return { content: [{ type: "text", text: "registered" }] };
                            }
                        });
                    }
                    "#,
                )],
            )
            .await;
            let extensions = handle.extension_manager().expect("extensions").clone();
            let mcp = handle.mcp_manager().expect("native MCP manager");
            // ubs:ignore[rust.panic.assert-macros]
            assert!(extensions.extension_mcp_servers().is_empty());
            // ubs:ignore[rust.panic.assert-macros]
            assert!(mcp.list().is_empty());
            assert_eq!(handle.sync_extension_mcp_registrations().await, 0);

            extensions
                .execute_command("install-mcp", "", 5_000)
                .await
                .expect("actual JavaScript command registration");
            assert_eq!(server_names(&extensions), ["late-command"]);
            extensions
                .dispatch_event(ExtensionEventName::TurnStart, Some(json!({})))
                .await
                .expect("actual JavaScript event registration");
            assert_eq!(server_names(&extensions), ["late-command", "late-event"]);
            extensions
                .js_runtime()
                .expect("JavaScript runtime")
                .execute_tool(
                    "install_mcp_tool".to_string(),
                    "mcp-registration-call".to_string(),
                    json!({}),
                    Arc::new(json!({})),
                    5_000,
                )
                .await
                .expect("actual JavaScript tool registration");
            assert_eq!(
                server_names(&extensions),
                ["late-command", "late-event", "late-tool"]
            );
            for spec in extensions.extension_mcp_servers() {
                assert_eq!(
                    spec.get("extension_id").and_then(Value::as_str),
                    owners.first().map(String::as_str)
                );
            }

            // No test-side registration shortcut: the SDK consumes only the
            // host snapshot published by the actual runtime callbacks above.
            assert_eq!(handle.sync_extension_mcp_registrations().await, 3);
            assert_eq!(mcp.list().len(), 3);
            for row in mcp.list() {
                assert_eq!(row.provenance, "extension");
                assert_eq!(row.trust, "pending");
                // ubs:ignore[rust.panic.assert-macros]
                assert!(
                    !handle
                        .session()
                        .agent
                        .has_tool(&format!("mcp__{}__echo", row.name)),
                    "an unacknowledged callback registration must not mount tools"
                );
            }
            assert_eq!(handle.sync_extension_mcp_registrations().await, 0);

            let shutdown = handle.shutdown_owned_resources().await;
            // ubs:ignore[rust.panic.assert-macros]
            assert!(shutdown.completed_cleanly(), "{shutdown:?}");
            // ubs:ignore[rust.panic.assert-macros]
            assert!(
                extensions.extension_mcp_servers().is_empty(),
                "actor shutdown retires its published host definitions"
            );
        });
}

#[test]
#[allow(clippy::too_many_lines)]
fn mcp_collision_quarantines_only_its_owner_and_reset_preserves_host_rows() {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build")
        .block_on(async {
            let directory = tempdir().expect("fixture directory");
            let (handle, owners) = mcp_session_fixture(
                directory.path(),
                &[
                    (
                        "mcp-owner.mjs",
                        r#"
                        export default function init(pi) {
                            const install = name => pi.registerMcpServer(name, {
                                command: "pi-mcp-owner-fixture-must-not-run"
                            });
                            const boot = install("owner-boot");
                            boot.name = "  owner-boot  ";
                            pi.registerCommand("owner-install", {
                                handler: async () => { install("owner-late"); }
                            });
                            pi.registerCommand("owner-collide", {
                                handler: async () => { install("peer-boot"); }
                            });
                        }
                        "#,
                    ),
                    (
                        "mcp-peer.mjs",
                        r#"
                        export default function init(pi) {
                            pi.registerMcpServer("peer-boot", {
                                command: "pi-mcp-peer-fixture-must-not-run"
                            });
                            pi.registerCommand("peer-install", {
                                handler: async () => {
                                    pi.registerMcpServer("peer-late", {
                                        command: "pi-mcp-peer-fixture-must-not-run"
                                    });
                                }
                            });
                        }
                        "#,
                    ),
                ],
            )
            .await;
            let extensions = handle.extension_manager().expect("extensions").clone();
            let js_runtime = extensions.js_runtime().expect("JavaScript runtime");
            let owner_id = owners.first().expect("owner id").as_str();
            let peer_id = owners.get(1).expect("peer id").as_str();
            // ubs:ignore[rust.panic.assert-macros]
            assert!(
                extensions
                    .extension_mcp_servers()
                    .iter()
                    .any(|spec| spec.get("name").and_then(Value::as_str) == Some("  owner-boot  ")),
                "startup retains the guest's exact registered definition"
            );
            // This is deliberately host-authored state, not a substitute for
            // exercising the JavaScript registration bridge.
            extensions.register_mcp_server(json!({
                "name": "host-only",
                "command": "pi-mcp-host-fixture-must-not-run",
                "extension_id": owner_id,
            }));
            extensions
                .execute_command("owner-install", "", 5_000)
                .await
                .expect("publish owner's late server");
            assert_eq!(
                server_names(&extensions),
                ["host-only", "owner-boot", "owner-late", "peer-boot"]
            );
            let error = extensions
                .execute_command("owner-collide", "", 5_000)
                .await
                .expect_err("a peer's MCP name cannot be claimed");
            // ubs:ignore[rust.panic.assert-macros]
            assert!(
                error.to_string().contains("server name collision"),
                "{error}"
            );
            assert_eq!(
                server_names(&extensions),
                ["host-only", "peer-boot"],
                "quarantine retires only the conflicting realm's accepted definitions"
            );
            let peer = extensions
                .extension_mcp_servers()
                .into_iter()
                .find(|spec| spec.get("name").and_then(Value::as_str) == Some("peer-boot"))
                .expect("peer server survived");
            assert_eq!(
                peer.get("extension_id").and_then(Value::as_str),
                Some(peer_id)
            );
            extensions
                .execute_command("peer-install", "", 5_000)
                .await
                .expect("unrelated peer remains executable");
            assert_eq!(
                server_names(&extensions),
                ["host-only", "peer-boot", "peer-late"]
            );

            js_runtime
                .reset_transient_state()
                .await
                .expect("reset runtime-owned registries");
            assert_eq!(server_names(&extensions), ["host-only"]);
            let shutdown = handle.shutdown_owned_resources().await;
            // ubs:ignore[rust.panic.assert-macros]
            assert!(shutdown.completed_cleanly(), "{shutdown:?}");
            assert_eq!(server_names(&extensions), ["host-only"]);
        });
}

#[test]
#[allow(clippy::too_many_lines)]
fn mcp_publication_fences_stale_actors_and_preserves_host_replacements() {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build")
        .block_on(async {
            let directory = tempdir().expect("fixture directory");
            let (handle, owners) = mcp_session_fixture(
                directory.path(),
                &[(
                    "mcp-fenced.mjs",
                    r#"
                    export default function init(pi) {
                        pi.registerMcpServer("runtime-owned", {
                            command: "pi-mcp-runtime-fixture-must-not-run"
                        });
                    }
                    "#,
                )],
            )
            .await;
            let extensions = handle.extension_manager().expect("extensions").clone();
            let js_runtime = extensions.js_runtime().expect("JavaScript runtime");
            let actor = js_runtime.actor_lifecycle.clone();
            let generation = extensions
                .inner
                .lock()
                .expect("extension manager lock")
                .js_provider_generation;
            let owner_id = owners.first().expect("owner id").as_str();
            let published = extensions.extension_mcp_servers();
            let replacement = json!({
                "name": "  callback-owned  ",
                "command": "pi-mcp-new-fixture-must-not-run",
                "extension_id": "forged-owner",
            });
            for (candidate_actor, candidate_generation, candidate_owner) in [
                (Weak::new(), generation, owner_id),
                (actor.clone(), Some(Uuid::new_v4()), owner_id),
                (actor.clone(), None, owner_id),
                (actor.clone(), generation, "unloaded-owner"),
            ] {
                // ubs:ignore[rust.panic.assert-macros]
                assert!(
                    !extensions
                        .replace_runtime_mcp_specs(
                            candidate_owner,
                            &candidate_actor,
                            candidate_generation,
                            &published,
                            std::slice::from_ref(&replacement),
                        )
                        .expect("stale authority must reject without mutation")
                );
                assert_eq!(extensions.extension_mcp_servers(), published);
                // ubs:ignore[rust.panic.assert-macros]
                assert!(
                    !extensions
                        .replace_runtime_mcp_specs(
                            candidate_owner,
                            &candidate_actor,
                            candidate_generation,
                            &published,
                            &[],
                        )
                        .expect("stale authority cannot retire a current definition")
                );
                assert_eq!(extensions.extension_mcp_servers(), published);
            }
            extensions
                .inner
                .lock()
                .expect("extension manager lock")
                .js_provider_catalog_ready = false;
            let unready = extensions
                .replace_runtime_mcp_specs(
                    owner_id,
                    &actor,
                    generation,
                    &[],
                    std::slice::from_ref(&replacement),
                )
                .expect("unaccepted load must reject without mutation");
            extensions
                .inner
                .lock()
                .expect("extension manager lock")
                .js_provider_catalog_ready = true;
            // ubs:ignore[rust.panic.assert-macros]
            assert!(!unready);
            assert_eq!(extensions.extension_mcp_servers(), published);

            // ubs:ignore[rust.panic.assert-macros]
            assert!(
                extensions
                    .replace_runtime_mcp_specs(
                        owner_id,
                        &actor,
                        generation,
                        &[],
                        std::slice::from_ref(&replacement),
                    )
                    .expect("accepted owner publication")
            );
            let callback = extensions
                .extension_mcp_servers()
                .into_iter()
                .find(|spec| spec.get("name").and_then(Value::as_str) == Some("  callback-owned  "))
                .expect("exact owned registration");
            assert_eq!(
                callback.get("extension_id").and_then(Value::as_str),
                Some(owner_id)
            );
            let before_collision = extensions.extension_mcp_servers();
            let collision = extensions
                .replace_runtime_mcp_specs(
                    owner_id,
                    &actor,
                    generation,
                    &[],
                    &[json!({
                        "name": "callback-owned",
                        "command": "pi-mcp-alias-fixture-must-not-run",
                    })],
                )
                .expect_err("trimmed aliases must collide without rewriting owned specs");
            // ubs:ignore[rust.panic.assert-macros]
            assert!(
                collision.to_string().contains("server name collision"),
                "{collision}"
            );
            assert_eq!(extensions.extension_mcp_servers(), before_collision);
            // ubs:ignore[rust.panic.assert-macros]
            assert!(
                extensions
                    .replace_runtime_mcp_specs(
                        owner_id,
                        &actor,
                        generation,
                        std::slice::from_ref(&replacement),
                        &[],
                    )
                    .expect("retire exact accepted callback definition")
            );

            let host_replacement = json!({
                "name": "runtime-owned",
                "command": "pi-mcp-host-replacement-must-not-run",
                "extension_id": owner_id,
            });
            extensions.register_mcp_server(host_replacement.clone());
            let error = extensions
                .replace_runtime_mcp_specs(owner_id, &actor, generation, &published, &published)
                .expect_err("runtime cannot overwrite a newer host-authored definition");
            // ubs:ignore[rust.panic.assert-macros]
            assert!(
                error.to_string().contains("server name collision"),
                "{error}"
            );
            // ubs:ignore[rust.panic.assert-macros]
            assert_eq!(
                extensions.extension_mcp_servers(),
                std::slice::from_ref(&host_replacement)
            );
            let shutdown = handle.shutdown_owned_resources().await;
            // ubs:ignore[rust.panic.assert-macros]
            assert!(shutdown.completed_cleanly(), "{shutdown:?}");
            assert_eq!(
                extensions.extension_mcp_servers(),
                [host_replacement],
                "stopped actor retirement cannot remove a replacement it never owned"
            );
        });
}
