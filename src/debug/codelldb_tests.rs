//! CodeLLDB profile and public DebugTool regressions. The Python peer tests
//! exercise Rust dispatch/framing, not actual LLDB. Real LLDB is opt-in below.

use super::{AdapterSpec, attach_arguments, default_adapters, launch_arguments, select_adapter};
use serde_json::json;
use std::path::Path;

fn event(case: &str, phase: &str) {
    eprintln!("{}", json!({"event":"codelldb_test","case":case,"phase":phase}));
}

#[test]
fn codelldb_profile_uses_stdio_and_no_unavailable_terminal_host() {
    event("profile", "start");
    let adapter = select_adapter(None, Some("codelldb"), &[]).unwrap();
    assert!(adapter.adapter_args.is_empty());
    assert!(adapter.command_candidates.iter().any(|name| name == "codelldb.exe"));
    let args = launch_arguments(&adapter, Path::new("program"), &["a b".into(), "$(literal)".into(), String::new()], Path::new("workspace"));
    assert_eq!(args["terminal"], "console");
    assert_eq!(args["stopOnEntry"], true);
    assert_eq!(args["args"], json!(["a b", "$(literal)", ""]));
    assert!(args.get("console").is_none());
    assert_eq!(attach_arguments(&adapter, 1234), json!({"pid":1234}));
    let ids: Vec<_> = default_adapters().into_iter().map(|adapter| adapter.id).collect();
    let position = |id: &str| ids.iter().position(|candidate| candidate == id).unwrap();
    assert!(position("lldb-dap") < position("gdb"));
    assert!(position("gdb") < position("codelldb"));
    event("profile", "verified");
}

