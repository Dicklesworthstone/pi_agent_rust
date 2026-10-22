//! Request-body lifecycle through PiJS's owned virtual HTTP route (#270).
//! These tests never bind a socket or permit external HTTP. They complement
//! virtual_http_server's provider ownership and response-stream coverage.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use serde_json::{Value, json};
use std::sync::Arc;

fn run_case(name: &str, body: &str) -> Value {
    let harness = common::TestHarness::new(name);
    let cwd = harness.temp_dir().to_path_buf();
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
    let entry = harness.create_file("installed extension/request.mjs", source.as_bytes());
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
    common::run_async(async move {
        manager
            .load_js_extensions(vec![spec])
            .await
            .expect("load extension");
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10_000)
            .await
            .expect("bounded event dispatch")
            .and_then(|value| value.get("result").cloned())
            .expect("result")
    })
}

#[test]
fn virtual_request_body_honors_pause_and_resume_with_bounded_binary_chunks() {
    let result = run_case(
        "virtual_request_pause",
        r#"
        let incoming, firstChunk;
        const first = new Promise(resolve => { firstChunk = resolve; });
        const chunks = [], sizes = [];
        let ends = 0;
        const server = http.createServer((req, res) => {
            incoming = req;
            req.pause();
            req.on('data', chunk => {
                chunks.push(chunk); sizes.push(chunk.length);
                if (chunks.length === 1) { req.pause(); firstChunk(); }
            });
            req.on('end', () => { ends++; res.end(Buffer.concat(chunks)); });
            res.writeHead(200, { 'content-type': 'application/octet-stream' });
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const bytes = new Uint8Array(40003);
            for (let i = 0; i < bytes.length; i++) bytes[i] = i % 256;
            const response = await fetch('http://127.0.0.1:28180/v1/chat/completions', {
                method: 'POST', body: bytes,
            });
            const before = { chunks: chunks.length, complete: incoming.complete, paused: incoming.isPaused() };
            incoming.resume(); incoming.resume();
            await first;
            const afterFirst = { chunks: chunks.length, complete: incoming.complete, paused: incoming.isPaused() };
            incoming.resume(); incoming.resume();
            const received = new Uint8Array(await response.arrayBuffer());
            return { result: { before, afterFirst, sizes, ends, complete: incoming.complete,
                intact: received.length === bytes.length && received.every((v, i) => v === bytes[i]) } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(
        result,
        json!({
            "before": {"chunks": 0, "complete": false, "paused": true},
            "afterFirst": {"chunks": 1, "complete": false, "paused": true},
            "sizes": [16384, 16384, 7235], "ends": 1, "complete": true, "intact": true,
        })
    );
}

#[test]
fn virtual_request_destroy_in_data_never_starts_end_handler_work() {
    let result = run_case(
        "virtual_request_destroy",
        r#"
        let incoming, ends = 0, aborted = 0, closed = 0, responseClosed = 0, socketClosed = 0;
        const server = http.createServer((req, res) => {
            incoming = req;
            req.on('aborted', () => { aborted++; });
            req.on('close', () => { throw new Error('cleanup observer'); });
            req.once('close', () => { closed++; });
            req.socket.on('close', () => { socketClosed++; });
            res.on('close', () => { responseClosed++; });
            req.on('data', () => req.destroy(new Error('fixture upload rejected')));
            req.on('end', () => { ends++; res.end('must not launch upstream work'); });
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            let rejected = false;
            try { await fetch('http://127.0.0.1:28180/reject', { method: 'POST', body: 'stop' }); }
            catch (error) { rejected = error.message.includes('fixture upload rejected'); }
            incoming.destroy(); incoming.resume();
            await Promise.resolve();
            return { result: { rejected, ends, aborted, closed, responseClosed, socketClosed,
                complete: incoming.complete, remainingCloseListeners: incoming.listenerCount('close') } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(
        result,
        json!({
            "rejected": true, "ends": 0, "aborted": 1, "closed": 1,
            "responseClosed": 1, "socketClosed": 1, "complete": false,
            "remainingCloseListeners": 1,
        })
    );
}

#[test]
fn virtual_request_callback_exceptions_are_terminal_and_release_slots() {
    for event in ["data", "end"] {
        let body = r#"
        let failures = 0, closed = 0, aborted = 0, ends = 0;
        const server = http.createServer((req, res) => {
            req.on('aborted', () => { aborted++; });
            req.on('data', () => {});
            req.on('EVENT', () => { throw new Error('fixture callback failed'); });
            req.on('end', () => { ends++; });
            res.on('close', () => { closed++; });
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            // Exceed the realm's 64 active-request ceiling cumulatively. Every
            // failed transaction must return its slot before the next request.
            for (let i = 0; i < 70; i++) {
                try { await fetch('http://127.0.0.1:28180/fail', { method: 'POST', body: 'body' }); }
                catch (error) { if (error.message.includes('fixture callback failed')) failures++; }
            }
            return { result: { failures, closed, aborted, ends } };
        } finally { server.close(); }
        "#
        .replace("'EVENT'", &format!("'{event}'"));
        let result = run_case(&format!("virtual_request_throw_{event}"), &body);
        assert_eq!(
            result,
            json!({
                "failures": 70, "closed": 70,
                "aborted": if event == "data" { 70 } else { 0 }, "ends": 0,
            }),
            "{event} callback must terminate the request, not stall the native provider"
        );
    }
}

#[test]
fn virtual_request_early_response_does_not_emit_end_after_rejection() {
    let result = run_case(
        "virtual_request_early_response",
        r#"
        let chunks = 0, ends = 0, incoming;
        const server = http.createServer((req, res) => {
            incoming = req;
            req.on('data', () => { chunks++; res.writeHead(413); res.end('rejected'); });
            req.on('end', () => { ends++; });
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const response = await fetch('http://127.0.0.1:28180/reject', {
                method: 'POST', body: new Uint8Array(40003),
            });
            const body = await response.text();
            return { result: { status: response.status, body, chunks, ends, complete: incoming.complete } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(
        result,
        json!({"status": 413, "body": "rejected", "chunks": 1, "ends": 0, "complete": false})
    );
}

#[test]
fn virtual_request_body_cancellation_stops_a_paused_upload() {
    let result = run_case(
        "virtual_request_cancel_paused",
        r#"
        let incoming, chunks = 0, ends = 0, closed = 0;
        const server = http.createServer((req, res) => {
            incoming = req; req.pause();
            req.on('data', () => { chunks++; }); req.on('end', () => { ends++; });
            res.on('close', () => { closed++; });
            res.writeHead(200);
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const response = await fetch('http://127.0.0.1:28180/paused', { method: 'POST', body: 'body' });
            await response.body.cancel();
            incoming.resume();
            await Promise.resolve();
            return { result: { chunks, ends, closed, complete: incoming.complete,
                aborted: incoming.aborted, destroyed: incoming.destroyed } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(
        result,
        json!({"chunks": 0, "ends": 0, "closed": 1, "complete": false, "aborted": true, "destroyed": true})
    );
}

#[test]
fn virtual_request_body_decoder_preserves_split_utf8_and_terminal_tail() {
    let result = run_case(
        "virtual_request_utf8",
        r#"
        let seen = '', types = true, ends = 0;
        const bytes = Buffer.concat([Buffer.alloc(16383, 97), Buffer.from('☃🎼é'), Buffer.from([0xe2, 0x82])]);
        const server = http.createServer((req, res) => {
            req.setEncoding('utf8');
            req.on('data', chunk => { types = types && typeof chunk === 'string'; seen += chunk; });
            req.on('end', () => { ends++; res.end(seen); });
        });
        await new Promise(resolve => server.listen(28180, '127.0.0.1', resolve));
        try {
            const response = await fetch('http://127.0.0.1:28180/text', { method: 'POST', body: bytes });
            const text = await response.text();
            return { result: { intact: text === bytes.toString('utf8'), tail: text.slice(-6), types, ends } };
        } finally { server.close(); }
        "#,
    );
    assert_eq!(result["intact"], true);
    assert_eq!(result["types"], true);
    assert_eq!(result["ends"], 1);
    assert!(result["tail"].as_str().expect("tail").ends_with("☃🎼é�"));
}
