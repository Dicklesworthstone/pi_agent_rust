//! Unit tests for the node:http and node:https shims (bd-1av0.8).
//!
//! Tests verify that `http.request`, `http.get`, `https.request`, `https.get`
//! return `ClientRequest` objects with the correct API surface (`write`, `end`,
//! `on`, `abort`, `destroy`), that `STATUS_CODES` and `METHODS` are exported,
//! and that `createServer` throws as expected. Network tests verify error
//! handling when no `pi.http()` hostcall is available.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::{PiJsRuntime, PiJsRuntimeConfig};
use pi::scheduler::{DeterministicClock, HostcallOutcome};
use pi::tools::ToolRegistry;
use std::sync::Arc;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn load_ext(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/http_test.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_entry_path).expect("load spec");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let js_config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(js_config, tools, manager)
                .await
                .expect("start js runtime")
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

fn http_ext_source(js_expr: &str) -> String {
    format!(
        r#"
import http from "node:http";

export default function activate(pi) {{
  pi.on("agent_start", (event, ctx) => {{
    let result;
    try {{
      result = String({js_expr});
    }} catch (e) {{
      result = "ERROR:" + e.message;
    }}
    return {{ result }};
  }});
}}
"#
    )
}

fn eval_http(js_expr: &str) -> String {
    let harness = common::TestHarness::new("http_shim");
    let source = http_ext_source(js_expr);
    let mgr = load_ext(&harness, &source);

    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch agent_start")
    });

    response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string())
}

// ─── STATUS_CODES export ────────────────────────────────────────────────────

#[test]
fn status_codes_exported() {
    let result = eval_http(r"http.STATUS_CODES[200]");
    assert_eq!(result, "OK");
}

#[test]
fn status_codes_404() {
    let result = eval_http(r"http.STATUS_CODES[404]");
    assert_eq!(result, "Not Found");
}

// ─── METHODS export ─────────────────────────────────────────────────────────

#[test]
fn methods_includes_standard() {
    let result = eval_http(
        r"http.METHODS.includes('GET') && http.METHODS.includes('POST') && http.METHODS.includes('PUT')",
    );
    assert_eq!(result, "true");
}

// ─── createServer throws ────────────────────────────────────────────────────

#[test]
fn create_server_throws() {
    let result = eval_http(r"http.createServer()");
    assert!(
        result.contains("ERROR:"),
        "createServer should throw, got: {result}"
    );
    assert!(result.contains("not available"), "got: {result}");
}

// ─── request returns ClientRequest ──────────────────────────────────────────

#[test]
fn request_returns_object_with_write() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', path: '/' });
        return typeof req.write === 'function';
    })()",
    );
    assert_eq!(result, "true");
}

#[test]
fn request_returns_object_with_end() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', path: '/' });
        return typeof req.end === 'function';
    })()",
    );
    assert_eq!(result, "true");
}

#[test]
fn request_returns_object_with_on() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', path: '/' });
        return typeof req.on === 'function';
    })()",
    );
    assert_eq!(result, "true");
}

#[test]
fn request_returns_object_with_abort() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', path: '/' });
        return typeof req.abort === 'function';
    })()",
    );
    assert_eq!(result, "true");
}

#[test]
fn request_returns_object_with_destroy() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', path: '/' });
        return typeof req.destroy === 'function';
    })()",
    );
    assert_eq!(result, "true");
}

// ─── get auto-ends request ──────────────────────────────────────────────────

#[test]
fn get_auto_ends() {
    let result = eval_http(
        r"(() => {
        const req = http.get({ hostname: 'example.com', path: '/' });
        return req._ended;
    })()",
    );
    assert_eq!(result, "true");
}

// ─── request method ─────────────────────────────────────────────────────────

#[test]
fn request_method_defaults_to_get() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', path: '/' });
        return req.method;
    })()",
    );
    assert_eq!(result, "GET");
}

#[test]
fn request_method_can_be_set() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', method: 'POST', path: '/' });
        return req.method;
    })()",
    );
    assert_eq!(result, "POST");
}

