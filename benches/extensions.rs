//! Benchmarks for extension connector / policy hot paths.
//!
//! Run with:
//! - `cargo bench --bench extensions`
//! - `cargo bench ext_policy`

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures::executor::block_on;
use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::HostcallOutcome;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sysinfo::System;

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

const BENCH_TOOL_SETUP: &str = r#"
__pi_begin_extension("ext.bench", { name: "Bench" });
pi.registerTool({
  name: "bench_tool",
  description: "Benchmark tool",
  parameters: { type: "object", properties: { value: { type: "number" } } },
  execute: async (_callId, input) => {
    return { ok: true, value: input.value };
  },
});
__pi_end_extension();
"#;

const BENCH_TOOL_CALL: &str = r#"
globalThis.__bench_done = false;
pi.tool("bench_tool", { value: 1 }).then(() => { globalThis.__bench_done = true; });
"#;

const BENCH_TOOL_ASSERT: &str = r#"
if (!globalThis.__bench_done) {
  throw new Error("bench tool call did not resolve");
}
"#;

fn print_bench_banner_once() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();

        let cpu_brand = system
            .cpus()
            .first()
            .map_or_else(|| "unknown".to_string(), |cpu| cpu.brand().to_string());

        let config = format!(
            "pkg={} git_sha={} build_ts={}",
            env!("CARGO_PKG_VERSION"),
            option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
            option_env!("VERGEN_BUILD_TIMESTAMP").unwrap_or(""),
        );
        let config_hash = sha256_hex(&config);

        eprintln!(
            "[bench-env] os={} arch={} cpu=\"{}\" cores={} mem_kb={} config_hash={}",
            System::long_os_version().unwrap_or_else(|| std::env::consts::OS.to_string()),
            std::env::consts::ARCH,
            cpu_brand,
            system.cpus().len(),
            system.total_memory(),
            config_hash
        );
    });
}

fn criterion_config() -> Criterion {
    print_bench_banner_once();
    Criterion::default()
}

fn artifact_single_file_entry(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/ext_conformance/artifacts")
        .join(name)
        .join(format!("{name}.ts"))
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
    let ctx_payload = json!({ "hasUI": false, "cwd": js_cwd });

    {
        let mut group = c.benchmark_group("ext_tool_call");
        group.throughput(Throughput::Elements(1));
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
                let rt = pi::extensions_js::QuickJsRuntime::new().await.unwrap();
                rt.run_pending_jobs().await.unwrap();
            });
        });
    });

    let rt = block_on(pi::extensions_js::QuickJsRuntime::new()).unwrap();
    group.bench_function("warm_eval_noop", |b| {
        b.iter(|| {
            block_on(async {
                rt.eval("1 + 1;").await.unwrap();
                rt.run_pending_jobs().await.unwrap();
            });
        });
    });

    group.bench_function("warm_run_pending_jobs_empty", |b| {
        b.iter(|| {
            block_on(async {
                rt.run_pending_jobs().await.unwrap();
            });
        });
    });

    let tool_runtime = block_on(PiJsRuntime::new()).unwrap();
    block_on(async {
        tool_runtime
            .eval(BENCH_TOOL_SETUP)
            .await
            .expect("register bench tool");
    });
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

criterion_group!(
    name = benches;
    config = criterion_config();
    targets =
        bench_extension_policy,
        bench_required_capability_for_host_call,
        bench_dispatch_decision,
        bench_protocol_parse_and_validate,
        bench_extension_load_init,
        bench_extension_tool_call_roundtrip,
        bench_extension_event_hook_dispatch,
        bench_js_runtime
);
criterion_main!(benches);
