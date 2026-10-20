//! Isolated loopback transport used by daemon-shaped extensions such as pi-freeflow.
//! No test binds an operating-system listener or permits external network access.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::{DeterministicClock, HostcallOutcome};
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::sync::Arc;

fn load_extension(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let entry = harness.create_file("installed extension/server.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&entry).expect("load spec");
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };
    let runtime = common::run_async({
        let manager = manager.clone();
        async move {
            JsExtensionRuntimeHandle::start(config, tools, manager)
                .await
                .expect("start runtime")
        }
    });
    manager.set_js_runtime(runtime);
    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });
    manager
}

fn run_case(name: &str, body: &str) -> Value {
    let harness = common::TestHarness::new(name);
    let source = format!(
        r#"
import http from 'node:http';
export default function(pi) {{
    pi.on('agent_start', async () => {{
        {body}
    }});
}}
"#
    );
    let manager = load_extension(&harness, &source);
    common::run_async(async move {
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10_000)
            .await
            .expect("dispatch event")
            .and_then(|value| value.get("result").cloned())
            .expect("result")
    })
}

#[test]
fn virtual_http_preserves_request_routing_headers_and_binary_bytes() {
    let result = run_case(
        "virtual_http_binary",
        r#"
        let observed;
        const server = http.createServer((req, res) => {
            const chunks = [];
            req.on('data', chunk => chunks.push(chunk));
            req.on('end', () => {
                observed = { method: req.method, path: req.url, token: req.headers['x-token'], host: req.headers.host };
                res.writeHead(201, { 'content-type': 'application/octet-stream', 'x-local': 'yes' });
                res.end(Buffer.concat(chunks));
            });
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const response = await fetch('http://127.0.0.1:28180/v1/chat/completions?q=1', {
                method: 'POST', headers: { 'X-Token': 'retained' }, body: new Uint8Array([0, 255, 65]),
            });
            return { result: { ...observed, status: response.status, header: response.headers.get('x-local'), bytes: Array.from(new Uint8Array(await response.arrayBuffer())) } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(
        result,
        json!({
            "method": "POST", "path": "/v1/chat/completions?q=1", "token": "retained",
            "host": "127.0.0.1:28180", "status": 201, "header": "yes", "bytes": [0, 255, 65],
        })
    );
}

#[test]
fn virtual_http_fetch_settles_at_headers_before_delayed_body() {
    let result = run_case(
        "virtual_http_stream_headers",
        r#"
        let ended = false;
        let finish;
        const server = http.createServer((_req, res) => {
            finish = () => { ended = true; res.end('data: hello\n\n'); };
            res.writeHead(200, { 'content-type': 'text/event-stream' });
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const response = await fetch('http://127.0.0.1:28180/v1/chat/completions');
            const beforeBody = ended;
            finish();
            return { result: { beforeBody, body: await response.text(), ended } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(
        result,
        json!({"beforeBody": false, "body": "data: hello\n\n", "ended": true})
    );
}

#[test]
fn virtual_http_abort_before_headers_closes_the_actual_request_once() {
    let result = run_case(
        "virtual_http_abort_headers",
        r#"
        let closed = 0;
        const controller = new AbortController();
        const server = http.createServer((_req, res) => {
            res.on('close', () => { closed++; });
            queueMicrotask(() => { controller.abort(); controller.abort(); });
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            try {
                await fetch('http://127.0.0.1:28180/pending', { signal: controller.signal });
                throw new Error('fetch unexpectedly completed');
            } catch (error) { return { result: { name: error.name, closed } }; }
        } finally { server.close(); }
        "#,
    );
    assert_eq!(result, json!({"name": "AbortError", "closed": 1}));
}

#[test]
fn virtual_http_body_cancel_releases_the_producer() {
    let result = run_case(
        "virtual_http_body_cancel",
        r#"
        let closed = false;
        const server = http.createServer((_req, res) => {
            res.on('close', () => { closed = true; });
            res.writeHead(200);
            res.write('first');
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const response = await fetch('http://127.0.0.1:28180/stream');
            const reader = response.body.getReader();
            const first = await reader.read();
            await reader.cancel();
            return { result: { text: new TextDecoder().decode(first.value), closed } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(result, json!({"text": "first", "closed": true}));
}

#[test]
fn virtual_http_rejects_external_bind_and_bounds_unconsumed_response() {
    let result = run_case(
        "virtual_http_bounds",
        r#"
        let externalDenied = false;
        try { http.createServer(() => {}).listen(28180, '0.0.0.0'); }
        catch (error) { externalDenied = error.message.includes('loopback'); }
        const server = http.createServer((_req, res) => res.end(new Uint8Array(1024 * 1024 + 1)));
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            try {
                const response = await fetch('http://127.0.0.1:28180/too-large');
                await response.arrayBuffer();
                return { result: { externalDenied, bounded: false } };
            } catch (error) { return { result: { externalDenied, bounded: error.message.includes('1 MiB') } }; }
        } finally { server.close(); }
        "#,
    );
    assert_eq!(result, json!({"externalDenied": true, "bounded": true}));
}

#[test]
fn virtual_http_provider_transport_is_derived_from_owned_listener() {
    let harness = common::TestHarness::new("virtual_http_provider_ownership");
    let manager = load_extension(
        &harness,
        r#"
        import http from 'node:http';
        export default async function(pi) {
            const server = http.createServer((_req, res) => res.end('ok'));
            await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
            const model = { id: 'test', name: 'test', contextWindow: 4096, maxTokens: 512 };
            pi.registerProvider('owned', { api: 'openai-completions', baseUrl: 'http://127.0.0.1:28180/v1', models: [model] });
            pi.registerProvider('forged', { api: 'openai-completions', baseUrl: 'https://example.com/v1', virtualHttpServer: 'virtual-http-server-1', models: [model] });
        }
        "#,
    );
    let providers = manager.extension_providers();
    let owned = providers
        .iter()
        .find(|provider| provider.get("id").and_then(Value::as_str) == Some("owned"))
        .expect("owned provider");
    let forged = providers
        .iter()
        .find(|provider| provider.get("id").and_then(Value::as_str) == Some("forged"))
        .expect("forged provider");
    assert!(
        owned["virtualHttpServer"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert!(forged.get("virtualHttpServer").is_none());
}

#[test]
fn readable_from_web_iterator_break_cancels_virtual_response() {
    let result = run_case(
        "virtual_http_iterator_cancel",
        r#"
        const { Readable } = await import('node:stream');
        let closed = 0;
        const server = http.createServer((_req, res) => {
            res.on('close', () => { closed++; });
            res.write('first');
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const response = await fetch('http://127.0.0.1:28180/stream');
            const source = Readable.fromWeb(response.body);
            let first;
            for await (const chunk of source) { first = chunk.toString('utf8'); break; }
            return { result: { first, closed, destroyed: source.destroyed, listeners: ['data', 'error', 'end', 'close'].map(event => source.listenerCount(event)) } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(
        result,
        json!({"first": "first", "closed": 1, "destroyed": true, "listeners": [0, 0, 0, 0]})
    );
}

#[test]
fn virtual_http_response_timeout_cancels_pending_body() {
    let result = run_case(
        "virtual_http_response_timeout",
        r#"
        let timedOut = false;
        let closed = 0;
        const server = http.createServer((_req, res) => {
            res.on('close', () => { closed++; });
            res.setTimeout(10, () => { timedOut = true; });
            res.writeHead(200);
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const response = await fetch('http://127.0.0.1:28180/slow');
            try { await response.text(); return { result: { unexpected: true } }; }
            catch (error) { return { result: { code: error.code, timedOut, closed } }; }
        } finally { server.close(); }
        "#,
    );
    assert_eq!(
        result,
        json!({"code": "ETIMEDOUT", "timedOut": true, "closed": 1})
    );
}

#[test]
fn node_http_abort_before_headers_cancels_host_transport_once() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.closed = 0;
                import('node:http').then(http => {
                    globalThis.request = http.request('http://example.com/pending');
                    request.on('close', () => { closed++; });
                    request.on('error', () => {});
                    request.end();
                });
                ",
            )
            .await
            .expect("start request");
        runtime.drain_microtasks().await.expect("enqueue request");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].payload["stream"], true);
        assert!(runtime.is_hostcall_active(&requests[0].call_id));
        runtime
            .eval("request.abort(); request.abort();")
            .await
            .expect("abort request");
        runtime.drain_microtasks().await.expect("settle abort");
        assert!(!runtime.is_hostcall_active(&requests[0].call_id));
        assert_eq!(
            runtime
                .get_global_json("closed")
                .await
                .expect("close count"),
            1
        );
        assert!(runtime.drain_hostcall_requests().is_empty());
    });
}

#[test]
fn node_http_response_destroy_cancels_active_body_transport() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.closed = 0;
                globalThis.ended = false;
                import('node:http').then(http => {
                    http.get('http://example.com/stream', incoming => {
                        globalThis.response = incoming;
                        incoming.on('close', () => { closed++; });
                        incoming.on('end', () => { ended = true; });
                    }).on('error', () => {});
                });
                ",
            )
            .await
            .expect("start request");
        runtime.drain_microtasks().await.expect("enqueue request");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: json!({"status": 200, "headers": {}}),
                is_final: false,
            },
        );
        runtime.tick().await.expect("deliver response headers");
        runtime
            .eval("response.destroy(); response.destroy();")
            .await
            .expect("destroy response body");
        runtime
            .drain_microtasks()
            .await
            .expect("settle cancellation");
        assert!(!runtime.is_hostcall_active(&requests[0].call_id));
        assert_eq!(
            runtime
                .get_global_json("closed")
                .await
                .expect("close count"),
            1
        );
        assert_eq!(
            runtime.get_global_json("ended").await.expect("end state"),
            false
        );
    });
}

#[test]
fn node_http_timeout_reaches_host_and_cancels_transport() {
    futures::executor::block_on(async {
        let clock = Arc::new(DeterministicClock::new(0));
        let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.result = { timedOut: false, code: null, closed: 0 };
                import('node:http').then(http => {
                    const request = http.request('http://example.com/slow', { timeout: 25 });
                    request.on('timeout', () => { result.timedOut = true; });
                    request.on('error', error => { result.code = error.code; });
                    request.on('close', () => { result.closed++; });
                    request.end();
                });
                ",
            )
            .await
            .expect("start timed request");
        runtime.drain_microtasks().await.expect("enqueue request");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].payload["timeout"], 25);
        clock.advance(25);
        runtime.tick().await.expect("fire timeout");
        runtime.drain_microtasks().await.expect("settle timeout");
        assert!(!runtime.is_hostcall_active(&requests[0].call_id));
        assert_eq!(
            runtime
                .get_global_json("result")
                .await
                .expect("timeout result"),
            json!({"timedOut": true, "code": "ETIMEDOUT", "closed": 1})
        );
    });
}

#[test]
fn node_http_set_encoding_preserves_utf8_split_across_host_chunks() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.result = null;
                import('node:https').then(https => {
                    https.get('https://example.com/stream', response => {
                        response.setEncoding('utf8');
                        let text = '';
                        response.on('data', chunk => { text += chunk; });
                        response.on('end', () => { result = { text, complete: response.complete }; });
                    });
                });
                ",
            )
            .await
            .expect("start encoded response");
        runtime.drain_microtasks().await.expect("enqueue request");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].payload["url"], "https://example.com/stream");
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: json!({"status": 200, "headers": {}}),
                is_final: false,
            },
        );
        runtime.tick().await.expect("deliver headers");
        for (sequence, body_bytes) in [(1, "zw=="), (2, "gPA="), (3, "n5k="), (4, "gmjDqWxsbw==")] {
            runtime.complete_hostcall(
                &requests[0].call_id,
                HostcallOutcome::StreamChunk {
                    sequence,
                    chunk: json!({"body_bytes": body_bytes}),
                    is_final: false,
                },
            );
            runtime.tick().await.expect("deliver split UTF-8 bytes");
        }
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::StreamChunk {
                sequence: 5,
                chunk: Value::Null,
                is_final: true,
            },
        );
        runtime.tick().await.expect("finish response");
        runtime.drain_microtasks().await.expect("settle response");
        assert_eq!(
            runtime
                .get_global_json("result")
                .await
                .expect("decoded response"),
            json!({"text": "π🙂héllo", "complete": true})
        );
    });
}