// ─── request path ───────────────────────────────────────────────────────────

#[test]
fn request_path_from_options() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', path: '/api/v1' });
        return req.path;
    })()",
    );
    assert_eq!(result, "/api/v1");
}

// ─── ClientRequest.write accumulates body ───────────────────────────────────

#[test]
fn write_accumulates_body() {
    let result = eval_http(
        r"(() => {
        const req = http.request({ hostname: 'example.com', path: '/' });
        req.write('part1');
        req.write('part2');
        return req._body.join('');
    })()",
    );
    assert_eq!(result, "part1part2");
}

// ─── Import styles ──────────────────────────────────────────────────────────

#[test]
fn named_import_works() {
    let harness = common::TestHarness::new("http_named_import");
    let source = r#"
import { request, STATUS_CODES } from "node:http";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    return { result: typeof request + ":" + STATUS_CODES[200] };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "function:OK");
}

#[test]
fn bare_http_import_works() {
    let harness = common::TestHarness::new("http_bare_import");
    let source = r#"
import http from "http";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    return { result: typeof http.request };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "function");
}

// ─── HTTPS module ───────────────────────────────────────────────────────────

#[test]
fn https_request_exists() {
    let harness = common::TestHarness::new("https_import");
    let source = r#"
import https from "node:https";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    return { result: typeof https.request + ":" + typeof https.get };
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "function:function");
}

#[test]
fn https_create_server_throws() {
    let harness = common::TestHarness::new("https_server");
    let source = r#"
import https from "node:https";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    try { https.createServer(); return { result: "no-throw" }; }
    catch(e) { return { result: "threw:" + e.message }; }
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert!(
        result.starts_with("threw:"),
        "createServer should throw, got: {result}"
    );
}

// ─── Mocked pi.http tests ───────────────────────────────────────────────────

/// Create an extension that overrides `pi.http` with a mock and runs an async
/// test that exercises the full request → response path.
fn http_mock_ext_source(mock_js: &str, test_js: &str) -> String {
    format!(
        r#"
import http from "node:http";

export default function activate(pi) {{
  // Override globalThis.pi.http with a controllable mock
  const __calls = [];
  const __origHttp = globalThis.pi.http;
  globalThis.pi.http = (req) => {{
    __calls.push(JSON.parse(JSON.stringify(req)));
    {mock_js}
  }};
  // Expose calls for inspection
  globalThis.__httpCalls = __calls;

  pi.on("agent_start", (event, ctx) => {{
    return new Promise((resolve, reject) => {{
      try {{
        {test_js}
      }} catch (e) {{
        resolve({{ result: "ERROR:" + e.message }});
      }}
    }});
  }});
}}
"#
    )
}

fn eval_http_mock(mock_js: &str, test_js: &str) -> String {
    let harness = common::TestHarness::new("http_mock");
    let source = http_mock_ext_source(mock_js, test_js);
    let mgr = load_ext(&harness, &source);

    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch agent_start")
    });

    response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string())
}

// ─── GET request with successful response ───────────────────────────────────

#[test]
fn get_receives_response_body() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: "hello world" });"#,
        r#"
        http.get("http://example.com/test", (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        "#,
    );
    assert_eq!(result, "hello world");
}

#[test]
fn get_receives_status_code() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 201, headers: {}, body: "" });"#,
        r#"
        http.get("http://example.com/api", (res) => {
            resolve({ result: String(res.statusCode) });
        });
        "#,
    );
    assert_eq!(result, "201");
}

#[test]
fn get_receives_response_headers() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: { "content-type": "application/json", "x-custom": "test" }, body: "{}" });"#,
        r#"
        http.get("http://example.com/api", (res) => {
            resolve({ result: res.headers["content-type"] + "|" + res.headers["x-custom"] });
        });
        "#,
    );
    assert_eq!(result, "application/json|test");
}

#[test]
fn get_receives_status_message() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 404, headers: {}, body: "" });"#,
        r#"
        http.get("http://example.com/missing", (res) => {
            resolve({ result: res.statusMessage });
        });
        "#,
    );
    assert_eq!(result, "Not Found");
}

