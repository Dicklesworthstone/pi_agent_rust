//! Unit tests for the node:buffer (`Buffer`) shim (bd-1av0.6).
//!
//! Tests verify that `Buffer` follows Node.js semantics: `from`/`alloc`/`concat`
//! factory methods, encoding/decoding (utf8, base64, hex, latin1), `isBuffer`,
//! `byteLength`, `write`, `copy`, `compare`, `equals`, `indexOf`, `lastIndexOf`,
//! `includes`, `fill`, `toJSON`, `slice`, byte swaps, and integer read/write methods.

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

fn global_buffer_ext_source(js_expr: &str) -> String {
    format!(
        r#"
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

fn eval_global_buffer(js_expr: &str) -> String {
    let harness = common::TestHarness::new("global_buffer_shim");
    let source = global_buffer_ext_source(js_expr);
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

#[test]
fn from_hex_truncates_at_invalid_or_incomplete_pair_like_node() {
    let result = eval_buffer(
        r#"(() => {
        const cases = ["1ag123", "1a7", "zz", "61 62", "61xz62", "0"];
        return cases.map((input) => [
            input,
            Buffer.from(input, "hex").toString("hex"),
            Buffer.byteLength(input, "hex"),
        ].join(":")).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "1ag123:1a:3|1a7:1a:1|zz::1|61 62:61:2|61xz62:61:3|0::0"
    );
}

// ─── Buffer.from + toString: latin1 ────────────────────────────────────────

#[test]
fn from_string_latin1_encode() {
    let result = eval_buffer(r#"Buffer.from("hello", "latin1").toString("latin1")"#);
    assert_eq!(result, "hello");
}

#[test]
fn single_byte_encodings_match_node_vectors() {
    let result = eval_buffer(
        r#"(() => {
        const latin1 = Buffer.from("\u00ffA", "latin1");
        const binary = Buffer.from("\u00ffA", "binary");
        const ascii = Buffer.from("\u00ffA", "ascii");
        const latin1Codes = Array.from(latin1.toString("latin1")).map((ch) => ch.charCodeAt(0).toString(16)).join(",");
        const binaryCodes = Array.from(binary.toString("binary")).map((ch) => ch.charCodeAt(0).toString(16)).join(",");
        const asciiCodes = Array.from(ascii.toString("ascii")).map((ch) => ch.charCodeAt(0).toString(16)).join(",");
        return [
            latin1.toString("hex"),
            binary.toString("hex"),
            ascii.toString("hex"),
            latin1Codes,
            binaryCodes,
            asciiCodes,
            Buffer.byteLength("\u00ffA", "latin1"),
            Buffer.byteLength("\u00ffA", "binary"),
            Buffer.byteLength("\u00ffA", "ascii"),
            Buffer.byteLength("\u00ffA", "utf8"),
        ].join("|");
    })()"#,
    );
    assert_eq!(result, "ff41|ff41|ff41|ff,41|ff,41|7f,41|2|2|2|3");
}

#[test]
fn utf16le_alias_encodings_match_node_vectors() {
    let result = eval_buffer(
        r#"(() => {
        const aliases = ["utf16le", "utf-16le", "ucs2", "ucs-2"];
        return aliases.map((enc) => {
            const from = Buffer.from("A\u2603", enc);
            const written = Buffer.alloc(4);
            const bytesWritten = written.write("A\u2603", 0, 4, enc);
            const codes = from.toString(enc).split("").map((ch) => ch.charCodeAt(0).toString(16)).join(",");
            return [enc, from.toString("hex"), codes, Buffer.byteLength("A\u2603", enc), Buffer.isEncoding(enc), bytesWritten, written.toString("hex")].join(":");
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "utf16le:41000326:41,2603:4:true:4:41000326|utf-16le:41000326:41,2603:4:true:4:41000326|ucs2:41000326:41,2603:4:true:4:41000326|ucs-2:41000326:41,2603:4:true:4:41000326"
    );
}

// ─── Buffer.from(array) ────────────────────────────────────────────────────

#[test]
fn from_array() {
    let result = eval_buffer(r"Buffer.from([104, 101, 108, 108, 111]).toString()");
    assert_eq!(result, "hello");
}

#[test]
fn from_json_object_inputs_match_node_vectors() {
    let result = eval_buffer(
        r#"(() => {
        const cases = [
            ["valid", { type: "Buffer", data: [1, 2, 255] }],
            ["empty", { type: "Buffer", data: [] }],
            ["mask", { type: "Buffer", data: [257, -1] }],
            ["missing_data", { type: "Buffer" }],
            ["missing_type", { data: [1] }],
        ];
        return cases.map(([label, value]) => {
            try {
                return label + ":" + Buffer.from(value).toString("hex");
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "valid:0102ff|empty:|mask:01ff|missing_data:TypeError|missing_type:TypeError"
    );
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
    let result =
        eval_buffer(r#"Buffer.concat([Buffer.from("hel"), Buffer.from("lo")]).toString()"#);
    assert_eq!(result, "hello");
}

#[test]
fn concat_with_total_length() {
    let result =
        eval_buffer(r#"Buffer.concat([Buffer.from("hello"), Buffer.from("world")], 5).toString()"#);
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

#[test]
fn write_accepts_encoding_overload_for_single_byte_strings() {
    let result = eval_buffer(
        r#"(() => {
        const latin1 = Buffer.alloc(2);
        const ascii = Buffer.alloc(2);
        return [
            latin1.write("\u00ffA", "latin1"),
            latin1.toString("hex"),
            ascii.write("\u00ffA", "ascii"),
            ascii.toString("hex"),
        ].join("|");
    })()"#,
    );
    assert_eq!(result, "2|ff41|2|ff41");
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
    let result = eval_buffer(r#"Buffer.from("abc").compare(Buffer.from("abc"))"#);
    assert_eq!(result, "0");
}

#[test]
fn compare_less() {
    let result = eval_buffer(r#"Buffer.from("abc").compare(Buffer.from("abd"))"#);
    assert_eq!(result, "-1");
}

#[test]
fn equals_true() {
    let result = eval_buffer(r#"Buffer.from("hello").equals(Buffer.from("hello"))"#);
    assert_eq!(result, "true");
}

#[test]
fn equals_false() {
    let result = eval_buffer(r#"Buffer.from("hello").equals(Buffer.from("world"))"#);
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
    let result = eval_buffer(r#"Buffer.from("hello world").indexOf("world")"#);
    assert_eq!(result, "6");
}

#[test]
fn index_of_negative_offset_matches_node() {
    let result = eval_buffer(
        r#"(() => {
        const buf = Buffer.from("abc");
        return [buf.indexOf("a", -1), buf.indexOf("c", -1), buf.indexOf(97, -1)].join(",");
    })()"#,
    );
    assert_eq!(result, "-1,2,-1");
}

#[test]
fn index_of_string_encoding_overload() {
    let result = eval_buffer(
        r#"(() => {
        const buf = Buffer.from("hello");
        return [buf.indexOf("6c6c", "hex"), buf.includes("6c6c", "hex")].join(",");
    })()"#,
    );
    assert_eq!(result, "2,true");
}

#[test]
fn includes_true() {
    let result = eval_buffer(r#"Buffer.from("hello world").includes("world")"#);
    assert_eq!(result, "true");
}

#[test]
fn includes_false() {
    let result = eval_buffer(r#"Buffer.from("hello").includes("xyz")"#);
    assert_eq!(result, "false");
}

#[test]
fn includes_negative_offset_matches_node() {
    let result = eval_buffer(
        r#"(() => {
        const buf = Buffer.from("abc");
        return [buf.includes("a", -1), buf.includes("c", -1)].join(",");
    })()"#,
    );
    assert_eq!(result, "false,true");
}

#[test]
fn last_index_of_vectors_match_node() {
    let result = eval_buffer(
        r#"(() => {
        const b = Buffer.from("abcabc");
        const hello = Buffer.from("hello");
        const cases = [
            ["str_default", () => b.lastIndexOf("bc")],
            ["str_offset3", () => b.lastIndexOf("bc", 3)],
            ["str_neg1", () => b.lastIndexOf("bc", -1)],
            ["num", () => b.lastIndexOf(0x61)],
            ["num_offset2", () => b.lastIndexOf(0x61, 2)],
            ["hex", () => hello.lastIndexOf("6c6c", "hex")],
            ["empty_default", () => b.lastIndexOf("")],
            ["empty_offset2", () => b.lastIndexOf("", 2)],
            ["uint8", () => b.lastIndexOf(new Uint8Array([98, 99]))],
            ["missing", () => b.lastIndexOf("zz")],
        ];
        return cases.map(([label, run]) => label + ":" + run()).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "str_default:4|str_offset3:1|str_neg1:4|num:3|num_offset2:0|hex:2|empty_default:6|empty_offset2:2|uint8:4|missing:-1"
    );
}

#[test]
fn byte_swap_vectors_match_node() {
    let result = eval_buffer(
        r#"(() => {
        const cases = [
            ["swap16_ok", () => Buffer.from([1, 2, 3, 4]).swap16().toString("hex")],
            ["swap16_self", () => { const b = Buffer.from([1, 2]); return b.swap16() === b; }],
            ["swap16_bad", () => Buffer.from([1, 2, 3]).swap16()],
            ["swap32_ok", () => Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]).swap32().toString("hex")],
            ["swap32_bad", () => Buffer.from([1, 2]).swap32()],
            ["swap64_ok", () => Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]).swap64().toString("hex")],
            ["swap64_bad", () => Buffer.from([1, 2, 3, 4]).swap64()],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "swap16_ok:02010403|swap16_self:true|swap16_bad:RangeError|swap32_ok:0403020108070605|swap32_bad:RangeError|swap64_ok:0807060504030201|swap64_bad:RangeError"
    );
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

#[test]
fn unknown_encoding_strict_entrypoints_match_node() {
    let result = eval_buffer(
        r#"(() => {
        const outcomes = [];
        try { Buffer.from("abc", "bogus"); outcomes.push("from:ok"); }
        catch (e) { outcomes.push("from:" + e.message); }
        try { Buffer.from([0x61]).toString("bogus"); outcomes.push("toString:ok"); }
        catch (e) { outcomes.push("toString:" + e.message); }
        try { Buffer.alloc(3).write("abc", 0, 3, "bogus"); outcomes.push("write:ok"); }
        catch (e) { outcomes.push("write:" + e.message); }
        outcomes.push("byteLength:" + Buffer.byteLength("abc", "bogus"));
        outcomes.push("isEncoding:" + Buffer.isEncoding("bogus"));
        return outcomes.join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "from:Unknown encoding: bogus|toString:Unknown encoding: bogus|write:Unknown encoding: bogus|byteLength:3|isEncoding:false"
    );
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

#[test]
fn global_buffer_allocation_size_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["zero", 0],
            ["three", 3],
            ["fraction", 3.5],
            ["string", "3"],
            ["negative", -1],
            ["nan", NaN],
            ["infinity", Infinity],
            ["null", null],
            ["undefined", undefined],
        ];
        const methods = ["alloc", "allocUnsafe", "allocUnsafeSlow"];
        const entries = methods.map((method) => {
            return method + "=" + cases.map(([label, size]) => {
                try {
                    return label + ":" + Buffer[method](size).length;
                } catch (e) {
                    return label + ":" + e.name;
                }
            }).join(",");
        });
        return "hasSlow:" + typeof Buffer.allocUnsafeSlow + "|" + entries.join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "hasSlow:function|alloc=zero:0,three:3,fraction:3,string:TypeError,negative:RangeError,nan:RangeError,infinity:RangeError,null:TypeError,undefined:TypeError|allocUnsafe=zero:0,three:3,fraction:3,string:TypeError,negative:RangeError,nan:RangeError,infinity:RangeError,null:TypeError,undefined:TypeError|allocUnsafeSlow=zero:0,three:3,fraction:3,string:TypeError,negative:RangeError,nan:RangeError,infinity:RangeError,null:TypeError,undefined:TypeError"
    );
}

#[test]
fn global_buffer_search_semantics_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const abc = Buffer.from("abc");
        const hello = Buffer.from("hello");
        return [abc.indexOf("a", -1), hello.indexOf("6c6c", "hex"), abc.includes("a", -1)].join(",");
    })()"#,
    );
    assert_eq!(result, "-1,2,false");
}

#[test]
fn global_buffer_last_index_of_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const b = Buffer.from("abcabc");
        const hello = Buffer.from("hello");
        const cases = [
            ["str_default", () => b.lastIndexOf("bc")],
            ["str_offset3", () => b.lastIndexOf("bc", 3)],
            ["str_neg1", () => b.lastIndexOf("bc", -1)],
            ["num", () => b.lastIndexOf(0x61)],
            ["num_offset2", () => b.lastIndexOf(0x61, 2)],
            ["hex", () => hello.lastIndexOf("6c6c", "hex")],
            ["empty_default", () => b.lastIndexOf("")],
            ["empty_offset2", () => b.lastIndexOf("", 2)],
            ["uint8", () => b.lastIndexOf(new Uint8Array([98, 99]))],
            ["missing", () => b.lastIndexOf("zz")],
        ];
        return cases.map(([label, run]) => label + ":" + run()).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "str_default:4|str_offset3:1|str_neg1:4|num:3|num_offset2:0|hex:2|empty_default:6|empty_offset2:2|uint8:4|missing:-1"
    );
}

