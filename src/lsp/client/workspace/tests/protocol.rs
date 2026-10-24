//! Exercise workspace service replies and configuration effects through the
//! actual client, registry, tool workflow and framed child stdio.

use super::*;
use crate::config::{Config, LspServerSettings, LspSettings};
use crate::lsp::LspTool;
use crate::tools::Tool as _;
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

struct Peer {
    tool: LspTool,
    rt: asupersync::runtime::Runtime,
    // Retire the child before cleaning up its working directory.
    root: tempfile::TempDir,
}

impl Peer {
    fn new(startup: Value, hold_initialize: bool) -> Option<Self> {
        Self::with_settings(startup, hold_initialize, None)
    }

    #[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
    fn with_settings(
        startup: Value,
        hold_initialize: bool,
        settings: Option<Value>,
    ) -> Option<Self> {
        let python = ["python3", "python"].into_iter().find(|program| {
            Command::new(program)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        });
        let Some(python) = python else {
            assert!(
                std::env::var_os("PI_LSP_REQUIRE_PROTOCOL").is_none(),
                "Python required for workspace configuration protocol tests"
            );
            eprintln!("SKIP workspace configuration peer: Python unavailable");
            return None;
        };
        let root = tempfile::tempdir().unwrap();
        Self::source(root.path());
        let caps = json!({"textDocumentSync":1,"renameProvider":true,
            "diagnosticProvider":{"interFileDependencies":true,"workspaceDiagnostics":false},
            "_fixtureHoldInitialize":hold_initialize});
        let config = Config {
            lsp: Some(LspSettings {
                servers: Some(HashMap::from([(
                    "configuration-peer".into(),
                    LspServerSettings {
                        command: Some(python.into()),
                        args: Some(vec![
                            "-I".into(),
                            "-u".into(),
                            "-c".into(),
                            crate::lsp::client::test_server::SERVER.into(),
                            caps.to_string(),
                            startup.to_string(),
                        ]),
                        extensions: Some(vec![".cfgfixture".into()]),
                        languages: Some(vec!["plaintext".into()]),
                        root_markers: Some(vec![".cfg-root".into()]),
                        initialization_options: Some(
                            json!({"fixtureInitializationOnly":"init-only"}),
                        ),
                        settings,
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let tool = LspTool::new(&root.path().canonicalize().unwrap(), Some(&config));
        let rt = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        Some(Self { tool, rt, root })
    }

    fn source(root: &Path) {
        std::fs::write(root.join(".cfg-root"), "").unwrap();
        std::fs::write(root.join("source.cfgfixture"), "old\n").unwrap();
    }

    fn run(&self, input: Value) -> Result<Value> {
        let result = self
            .rt
            .block_on(self.tool.execute("configuration", input, None))?;
        assert!(!result.is_error);
        Ok(result.details.unwrap())
    }

    #[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
    fn configure(&self, settings: Value, apply: bool) -> Value {
        self.run(json!({"action":"configure","file":"source.cfgfixture",
            "payload":settings,"apply":apply,"timeout":5}))
            .unwrap()
    }

    #[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
    fn wire(&self, method: &str, params: Value) -> Value {
        self.run(json!({"action":"request","file":"source.cfgfixture",
            "method":method,"payload":params,"timeout":5}))
            .unwrap()["payload"]["result"]
            .clone()
    }

    #[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
    fn reverse(&self, method: &str, params: Value) -> Value {
        self.wire(
            "test/clientRequest",
            json!({"id":"server/configuration",
            "method":method,"params":params}),
        )
    }

    fn frames(&self) -> Vec<Value> {
        self.wire("test/frames", json!({}))
            .as_array()
            .unwrap()
            .clone()
    }

    fn entry(&self) -> Arc<crate::lsp::registry::ServerEntry> {
        let root = self.root.path().canonicalize().unwrap();
        self.tool
            .registry
            .entry_for_root("configuration-peer", &root)
            .unwrap()
    }
}

#[test]
fn configuration_services_answer_before_initialize_completes() {
    let startup = json!([
        {"id":"early/config","method":"workspace/configuration","params":{"items":[{}, {"section":"missing"}]}},
        {"id":77,"method":"workspace/workspaceFolders"},
        {"id":"early/invalid","method":"workspace/configuration","params":{"items":false}}
    ]);
    let Some(peer) = Peer::new(startup, false) else {
        return;
    };
    let inspected = peer
        .run(json!({"action":"configure","file":"source.cfgfixture"}))
        .unwrap();
    assert_eq!(inspected["settings"], json!({}));
    assert_eq!(inspected["configured"], false);
    let frames = peer.frames();
    let init = frames
        .iter()
        .find(|frame| frame["method"] == "initialize")
        .unwrap();
    assert_eq!(
        init["params"]["capabilities"]["workspace"]["configuration"],
        true
    );
    assert_eq!(
        init["params"]["capabilities"]["workspace"]["didChangeConfiguration"]["dynamicRegistration"],
        false
    );
    let config = frames
        .iter()
        .position(|frame| frame["id"] == "early/config")
        .unwrap();
    let initialized = frames
        .iter()
        .position(|frame| frame["method"] == "initialized")
        .unwrap();
    assert!(
        config < initialized,
        "server waits for our reply before completing initialize"
    );
    assert_eq!(frames[config]["result"], json!([{}, null]));
    let folders = frames.iter().find(|frame| frame["id"] == 77).unwrap();
    assert_eq!(folders["result"], init["params"]["workspaceFolders"]);
    let malformed = frames
        .iter()
        .find(|frame| frame["id"] == "early/invalid")
        .unwrap();
    assert_eq!(malformed["error"]["code"], -32602);
    assert!(malformed.get("result").is_none());
}

#[test]
fn configured_settings_are_available_during_initialize_then_pushed_before_documents() {
    let settings = json!({"analysis":{"enabled":false,"depth":0,"paths":["src"]}});
    let startup = json!([
        {"id":"configured/early","method":"workspace/configuration","params":{"items":[
            {},{"section":"analysis.enabled"},{"section":"analysis.depth"},
            {"section":"fixtureInitializationOnly"}
        ]}}
    ]);
    let Some(peer) = Peer::with_settings(startup, false, Some(settings.clone())) else {
        return;
    };
    let inspected = peer
        .run(json!({"action":"configure","file":"source.cfgfixture"}))
        .unwrap();
    assert_eq!(inspected["settings"], settings);
    assert_eq!(inspected["configured"], true);
    assert_eq!(inspected["revision"], 1);
    assert_eq!(
        inspected["notificationQueued"], false,
        "inspection must not push twice"
    );
    peer.wire("test/success", json!({}));
    let frames = peer.frames();
    let init = frames
        .iter()
        .find(|frame| frame["method"] == "initialize")
        .unwrap();
    assert_eq!(
        init["params"]["initializationOptions"],
        json!({"fixtureInitializationOnly":"init-only"})
    );
    let early = frames
        .iter()
        .position(|frame| frame["id"] == "configured/early")
        .unwrap();
    assert_eq!(frames[early]["result"], json!([settings, false, 0, null]));
    let initialized = frames
        .iter()
        .position(|frame| frame["method"] == "initialized")
        .unwrap();
    let configured = frames
        .iter()
        .position(|frame| frame["method"] == "workspace/didChangeConfiguration")
        .unwrap();
    let opened = frames
        .iter()
        .position(|frame| frame["method"] == "textDocument/didOpen")
        .unwrap();
    assert!(early < initialized && initialized < configured && configured < opened);
    assert_eq!(frames[configured]["params"], json!({"settings":settings}));
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "workspace/didChangeConfiguration")
            .count(),
        1
    );
    let unchanged = peer.configure(settings, true);
    assert_eq!(unchanged["changed"], false);
    assert_eq!(unchanged["notificationQueued"], false);
    assert_eq!(unchanged["revision"], 1);
}

#[test]
fn configured_empty_settings_are_pushed_but_absent_settings_are_not() {
    for settings in [None, Some(json!({}))] {
        let configured = settings.is_some();
        let Some(peer) = Peer::with_settings(json!([]), false, settings) else {
            return;
        };
        let inspected = peer
            .run(json!({"action":"configure","file":"source.cfgfixture"}))
            .unwrap();
        assert_eq!(inspected["settings"], json!({}));
        assert_eq!(inspected["configured"], configured);
        assert_eq!(inspected["revision"], u64::from(configured));
        let pushes = peer
            .frames()
            .iter()
            .filter(|frame| frame["method"] == "workspace/didChangeConfiguration")
            .count();
        assert_eq!(pushes, usize::from(configured));
    }
}

#[test]
fn reload_restores_configured_settings_after_a_connection_local_override() {
    let baseline = json!({"analysis":{"strict":false}});
    let Some(peer) = Peer::with_settings(json!([]), false, Some(baseline.clone())) else {
        return;
    };
    let applied = peer.configure(json!({"analysis":{"strict":true}}), true);
    assert_eq!(applied["settings"]["analysis"]["strict"], true);
    assert_eq!(applied["revision"], 2);
    assert_eq!(
        peer.reverse("workspace/configuration", json!({"items":[{}]}))["result"],
        json!([{"analysis":{"strict":true}}])
    );
    let reloaded = peer
        .run(json!({"action":"reload","file":"source.cfgfixture"}))
        .unwrap();
    assert_eq!(reloaded["killed"], 1);
    let restored = peer
        .run(json!({"action":"configure","file":"source.cfgfixture"}))
        .unwrap();
    assert_eq!(restored["settings"], baseline);
    assert_eq!(restored["revision"], 1);
    let frames = peer.frames();
    let pushes: Vec<_> = frames
        .iter()
        .filter(|frame| frame["method"] == "workspace/didChangeConfiguration")
        .collect();
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["params"], json!({"settings":baseline}));
}

#[test]
fn preview_is_read_only_and_application_updates_push_and_pull_views() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    let settings =
        json!({"python":{"analysis":{"typeCheckingMode":"strict","enabled":false}},"zero":0});
    let preview = peer.configure(settings.clone(), false);
    assert_eq!(preview["preview"], true);
    assert_eq!(preview["revision"], 0);
    assert_eq!(preview["notificationQueued"], false);
    assert_eq!(
        peer.reverse("workspace/configuration", json!({"items":[{}]}))["result"],
        json!([{}])
    );
    assert!(
        !peer
            .frames()
            .iter()
            .any(|frame| frame["method"] == "workspace/didChangeConfiguration")
    );
    let applied = peer.configure(settings.clone(), true);
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["notificationQueued"], true);
    assert_eq!(applied["persisted"], false);
    assert_eq!(applied["revision"], 1);
    assert_eq!(
        peer.reverse(
            "workspace/configuration",
            json!({"items":[
                {"section":"zero"},{"section":"python.analysis.enabled"},{"section":"missing"},{}
            ]})
        )["result"],
        json!([0, false, null, settings])
    );
    let frames = peer.frames();
    let notices: Vec<_> = frames
        .iter()
        .filter(|frame| frame["method"] == "workspace/didChangeConfiguration")
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["params"]["settings"], settings);
    assert!(
        !frames
            .iter()
            .any(|frame| frame["method"] == "textDocument/didOpen")
    );
    assert_eq!(
        std::fs::read(peer.root.path().join("source.cfgfixture")).unwrap(),
        b"old\n"
    );
}

#[test]
fn first_empty_configuration_is_sent_but_repetition_is_a_no_op() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    assert_eq!(peer.configure(json!({}), true)["applied"], true);
    let again = peer.configure(json!({}), true);
    assert_eq!(again["applied"], false);
    assert_eq!(again["changed"], false);
    assert_eq!(again["revision"], 1);
    peer.configure(json!({"old":true,"keep":1}), true);
    let replaced = peer.configure(json!({"keep":2}), true);
    assert_eq!(replaced["revision"], 3);
    assert_eq!(
        peer.reverse("workspace/configuration", json!({"items":[{}]}))["result"],
        json!([{"keep":2}])
    );
    assert_eq!(
        peer.frames()
            .iter()
            .filter(|frame| frame["method"] == "workspace/didChangeConfiguration")
            .count(),
        3
    );
}

#[test]
fn rejected_reverse_requests_are_errors_and_do_not_replace_the_edit_handler() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    peer.configure(json!({"serverOnly":"value"}), true);
    for params in [
        json!({}),
        json!({"items":[{"section":5}]}),
        json!({"items":[{"scopeUri":"relative"}]}),
    ] {
        let reply = peer.reverse("workspace/configuration", params);
        assert_eq!(reply["id"], "server/configuration");
        assert_eq!(reply["error"]["code"], -32602);
        assert!(reply.get("result").is_none());
    }
    let uri = crate::lsp::client::try_path_to_uri(
        &peer
            .root
            .path()
            .canonicalize()
            .unwrap()
            .join("source.cfgfixture"),
    )
    .unwrap();
    let reply = peer.reverse("workspace/applyEdit", json!({"edit":{"changes":{uri:[{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"wrong"
    }]}}}));
    assert_eq!(reply["result"]["applied"], false);
    assert_eq!(
        peer.reverse("workspace/configuration", json!({"items":[{}]}))["result"],
        json!([{"serverOnly":"value"}])
    );
    assert_eq!(
        std::fs::read(peer.root.path().join("source.cfgfixture")).unwrap(),
        b"old\n"
    );
    assert!(peer.entry().client.is_alive());
}