#[test]
fn freeflow_daemon_spawn_options_fail_before_any_exec_hostcall() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.result = null;
                import('node:child_process').then(({ spawn }) => {
                    try {
                        spawn('C:\\Program Files\\nodejs\\node.exe', ['C:\\Users\\reporter\\freeflow\\daemon.ts'], {
                            detached: true, stdio: 'ignore', windowsHide: true,
                            env: { ...process.env, PI_FREEFLOW_DAEMON: '0' },
                        });
                        result = 'unexpected launch';
                    } catch (error) { result = error.message; }
                });
                ",
            )
            .await
            .expect("evaluate actual daemon options");
        runtime
            .drain_microtasks()
            .await
            .expect("settle spawn rejection");
        assert!(runtime.drain_hostcall_requests().is_empty());
        assert!(
            runtime
                .get_global_json("result")
                .await
                .expect("spawn rejection")
                .as_str()
                .is_some_and(|message| message.contains("unsupported option"))
        );
    });
}

#[test]
fn node_http_rejects_custom_tls_or_socket_authority_before_egress() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.result = [];
                import('node:https').then(https => {
                    for (const options of [{ rejectUnauthorized: false }, { ca: 'custom' }, { createConnection() {} }, { socketPath: '/tmp/private.sock' }]) {
                        const request = https.request('https://example.com/', options);
                        request.on('error', error => { result.push(error.message.includes('host networking')); });
                        request.end();
                    }
                });
                ",
            )
            .await
            .expect("evaluate unsupported transport overrides");
        runtime.drain_microtasks().await.expect("settle rejections");
        assert!(runtime.drain_hostcall_requests().is_empty());
        assert_eq!(
            runtime.get_global_json("result").await.expect("rejections"),
            json!([true, true, true, true])
        );
    });
}