#[test]
fn global_buffer_hex_truncates_at_invalid_or_incomplete_pair_like_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = ["1ag123", "1a7", "zz", "61 62", "61xz62", "0"];
        return cases.map((input) => [
            input,
            Buffer.from(input, "hex").toString("hex"),
            Buffer.byteLength(input, "hex"),
        ].join(":")).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "1ag123:1a:3|1a7:1a:1|zz::1|61 62:61:2|61xz62:61:3|0::0"
    );
}

#[test]
fn global_buffer_base64_byte_length_padding_matches_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = ["aGVsbG8=", "aGVsbG8", "TWE=", "TQ==", ""];
        return cases.map((input) => Buffer.byteLength(input, "base64")).join("|");
    })()"#,
    );
    assert_eq!(result, "5|5|2|1|0");
}

#[test]
fn global_buffer_byte_length_byte_backed_inputs_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            new ArrayBuffer(3),
            new Uint16Array(2),
            new Uint8Array([1, 2, 3]),
            Buffer.from([1, 2, 3, 4]),
        ];
        return cases.map((input) => Buffer.byteLength(input)).join("|");
    })()"#,
    );
    assert_eq!(result, "3|4|3|4");
}

#[test]
fn global_buffer_byte_length_rejects_invalid_inputs_like_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["null", null],
            ["undefined", undefined],
            ["object", {}],
            ["length_object", { length: 3 }],
            ["array_empty", []],
            ["array_values", [1, 2]],
            ["array_buffer", new ArrayBuffer(2)],
            ["uint16", new Uint16Array(2)],
            ["buffer", Buffer.from([1, 2, 3])],
        ];
        return cases.map(([label, input]) => {
            try {
                return label + ":" + Buffer.byteLength(input);
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "null:TypeError|undefined:TypeError|object:TypeError|length_object:TypeError|array_empty:TypeError|array_values:TypeError|array_buffer:2|uint16:4|buffer:3"
    );
}

#[test]
fn global_buffer_string_and_buffer_fill_match_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const allocString = Buffer.alloc(5, "ab").toString("hex");
        const allocHex = Buffer.alloc(4, "61", "hex").toString("hex");
        const fillString = Buffer.alloc(5);
        fillString.fill("ab", 1, 5);
        const fillHex = Buffer.alloc(5);
        fillHex.fill("61", 1, 5, "hex");
        const fillBuffer = Buffer.alloc(5);
        fillBuffer.fill(Buffer.from([1, 2]), 1, 5);
        return [
            allocString,
            allocHex,
            fillString.toString("hex"),
            fillHex.toString("hex"),
            fillBuffer.toString("hex"),
        ].join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "6162616261|61616161|0061626162|0061616161|0001020102"
    );
}

#[test]
fn global_buffer_fill_range_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["fill_default", () => {
                const b = Buffer.alloc(5);
                const returned = b.fill("ab");
                return (returned === b) + ":" + b.toString("hex");
            }],
            ["fill_range", () => {
                const b = Buffer.alloc(5);
                const returned = b.fill("ab", 1, 4);
                return (returned === b) + ":" + b.toString("hex");
            }],
            ["fill_start_eq_end", () => {
                const b = Buffer.from([1, 2, 3]);
                const returned = b.fill(9, 2, 2);
                return (returned === b) + ":" + b.toString("hex");
            }],
            ["fill_start_gt_end", () => {
                const b = Buffer.from([1, 2, 3]);
                const returned = b.fill(9, 3, 1);
                return (returned === b) + ":" + b.toString("hex");
            }],
            ["fill_negative_start", () => {
                const b = Buffer.alloc(2);
                return b.fill(1, -1);
            }],
            ["fill_negative_end", () => {
                const b = Buffer.alloc(2);
                return b.fill(1, 0, -1);
            }],
            ["fill_start_oob", () => {
                const b = Buffer.alloc(2);
                const returned = b.fill(1, 3);
                return (returned === b) + ":" + b.toString("hex");
            }],
            ["fill_end_oob", () => {
                const b = Buffer.alloc(2);
                return b.fill(1, 0, 3);
            }],
            ["fill_hex", () => {
                const b = Buffer.alloc(5);
                b.fill("61", 1, 5, "hex");
                return b.toString("hex");
            }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "fill_default:true:6162616261|fill_range:true:0061626100|fill_start_eq_end:true:010203|fill_start_gt_end:true:010203|fill_negative_start:RangeError|fill_negative_end:RangeError|fill_start_oob:true:0000|fill_end_oob:RangeError|fill_hex:0061616161"
    );
}

#[test]
fn global_buffer_fill_argument_validation_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["start_null", () => Buffer.alloc(4).fill(1, null).toString("hex")],
            ["start_true", () => Buffer.alloc(4).fill(1, true).toString("hex")],
            ["start_object", () => Buffer.alloc(4).fill(1, { valueOf() { return 1; } }).toString("hex")],
            ["start_fraction", () => Buffer.alloc(4).fill(1, 1.9).toString("hex")],
            ["start_nan", () => Buffer.alloc(4).fill(1, NaN).toString("hex")],
            ["start_infinity", () => Buffer.alloc(4).fill(1, Infinity).toString("hex")],
            ["start_oob", () => {
                const b = Buffer.alloc(2);
                const returned = b.fill(1, 3);
                return (returned === b) + ":" + b.toString("hex");
            }],
            ["start_string_numeric_fill", () => Buffer.alloc(4).fill(1, "utf8").toString("hex")],
            ["start_string_string_fill", () => Buffer.alloc(4).fill("61", "hex").toString("hex")],
            ["end_null", () => Buffer.alloc(4).fill(1, 0, null).toString("hex")],
            ["end_false", () => Buffer.alloc(4).fill(1, 0, false).toString("hex")],
            ["end_object", () => Buffer.alloc(4).fill(1, 0, { valueOf() { return 1; } }).toString("hex")],
            ["end_fraction", () => Buffer.alloc(4).fill(1, 0, 2.9).toString("hex")],
            ["end_nan", () => Buffer.alloc(4).fill(1, 0, NaN).toString("hex")],
            ["end_infinity", () => Buffer.alloc(4).fill(1, 0, Infinity).toString("hex")],
            ["end_string_numeric_fill", () => Buffer.alloc(4).fill(1, 0, "utf8").toString("hex")],
            ["end_string_string_fill", () => Buffer.alloc(4).fill("61", 0, "hex").toString("hex")],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "start_null:TypeError|start_true:TypeError|start_object:TypeError|start_fraction:RangeError|start_nan:RangeError|start_infinity:RangeError|start_oob:true:0000|start_string_numeric_fill:TypeError|start_string_string_fill:61616161|end_null:TypeError|end_false:TypeError|end_object:TypeError|end_fraction:RangeError|end_nan:RangeError|end_infinity:RangeError|end_string_numeric_fill:TypeError|end_string_string_fill:61616161"
    );
}

