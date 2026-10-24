//! Public DebugTool requests over the real Rust DAP transport to the shared
//! deterministic protocol peer. A separate opt-in case uses actual debugpy.

use crate::debug::{DebugTool, adapters};
use crate::error::Result;
use crate::tools::Tool;
use serde_json::{Value, json};
use std::path::Path;

const KEY: &str = "PI_DEBUG_ENV_FIXTURE_ONLY";
const VALUE: &str = "  literal 雪\n$HOME ${OTHER} $(no-shell)  ";

fn event(case: &str) {
    eprintln!(
        "{}",
        json!({"schema":"pi.debug.environment.native.v1","case":case})
    );
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .unwrap()
}

fn fixture(root: &Path) -> Option<DebugTool> {
    let python = adapters::select_adapter(Some(Path::new("app.py")), Some("debugpy"), &[])
        .unwrap()
        .resolve_command();
    let Some(python) = python else {
        assert!(
            std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(),
            "python3 required"
        );
        event("skipped_python_missing");
        return None;
    };
    let peer = root.join("adapter.py");
    // Inspect only this test's key, never dump unrelated host credentials.
    let prefix = format!(
        "import json, os\nfrom pathlib import Path\nPath('adapter-env.json').write_text(json.dumps({{'value': os.environ.get({})}}))\n",
        serde_json::to_string(KEY).unwrap()
    );
    std::fs::write(&peer, prefix + include_str!("test_adapter.py")).unwrap();
    std::fs::write(root.join("app.py"), "value = 1\n").unwrap();
    Some(
        DebugTool::new(root, None).with_adapters(vec![adapters::AdapterSpec {
            id: "env-fixture".into(),
            command_candidates: vec![python],
            adapter_args: vec![
                "-I".into(),
                "-u".into(),
                peer.to_str().unwrap().into(),
                "normal".into(),
            ],
            languages: vec!["python"],
            install_hint: "test fixture".into(),
        }]),
    )
}

fn call(tool: &DebugTool, runtime: &asupersync::runtime::Runtime, input: Value) -> Result<Value> {
    let result = runtime.block_on(tool.execute("env-regression", input, None))?;
    assert!(!result.is_error);
    Ok(result.details.unwrap())
}

fn launch() -> Value {
    json!({"action":"launch","program":"app.py","adapter":"env-fixture"})
}

