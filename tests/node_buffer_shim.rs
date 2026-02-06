//! Unit tests for the node:buffer (`Buffer`) shim (bd-1av0.6).
//!
//! Tests verify that `Buffer` follows Node.js semantics: `from`/`alloc`/`concat`
//! factory methods, encoding/decoding (utf8, base64, hex, latin1), `isBuffer`,
//! `byteLength`, `write`, `copy`, `compare`, `equals`, `indexOf`, `includes`,
//! `fill`, `toJSON`, `slice`, and integer read/write methods.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::sync::Arc;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn load_ext(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/buffer_test.mjs", source.as_bytes());
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

fn buffer_ext_source(js_expr: &str) -> String {
    format!(
        r#"
import {{ Buffer }} from "node:buffer";

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

fn eval_buffer(js_expr: &str) -> String {
    let harness = common::TestHarness::new("buffer_shim");
    let source = buffer_ext_source(js_expr);
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

// ─── Buffer.from + toString: UTF-8 ─────────────────────────────────────────

#[test]
fn from_string_utf8_roundtrip() {
    let result = eval_buffer(r#"Buffer.from("hello").toString()"#);
    assert_eq!(result, "hello");
}

#[test]
fn from_string_utf8_explicit() {
    let result = eval_buffer(r#"Buffer.from("hello", "utf8").toString("utf8")"#);
    assert_eq!(result, "hello");
}

// ─── Buffer.from + toString: base64 ────────────────────────────────────────

#[test]
fn from_string_base64_encode() {
    let result = eval_buffer(r#"Buffer.from("hello").toString("base64")"#);
    assert_eq!(result, "aGVsbG8=");
}

#[test]
fn from_base64_decode() {
    let result = eval_buffer(r#"Buffer.from("aGVsbG8=", "base64").toString("utf8")"#);
    assert_eq!(result, "hello");
}

// ─── Buffer.from + toString: hex ────────────────────────────────────────────

#[test]
fn from_string_hex_encode() {
    let result = eval_buffer(r#"Buffer.from("hello").toString("hex")"#);
    assert_eq!(result, "68656c6c6f");
}

#[test]
fn from_hex_decode() {
    let result = eval_buffer(r#"Buffer.from("68656c6c6f", "hex").toString("utf8")"#);
    assert_eq!(result, "hello");
}

// ─── Buffer.from + toString: latin1 ────────────────────────────────────────

#[test]
fn from_string_latin1_encode() {
    let result = eval_buffer(r#"Buffer.from("hello", "latin1").toString("latin1")"#);
    assert_eq!(result, "hello");
}

// ─── Buffer.from(array) ────────────────────────────────────────────────────

#[test]
fn from_array() {
    let result = eval_buffer(r"Buffer.from([104, 101, 108, 108, 111]).toString()");
    assert_eq!(result, "hello");
}

// ─── Buffer.alloc ──────────────────────────────────────────────────────────

#[test]
fn alloc_zero_filled() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.alloc(5);
        return buf.every(b => b === 0) && buf.length === 5;
    })()",
    );
    assert_eq!(result, "true");
}

#[test]
fn alloc_with_fill() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.alloc(3, 0x41);
        return buf.toString();
    })()",
    );
    assert_eq!(result, "AAA");
}

// ─── Buffer.isBuffer ───────────────────────────────────────────────────────

#[test]
fn is_buffer_true() {
    let result = eval_buffer(r"Buffer.isBuffer(Buffer.alloc(0))");
    assert_eq!(result, "true");
}

#[test]
fn is_buffer_false_for_uint8array() {
    let result = eval_buffer(r"Buffer.isBuffer(new Uint8Array(0))");
    assert_eq!(result, "false");
}

// ─── Buffer.byteLength ────────────────────────────────────────────────────

#[test]
fn byte_length_utf8() {
    let result = eval_buffer(r#"Buffer.byteLength("hello", "utf8")"#);
    assert_eq!(result, "5");
}

#[test]
fn byte_length_base64() {
    let result = eval_buffer(r#"Buffer.byteLength("aGVsbG8=", "base64")"#);
    assert_eq!(result, "5");
}

// ─── Buffer.concat ─────────────────────────────────────────────────────────

#[test]
fn concat_two_buffers() {
    let result = eval_buffer(
        r#"Buffer.concat([Buffer.from("hel"), Buffer.from("lo")]).toString()"#,
    );
    assert_eq!(result, "hello");
}

#[test]
fn concat_with_total_length() {
    let result = eval_buffer(
        r#"Buffer.concat([Buffer.from("hello"), Buffer.from("world")], 5).toString()"#,
    );
    assert_eq!(result, "hello");
}

// ─── buf.write ─────────────────────────────────────────────────────────────

#[test]
fn write_into_buffer() {
    let result = eval_buffer(
        r#"(() => {
        const buf = Buffer.alloc(5);
        buf.write("hi");
        return buf.toString();
    })()"#,
    );
    // "hi" + 3 null bytes renders as "hi\0\0\0" but toString utf8 stops at the nulls
    // Actually Node.js returns "hi\0\0\0" — let's test the first 2 bytes
    assert!(result.starts_with("hi"), "expected 'hi...' got: {result}");
}

// ─── buf.slice ─────────────────────────────────────────────────────────────

#[test]
fn slice_returns_buffer() {
    let result = eval_buffer(
        r#"(() => {
        const buf = Buffer.from("hello world");
        const sliced = buf.slice(0, 5);
        return Buffer.isBuffer(sliced) + ":" + sliced.toString();
    })()"#,
    );
    assert_eq!(result, "true:hello");
}