#[test]
fn global_buffer_arraybuffer_offset_length_match_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const ab = new Uint8Array([1, 2, 3, 4, 5]).buffer;
        const cases = [];
        for (const [label, args] of [
            ["all", [ab]],
            ["offset", [ab, 1]],
            ["offset_len", [ab, 1, 2]],
            ["oversize_len", [ab, 4, 99]],
            ["negative_offset", [ab, -1]],
            ["oob_offset", [ab, 6]],
        ]) {
            try {
                cases.push(label + ":" + Buffer.from(...args).toString("hex"));
            } catch (e) {
                cases.push(label + ":" + e.name);
            }
        }
        return cases.join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "all:0102030405|offset:02030405|offset_len:0203|oversize_len:RangeError|negative_offset:RangeError|oob_offset:RangeError"
    );
}

#[test]
fn global_buffer_arraybuffer_length_coercion_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const ab = new ArrayBuffer(4);
        new Uint8Array(ab).set([1, 2, 3, 4]);
        const cases = [
            ["undefined", undefined],
            ["two", 2],
            ["fraction", 2.9],
            ["string_two", "2"],
            ["negative", -1],
            ["string_negative", "-1"],
            ["negative_infinity", -Infinity],
            ["nan", NaN],
            ["bad_string", "bad"],
            ["null", null],
            ["infinity", Infinity],
            ["oversize", 99],
        ];
        return cases.map(([label, length]) => {
            try {
                const out = length === undefined ? Buffer.from(ab, 1) : Buffer.from(ab, 1, length);
                return label + ":" + out.toString("hex");
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "undefined:020304|two:0203|fraction:0203|string_two:0203|negative:|string_negative:|negative_infinity:|nan:|bad_string:|null:|infinity:RangeError|oversize:RangeError"
    );
}

#[test]
fn global_buffer_arraybuffer_byte_offset_coercion_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const ab = new ArrayBuffer(4);
        new Uint8Array(ab).set([1, 2, 3, 4]);
        const cases = [
            ["undefined", undefined],
            ["zero", 0],
            ["positive_fraction_lt_one", 0.9],
            ["one", 1],
            ["fraction", 1.9],
            ["string_one", "1"],
            ["string_fraction", "1.9"],
            ["negative_fraction", -0.9],
            ["negative", -1],
            ["string_negative", "-1"],
            ["nan", NaN],
            ["bad_string", "bad"],
            ["null", null],
            ["infinity", Infinity],
            ["negative_infinity", -Infinity],
            ["at_end", 4],
            ["past_fraction", 4.1],
            ["oversize", 99],
            ["true", true],
            ["false", false],
        ];
        return cases.map(([label, offset]) => {
            try {
                const out = offset === undefined ? Buffer.from(ab) : Buffer.from(ab, offset);
                return label + ":" + out.toString("hex");
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "undefined:01020304|zero:01020304|positive_fraction_lt_one:010203|one:020304|fraction:0203|string_one:020304|string_fraction:0203|negative_fraction:01020304|negative:RangeError|string_negative:RangeError|nan:01020304|bad_string:01020304|null:01020304|infinity:RangeError|negative_infinity:RangeError|at_end:|past_fraction:RangeError|oversize:RangeError|true:020304|false:01020304"
    );
}

#[test]
fn global_buffer_arraybuffer_backing_view_matches_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const ab = new ArrayBuffer(4);
        const view = new Uint8Array(ab);
        view.set([1, 2, 3, 4]);
        const shared = Buffer.from(ab, 1, 2);
        shared[0] = 9;
        view[2] = 8;

        const typed = new Uint8Array([1, 2, 3]);
        const copied = Buffer.from(typed);
        copied[0] = 9;
        typed[1] = 8;

        return [
            "shared_hex:" + shared.toString("hex"),
            "ab:" + Array.from(view).join(","),
            "same:" + (shared.buffer === ab),
            "offset:" + shared.byteOffset,
            "len:" + shared.byteLength,
            "typed_hex:" + copied.toString("hex"),
            "typed:" + Array.from(typed).join(","),
            "typed_same:" + (copied.buffer === typed.buffer),
        ].join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "shared_hex:0908|ab:1,9,8,4|same:true|offset:1|len:2|typed_hex:090203|typed:1,8,3|typed_same:false"
    );
}

#[test]
fn global_buffer_typed_array_inputs_match_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            new Uint8Array([1, 2, 3]),
            new Uint16Array([0x0102, 0x03ff]),
            new Int16Array([-1, 258]),
            new DataView(new Uint8Array([1, 2, 3, 4]).buffer),
        ];
        return cases.map((input) => Buffer.from(input).toString("hex")).join("|");
    })()"#,
    );
    assert_eq!(result, "010203|02ff|ff02|");
}

#[test]
fn global_buffer_array_like_inputs_match_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["plain", { 0: 65, 1: 66, length: 2 }],
            ["mask", { 0: 257, 1: -1, length: 2 }],
            ["empty", { length: 0 }],
        ];
        return cases.map(([label, value]) => {
            try {
                return label + ":" + Buffer.from(value).toString("hex");
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(result, "plain:4142|mask:01ff|empty:");
}

#[test]
fn global_buffer_array_like_length_coercion_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["number_two", { 0: 65, 1: 66, length: 2 }],
            ["string_two", { 0: 65, 1: 66, length: "2" }],
            ["fraction", { 0: 65, 1: 66, 2: 67, length: 2.9 }],
            ["negative_fraction", { 0: 65, length: -0.9 }],
            ["negative", { 0: 65, length: -1 }],
            ["negative_infinity", { 0: 65, length: -Infinity }],
            ["nan", { 0: 65, length: NaN }],
            ["bad_string", { 0: 65, length: "bad" }],
            ["null_len", { 0: 65, length: null }],
            ["object_len", { 0: 65, length: { valueOf() { return 2; } } }],
            ["undefined_len", { 0: 65, length: undefined }],
            ["infinity", { 0: 65, length: Infinity }],
            ["true_len", { 0: 65, length: true }],
            ["false_len", { 0: 65, length: false }],
            ["no_length", { 0: 65 }],
        ];
        return cases.map(([label, value]) => {
            try {
                const out = Buffer.from(value);
                return label + ":" + out.length + ":" + out.toString("hex");
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "number_two:2:4142|string_two:0:|fraction:2:4142|negative_fraction:0:|negative:0:|negative_infinity:0:|nan:0:|bad_string:0:|null_len:0:|object_len:0:|undefined_len:TypeError|infinity:RangeError|true_len:0:|false_len:0:|no_length:TypeError"
    );
}

#[test]
fn global_buffer_json_object_inputs_match_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["valid", { type: "Buffer", data: [1, 2, 255] }],
            ["empty", { type: "Buffer", data: [] }],
            ["mask", { type: "Buffer", data: [257, -1] }],
            ["missing_data", { type: "Buffer" }],
            ["missing_type", { data: [1] }],
        ];
        return cases.map(([label, value]) => {
            try {
                return label + ":" + Buffer.from(value).toString("hex");
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "valid:0102ff|empty:|mask:01ff|missing_data:TypeError|missing_type:TypeError"
    );
}

#[test]
fn global_buffer_concat_rejects_non_arrays_like_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["empty", () => Buffer.concat([]).toString("hex")],
            ["not_array_string", () => Buffer.concat("abc").toString("hex")],
            ["not_array_uint8", () => Buffer.concat(new Uint8Array([1, 2])).toString("hex")],
            ["string_element", () => Buffer.concat(["ab"]).toString("hex")],
            ["array_element", () => Buffer.concat([[97, 98]]).toString("hex")],
            ["arraybuffer_element", () => Buffer.concat([new ArrayBuffer(2)]).toString("hex")],
            ["object_element", () => Buffer.concat([{ length: 2, 0: 97, 1: 98 }]).toString("hex")],
            ["uint16_element", () => Buffer.concat([new Uint16Array([0x6162])]).toString("hex")],
            ["dataview_element", () => Buffer.concat([new DataView(new ArrayBuffer(2))]).toString("hex")],
            ["null_element", () => Buffer.concat([null]).toString("hex")],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "empty:|not_array_string:TypeError|not_array_uint8:TypeError|string_element:TypeError|array_element:TypeError|arraybuffer_element:TypeError|object_element:TypeError|uint16_element:TypeError|dataview_element:TypeError|null_element:TypeError"
    );
}

