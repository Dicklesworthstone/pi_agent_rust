//! Conversation ownership through the real DebugTool/registry and DAP transport.
//! The Python peer simulates an adapter; these are not live-debugger acceptance.
#![cfg(unix)]

mod common;

use common::TestHarness;
use common::logging::validate_jsonl_v2_only;
use pi::debug::DebugTool;
use pi::debug::adapters::AdapterSpec;
use pi::jobs::JobSessionScope;
use pi::tools::{Tool, ToolRegistry};
use serde_json::{Value, json};
use std::sync::Arc;

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::new()
        .enable_parking(false)
        .worker_threads(1)
        .blocking_threads(1, 8)
        .build()
        .expect("runtime")
}

fn fixture(case: &str) -> Option<(TestHarness, ToolRegistry)> {
    let harness = TestHarness::new(case);
    let python = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join("python3"))
            .find(|path| path.is_file())
    });
    let Some(python) = python else {
        assert!(
            std::env::var_os("PI_DEBUG_REQUIRE_PROTOCOL").is_none(),
            "python3 required for DAP protocol tests"
        );
        eprintln!("skip: python3 unavailable; no DAP ownership protocol case executed");
        return None;
    };
    let root = harness.temp_path(".");
    let adapter = root.join("test-adapter.py");
    std::fs::write(&adapter, include_str!("../src/debug/test_adapter.py")).unwrap();
    std::fs::write(root.join("program.py"), "value = 1\n").unwrap();
    let tool = DebugTool::new(&root, None).with_adapters(vec![AdapterSpec {
        id: "ownership-fixture".into(),
        command_candidates: vec![python.display().to_string()],
        adapter_args: vec![
            "-I".into(),
            "-u".into(),
            adapter.display().to_string(),
            "normal".into(),
        ],
        languages: vec!["python"],
        install_hint: "deterministic protocol fixture".into(),
    }]);
    Some((harness, ToolRegistry::from_tools(vec![Box::new(tool)])))
}

fn select(scope: &JobSessionScope, id: &str) {
    let id = id.to_string();
    scope.bind(Arc::new(move || {
        let id = id.clone();
        Box::pin(async move { Some(id) })
    }));
}

fn call(
    harness: &TestHarness,
    tool: &dyn Tool,
    runtime: &asupersync::runtime::Runtime,
    input: Value,
) -> Value {
    harness.log().info("action", input.to_string());
    let output = runtime
        .block_on(tool.execute("ownership-test", input, None))
        .expect("debug request");
    assert!(!output.is_error, "{output:?}");
    output.details.expect("debug details")
}

fn denied(
    harness: &TestHarness,
    tool: &dyn Tool,
    runtime: &asupersync::runtime::Runtime,
    input: Value,
    code: &str,
) {
    harness.log().info("action", input.to_string());
    let error = runtime
        .block_on(tool.execute("denied-test", input, None))
        .unwrap_err();
    harness.log().info("decision", error.to_string());
    assert!(
        error.to_string().contains(code),
        "expected {code}, received {error}"
    );
}

fn launch(harness: &TestHarness, tool: &dyn Tool, runtime: &asupersync::runtime::Runtime) {
    call(
        harness,
        tool,
        runtime,
        json!({"action":"launch","program":"program.py","adapter":"ownership-fixture"}),
    );
}

fn count(harness: &TestHarness, tool: &dyn Tool, runtime: &asupersync::runtime::Runtime) -> Value {
    call(
        harness,
        tool,
        runtime,
        json!({"action":"evaluate","expression":"count","threadId":7}),
    )["result"]
        .clone()
}

fn finish(harness: &TestHarness, case: &str) {
    harness
        .log()
        .info("verify", format!("case '{case}' assertions passed"));
    let path = harness.temp_path(format!("{case}.jsonl"));
    harness.write_jsonl_logs(&path).expect("write JSONL logs");
    let payload = std::fs::read_to_string(path).expect("read JSONL logs");
    let errors = validate_jsonl_v2_only(&payload);
    assert!(errors.is_empty(), "JSONL validation errors: {errors:?}");
}