// ─── POST request with body ─────────────────────────────────────────────────

#[test]
fn post_sends_body_via_hostcall() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: req.body || "NO_BODY" });"#,
        r#"
        const req = http.request({
            hostname: 'example.com',
            path: '/api',
            method: 'POST',
            headers: { 'Content-Type': 'application/json' }
        }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        req.write('{"key":"value"}');
        req.end();
        "#,
    );
    assert_eq!(result, r#"{"key":"value"}"#);
}

#[test]
fn post_multiple_writes_joined() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: req.body || "" });"#,
        r"
        const req = http.request({
            hostname: 'example.com',
            path: '/upload',
            method: 'POST'
        }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        req.write('part1');
        req.write('part2');
        req.write('part3');
        req.end();
        ",
    );
    assert_eq!(result, "part1part2part3");
}

#[test]
fn post_buffer_write_sends_body_bytes_via_hostcall() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: req.body_bytes || "" });"#,
        r"
        const req = http.request({
            hostname: 'example.com',
            path: '/upload',
            method: 'POST'
        }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        req.write(Buffer.from([0, 255, 65]));
        req.end();
        ",
    );
    assert_eq!(result, "AP9B");
}

#[test]
fn post_typed_array_write_sends_body_bytes_via_hostcall() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: req.body_bytes || "" });"#,
        r"
        const req = http.request({
            hostname: 'example.com',
            path: '/upload',
            method: 'POST'
        }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        req.write(new Uint8Array([104, 105]));
        req.end();
        ",
    );
    assert_eq!(result, "aGk=");
}

// ─── URL construction ───────────────────────────────────────────────────────

#[test]
fn request_constructs_url_from_options() {
    let result = eval_http_mock(
        r"return Promise.resolve({ status: 200, headers: {}, body: req.url });",
        r"
        http.get({ hostname: 'api.example.com', port: 8080, path: '/v1/data?q=test' }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        ",
    );
    assert_eq!(result, "http://api.example.com:8080/v1/data?q=test");
}

#[test]
fn request_parses_string_url() {
    let result = eval_http_mock(
        r"return Promise.resolve({ status: 200, headers: {}, body: req.url });",
        r#"
        http.get("http://example.com/path?key=val", (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        "#,
    );
    assert_eq!(result, "http://example.com/path?key=val");
}

// ─── Method and headers ─────────────────────────────────────────────────────

#[test]
fn request_sends_correct_method() {
    let result = eval_http_mock(
        r"return Promise.resolve({ status: 200, headers: {}, body: req.method });",
        r"
        const req = http.request({ hostname: 'example.com', path: '/', method: 'PUT' }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        req.end();
        ",
    );
    assert_eq!(result, "PUT");
}

#[test]
fn request_lowercases_headers() {
    let result = eval_http_mock(
        r"return Promise.resolve({ status: 200, headers: {}, body: JSON.stringify(req.headers) });",
        r"
        const req = http.request({
            hostname: 'example.com',
            path: '/',
            headers: { 'Content-Type': 'text/plain', 'X-Custom-Header': 'value' }
        }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        req.end();
        ",
    );
    let headers: serde_json::Value = serde_json::from_str(&result).expect("parse headers JSON");
    assert_eq!(headers["content-type"], "text/plain");
    assert_eq!(headers["x-custom-header"], "value");
}

#[test]
fn request_header_mutators_update_hostcall_headers() {
    let result = eval_http_mock(
        r"return Promise.resolve({ status: 200, headers: {}, body: JSON.stringify(req.headers) });",
        r"
        const req = http.request({
            hostname: 'example.com',
            path: '/',
            headers: { 'X-Initial': 'seed' }
        }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({
                    result: JSON.stringify({
                        initial: req.getHeader('X-Initial'),
                        afterSet,
                        afterRemove: String(req.getHeader('x-custom')),
                        sent: JSON.parse(body),
                    }),
                });
            });
        });
        req.setHeader('Content-Type', 'application/json');
        req.setHeader('X-Custom', 'value');
        const afterSet = req.getHeader('content-type') + '|' + req.getHeader('x-custom');
        req.removeHeader('X-Custom');
        req.end();
        ",
    );
    let payload: serde_json::Value = serde_json::from_str(&result).expect("parse result JSON");
    assert_eq!(payload["initial"], "seed");
    assert_eq!(payload["afterSet"], "application/json|value");
    assert_eq!(payload["afterRemove"], "undefined");
    assert_eq!(payload["sent"]["x-initial"], "seed");
    assert_eq!(payload["sent"]["content-type"], "application/json");
    assert!(payload["sent"].get("x-custom").is_none());
}

// ─── Error handling ─────────────────────────────────────────────────────────

#[test]
fn request_emits_error_on_rejection() {
    let result = eval_http_mock(
        r#"return Promise.reject("connection refused");"#,
        r"
        const req = http.request({ hostname: 'example.com', path: '/' });
        req.on('error', (err) => {
            resolve({ result: err.message });
        });
        req.end();
        ",
    );
    assert_eq!(result, "connection refused");
}

#[test]
fn request_emits_error_on_invalid_response() {
    let result = eval_http_mock(
        r#"return Promise.resolve("not an object");"#,
        r"
        const req = http.request({ hostname: 'example.com', path: '/' });
        req.on('error', (err) => {
            resolve({ result: err.message });
        });
        req.end();
        ",
    );
    assert!(
        result.contains("Invalid"),
        "expected invalid response error, got: {result}"
    );
}

// ─── IncomingMessage events ─────────────────────────────────────────────────

#[test]
fn response_emits_end_after_data() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: "chunk1" });"#,
        r#"
        const events = [];
        http.get("http://example.com/", (res) => {
            res.on('data', () => events.push('data'));
            res.on('end', () => {
                events.push('end');
                resolve({ result: events.join(',') });
            });
        });
        "#,
    );
    assert_eq!(result, "data,end");
}