#[test]
fn global_buffer_concat_total_length_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const totals = [
            ["default", undefined],
            ["zero", 0],
            ["one", 1],
            ["fraction", 1.9],
            ["string", "1"],
            ["negative", -1],
            ["nan", NaN],
            ["infinity", Infinity],
            ["null", null],
        ];
        return totals.map(([label, total]) => {
            try {
                const list = [Buffer.from([97, 98])];
                const out = total === undefined ? Buffer.concat(list) : Buffer.concat(list, total);
                return label + ":" + out.length + ":" + out.toString("hex");
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "default:2:6162|zero:0:|one:1:61|fraction:RangeError|string:TypeError|negative:RangeError|nan:RangeError|infinity:RangeError|null:TypeError"
    );
}

#[test]
fn global_buffer_copy_range_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["trunc_full", () => {
                const dst = Buffer.alloc(3);
                const copied = Buffer.from("abcdef").copy(dst);
                return copied + ":" + dst.toString("hex");
            }],
            ["trunc_target_start", () => {
                const dst = Buffer.alloc(3);
                const copied = Buffer.from("abcdef").copy(dst, 1);
                return copied + ":" + dst.toString("hex");
            }],
            ["source_range", () => {
                const dst = Buffer.alloc(4);
                const copied = Buffer.from("abcdef").copy(dst, 0, 2, 5);
                return copied + ":" + dst.toString("hex");
            }],
            ["source_end_oob", () => {
                const dst = Buffer.alloc(8);
                const copied = Buffer.from("abc").copy(dst, 0, 1, 99);
                return copied + ":" + dst.toString("hex");
            }],
            ["target_start_oob", () => {
                const dst = Buffer.alloc(2);
                const copied = Buffer.from("abc").copy(dst, 9);
                return copied + ":" + dst.toString("hex");
            }],
            ["negative_target", () => {
                const dst = Buffer.alloc(2);
                return Buffer.from("abc").copy(dst, -1);
            }],
            ["negative_source", () => {
                const dst = Buffer.alloc(2);
                return Buffer.from("abc").copy(dst, 0, -1);
            }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "trunc_full:3:616263|trunc_target_start:2:006162|source_range:3:63646500|source_end_oob:2:6263000000000000|target_start_oob:0:0000|negative_target:RangeError|negative_source:RangeError"
    );
}

#[test]
fn global_buffer_copy_bound_coercion_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["target_fraction", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 1.9) + ":" + dst.toString("hex");
            }],
            ["source_fraction", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, 1.9) + ":" + dst.toString("hex");
            }],
            ["end_fraction", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, 0, 2.9) + ":" + dst.toString("hex");
            }],
            ["target_nan", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, NaN) + ":" + dst.toString("hex");
            }],
            ["source_nan", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, NaN) + ":" + dst.toString("hex");
            }],
            ["end_nan", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, 0, NaN) + ":" + dst.toString("hex");
            }],
            ["target_bad_string", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, "bad") + ":" + dst.toString("hex");
            }],
            ["source_bad_string", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, "bad") + ":" + dst.toString("hex");
            }],
            ["end_bad_string", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, 0, "bad") + ":" + dst.toString("hex");
            }],
            ["target_infinity", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, Infinity) + ":" + dst.toString("hex");
            }],
            ["source_infinity", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, Infinity) + ":" + dst.toString("hex");
            }],
            ["end_infinity", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, 0, Infinity) + ":" + dst.toString("hex");
            }],
            ["target_negative_fraction", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, -0.9) + ":" + dst.toString("hex");
            }],
            ["source_negative_fraction", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, -0.9) + ":" + dst.toString("hex");
            }],
            ["end_negative_fraction", () => {
                const dst = Buffer.alloc(4);
                return Buffer.from("abcd").copy(dst, 0, 0, -0.9) + ":" + dst.toString("hex");
            }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "target_fraction:3:00616263|source_fraction:3:62636400|end_fraction:2:61620000|target_nan:4:61626364|source_nan:4:61626364|end_nan:0:00000000|target_bad_string:4:61626364|source_bad_string:4:61626364|end_bad_string:0:00000000|target_infinity:4:61626364|source_infinity:4:61626364|end_infinity:0:00000000|target_negative_fraction:RangeError|source_negative_fraction:RangeError|end_negative_fraction:RangeError"
    );
}

#[test]
fn global_buffer_write_range_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["write_default", () => {
                const b = Buffer.alloc(4);
                const written = b.write("abcdef");
                return written + ":" + b.toString("hex");
            }],
            ["write_offset", () => {
                const b = Buffer.alloc(4);
                const written = b.write("abcdef", 2);
                return written + ":" + b.toString("hex");
            }],
            ["write_len", () => {
                const b = Buffer.alloc(4);
                const written = b.write("abcdef", 1, 2);
                return written + ":" + b.toString("hex");
            }],
            ["write_oob", () => {
                const b = Buffer.alloc(2);
                const written = b.write("abc", 9);
                return written + ":" + b.toString("hex");
            }],
            ["write_negative", () => {
                const b = Buffer.alloc(2);
                return b.write("abc", -1);
            }],
            ["write_len_oob", () => {
                const b = Buffer.alloc(3);
                const written = b.write("abcdef", 1, 99);
                return written + ":" + b.toString("hex");
            }],
            ["write_negative_len", () => {
                const b = Buffer.alloc(3);
                return b.write("abc", 0, -1);
            }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "write_default:4:61626364|write_offset:2:00006162|write_len:2:00616200|write_oob:RangeError|write_negative:RangeError|write_len_oob:RangeError|write_negative_len:RangeError"
    );
}

#[test]
fn global_buffer_write_argument_type_validation_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["offset_null", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", null) + ":" + b.toString("hex");
            }],
            ["offset_true", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", true) + ":" + b.toString("hex");
            }],
            ["offset_object", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", { valueOf() { return 1; } }) + ":" + b.toString("hex");
            }],
            ["offset_array", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", [1]) + ":" + b.toString("hex");
            }],
            ["offset_undefined", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", undefined) + ":" + b.toString("hex");
            }],
            ["offset_string_encoding", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", "utf8") + ":" + b.toString("hex");
            }],
            ["length_null", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", 0, null) + ":" + b.toString("hex");
            }],
            ["length_false", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", 0, false) + ":" + b.toString("hex");
            }],
            ["length_object", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", 0, { valueOf() { return 1; } }) + ":" + b.toString("hex");
            }],
            ["length_array", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", 0, [1]) + ":" + b.toString("hex");
            }],
            ["length_undefined", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", 0, undefined) + ":" + b.toString("hex");
            }],
            ["length_string_encoding", () => {
                const b = Buffer.alloc(4);
                return b.write("abcd", 0, "utf8") + ":" + b.toString("hex");
            }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "offset_null:TypeError|offset_true:TypeError|offset_object:TypeError|offset_array:TypeError|offset_undefined:4:61626364|offset_string_encoding:4:61626364|length_null:TypeError|length_false:TypeError|length_object:TypeError|length_array:TypeError|length_undefined:4:61626364|length_string_encoding:4:61626364"
    );
}

#[test]
fn global_buffer_slice_and_subarray_are_shared_buffer_views_like_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const buf = Buffer.from([1, 2, 3, 4]);
        const sliced = buf.slice(1, 3);
        sliced[0] = 9;
        const sub = buf.subarray(2, 4);
        sub[0] = 8;
        return [
            sliced.toString("hex"),
            buf.toString("hex"),
            Buffer.isBuffer(sliced),
            Buffer.isBuffer(sub),
        ].join("|");
    })()"#,
    );
    assert_eq!(result, "0908|01090804|true|true");
}

#[test]
fn global_buffer_compare_and_equals_type_validation_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["equals_buf", () => Buffer.from("a").equals(Buffer.from("a"))],
            ["equals_uint8", () => Buffer.from("a").equals(new Uint8Array([97]))],
            ["compare_uint8", () => Buffer.from("a").compare(new Uint8Array([97]))],
            ["static_compare_uint8", () => Buffer.compare(Buffer.from("a"), new Uint8Array([98]))],
            ["equals_string", () => Buffer.from("a").equals("a")],
            ["compare_string", () => Buffer.from("a").compare("a")],
            ["static_compare_string", () => Buffer.compare(Buffer.from("a"), "a")],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "equals_buf:true|equals_uint8:true|compare_uint8:0|static_compare_uint8:-1|equals_string:TypeError|compare_string:TypeError|static_compare_string:TypeError"
    );
}

#[test]
fn global_buffer_compare_range_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["range_equal", () => Buffer.from("abcdef").compare(Buffer.from("zzbcdzz"), 2, 5, 1, 4)],
            ["range_less", () => Buffer.from("abc").compare(Buffer.from("abd"), 0, 3, 0, 3)],
            ["range_greater", () => Buffer.from("abd").compare(Buffer.from("abc"), 0, 3, 0, 3)],
            ["empty_slices", () => Buffer.from("abc").compare(Buffer.from("xyz"), 1, 1, 2, 2)],
            ["uint8_range", () => Buffer.from("abc").compare(new Uint8Array([120, 97, 98, 99]), 1, 4, 0, 3)],
        ];
        return cases.map(([label, run]) => label + ":" + run()).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "range_equal:0|range_less:-1|range_greater:1|empty_slices:0|uint8_range:0"
    );
}

#[test]
fn global_buffer_compare_invalid_range_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["compare_neg_target_start", () => Buffer.from("abc").compare(Buffer.from("abc"), -1)],
            ["compare_neg_target_end", () => Buffer.from("abc").compare(Buffer.from("abc"), 0, -1)],
            ["compare_target_start_oob", () => Buffer.from("abc").compare(Buffer.from("abc"), 4)],
            ["compare_target_end_oob", () => Buffer.from("abc").compare(Buffer.from("abc"), 0, 4)],
            ["compare_neg_source_start", () => Buffer.from("abc").compare(Buffer.from("abc"), 0, 3, -1)],
            ["compare_neg_source_end", () => Buffer.from("abc").compare(Buffer.from("abc"), 0, 3, 0, -1)],
            ["compare_source_start_oob", () => Buffer.from("abc").compare(Buffer.from("abc"), 0, 3, 4)],
            ["compare_source_end_oob", () => Buffer.from("abc").compare(Buffer.from("abc"), 0, 3, 0, 4)],
            ["compare_target_start_eq_len", () => Buffer.from("abc").compare(Buffer.from("abc"), 3, 3, 0, 0)],
            ["compare_source_start_eq_len", () => Buffer.from("abc").compare(Buffer.from("abc"), 0, 0, 3, 3)],
            ["compare_cross_empty", () => Buffer.from("abc").compare(Buffer.from("abc"), 2, 1, 2, 1)],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "compare_neg_target_start:RangeError|compare_neg_target_end:RangeError|compare_target_start_oob:1|compare_target_end_oob:RangeError|compare_neg_source_start:RangeError|compare_neg_source_end:RangeError|compare_source_start_oob:-1|compare_source_end_oob:RangeError|compare_target_start_eq_len:0|compare_source_start_eq_len:0|compare_cross_empty:0"
    );
}