#[test]
fn registry_retains_independent_debuggers_breakpoints_and_handles() {
    let case = "registry_retains_independent_debuggers_breakpoints_and_handles";
    let Some((harness, registry)) = fixture(case) else {
        return;
    };
    let scope = registry.job_session_scope();
    let tool = registry.get("debug").unwrap();
    let runtime = runtime();
    select(&scope, "owner-a");
    launch(&harness, tool, &runtime);
    call(
        &harness,
        tool,
        &runtime,
        json!({"action":"set_expression","expression":"count","value":"111","threadId":7}),
    );
    call(
        &harness,
        tool,
        &runtime,
        json!({"action":"set_breakpoint","file":"program.py","line":10}),
    );
    let frame = call(
        &harness,
        tool,
        &runtime,
        json!({"action":"stack_trace","threadId":7}),
    )["frames"][0]["id"]
        .clone();
    let variables = call(
        &harness,
        tool,
        &runtime,
        json!({"action":"scopes","frameId":frame}),
    )["scopes"][0]["variablesReference"]
        .clone();

    select(&scope, "owner-b");
    assert_eq!(
        call(&harness, tool, &runtime, json!({"action":"sessions"}))["sessions"],
        json!([])
    );
    for action in ["threads", "output", "continue", "terminate", "disconnect"] {
        denied(
            &harness,
            tool,
            &runtime,
            json!({"action":action}),
            "DAP_NO_SESSION",
        );
    }
    launch(&harness, tool, &runtime);
    assert_eq!(count(&harness, tool, &runtime), "42");
    denied(
        &harness,
        tool,
        &runtime,
        json!({"action":"variables","variablesReference":variables}),
        "DAP_STALE_REFERENCE",
    );
    call(
        &harness,
        tool,
        &runtime,
        json!({"action":"set_expression","expression":"count","value":"222","threadId":7}),
    );
    call(
        &harness,
        tool,
        &runtime,
        json!({"action":"set_breakpoint","file":"program.py","line":20}),
    );

    select(&scope, "owner-a");
    assert_eq!(count(&harness, tool, &runtime), "111");
    let restored = call(
        &harness,
        tool,
        &runtime,
        json!({"action":"variables","variablesReference":variables}),
    );
    assert_eq!(restored["variables"][0]["value"], "111");
    let breakpoints = call(
        &harness,
        tool,
        &runtime,
        json!({"action":"remove_breakpoint","file":"program.py","line":20}),
    );
    assert_eq!(breakpoints["count"], 1);
    assert_eq!(breakpoints["breakpoints"][0]["line"], 10);
    assert!(
        !call(&harness, tool, &runtime, json!({"action":"sessions"}))
            .to_string()
            .contains("owner-b")
    );

    select(&scope, "owner-b");
    assert_eq!(count(&harness, tool, &runtime), "222");
    call(&harness, tool, &runtime, json!({"action":"terminate"}));
    select(&scope, "owner-a");
    assert_eq!(count(&harness, tool, &runtime), "111");
    call(&harness, tool, &runtime, json!({"action":"terminate"}));
    finish(&harness, case);
}

#[test]
fn capacity_refuses_without_evicting_and_termination_releases_a_slot() {
    let case = "capacity_refuses_without_evicting_and_termination_releases_a_slot";
    let Some((harness, registry)) = fixture(case) else {
        return;
    };
    let scope = registry.job_session_scope();
    let tool = registry.get("debug").unwrap();
    let runtime = runtime();
    for index in 0..4 {
        select(&scope, &format!("owner-{index}"));
        launch(&harness, tool, &runtime);
        call(
            &harness,
            tool,
            &runtime,
            json!({"action":"set_expression","expression":"count","value":format!("{index}"),"threadId":7}),
        );
    }
    select(&scope, "fifth-owner");
    denied(
        &harness,
        tool,
        &runtime,
        json!({"action":"launch","program":"program.py","adapter":"ownership-fixture"}),
        "DAP_SESSION_CAPACITY",
    );
    assert_eq!(
        call(&harness, tool, &runtime, json!({"action":"sessions"}))["sessions"],
        json!([])
    );
    for index in 0..4 {
        select(&scope, &format!("owner-{index}"));
        assert_eq!(count(&harness, tool, &runtime), format!("{index}"));
    }
    // Free precisely owner-3's launched target; every other concrete adapter remains.
    call(&harness, tool, &runtime, json!({"action":"terminate"}));
    select(&scope, "fifth-owner");
    launch(&harness, tool, &runtime);
    assert_eq!(count(&harness, tool, &runtime), "42");
    call(&harness, tool, &runtime, json!({"action":"terminate"}));
    for index in 0..3 {
        select(&scope, &format!("owner-{index}"));
        assert_eq!(count(&harness, tool, &runtime), format!("{index}"));
        call(&harness, tool, &runtime, json!({"action":"terminate"}));
    }
    finish(&harness, case);
}