// ─── buf.copy ──────────────────────────────────────────────────────────────

#[test]
fn copy_between_buffers() {
    let result = eval_buffer(
        r#"(() => {
        const src = Buffer.from("hello");
        const dst = Buffer.alloc(5);
        src.copy(dst);
        return dst.toString();
    })()"#,
    );
    assert_eq!(result, "hello");
}

// ─── buf.compare / buf.equals ──────────────────────────────────────────────

#[test]
fn compare_equal() {
    let result = eval_buffer(
        r#"Buffer.from("abc").compare(Buffer.from("abc"))"#,
    );
    assert_eq!(result, "0");
}

#[test]
fn compare_less() {
    let result = eval_buffer(
        r#"Buffer.from("abc").compare(Buffer.from("abd"))"#,
    );
    assert_eq!(result, "-1");
}

#[test]
fn equals_true() {
    let result = eval_buffer(
        r#"Buffer.from("hello").equals(Buffer.from("hello"))"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn equals_false() {
    let result = eval_buffer(
        r#"Buffer.from("hello").equals(Buffer.from("world"))"#,
    );
    assert_eq!(result, "false");
}

// ─── buf.indexOf / buf.includes ────────────────────────────────────────────

#[test]
fn index_of_byte() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.from([1, 2, 3, 4, 5]);
        return buf.indexOf(3);
    })()",
    );
    assert_eq!(result, "2");
}

#[test]
fn index_of_string() {
    let result = eval_buffer(
        r#"Buffer.from("hello world").indexOf("world")"#,
    );
    assert_eq!(result, "6");
}

#[test]
fn includes_true() {
    let result = eval_buffer(
        r#"Buffer.from("hello world").includes("world")"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn includes_false() {
    let result = eval_buffer(
        r#"Buffer.from("hello").includes("xyz")"#,
    );
    assert_eq!(result, "false");
}

// ─── buf.fill ──────────────────────────────────────────────────────────────

#[test]
fn fill_with_byte() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.alloc(3);
        buf.fill(65);
        return buf.toString();
    })()",
    );
    assert_eq!(result, "AAA");
}

#[test]
fn fill_with_string() {
    let result = eval_buffer(
        r#"(() => {
        const buf = Buffer.alloc(6);
        buf.fill("ab");
        return buf.toString();
    })()"#,
    );
    assert_eq!(result, "ababab");
}

// ─── buf.toJSON ────────────────────────────────────────────────────────────

#[test]
fn to_json_format() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.from([1, 2, 3]);
        const json = buf.toJSON();
        return json.type + ':' + JSON.stringify(json.data);
    })()",
    );
    assert_eq!(result, "Buffer:[1,2,3]");
}

// ─── Integer read/write ────────────────────────────────────────────────────

#[test]
fn read_write_uint8() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.alloc(1);
        buf.writeUInt8(42, 0);
        return buf.readUInt8(0);
    })()",
    );
    assert_eq!(result, "42");
}

#[test]
fn read_write_uint16_le() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.alloc(2);
        buf.writeUInt16LE(0x0102, 0);
        return buf.readUInt16LE(0);
    })()",
    );
    assert_eq!(result, "258");
}

#[test]
fn read_write_uint32_be() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.alloc(4);
        buf.writeUInt32BE(0x01020304, 0);
        return buf.readUInt32BE(0);
    })()",
    );
    assert_eq!(result, "16909060");
}

// ─── Buffer.isEncoding ─────────────────────────────────────────────────────

#[test]
fn is_encoding_valid() {
    let result = eval_buffer(
        r#"[Buffer.isEncoding("utf8"), Buffer.isEncoding("hex"), Buffer.isEncoding("base64")].join(",")"#,
    );
    assert_eq!(result, "true,true,true");
}

#[test]
fn is_encoding_invalid() {
    let result = eval_buffer(r#"Buffer.isEncoding("foobar")"#);
    assert_eq!(result, "false");
}

// ─── Import styles ─────────────────────────────────────────────────────────

#[test]
fn default_import_works() {
    let harness = common::TestHarness::new("buffer_default_import");
    let source = r#"
import buffer from "node:buffer";
const { Buffer } = buffer;

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    return { result: Buffer.from("test").toString("hex") };
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
    assert_eq!(result, "74657374");
}

#[test]
fn bare_buffer_import_works() {
    let harness = common::TestHarness::new("buffer_bare_import");
    let source = r#"
import { Buffer } from "buffer";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    return { result: Buffer.from("hi").toString("base64") };
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
    assert_eq!(result, "aGk=");
}

// ─── Global Buffer availability ────────────────────────────────────────────

#[test]
fn global_buffer_available() {
    let result = eval_buffer(r"typeof globalThis.Buffer === 'function'");
    assert_eq!(result, "true");
}

// ─── Edge cases ────────────────────────────────────────────────────────────

#[test]
fn empty_buffer() {
    let result = eval_buffer(
        r#"(() => {
        const buf = Buffer.alloc(0);
        return buf.length + ":" + buf.toString();
    })()"#,
    );
    assert_eq!(result, "0:");
}

#[test]
fn allocunsafe_returns_buffer() {
    let result = eval_buffer(
        r"(() => {
        const buf = Buffer.allocUnsafe(10);
        return Buffer.isBuffer(buf) && buf.length === 10;
    })()",
    );
    assert_eq!(result, "true");
}

#[test]
fn static_compare() {
    let result = eval_buffer(
        r#"Buffer.compare(Buffer.from("a"), Buffer.from("b"))"#,
    );
    assert_eq!(result, "-1");
}