fn rename_preview(peer: &Peer) -> Value {
    let uri = crate::lsp::client::try_path_to_uri(
        &peer
            .root
            .path()
            .canonicalize()
            .unwrap()
            .join("source.cfgfixture"),
    )
    .unwrap();
    peer.wire("test/configure", json!({"replies":{"textDocument/rename":[{"result":{"changes":{uri:[{
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"new"
    }]}}}]}}));
    peer.run(json!({"action":"rename","file":"source.cfgfixture","symbol":"old","newName":"new","apply":false,"timeout":5})).unwrap()
}

#[test]
fn preview_and_noop_preserve_approval_but_changed_settings_retire_it() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    peer.configure(json!({"strict":true}), true);
    let preview = rename_preview(&peer);
    peer.configure(json!({"strict":false}), false);
    peer.configure(json!({"strict":true}), true);
    let inspect = json!({"action":"rename","refactorId":preview["refactorId"]});
    assert_eq!(
        peer.run(inspect.clone()).unwrap()["refactorId"],
        preview["refactorId"]
    );
    peer.configure(json!({"strict":false}), true);
    let mut approve = inspect;
    approve["apply"] = json!(true);
    assert!(
        peer.run(approve)
            .unwrap_err()
            .to_string()
            .contains("LSP_REFACTOR_STALE")
    );
    assert_eq!(
        std::fs::read(peer.root.path().join("source.cfgfixture")).unwrap(),
        b"old\n"
    );
    assert_eq!(peer.entry().client.open_document_count(), 0);
}

