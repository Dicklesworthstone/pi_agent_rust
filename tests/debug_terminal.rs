//! Public DebugTool terminal lifecycle against a deterministic DAP peer.
//! Real PTY/process/input/output effects; this peer is not a language debugger.
#![cfg(unix)]
#![recursion_limit = "512"]

mod common;
use common::TestHarness;
use pi::debug::{DebugTool, adapters::AdapterSpec};
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread().blocking_threads(1, 8).build().unwrap()
}
fn select(registry: &ToolRegistry, id: &str) {
    let id = id.to_owned();
    registry.bind_job_session_resolver(Arc::new(move || {
        let id = id.clone(); Box::pin(async move { Some(id) })
    }));
}
fn call(registry: &ToolRegistry, input: Value) -> pi::error::Result<Value> {
    runtime().block_on(registry.get("debug").unwrap().execute("terminal-test", input, None))
        .map(|output| { assert!(!output.is_error); output.details.unwrap() })
}
fn fixture(root: &Path, mode: &str) -> Option<ToolRegistry> {
    let python = std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths)
        .map(|path| path.join("python3")).find(|path| path.is_file()));
    let Some(python) = python else {
        assert!(std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(), "Python fixture required");
        eprintln!("skip: Python unavailable; terminal integration not run");
        return None;
    };
    let adapter = root.join("adapter.py");
    std::fs::write(&adapter, include_str!("../src/debug/terminal/test_adapter.py")).unwrap();
    std::fs::write(root.join("program.py"), r#"import json, os, sys, time
from pathlib import Path
print('TERMINAL_READY', flush=True)
Path('observed.json').write_text(json.dumps({'tty':[os.isatty(i) for i in (0,1,2)],'argv':sys.argv[1:],
    'set':os.environ.get('PI_TERMINAL_TEST_MARKER'),'removed':os.environ.get('CARGO_TARGET_DIR')}))
value = input('input> ')
Path('received.txt').write_text(value)
print('RECEIVED:' + value, flush=True)
while True: time.sleep(.05)
"#).unwrap();
    let tool = DebugTool::new(root, None).with_adapters(vec![AdapterSpec {
        id: "debugpy".into(), command_candidates: vec![python.to_str().unwrap().into()],
        adapter_args: vec!["-I".into(), "-u".into(), adapter.to_str().unwrap().into(), mode.into()],
        languages: vec!["python"], install_hint: "protocol fixture".into(),
    }]);
    let registry = ToolRegistry::from_tools(vec![Box::new(tool)]);
    select(&registry, "conversation-a");
    Some(registry)
}
fn wait_file(path: &Path) -> String {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) && !text.is_empty() { return text; }
        assert!(Instant::now() < until, "missing fixture output: {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn wait_reaped(pid: u32) {
    let until = Instant::now() + Duration::from_secs(5);
    while std::process::Command::new("kill").args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().unwrap().success()
    {
        assert!(Instant::now() < until, "owned terminal child not reaped");
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn finish(harness: &TestHarness, case: &str) {
    harness.log().info("verify", format!("{case}: assertions passed"));
    let path = harness.temp_path("terminal.jsonl");
    harness.write_jsonl_logs(&path).unwrap();
    let errors = common::logging::validate_jsonl_v2_only(&std::fs::read_to_string(path).unwrap());
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn real_terminal_round_trip_retains_literal_arguments_and_conversation_ownership() {
    let case = "real_terminal_round_trip";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let Some(registry) = fixture(&root, "normal") else { return; };
    let launch = call(&registry, json!({"action":"launch","program":"program.py","adapter":"debugpy",
        "runInTerminal":true,"args":["","$literal; `not-a-command`"],
        "initialBreakpoints":[{"file":"program.py","line":3}]})).unwrap();
    assert_eq!(launch["threadState"]["terminal"]["state"], "running");
    let pid = u32::try_from(launch["threadState"]["terminal"]["pid"].as_u64().unwrap()).unwrap();
    let observed: Value = serde_json::from_str(&wait_file(&root.join("observed.json"))).unwrap();
    assert_eq!(observed["tty"], json!([true,true,true]));
    assert_eq!(observed["argv"], json!(["","$literal; `not-a-command`"]));
    assert_eq!(observed["set"], "literal-$value;");
    assert!(observed["removed"].is_null());
    let capture = call(&registry, json!({"action":"custom_request","command":"capture"})).unwrap();
    let requests = capture["result"]["requests"].as_array().unwrap();
    assert_eq!(requests[0]["arguments"]["supportsRunInTerminalRequest"], true);
    let commands: Vec<_> = requests.iter().map(|request| request["command"].as_str().unwrap()).collect();
    assert_eq!(&commands[..4], &["initialize","launch","setBreakpoints","configurationDone"]);
    select(&registry, "conversation-b");
    assert_eq!(call(&registry, json!({"action":"sessions"})).unwrap()["sessions"], json!([]));
    let error = call(&registry, json!({"action":"terminal_input","text":"foreign"})).unwrap_err();
    assert!(error.to_string().contains("DAP_NO_SESSION"));
    assert!(!root.join("received.txt").exists());
    select(&registry, "conversation-a");
    assert_eq!(call(&registry, json!({"action":"terminal_input","text":"answer-42"})).unwrap()["bytesWritten"], 10);
    assert_eq!(wait_file(&root.join("received.txt")), "answer-42");
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let output = call(&registry, json!({"action":"output"})).unwrap();
        if output["tail"].as_str().unwrap().contains("RECEIVED:answer-42") { break; }
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(10));
    }
    call(&registry, json!({"action":"terminate"})).unwrap();
    wait_reaped(pid);
    finish(&harness, case);
}

#[test]
fn failed_launch_retires_started_terminal_and_does_not_publish_a_debug_session() {
    let case = "failed_terminal_launch";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let Some(registry) = fixture(&root, "reject_after_start") else { return; };
    let error = call(&registry, json!({"action":"launch","program":"program.py","runInTerminal":true})).unwrap_err();
    assert!(error.to_string().contains("deliberate launch rejection"), "{error}");
    let pid = wait_file(&root.join("terminal-pid.txt")).parse().unwrap();
    wait_reaped(pid);
    assert_eq!(call(&registry, json!({"action":"sessions"})).unwrap()["sessions"], json!([]));
    finish(&harness, case);
}

#[test]
fn missing_reverse_launch_and_shell_requests_cannot_report_terminal_success() {
    for mode in ["no_reverse", "shell_request"] {
        let harness = TestHarness::new(mode);
        let root = harness.temp_path(".");
        let Some(registry) = fixture(&root, mode) else { return; };
        let error = call(&registry, json!({"action":"launch","program":"program.py","runInTerminal":true})).unwrap_err();
        assert!(error.to_string().contains(if mode == "no_reverse" {
            "DAP_TERMINAL_NOT_STARTED"
        } else { "DAP_TERMINAL_USAGE" }), "{error}");
        assert!(!root.join("terminal-pid.txt").exists());
        assert!(!root.join("observed.json").exists());
        finish(&harness, mode);
    }
}

#[test]
fn dropping_registry_reaps_a_terminal_in_a_dormant_conversation() {
    let case = "dormant_terminal_cleanup";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let Some(registry) = fixture(&root, "normal") else { return; };
    let output = call(&registry, json!({"action":"launch","program":"program.py","runInTerminal":true})).unwrap();
    let pid = u32::try_from(output["threadState"]["terminal"]["pid"].as_u64().unwrap()).unwrap();
    select(&registry, "conversation-b");
    call(&registry, json!({"action":"sessions"})).unwrap();
    drop(registry);
    wait_reaped(pid);
    finish(&harness, case);
}

#[test]
#[ignore = "requires a real installed debugpy and PTY; opt into this lane through DSR"]
fn live_debugpy_terminal_supports_stdin_breakpoints_and_mutation() {
    let case = "live_debugpy_terminal";
    let harness = TestHarness::new(case);
    let root = harness.temp_path(".");
    let python = std::env::var("PI_DEBUG_TEST_PYTHON").unwrap_or_else(|_| "python3".into());
    let program = root.join("interactive.py");
    std::fs::write(&program, "from pathlib import Path\nvalue = 41\nprint('READY', flush=True)\nline = input('prompt> ')\nvalue += 1\nPath('live-result').write_text(f'{value}:{line}')\nimport time; time.sleep(30)\n").unwrap();
    let tool = DebugTool::new(&root, None).with_adapters(vec![AdapterSpec {
        id:"debugpy".into(), command_candidates:vec![python],
        adapter_args:vec!["-m".into(), "debugpy.adapter".into()], languages:vec!["python"],
        install_hint:"install debugpy in the test interpreter".into(),
    }]);
    let registry = ToolRegistry::from_tools(vec![Box::new(tool)]);
    let result = call(&registry, json!({"action":"launch","program":"interactive.py","adapter":"debugpy",
        "runInTerminal":true,"stopOnEntry":false,"initialBreakpoints":[{"file":"interactive.py","line":5}]})).unwrap();
    let pid = u32::try_from(result["threadState"]["terminal"]["pid"].as_u64().unwrap()).unwrap();
    call(&registry, json!({"action":"terminal_input","text":"answer-42"})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let state = call(&registry, json!({"action":"sessions"})).unwrap();
        if state["sessions"][0]["state"]["state"] == "stopped" { break; }
        assert!(Instant::now() < deadline, "live debugger never stopped at the breakpoint");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(call(&registry, json!({"action":"evaluate","expression":"value"})).unwrap()["result"], "41");
    assert_eq!(call(&registry, json!({"action":"set_expression","expression":"value","value":"99"})).unwrap()["value"], "99");
    call(&registry, json!({"action":"continue"})).unwrap();
    assert_eq!(wait_file(&root.join("live-result")), "100:answer-42");
    call(&registry, json!({"action":"terminate"})).unwrap();
    wait_reaped(pid);
    finish(&harness, case);
}