#[test]
fn response_sets_complete_after_end() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: "done" });"#,
        r#"
        http.get("http://example.com/", (res) => {
            res.on('end', () => {
                resolve({ result: String(res.complete) });
            });
        });
        "#,
    );
    assert_eq!(result, "true");
}

#[test]
fn response_empty_body_emits_end_without_data() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 204, headers: {}, body: "" });"#,
        r#"
        const events = [];
        http.get("http://example.com/", (res) => {
            res.on('data', () => events.push('data'));
            res.on('end', () => {
                events.push('end');
                resolve({ result: events.join(',') });
            });
        });
        "#,
    );
    assert_eq!(result, "end");
}

#[test]
fn response_destroy_before_body_suppresses_data_and_end() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: "late" });"#,
        r#"
        const events = [];
        http.get("http://example.com/", (res) => {
            events.push('response');
            res.on('data', () => events.push('data'));
            res.on('end', () => events.push('end'));
            res.on('close', () => {
                events.push('close');
                Promise.resolve().then(() => resolve({ result: events.join(',') }));
            });
            res.destroy();
        });
        "#,
    );
    assert_eq!(result, "response,close");
}

#[test]
fn response_destroy_during_data_suppresses_end() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: "late" });"#,
        r#"
        const events = [];
        http.get("http://example.com/", (res) => {
            res.on('data', () => {
                events.push('data');
                res.destroy();
            });
            res.on('end', () => events.push('end'));
            res.on('close', () => {
                events.push('close');
                Promise.resolve().then(() => resolve({ result: events.join(',') }));
            });
        });
        "#,
    );
    assert_eq!(result, "data,close");
}

#[test]
fn response_body_bytes_emits_binary_chunk() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body_bytes: "AP9B" });"#,
        r#"
        http.get("http://example.com/binary", (res) => {
            let sawBuffer = false;
            let bytes = '';
            res.on('data', (chunk) => {
                sawBuffer = typeof Buffer !== 'undefined' && Buffer.isBuffer(chunk);
                bytes = Array.from(chunk).join(',');
            });
            res.on('end', () => {
                resolve({ result: String(sawBuffer) + "|" + bytes });
            });
        });
        "#,
    );
    assert_eq!(result, "true|0,255,65");
}

