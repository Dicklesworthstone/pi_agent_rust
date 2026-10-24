use super::*;
#[cfg(unix)]
use crate::debug::adapters::AdapterSpec;
use crate::debug::{DebugInput, DebugTool};
use crate::tools::Tool;
#[cfg(unix)]
use std::time::Instant;

fn run<F: std::future::Future>(future: F) -> F::Output {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn terminal_arguments_preserve_literal_argv_and_refuse_unrequested_authority() {
    let good = json!({"kind":"integrated","cwd":".","args":["python3","","$name; `command`"],
        "env":{"REMOVE":null,"SET":"literal"},"argsCanBeInterpretedByShell":false});
    let parsed = Launch::parse(good.clone()).unwrap();
    assert_eq!(parsed.args[1], "");
    assert_eq!(parsed.args[2], "$name; `command`");
    assert_eq!(parsed.env.unwrap()["REMOVE"], None);
    for (key, value) in [
        ("kind", json!("external")),
        ("argsCanBeInterpretedByShell", json!(true)),
        ("args", json!([])),
        ("args", json!([""])),
        ("args", json!(["bad\u{0}name"])),
        ("env", json!({"BAD=KEY":"x"})),
        ("env", json!({"KEY":42})),
        ("title", json!("x".repeat(257))),
        ("cwd", json!("x".repeat(4097))),
    ] {
        let mut input = good.clone();
        input[key] = value;
        assert!(Launch::parse(input).is_err(), "accepted {key}");
    }
}

#[test]
fn terminal_directory_requires_an_existing_workspace_local_root() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let mut launch = Launch::parse(json!({"cwd":"","args":["python3"]})).unwrap();
    assert_eq!(launch.directory(&root).unwrap(), root);
    launch.cwd = outside.path().to_str().unwrap().into();
    assert!(
        launch
            .directory(&root)
            .unwrap_err()
            .to_string()
            .contains("DAP_TERMINAL_CWD")
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("escape")).unwrap();
        launch.cwd = "escape".into();
        assert!(launch.directory(&root).is_err());
    }
}

#[test]
fn terminal_tail_bounds_bytes_without_corrupting_chunk_split_unicode() {
    let mut tail = Tail::default();
    let value = "🙂界".repeat(100);
    for byte in value.as_bytes() {
        tail.push(std::slice::from_ref(byte));
    }
    assert_eq!(tail.text(), value);
    let suffix = "final-marker";
    tail.push(&vec![b'x'; OUTPUT_BYTES + 100]);
    tail.push(suffix.as_bytes());
    assert_eq!(tail.bytes.len(), OUTPUT_BYTES);
    assert_eq!(
        tail.observed,
        (value.len() + OUTPUT_BYTES + 100 + suffix.len()) as u64
    );
    assert!(tail.text().starts_with("[terminal output prefix omitted]"));
    assert!(tail.text().ends_with(suffix));
    assert!(!tail.complete);
}

#[test]
fn public_terminal_options_are_launch_or_input_only_and_do_not_relax_effects() {
    for input in [
        json!({"action":"attach","pid":1,"runInTerminal":true}),
        json!({"action":"attach","port":5678,"runInTerminal":true}),
        json!({"action":"launch","text":"unexpected"}),
        json!({"action":"output","enter":false}),
        json!({"action":"terminal_input"}),
        json!({"action":"terminal_input","text":"","enter":false}),
        json!({"action":"terminal_input","text":"x".repeat(1025)}),
    ] {
        let parsed: DebugInput = serde_json::from_value(input).unwrap();
        assert!(parsed.validate().is_err());
    }
    let parsed: DebugInput =
        serde_json::from_value(json!({"action":"terminal_input","text":""})).unwrap();
    parsed.validate().unwrap();
    let tool = DebugTool::new(Path::new("."), None);
    assert!(
        tool.effects_for_input(&json!({"action":"terminal_input","text":"yes"}))
            .processes()
    );
    assert!(
        tool.parameters()["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action == "terminal_input")
    );
}

#[test]
fn cancelled_launch_never_reserves_a_terminal_or_polls_native_spawn() {
    let workspace = tempfile::tempdir().unwrap();
    let host = TerminalHost::new(workspace.path(), Arc::new(|| true)).unwrap();
    let owner = AgentCx::for_request();
    owner.set_cancel_requested(true);
    let error = run(owner.with_current(host.launch(json!({"cwd":".","args":["should-not-run"]}))))
        .unwrap_err();
    assert!(error.to_string().contains("DAP_CANCELLED"));
    assert!(lock(&host.terminal).is_none());
}

#[cfg(unix)]
fn fixture(workspace: &Path, mode: &str) -> Option<DebugTool> {
    let python = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|path| path.join("python3"))
            .find(|path| path.is_file())
    });
    let Some(python) = python else {
        assert!(
            std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(),
            "Python fixture required"
        );
        eprintln!("skip: Python unavailable; no terminal protocol case executed");
        return None;
    };
    let adapter = workspace.join("adapter.py");
    std::fs::write(&adapter, include_str!("test_adapter.py")).unwrap();
    std::fs::write(
        workspace.join("program.py"),
        "import time\nwhile True: time.sleep(.05)\n",
    )
    .unwrap();
    Some(
        DebugTool::new(workspace, None).with_adapters(vec![AdapterSpec {
            id: "debugpy".into(),
            command_candidates: vec![python.to_str().unwrap().into()],
            adapter_args: vec![
                "-I".into(),
                "-u".into(),
                adapter.to_str().unwrap().into(),
                mode.into(),
            ],
            languages: vec!["python"],
            install_hint: "protocol fixture".into(),
        }]),
    )
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[cfg(unix)]
#[test]
fn cancelling_public_launch_after_real_terminal_start_reaps_the_owned_child() {
    let directory = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(directory.path(), "stall") else {
        return;
    };
    let owner = AgentCx::for_request();
    let pid =
        run(async {
            let mut call = Box::pin(owner.with_current(tool.execute("launch", json!({
            "action":"launch","program":"program.py","adapter":"debugpy","runInTerminal":true,
        }), None)));
            let until = Instant::now() + Duration::from_secs(5);
            let pid = loop {
                assert!(futures::poll!(call.as_mut()).is_pending());
                if let Ok(text) = std::fs::read_to_string(directory.path().join("terminal-pid.txt"))
                    && let Ok(pid) = text.parse::<u32>()
                {
                    break pid;
                }
                assert!(
                    Instant::now() < until,
                    "adapter never received terminal process ID"
                );
                AgentCx::for_current_or_request()
                    .time()
                    .sleep(Duration::from_millis(10))
                    .await;
            };
            assert!(alive(pid));
            owner.set_cancel_requested(true);
            let error = call.await.unwrap_err();
            assert!(error.to_string().contains("DAP_CANCELLED"), "{error}");
            assert!(lock(&tool.session).is_none());
            pid
        });
    let until = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(
            Instant::now() < until,
            "cancelled launch left its terminal child alive"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
