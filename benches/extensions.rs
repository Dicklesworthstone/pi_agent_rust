//! Benchmarks for extension connector / policy hot paths.
//!
//! Run with:
//! - `cargo bench --bench extensions`
//! - `cargo bench ext_policy`

#[path = "bench_env.rs"]
mod bench_env;

use std::hint::black_box;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::executor::block_on;
use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::{HostcallKind, HostcallRequest, PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::HostcallOutcome;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};

const BENCH_TOOL_CALL: &str = r#"
globalThis.__bench_done = false;
pi.tool("bench_tool", { value: 1 }).then(() => { globalThis.__bench_done = true; });
"#;

const BENCH_TOOL_ASSERT: &str = r#"
if (!globalThis.__bench_done) {
  throw new Error("bench tool call did not resolve");
}
"#;

fn criterion_config() -> Criterion {
    bench_env::criterion_config()
}

fn artifact_single_file_entry(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/ext_conformance/artifacts")
        .join(name)
        .join(format!("{name}.ts"))
}

#[derive(Default)]
struct BenchSession;

#[async_trait::async_trait]
impl pi::extensions::ExtensionSession for BenchSession {
    async fn get_state(&self) -> Value {
        json!({
            "sessionFile": "bench-session.jsonl",
            "sessionName": "bench",
        })
    }

    async fn get_messages(&self) -> Vec<pi::session::SessionMessage> {
        Vec::new()
    }

    async fn get_entries(&self) -> Vec<Value> {
        Vec::new()
    }

    async fn get_branch(&self) -> Vec<Value> {
        Vec::new()
    }

    async fn set_name(&self, _name: String) -> pi::error::Result<()> {
        Ok(())
    }

    async fn append_message(&self, _message: pi::session::SessionMessage) -> pi::error::Result<()> {
        Ok(())
    }

    async fn append_custom_entry(
        &self,
        _custom_type: String,
        _data: Option<Value>,
    ) -> pi::error::Result<()> {
        Ok(())
    }

    async fn set_model(&self, _provider: String, _model_id: String) -> pi::error::Result<()> {
        Ok(())
    }

    async fn get_model(&self) -> (Option<String>, Option<String>) {
        (None, None)
    }

    async fn set_thinking_level(&self, _level: String) -> pi::error::Result<()> {
        Ok(())
    }

    async fn get_thinking_level(&self) -> Option<String> {
        None
    }

