use super::*;

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .expect("runtime")
}

fn run(tool: &DebugTool, runtime: &asupersync::runtime::Runtime, input: Value) -> Result<Value> {
    runtime
        .block_on(tool.execute("debug-test", input, None))
        .map(|output| output.details.expect("details"))
}

#[test]
fn missing_session_is_named_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let error = run(
        &DebugTool::new(temp.path(), None),
        &runtime(),
        json!({"action":"threads"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("DAP_NO_SESSION"));
}

#[test]
fn launch_without_program_is_usage_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let error = run(
        &DebugTool::new(temp.path(), None),
        &runtime(),
        json!({"action":"launch"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("DAP_USAGE"));
}

#[test]
fn missing_target_is_named_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let error = run(
        &DebugTool::new(temp.path(), None),
        &runtime(),
        json!({"action":"launch","program":"missing.py"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("DAP_TARGET_MISSING"));
}

#[test]
fn sessions_empty_without_session() {
    let temp = tempfile::tempdir().expect("tempdir");
    let output = run(
        &DebugTool::new(temp.path(), None),
        &runtime(),
        json!({"action":"sessions"}),
    )
    .unwrap();
    assert_eq!(output["sessions"], json!([]));
}

#[cfg(unix)]
fn fixture(path: &Path, mode: &str) -> Option<DebugTool> {
    let python = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("python3"))
            .find(|path| path.is_file())
    });
    let Some(python) = python else {
        assert!(
            std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(),
            "python3 required for DAP protocol tests"
        );
        eprintln!("skip: python3 is absent; no DAP protocol fixture ran");
        return None;
    };
    let adapter = path.join("test-adapter.py");
    std::fs::write(&adapter, include_str!("test_adapter.py")).unwrap();
    std::fs::write(path.join("program.py"), "value = 1\n").unwrap();
    Some(DebugTool::new(path, None).with_adapters(vec![AdapterSpec {
        id: "protocol-fixture".into(),
        command_candidates: vec![python.display().to_string()],
        adapter_args: vec![
            "-I".into(),
            "-u".into(),
            adapter.display().to_string(),
            mode.into(),
        ],
        languages: vec!["python"],
        install_hint: "test fixture".into(),
    }]))
}

#[cfg(unix)]
fn launch(tool: &DebugTool, runtime: &asupersync::runtime::Runtime) {
    run(
        tool,
        runtime,
        json!({"action":"launch","program":"program.py","adapter":"protocol-fixture"}),
    )
    .unwrap();
}

#[cfg(unix)]
fn frame(tool: &DebugTool, runtime: &asupersync::runtime::Runtime, thread: u64) -> u64 {
    run(
        tool,
        runtime,
        json!({"action":"stack_trace","threadId":thread}),
    )
    .unwrap()["frames"][0]["id"]
        .as_u64()
        .unwrap()
}

#[cfg(unix)]
fn locals(tool: &DebugTool, runtime: &asupersync::runtime::Runtime, frame: u64) -> u64 {
    run(tool, runtime, json!({"action":"scopes","frameId":frame})).unwrap()["scopes"][0]["variablesReference"].as_u64().unwrap()
}

#[cfg(unix)]
fn object(tool: &DebugTool, runtime: &asupersync::runtime::Runtime, thread: u64) -> u64 {
    run(
        tool,
        runtime,
        json!({"action":"evaluate","expression":"value","threadId":thread}),
    )
    .unwrap()["variablesReference"]
        .as_u64()
        .unwrap()
}

#[cfg(unix)]
fn capture(tool: &DebugTool, runtime: &asupersync::runtime::Runtime) -> Vec<Value> {
    run(
        tool,
        runtime,
        json!({"action":"custom_request","command":"capture"}),
    )
    .unwrap()["result"]["requests"]
        .as_array()
        .unwrap()
        .clone()
}

