//! Integration tests for the shape-aware conformance harness (bd-ljzb).
//!
//! These tests load real base fixtures through the `QuickJS` runtime and verify
//! that each extension shape is correctly handled: load, registration
//! verification, invocation (where applicable), and shutdown.

mod common;

use pi::conformance_shapes::*;
use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn base_fixtures_dir() -> PathBuf {
    repo_root().join("tests/ext_conformance/artifacts/base_fixtures")
}

static SHAPE_TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn sanitize_fixture_id(fixture_name: &str) -> String {
    fixture_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn shape_test_root(fixture_name: &str) -> PathBuf {
    let sequence = SHAPE_TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "pi-ext-shape-test-{}-{sequence}-{}",
        std::process::id(),
        sanitize_fixture_id(fixture_name)
    ))
}

fn deterministic_env(cwd: &Path, home: &Path) -> HashMap<String, String> {
    let cwd = cwd.to_string_lossy().into_owned();
    let home = home.to_string_lossy().into_owned();
    let mut env = HashMap::new();
    env.insert(
        "PI_DETERMINISTIC_TIME_MS".to_string(),
        "1700000000000".to_string(),
    );
    env.insert("PI_DETERMINISTIC_TIME_STEP_MS".to_string(), "1".to_string());
    env.insert("PI_DETERMINISTIC_CWD".to_string(), cwd);
    env.insert("PI_DETERMINISTIC_HOME".to_string(), home.clone());
    env.insert("HOME".to_string(), home);
    env.insert("PI_DETERMINISTIC_RANDOM".to_string(), "0.5".to_string());
    env
}

/// Load an extension and return manager + runtime + registration snapshot.
fn load_and_snapshot(
    fixture_path: &Path,
    fixture_name: &str,
) -> Result<
    (
        ExtensionManager,
        JsExtensionRuntimeHandle,
        RegistrationSnapshot,
    ),
    String,
> {
    let manager = ExtensionManager::new();
    let root = shape_test_root(fixture_name);
    let cwd = root.join("cwd");
    let home = root.join("home");
    let module_cache = root.join("module-cache");
    let fixture_dir = root.join("fixture").join(fixture_name);
    std::fs::create_dir_all(&cwd).map_err(|e| format!("create cwd: {e}"))?;
    std::fs::create_dir_all(&home).map_err(|e| format!("create home: {e}"))?;
    std::fs::create_dir_all(&module_cache).map_err(|e| format!("create module cache: {e}"))?;
    std::fs::create_dir_all(&fixture_dir).map_err(|e| format!("create fixture dir: {e}"))?;

    // The production loader auto-discovers sibling entrypoints, so copy the
    // selected fixture into a one-fixture root before loading this harness case.
    let isolated_entry = fixture_dir.join("index.ts");
    let _ = std::fs::copy(fixture_path, &isolated_entry)
        .map_err(|e| format!("copy fixture into isolated root: {e}"))?;
    let spec = JsExtensionLoadSpec::from_entry_path(&isolated_entry)
        .map_err(|e| format!("load spec: {e}"))?;

    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));

    let js_config = PiJsRuntimeConfig {
        cwd: cwd.to_string_lossy().into_owned(),
        env: deterministic_env(&cwd, &home),
        disk_cache_dir: Some(module_cache),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .map_err(|e| format!("start runtime: {e}"))
        }
    })?;
    manager.set_js_runtime(runtime.clone());

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .map_err(|e| format!("load extension: {e}"))
        }
    })?;

    // Extract registration snapshot from manager
    let snapshot = RegistrationSnapshot {
        tools: manager.extension_tool_defs(),
        slash_commands: manager.list_commands(),
        shortcuts: manager.list_shortcuts(),
        flags: manager.list_flags(),
        event_hooks: manager.list_event_hooks(),
        providers: manager.extension_providers(),
        models: Vec::new(),
        message_renderers: Vec::new(),
    };

    Ok((manager, runtime, snapshot))
}