fn wire_launch(tool: &DebugTool, runtime: &asupersync::runtime::Runtime) -> Value {
    let captured = call(
        tool,
        runtime,
        json!({"action":"custom_request","command":"capture"}),
    )
    .unwrap();
    captured["result"]["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|request| request["command"] == "launch")
        .unwrap()["arguments"]
        .clone()
}

#[test]
fn explicit_environment_reaches_launch_wire_without_mutating_host_or_adapter() {
    event("literal_native_launch");
    let root = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(root.path()) else {
        return;
    };
    let before = std::env::var_os(KEY);
    std::fs::write(
        root.path().join("dap.json"),
        json!({"adapters":{"env-fixture":{
            "launchDefaults":{"env":{"KEEP":"configured","OVERRIDE":"old"}}
        }}})
        .to_string(),
    )
    .unwrap();
    let runtime = runtime();
    let mut request = launch();
    request["adapterConfig"] = json!("dap.json");
    request["env"] = json!({"OVERRIDE":"explicit","EMPTY":""});
    request["env"][KEY] = json!(VALUE);
    let output = call(&tool, &runtime, request).unwrap();
    assert!(
        !output.to_string().contains("no-shell"),
        "launch receipts do not echo env"
    );
    let arguments = wire_launch(&tool, &runtime);
    assert_eq!(arguments["env"][KEY], VALUE);
    assert_eq!(arguments["env"]["KEEP"], "configured");
    assert_eq!(arguments["env"]["OVERRIDE"], "explicit");
    assert_eq!(arguments["env"]["EMPTY"], "");
    let adapter: Value =
        serde_json::from_slice(&std::fs::read(root.path().join("adapter-env.json")).unwrap())
            .unwrap();
    assert_eq!(adapter["value"], json!(std::env::var(KEY).ok()));
    assert_eq!(std::env::var_os(KEY), before);
    // `disconnect` preserves attached targets only; a Pi-launched program ends with `terminate`.
    call(&tool, &runtime, json!({"action":"terminate"})).unwrap();
    // Same tool, new session: overrides are request-local, never sticky.
    call(&tool, &runtime, launch()).unwrap();
    assert!(wire_launch(&tool, &runtime).get("env").is_none());
    call(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[test]
fn incompatible_config_environment_is_refused_before_spawn_and_does_not_poison_retry() {
    event("configured_env_refusal");
    let root = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(root.path()) else {
        return;
    };
    let runtime = runtime();
    for defaults in [
        json!(["TOKEN=CONFIG_SECRET"]),
        json!({"TOKEN":17}),
        json!({"TOKEN":"CONFIG_SECRET\u{0000}"}),
        json!({"TOKEN":"x".repeat(65536)}),
    ] {
        std::fs::write(
            root.path().join("dap.json"),
            json!({"adapters":{"env-fixture":{
                "launchDefaults":{"env":defaults}
            }}})
            .to_string(),
        )
        .unwrap();
        let mut request = launch();
        request["adapterConfig"] = json!("dap.json");
        request["env"] = json!({"EXTRA":"explicit"});
        let error = call(&tool, &runtime, request).unwrap_err().to_string();
        assert!(!error.contains("CONFIG_SECRET"));
        assert!(error.contains("DAP_ENV") || error.contains("DAP_CONFIG"));
        assert!(!root.path().join("adapter-env.json").exists());
        assert_eq!(
            call(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"],
            json!([])
        );
    }
    call(&tool, &runtime, launch()).unwrap();
    call(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[test]
fn rejected_scope_and_payload_never_start_an_adapter_or_connect() {
    event("scope_refusal");
    let root = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(root.path()) else {
        return;
    };
    let runtime = runtime();
    for input in [
        json!({"action":"attach","pid":42,"adapter":"env-fixture","env":{}}),
        json!({"action":"attach","port":1,"adapter":"debugpy","env":{}}),
        json!({"action":"sessions","env":{"TOKEN":"PRIVATE_CANARY"}}),
        json!({"action":"launch","program":"app.py","adapter":"env-fixture","env":["PRIVATE_CANARY"]}),
        json!({"action":"launch","program":"app.py","adapter":"env-fixture","env":{"TOKEN":null}}),
    ] {
        let error = call(&tool, &runtime, input).unwrap_err().to_string();
        assert!(error.contains("DAP_USAGE"));
        assert!(!error.contains("PRIVATE_CANARY"));
        assert!(!root.path().join("adapter-env.json").exists());
    }
}

#[test]
fn live_debugpy_target_observes_literal_environment_overrides() {
    if std::env::var_os("PI_DEBUG_REQUIRE_DEBUGPY_ENV").is_none() {
        event("live_debugpy_skipped_not_requested");
        return;
    }
    event("live_debugpy_environment");
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("app.py"),
        "import os\nvalue = 1\nprint(value)\n",
    )
    .unwrap();
    let tool = DebugTool::new(root.path(), None);
    let runtime = runtime();
    let mut request = json!({"action":"launch","program":"app.py","adapter":"debugpy","env":{},"stopOnEntry":true});
    request["env"][KEY] = json!(VALUE);
    call(&tool, &runtime, request).unwrap();
    let result = call(&tool, &runtime, json!({"action":"evaluate","context":"watch",
        "expression":format!("__import__('os').environ[{}] == {}", serde_json::to_string(KEY).unwrap(), serde_json::to_string(VALUE).unwrap())
    })).unwrap();
    assert_eq!(result["result"], "True");
    call(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}
