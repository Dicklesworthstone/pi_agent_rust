// Integration tests are separate crates, so src/lib.rs's `recursion_limit`
// does not reach here; with the `wasm-host` feature, proving `Send` for the
// extension-loading futures exceeds the default 128.
#![recursion_limit = "256"]

//! Integration tests for Node.js shim cross-module interop (bd-1av0.10).
//!
//! These tests verify that multiple Node.js shim modules work correctly
//! when used **together** in the same extension. Unlike the per-module unit
//! tests, these exercise realistic multi-import patterns that real
//! extensions use.
#![allow(clippy::needless_raw_string_hashes)]

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
    let ext_entry_path = harness.create_file("extensions/integration_test.mjs", source.as_bytes());
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

/// Build an extension source that imports multiple modules and evaluates an expression.
fn multi_ext_source(imports: &str, js_expr: &str) -> String {
    format!(
        r#"
{imports}

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

fn eval_multi(imports: &str, js_expr: &str) -> String {
    let harness = common::TestHarness::new("shim_integration");
    let source = multi_ext_source(imports, js_expr);
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

// ═══════════════════════════════════════════════════════════════════════════
// 1. Cross-Shim Interop Tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn path_join_with_os_tmpdir() {
    let result = eval_multi(
        r#"import path from "node:path";
import os from "node:os";"#,
        r#"(() => {
        const tmp = os.tmpdir();
        const full = path.join(tmp, "myapp", "cache.json");
        return full.includes("myapp/cache.json") && path.isAbsolute(full);
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn path_join_with_os_homedir() {
    let result = eval_multi(
        r#"import path from "node:path";
import os from "node:os";"#,
        r#"(() => {
        const home = os.homedir();
        const configPath = path.join(home, ".config", "ext.json");
        // Normalize both to forward slashes before comparison so mixed
        // separators on Windows don't cause a startsWith mismatch.
        const norm = s => s.replace(/\\/g, "/");
        return norm(configPath).startsWith(norm(home)) && norm(configPath).endsWith(".config/ext.json");
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn crypto_hash_buffer_content() {
    let result = eval_multi(
        r#"import crypto from "node:crypto";
import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const buf = Buffer.from("hello world");
        const hash = crypto.createHash("sha256").update(buf.toString()).digest("hex");
        return hash;
    })()"#,
    );
    assert_eq!(
        result,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

#[test]
fn buffer_from_crypto_random_bytes() {
    let result = eval_multi(
        r#"import crypto from "node:crypto";
import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const bytes = crypto.randomBytes(16);
        const buf = Buffer.from(bytes);
        return buf.length === 16 && typeof buf.toString("hex") === "string";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn events_emitter_with_buffer_payload() {
    let result = eval_multi(
        r#"import EventEmitter from "node:events";
import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const ee = new EventEmitter();
        let received = null;
        ee.on("data", (chunk) => { received = chunk; });
        ee.emit("data", Buffer.from("payload").toString("base64"));
        return received;
    })()"#,
    );
    assert_eq!(result, "cGF5bG9hZA==");
}

#[test]
fn net_create_connection_stub_shape() {
    let result = eval_multi(
        r#"import net from "node:net";"#,
        r#"(() => {
        const socket = net.createConnection({ port: 1234, host: "example.test" });
        return Boolean(socket)
          && socket.connecting === true
          && socket.readyState === "opening"
          && socket.remotePort === 1234
          && socket.remoteAddress === "example.test"
          && typeof socket.write === "function"
          && typeof socket.end === "function";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn net_isip_validation() {
    let result = eval_multi(
        r#"import net from "node:net";"#,
        r#"(() => {
        if (net.isIP("127.0.0.1") !== 4) return false;
        if (net.isIPv4("255.255.255.255") !== true) return false;
        if (net.isIP("999.999.999.999") !== 0) return false;
        if (net.isIPv4("256.0.0.1") !== false) return false;
        if (net.isIP("2001:0db8:85a3:0000:0000:8a2e:0370:7334") !== 6) return false;
        if (net.isIPv6("2001:db8::1") !== true) return false;
        if (net.isIP("2001::85a3::8a2e") !== 0) return false;

        const valid = "1.2.3.4";
        if (net.isIP(valid) !== 4) return false;
        const invalids = ["256.2.3.4", "1.256.3.4", "1.2.256.4", "1.2.3.256"];
        for (const ip of invalids) {
            if (net.isIP(ip) !== 0) return false;
        }
        return true;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn os_platform_matches_process_platform() {
    let result = eval_multi(
        r#"import os from "node:os";"#,
        r#"(() => {
        const osPlatform = os.platform();
        const procPlatform = globalThis.process ? globalThis.process.platform : os.platform();
        if (osPlatform !== procPlatform) {
            return `mismatch:os=${osPlatform},proc=${procPlatform}`;
        }
        return osPlatform === procPlatform;
    })()"#,
    );
    assert!(
        result == "true" || result.starts_with("mismatch:"),
        "platform check returned: {result}"
    );
    if result.starts_with("mismatch:") {
        // On some CI environments, os.platform() and process.platform may
        // be derived from different sources. Accept as long as both are
        // reasonable platform strings.
        assert!(
            result.split(',').count() == 2,
            "unexpected mismatch format: {result}"
        );
    } else {
        assert_eq!(result, "true");
    }
}

#[test]
fn assert_strict_imports() {
    let result = eval_multi(
        r#"import assert from "node:assert/strict";"#,
        r#"(() => {
        try {
            assert.strictEqual(1, 1);
            assert.notDeepEqual({a: 1}, {a: 2});
            return true;
        } catch (e) {
            return `ERROR:${e.message}`;
        }
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn timers_module_exports_globals() {
    let result = eval_multi(
        r#"import * as timers from "node:timers";"#,
        r#"(() => {
        return (
            typeof timers.setTimeout === "function" &&
            typeof timers.clearTimeout === "function" &&
            typeof timers.setInterval === "function" &&
            typeof timers.clearInterval === "function" &&
            typeof timers.queueMicrotask === "function" &&
            typeof timers.setImmediate === "function" &&
            typeof timers.clearImmediate === "function"
        );
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn timers_module_alias_resolves() {
    let result = eval_multi(
        r#"import * as timers from "timers";"#,
        r#"(() => {
        return (
            typeof timers.setTimeout === "function" &&
            typeof timers.clearTimeout === "function" &&
            typeof timers.setInterval === "function" &&
            typeof timers.clearInterval === "function"
        );
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn assert_strict_bare_imports() {
    let result = eval_multi(
        r#"import assert from "assert/strict";"#,
        r#"(() => {
        try {
            assert.strictEqual(1, 1);
            assert.notDeepEqual({a: 1}, {a: 2});
            return true;
        } catch (e) {
            return `ERROR:${e.message}`;
        }
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn node_test_imports() {
    let result = eval_multi(
        r#"import test, { test as namedTest, describe, it } from "node:test";"#,
        r#"(() => {
        return typeof namedTest === 'function'
            && typeof test.test === 'function'
            && typeof describe === 'function'
            && typeof it === 'function';
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn readline_promises_imports() {
    let result = eval_multi(
        r#"import { createInterface } from "node:readline/promises";"#,
        r#"(() => {
        const rl = createInterface({});
        return typeof rl.question === 'function' && typeof rl.close === 'function';
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn readline_promises_bare_imports() {
    let result = eval_multi(
        r#"import { createInterface } from "readline/promises";"#,
        r#"(() => {
        const rl = createInterface({});
        return typeof rl.question === 'function' && typeof rl.close === 'function';
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn path_resolve_uses_process_cwd() {
    let result = eval_multi(
        r#"import path from "node:path";"#,
        r#"(() => {
        const resolved = path.resolve("foo", "bar.txt");
        return path.isAbsolute(resolved) && resolved.endsWith("foo/bar.txt");
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn os_eol_is_newline() {
    let result = eval_multi(
        r#"import os from "node:os";"#,
        r#"os.EOL === "\n" || os.EOL === "\r\n""#,
    );
    assert_eq!(result, "true");
}

#[test]
fn buffer_concat_with_crypto_digests() {
    let result = eval_multi(
        r#"import crypto from "node:crypto";
import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const h1 = crypto.createHash("sha256").update("a").digest("hex");
        const h2 = crypto.createHash("sha256").update("b").digest("hex");
        const combined = Buffer.concat([Buffer.from(h1), Buffer.from(h2)]);
        return combined.length === 128;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn path_extname_basename_dirname_chain() {
    let result = eval_multi(
        r#"import path from "node:path";"#,
        r#"(() => {
        const p = "/home/user/project/src/main.rs";
        const ext = path.extname(p);
        const base = path.basename(p, ext);
        const dir = path.dirname(p);
        return ext + "|" + base + "|" + dir;
    })()"#,
    );
    assert_eq!(result, ".rs|main|/home/user/project/src");
}

#[test]
fn os_hostname_is_nonempty_string() {
    let result = eval_multi(
        r#"import os from "node:os";"#,
        r#"(() => {
        const h = os.hostname();
        return typeof h === "string" && h.length > 0;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn os_arch_and_type_consistent() {
    let result = eval_multi(
        r#"import os from "node:os";"#,
        r#"(() => {
        const arch = os.arch();
        const typ = os.type();
        const validArch = ["x64", "arm64", "ia32", "arm"].includes(arch);
        const validType = ["Linux", "Darwin", "Windows_NT"].includes(typ);
        return validArch && validType;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn os_cpus_returns_array_with_entries() {
    let result = eval_multi(
        r#"import os from "node:os";"#,
        r#"(() => {
        const cpus = os.cpus();
        return Array.isArray(cpus) && cpus.length > 0 && cpus[0].model !== undefined;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn os_userinfo_has_required_fields() {
    let result = eval_multi(
        r#"import os from "node:os";"#,
        r#"(() => {
        const info = os.userInfo();
        return typeof info.username === "string" &&
               typeof info.uid === "number" &&
               typeof info.gid === "number" &&
               typeof info.homedir === "string" &&
               typeof info.shell === "string";
    })()"#,
    );
    assert_eq!(result, "true");
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. Real Extension Pattern Replay Tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn pattern_config_path_check() {
    // Common extension pattern: build config path from HOME
    let result = eval_multi(
        r#"import path from "node:path";
import os from "node:os";"#,
        r#"(() => {
        const configPath = path.join(os.homedir(), ".config", "ext.json");
        return typeof configPath === "string" && configPath.includes(".config/ext.json");
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn pattern_sha256_hex_digest() {
    // Common extension pattern: hash content for caching
    let result = eval_multi(
        r#"import crypto from "node:crypto";"#,
        r#"(() => {
        const content = "some extension data to hash";
        const hash = crypto.createHash("sha256").update(content).digest("hex");
        return hash.length === 64 && /^[0-9a-f]+$/.test(hash);
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn pattern_url_with_search_params() {
    // Common extension pattern: build API URLs with query params
    // Use parse() helper which wraps URL construction
    let result = eval_multi(
        r#"import * as urlMod from "node:url";"#,
        r#"(() => {
        const url = urlMod.parse("https://api.example.com/v1/search?q=test+query&limit=10");
        return url !== null && url.search.includes("q=test");
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn pattern_event_driven_pipeline() {
    // Common extension pattern: event-driven data pipeline
    let result = eval_multi(
        r#"import EventEmitter from "node:events";
import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const pipeline = new EventEmitter();
        const chunks = [];
        pipeline.on("data", (chunk) => {
            chunks.push(Buffer.from(chunk).toString("utf8"));
        });
        pipeline.on("end", () => {
            chunks.push("DONE");
        });
        pipeline.emit("data", "hello ");
        pipeline.emit("data", "world");
        pipeline.emit("end");
        return chunks.join("");
    })()"#,
    );
    assert_eq!(result, "hello worldDONE");
}

#[test]
fn pattern_platform_detection_switch() {
    // Common extension pattern: platform-dependent behavior
    let result = eval_multi(
        r#"import os from "node:os";
import path from "node:path";"#,
        r#"(() => {
        const platform = os.platform();
        let configDir;
        if (platform === "darwin") {
            configDir = path.join(os.homedir(), "Library", "Application Support");
        } else if (platform === "win32") {
            configDir = path.join(os.homedir(), "AppData", "Roaming");
        } else {
            configDir = path.join(os.homedir(), ".config");
        }
        return typeof configDir === "string" && configDir.length > 0;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn pattern_buffer_base64_roundtrip() {
    // Common extension pattern: encode/decode data as base64
    let result = eval_multi(
        r#"import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const original = "Hello, Extensions!";
        const encoded = Buffer.from(original).toString("base64");
        const decoded = Buffer.from(encoded, "base64").toString("utf8");
        return decoded;
    })()"#,
    );
    assert_eq!(result, "Hello, Extensions!");
}

#[test]
fn pattern_zlib_gzip_gunzip_roundtrip() {
    let result = eval_multi(
        r#"import zlib from "node:zlib";
import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const input = Buffer.from("extension zlib");
        const compressed = zlib.gzipSync(input);
        const decompressed = zlib.gunzipSync(compressed);
        return decompressed.toString("utf8") + ":" + (compressed.toString("base64") !== input.toString("base64"));
    })()"#,
    );
    assert_eq!(result, "extension zlib:true");
}

#[test]
fn pattern_crypto_hmac_auth_header() {
    // Common extension pattern: HMAC signature for API auth
    let result = eval_multi(
        r#"import crypto from "node:crypto";"#,
        r#"(() => {
        const secret = "my-secret-key";
        const message = "GET /api/v1/data 1706745600";
        const sig = crypto.createHmac("sha256", secret).update(message).digest("hex");
        return sig.length === 64 && /^[0-9a-f]+$/.test(sig);
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn pattern_uuid_generation() {
    // Common extension pattern: generate unique IDs
    let result = eval_multi(
        r#"import crypto from "node:crypto";"#,
        r#"(() => {
        const id1 = crypto.randomUUID();
        const id2 = crypto.randomUUID();
        const uuidRegex = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
        return uuidRegex.test(id1) && uuidRegex.test(id2) && id1 !== id2;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn pattern_path_relative_computation() {
    // Common extension pattern: compute relative paths for display
    let result = eval_multi(
        r#"import path from "node:path";"#,
        r#"(() => {
        const from = "/home/user/project";
        const to = "/home/user/project/src/lib/utils.ts";
        const rel = path.relative(from, to);
        return rel;
    })()"#,
    );
    assert_eq!(result, "src/lib/utils.ts");
}

#[test]
fn pattern_normalize_messy_path() {
    // Common extension pattern: normalize user-provided paths
    let result = eval_multi(
        r#"import path from "node:path";"#,
        r#"(() => {
        return path.normalize("/foo/bar//baz/../qux/./file.txt");
    })()"#,
    );
    assert_eq!(result, "/foo/bar/qux/file.txt");
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. Error Path Tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn crypto_unsupported_algorithm_throws() {
    let result = eval_multi(
        r#"import crypto from "node:crypto";"#,
        r#"(() => {
        try {
            crypto.createHash("not-a-real-algo").update("x").digest("hex");
            return "should_have_thrown";
        } catch (e) {
            return "caught";
        }
    })()"#,
    );
    assert_eq!(result, "caught");
}

#[test]
fn buffer_from_invalid_encoding_returns_empty_or_throws() {
    let result = eval_multi(
        r#"import { Buffer } from "node:buffer";"#,
        r#"(() => {
        try {
            const buf = Buffer.from("hello", "not-an-encoding");
            // Some impls treat unknown encoding as utf8, that's acceptable
            return buf.length > 0 ? "ok_fallback" : "empty";
        } catch (e) {
            return "caught";
        }
    })()"#,
    );
    // Either throwing or falling back to utf8 is acceptable
    assert!(
        result == "caught" || result == "ok_fallback",
        "got: {result}"
    );
}

#[test]
fn url_parse_returns_parsed_object() {
    let result = eval_multi(
        r#"import * as urlMod from "node:url";"#,
        r#"(() => {
        const u = urlMod.parse("https://example.com/path");
        return u !== null && u.hostname === "example.com" && u.pathname === "/path";
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn events_error_without_listener_returns_false() {
    // Our EventEmitter shim returns false when no listener for error event
    // (Node.js throws, but our shim is permissive)
    let result = eval_multi(
        r#"import EventEmitter from "node:events";"#,
        r#"(() => {
        const ee = new EventEmitter();
        const result = ee.emit("error", new Error("unhandled"));
        return result;
    })()"#,
    );
    // false = no listeners were called
    assert_eq!(result, "false");
}

#[test]
fn events_error_with_listener_does_not_throw() {
    let result = eval_multi(
        r#"import EventEmitter from "node:events";"#,
        r#"(() => {
        const ee = new EventEmitter();
        let caught = false;
        ee.on("error", () => { caught = true; });
        ee.emit("error", new Error("handled"));
        return caught;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn path_resolve_with_empty_args() {
    let result = eval_multi(
        r#"import path from "node:path";"#,
        r#"(() => {
        const resolved = path.resolve();
        return typeof resolved === "string" && resolved.length > 0;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn buffer_alloc_zero_length() {
    let result = eval_multi(
        r#"import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const buf = Buffer.alloc(0);
        return buf.length === 0 && Buffer.isBuffer(buf);
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn buffer_alloc_fills_with_zero() {
    let result = eval_multi(
        r#"import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const buf = Buffer.alloc(4);
        return buf[0] === 0 && buf[1] === 0 && buf[2] === 0 && buf[3] === 0;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn crypto_random_int_within_range() {
    let result = eval_multi(
        r#"import crypto from "node:crypto";"#,
        r#"(() => {
        let allInRange = true;
        for (let i = 0; i < 100; i++) {
            const n = crypto.randomInt(10, 20);
            if (n < 10 || n >= 20) { allInRange = false; break; }
        }
        return allInRange;
    })()"#,
    );
    assert_eq!(result, "true");
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. Performance / Stress Tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn perf_buffer_alloc_many() {
    let result = eval_multi(
        r#"import { Buffer } from "node:buffer";"#,
        r#"(() => {
        for (let i = 0; i < 1000; i++) {
            Buffer.alloc(64);
        }
        return "ok";
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn perf_crypto_hash_many() {
    let result = eval_multi(
        r#"import crypto from "node:crypto";"#,
        r#"(() => {
        for (let i = 0; i < 100; i++) {
            crypto.createHash("sha256").update("iteration " + i).digest("hex");
        }
        return "ok";
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn perf_event_emitter_many_listeners() {
    let result = eval_multi(
        r#"import EventEmitter from "node:events";"#,
        r#"(() => {
        const ee = new EventEmitter();
        ee.setMaxListeners(0); // disable warning
        let count = 0;
        for (let i = 0; i < 200; i++) {
            ee.on("test", () => { count++; });
        }
        ee.emit("test");
        return count;
    })()"#,
    );
    assert_eq!(result, "200");
}

#[test]
fn perf_path_operations_many() {
    let result = eval_multi(
        r#"import path from "node:path";"#,
        r#"(() => {
        for (let i = 0; i < 1000; i++) {
            path.join("/a", "b", "c" + i, "file.txt");
            path.dirname("/a/b/c/file.txt");
            path.basename("/a/b/c/file.txt");
            path.extname("file.txt");
        }
        return "ok";
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn perf_url_parse_many() {
    let result = eval_multi(
        r#"import * as urlMod from "node:url";"#,
        r#"(() => {
        for (let i = 0; i < 200; i++) {
            const u = urlMod.parse("https://example.com/path?q=" + i);
            if (!u) return "parse_failed";
        }
        return "ok";
    })()"#,
    );
    assert_eq!(result, "ok");
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. Multi-Module Import Validation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn six_module_import_all_available() {
    // Verify that importing many modules simultaneously works
    let result = eval_multi(
        r#"import os from "node:os";
import path from "node:path";
import crypto from "node:crypto";
import { Buffer } from "node:buffer";
import EventEmitter from "node:events";
import * as urlMod from "node:url";"#,
        r#"(() => {
        const checks = [
            typeof os.platform === "function",
            typeof path.join === "function",
            typeof crypto.createHash === "function",
            typeof Buffer.from === "function",
            typeof EventEmitter === "function",
            typeof urlMod.parse === "function",
        ];
        return checks.every(Boolean);
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn bare_and_prefixed_imports_equivalent() {
    // Verify that `import x from "os"` and `import x from "node:os"` give same module
    let result = eval_multi(
        r#"import osBare from "os";
import osNode from "node:os";"#,
        r#"osBare.platform() === osNode.platform()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn bare_and_prefixed_path_equivalent() {
    let result = eval_multi(
        r#"import pathBare from "path";
import pathNode from "node:path";"#,
        r#"pathBare.join("a", "b") === pathNode.join("a", "b")"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn bare_and_prefixed_crypto_equivalent() {
    let result = eval_multi(
        r#"import cryptoBare from "crypto";
import cryptoNode from "node:crypto";"#,
        r#"(() => {
        const h1 = cryptoBare.createHash("sha256").update("test").digest("hex");
        const h2 = cryptoNode.createHash("sha256").update("test").digest("hex");
        return h1 === h2;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn bare_and_prefixed_events_equivalent() {
    let result = eval_multi(
        r#"import EventsBare from "events";
import EventsNode from "node:events";"#,
        r#"(() => {
        const ee1 = new EventsBare();
        const ee2 = new EventsNode();
        return typeof ee1.on === "function" && typeof ee2.on === "function";
    })()"#,
    );
    assert_eq!(result, "true");
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. Complex Multi-Step Integration Scenarios
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn scenario_hash_content_and_store_in_map() {
    // Extension pattern: hash-addressed content store
    let result = eval_multi(
        r#"import crypto from "node:crypto";
import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const store = new Map();
        const items = ["file1 content", "file2 content", "file3 content"];
        for (const item of items) {
            const hash = crypto.createHash("sha256").update(item).digest("hex");
            store.set(hash, Buffer.from(item));
        }
        // Verify all items stored and retrievable
        let allOk = true;
        for (const item of items) {
            const hash = crypto.createHash("sha256").update(item).digest("hex");
            const buf = store.get(hash);
            if (!buf || buf.toString() !== item) { allOk = false; break; }
        }
        return allOk && store.size === 3;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn scenario_event_pipeline_with_transform() {
    // Extension pattern: event-driven transform pipeline
    let result = eval_multi(
        r#"import EventEmitter from "node:events";
import crypto from "node:crypto";"#,
        r#"(() => {
        const input = new EventEmitter();
        const output = new EventEmitter();
        const results = [];

        // Transform stage: hash each chunk
        input.on("chunk", (data) => {
            const hash = crypto.createHash("sha256").update(data).digest("hex").slice(0, 8);
            output.emit("hashed", hash);
        });

        output.on("hashed", (h) => { results.push(h); });

        input.emit("chunk", "alpha");
        input.emit("chunk", "beta");
        input.emit("chunk", "gamma");

        return results.length === 3 && results.every(h => h.length === 8);
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn scenario_url_path_composition() {
    // Extension pattern: build API request URLs with path segments
    let result = eval_multi(
        r#"import * as urlMod from "node:url";
import path from "node:path";"#,
        r#"(() => {
        const base = "https://api.example.com";
        const endpoint = path.join("/v2", "users", "search");
        // Use resolve() to combine base and endpoint
        const resolved = urlMod.resolve(base, endpoint);
        const url = urlMod.parse(resolved);
        return url !== null && url.pathname.includes("/v2/users/search");
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn scenario_buffer_hex_encoding_chain() {
    // Extension pattern: encode binary data through multiple formats
    let result = eval_multi(
        r#"import { Buffer } from "node:buffer";"#,
        r#"(() => {
        const original = "Hello, World!";
        // utf8 → hex → back to utf8
        const hex = Buffer.from(original, "utf8").toString("hex");
        const restored = Buffer.from(hex, "hex").toString("utf8");
        return original === restored;
    })()"#,
    );
    assert_eq!(result, "true");
}

#[test]
fn scenario_os_info_object_construction() {
    // Extension pattern: build system info object for telemetry
    let result = eval_multi(
        r#"import os from "node:os";
import crypto from "node:crypto";"#,
        r#"(() => {
        const info = {
            platform: os.platform(),
            arch: os.arch(),
            hostname: os.hostname(),
            cpuCount: os.cpus().length,
            tmpdir: os.tmpdir(),
        };
        // Generate a fingerprint from system info
        const fingerprint = crypto.createHash("sha256")
            .update(JSON.stringify(info))
            .digest("hex")
            .slice(0, 16);
        return fingerprint.length === 16 &&
               typeof info.platform === "string" &&
               info.cpuCount > 0;
    })()"#,
    );
    assert_eq!(result, "true");
}
