//! End-to-end native DebugTool calls against the deterministic DAP wire peer.
//! These are not substitutes for the separately gated real-GDB execution test.

#![cfg(unix)]

use super::{AdapterSpec, select_adapter};
use crate::debug::DebugTool;
use crate::error::Result;
use crate::tools::Tool;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .expect("runtime")
}

fn event(case: &str, phase: &str) {
    eprintln!(
        "{}",
        json!({"event":"debug_adapter_configuration", "case":case, "phase":phase})
    );
}

#[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
fn call(tool: &DebugTool, runtime: &asupersync::runtime::Runtime, input: Value) -> Result<Value> {
    let action = input["action"].as_str().unwrap_or("invalid");
    event(action, "dispatch");
    let output = runtime.block_on(tool.execute("configured-adapter-test", input.clone(), None))?;
    assert!(!output.is_error);
    event(action, "complete");
    Ok(output.details.expect("native debugger details"))
}

fn fixture(root: &Path) -> Option<PathBuf> {
    let python = select_adapter(Some(Path::new("app.py")), Some("debugpy"), &[])?.resolve_command();
    let Some(python) = python else {
        assert!(
            std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(),
            "python3 required for DAP protocol tests"
        );
        event("fixture", "skipped_python_missing");
        return None;
    };
    let adapter = root.join("adapter.py");
    let source = format!(
        "import os\nfrom pathlib import Path\nPath('adapter.pid').write_text(str(os.getpid()))\n{}",
        include_str!("test_adapter.py")
    );
    std::fs::write(&adapter, source).unwrap();
    std::fs::write(root.join("program.py"), "value = 1\n").unwrap();
    Some(PathBuf::from(python))
}

#[allow(clippy::needless_pass_by_value)] // callers pass json! temporaries
fn configure(root: &Path, python: &Path, id: &str, mode: &str, launch: Value, attach: Value) {
    let mut definitions = serde_json::Map::new();
    definitions.insert(
        id.to_owned(),
        json!({
            "command":python,
            "args":["-I","-u",root.join("adapter.py"),mode],
            "launchDefaults":launch,
            "attachDefaults":attach,
            "acceptsDirectoryProgram":true,
            "connectMode":"stdio"
        }),
    );
    std::fs::write(
        root.join("dap.json"),
        serde_json::to_vec(&json!({"adapters":definitions})).unwrap(),
    )
    .unwrap();
}

fn transcript(tool: &DebugTool, runtime: &asupersync::runtime::Runtime) -> Vec<Value> {
    call(
        tool,
        runtime,
        json!({"action":"custom_request","command":"capture"}),
    )
    .unwrap()["result"]["requests"]
        .as_array()
        .unwrap()
        .clone()
}

fn request<'a>(transcript: &'a [Value], command: &str) -> &'a Value {
    transcript
        .iter()
        .find(|entry| entry["command"] == command)
        .expect("wire request")
}