#[test]
fn missing_identity_and_failed_start_do_not_destroy_retained_targets() {
    let case = "missing_identity_and_failed_start_do_not_destroy_retained_targets";
    let Some((harness, registry)) = fixture(case) else {
        return;
    };
    let scope = registry.job_session_scope();
    let tool = registry.get("debug").unwrap();
    let runtime = runtime();
    select(&scope, "owner-a");
    launch(&harness, tool, &runtime);
    call(
        &harness,
        tool,
        &runtime,
        json!({"action":"set_expression","expression":"count","value":"919","threadId":7}),
    );
    scope.bind(Arc::new(|| Box::pin(async { None })));
    for action in ["sessions", "output", "terminate"] {
        denied(
            &harness,
            tool,
            &runtime,
            json!({"action":action}),
            "DAP_SESSION_UNAVAILABLE",
        );
    }
    for index in 0..8 {
        select(&scope, &format!("new-fork-{index}"));
        denied(
            &harness,
            tool,
            &runtime,
            json!({"action":"launch","program":"missing.py","adapter":"ownership-fixture"}),
            "DAP_TARGET_MISSING",
        );
        assert_eq!(
            call(&harness, tool, &runtime, json!({"action":"sessions"}))["sessions"],
            json!([])
        );
    }
    select(&scope, "owner-a");
    assert_eq!(count(&harness, tool, &runtime), "919");
    call(&harness, tool, &runtime, json!({"action":"terminate"}));
    finish(&harness, case);
}

#[test]
fn result_time_switch_suppresses_publication_without_retargeting_launch() {
    let case = "result_time_switch_suppresses_publication_without_retargeting_launch";
    let Some((harness, registry)) = fixture(case) else {
        return;
    };
    let scope = registry.job_session_scope();
    let tool = registry.get("debug").unwrap();
    let runtime = runtime();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count_calls = Arc::clone(&calls);
    // The first two reads are capture/admission; the third is publication.
    scope.bind(Arc::new(move || {
        let id = if count_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 2 {
            "owner-a"
        } else {
            "owner-b"
        };
        Box::pin(async move { Some(id.to_string()) })
    }));
    denied(
        &harness,
        tool,
        &runtime,
        json!({"action":"launch","program":"program.py","adapter":"ownership-fixture"}),
        "DAP_SESSION_CHANGED",
    );
    select(&scope, "owner-b");
    assert_eq!(
        call(&harness, tool, &runtime, json!({"action":"sessions"}))["sessions"],
        json!([])
    );
    select(&scope, "owner-a");
    assert_eq!(count(&harness, tool, &runtime), "42");
    denied(
        &harness,
        tool,
        &runtime,
        json!({"action":"launch","program":"program.py","adapter":"ownership-fixture"}),
        "DAP_SESSION_EXISTS",
    );
    call(&harness, tool, &runtime, json!({"action":"terminate"}));
    finish(&harness, case);
}

#[test]
fn disconnecting_an_attached_session_preserves_other_owners() {
    let case = "disconnecting_an_attached_session_preserves_other_owners";
    let Some((harness, registry)) = fixture(case) else {
        return;
    };
    let scope = registry.job_session_scope();
    let tool = registry.get("debug").unwrap();
    let runtime = runtime();
    select(&scope, "launched-owner");
    launch(&harness, tool, &runtime);
    call(
        &harness,
        tool,
        &runtime,
        json!({"action":"set_expression","expression":"count","value":"808","threadId":7}),
    );
    select(&scope, "attached-owner");
    call(
        &harness,
        tool,
        &runtime,
        json!({"action":"attach","pid":123,"adapter":"ownership-fixture"}),
    );
    call(&harness, tool, &runtime, json!({"action":"disconnect"}));
    let arguments: Value =
        serde_json::from_slice(&std::fs::read(harness.temp_path("disconnect.json")).unwrap())
            .unwrap();
    assert_eq!(arguments["terminateDebuggee"], false);
    select(&scope, "launched-owner");
    assert_eq!(count(&harness, tool, &runtime), "808");
    call(&harness, tool, &runtime, json!({"action":"terminate"}));
    finish(&harness, case);
}