#[test]
fn new_configuration_retires_diagnostics_and_previous_pull_result_ids() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    peer.wire(
        "test/configure",
        json!({"replies":{"textDocument/diagnostic":[
            {"result":{"kind":"full","resultId":"before","items":[]}},
            {"result":{"kind":"full","resultId":"after","items":[]}}
        ]}}),
    );
    let input = json!({"action":"diagnostics","file":"source.cfgfixture","timeout":5});
    peer.run(input.clone()).unwrap();
    let entry = peer.entry();
    let before = entry
        .client
        .document_snapshots()
        .values()
        .next()
        .unwrap()
        .version;
    assert_eq!(entry.client.diagnostics_snapshot().len(), 1);
    peer.configure(json!({"strict":true}), true);
    assert!(entry.client.diagnostics_snapshot().is_empty());
    peer.run(input).unwrap();
    let after = entry
        .client
        .document_snapshots()
        .values()
        .next()
        .unwrap()
        .version;
    assert!(after > before);
    let frames = peer.frames();
    let pulls: Vec<_> = frames
        .iter()
        .filter(|frame| frame["method"] == "textDocument/diagnostic")
        .collect();
    assert_eq!(pulls.len(), 2);
    assert!(pulls[1]["params"].get("previousResultId").is_none());
}