/// Run a complete shape test lifecycle on a fixture, returning the result.
#[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
fn run_shape_test(shape: ExtensionShape, fixture_name: &str) -> ShapeTestResult {
    let fixture_path = base_fixtures_dir().join(fixture_name).join("index.ts");
    let correlation_id = format!("shape-test-{shape}");
    let start = Instant::now();
    let mut events = Vec::new();
    let mut all_failures = Vec::new();

    // Phase 1: Load
    let load_start = Instant::now();
    let mut load_event =
        ShapeEvent::new(&correlation_id, fixture_name, shape, LifecyclePhase::Load);

    let load_result = load_and_snapshot(&fixture_path, fixture_name);
    load_event.duration_ms = load_start.elapsed().as_millis() as u64;

    let (manager, runtime, snapshot) = match load_result {
        Ok(result) => {
            load_event.status = ShapeEventStatus::Ok;
            events.push(load_event);
            result
        }
        Err(err) => {
            load_event.status = ShapeEventStatus::Fail;
            let failure = ShapeFailure::new(FailureClass::LoadError, &err);
            load_event.failures.push(failure.clone());
            all_failures.push(failure);
            events.push(load_event);

            return ShapeTestResult {
                extension_id: fixture_name.to_string(),
                extension_path: fixture_path,
                shape,
                detected_shape: ExtensionShape::General,
                passed: false,
                events,
                failures: all_failures,
                total_duration_ms: start.elapsed().as_millis() as u64,
            };
        }
    };

    let detected_shape = snapshot.detected_shape();

    // Phase 2: Verify registrations
    let verify_start = Instant::now();
    let mut verify_event = ShapeEvent::new(
        &correlation_id,
        fixture_name,
        shape,
        LifecyclePhase::VerifyRegistrations,
    );

    let reg_failures = verify_registrations(shape, &snapshot);
    verify_event.duration_ms = verify_start.elapsed().as_millis() as u64;

    if reg_failures.is_empty() {
        verify_event.status = ShapeEventStatus::Ok;
    } else {
        verify_event.status = ShapeEventStatus::Fail;
        verify_event.failures.clone_from(&reg_failures);
        all_failures.extend(reg_failures);
    }
    events.push(verify_event);

    // Phase 3: Invoke (if the shape supports it)
    let invocation = ShapeInvocation::default_for_shape(shape, &snapshot);
    let mut invoke_event =
        ShapeEvent::new(&correlation_id, fixture_name, shape, LifecyclePhase::Invoke);
    let invoke_start = Instant::now();

    match &invocation {
        ShapeInvocation::ToolCall {
            tool_name,
            arguments,
        } => {
            let result = common::run_async({
                let tool_name = tool_name.clone();
                let arguments = arguments.clone();
                async move {
                    runtime
                        .execute_tool(
                            tool_name,
                            "tc-shape-test".to_string(),
                            arguments,
                            std::sync::Arc::new(serde_json::json!({})),
                            20_000,
                        )
                        .await
                        .map_err(|e| format!("execute_tool: {e}"))
                }
            });

            invoke_event.duration_ms = invoke_start.elapsed().as_millis() as u64;
            match result {
                Ok(val) => {
                    invoke_event.status = ShapeEventStatus::Ok;
                    invoke_event.details = Some(val);
                }
                Err(err) => {
                    invoke_event.status = ShapeEventStatus::Fail;
                    let failure = ShapeFailure::new(FailureClass::InvocationError, &err);
                    invoke_event.failures.push(failure.clone());
                    all_failures.push(failure);
                }
            }
        }
        ShapeInvocation::CommandExec { command_name, args } => {
            let result = common::run_async({
                let command_name = command_name.clone();
                let args = args.clone();
                async move {
                    runtime
                        .execute_command(
                            command_name,
                            args,
                            std::sync::Arc::new(serde_json::json!({})),
                            20_000,
                        )
                        .await
                        .map_err(|e| format!("execute_command: {e}"))
                }
            });

            invoke_event.duration_ms = invoke_start.elapsed().as_millis() as u64;
            match result {
                Ok(val) => {
                    invoke_event.status = ShapeEventStatus::Ok;
                    invoke_event.details = Some(val);
                }
                Err(err) => {
                    invoke_event.status = ShapeEventStatus::Fail;
                    let failure = ShapeFailure::new(FailureClass::InvocationError, &err);
                    invoke_event.failures.push(failure.clone());
                    all_failures.push(failure);
                }
            }
        }
        ShapeInvocation::EventDispatch {
            event_name,
            payload,
        } => {
            let result = common::run_async({
                let event_name = event_name.clone();
                let payload = payload.clone();
                async move {
                    runtime
                        .dispatch_event(
                            event_name,
                            payload,
                            std::sync::Arc::new(serde_json::json!({})),
                            20_000,
                        )
                        .await
                        .map_err(|e| format!("dispatch_event: {e}"))
                }
            });

            invoke_event.duration_ms = invoke_start.elapsed().as_millis() as u64;
            match result {
                Ok(val) => {
                    invoke_event.status = ShapeEventStatus::Ok;
                    invoke_event.details = Some(val);
                }
                Err(err) => {
                    invoke_event.status = ShapeEventStatus::Fail;
                    let failure = ShapeFailure::new(FailureClass::InvocationError, &err);
                    invoke_event.failures.push(failure.clone());
                    all_failures.push(failure);
                }
            }
        }
        ShapeInvocation::ProviderCheck
        | ShapeInvocation::UiComponentCheck
        | ShapeInvocation::ConfigurationCheck
        | ShapeInvocation::NoOp => {
            invoke_event.duration_ms = invoke_start.elapsed().as_millis() as u64;
            invoke_event.status = ShapeEventStatus::Skip;
        }
    }
    events.push(invoke_event);

    // Phase 4: Shutdown
    let shutdown_start = Instant::now();
    let mut shutdown_event = ShapeEvent::new(
        &correlation_id,
        fixture_name,
        shape,
        LifecyclePhase::Shutdown,
    );

    let shutdown_ok =
        common::run_async(async move { manager.shutdown(std::time::Duration::from_secs(5)).await });

    shutdown_event.duration_ms = shutdown_start.elapsed().as_millis() as u64;
    if shutdown_ok {
        shutdown_event.status = ShapeEventStatus::Ok;
    } else {
        shutdown_event.status = ShapeEventStatus::Fail;
        let failure = ShapeFailure::new(
            FailureClass::ShutdownError,
            "Shutdown did not complete within budget",
        );
        shutdown_event.failures.push(failure.clone());
        all_failures.push(failure);
    }
    events.push(shutdown_event);

    let passed = all_failures.is_empty();

    ShapeTestResult {
        extension_id: fixture_name.to_string(),
        extension_path: fixture_path,
        shape,
        detected_shape,
        passed,
        events,
        failures: all_failures,
        total_duration_ms: start.elapsed().as_millis() as u64,
    }
}