    async fn set_label(&self, _target_id: String, _label: Option<String>) -> pi::error::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct BenchUiHandler;

#[async_trait::async_trait]
impl pi::extension_dispatcher::ExtensionUiHandler for BenchUiHandler {
    async fn request_ui(
        &self,
        _request: pi::extensions::ExtensionUiRequest,
    ) -> pi::error::Result<Option<pi::extensions::ExtensionUiResponse>> {
        Ok(None)
    }
}

fn bench_extension_policy(c: &mut Criterion) {
    let prompt = pi::extensions::ExtensionPolicy::default();
    let strict = pi::extensions::ExtensionPolicy {
        mode: pi::extensions::ExtensionPolicyMode::Strict,
        ..pi::extensions::ExtensionPolicy::default()
    };
    let permissive = pi::extensions::ExtensionPolicy {
        mode: pi::extensions::ExtensionPolicyMode::Permissive,
        ..pi::extensions::ExtensionPolicy::default()
    };

    let cases: Vec<(&str, &pi::extensions::ExtensionPolicy, &str)> = vec![
        ("prompt_allow", &prompt, "read"),
        ("prompt_prompt", &prompt, "session"),
        ("prompt_deny", &prompt, "exec"),
        ("strict_allow", &strict, "read"),
        ("strict_deny", &strict, "session"),
        ("permissive_allow", &permissive, "env"),
    ];

    let mut group = c.benchmark_group("ext_policy");
    for (name, policy, cap) in cases {
        group.bench_function(BenchmarkId::new("evaluate", name), |b| {
            b.iter(|| black_box(policy.evaluate(black_box(cap))));
        });
    }
    group.finish();
}

fn bench_required_capability_for_host_call(c: &mut Criterion) {
    let tool_bash = json!({"name": "bash"});
    let tool_read_small = json!({"name": "read"});
    let tool_read_large = {
        let mut obj = serde_json::Map::new();
        obj.insert("name".to_string(), Value::String("read".to_string()));
        for idx in 0..64 {
            obj.insert(format!("pad_{idx:02}"), Value::String("x".repeat(64)));
        }
        Value::Object(obj)
    };
    let empty = json!({});

    let cases: Vec<(&str, pi::extensions::HostCallPayload)> = vec![
        (
            "tool_read_small",
            pi::extensions::HostCallPayload {
                call_id: "call-1".to_string(),
                capability: "read".to_string(),
                method: "tool".to_string(),
                params: tool_read_small,
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "tool_read_large",
            pi::extensions::HostCallPayload {
                call_id: "call-1".to_string(),
                capability: "read".to_string(),
                method: "tool".to_string(),
                params: tool_read_large,
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "tool_bash",
            pi::extensions::HostCallPayload {
                call_id: "call-1".to_string(),
                capability: "exec".to_string(),
                method: "tool".to_string(),
                params: tool_bash,
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "exec",
            pi::extensions::HostCallPayload {
                call_id: "call-1".to_string(),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: empty.clone(),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "http",
            pi::extensions::HostCallPayload {
                call_id: "call-1".to_string(),
                capability: "http".to_string(),
                method: "http".to_string(),
                params: empty.clone(),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "unknown",
            pi::extensions::HostCallPayload {
                call_id: "call-1".to_string(),
                capability: "unknown".to_string(),
                method: "unknown".to_string(),
                params: empty,
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
    ];

    let mut group = c.benchmark_group("ext_required_capability");
    group.throughput(Throughput::Elements(1));
    for (case, call) in cases {
        group.bench_function(BenchmarkId::new("host_call", case), move |b| {
            b.iter(|| {
                black_box(pi::extensions::required_capability_for_host_call(
                    black_box(&call),
                ))
            });
        });
    }
    group.finish();
}

fn bench_dispatch_decision(c: &mut Criterion) {
    let policy = pi::extensions::ExtensionPolicy::default();

    let warm_call = pi::extensions::HostCallPayload {
        call_id: "call-1".to_string(),
        capability: "read".to_string(),
        method: "tool".to_string(),
        params: json!({"name": "read"}),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let mut group = c.benchmark_group("ext_dispatch");

    group.bench_function("decision_warm", |b| {
        b.iter(|| {
            let cap = pi::extensions::required_capability_for_host_call(black_box(&warm_call))
                .unwrap_or_else(|| "unknown".to_string());
            black_box(policy.evaluate(&cap))
        });
    });

    group.bench_function("decision_cold", |b| {
        b.iter_batched(
            || pi::extensions::HostCallPayload {
                call_id: "call-1".to_string(),
                capability: "read".to_string(),
                method: "tool".to_string(),
                params: json!({"name": "read"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
            |call| {
                let cap = pi::extensions::required_capability_for_host_call(black_box(&call))
                    .unwrap_or_else(|| "unknown".to_string());
                black_box(policy.evaluate(&cap))
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_snapshot_lookup(c: &mut Criterion) {
    let mut policy = pi::extensions::ExtensionPolicy::default();
    policy.default_caps.push("read".to_string());
    policy.default_caps.push("write".to_string());
    policy.default_caps.push("http".to_string());
    policy.deny_caps.push("exec".to_string());

    let mut ext_overrides = pi::extensions::ExtensionOverride::default();
    ext_overrides.allow.push("exec".to_string());
    policy
        .per_extension
        .insert("ext.special".to_string(), ext_overrides);

    let snapshot = pi::extensions::PolicySnapshot::compile(&policy);

    let mut group = c.benchmark_group("ext_snapshot");
    group.throughput(Throughput::Elements(1));

    group.bench_function("lookup_global_known", |b| {
        b.iter(|| black_box(snapshot.lookup(black_box("read"), None)));
    });

    group.bench_function("lookup_per_ext_known", |b| {
        b.iter(|| black_box(snapshot.lookup(black_box("exec"), Some("ext.special"))));
    });

    group.bench_function("lookup_unknown_cap", |b| {
        b.iter(|| black_box(snapshot.lookup(black_box("custom_xyz"), None)));
    });

    group.bench_function("lookup_unknown_ext_fallback", |b| {
        b.iter(|| black_box(snapshot.lookup(black_box("read"), Some("ext.unknown"))));
    });

    // Compare: direct evaluate_for for same operations
    group.bench_function("evaluate_for_baseline", |b| {
        b.iter(|| black_box(policy.evaluate_for(black_box("read"), None)));
    });

    group.bench_function("compile", |b| {
        b.iter(|| black_box(pi::extensions::PolicySnapshot::compile(black_box(&policy))));
    });

    group.finish();
}

fn bench_protocol_parse_and_validate(c: &mut Criterion) {
    let host_call_small = format!(
        r#"{{"id":"msg-1","version":"{}","type":"host_call","payload":{{"call_id":"call-1","capability":"read","method":"tool","params":{{"name":"read"}}}}}}"#,
        pi::extensions::PROTOCOL_VERSION
    );

    let big_text = "x".repeat(16 * 1024);
    let log_big = format!(
        r#"{{"id":"msg-2","version":"{}","type":"log","payload":{{"schema":"{}","ts":"2026-02-03T00:00:00.000Z","level":"info","event":"bench","message":"{}","correlation":{{"extension_id":"ext","scenario_id":"scn"}},"source":{{"component":"runtime"}}}}}}"#,
        pi::extensions::PROTOCOL_VERSION,
        pi::extensions::LOG_SCHEMA_VERSION,
        big_text
    );

    let cases: Vec<(&str, &str)> =
        vec![("host_call_small", &host_call_small), ("log_big", &log_big)];

    let mut group = c.benchmark_group("ext_protocol");
    for (name, payload) in cases {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_function(BenchmarkId::new("parse_and_validate", name), |b| {
            b.iter(|| {
                black_box(pi::extensions::ExtensionMessage::parse_and_validate(
                    payload,
                ))
            });
        });
    }
    group.finish();
}

fn bench_protocol_dispatch(c: &mut Criterion) {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let runtime = Rc::new(block_on(PiJsRuntime::new()).expect("create PiJsRuntime"));
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let http_connector = Arc::new(pi::connectors::http::HttpConnector::new(
        pi::connectors::http::HttpConnectorConfig::default(),
    ));
    let session: Arc<dyn pi::extensions::ExtensionSession + Send + Sync> = Arc::new(BenchSession);
    let ui_handler: Arc<dyn pi::extension_dispatcher::ExtensionUiHandler + Send + Sync> =
        Arc::new(BenchUiHandler);
    let dispatcher =
        pi::ExtensionDispatcher::new(runtime, tools, http_connector, session, ui_handler, cwd);

    let host_call = pi::extensions::HostCallPayload {
        call_id: "bench-call-1".to_string(),
        capability: "session".to_string(),
        method: "session".to_string(),
        params: json!({ "op": "get_state" }),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };
    let message = pi::extensions::ExtensionMessage {
        id: "bench-msg-1".to_string(),
        version: pi::extensions::PROTOCOL_VERSION.to_string(),
        body: pi::extensions::ExtensionBody::HostCall(host_call),
    };

    let mut group = c.benchmark_group("ext_protocol_dispatch");
    group.throughput(Throughput::Elements(1));
    group.bench_function("session_get_state", |b| {
        b.iter(|| {
            let response =
                block_on(dispatcher.dispatch_protocol_message(black_box(message.clone())))
                    .expect("dispatch protocol message");
            black_box(response);
        });
    });
    group.finish();
}

fn bench_extension_load_init(c: &mut Criterion) {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let js_cwd = cwd.display().to_string();

    let mut group = c.benchmark_group("ext_load_init");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    let cases = [
        ("hello", artifact_single_file_entry("hello")),
        ("pirate", artifact_single_file_entry("pirate")),
    ];

    for (ext_name, entry_path) in cases {
        let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).unwrap_or_else(|_| {
            panic!(
                "expected extension artifact entry at {}",
                entry_path.display()
            )
        });
        let cwd = cwd.clone();
        let js_cwd = js_cwd.clone();

        group.bench_function(BenchmarkId::new("load_init_cold", ext_name), move |b| {
            let spec = spec.clone();
            let cwd = cwd.clone();
            let js_cwd = js_cwd.clone();

            b.iter_batched(
                || {
                    let manager = ExtensionManager::new();
                    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
                    let runtime = block_on({
                        let manager = manager.clone();
                        let tools = Arc::clone(&tools);
                        let js_config = PiJsRuntimeConfig {
                            cwd: js_cwd.clone(),
                            ..Default::default()
                        };
                        async move {
                            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                                .await
                                .expect("start js runtime")
                        }
                    });
                    manager.set_js_runtime(runtime);
                    manager
                },
                |manager| {
                    block_on({
                        let spec = spec.clone();
                        async move {
                            manager
                                .load_js_extensions(vec![spec])
                                .await
                                .expect("load extension");
                            // Avoid leaking runtime threads during benchmark runs.
                            let _ok = manager.shutdown(Duration::from_millis(250)).await;
                        }
                    });
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

fn bench_extension_tool_call_roundtrip(c: &mut Criterion) {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let js_cwd = cwd.display().to_string();
    let entry_path = artifact_single_file_entry("hello");
    let spec = JsExtensionLoadSpec::from_entry_path(&entry_path)
        .unwrap_or_else(|_| panic!("expected hello artifact at {}", entry_path.display()));

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: js_cwd.clone(),
        ..Default::default()
    };

    let runtime = block_on({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    block_on({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load hello extension");
        }
    });

    let runtime = manager.js_runtime().expect("runtime should exist");
    let tool_name = "hello".to_string();
    let call_id = "bench-call-1".to_string();
    let input = json!({"name": "World"});
    let ctx_payload = std::sync::Arc::new(json!({ "hasUI": false, "cwd": js_cwd }));

    {
        let mut group = c.benchmark_group("ext_tool_call");
        group.throughput(Throughput::Elements(1));
        // Reduce sample size — each iteration spawns threads that may not be
        // reclaimed fast enough on resource-constrained CI runners.
        group.sample_size(10);
        group.bench_function("hello", |b| {
            b.iter(|| {
                let result = block_on(runtime.execute_tool(
                    black_box(tool_name.clone()),
                    black_box(call_id.clone()),
                    black_box(input.clone()),
                    black_box(ctx_payload.clone()),
                    5_000,
                ))
                .expect("execute hello tool");
                black_box(result);
            });
        });
        group.finish();
    }

    let _ = block_on(manager.shutdown(Duration::from_millis(250)));
}

fn bench_extension_event_hook_dispatch(c: &mut Criterion) {
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let js_cwd = cwd.display().to_string();
    let entry_path = artifact_single_file_entry("pirate");
    let spec = JsExtensionLoadSpec::from_entry_path(&entry_path)
        .unwrap_or_else(|_| panic!("expected pirate artifact at {}", entry_path.display()));

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: js_cwd,
        ..Default::default()
    };

    let runtime = block_on({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(runtime);

    block_on({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load pirate extension");
        }
    });

    let event_payload = json!({"systemPrompt": "You are Pi."});
    let manager_for_bench = manager.clone();

    {
        let mut group = c.benchmark_group("ext_event_hook");
        group.throughput(Throughput::Elements(1));
        group.bench_function("before_agent_start", move |b| {
            let manager = manager_for_bench.clone();
            let event_payload = event_payload.clone();

            b.iter(|| {
                block_on(manager.dispatch_event_with_response(
                    ExtensionEventName::BeforeAgentStart,
                    Some(black_box(event_payload.clone())),
                    5_000,
                ))
                .expect("dispatch before_agent_start");
            });
        });
        group.finish();
    }

    let _ = block_on(manager.shutdown(Duration::from_millis(250)));
}

fn bench_js_runtime(c: &mut Criterion) {
    let mut group = c.benchmark_group("ext_js_runtime");
    group.bench_function("cold_start", |b| {
        b.iter(|| {
            block_on(async {
                let rt = PiJsRuntime::new().await.unwrap();
                rt.drain_microtasks().await.unwrap();
            });
        });
    });

    let rt = block_on(PiJsRuntime::new()).unwrap();
    group.bench_function("warm_eval_noop", |b| {
        b.iter(|| {
            block_on(async {
                rt.eval("1 + 1;").await.unwrap();
                rt.drain_microtasks().await.unwrap();
            });
        });
    });

    group.bench_function("warm_run_pending_jobs_empty", |b| {
        b.iter(|| {
            block_on(async {
                rt.drain_microtasks().await.unwrap();
            });
        });
    });

    let tool_runtime = block_on(
        PiJsRuntime::with_clock_and_config_with_policy_for_extension(
            pi::scheduler::WallClock,
            PiJsRuntimeConfig::default(),
            None,
            "ext.bench".to_string(),
        ),
    )
    .unwrap();
    group.bench_function("tool_call_roundtrip", |b| {
        b.iter(|| {
            block_on(async {
                tool_runtime
                    .eval(BENCH_TOOL_CALL)
                    .await
                    .expect("eval tool call");
                let mut requests = tool_runtime.drain_hostcall_requests();
                assert_eq!(requests.len(), 1, "expected one hostcall request");
                let request = requests.pop_front().expect("hostcall request");
                tool_runtime.complete_hostcall(
                    request.call_id,
                    HostcallOutcome::Success(json!({"ok": true})),
                );
                tool_runtime.tick().await.expect("tick");
                tool_runtime
                    .eval(BENCH_TOOL_ASSERT)
                    .await
                    .expect("assert bench tool call");
            });
        });
    });

    group.finish();
}

fn bench_hostcall_params_hash(c: &mut Criterion) {
    let small_payload = json!({
        "name": "read",
        "input": {
            "path": "README.md"
        }
    });

    let mut large_input = serde_json::Map::new();
    for idx in 0..64 {
        large_input.insert(format!("k{idx:02}"), Value::String("x".repeat(64)));
    }
    large_input.insert(
        "nested".to_string(),
        json!({
            "z": 1,
            "a": [1, 2, 3, {"k": "v"}]
        }),
    );
    let large_payload = Value::Object(large_input);

    let req_small = HostcallRequest {
        call_id: "bench-hash-small".to_string(),
        kind: HostcallKind::Tool {
            name: "read".to_string(),
        },
        payload: small_payload,
        trace_id: 1,
        extension_id: Some("ext.bench".to_string()),
    };

    let req_large = HostcallRequest {
        call_id: "bench-hash-large".to_string(),
        kind: HostcallKind::Tool {
            name: "read".to_string(),
        },
        payload: large_payload,
        trace_id: 2,
        extension_id: Some("ext.bench".to_string()),
    };

    let mut group = c.benchmark_group("ext_hostcall_hash");
    group.throughput(Throughput::Elements(1));

    group.bench_function("request_small", |b| {
        b.iter(|| black_box(req_small.params_hash()));
    });

    group.bench_function("request_large", |b| {
        b.iter(|| black_box(req_large.params_hash()));
    });

    group.finish();
}

// ============================================================================
// Phase 3 profiling: Hostcall bridge roundtrip decomposition (bd-3ar8v.4.1)
// ============================================================================

/// Measure `hostcall_request_to_payload` conversion overhead for various
/// hostcall kinds and payload sizes.
fn bench_hostcall_request_to_payload(c: &mut Criterion) {
    use pi::extensions::hostcall_request_to_payload;

    let tool_small = HostcallRequest {
        call_id: "call-1".to_string(),
        kind: HostcallKind::Tool {
            name: "read".to_string(),
        },
        payload: json!({"path": "README.md"}),
        trace_id: 1,
        extension_id: Some("ext.bench".to_string()),
    };

    let session_op = HostcallRequest {
        call_id: "call-2".to_string(),
        kind: HostcallKind::Session {
            op: "get_state".to_string(),
        },
        payload: json!({}),
        trace_id: 2,
        extension_id: Some("ext.bench".to_string()),
    };

    let events_op = HostcallRequest {
        call_id: "call-3".to_string(),
        kind: HostcallKind::Events {
            op: "getActiveTools".to_string(),
        },
        payload: json!({}),
        trace_id: 3,
        extension_id: Some("ext.bench".to_string()),
    };

    // Large payload: 64 keys with 64-char values
    let mut large_input = serde_json::Map::new();
    for idx in 0..64 {
        large_input.insert(format!("k{idx:02}"), Value::String("x".repeat(64)));
    }
    let tool_large = HostcallRequest {
        call_id: "call-4".to_string(),
        kind: HostcallKind::Tool {
            name: "read".to_string(),
        },
        payload: Value::Object(large_input),
        trace_id: 4,
        extension_id: Some("ext.bench".to_string()),
    };

    let mut group = c.benchmark_group("ext_request_to_payload");
    group.throughput(Throughput::Elements(1));

    group.bench_function("tool_small", |b| {
        b.iter(|| black_box(hostcall_request_to_payload(black_box(&tool_small))));
    });
    group.bench_function("tool_large_64k", |b| {
        b.iter(|| black_box(hostcall_request_to_payload(black_box(&tool_large))));
    });
    group.bench_function("session_get_state", |b| {
        b.iter(|| black_box(hostcall_request_to_payload(black_box(&session_op))));
    });
    group.bench_function("events_get_active_tools", |b| {
        b.iter(|| black_box(hostcall_request_to_payload(black_box(&events_op))));
    });
    group.finish();
}

/// Measure `host_result_to_outcome` and `outcome_to_host_result` conversion
/// overhead (the Rust↔JS result bridge).
fn bench_hostcall_outcome_conversion(c: &mut Criterion) {
    use pi::extensions::{host_result_to_outcome, outcome_to_host_result};
    use pi::scheduler::HostcallOutcome;

    let success_small = HostcallOutcome::Success(json!({"ok": true}));
    let success_large = HostcallOutcome::Success(json!({
        "content": "x".repeat(8192),
        "metadata": {"lines": 200, "bytes": 8192}
    }));
    let error = HostcallOutcome::Error {
        code: "denied".to_string(),
        message: "Policy denied access".to_string(),
    };
    let stream_chunk = HostcallOutcome::StreamChunk {
        sequence: 42,
        chunk: json!({"data": "partial response"}),
        is_final: false,
    };

    let mut group = c.benchmark_group("ext_outcome_conversion");
    group.throughput(Throughput::Elements(1));

    // outcome → host_result
    group.bench_function("to_host_result/success_small", |b| {
        b.iter(|| black_box(outcome_to_host_result("call-1", black_box(&success_small))));
    });
    group.bench_function("to_host_result/success_large", |b| {
        b.iter(|| black_box(outcome_to_host_result("call-1", black_box(&success_large))));
    });
    group.bench_function("to_host_result/error", |b| {
        b.iter(|| black_box(outcome_to_host_result("call-1", black_box(&error))));
    });
    group.bench_function("to_host_result/stream_chunk", |b| {
        b.iter(|| black_box(outcome_to_host_result("call-1", black_box(&stream_chunk))));
    });

    // host_result → outcome
    let result_success = outcome_to_host_result("call-1", &success_small);
    let result_error = outcome_to_host_result("call-1", &error);
    let result_stream = outcome_to_host_result("call-1", &stream_chunk);

    group.bench_function("to_outcome/success", |b| {
        b.iter(|| black_box(host_result_to_outcome(black_box(result_success.clone()))));
    });
    group.bench_function("to_outcome/error", |b| {
        b.iter(|| black_box(host_result_to_outcome(black_box(result_error.clone()))));
    });
    group.bench_function("to_outcome/stream_chunk", |b| {
        b.iter(|| black_box(host_result_to_outcome(black_box(result_stream.clone()))));
    });

    group.finish();
}

/// Measure the full `dispatch_host_call_shared` roundtrip for session ops,
/// which are the most common lightweight hostcalls.
fn bench_dispatch_shared_session(c: &mut Criterion) {
    use pi::connectors::http::{HttpConnector, HttpConnectorConfig};
    use pi::extensions::{
        ExtensionManager, ExtensionPolicy, HostCallContext, HostCallPayload,
        dispatch_host_call_shared,
    };

    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let http = Arc::new(HttpConnector::new(HttpConnectorConfig::default()));
    let policy = ExtensionPolicy::default();

    let manager = ExtensionManager::new();
    let session: Arc<dyn pi::extensions::ExtensionSession + Send + Sync> = Arc::new(BenchSession);
    manager.set_session(session);

    let calls: Vec<(&str, HostCallPayload)> = vec![
        (
            "get_state",
            HostCallPayload {
                call_id: "call-1".to_string(),
                capability: "session".to_string(),
                method: "session".to_string(),
                params: json!({"op": "get_state"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "get_name",
            HostCallPayload {
                call_id: "call-2".to_string(),
                capability: "session".to_string(),
                method: "session".to_string(),
                params: json!({"op": "get_name"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "set_name",
            HostCallPayload {
                call_id: "call-3".to_string(),
                capability: "session".to_string(),
                method: "session".to_string(),
                params: json!({"op": "set_name", "name": "bench-session"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "get_model",
            HostCallPayload {
                call_id: "call-4".to_string(),
                capability: "session".to_string(),
                method: "session".to_string(),
                params: json!({"op": "get_model"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
    ];

    let mut group = c.benchmark_group("ext_dispatch_shared_session");
    group.throughput(Throughput::Elements(1));

    for (name, call) in &calls {
        let ctx = HostCallContext {
            runtime_name: "bench",
            extension_id: Some("ext.bench"),
            tools: &tools,
            http: &http,
            manager: Some(manager.clone()),
            policy: &policy,
            js_runtime: None,
            interceptor: None,
        };
        let call = call.clone();
        group.bench_function(BenchmarkId::new("roundtrip", name), move |b| {
            b.iter(|| {
                block_on(dispatch_host_call_shared(
                    black_box(&ctx),
                    black_box(call.clone()),
                ))
            });
        });
    }

    group.finish();
}

/// Measure the full `dispatch_host_call_shared` roundtrip for events ops.
fn bench_dispatch_shared_events(c: &mut Criterion) {
    use pi::connectors::http::{HttpConnector, HttpConnectorConfig};
    use pi::extensions::{
        ExtensionManager, ExtensionPolicy, HostCallContext, HostCallPayload,
        dispatch_host_call_shared,
    };

    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let http = Arc::new(HttpConnector::new(HttpConnectorConfig::default()));
    let policy = ExtensionPolicy::default();

    let manager = ExtensionManager::new();

    let calls: Vec<(&str, HostCallPayload)> = vec![
        (
            "getActiveTools",
            HostCallPayload {
                call_id: "call-e1".to_string(),
                capability: "events".to_string(),
                method: "events".to_string(),
                params: json!({"op": "getActiveTools"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "getAllTools",
            HostCallPayload {
                call_id: "call-e2".to_string(),
                capability: "events".to_string(),
                method: "events".to_string(),
                params: json!({"op": "getAllTools"}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
        (
            "emit",
            HostCallPayload {
                call_id: "call-e3".to_string(),
                capability: "events".to_string(),
                method: "events".to_string(),
                params: json!({"op": "emit", "event": "test_event", "data": {"key": "value"}}),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            },
        ),
    ];

    let mut group = c.benchmark_group("ext_dispatch_shared_events");
    group.throughput(Throughput::Elements(1));

    for (name, call) in &calls {
        let ctx = HostCallContext {
            runtime_name: "bench",
            extension_id: Some("ext.bench"),
            tools: &tools,
            http: &http,
            manager: Some(manager.clone()),
            policy: &policy,
            js_runtime: None,
            interceptor: None,
        };
        let call = call.clone();
        group.bench_function(BenchmarkId::new("roundtrip", name), move |b| {
            b.iter(|| {
                block_on(dispatch_host_call_shared(
                    black_box(&ctx),
                    black_box(call.clone()),
                ))
            });
        });
    }

    group.finish();
}

/// Measure the JS↔Rust serialization bridge overhead by roundtripping
/// payloads through hostcall requests.
///
/// Each hostcall crosses the JS→JSON boundary on entry (`js_to_json` in the
/// native function) and JSON→JS on return (`json_to_js` in `deliver_completion`).
/// We measure this indirectly via the `complete_hostcall` + `tick` path with
/// varying payload sizes.
fn bench_js_serde_bridge(c: &mut Criterion) {
    // Use an immutable benchmark principal so hostcall attribution follows the
    // same Rust-owned identity boundary as production extension shards.
    let rt = block_on(
        PiJsRuntime::with_clock_and_config_with_policy_for_extension(
            pi::scheduler::WallClock,
            PiJsRuntimeConfig::default(),
            None,
            "ext.bench".to_string(),
        ),
    )
    .unwrap();

    let payloads: Vec<(&str, Value)> = vec![
        ("null", Value::Null),
        ("bool", json!(true)),
        ("int", json!(42)),
        ("string_short", json!("hello")),
        ("string_1kb", json!("x".repeat(1024))),
        ("object_small", json!({"name": "read", "path": "README.md"})),
        ("object_medium", {
            let mut obj = serde_json::Map::new();
            for i in 0..16 {
                obj.insert(format!("key_{i:02}"), json!(format!("value_{i}")));
            }
            Value::Object(obj)
        }),
        ("object_large", {
            let mut obj = serde_json::Map::new();
            for i in 0..64 {
                obj.insert(format!("key_{i:02}"), json!("x".repeat(64)));
            }
            Value::Object(obj)
        }),
        ("array_10", json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10])),
        (
            "nested_deep",
            json!({
                "a": {"b": {"c": {"d": {"e": {"f": "deep_value"}}}}}
            }),
        ),
    ];

    // Measure the complete_hostcall + tick path which exercises json_to_js
    // (converting the outcome Value back to JS) and drain_jobs (resolving
    // the Promise). This is the return-path serialization hot path.
    let mut group = c.benchmark_group("ext_serde_bridge_roundtrip");
    group.throughput(Throughput::Elements(1));

    for (name, payload) in &payloads {
        let payload = payload.clone();
        group.bench_function(*name, |b| {
            b.iter(|| {
                block_on(async {
                    // Trigger a tool call from JS to generate a hostcall request.
                    rt.eval(BENCH_TOOL_CALL).await.expect("eval tool call");
                    let mut requests = rt.drain_hostcall_requests();
                    let request = requests.pop_front().expect("hostcall request");
                    // Complete with the test payload — this exercises json_to_js.
                    rt.complete_hostcall(
                        request.call_id,
                        HostcallOutcome::Success(black_box(payload.clone())),
                    );
                    rt.tick().await.expect("tick");
                });
            });
        });
    }

    group.finish();
}

/// Measure policy evaluation in the full shared-dispatch context including
/// quota check and runtime risk evaluation overhead.
fn bench_dispatch_overhead_breakdown(c: &mut Criterion) {
    use pi::connectors::http::{HttpConnector, HttpConnectorConfig};
    use pi::extensions::{
        ExtensionManager, ExtensionPolicy, ExtensionPolicyMode, HostCallContext, HostCallPayload,
        dispatch_host_call_shared,
    };

    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let http = Arc::new(HttpConnector::new(HttpConnectorConfig::default()));

    let policies = vec![
        ("default", ExtensionPolicy::default()),
        (
            "strict",
            ExtensionPolicy {
                mode: ExtensionPolicyMode::Strict,
                ..ExtensionPolicy::default()
            },
        ),
        (
            "permissive",
            ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                ..ExtensionPolicy::default()
            },
        ),
    ];

    // A simple session read call that will be allowed through policy.
    let call = HostCallPayload {
        call_id: "call-overhead".to_string(),
        capability: "session".to_string(),
        method: "session".to_string(),
        params: json!({"op": "get_state"}),
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    let mut group = c.benchmark_group("ext_dispatch_overhead");
    group.throughput(Throughput::Elements(1));

    // Without manager (minimal overhead path)
    for (policy_name, policy) in &policies {
        let ctx = HostCallContext {
            runtime_name: "bench",
            extension_id: Some("ext.bench"),
            tools: &tools,
            http: &http,
            manager: None,
            policy,
            js_runtime: None,
            interceptor: None,
        };
        let call = call.clone();
        group.bench_function(BenchmarkId::new("no_manager", *policy_name), move |b| {
            b.iter(|| {
                block_on(dispatch_host_call_shared(
                    black_box(&ctx),
                    black_box(call.clone()),
                ))
            });
        });
    }

    // With manager (full overhead: quota + risk eval)
    let manager = ExtensionManager::new();
    let session: Arc<dyn pi::extensions::ExtensionSession + Send + Sync> = Arc::new(BenchSession);
    manager.set_session(session);

    for (policy_name, policy) in &policies {
        let ctx = HostCallContext {
            runtime_name: "bench",
            extension_id: Some("ext.bench"),
            tools: &tools,
            http: &http,
            manager: Some(manager.clone()),
            policy,
            js_runtime: None,
            interceptor: None,
        };
        let call = call.clone();
        group.bench_function(BenchmarkId::new("with_manager", *policy_name), move |b| {
            b.iter(|| {
                block_on(dispatch_host_call_shared(
                    black_box(&ctx),
                    black_box(call.clone()),
                ))
            });
        });
    }

    group.finish();
}

criterion_group!(
    name = benches;
    config = criterion_config();
    targets =
        bench_extension_policy,
        bench_required_capability_for_host_call,
        bench_dispatch_decision,
        bench_snapshot_lookup,
        bench_protocol_parse_and_validate,
        bench_protocol_dispatch,
        bench_extension_load_init,
        bench_extension_tool_call_roundtrip,
        bench_extension_event_hook_dispatch,
        bench_js_runtime,
        bench_hostcall_params_hash,
        bench_hostcall_request_to_payload,
        bench_hostcall_outcome_conversion,
        bench_dispatch_shared_session,
        bench_dispatch_shared_events,
        bench_js_serde_bridge,
        bench_dispatch_overhead_breakdown
);
criterion_main!(benches);