#[test]
fn configuration_is_scoped_to_one_live_server_and_resets_on_reload() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    peer.configure(json!({"only":"first"}), true);
    let second = peer.root.path().join("second");
    std::fs::create_dir(&second).unwrap();
    Peer::source(&second);
    let other = peer.run(json!({"action":"configure","file":"second/source.cfgfixture","payload":{"only":"second"},"apply":true})).unwrap();
    assert_eq!(other["revision"], 1);
    let second_uri = crate::lsp::client::try_path_to_uri(
        &second.canonicalize().unwrap().join("source.cfgfixture"),
    )
    .unwrap();
    // Scope is the first server's root, not a per-folder override mechanism.
    // A nested path remains within that root and gets that connection's values.
    assert_eq!(
        peer.reverse(
            "workspace/configuration",
            json!({"items":[{"scopeUri":second_uri}]})
        )["result"],
        json!([{"only":"first"}])
    );
    let second_read = peer
        .run(json!({"action":"configure","file":"second/source.cfgfixture"}))
        .unwrap();
    assert_eq!(second_read["settings"], json!({"only":"second"}));
    peer.run(json!({"action":"reload","file":"source.cfgfixture"}))
        .unwrap();
    let reloaded = peer
        .run(json!({"action":"configure","file":"source.cfgfixture"}))
        .unwrap();
    assert_eq!(reloaded["settings"], json!({}));
    assert_eq!(reloaded["revision"], 0);
    assert!(!peer.root.path().join("settings.json").exists());
}