#[test]
fn global_buffer_compare_argument_type_validation_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["target_start_null", () => Buffer.from("abcd").compare(Buffer.from("abcd"), null, 4, 0, 4)],
            ["target_start_object", () => Buffer.from("abcd").compare(Buffer.from("zabcd"), { valueOf() { return 1; } }, 5, 0, 4)],
            ["target_start_array", () => Buffer.from("abcd").compare(Buffer.from("zabcd"), [1], 5, 0, 4)],
            ["target_start_bad_string", () => Buffer.from("abcd").compare(Buffer.from("abcd"), "bad", 4, 0, 4)],
            ["target_end_null", () => Buffer.from("abcd").compare(Buffer.from("abcd"), 0, null, 0, 4)],
            ["source_start_null", () => Buffer.from("abcd").compare(Buffer.from("abcd"), 0, 4, null, 4)],
            ["source_start_object", () => Buffer.from("zabcd").compare(Buffer.from("abcd"), 0, 4, { valueOf() { return 1; } }, 5)],
            ["source_end_array", () => Buffer.from("abcd").compare(Buffer.from("ab"), 0, 2, 0, [1])],
            ["target_start_fraction", () => Buffer.from("abcd").compare(Buffer.from("zzabcd"), 2.9, 6, 0, 4)],
            ["target_start_nan", () => Buffer.from("abcd").compare(Buffer.from("abcd"), NaN, 4, 0, 4)],
            ["source_end_fraction", () => Buffer.from("abcd").compare(Buffer.from("ab"), 0, 2, 0, 2.9)],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "target_start_null:TypeError|target_start_object:TypeError|target_start_array:TypeError|target_start_bad_string:TypeError|target_end_null:TypeError|source_start_null:TypeError|source_start_object:TypeError|source_end_array:TypeError|target_start_fraction:RangeError|target_start_nan:RangeError|source_end_fraction:RangeError"
    );
}

#[test]
fn global_buffer_byte_swap_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["swap16_ok", () => Buffer.from([1, 2, 3, 4]).swap16().toString("hex")],
            ["swap16_self", () => { const b = Buffer.from([1, 2]); return b.swap16() === b; }],
            ["swap16_bad", () => Buffer.from([1, 2, 3]).swap16()],
            ["swap32_ok", () => Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]).swap32().toString("hex")],
            ["swap32_bad", () => Buffer.from([1, 2]).swap32()],
            ["swap64_ok", () => Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]).swap64().toString("hex")],
            ["swap64_bad", () => Buffer.from([1, 2, 3, 4]).swap64()],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "swap16_ok:02010403|swap16_self:true|swap16_bad:RangeError|swap32_ok:0403020108070605|swap32_bad:RangeError|swap64_ok:0807060504030201|swap64_bad:RangeError"
    );
}

#[test]
fn global_buffer_signed_integer_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const b = Buffer.alloc(12);
        const parts = [];
        parts.push("writeInt8:" + b.writeInt8(-1, 0));
        parts.push("writeInt16LE:" + b.writeInt16LE(-2, 1));
        parts.push("writeInt16BE:" + b.writeInt16BE(-3, 3));
        parts.push("writeInt32LE:" + b.writeInt32LE(-4, 5));
        parts.push("writeInt32BE:" + b.writeInt32BE(-5, 8));
        parts.push("hex:" + b.toString("hex"));
        parts.push("readInt8:" + b.readInt8(0));
        parts.push("readInt16LE:" + b.readInt16LE(1));
        parts.push("readInt16BE:" + b.readInt16BE(3));
        parts.push("readInt32LE:" + b.readInt32LE(5));
        parts.push("readInt32BE:" + b.readInt32BE(8));
        return parts.join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "writeInt8:1|writeInt16LE:3|writeInt16BE:5|writeInt32LE:9|writeInt32BE:12|hex:fffefffffdfcfffffffffffb|readInt8:-1|readInt16LE:-2|readInt16BE:-3|readInt32LE:-4|readInt32BE:-5"
    );
}

#[test]
fn global_buffer_integer_bounds_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["readUInt8_oob", () => Buffer.from([1]).readUInt8(1)],
            ["readUInt16LE_short", () => Buffer.from([1]).readUInt16LE(0)],
            ["readInt32BE_short", () => Buffer.from([1, 2, 3]).readInt32BE(0)],
            ["writeUInt8_oob", () => Buffer.alloc(1).writeUInt8(1, 1)],
            ["writeUInt16LE_short", () => Buffer.alloc(1).writeUInt16LE(1, 0)],
            ["writeInt32BE_short", () => Buffer.alloc(3).writeInt32BE(-1, 0)],
            ["readUInt8_negative", () => Buffer.from([1]).readUInt8(-1)],
            ["writeUInt8_negative", () => Buffer.alloc(1).writeUInt8(1, -1)],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "readUInt8_oob:RangeError|readUInt16LE_short:RangeError|readInt32BE_short:RangeError|writeUInt8_oob:RangeError|writeUInt16LE_short:RangeError|writeInt32BE_short:RangeError|readUInt8_negative:RangeError|writeUInt8_negative:RangeError"
    );
}

#[test]
fn global_buffer_integer_write_value_range_validation_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["u8_ok", () => { const b = Buffer.alloc(1); return b.writeUInt8(255, 0) + ":" + b.toString("hex"); }],
            ["u8_neg", () => { const b = Buffer.alloc(1); return b.writeUInt8(-1, 0) + ":" + b.toString("hex"); }],
            ["u8_oob", () => { const b = Buffer.alloc(1); return b.writeUInt8(256, 0) + ":" + b.toString("hex"); }],
            ["u8_fraction", () => { const b = Buffer.alloc(1); return b.writeUInt8(1.9, 0) + ":" + b.toString("hex"); }],
            ["u8_nan", () => { const b = Buffer.alloc(1); return b.writeUInt8(NaN, 0) + ":" + b.toString("hex"); }],
            ["u8_string", () => { const b = Buffer.alloc(1); return b.writeUInt8("1", 0) + ":" + b.toString("hex"); }],
            ["u8_infinity", () => { const b = Buffer.alloc(1); return b.writeUInt8(Infinity, 0) + ":" + b.toString("hex"); }],
            ["u8_bigint", () => { const b = Buffer.alloc(1); return b.writeUInt8(1n, 0) + ":" + b.toString("hex"); }],
            ["i8_ok_neg", () => { const b = Buffer.alloc(1); return b.writeInt8(-1, 0) + ":" + b.toString("hex"); }],
            ["i8_low", () => { const b = Buffer.alloc(1); return b.writeInt8(-129, 0) + ":" + b.toString("hex"); }],
            ["i8_high", () => { const b = Buffer.alloc(1); return b.writeInt8(128, 0) + ":" + b.toString("hex"); }],
            ["u16_ok", () => { const b = Buffer.alloc(2); return b.writeUInt16LE(65535, 0) + ":" + b.toString("hex"); }],
            ["u16_oob", () => { const b = Buffer.alloc(2); return b.writeUInt16LE(65536, 0) + ":" + b.toString("hex"); }],
            ["i16_ok_neg", () => { const b = Buffer.alloc(2); return b.writeInt16BE(-2, 0) + ":" + b.toString("hex"); }],
            ["i16_low", () => { const b = Buffer.alloc(2); return b.writeInt16BE(-32769, 0) + ":" + b.toString("hex"); }],
            ["i16_high", () => { const b = Buffer.alloc(2); return b.writeInt16BE(32768, 0) + ":" + b.toString("hex"); }],
            ["u32_ok", () => { const b = Buffer.alloc(4); return b.writeUInt32BE(4294967295, 0) + ":" + b.toString("hex"); }],
            ["u32_oob", () => { const b = Buffer.alloc(4); return b.writeUInt32BE(4294967296, 0) + ":" + b.toString("hex"); }],
            ["i32_ok_neg", () => { const b = Buffer.alloc(4); return b.writeInt32LE(-2, 0) + ":" + b.toString("hex"); }],
            ["i32_low", () => { const b = Buffer.alloc(4); return b.writeInt32LE(-2147483649, 0) + ":" + b.toString("hex"); }],
            ["i32_high", () => { const b = Buffer.alloc(4); return b.writeInt32LE(2147483648, 0) + ":" + b.toString("hex"); }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "u8_ok:1:ff|u8_neg:RangeError|u8_oob:RangeError|u8_fraction:1:01|u8_nan:1:00|u8_string:1:01|u8_infinity:RangeError|u8_bigint:TypeError|i8_ok_neg:1:ff|i8_low:RangeError|i8_high:RangeError|u16_ok:2:ffff|u16_oob:RangeError|i16_ok_neg:2:fffe|i16_low:RangeError|i16_high:RangeError|u32_ok:4:ffffffff|u32_oob:RangeError|i32_ok_neg:4:feffffff|i32_low:RangeError|i32_high:RangeError"
    );
}