fn assert_adapter_reaped(root: &Path) {
    let pid: u32 = std::fs::read_to_string(root.join("adapter.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let pid = sysinfo::Pid::from_u32(pid);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        if system.process(pid).is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "configured adapter process survived cleanup"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn configured_launch_preserves_target_arguments_and_configures_before_execution() {
    event("configured_launch", "start");
    let root = tempfile::tempdir().unwrap();
    let Some(python) = fixture(root.path()) else {
        return;
    };
    configure(
        root.path(),
        &python,
        "custom",
        "normal",
        json!({"justMyCode":false}),
        json!({"attachOnly":true}),
    );
    let runtime = runtime();
    let tool = DebugTool::new(root.path(), None);
    let output = call(&tool, &runtime, json!({
        "action":"launch", "program":"program.py", "adapter":"custom", "adapterConfig":"dap.json",
        "args":["a b","$(literal)"], "initialBreakpoints":[{"file":"program.py","line":10}]
    })).unwrap();
    assert_eq!(output["adapter"], "custom");
    assert_eq!(output["transport"], "stdio");
    let result = call(
        &tool,
        &runtime,
        json!({"action":"evaluate","expression":"count","context":"watch"}),
    )
    .unwrap();
    assert_eq!(result["result"], "42");
    let wire = transcript(&tool, &runtime);
    let launch = &request(&wire, "launch")["arguments"];
    assert_eq!(
        launch["program"],
        json!(root.path().join("program.py").canonicalize().unwrap())
    );
    assert_eq!(launch["args"], json!(["a b", "$(literal)"]));
    assert_eq!(launch["justMyCode"], false);
    assert!(launch.get("attachOnly").is_none());
    let position = |command| {
        wire.iter()
            .position(|entry| entry["command"] == command)
            .unwrap()
    };
    assert!(position("launch") < position("setBreakpoints"));
    assert!(position("setBreakpoints") < position("configurationDone"));
    assert_eq!(
        request(&wire, "initialize")["arguments"]["supportsRunInTerminalRequest"],
        false
    );
    call(&tool, &runtime, json!({"action":"disconnect"})).unwrap();
    assert_adapter_reaped(root.path());
    event("configured_launch", "verified");
}

#[test]
fn configured_attach_uses_registered_pid_profile_and_detaches_without_termination() {
    event("configured_attach", "start");
    let root = tempfile::tempdir().unwrap();
    let Some(python) = fixture(root.path()) else {
        return;
    };
    configure(
        root.path(),
        &python,
        "debugpy",
        "normal",
        json!({"launchOnly":true}),
        json!({"justMyCode":false}),
    );
    let runtime = runtime();
    let tool = DebugTool::new(root.path(), None);
    call(
        &tool,
        &runtime,
        json!({"action":"attach","pid":4242,"adapter":"debugpy","adapterConfig":"dap.json"}),
    )
    .unwrap();
    let wire = transcript(&tool, &runtime);
    let attach = &request(&wire, "attach")["arguments"];
    assert_eq!(attach["processId"], 4242);
    assert_eq!(attach["justMyCode"], false);
    assert!(attach.get("pid").is_none());
    assert!(attach.get("launchOnly").is_none());
    assert_eq!(
        request(&wire, "initialize")["arguments"]["supportsRunInTerminalRequest"],
        false
    );
    call(&tool, &runtime, json!({"action":"disconnect"})).unwrap();
    let disconnect: Value =
        serde_json::from_slice(&std::fs::read(root.path().join("disconnect.json")).unwrap())
            .unwrap();
    assert_eq!(disconnect["terminateDebuggee"], false);
    assert_adapter_reaped(root.path());
    event("configured_attach", "verified");
}

#[test]
fn configuration_failure_never_spawns_or_silently_uses_a_builtin() {
    event("config_rejection", "start");
    let root = tempfile::tempdir().unwrap();
    let Some(python) = fixture(root.path()) else {
        return;
    };
    configure(
        root.path(),
        &python,
        "custom",
        "normal",
        json!({}),
        json!({}),
    );
    let runtime = runtime();
    let tool = DebugTool::new(root.path(), None);
    let error = call(
        &tool,
        &runtime,
        json!({"action":"launch","program":"program.py","adapter":"custom"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("DAP_ADAPTER_MISSING"));
    assert!(!root.path().join("adapter.pid").exists());
    configure(
        root.path(),
        &python,
        "debugpy",
        "normal",
        json!({"program":"other.py"}),
        json!({}),
    );
    let error = call(&tool, &runtime, json!({"action":"launch","program":"program.py","adapter":"debugpy","adapterConfig":"dap.json"})).unwrap_err();
    assert!(error.to_string().contains("DAP_CONFIG"));
    assert!(!root.path().join("adapter.pid").exists());
    assert_eq!(
        call(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"],
        json!([])
    );
    event("config_rejection", "verified");
}

#[test]
fn rejected_startup_reaps_configured_process_and_allows_a_fresh_attempt() {
    event("startup_cleanup", "start");
    let root = tempfile::tempdir().unwrap();
    let Some(python) = fixture(root.path()) else {
        return;
    };
    configure(
        root.path(),
        &python,
        "custom",
        "configuration_error",
        json!({}),
        json!({}),
    );
    let runtime = runtime();
    let tool = DebugTool::new(root.path(), None);
    let launch = json!({"action":"launch","program":"program.py","adapter":"custom","adapterConfig":"dap.json"});
    assert!(call(&tool, &runtime, launch.clone()).is_err());
    assert_adapter_reaped(root.path());
    assert_eq!(
        call(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"],
        json!([])
    );
    configure(
        root.path(),
        &python,
        "custom",
        "normal",
        json!({}),
        json!({}),
    );
    call(&tool, &runtime, launch).unwrap();
    drop(tool);
    assert_adapter_reaped(root.path());
    event("startup_cleanup", "verified");
}

#[test]
fn configured_directory_program_uses_the_same_native_launch_transaction() {
    event("directory_target", "start");
    let root = tempfile::tempdir().unwrap();
    let Some(python) = fixture(root.path()) else {
        return;
    };
    configure(
        root.path(),
        &python,
        "custom",
        "normal",
        json!({}),
        json!({}),
    );
    std::fs::create_dir(root.path().join("package")).unwrap();
    let runtime = runtime();
    let tool = DebugTool::new(root.path(), None);
    call(&tool, &runtime, json!({"action":"launch","program":"package","adapter":"custom","adapterConfig":"dap.json"})).unwrap();
    let wire = transcript(&tool, &runtime);
    assert_eq!(
        request(&wire, "launch")["arguments"]["program"],
        json!(root.path().join("package").canonicalize().unwrap())
    );
    drop(tool);
    assert_adapter_reaped(root.path());
    event("directory_target", "verified");
}

#[test]
fn configured_gdb_profile_normalizes_both_stop_policies_on_the_wire() {
    event("gdb_wire_policy", "start");
    for stop in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let Some(python) = fixture(root.path()) else {
            return;
        };
        configure(root.path(), &python, "gdb", "normal", json!({}), json!({}));
        let runtime = runtime();
        let tool = DebugTool::new(root.path(), None);
        call(&tool, &runtime, json!({"action":"launch","program":"program.py","adapter":"gdb","adapterConfig":"dap.json","stopOnEntry":stop})).unwrap();
        let wire = transcript(&tool, &runtime);
        let launch = &request(&wire, "launch")["arguments"];
        assert_eq!(launch["stopAtBeginningOfMainSubprogram"], stop);
        assert_eq!(launch["stopOnEntry"], false);
        assert!(launch.get("console").is_none());
        drop(tool);
        assert_adapter_reaped(root.path());
    }
    event("gdb_wire_policy", "verified");
}

#[test]
fn gdb_fallback_selection_respects_host_overrides_without_discovery_processes() {
    let root = tempfile::tempdir().unwrap();
    let Some(python) = fixture(root.path()) else {
        return;
    };
    let overrides = vec![
        AdapterSpec {
            id: "lldb-dap".into(),
            command_candidates: vec![],
            adapter_args: vec![],
            languages: vec!["binary"],
            install_hint: String::new(),
        },
        AdapterSpec {
            id: "gdb".into(),
            command_candidates: vec![python.display().to_string()],
            adapter_args: vec!["host-argument".into()],
            languages: vec!["binary"],
            install_hint: String::new(),
        },
    ];
    let picked = select_adapter(Some(Path::new("native-program")), None, &overrides).unwrap();
    assert_eq!(picked.id, "gdb");
    assert_eq!(picked.adapter_args, ["host-argument"]);
    assert!(!root.path().join("adapter.pid").exists());
}

#[test]
fn real_gdb_stops_steps_inspects_and_rejects_expired_handles() {
    // This explicit opt-in is a distinct live-backend lane, not a fixture pass.
    // The DSR host needs Python-enabled GDB, a C compiler and ptrace permission.
    if std::env::var_os("PI_DEBUG_REQUIRE_GDB").is_none() {
        event("real_gdb", "skipped_not_requested");
        return;
    }
    event("real_gdb", "start");
    assert!(
        select_adapter(None, Some("gdb"), &[])
            .unwrap()
            .resolve_command()
            .is_some(),
        "GDB is required"
    );
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("target.c");
    let binary = root.path().join("target");
    std::fs::write(
        &source,
        "int main(void) {\n  volatile int value = 40;\n  value += 2;\n  return value;\n}\n",
    )
    .unwrap();
    let compiled = std::process::Command::new("cc")
        .args(["-g", "-O0"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .status()
        .expect("C compiler required");
    assert!(compiled.success());
    let runtime = runtime();
    let tool = DebugTool::new(root.path(), None);
    call(
        &tool,
        &runtime,
        json!({"action":"launch","program":binary,"adapter":"gdb"}),
    )
    .unwrap();
    call(&tool, &runtime, json!({"action":"step_over"})).unwrap();
    let stack = call(&tool, &runtime, json!({"action":"stack_trace"})).unwrap();
    let old_frame = stack["frames"][0]["id"].as_u64().unwrap();
    let value = call(
        &tool,
        &runtime,
        json!({"action":"evaluate","expression":"value","context":"watch","frameId":old_frame}),
    )
    .unwrap();
    assert_eq!(value["result"], "40");
    call(&tool, &runtime, json!({"action":"step_over"})).unwrap();
    assert!(
        call(
            &tool,
            &runtime,
            json!({"action":"scopes","frameId":old_frame})
        )
        .is_err()
    );
    let value = call(
        &tool,
        &runtime,
        json!({"action":"evaluate","expression":"value","context":"watch"}),
    )
    .unwrap();
    assert_eq!(value["result"], "42");
    call(&tool, &runtime, json!({"action":"terminate"})).unwrap();
    assert_eq!(
        call(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"],
        json!([])
    );
    event("real_gdb", "verified");
}