#[test]
fn invalid_inputs_and_restricted_callers_cannot_start_or_update_servers() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    for payload in [
        Value::Null,
        json!([]),
        json!(true),
        json!(1),
        json!({"x":"x".repeat(MAX_SETTINGS_BYTES)}),
    ] {
        assert!(
            peer.run(json!({"action":"configure","file":"source.cfgfixture","payload":payload}))
                .is_err()
        );
    }
    for extra in [
        json!({"apply":true}),
        json!({"symbol":"old"}),
        json!({"query":"other"}),
        json!({"method":"bad"}),
    ] {
        let mut input = json!({"action":"configure","file":"source.cfgfixture"});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(peer.run(input).is_err());
    }
    peer.rt.block_on(async {
        let restricted = asupersync::Cx::for_request().restrict::<asupersync::cx::cap::None>();
        let _guard = restricted.set_current_restricted();
        let error = peer
            .tool
            .execute(
                "restricted",
                json!({"action":"configure","file":"source.cfgfixture","payload":{},"apply":true}),
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("LSP_EDIT_PERMISSION"));
    });
    assert!(peer.tool.registry.status().is_empty());
}

#[test]
fn cancelled_and_dead_clients_keep_their_previous_configuration_snapshot() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    peer.configure(json!({"kept":true}), true);
    let entry = peer.entry();
    let replacement = Settings::parse(&json!({"wrong":true})).unwrap();
    let owner = AgentCx::for_request();
    owner.cancel_with(
        asupersync::types::CancelKind::User,
        Some("cancel configuration"),
    );
    let error = peer
        .rt
        .block_on(owner.with_current(async { entry.client.set_workspace_settings(&replacement) }))
        .unwrap_err();
    assert!(error.to_string().contains("LSP_CANCELLED"));
    let (settings, revision) = entry.client.workspace_settings();
    assert_eq!(*settings, json!({"kept":true}));
    assert_eq!(revision, 1);
    assert_eq!(
        peer.frames()
            .iter()
            .filter(|frame| frame["method"] == "workspace/didChangeConfiguration")
            .count(),
        1
    );
    entry.client.kill();
    assert!(entry.client.set_workspace_settings(&replacement).is_err());
    assert_eq!(entry.client.workspace_settings(), (settings, revision));
}

#[test]
fn refused_notification_does_not_publish_new_settings_or_a_revision() {
    let Some(peer) = Peer::new(json!([]), false) else {
        return;
    };
    peer.configure(json!({"kept":true}), true);
    let entry = peer.entry();
    let previous = entry.client.workspace_settings();
    // Exercise the actual outbound admission path, not an injected success.
    // The settings notification cannot fit in this deliberately tiny queue.
    let paused = entry.client.rpc.pause_outbound_for_test(1, 1);
    let proposed = Settings::parse(&json!({"rejected":true})).unwrap();
    assert!(entry.client.set_workspace_settings(&proposed).is_err());
    assert_eq!(entry.client.workspace_settings(), previous);
    assert!(paused.drain().is_empty());
    assert!(
        !entry.client.is_alive(),
        "failed notification retires an indeterminate connection"
    );
}

#[test]
fn startup_timeout_cannot_report_configuration_as_applied() {
    let Some(peer) = Peer::new(json!([]), true) else {
        return;
    };
    let error = peer.run(json!({"action":"configure","file":"source.cfgfixture","payload":{},"apply":true,"timeout":1})).unwrap_err();
    assert!(error.to_string().contains("LSP_TIMEOUT"), "{error}");
    assert!(peer.tool.registry.status().is_empty());
    assert_eq!(
        std::fs::read(peer.root.path().join("source.cfgfixture")).unwrap(),
        b"old\n"
    );
}
