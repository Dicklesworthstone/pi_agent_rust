// Included by debug/tests.rs so the existing runtime and deterministic adapter
// fixture exercise the real production TCP transport through DebugTool.

#[cfg(unix)]
fn delve_fixture(path: &Path, mode: &str) -> Option<DebugTool> {
    let mut tool = fixture(path, mode)?;
    let adapter = &mut tool.adapters[0];
    adapter.id = "dlv".into();
    adapter.languages = vec!["go"];
    adapter.adapter_args.insert(3, "tcp".into());
    for name in ["program.go", "program_test.go", "prebuilt"] {
        std::fs::write(path.join(name), "package main\n").unwrap();
    }
    Some(tool)
}

#[cfg(unix)]
fn assert_delve_fixture_reaped(path: &Path) {
    let pid = std::fs::read_to_string(path.join("adapter.pid")).unwrap();
    let status = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status().unwrap();
    assert!(!status.success(), "owned adapter must be reaped");
}

#[cfg(unix)]
#[test]
fn delve_tcp_routes_go_sources_tests_packages_and_prebuilt_binaries() {
    for (program, selected, expected) in [
        ("program.go", None, "debug"),
        ("program_test.go", None, "test"),
        (".", Some("test"), "test"),
        ("prebuilt", None, "exec"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = delve_fixture(temp.path(), "normal") else { return; };
        let runtime = runtime();
        let mut args = json!({"action":"launch","program":program,"adapter":"dlv",
            "args":["literal argument with spaces"],
            "initialBreakpoints":[{"file":"program.go","line":10},{"file":"program.go","line":20}]});
        if let Some(mode) = selected { args["goMode"] = json!(mode); }
        let output = run(&tool, &runtime, args).unwrap();
        assert_eq!(output["transport"], "tcp");
        assert_eq!(output["goMode"], expected);
        assert_eq!(output["state"], "stopped_entry");
        let captured = run(&tool, &runtime, json!({"action":"custom_request","command":"capture"})).unwrap();
        let requests = captured["result"]["requests"].as_array().unwrap();
        let commands: Vec<_> = requests.iter().map(|request| request["command"].as_str().unwrap()).collect();
        assert_eq!(&commands[..4], &["initialize", "launch", "setBreakpoints", "configurationDone"]);
        let launch = &requests[1]["arguments"];
        assert_eq!(launch["mode"], expected);
        assert_eq!(launch["args"], json!(["literal argument with spaces"]));
        assert_eq!(requests[2]["arguments"]["breakpoints"].as_array().unwrap().len(), 2);
        let build_parent = launch["output"].as_str().map(|path| Path::new(path).parent().unwrap().to_path_buf());
        if expected == "exec" {
            assert!(build_parent.is_none());
        } else {
            let parent = build_parent.as_ref().unwrap();
            assert!(parent.is_dir());
            assert!(!parent.starts_with(temp.path()), "generated binary is not placed in the workspace");
        }
        run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&std::fs::read_to_string(temp.path().join("disconnect.json")).unwrap()).unwrap()["terminateDebuggee"], true);
        if let Some(parent) = build_parent { assert!(!parent.exists()); }
        assert_delve_fixture_reaped(temp.path());
    }
}

#[cfg(unix)]
#[test]
fn delve_tcp_does_not_decode_debuggee_stdout_as_protocol_frames() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = delve_fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    // No explicit adapter: a local Go package directory selects registered dlv.
    run(&tool, &runtime, json!({"action":"launch","program":"."})).unwrap();
    run(&tool, &runtime, json!({"action":"custom_request","command":"flood"})).unwrap();
    let tail = run(&tool, &runtime, json!({"action":"output"})).unwrap();
    assert!(tail["tail"].as_str().unwrap().contains("debuggee stdout is not a DAP frame"));
    assert!(tail["tail"].as_str().unwrap().contains("progress"));
    let sessions = run(&tool, &runtime, json!({"action":"sessions"})).unwrap();
    assert_eq!(sessions["sessions"][0]["state"]["thread_id"], 9);
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn delve_tcp_rejects_untrusted_discovery_without_publishing_a_session() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = delve_fixture(temp.path(), "bad_endpoint") else { return; };
    let runtime = runtime();
    let error = run(&tool, &runtime, json!({"action":"launch","program":"program.go","adapter":"dlv"})).unwrap_err();
    assert!(error.to_string().contains("non-loopback"));
    assert_eq!(run(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"], json!([]));
    assert_delve_fixture_reaped(temp.path());
}

#[cfg(unix)]
#[test]
fn cancelled_delve_startup_drops_and_reaps_the_unadopted_process() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = delve_fixture(temp.path(), "never_ready") else { return; };
    let adapter = &tool.adapters[0];
    let command = adapter.resolve_command().unwrap();
    runtime().block_on(async {
        let mut pending = Box::pin(dap::DapTransport::spawn_delve(&command, &adapter.adapter_args, &[], temp.path()));
        let owner = AgentCx::for_current_or_request();
        for _ in 0..500 {
            assert!(futures::poll!(pending.as_mut()).is_pending());
            if temp.path().join("adapter.pid").exists() { break; }
            owner.time().sleep(Duration::from_millis(10)).await;
        }
        assert!(temp.path().join("adapter.pid").exists());
        drop(pending);
    });
    assert_delve_fixture_reaped(temp.path());
}