#[cfg(unix)]
mod native {
    use super::*;
    use crate::debug::DebugTool;
    use crate::error::Result;
    use crate::tools::Tool;
    use serde_json::Value;
    use std::time::{Duration, Instant};

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::new().enable_parking(false)
            .worker_threads(1).blocking_threads(1, 8).build().unwrap()
    }

    fn call(tool: &DebugTool, runtime: &asupersync::runtime::Runtime, input: Value) -> Result<Value> {
        event(input["action"].as_str().unwrap_or("invalid"), "dispatch");
        let output = runtime.block_on(tool.execute("codelldb-test", input, None))?;
        assert!(!output.is_error);
        Ok(output.details.unwrap())
    }

    fn fixture(root: &Path) -> Option<AdapterSpec> {
        let python = select_adapter(None, Some("debugpy"), &[]).unwrap().resolve_command();
        let Some(python) = python else {
            assert!(std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(), "python3 required for CodeLLDB protocol tests");
            event("protocol", "skipped_python_missing");
            return None;
        };
        let path = root.join("peer.py");
        std::fs::write(&path, format!("import os\nfrom pathlib import Path\nPath('adapter.pid').write_text(str(os.getpid()))\n{}", include_str!("test_adapter.py"))).unwrap();
        std::fs::write(root.join("program"), "value = 1\n").unwrap();
        Some(AdapterSpec { id:"codelldb".into(), command_candidates:vec![python],
            adapter_args:vec!["-I".into(), "-u".into(), path.display().to_string(), "normal".into()],
            languages:vec!["binary"], install_hint:"protocol peer, not live LLDB".into() })
    }

    fn captured(tool: &DebugTool, runtime: &asupersync::runtime::Runtime) -> Vec<Value> {
        call(tool, runtime, json!({"action":"custom_request","command":"capture"})).unwrap()["result"]["requests"].as_array().unwrap().clone()
    }

    fn request<'a>(wire: &'a [Value], command: &str) -> &'a Value {
        wire.iter().find(|request| request["command"] == command).unwrap()
    }

    fn reaped(root: &Path) {
        let pid: u32 = std::fs::read_to_string(root.join("adapter.pid")).unwrap().parse().unwrap();
        let pid = sysinfo::Pid::from_u32(pid);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut system = sysinfo::System::new();
            system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
            if system.process(pid).is_none() { return; }
            assert!(Instant::now() < deadline, "CodeLLDB peer survived cleanup");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn native_launch_configures_breakpoints_and_expires_inspection_handles() {
        event("launch", "start");
        let root = tempfile::tempdir().unwrap();
        let Some(adapter) = fixture(root.path()) else { return; };
        let tool = DebugTool::new(root.path(), None).with_adapters(vec![adapter]);
        let runtime = runtime();
        let started = call(&tool, &runtime, json!({"action":"launch","adapter":"codelldb","program":"program",
            "args":["a b","$(literal)"],"initialBreakpoints":[{"file":"program","line":10}]})).unwrap();
        assert_eq!(started["adapter"], "codelldb");
        assert_eq!(started["transport"], "stdio");
        let frame = call(&tool, &runtime, json!({"action":"stack_trace"})).unwrap()["frames"][0]["id"].as_u64().unwrap();
        assert_eq!(call(&tool, &runtime, json!({"action":"evaluate","expression":"count","frameId":frame})).unwrap()["result"], "42");
        call(&tool, &runtime, json!({"action":"step_over"})).unwrap();
        assert!(call(&tool, &runtime, json!({"action":"scopes","frameId":frame})).is_err());
        let wire = captured(&tool, &runtime);
        let args = &request(&wire, "launch")["arguments"];
        assert_eq!(args["terminal"], "console");
        assert_eq!(args["args"], json!(["a b","$(literal)"]));
        let position = |command: &str| wire.iter().position(|entry| entry["command"] == command).unwrap();
        assert!(position("setBreakpoints") < position("configurationDone"));
        assert_eq!(request(&wire, "initialize")["arguments"]["supportsRunInTerminalRequest"], false);
        drop(tool);
        reaped(root.path());
        event("launch", "verified");
    }

    #[test]
    fn native_pid_attach_detaches_without_requesting_target_termination() {
        event("attach", "start");
        let root = tempfile::tempdir().unwrap();
        let Some(adapter) = fixture(root.path()) else { return; };
        let tool = DebugTool::new(root.path(), None).with_adapters(vec![adapter]);
        let runtime = runtime();
        call(&tool, &runtime, json!({"action":"attach","adapter":"codelldb","pid":4242})).unwrap();
        let wire = captured(&tool, &runtime);
        assert_eq!(request(&wire, "attach")["arguments"], json!({"pid":4242}));
        call(&tool, &runtime, json!({"action":"disconnect"})).unwrap();
        let detached: Value = serde_json::from_slice(&std::fs::read(root.path().join("disconnect.json")).unwrap()).unwrap();
        assert_eq!(detached["terminateDebuggee"], false);
        reaped(root.path());
        event("attach", "verified");
    }

    #[test]
    fn project_adapter_configuration_reaches_the_codelldb_profile() {
        event("configured", "start");
        let root = tempfile::tempdir().unwrap();
        let Some(adapter) = fixture(root.path()) else { return; };
        std::fs::write(root.path().join("dap.json"), serde_json::to_vec(&json!({"adapters":{"codelldb":{
            "command":adapter.command_candidates[0],"args":adapter.adapter_args,"connectMode":"stdio",
            "launchDefaults":{"sourceMap":{"/build/source":"/workspace/source"}}
        }}})).unwrap()).unwrap();
        let tool = DebugTool::new(root.path(), None);
        let runtime = runtime();
        call(&tool, &runtime, json!({"action":"launch","adapter":"codelldb","adapterConfig":"dap.json","program":"program","stopOnEntry":false})).unwrap();
        let wire = captured(&tool, &runtime);
        let args = &request(&wire, "launch")["arguments"];
        assert_eq!(args["sourceMap"]["/build/source"], "/workspace/source");
        assert_eq!(args["terminal"], "console");
        assert_eq!(args["stopOnEntry"], false);
        drop(tool);
        reaped(root.path());
        event("configured", "verified");
    }

    #[test]
    fn unavailable_terminal_authority_is_rejected_before_spawn() {
        event("terminal_refusal", "start");
        let root = tempfile::tempdir().unwrap();
        let Some(adapter) = fixture(root.path()) else { return; };
        let tool = DebugTool::new(root.path(), None).with_adapters(vec![adapter]);
        let error = call(&tool, &runtime(), json!({"action":"launch","adapter":"codelldb","program":"program","runInTerminal":true})).unwrap_err();
        assert!(error.to_string().contains("DAP_UNSUPPORTED"));
        assert!(!root.path().join("adapter.pid").exists());
        event("terminal_refusal", "verified");
    }

    #[test]
    fn real_codelldb_hits_a_source_breakpoint_steps_and_inspects() {
        if std::env::var_os("PI_DEBUG_REQUIRE_CODELLDB").is_none() {
            event("real_codelldb", "skipped_not_requested");
            return;
        }
        event("real_codelldb", "start");
        assert!(select_adapter(None, Some("codelldb"), &[]).unwrap().resolve_command().is_some(), "CodeLLDB 1.12.3 required");
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("program.c");
        let binary = root.path().join("program");
        std::fs::write(&source, "int main(void) {\n  volatile int value = 40;\n  value += 2;\n  return value;\n}\n").unwrap();
        assert!(std::process::Command::new("cc").args(["-g","-O0"]).arg(&source).arg("-o").arg(&binary).status().expect("C compiler required").success());
        let runtime = runtime();
        let tool = DebugTool::new(root.path(), None);
        call(&tool, &runtime, json!({"action":"launch","adapter":"codelldb","program":binary,"stopOnEntry":false,
            "initialBreakpoints":[{"file":"program.c","line":3}]})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = call(&tool, &runtime, json!({"action":"sessions"})).unwrap();
            if state["sessions"][0]["state"]["state"] == "stopped" { break; }
            assert!(Instant::now() < deadline, "source breakpoint not reached");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(call(&tool, &runtime, json!({"action":"evaluate","expression":"value","context":"watch"})).unwrap()["result"], "40");
        call(&tool, &runtime, json!({"action":"step_over"})).unwrap();
        assert_eq!(call(&tool, &runtime, json!({"action":"evaluate","expression":"value","context":"watch"})).unwrap()["result"], "42");
        call(&tool, &runtime, json!({"action":"terminate"})).unwrap();
        assert_eq!(call(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"], json!([]));
        event("real_codelldb", "verified");
    }
}