#[test]
fn global_buffer_lowercase_uint_aliases_match_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const names = [
            "readUint8",
            "readUint16BE",
            "readUint16LE",
            "readUint32BE",
            "readUint32LE",
            "writeUint8",
            "writeUint16BE",
            "writeUint16LE",
            "writeUint32BE",
            "writeUint32LE",
        ];
        const b = Buffer.alloc(13);
        const writes = [
            b.writeUint8(0x12, 0),
            b.writeUint16BE(0x3456, 1),
            b.writeUint16LE(0x789a, 3),
            b.writeUint32BE(0x01020304, 5),
            b.writeUint32LE(0x05060708, 9),
        ].join(",");
        const reads = [
            b.readUint8(0),
            b.readUint16BE(1),
            b.readUint16LE(3),
            b.readUint32BE(5),
            b.readUint32LE(9),
        ].join(",");
        const invalid = [];
        try { Buffer.alloc(1).writeUint8(-1, 0); invalid.push("neg:ok"); }
        catch (e) { invalid.push("neg:" + e.name); }
        try { Buffer.from([1, 2]).readUint16LE(null); invalid.push("offset:ok"); }
        catch (e) { invalid.push("offset:" + e.name); }
        return [
            names.map((name) => name + ":" + typeof Buffer.prototype[name]).join(","),
            writes,
            b.toString("hex"),
            reads,
            invalid.join(","),
        ].join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "readUint8:function,readUint16BE:function,readUint16LE:function,readUint32BE:function,readUint32LE:function,writeUint8:function,writeUint16BE:function,writeUint16LE:function,writeUint32BE:function,writeUint32LE:function|1,3,5,9,13|1234569a780102030408070605|18,13398,30874,16909060,84281096|neg:RangeError,offset:TypeError"
    );
}

#[test]
fn global_buffer_variable_width_unsigned_integer_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["types", () => ["readUIntBE", "readUIntLE", "writeUIntBE", "writeUIntLE"].map((name) => name + ":" + typeof Buffer.prototype[name]).join(",")],
            ["read_be_1", () => Buffer.from([0x12]).readUIntBE(0, 1)],
            ["read_be_2", () => Buffer.from([0x12, 0x34]).readUIntBE(0, 2)],
            ["read_be_6", () => Buffer.from([1, 2, 3, 4, 5, 6]).readUIntBE(0, 6)],
            ["read_le_6", () => Buffer.from([1, 2, 3, 4, 5, 6]).readUIntLE(0, 6)],
            ["write_be_6", () => { const b = Buffer.alloc(6); return b.writeUIntBE(0x010203040506, 0, 6) + ":" + b.toString("hex"); }],
            ["write_le_6", () => { const b = Buffer.alloc(6); return b.writeUIntLE(0x010203040506, 0, 6) + ":" + b.toString("hex"); }],
            ["read_len_zero", () => Buffer.from([1]).readUIntBE(0, 0)],
            ["read_len_seven", () => Buffer.from([1, 2, 3, 4, 5, 6, 7]).readUIntBE(0, 7)],
            ["read_len_fraction", () => Buffer.from([1, 2]).readUIntBE(0, 1.9)],
            ["read_len_string", () => Buffer.from([1, 2]).readUIntBE(0, "2")],
            ["read_len_null", () => Buffer.from([1, 2]).readUIntBE(0, null)],
            ["read_offset_null", () => Buffer.from([1, 2]).readUIntBE(null, 1)],
            ["write_oob_value", () => { const b = Buffer.alloc(2); return b.writeUIntBE(0x10000, 0, 2) + ":" + b.toString("hex"); }],
            ["write_fraction_value", () => { const b = Buffer.alloc(2); return b.writeUIntBE(1.9, 0, 2) + ":" + b.toString("hex"); }],
            ["write_nan_value", () => { const b = Buffer.alloc(2); return b.writeUIntBE(NaN, 0, 2) + ":" + b.toString("hex"); }],
            ["write_string_value", () => { const b = Buffer.alloc(2); return b.writeUIntBE("1", 0, 2) + ":" + b.toString("hex"); }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "types:readUIntBE:function,readUIntLE:function,writeUIntBE:function,writeUIntLE:function|read_be_1:18|read_be_2:4660|read_be_6:1108152157446|read_le_6:6618611909121|write_be_6:6:010203040506|write_le_6:6:060504030201|read_len_zero:RangeError|read_len_seven:RangeError|read_len_fraction:RangeError|read_len_string:TypeError|read_len_null:TypeError|read_offset_null:TypeError|write_oob_value:RangeError|write_fraction_value:2:0001|write_nan_value:2:0000|write_string_value:2:0001"
    );
}

#[test]
fn global_buffer_lowercase_variable_width_uint_aliases_match_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["types", () => ["readUintBE", "readUintLE", "writeUintBE", "writeUintLE"].map((name) => name + ":" + typeof Buffer.prototype[name]).join(",")],
            ["read_be_6", () => Buffer.from([1, 2, 3, 4, 5, 6]).readUintBE(0, 6)],
            ["read_le_6", () => Buffer.from([1, 2, 3, 4, 5, 6]).readUintLE(0, 6)],
            ["write_be_6", () => { const b = Buffer.alloc(6); return b.writeUintBE(0x010203040506, 0, 6) + ":" + b.toString("hex"); }],
            ["write_le_6", () => { const b = Buffer.alloc(6); return b.writeUintLE(0x010203040506, 0, 6) + ":" + b.toString("hex"); }],
            ["read_len_string", () => Buffer.from([1, 2]).readUintBE(0, "2")],
            ["write_oob", () => { const b = Buffer.alloc(2); return b.writeUintBE(0x10000, 0, 2) + ":" + b.toString("hex"); }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "types:readUintBE:function,readUintLE:function,writeUintBE:function,writeUintLE:function|read_be_6:1108152157446|read_le_6:6618611909121|write_be_6:6:010203040506|write_le_6:6:060504030201|read_len_string:TypeError|write_oob:RangeError"
    );
}

#[test]
fn global_buffer_biguint64_integer_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["types", () => ["readBigUInt64BE", "readBigUInt64LE", "readBigUint64BE", "readBigUint64LE", "writeBigUInt64BE", "writeBigUInt64LE", "writeBigUint64BE", "writeBigUint64LE"].map((name) => name + ":" + typeof Buffer.prototype[name]).join(",")],
            ["read_be", () => Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]).readBigUInt64BE(0)],
            ["read_le", () => Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]).readBigUInt64LE(0)],
            ["read_alias", () => Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]).readBigUint64BE(0)],
            ["write_be", () => { const b = Buffer.alloc(8); return b.writeBigUInt64BE(0x0102030405060708n, 0) + ":" + b.toString("hex"); }],
            ["write_le", () => { const b = Buffer.alloc(8); return b.writeBigUInt64LE(0x0102030405060708n, 0) + ":" + b.toString("hex"); }],
            ["write_alias", () => { const b = Buffer.alloc(8); return b.writeBigUint64BE(0x0102030405060708n, 0) + ":" + b.toString("hex"); }],
            ["read_offset_null", () => Buffer.alloc(8).readBigUInt64BE(null)],
            ["read_oob", () => Buffer.alloc(7).readBigUInt64BE(0)],
            ["write_number", () => { const b = Buffer.alloc(8); return b.writeBigUInt64BE(1, 0) + ":" + b.toString("hex"); }],
            ["write_string", () => { const b = Buffer.alloc(8); return b.writeBigUInt64BE("1", 0) + ":" + b.toString("hex"); }],
            ["write_negative", () => { const b = Buffer.alloc(8); return b.writeBigUInt64BE(-1n, 0) + ":" + b.toString("hex"); }],
            ["write_too_high", () => { const b = Buffer.alloc(8); return b.writeBigUInt64BE(0x10000000000000000n, 0) + ":" + b.toString("hex"); }],
            ["write_offset_null", () => { const b = Buffer.alloc(8); return b.writeBigUInt64BE(1n, null) + ":" + b.toString("hex"); }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "types:readBigUInt64BE:function,readBigUInt64LE:function,readBigUint64BE:function,readBigUint64LE:function,writeBigUInt64BE:function,writeBigUInt64LE:function,writeBigUint64BE:function,writeBigUint64LE:function|read_be:72623859790382856|read_le:578437695752307201|read_alias:72623859790382856|write_be:8:0102030405060708|write_le:8:0807060504030201|write_alias:8:0102030405060708|read_offset_null:TypeError|read_oob:RangeError|write_number:TypeError|write_string:TypeError|write_negative:RangeError|write_too_high:RangeError|write_offset_null:TypeError"
    );
}