#[cfg(unix)]
#[test]
fn startup_budget_covers_configuration_and_cleans_failed_go_build_state() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = delve_fixture(temp.path(), "configuration_stall") else { return; };
    let runtime = runtime();
    let error = run(&tool, &runtime, json!({"action":"launch","program":"program.go","adapter":"dlv","startupTimeoutMs":50})).unwrap_err();
    assert!(error.to_string().contains("TIMEOUT"), "{error}");
    assert_eq!(run(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"], json!([]));
    assert_delve_fixture_reaped(temp.path());
}

#[cfg(unix)]
#[test]
fn delve_attach_disconnect_requests_preservation_not_termination() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = delve_fixture(temp.path(), "normal") else { return; };
    let runtime = runtime();
    run(&tool, &runtime, json!({"action":"attach","pid":4242,"adapter":"dlv"})).unwrap();
    let captured = run(&tool, &runtime, json!({"action":"custom_request","command":"capture"})).unwrap();
    assert_eq!(captured["result"]["requests"][1]["arguments"], json!({"processId":4242,"mode":"local"}));
    let output = run(&tool, &runtime, json!({"action":"disconnect"})).unwrap();
    assert_eq!(output["adapterAcknowledged"], true);
    assert_eq!(output["debuggeeTerminationRequested"], false);
    let arguments: Value = serde_json::from_str(&std::fs::read_to_string(temp.path().join("disconnect.json")).unwrap()).unwrap();
    assert_eq!(arguments["terminateDebuggee"], false);
    assert_delve_fixture_reaped(temp.path());
}

#[cfg(unix)]
#[test]
fn unsupported_attached_termination_preserves_the_live_session_for_detach() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = delve_fixture(temp.path(), "no_terminate_attached") else { return; };
    let runtime = runtime();
    run(&tool, &runtime, json!({"action":"attach","pid":4242,"adapter":"dlv"})).unwrap();
    assert!(run(&tool, &runtime, json!({"action":"terminate"})).unwrap_err().to_string().contains("DAP_UNSUPPORTED"));
    assert!(!temp.path().join("disconnect.json").exists());
    run(&tool, &runtime, json!({"action":"threads"})).unwrap();
    run(&tool, &runtime, json!({"action":"disconnect"})).unwrap();
    let arguments: Value = serde_json::from_str(&std::fs::read_to_string(temp.path().join("disconnect.json")).unwrap()).unwrap();
    assert!(arguments.get("terminateDebuggee").is_none());
}

#[cfg(unix)]
#[test]
fn rejected_disconnect_is_not_reported_as_success_or_forced_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = delve_fixture(temp.path(), "disconnect_error") else { return; };
    let runtime = runtime();
    run(&tool, &runtime, json!({"action":"attach","pid":4242,"adapter":"dlv"})).unwrap();
    let error = run(&tool, &runtime, json!({"action":"disconnect"})).unwrap_err();
    assert!(error.to_string().contains("disconnect rejected"));
    assert_eq!(run(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"].as_array().unwrap().len(), 1);
    run(&tool, &runtime, json!({"action":"threads"})).unwrap();
    drop(tool);
    assert_delve_fixture_reaped(temp.path());
}

#[test]
fn invalid_go_modes_and_startup_budgets_fail_before_adapter_dispatch() {
    for args in [
        json!({"action":"launch","goMode":"remote"}),
        json!({"action":"attach","goMode":"debug"}),
        json!({"action":"launch","startupTimeoutMs":0}),
        json!({"action":"launch","startupTimeoutMs":300_001}),
        json!({"action":"threads","startupTimeoutMs":10}),
    ] {
        let input: DebugInput = serde_json::from_value(args).unwrap();
        assert!(input.validate().is_err());
    }
}