#[test]
fn response_set_encoding_decodes_body_bytes_to_text() {
    let result = eval_http_mock(
        r"return Promise.resolve({ status: 200, headers: {}, body_bytes: Buffer.from('héllo').toString('base64') });",
        r#"
        http.get("http://example.com/text", (res) => {
            res.setEncoding('utf8');
            let chunkType = '';
            let body = '';
            res.on('data', (chunk) => {
                chunkType = typeof chunk;
                body += chunk;
            });
            res.on('end', () => {
                resolve({ result: chunkType + "|" + body });
            });
        });
        "#,
    );
    assert_eq!(result, "string|héllo");
}

// ─── Timeout option ─────────────────────────────────────────────────────────

#[test]
fn request_sends_timeout_to_hostcall() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: String(req.timeout || "none") });"#,
        r"
        const req = http.request({ hostname: 'example.com', path: '/' }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        req.setTimeout(5000);
        req.end();
        ",
    );
    assert_eq!(result, "5000");
}

// ─── HTTPS forces protocol ──────────────────────────────────────────────────

#[test]
fn https_request_uses_https_protocol() {
    let harness = common::TestHarness::new("https_protocol");
    let source = r#"
import https from "node:https";

export default function activate(pi) {
  globalThis.pi.http = (req) => {
    return Promise.resolve({ status: 200, headers: {}, body: req.url });
  };

  pi.on("agent_start", (event, ctx) => {
    return new Promise((resolve) => {
      https.get("https://secure.example.com/api", (res) => {
        let body = '';
        res.on('data', (chunk) => { body += chunk; });
        res.on('end', () => {
          resolve({ result: body });
        });
      });
    });
  });
}
"#;
    let mgr = load_ext(&harness, source);
    let response = common::run_async(async move {
        mgr.dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });
    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert!(
        result.starts_with("https://"),
        "https should use https: protocol, got: {result}"
    );
}

// ─── Abort / destroy ────────────────────────────────────────────────────────

#[test]
fn abort_emits_abort_and_close_events() {
    let result = eval_http(
        r"(() => {
        const events = [];
        const req = http.request({ hostname: 'example.com', path: '/' });
        req.on('abort', () => events.push('abort'));
        req.on('close', () => events.push('close'));
        req.abort();
        return events.join(',');
    })()",
    );
    assert_eq!(result, "abort,close");
}

#[test]
fn destroy_with_error_emits_error_and_close() {
    let result = eval_http(
        r"(() => {
        const events = [];
        const req = http.request({ hostname: 'example.com', path: '/' });
        req.on('error', (e) => events.push('error:' + e.message));
        req.on('close', () => events.push('close'));
        req.destroy(new Error('test error'));
        return events.join(',');
    })()",
    );
    assert_eq!(result, "error:test error,close");
}

#[test]
fn abort_after_end_suppresses_late_response_events() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: "late" });"#,
        r"
        const events = [];
        const req = http.request({ hostname: 'example.com', path: '/' });
        req.on('response', (res) => {
            events.push('response');
            res.on('data', () => events.push('data'));
            res.on('end', () => events.push('end'));
        });
        req.on('abort', () => events.push('abort'));
        req.on('close', () => events.push('close'));
        req.end();
        req.abort();
        Promise.resolve().then(() => Promise.resolve().then(() => {
            resolve({ result: events.join(',') });
        }));
        ",
    );
    assert_eq!(result, "abort,close");
}

#[test]
fn abort_in_response_callback_suppresses_body_delivery() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: "late" });"#,
        r#"
        const events = [];
        let req;
        req = http.get("http://example.com/", (res) => {
            events.push('response');
            res.on('data', () => events.push('data'));
            res.on('end', () => events.push('end'));
            req.abort();
        });
        req.on('abort', () => events.push('abort'));
        req.on('close', () => {
            events.push('close');
            Promise.resolve().then(() => resolve({ result: events.join(',') }));
        });
        "#,
    );
    assert_eq!(result, "response,abort,close");
}