// ─── Per-shape tests ─────────────────────────────────────────────────────────

#[test]
fn shape_harness_tool_loads_and_registers() {
    let result = run_shape_test(ExtensionShape::Tool, "minimal_tool");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    // Tool fixture should load successfully even if tool is registered
    // via registerTool (which goes to tool registry, not slash_commands).
    // The harness verifies the load completed without error.
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Ok),
        "Load phase should succeed for minimal_tool"
    );
}

#[test]
fn shape_harness_command_loads_and_registers() {
    let result = run_shape_test(ExtensionShape::Command, "minimal_command");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Ok),
        "Load phase should succeed for minimal_command"
    );
}

#[test]
fn shape_harness_provider_loads_and_registers() {
    let result = run_shape_test(ExtensionShape::Provider, "minimal_provider");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Ok),
        "Load phase should succeed for minimal_provider"
    );
}

#[test]
fn shape_harness_event_hook_loads_and_registers() {
    let result = run_shape_test(ExtensionShape::EventHook, "minimal_event");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Ok),
        "Load phase should succeed for minimal_event"
    );
}

#[test]
fn shape_harness_general_loads() {
    let result = run_shape_test(ExtensionShape::General, "minimal_resources");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Ok),
        "Load phase should succeed for minimal_resources"
    );
    // General extensions should pass registration verification (no required registrations)
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::VerifyRegistrations
                && e.status == ShapeEventStatus::Ok),
        "Registration verification should pass for General shape"
    );
}

#[test]
fn shape_harness_configuration_loads_and_registers() {
    let result = run_shape_test(ExtensionShape::Configuration, "minimal_configuration");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Ok),
        "Load phase should succeed for minimal_configuration"
    );
    // Configuration should pass registration (flags or shortcuts present)
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::VerifyRegistrations
                && e.status == ShapeEventStatus::Ok),
        "Registration verification should pass for Configuration shape"
    );
}

#[test]
fn shape_harness_ui_component_loads_and_registers() {
    let result = run_shape_test(ExtensionShape::UiComponent, "minimal_ui_component");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Ok),
        "Load phase should succeed for minimal_ui_component"
    );
}