#[test]
fn global_buffer_bigint64_integer_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["types", () => ["readBigInt64BE", "readBigInt64LE", "writeBigInt64BE", "writeBigInt64LE"].map((name) => name + ":" + typeof Buffer.prototype[name]).join(",")],
            ["read_be_pos", () => Buffer.from([0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).readBigInt64BE(0)],
            ["read_be_neg", () => Buffer.from([0x80, 0, 0, 0, 0, 0, 0, 0]).readBigInt64BE(0)],
            ["read_le_neg", () => Buffer.from([0, 0, 0, 0, 0, 0, 0, 0x80]).readBigInt64LE(0)],
            ["read_le_minus1", () => Buffer.from([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).readBigInt64LE(0)],
            ["write_be_neg", () => { const b = Buffer.alloc(8); return b.writeBigInt64BE(-0x0102030405060708n, 0) + ":" + b.toString("hex"); }],
            ["write_le_neg", () => { const b = Buffer.alloc(8); return b.writeBigInt64LE(-0x0102030405060708n, 0) + ":" + b.toString("hex"); }],
            ["write_be_pos", () => { const b = Buffer.alloc(8); return b.writeBigInt64BE(0x0102030405060708n, 0) + ":" + b.toString("hex"); }],
            ["read_offset_null", () => Buffer.alloc(8).readBigInt64BE(null)],
            ["read_oob", () => Buffer.alloc(7).readBigInt64BE(0)],
            ["write_number", () => { const b = Buffer.alloc(8); return b.writeBigInt64BE(1, 0) + ":" + b.toString("hex"); }],
            ["write_string", () => { const b = Buffer.alloc(8); return b.writeBigInt64BE("1", 0) + ":" + b.toString("hex"); }],
            ["write_low_oob", () => { const b = Buffer.alloc(8); return b.writeBigInt64BE(-0x8000000000000001n, 0) + ":" + b.toString("hex"); }],
            ["write_high_oob", () => { const b = Buffer.alloc(8); return b.writeBigInt64BE(0x8000000000000000n, 0) + ":" + b.toString("hex"); }],
            ["write_offset_null", () => { const b = Buffer.alloc(8); return b.writeBigInt64BE(1n, null) + ":" + b.toString("hex"); }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "types:readBigInt64BE:function,readBigInt64LE:function,writeBigInt64BE:function,writeBigInt64LE:function|read_be_pos:9223372036854775807|read_be_neg:-9223372036854775808|read_le_neg:-9223372036854775808|read_le_minus1:-1|write_be_neg:8:fefdfcfbfaf9f8f8|write_le_neg:8:f8f8f9fafbfcfdfe|write_be_pos:8:0102030405060708|read_offset_null:TypeError|read_oob:RangeError|write_number:TypeError|write_string:TypeError|write_low_oob:RangeError|write_high_oob:RangeError|write_offset_null:TypeError"
    );
}

#[test]
fn global_buffer_floating_point_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["types", () => ["readFloatBE", "readFloatLE", "readDoubleBE", "readDoubleLE", "writeFloatBE", "writeFloatLE", "writeDoubleBE", "writeDoubleLE"].map((name) => name + ":" + typeof Buffer.prototype[name]).join(",")],
            ["read_float_be", () => Buffer.from([0x3f, 0xc0, 0, 0]).readFloatBE(0)],
            ["read_float_le", () => Buffer.from([0, 0, 0xc0, 0x3f]).readFloatLE(0)],
            ["read_float_nan", () => Number.isNaN(Buffer.from([0x7f, 0xc0, 0, 0]).readFloatBE(0))],
            ["read_double_be", () => Buffer.from([0x40, 0x09, 0x21, 0xfb, 0x54, 0x44, 0x2d, 0x18]).readDoubleBE(0)],
            ["read_double_le", () => Buffer.from([0x18, 0x2d, 0x44, 0x54, 0xfb, 0x21, 0x09, 0x40]).readDoubleLE(0)],
            ["write_float_be", () => { const b = Buffer.alloc(4); return b.writeFloatBE(1.5, 0) + ":" + b.toString("hex"); }],
            ["write_float_le", () => { const b = Buffer.alloc(4); return b.writeFloatLE(1.5, 0) + ":" + b.toString("hex"); }],
            ["write_float_string", () => { const b = Buffer.alloc(4); return b.writeFloatBE("1.5", 0) + ":" + b.toString("hex"); }],
            ["write_float_nan", () => { const b = Buffer.alloc(4); return b.writeFloatBE(NaN, 0) + ":" + b.toString("hex"); }],
            ["write_double_be", () => { const b = Buffer.alloc(8); return b.writeDoubleBE(Math.PI, 0) + ":" + b.toString("hex"); }],
            ["write_double_le", () => { const b = Buffer.alloc(8); return b.writeDoubleLE(Math.PI, 0) + ":" + b.toString("hex"); }],
            ["write_double_string", () => { const b = Buffer.alloc(8); return b.writeDoubleBE("3.25", 0) + ":" + b.toString("hex"); }],
            ["read_offset_null", () => Buffer.alloc(4).readFloatBE(null)],
            ["read_float_oob", () => Buffer.alloc(3).readFloatBE(0)],
            ["read_double_oob", () => Buffer.alloc(7).readDoubleLE(0)],
            ["write_float_offset_null", () => { const b = Buffer.alloc(4); return b.writeFloatBE(1.5, null) + ":" + b.toString("hex"); }],
            ["write_double_offset_null", () => { const b = Buffer.alloc(8); return b.writeDoubleBE(1.5, null) + ":" + b.toString("hex"); }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "types:readFloatBE:function,readFloatLE:function,readDoubleBE:function,readDoubleLE:function,writeFloatBE:function,writeFloatLE:function,writeDoubleBE:function,writeDoubleLE:function|read_float_be:1.5|read_float_le:1.5|read_float_nan:true|read_double_be:3.141592653589793|read_double_le:3.141592653589793|write_float_be:4:3fc00000|write_float_le:4:0000c03f|write_float_string:4:3fc00000|write_float_nan:4:7fc00000|write_double_be:8:400921fb54442d18|write_double_le:8:182d4454fb210940|write_double_string:8:400a000000000000|read_offset_null:TypeError|read_float_oob:RangeError|read_double_oob:RangeError|write_float_offset_null:TypeError|write_double_offset_null:TypeError"
    );
}

#[test]
fn global_buffer_variable_width_signed_integer_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["types", () => ["readIntBE", "readIntLE", "writeIntBE", "writeIntLE"].map((name) => name + ":" + typeof Buffer.prototype[name]).join(",")],
            ["read_be_pos_1", () => Buffer.from([0x7f]).readIntBE(0, 1)],
            ["read_be_neg_1", () => Buffer.from([0x80]).readIntBE(0, 1)],
            ["read_be_neg_2", () => Buffer.from([0xff, 0xfe]).readIntBE(0, 2)],
            ["read_be_pos_6", () => Buffer.from([0x7f, 0xff, 0xff, 0xff, 0xff, 0xff]).readIntBE(0, 6)],
            ["read_be_neg_6", () => Buffer.from([0x80, 0, 0, 0, 0, 0]).readIntBE(0, 6)],
            ["read_le_neg_6", () => Buffer.from([0, 0, 0, 0, 0, 0x80]).readIntLE(0, 6)],
            ["write_be_neg_6", () => { const b = Buffer.alloc(6); return b.writeIntBE(-0x010203040506, 0, 6) + ":" + b.toString("hex"); }],
            ["write_le_neg_6", () => { const b = Buffer.alloc(6); return b.writeIntLE(-0x010203040506, 0, 6) + ":" + b.toString("hex"); }],
            ["read_len_zero", () => Buffer.from([1]).readIntBE(0, 0)],
            ["read_len_seven", () => Buffer.from([1, 2, 3, 4, 5, 6, 7]).readIntBE(0, 7)],
            ["read_len_fraction", () => Buffer.from([1, 2]).readIntBE(0, 1.9)],
            ["read_len_string", () => Buffer.from([1, 2]).readIntBE(0, "2")],
            ["read_offset_null", () => Buffer.from([1, 2]).readIntBE(null, 1)],
            ["write_high_oob", () => { const b = Buffer.alloc(2); return b.writeIntBE(0x8000, 0, 2) + ":" + b.toString("hex"); }],
            ["write_low_oob", () => { const b = Buffer.alloc(2); return b.writeIntBE(-0x8001, 0, 2) + ":" + b.toString("hex"); }],
            ["write_fraction_value", () => { const b = Buffer.alloc(2); return b.writeIntBE(-1.9, 0, 2) + ":" + b.toString("hex"); }],
            ["write_nan_value", () => { const b = Buffer.alloc(2); return b.writeIntBE(NaN, 0, 2) + ":" + b.toString("hex"); }],
            ["write_string_value", () => { const b = Buffer.alloc(2); return b.writeIntBE("-1", 0, 2) + ":" + b.toString("hex"); }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "types:readIntBE:function,readIntLE:function,writeIntBE:function,writeIntLE:function|read_be_pos_1:127|read_be_neg_1:-128|read_be_neg_2:-2|read_be_pos_6:140737488355327|read_be_neg_6:-140737488355328|read_le_neg_6:-140737488355328|write_be_neg_6:6:fefdfcfbfafa|write_le_neg_6:6:fafafbfcfdfe|read_len_zero:RangeError|read_len_seven:RangeError|read_len_fraction:RangeError|read_len_string:TypeError|read_offset_null:TypeError|write_high_oob:RangeError|write_low_oob:RangeError|write_fraction_value:2:ffff|write_nan_value:2:0000|write_string_value:2:ffff"
    );
}

#[test]
fn global_buffer_integer_offset_argument_validation_matches_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const readCases = [
            ["read_null", () => Buffer.from([1, 2, 3, 4]).readUInt8(null)],
            ["read_true", () => Buffer.from([1, 2, 3, 4]).readUInt8(true)],
            ["read_object", () => Buffer.from([1, 2, 3, 4]).readUInt8({ valueOf() { return 1; } })],
            ["read_array", () => Buffer.from([1, 2, 3, 4]).readUInt8([1])],
            ["read_fraction", () => Buffer.from([1, 2, 3, 4]).readUInt8(1.9)],
            ["read_nan", () => Buffer.from([1, 2, 3, 4]).readUInt8(NaN)],
            ["read_bad_string", () => Buffer.from([1, 2, 3, 4]).readUInt8("bad")],
            ["read_undefined", () => Buffer.from([1, 2, 3, 4]).readUInt8(undefined)],
        ];
        const writeCases = [
            ["write_null", () => { const b = Buffer.alloc(4); return b.writeUInt8(9, null) + ":" + b.toString("hex"); }],
            ["write_true", () => { const b = Buffer.alloc(4); return b.writeUInt8(9, true) + ":" + b.toString("hex"); }],
            ["write_object", () => { const b = Buffer.alloc(4); return b.writeUInt8(9, { valueOf() { return 1; } }) + ":" + b.toString("hex"); }],
            ["write_array", () => { const b = Buffer.alloc(4); return b.writeUInt8(9, [1]) + ":" + b.toString("hex"); }],
            ["write_fraction", () => { const b = Buffer.alloc(4); return b.writeUInt8(9, 1.9) + ":" + b.toString("hex"); }],
            ["write_nan", () => { const b = Buffer.alloc(4); return b.writeUInt8(9, NaN) + ":" + b.toString("hex"); }],
            ["write_bad_string", () => { const b = Buffer.alloc(4); return b.writeUInt8(9, "bad") + ":" + b.toString("hex"); }],
            ["write_undefined", () => { const b = Buffer.alloc(4); return b.writeUInt8(9, undefined) + ":" + b.toString("hex"); }],
        ];
        return readCases.concat(writeCases).map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "read_null:TypeError|read_true:TypeError|read_object:TypeError|read_array:TypeError|read_fraction:RangeError|read_nan:RangeError|read_bad_string:TypeError|read_undefined:1|write_null:TypeError|write_true:TypeError|write_object:TypeError|write_array:TypeError|write_fraction:RangeError|write_nan:RangeError|write_bad_string:TypeError|write_undefined:1:09000000"
    );
}