#[test]
fn destroy_after_end_suppresses_late_response_events() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: "late" });"#,
        r"
        const events = [];
        const req = http.request({ hostname: 'example.com', path: '/' });
        req.on('response', (res) => {
            events.push('response');
            res.on('data', () => events.push('data'));
            res.on('end', () => events.push('end'));
        });
        req.on('error', (err) => events.push('error:' + err.message));
        req.on('close', () => events.push('close'));
        req.end();
        req.destroy(new Error('boom'));
        Promise.resolve().then(() => Promise.resolve().then(() => {
            resolve({ result: events.join(',') });
        }));
        ",
    );
    assert_eq!(result, "error:boom,close");
}

// ─── Default status code ────────────────────────────────────────────────────

#[test]
fn response_defaults_to_200() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ headers: {}, body: "" });"#,
        r#"
        http.get("http://example.com/", (res) => {
            res.on('end', () => {
                resolve({ result: String(res.statusCode) });
            });
        });
        "#,
    );
    assert_eq!(result, "200");
}

// ─── Finish event on end ────────────────────────────────────────────────────

#[test]
fn request_emits_finish_on_end() {
    let result = eval_http(
        r"(() => {
        let finished = false;
        const req = http.request({ hostname: 'example.com', path: '/' });
        req.on('finish', () => { finished = true; });
        req.end();
        return String(finished);
    })()",
    );
    assert_eq!(result, "true");
}

// ─── end() with callback ────────────────────────────────────────────────────

#[test]
fn end_with_chunk_writes_before_send() {
    let result = eval_http_mock(
        r#"return Promise.resolve({ status: 200, headers: {}, body: req.body || "" });"#,
        r"
        const req = http.request({ hostname: 'example.com', path: '/', method: 'POST' }, (res) => {
            let body = '';
            res.on('data', (chunk) => { body += chunk; });
            res.on('end', () => {
                resolve({ result: body });
            });
        });
        req.write('before-');
        req.end('after');
        ",
    );
    assert_eq!(result, "before-after");
}

// Fetch uses the same capability-gated HTTP bridge as the Node client APIs.
// These deterministic tests deliberately complete real PiJS hostcall records;
// Promise rejection alone would not prove cancellation reached that bridge.

#[test]
fn fetch_abort_signal_composition_keeps_first_reason_and_ignores_synthetic_events() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                const first = new AbortController();
                const second = new AbortController();
                const combined = AbortSignal.any([first.signal, second.signal, first.signal]);
                let calls = 0;
                combined.addEventListener('abort', () => { throw new Error('listener failure'); });
                combined.addEventListener('abort', () => { calls++; }, { once: true });
                first.signal.dispatchEvent(new Event('abort'));
                const syntheticAborted = combined.aborted;
                const reason = new Error('first reason');
                first.abort(reason);
                second.abort('second reason');
                first.abort('third reason');
                let invalid = false;
                try { AbortSignal.any([AbortSignal.abort(), {}]); }
                catch (error) { invalid = error instanceof TypeError; }
                globalThis.result = {
                    syntheticAborted,
                    sameReason: combined.reason === reason && first.signal.reason === reason,
                    calls,
                    nullReason: AbortSignal.any([AbortSignal.abort(null), second.signal]).reason,
                    emptyAborted: AbortSignal.any([]).aborted,
                    invalid,
                    eventTarget: combined instanceof EventTarget,
                };
                ",
            )
            .await
            .expect("evaluate signals");
        assert_eq!(
            runtime.get_global_json("result").await.expect("result"),
            serde_json::json!({
                "syntheticAborted": false,
                "sameReason": true,
                "calls": 1,
                "nullReason": null,
                "emptyAborted": false,
                "invalid": true,
                "eventTarget": true,
            })
        );
    });
}