#[test]
fn shape_harness_multi_loads_and_registers() {
    let result = run_shape_test(ExtensionShape::Multi, "minimal_multi");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Ok),
        "Load phase should succeed for minimal_multi"
    );
    // Multi should verify that 2+ registration types are present
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::VerifyRegistrations
                && e.status == ShapeEventStatus::Ok),
        "Registration verification should pass for Multi shape (tool + event_hook)"
    );
}

// ─── Batch summary test ──────────────────────────────────────────────────────

#[test]
fn shape_harness_batch_summary() {
    let shapes_and_fixtures = [
        (ExtensionShape::Tool, "minimal_tool"),
        (ExtensionShape::Command, "minimal_command"),
        (ExtensionShape::Provider, "minimal_provider"),
        (ExtensionShape::EventHook, "minimal_event"),
        (ExtensionShape::UiComponent, "minimal_ui_component"),
        (ExtensionShape::Configuration, "minimal_configuration"),
        (ExtensionShape::Multi, "minimal_multi"),
        (ExtensionShape::General, "minimal_resources"),
    ];

    let results: Vec<ShapeTestResult> = shapes_and_fixtures
        .iter()
        .map(|(shape, fixture)| run_shape_test(*shape, fixture))
        .collect();

    let summary = ShapeBatchSummary::from_results(&results);
    let md = summary.render_markdown();
    eprintln!("{md}");

    // All 8 shapes should be represented
    assert_eq!(summary.total, 8, "Should have 8 results (one per shape)");
    // Print per-result summary
    for result in &results {
        eprintln!("{}", result.summary_line());
    }
}

// ─── JSONL output test ───────────────────────────────────────────────────────

#[test]
fn shape_harness_emits_valid_jsonl() {
    let result = run_shape_test(ExtensionShape::Tool, "minimal_tool");

    // Each event should be valid JSON
    for event in &result.events {
        let jsonl = event.to_jsonl();
        let parsed: Value = serde_json::from_str(&jsonl)
            .unwrap_or_else(|e| panic!("Invalid JSONL: {e}\nLine: {jsonl}"));

        // Must have required fields
        assert!(parsed.get("timestamp").is_some(), "Missing timestamp");
        assert!(
            parsed.get("correlation_id").is_some(),
            "Missing correlation_id"
        );
        assert!(parsed.get("extension_id").is_some(), "Missing extension_id");
        assert!(parsed.get("shape").is_some(), "Missing shape");
        assert!(parsed.get("phase").is_some(), "Missing phase");
        assert!(parsed.get("status").is_some(), "Missing status");
        assert!(parsed.get("duration_ms").is_some(), "Missing duration_ms");
    }

    // Should have all 4 lifecycle phases
    let phases: Vec<LifecyclePhase> = result.events.iter().map(|e| e.phase).collect();
    assert!(phases.contains(&LifecyclePhase::Load));
    assert!(phases.contains(&LifecyclePhase::VerifyRegistrations));
    assert!(phases.contains(&LifecyclePhase::Invoke));
    assert!(phases.contains(&LifecyclePhase::Shutdown));
}

// ─── Error reporting test ────────────────────────────────────────────────────

#[test]
fn shape_harness_nonexistent_fixture_reports_load_error() {
    let fixture_path = base_fixtures_dir().join("nonexistent").join("index.ts");
    let correlation_id = "test-nonexistent";
    let shape = ExtensionShape::Tool;

    let load_start = Instant::now();
    let mut load_event =
        ShapeEvent::new(correlation_id, "nonexistent", shape, LifecyclePhase::Load);

    let result = load_and_snapshot(&fixture_path, "nonexistent");
    #[allow(clippy::cast_possible_truncation)]
    {
        load_event.duration_ms = load_start.elapsed().as_millis() as u64;
    }

    assert!(result.is_err(), "Should fail on nonexistent fixture");
    let Err(err) = result else { panic!() };

    let failure = ShapeFailure::new(FailureClass::LoadError, &err);
    assert_eq!(failure.class, FailureClass::LoadError);
    assert!(!failure.message.is_empty());

    // The display format should include the class
    let display = failure.to_string();
    assert!(display.contains("load_error"), "Display: {display}");
}

// ─── Negative tests (bd-icjb) ────────────────────────────────────────────────

#[test]
fn negative_syntax_error_fails_cleanly() {
    let result = run_shape_test(ExtensionShape::Tool, "negative_syntax_error");
    eprintln!("{}", result.summary_line());
    assert!(!result.passed, "Syntax error fixture should not pass");
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Fail),
        "Load phase should fail for syntax error"
    );
    // Error message should be useful
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.class == FailureClass::LoadError),
        "Should have LoadError failure class"
    );
}