#[cfg(unix)]
#[test]
#[allow(clippy::literal_string_with_formatting_args)]
fn breakpoint_workflow_preserves_all_families_and_removes_individual_entries() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    for line in [10, 20] {
        let output = run(
            &tool,
            &runtime,
            json!({"action":"set_breakpoint","file":"program.py","line":line}),
        )
        .unwrap();
        assert_eq!(output["verified"], true);
    }
    let output = run(&tool, &runtime, json!({"action":"set_breakpoint","file":"./program.py","line":10,"condition":"value > 2","logMessage":"value={value}"})).unwrap();
    assert_eq!(output["count"], 2);
    assert_eq!(output["breakpoint"]["condition"], "value > 2");
    let removed = run(
        &tool,
        &runtime,
        json!({"action":"remove_breakpoint","file":"program.py","line":10}),
    )
    .unwrap();
    assert_eq!(removed["breakpoints"][0]["line"], 20);
    for (set, remove, field, first, second) in [
        (
            "set_function_breakpoint",
            "remove_function_breakpoint",
            "name",
            "first",
            "second",
        ),
        (
            "set_instruction_breakpoint",
            "remove_instruction_breakpoint",
            "reference",
            "0x10",
            "0x20",
        ),
        (
            "set_data_breakpoint",
            "remove_data_breakpoint",
            "dataId",
            "watch-A",
            "watch-B",
        ),
    ] {
        for value in [first, second] {
            let mut input = json!({"action":set});
            input[field] = json!(value);
            run(&tool, &runtime, input).unwrap();
        }
        let mut input = json!({"action":remove});
        input[field] = json!(first);
        assert_eq!(run(&tool, &runtime, input).unwrap()["count"], 1);
    }
    let frame = frame(&tool, &runtime, 7);
    let reference = locals(&tool, &runtime, frame);
    let info = run(&tool, &runtime, json!({"action":"data_breakpoint_info","name":"value","variablesReference":reference,"frameId":frame})).unwrap();
    assert_eq!(info["result"]["dataId"], "opaque-watch-id");
    let inventory = run(&tool, &runtime, json!({"action":"list_breakpoints"})).unwrap();
    assert_eq!(inventory["groups"].as_array().unwrap().len(), 4);
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn launch_configures_initial_breakpoints_before_configuration_done() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let runtime = runtime();
    let output = run(&tool, &runtime, json!({"action":"launch","program":"program.py","adapter":"protocol-fixture",
        "initialBreakpoints":[{"file":"program.py","line":10},{"file":"./program.py","line":20}],"exceptionFilters":["raised"]})).unwrap();
    assert_eq!(output["execution"]["reason"], "entry");
    let requests = capture(&tool, &runtime);
    let commands: Vec<_> = requests
        .iter()
        .map(|request| request["command"].as_str().unwrap())
        .collect();
    assert_eq!(
        &commands[..5],
        &[
            "initialize",
            "launch",
            "setBreakpoints",
            "setExceptionBreakpoints",
            "configurationDone"
        ]
    );
    assert_eq!(
        requests[2]["arguments"]["breakpoints"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let error = run(
        &tool,
        &runtime,
        json!({"action":"launch","program":"program.py","adapter":"protocol-fixture"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("DAP_SESSION_EXISTS"));
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn configuration_failure_does_not_publish_a_session_or_ignore_adapter_errors() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "configuration_error") else {
        return;
    };
    let runtime = runtime();
    let error = run(
        &tool,
        &runtime,
        json!({"action":"launch","program":"program.py","adapter":"protocol-fixture"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("configuration rejected"));
    assert_eq!(
        run(&tool, &runtime, json!({"action":"sessions"})).unwrap()["sessions"],
        json!([])
    );
}

#[cfg(unix)]
#[test]
fn unsupported_configuration_done_is_not_sent() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "no_configuration_done") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    assert!(
        !capture(&tool, &runtime)
            .iter()
            .any(|request| request["command"] == "configurationDone")
    );
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn stepping_waits_for_a_fresh_stop_in_either_reply_order() {
    for mode in ["normal", "stop_before_reply"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let runtime = runtime();
        launch(&tool, &runtime);
        let output = run(&tool, &runtime, json!({"action":"step_over"})).unwrap();
        assert_eq!(
            output["stopped"]["reason"], "step",
            "must not return the old entry stop"
        );
        assert_eq!(output["stopped"]["threadId"], 8);
        run(&tool, &runtime, json!({"action":"continue"})).unwrap();
        let error = run(&tool, &runtime, json!({"action":"stack_trace"})).unwrap_err();
        assert!(error.to_string().contains("DAP_STATE_RUNNING"));
        run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn failed_breakpoint_updates_keep_the_acknowledged_set_and_mark_uncertainty() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    run(
        &tool,
        &runtime,
        json!({"action":"set_breakpoint","file":"program.py","line":10}),
    )
    .unwrap();
    for rejected in [13, 99] {
        assert!(
            run(
                &tool,
                &runtime,
                json!({"action":"set_breakpoint","file":"program.py","line":rejected})
            )
            .is_err()
        );
        let inventory = run(&tool, &runtime, json!({"action":"list_breakpoints"})).unwrap();
        assert_eq!(inventory["groups"][0]["synchronized"], false);
        assert_eq!(inventory["groups"][0]["requested"], json!([{"line":10}]));
    }
    let repaired = run(
        &tool,
        &runtime,
        json!({"action":"set_breakpoint","file":"program.py","line":20}),
    )
    .unwrap();
    assert_eq!(repaired["count"], 2);
    assert_eq!(repaired["synchronized"], true);
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn evaluation_retains_expandable_references_and_variable_paging() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    let output = run(
        &tool,
        &runtime,
        json!({"action":"evaluate","expression":"value"}),
    )
    .unwrap();
    let reference = output["variablesReference"].as_u64().unwrap();
    assert!(reference > 0);
    assert_eq!(output["memoryReference"], "0x100");
    run(&tool, &runtime, json!({"action":"variables","variablesReference":reference,"start":1,"limit":2,"filter":"named"})).unwrap();
    let requests = capture(&tool, &runtime);
    let variables = requests
        .iter()
        .find(|request| request["command"] == "variables")
        .unwrap();
    assert_eq!(
        variables["arguments"],
        json!({"variablesReference":66,"start":1,"count":2,"filter":"named"})
    );
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn partial_resume_preserves_peer_handles_and_pause_targets_the_running_thread() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "thread_pair") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    run(&tool, &runtime, json!({"action":"threads"})).unwrap();
    let first_frame = frame(&tool, &runtime, 7);
    let first_object = object(&tool, &runtime, 7);
    let peer_frame = frame(&tool, &runtime, 8);
    let peer_object = object(&tool, &runtime, 8);
    let resumed = run(
        &tool,
        &runtime,
        json!({"action":"continue","threadId":7,"singleThread":true}),
    )
    .unwrap();
    let states = resumed["execution"]["threads"].as_array().unwrap();
    assert_eq!(
        states.iter().find(|thread| thread["id"] == 7).unwrap()["state"],
        "running"
    );
    assert_eq!(
        states.iter().find(|thread| thread["id"] == 8).unwrap()["state"],
        "stopped"
    );
    for input in [
        json!({"action":"scopes","frameId":first_frame}),
        json!({"action":"variables","variablesReference":first_object}),
    ] {
        assert!(
            run(&tool, &runtime, input)
                .unwrap_err()
                .to_string()
                .contains("DAP_STALE_REFERENCE")
        );
    }
    run(
        &tool,
        &runtime,
        json!({"action":"scopes","frameId":peer_frame,"threadId":8}),
    )
    .unwrap();
    run(
        &tool,
        &runtime,
        json!({"action":"variables","variablesReference":peer_object,"threadId":8}),
    )
    .unwrap();
    let paused = run(&tool, &runtime, json!({"action":"pause"})).unwrap();
    assert_eq!(paused["threadId"], 7);
    assert_eq!(paused["stopped"]["reason"], "pause");
    let requests = capture(&tool, &runtime);
    let variables = requests
        .iter()
        .find(|request| request["command"] == "variables")
        .unwrap();
    assert_eq!(variables["arguments"], json!({"variablesReference":67}));
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn single_thread_step_waits_for_the_requested_thread_and_rejects_recycled_handles() {
    for mode in ["thread_pair", "thread_pair_before_reply"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let runtime = runtime();
        launch(&tool, &runtime);
        run(&tool, &runtime, json!({"action":"threads"})).unwrap();
        let old_frame = frame(&tool, &runtime, 7);
        let old_object = object(&tool, &runtime, 7);
        let peer_frame = frame(&tool, &runtime, 8);
        let stepped = run(&tool, &runtime, json!({"action":"step_over","threadId":7,"singleThread":true,"granularity":"instruction"})).unwrap();
        assert_eq!(stepped["stopped"], json!({"threadId":7,"reason":"step"}));
        let new_frame = frame(&tool, &runtime, 7);
        let new_object = object(&tool, &runtime, 7);
        assert_ne!(
            new_frame, old_frame,
            "adapter frame ID 21 is deliberately recycled"
        );
        assert_ne!(
            new_object, old_object,
            "adapter object ID 66 is deliberately recycled"
        );
        assert!(
            run(
                &tool,
                &runtime,
                json!({"action":"scopes","frameId":old_frame})
            )
            .is_err()
        );
        assert!(
            run(
                &tool,
                &runtime,
                json!({"action":"variables","variablesReference":old_object})
            )
            .is_err()
        );
        run(
            &tool,
            &runtime,
            json!({"action":"scopes","frameId":peer_frame}),
        )
        .unwrap();
        let requests = capture(&tool, &runtime);
        let step = requests
            .iter()
            .find(|request| request["command"] == "next")
            .unwrap();
        assert_eq!(
            step["arguments"],
            json!({"threadId":7,"singleThread":true,"granularity":"instruction"})
        );
        run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn rejected_resume_restores_selected_thread_without_resurrecting_a_resumed_peer() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "resume_rejected") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    run(&tool, &runtime, json!({"action":"threads"})).unwrap();
    let first_frame = frame(&tool, &runtime, 7);
    let peer_frame = frame(&tool, &runtime, 8);
    let error = run(
        &tool,
        &runtime,
        json!({"action":"continue","threadId":7,"singleThread":true}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("resume rejected"));
    run(
        &tool,
        &runtime,
        json!({"action":"scopes","frameId":first_frame}),
    )
    .unwrap();
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"scopes","frameId":peer_frame})
        )
        .is_err()
    );
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"stack_trace","threadId":8})
        )
        .unwrap_err()
        .to_string()
        .contains("DAP_STATE_RUNNING")
    );
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn a_running_thread_cannot_borrow_a_stop_or_another_threads_frame() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    run(&tool, &runtime, json!({"action":"threads"})).unwrap();
    let first_frame = frame(&tool, &runtime, 7);
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"stack_trace","threadId":8})
        )
        .unwrap_err()
        .to_string()
        .contains("DAP_STATE_RUNNING")
    );
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"evaluate","expression":"count","frameId":first_frame,"threadId":8})
        )
        .is_err()
    );
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"variables","variablesReference":first_frame})
        )
        .unwrap_err()
        .to_string()
        .contains("DAP_STALE_REFERENCE")
    );
    let requests = capture(&tool, &runtime);
    assert!(
        !requests
            .iter()
            .any(|request| request["command"] == "evaluate" || request["command"] == "variables")
    );
    assert!(
        !requests
            .iter()
            .any(|request| request["command"] == "stackTrace"
                && request["arguments"]["threadId"] == 8)
    );
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn assignment_roundtrip_refreshes_objects_and_retains_exception_causes() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    let first_frame = frame(&tool, &runtime, 7);
    let parent = locals(&tool, &runtime, first_frame);
    let changed = run(&tool, &runtime, json!({"action":"set_variable","variablesReference":parent,"threadId":7,"name":"count","value":"41"})).unwrap();
    assert_eq!(changed["value"], "41");
    assert_eq!(changed["variablesReference"], 0);
    assert_eq!(changed["refreshVariableReferences"], true);
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"variables","variablesReference":parent})
        )
        .unwrap_err()
        .to_string()
        .contains("DAP_STALE_REFERENCE")
    );
    let value = run(
        &tool,
        &runtime,
        json!({"action":"evaluate","frameId":first_frame,"expression":"count"}),
    )
    .unwrap();
    assert_eq!(value["result"], "41");
    let assigned = run(&tool, &runtime, json!({"action":"set_expression","frameId":first_frame,"threadId":7,"expression":"count","value":"99"})).unwrap();
    assert_eq!(assigned["value"], "99");
    let value = run(
        &tool,
        &runtime,
        json!({"action":"evaluate","frameId":first_frame,"expression":"count"}),
    )
    .unwrap();
    assert_eq!(value["result"], "99");
    assert_ne!(locals(&tool, &runtime, first_frame), parent);
    let exception = run(
        &tool,
        &runtime,
        json!({"action":"exception_info","threadId":7}),
    )
    .unwrap();
    assert_eq!(exception["exceptionId"], "ValueError");
    assert_eq!(exception["details"]["stackTrace"], "fixture stack");
    assert_eq!(
        exception["details"]["innerException"][0]["message"],
        "root cause"
    );
    let requests = capture(&tool, &runtime);
    let set = requests
        .iter()
        .find(|request| request["command"] == "setVariable")
        .unwrap();
    assert_eq!(
        set["arguments"],
        json!({"variablesReference":41,"name":"count","value":"41"})
    );
    let set = requests
        .iter()
        .find(|request| request["command"] == "setExpression")
        .unwrap();
    assert_eq!(
        set["arguments"],
        json!({"frameId":21,"expression":"count","value":"99"})
    );
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn unsupported_advanced_operations_fail_before_adapter_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "no_inspection_capabilities") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    for input in [
        json!({"action":"exception_info"}),
        json!({"action":"set_variable","variablesReference":1,"name":"count","value":"1"}),
        json!({"action":"set_expression","expression":"count","value":"1"}),
        json!({"action":"continue","singleThread":true}),
        json!({"action":"step_over","granularity":"instruction"}),
    ] {
        let error = run(&tool, &runtime, input).unwrap_err();
        assert!(error.to_string().contains("DAP_UNSUPPORTED"), "{error}");
    }
    let requests = capture(&tool, &runtime);
    assert!(!requests.iter().any(|request| matches!(
        request["command"].as_str(),
        Some("exceptionInfo" | "setVariable" | "setExpression" | "continue" | "next")
    )));
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn invalidation_or_resume_during_a_reply_does_not_publish_stale_inspection() {
    for mode in ["invalidate_during_variables", "resume_during_variables"] {
        let temp = tempfile::tempdir().unwrap();
        let Some(tool) = fixture(temp.path(), mode) else {
            return;
        };
        let runtime = runtime();
        launch(&tool, &runtime);
        let old_frame = frame(&tool, &runtime, 7);
        let old_object = object(&tool, &runtime, 7);
        let error = run(
            &tool,
            &runtime,
            json!({"action":"variables","variablesReference":old_object}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("DAP_STALE_REFERENCE"), "{error}");
        assert!(
            run(
                &tool,
                &runtime,
                json!({"action":"variables","variablesReference":old_object})
            )
            .is_err()
        );
        assert!(
            run(
                &tool,
                &runtime,
                json!({"action":"scopes","frameId":old_frame})
            )
            .is_err()
        );
        assert_ne!(object(&tool, &runtime, 7), old_object);
        let requests = capture(&tool, &runtime);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["command"] == "variables")
                .count(),
            1
        );
        run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn malformed_assignment_response_does_not_restore_revoked_parent_or_claim_success() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "invalid_set_result") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    let first_frame = frame(&tool, &runtime, 7);
    let parent = locals(&tool, &runtime, first_frame);
    let error = run(
        &tool,
        &runtime,
        json!({"action":"set_variable","variablesReference":parent,"name":"count","value":"123"}),
    )
    .unwrap_err();
    assert!(error.to_string().contains("DAP_PROTOCOL"));
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"variables","variablesReference":parent})
        )
        .is_err()
    );
    // A bad acknowledgement is not proof of rollback: fixture accepted the write.
    let value = run(
        &tool,
        &runtime,
        json!({"action":"evaluate","frameId":first_frame,"expression":"count"}),
    )
    .unwrap();
    assert_eq!(value["result"], "123");
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[cfg(unix)]
#[test]
fn frame_handles_cannot_cross_vendor_requests_or_session_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let Some(tool) = fixture(temp.path(), "normal") else {
        return;
    };
    let runtime = runtime();
    launch(&tool, &runtime);
    let old_frame = frame(&tool, &runtime, 7);
    capture(&tool, &runtime);
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"scopes","frameId":old_frame})
        )
        .is_err()
    );
    let previous_session = frame(&tool, &runtime, 7);
    assert_ne!(previous_session, old_frame);
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
    launch(&tool, &runtime);
    let fresh = frame(&tool, &runtime, 7);
    assert_ne!(fresh, previous_session);
    assert!(
        run(
            &tool,
            &runtime,
            json!({"action":"scopes","frameId":previous_session})
        )
        .is_err()
    );
    run(&tool, &runtime, json!({"action":"scopes","frameId":fresh})).unwrap();
    for command in [
        "variables",
        "setVariable",
        "setExpression",
        "exceptionInfo",
        "continue",
        "restartFrame",
    ] {
        assert!(
            run(
                &tool,
                &runtime,
                json!({"action":"custom_request","command":command})
            )
            .unwrap_err()
            .to_string()
            .contains("DAP_USAGE")
        );
    }
    run(&tool, &runtime, json!({"action":"terminate"})).unwrap();
}

#[test]
fn thread_execution_and_assignment_options_are_not_silently_ignored() {
    for input in [
        json!({"action":"pause","singleThread":true}),
        json!({"action":"continue","granularity":"instruction"}),
        json!({"action":"step_over","granularity":"byte"}),
        json!({"action":"evaluate","value":"1"}),
        json!({"action":"set_variable","value":format!("x{}y", char::from(0))}),
        json!({"action":"scopes","frameId":0}),
    ] {
        assert!(
            serde_json::from_value::<DebugInput>(input)
                .unwrap()
                .validate()
                .is_err()
        );
    }
    let input: DebugInput =
        serde_json::from_value(json!({"action":"set_expression","expression":"text","value":""}))
            .unwrap();
    input.validate().unwrap();
    assert_eq!(input.assignment_value().unwrap(), "");
}

include!("delve_tests.rs");