#[test]
fn fetch_preabort_and_unsupported_dispatcher_do_not_enqueue_http() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.result = [];
                (async () => {
                    const before = new AbortController();
                    before.abort('already cancelled');
                    try { await fetch('https://example.com/never', { signal: before.signal }); }
                    catch (error) { result.push(error); }
                    try { await fetch('https://example.com/never', { dispatcher: { dispatch() {} } }); }
                    catch (error) { result.push(error.name); }
                    const during = new AbortController();
                    const headers = { get test() { during.abort('normalization cancelled'); return 'value'; } };
                    try { await fetch('https://example.com/never', { signal: during.signal, headers }); }
                    catch (error) { result.push(error); }
                })();
                ",
            )
            .await
            .expect("evaluate fetch rejection");
        runtime.drain_microtasks().await.expect("settle rejections");
        assert!(runtime.drain_hostcall_requests().is_empty());
        assert_eq!(
            runtime.get_global_json("result").await.expect("result"),
            serde_json::json!(["already cancelled", "TypeError", "normalization cancelled"])
        );
    });
}

#[test]
fn fetch_resolves_at_headers_and_streams_binary_bytes_to_both_clones() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.status = null;
                globalThis.result = null;
                fetch('https://example.com/relay', {
                    method: 'POST',
                    headers: new Headers({ 'X-Relay-Target': 'https://upstream.example.com' }),
                    body: new Uint8Array([0, 255, 65]),
                }).then(async response => {
                    globalThis.status = response.status;
                    const copy = response.clone();
                    const bodies = await Promise.all([response.arrayBuffer(), copy.arrayBuffer()]);
                    globalThis.result = bodies.map(body => Array.from(new Uint8Array(body)));
                });
                ",
            )
            .await
            .expect("start fetch");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.payload["stream"], true);
        assert_eq!(request.payload["body_bytes"], "AP9B");
        assert!(request.payload.get("body").is_none());
        assert_eq!(
            request.payload["headers"]["x-relay-target"],
            "https://upstream.example.com"
        );
        runtime.complete_hostcall(
            &request.call_id,
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: serde_json::json!({ "status": 200, "headers": {} }),
                is_final: false,
            },
        );
        runtime.tick().await.expect("deliver headers");
        assert_eq!(
            runtime.get_global_json("status").await.expect("status"),
            200
        );
        assert_eq!(
            runtime
                .get_global_json("result")
                .await
                .expect("pending body"),
            serde_json::Value::Null
        );
        for (sequence, encoded) in [(1, "AP9B"), (2, "w6k=")] {
            runtime.complete_hostcall(
                &request.call_id,
                HostcallOutcome::StreamChunk {
                    sequence,
                    chunk: serde_json::json!({ "body_bytes": encoded }),
                    is_final: false,
                },
            );
            runtime.tick().await.expect("deliver bytes");
        }
        runtime.complete_hostcall(
            &request.call_id,
            HostcallOutcome::StreamChunk {
                sequence: 3,
                chunk: serde_json::Value::Null,
                is_final: true,
            },
        );
        runtime.tick().await.expect("finish stream");
        runtime
            .drain_microtasks()
            .await
            .expect("settle body readers");
        assert_eq!(
            runtime.get_global_json("result").await.expect("bodies"),
            serde_json::json!([[0, 255, 65, 195, 169], [0, 255, 65, 195, 169]])
        );
    });
}

#[test]
fn fetch_abort_before_headers_cancels_the_pending_hostcall() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.controller = new AbortController();
                globalThis.result = null;
                fetch('https://example.com/slow', { signal: controller.signal })
                    .then(() => { result = 'unexpected response'; }, error => { result = error.name; });
                ",
            )
            .await
            .expect("start fetch");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        assert!(runtime.is_hostcall_active(&requests[0].call_id));
        runtime
            .eval("controller.signal.dispatchEvent(new Event('abort'));")
            .await
            .expect("dispatch synthetic abort");
        assert!(runtime.is_hostcall_active(&requests[0].call_id));
        runtime
            .eval("controller.abort(); controller.abort();")
            .await
            .expect("abort");
        runtime.drain_microtasks().await.expect("settle abort");
        assert!(!runtime.is_hostcall_active(&requests[0].call_id));
        assert_eq!(
            runtime
                .get_global_json("result")
                .await
                .expect("abort result"),
            "AbortError"
        );
        assert!(runtime.drain_hostcall_requests().is_empty());
    });
}