#[test]
fn negative_no_export_default_fails_cleanly() {
    let result = run_shape_test(ExtensionShape::Tool, "negative_no_export");
    eprintln!("{}", result.summary_line());
    // Missing export default should fail at load or produce empty registrations
    // Either way, tool shape verification should fail
    let has_failure = !result.passed
        || result
            .events
            .iter()
            .any(|e| e.status == ShapeEventStatus::Fail);
    assert!(
        has_failure,
        "No export default should produce a failure somewhere in the lifecycle"
    );
}

#[test]
fn negative_throw_on_load_fails_cleanly() {
    let result = run_shape_test(ExtensionShape::Tool, "negative_throw_on_load");
    eprintln!("{}", result.summary_line());
    assert!(
        !result.passed,
        "Extension that throws on load should not pass"
    );
    assert!(
        result
            .events
            .iter()
            .any(|e| e.phase == LifecyclePhase::Load && e.status == ShapeEventStatus::Fail),
        "Load phase should fail when extension throws"
    );
    // Error message should contain the thrown message
    let load_err = result
        .failures
        .iter()
        .find(|f| f.class == FailureClass::LoadError)
        .map_or("", |f| f.message.as_str());
    assert!(
        load_err.contains("intentionally crashes"),
        "Error should contain the thrown message, got: {load_err}"
    );
}

#[test]
fn negative_invalid_tool_schema_reports_missing_name() {
    let result = run_shape_test(ExtensionShape::Tool, "negative_invalid_tool_schema");
    eprintln!("{}", result.summary_line());
    // Extension should load (the call may or may not throw), but verification
    // should detect the missing tool name or the registration error
    let has_load_or_verify_failure = result.events.iter().any(|e| {
        (e.phase == LifecyclePhase::Load || e.phase == LifecyclePhase::VerifyRegistrations)
            && e.status == ShapeEventStatus::Fail
    });
    assert!(
        has_load_or_verify_failure,
        "Invalid tool schema should fail at load or verification"
    );
}

#[test]
fn negative_wrong_export_type_fails_cleanly() {
    let result = run_shape_test(ExtensionShape::General, "negative_wrong_type");
    eprintln!("{}", result.summary_line());
    // Exporting a string instead of a function should fail at load
    let has_failure = result
        .events
        .iter()
        .any(|e| e.status == ShapeEventStatus::Fail);
    assert!(has_failure, "Exporting wrong type should produce a failure");
}

#[test]
fn negative_missing_handler_registers_but_invocation_fails() {
    let result = run_shape_test(ExtensionShape::Tool, "negative_missing_handler");
    eprintln!("{}", result.summary_line());
    for event in &result.events {
        eprintln!("  {}", event.to_jsonl());
    }
    // If tool registers without handler, invocation should fail
    // (or registration itself might fail)
    let has_failure = result
        .events
        .iter()
        .any(|e| e.status == ShapeEventStatus::Fail);
    assert!(
        has_failure,
        "Tool without execute handler should fail at registration or invocation"
    );
}

#[test]
fn negative_tests_do_not_crash_host() {
    // Run all negative fixtures in sequence to verify none crash the host process
    let negatives = [
        ("negative_syntax_error", ExtensionShape::Tool),
        ("negative_no_export", ExtensionShape::Tool),
        ("negative_throw_on_load", ExtensionShape::Tool),
        ("negative_invalid_tool_schema", ExtensionShape::Tool),
        ("negative_missing_handler", ExtensionShape::Tool),
        ("negative_wrong_type", ExtensionShape::General),
    ];

    let mut pass_count = 0;
    let mut fail_count = 0;
    for (fixture, shape) in &negatives {
        let result = run_shape_test(*shape, fixture);
        if result.passed {
            pass_count += 1;
        } else {
            fail_count += 1;
        }
        eprintln!("{}", result.summary_line());
    }

    eprintln!(
        "\nNegative test summary: {fail_count} correctly failed, {pass_count} unexpectedly passed"
    );
    // At least 4 of 6 should fail (syntax error, throw, invalid schema, wrong type)
    assert!(
        fail_count >= 4,
        "Expected at least 4 negative tests to fail, got {fail_count}"
    );
}