#[test]
fn global_buffer_unknown_encoding_strict_entrypoints_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const outcomes = [];
        try { Buffer.from("abc", "bogus"); outcomes.push("from:ok"); }
        catch (e) { outcomes.push("from:" + e.message); }
        try { Buffer.from([0x61]).toString("bogus"); outcomes.push("toString:ok"); }
        catch (e) { outcomes.push("toString:" + e.message); }
        try { Buffer.alloc(3).write("abc", 0, 3, "bogus"); outcomes.push("write:ok"); }
        catch (e) { outcomes.push("write:" + e.message); }
        outcomes.push("byteLength:" + Buffer.byteLength("abc", "bogus"));
        outcomes.push("isEncoding:" + Buffer.isEncoding("bogus"));
        return outcomes.join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "from:Unknown encoding: bogus|toString:Unknown encoding: bogus|write:Unknown encoding: bogus|byteLength:3|isEncoding:false"
    );
}

#[test]
fn global_buffer_encoding_specific_slice_write_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["types", () => ["utf8Slice", "asciiSlice", "latin1Slice", "hexSlice", "base64Slice", "base64urlSlice", "ucs2Slice", "utf8Write", "asciiWrite", "latin1Write", "hexWrite", "base64Write", "base64urlWrite", "ucs2Write"].map((name) => name + ":" + typeof Buffer.prototype[name]).join(",")],
            ["is_base64url", () => Buffer.isEncoding("base64url")],
            ["from_base64url", () => Buffer.from("aGVsbG8_", "base64url").toString("hex")],
            ["to_base64url", () => Buffer.from("hello?").toString("base64url")],
            ["byte_length_base64url", () => Buffer.byteLength("aGVsbG8_", "base64url")],
            ["utf8Slice", () => Buffer.from("hi").utf8Slice(0, 2)],
            ["asciiSlice", () => Buffer.from([0xc1, 0x42]).asciiSlice(0, 2)],
            ["latin1Slice", () => Buffer.from([0xc1, 0x42]).latin1Slice(0, 2)],
            ["hexSlice", () => Buffer.from([0, 15, 16, 255]).hexSlice(0, 4)],
            ["base64Slice", () => Buffer.from("hello").base64Slice(0, 5)],
            ["base64urlSlice", () => Buffer.from("hello?").base64urlSlice(0, 6)],
            ["ucs2Slice", () => Buffer.from([0x41, 0, 0x42, 0]).ucs2Slice(0, 4)],
            ["utf8Write", () => { const b = Buffer.alloc(8); return b.utf8Write("hi", 1, 3) + ":" + b.toString("hex"); }],
            ["asciiWrite", () => { const b = Buffer.alloc(4); return b.asciiWrite(String.fromCharCode(0x100) + "B", 0, 4) + ":" + b.toString("hex"); }],
            ["latin1Write", () => { const b = Buffer.alloc(4); return b.latin1Write(String.fromCharCode(0xc1) + "B", 0, 4) + ":" + b.toString("hex"); }],
            ["hexWrite", () => { const b = Buffer.alloc(4); return b.hexWrite("000f10ff", 0, 4) + ":" + b.toString("hex"); }],
            ["base64Write", () => { const b = Buffer.alloc(8); return b.base64Write("aGVsbG8=", 0, 8) + ":" + b.toString("hex"); }],
            ["base64urlWrite", () => { const b = Buffer.alloc(8); return b.base64urlWrite("aGVsbG8_", 0, 8) + ":" + b.toString("hex"); }],
            ["ucs2Write", () => { const b = Buffer.alloc(8); return b.ucs2Write("AB", 0, 8) + ":" + b.toString("hex"); }],
            ["slice_null_start", () => Buffer.from("hello").utf8Slice(null, 4)],
            ["write_null_offset", () => { const b = Buffer.alloc(4); return b.utf8Write("hi", null, 2) + ":" + b.toString("hex"); }],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "types:utf8Slice:function,asciiSlice:function,latin1Slice:function,hexSlice:function,base64Slice:function,base64urlSlice:function,ucs2Slice:function,utf8Write:function,asciiWrite:function,latin1Write:function,hexWrite:function,base64Write:function,base64urlWrite:function,ucs2Write:function|is_base64url:true|from_base64url:68656c6c6f3f|to_base64url:aGVsbG8_|byte_length_base64url:6|utf8Slice:hi|asciiSlice:AB|latin1Slice:\u{c1}B|hexSlice:000f10ff|base64Slice:aGVsbG8=|base64urlSlice:aGVsbG8_|ucs2Slice:AB|utf8Write:2:0068690000000000|asciiWrite:2:00420000|latin1Write:2:c1420000|hexWrite:4:000f10ff|base64Write:5:68656c6c6f000000|base64urlWrite:6:68656c6c6f3f0000|ucs2Write:4:4100420000000000|slice_null_start:hell|write_null_offset:2:68690000"
    );
}

#[test]
fn global_buffer_inspect_and_locale_string_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const cases = [
            ["types", () => ["inspect", "toLocaleString"].map((name) => name + ":" + typeof Buffer.prototype[name]).join(",")],
            ["locale_utf8", () => Buffer.from("hi").toLocaleString()],
            ["locale_hex_args", () => Buffer.from([0, 15, 16, 255]).toLocaleString("hex", 1, 3)],
            ["inspect_empty", () => Buffer.alloc(0).inspect()],
            ["inspect_small", () => Buffer.from([0, 15, 16, 255]).inspect()],
            ["inspect_ellipsis", () => Buffer.from(Array.from({ length: 60 }, (_, i) => i)).inspect()],
            ["inspect_custom_symbol", () => Buffer.prototype[Symbol.for("nodejs.util.inspect.custom")] === Buffer.prototype.inspect],
        ];
        return cases.map(([label, run]) => {
            try {
                return label + ":" + run();
            } catch (e) {
                return label + ":" + e.name;
            }
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "types:inspect:function,toLocaleString:function|locale_utf8:hi|locale_hex_args:0f10|inspect_empty:<Buffer >|inspect_small:<Buffer 00 0f 10 ff>|inspect_ellipsis:<Buffer 00 01 02 03 04 05 06 07 08 09 0a 0b 0c 0d 0e 0f 10 11 12 13 14 15 16 17 18 19 1a 1b 1c 1d 1e 1f 20 21 22 23 24 25 26 27 28 29 2a 2b 2c 2d 2e 2f 30 31 ... 10 more bytes>|inspect_custom_symbol:true"
    );
}

#[test]
fn global_buffer_to_string_range_vectors_match_node() {
    let result = eval_global_buffer(
        r#"(() => {
        const b = Buffer.from([0x61, 0x62, 0x63, 0x64]);
        const cases = [
            ["default", () => b.toString()],
            ["start1", () => b.toString("utf8", 1)],
            ["range1_3", () => b.toString("utf8", 1, 3)],
            ["neg_start", () => b.toString("utf8", -2)],
            ["neg_end", () => b.toString("utf8", 0, -1)],
            ["start_oob", () => b.toString("utf8", 99)],
            ["end_oob", () => b.toString("utf8", 1, 99)],
            ["start_gt_end", () => b.toString("utf8", 3, 1)],
            ["fraction", () => b.toString("utf8", 1.9, 3.9)],
            ["nan", () => b.toString("utf8", NaN, 3)],
            ["inf", () => b.toString("utf8", Infinity)],
            ["neg_inf_start", () => b.toString("utf8", -Infinity, 2)],
            ["neg_inf_end", () => b.toString("utf8", 0, -Infinity)],
            ["null_start", () => b.toString("utf8", null, 2)],
            ["undefined_end", () => b.toString("utf8", 1, undefined)],
        ];
        return cases.map(([label, run]) => label + ":" + run()).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "default:abcd|start1:bcd|range1_3:bc|neg_start:abcd|neg_end:|start_oob:|end_oob:bcd|start_gt_end:|fraction:bc|nan:abc|inf:|neg_inf_start:ab|neg_inf_end:|null_start:ab|undefined_end:bcd"
    );
}

#[test]
fn global_buffer_utf16le_alias_encodings_match_node_vectors() {
    let result = eval_global_buffer(
        r#"(() => {
        const aliases = ["utf16le", "utf-16le", "ucs2", "ucs-2"];
        return aliases.map((enc) => {
            const from = Buffer.from("A\u2603", enc);
            const written = Buffer.alloc(4);
            const bytesWritten = written.write("A\u2603", 0, 4, enc);
            const codes = from.toString(enc).split("").map((ch) => ch.charCodeAt(0).toString(16)).join(",");
            return [enc, from.toString("hex"), codes, Buffer.byteLength("A\u2603", enc), Buffer.isEncoding(enc), bytesWritten, written.toString("hex")].join(":");
        }).join("|");
    })()"#,
    );
    assert_eq!(
        result,
        "utf16le:41000326:41,2603:4:true:4:41000326|utf-16le:41000326:41,2603:4:true:4:41000326|ucs2:41000326:41,2603:4:true:4:41000326|ucs-2:41000326:41,2603:4:true:4:41000326"
    );
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
    let result = eval_buffer(r#"Buffer.compare(Buffer.from("a"), Buffer.from("b"))"#);
    assert_eq!(result, "-1");
}