#[test]
fn fetch_abort_after_headers_rejects_body_read_and_stops_transport() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.controller = new AbortController();
                globalThis.result = null;
                fetch('https://example.com/stream', { signal: controller.signal })
                    .then(response => response.text())
                    .then(() => { result = 'unexpected body'; }, error => { result = error.name; });
                ",
            )
            .await
            .expect("start fetch");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: serde_json::json!({ "status": 200, "headers": {} }),
                is_final: false,
            },
        );
        runtime.tick().await.expect("deliver headers");
        runtime
            .eval("controller.abort();")
            .await
            .expect("abort body");
        runtime.drain_microtasks().await.expect("settle body error");
        assert!(!runtime.is_hostcall_active(&requests[0].call_id));
        assert_eq!(
            runtime.get_global_json("result").await.expect("body error"),
            "AbortError"
        );
    });
}

#[test]
fn fetch_cancel_clones_keeps_transport_until_last_consumer_cancels() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.original = null;
                globalThis.copy = null;
                fetch('https://example.com/stream').then(response => {
                    original = response;
                    copy = response.clone();
                });
                ",
            )
            .await
            .expect("start fetch");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::StreamChunk {
                sequence: 0,
                chunk: serde_json::json!({ "status": 200, "headers": {} }),
                is_final: false,
            },
        );
        runtime.tick().await.expect("deliver headers");
        runtime
            .eval("copy.body.cancel();")
            .await
            .expect("cancel copy");
        runtime
            .drain_microtasks()
            .await
            .expect("settle copy cancellation");
        assert!(runtime.is_hostcall_active(&requests[0].call_id));
        runtime
            .eval("original.body.cancel();")
            .await
            .expect("cancel original");
        runtime
            .drain_microtasks()
            .await
            .expect("settle final cancellation");
        assert!(!runtime.is_hostcall_active(&requests[0].call_id));
    });
}

#[test]
fn fetch_timeout_signal_cancels_transport_and_preserves_caller_signal() {
    futures::executor::block_on(async {
        let clock = Arc::new(DeterministicClock::new(0));
        let runtime = PiJsRuntime::with_clock(Arc::clone(&clock))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.caller = new AbortController();
                globalThis.result = null;
                const signal = AbortSignal.any([caller.signal, AbortSignal.timeout(25)]);
                fetch('https://example.com/slow', { signal })
                    .catch(error => { result = { name: error.name, callerAborted: caller.signal.aborted }; });
                ",
            )
            .await
            .expect("start timed fetch");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        clock.advance(25);
        runtime.tick().await.expect("fire timeout");
        runtime.drain_microtasks().await.expect("settle timeout");
        assert!(!runtime.is_hostcall_active(&requests[0].call_id));
        assert_eq!(
            runtime
                .get_global_json("result")
                .await
                .expect("timeout result"),
            serde_json::json!({ "name": "TimeoutError", "callerAborted": false })
        );
    });
}

#[test]
fn fetch_preserves_http_policy_denial_without_an_alternate_transport() {
    futures::executor::block_on(async {
        let runtime = PiJsRuntime::with_clock(DeterministicClock::new(0))
            .await
            .expect("runtime");
        runtime
            .eval(
                r"
                globalThis.result = null;
                fetch('https://blocked.example.com/relay')
                    .catch(error => { result = { code: error.code, message: error.message }; });
                ",
            )
            .await
            .expect("start fetch");
        let requests = runtime.drain_hostcall_requests();
        assert_eq!(requests.len(), 1);
        runtime.complete_hostcall(
            &requests[0].call_id,
            HostcallOutcome::Error {
                code: "denied".to_string(),
                message: "HTTP host is denied".to_string(),
            },
        );
        runtime.tick().await.expect("deliver denial");
        assert_eq!(
            runtime.get_global_json("result").await.expect("denial"),
            serde_json::json!({ "code": "denied", "message": "HTTP host is denied" })
        );
        assert!(runtime.drain_hostcall_requests().is_empty());
    });
}
