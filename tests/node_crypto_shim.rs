//! Unit tests for the node:crypto shim (bd-1av0.3).
//!
//! Tests verify that `createHash`, `createHmac`, `randomUUID`, `randomBytes`,
//! `randomInt`, `timingSafeEqual`, and `getHashes` produce output matching
//! Node.js semantics. Crypto operations delegate to real Rust crates (sha2,
//! sha1, md-5, hmac) via hostcalls registered on the `QuickJS` runtime.

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::PiJsRuntimeConfig;
use pi::tools::ToolRegistry;
use std::sync::Arc;

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Load a JS extension and return the manager.
fn load_ext(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_entry_path = harness.create_file("extensions/crypto_test.mjs", source.as_bytes());
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

/// Build an extension that computes a crypto expression and returns it via
/// the `agent_start` event handler.
fn crypto_ext_source(js_expr: &str) -> String {
    format!(
        r#"
import crypto from "node:crypto";
const {{
  createHash,
  createHmac,
  randomUUID,
  randomBytes,
  randomInt,
  timingSafeEqual,
  getHashes,
  pbkdf2Sync,
  pbkdf2,
  scryptSync,
  createCipheriv,
  createDecipheriv,
  sign,
  verify,
}} = crypto;

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

/// Evaluate a crypto JS expression: loads extension, fires `agent_start`, returns result.
fn eval_crypto(js_expr: &str) -> String {
    let harness = common::TestHarness::new("crypto_shim");
    let source = crypto_ext_source(js_expr);
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

/// Evaluate the production `node:crypto` module in a bare QuickJS context.
///
/// The full extension runtime intentionally installs non-configurable native
/// crypto bridges, so tests must not mutate those production capabilities to
/// simulate an initialization failure. A bare context provides the honest
/// missing-hostcall state while exercising the exact shipped module source.
fn eval_crypto_without_hostcalls(js_expr: &str) -> String {
    let runtime = rquickjs::Runtime::new().expect("create bare QuickJS runtime");
    let context = rquickjs::Context::full(&runtime).expect("create bare QuickJS context");

    context
        .with(|ctx| -> rquickjs::Result<String> {
            let (module, evaluation) = rquickjs::Module::declare(
                ctx.clone(),
                "node:crypto",
                pi::crypto_shim::NODE_CRYPTO_JS,
            )?
            .eval()?;
            evaluation.finish::<()>()?;

            let namespace = module.namespace()?;
            let globals = ctx.globals();
            for name in ["createHash", "randomBytes", "randomUUID", "randomInt"] {
                let function: rquickjs::Function<'_> = namespace.get(name)?;
                globals.set(name, function)?;
            }

            ctx.eval::<(), _>(format!(
                "globalThis.__crypto_missing_hostcall_result = String({js_expr});"
            ))?;
            globals.get("__crypto_missing_hostcall_result")
        })
        .expect("evaluate node:crypto without native hostcalls")
}

// ─── SHA-256 Tests ──────────────────────────────────────────────────────────

#[test]
fn sha256_hello_hex() {
    let result = eval_crypto(r#"createHash("sha256").update("hello").digest("hex")"#);
    assert_eq!(
        result, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        "SHA-256 of 'hello' must match Node.js"
    );
}

#[test]
fn sha256_empty_hex() {
    let result = eval_crypto(r#"createHash("sha256").update("").digest("hex")"#);
    assert_eq!(
        result,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn sha256_base64() {
    let result = eval_crypto(r#"createHash("sha256").update("hello").digest("base64")"#);
    assert_eq!(result, "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=");
}

#[test]
fn sha256_chained_update() {
    let result =
        eval_crypto(r#"createHash("sha256").update("hello").update(" world").digest("hex")"#);
    assert_eq!(
        result,
        "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
    );
}

// ─── SHA-1 Tests ────────────────────────────────────────────────────────────

#[test]
fn sha1_hello_hex() {
    let result = eval_crypto(r#"createHash("sha1").update("hello").digest("hex")"#);
    assert_eq!(result, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
}

// ─── MD5 Tests ──────────────────────────────────────────────────────────────

#[test]
fn md5_hello_hex() {
    let result = eval_crypto(r#"createHash("md5").update("hello").digest("hex")"#);
    assert_eq!(result, "5d41402abc4b2a76b9719d911017c592");
}

// ─── SHA-384/SHA-512 Tests ─────────────────────────────────────────────────

#[test]
fn sha384_hello_hex() {
    let result = eval_crypto(r#"createHash("sha384").update("hello").digest("hex")"#);
    assert_eq!(
        result,
        "59e1748777448c69de6b800d7a33bbfb9ff1b463e44354c3553bcdb9c666fa90125a3c79f90397bdf5f6a13de828684f"
    );
}

#[test]
fn sha512_hello_hex() {
    let result = eval_crypto(r#"createHash("sha512").update("hello").digest("hex")"#);
    assert_eq!(
        result,
        "9b71d224bd62f3785d96d46ad3ea3d73319bfbc2890caadae2dff72519673ca72323c3d99ba5c11d7c7acc6e14b8c5da0c4663475c2e5c3adef46f73bcdec043"
    );
}

// ─── HMAC Tests ─────────────────────────────────────────────────────────────

#[test]
fn hmac_sha256_hex() {
    let result = eval_crypto(r#"createHmac("sha256", "secret").update("hello").digest("hex")"#);
    assert_eq!(
        result,
        "88aab3ede8d3adf94d26ab90d3bafd4a2083070c3bcce9c014ee04a443847c0b"
    );
}

#[test]
fn hmac_sha384_hex() {
    let result = eval_crypto(r#"createHmac("sha384", "secret").update("hello").digest("hex")"#);
    assert_eq!(
        result,
        "7e1e620ca0068fd1fce00c1ad3f5c6dbb12874dd2fb9c26502d09d0d804f2c0ba1d921b9458416cba480417571001e18"
    );
}

#[test]
fn hmac_sha1_hex() {
    let result = eval_crypto(r#"createHmac("sha1", "key").update("data").digest("hex")"#);
    assert_eq!(result, "104152c5bfdca07bc633eebd46199f0255c9f49d");
}

// ─── randomUUID Tests ───────────────────────────────────────────────────────

#[test]
fn random_uuid_format() {
    let result = eval_crypto("randomUUID()");
    let re =
        regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$")
            .unwrap();
    assert!(
        re.is_match(&result),
        "UUID should be valid v4 format, got: {result}"
    );
}

#[test]
fn random_uuid_uniqueness() {
    let result = eval_crypto("randomUUID() + '|' + randomUUID()");
    let parts: Vec<&str> = result.split('|').collect();
    assert_eq!(parts.len(), 2);
    assert_ne!(parts[0], parts[1], "Two UUIDs should differ");
}

// ─── randomBytes Tests ──────────────────────────────────────────────────────

#[test]
fn random_bytes_length() {
    let result = eval_crypto("randomBytes(16).length");
    assert_eq!(result, "16");
}

#[test]
fn random_bytes_hex_encoding() {
    let result = eval_crypto("randomBytes(4).toString('hex').length");
    assert_eq!(result, "8");
}

#[test]
fn random_bytes_hex_valid() {
    let result = eval_crypto("randomBytes(16).toString('hex')");
    let re = regex::Regex::new(r"^[0-9a-f]{32}$").unwrap();
    assert!(
        re.is_match(&result),
        "randomBytes hex should be 32 hex chars, got: {result}"
    );
}

#[test]
fn random_bytes_rejects_non_integer() {
    let result = eval_crypto(
        r#"(() => {
        try {
            randomBytes(3.5);
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("non-negative integer"),
        "Expected randomBytes to reject non-integer size, got: {result}"
    );
}

// ─── randomInt Tests ────────────────────────────────────────────────────────

#[test]
fn random_int_range() {
    let result = eval_crypto(
        r#"(() => {
        const vals = [];
        for (let i = 0; i < 100; i++) vals.push(randomInt(10, 20));
        return vals.every(v => v >= 10 && v < 20) ? "ok" : "fail:" + JSON.stringify(vals);
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn random_int_single_arg() {
    let result = eval_crypto(
        r#"(() => {
        const vals = [];
        for (let i = 0; i < 100; i++) vals.push(randomInt(5));
        return vals.every(v => v >= 0 && v < 5) ? "ok" : "fail:" + JSON.stringify(vals);
    })()"#,
    );
    assert_eq!(result, "ok");
}

#[test]
fn random_int_rejects_min_ge_max() {
    let result = eval_crypto(
        r#"(() => {
        try {
            randomInt(5, 5);
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("min must be less than max"),
        "Expected min>=max to throw, got: {result}"
    );
}

// ─── timingSafeEqual Tests ──────────────────────────────────────────────────

#[test]
fn timing_safe_equal_same() {
    let result = eval_crypto(
        r"(() => {
        const a = new Uint8Array([1, 2, 3, 4]);
        const b = new Uint8Array([1, 2, 3, 4]);
        return timingSafeEqual(a, b);
    })()",
    );
    assert_eq!(result, "true");
}

#[test]
fn timing_safe_equal_different() {
    let result = eval_crypto(
        r"(() => {
        const a = new Uint8Array([1, 2, 3, 4]);
        const b = new Uint8Array([1, 2, 3, 5]);
        return timingSafeEqual(a, b);
    })()",
    );
    assert_eq!(result, "false");
}

#[test]
fn timing_safe_equal_length_mismatch() {
    let result = eval_crypto(
        r#"(() => {
        const a = new Uint8Array([1, 2, 3]);
        const b = new Uint8Array([1, 2, 3, 4]);
        try { timingSafeEqual(a, b); return "no-throw"; } catch(e) { return "threw:" + e.message; }
    })()"#,
    );
    assert!(
        result.contains("threw:"),
        "Should throw on length mismatch, got: {result}"
    );
}

// ─── getHashes Tests ────────────────────────────────────────────────────────

#[test]
fn get_hashes_includes_standard() {
    let result = eval_crypto("JSON.stringify(getHashes().sort())");
    let hashes: Vec<String> = serde_json::from_str(&result).expect("parse JSON");
    assert!(hashes.contains(&"sha256".to_string()));
    assert!(hashes.contains(&"sha384".to_string()));
    assert!(hashes.contains(&"sha1".to_string()));
    assert!(hashes.contains(&"md5".to_string()));
    assert!(hashes.contains(&"sha512".to_string()));
}

// ─── Import style tests ────────────────────────────────────────────────────

#[test]
fn named_import_works() {
    let harness = common::TestHarness::new("crypto_named_import");
    let source = r#"
import { createHash } from "node:crypto";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    return { result: createHash("sha256").update("test").digest("hex") };
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
    assert_eq!(
        result, "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
        "SHA-256 of 'test'"
    );
}

#[test]
fn bare_crypto_import_works() {
    let harness = common::TestHarness::new("crypto_bare_import");
    let source = r#"
import crypto from "crypto";

export default function activate(pi) {
  pi.on("agent_start", (event, ctx) => {
    return { result: crypto.createHash("md5").update("abc").digest("hex") };
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
    assert_eq!(result, "900150983cd24fb0d6963f7d28e17f72");
}

// ─── Digest returns Buffer-like object ──────────────────────────────────────

#[test]
fn digest_no_encoding_returns_buffer() {
    let result = eval_crypto(
        r#"(() => {
        const buf = createHash("sha256").update("hello").digest();
        return typeof buf.toString === "function" ? buf.toString("hex") : "no-toString";
    })()"#,
    );
    assert_eq!(
        result,
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
    );
}

#[test]
fn create_hash_unsupported_encoding_throws() {
    let result = eval_crypto(
        r#"(() => {
        try {
            createHash("sha256").update("hello").digest("utf16le");
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("unsupported encoding"),
        "Expected unsupported-encoding error, got: {result}"
    );
}

#[test]
fn create_hmac_unsupported_encoding_throws() {
    let result = eval_crypto(
        r#"(() => {
        try {
            createHmac("sha256", "secret").update("hello").digest("utf16le");
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("unsupported encoding"),
        "Expected unsupported-encoding error, got: {result}"
    );
}

#[test]
fn create_hash_second_digest_throws() {
    let result = eval_crypto(
        r#"(() => {
        const hash = createHash("sha256").update("hello");
        hash.digest("hex");
        try {
            hash.digest("hex");
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("digest() already called"),
        "Expected repeated digest() to throw, got: {result}"
    );
}

#[test]
fn create_hash_update_after_digest_throws() {
    let result = eval_crypto(
        r#"(() => {
        const hash = createHash("sha256").update("hello");
        hash.digest("hex");
        try {
            hash.update("world");
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("digest() already called"),
        "Expected update() after digest() to throw, got: {result}"
    );
}

#[test]
fn hash_update_latin1_input_encoding_uses_single_byte_string() {
    let result = eval_crypto(r#"createHash("sha256").update("\u00ffA", "latin1").digest("hex")"#);
    assert_eq!(
        result, "be611a063fe2322ed4671804fd2e68027756b32e14ec8d64f2e790344eb93261",
        "Hash.update latin1 input encoding should match Node Buffer single-byte semantics"
    );
}

#[test]
fn create_hmac_second_digest_throws() {
    let result = eval_crypto(
        r#"(() => {
        const hmac = createHmac("sha256", "secret").update("hello");
        hmac.digest("hex");
        try {
            hmac.digest("hex");
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("digest() already called"),
        "Expected repeated HMAC digest() to throw, got: {result}"
    );
}

#[test]
fn create_hmac_update_after_digest_throws() {
    let result = eval_crypto(
        r#"(() => {
        const hmac = createHmac("sha256", "secret").update("hello");
        hmac.digest("hex");
        try {
            hmac.update("world");
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("digest() already called"),
        "Expected HMAC update() after digest() to throw, got: {result}"
    );
}

#[test]
fn hmac_update_latin1_binary_ascii_use_single_byte_strings() {
    let result = eval_crypto(
        r#"(() => {
        return ["latin1", "binary", "ascii"]
            .map((enc) => createHmac("sha256", "secret")
                .update("\u00ffA", enc)
                .digest("hex"))
            .join("|");
    })()"#,
    );
    assert_eq!(
        result,
        concat!(
            "bbd41c1d309f223632dd368957de328977b73705b7001c50d270fe61ff1b9015|",
            "bbd41c1d309f223632dd368957de328977b73705b7001c50d270fe61ff1b9015|",
            "bbd41c1d309f223632dd368957de328977b73705b7001c50d270fe61ff1b9015"
        ),
        "latin1/binary/ascii input encodings should match Node Buffer single-byte semantics"
    );
}

#[test]
fn create_hmac_key_encoding_option_matches_node_vectors() {
    let result = eval_crypto(
        r#"(() => {
        return [
            createHmac("sha256", "\u00ffA").update("data").digest("hex"),
            createHmac("sha256", "\u00ffA", { encoding: "utf8" }).update("data").digest("hex"),
            createHmac("sha256", "\u00ffA", { encoding: "latin1" }).update("data").digest("hex"),
            createHmac("sha256", "\u00ffA", { encoding: "binary" }).update("data").digest("hex"),
        ].join("|");
    })()"#,
    );
    assert_eq!(
        result,
        concat!(
            "3919b4895e983e4f4a93c7dac6d603ccc8d15a166f2fc6193637d66800f925fc|",
            "3919b4895e983e4f4a93c7dac6d603ccc8d15a166f2fc6193637d66800f925fc|",
            "cd94c8465ef9705cd72ea90cb12e9cfb8196f355277b69195c2b38ab88b4989b|",
            "cd94c8465ef9705cd72ea90cb12e9cfb8196f355277b69195c2b38ab88b4989b"
        ),
        "createHmac key encoding option should match Node vectors"
    );
}

#[test]
fn create_hmac_rejects_unsupported_key_encoding_option() {
    let result = eval_crypto(
        r#"(() => {
        try {
            createHmac("sha256", "secret", { encoding: "utf16le" });
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("unsupported input encoding"),
        "Expected unsupported key encoding to throw, got: {result}"
    );
}

#[test]
fn pbkdf2_sync_derives_expected_key() {
    let result =
        eval_crypto(r#"pbkdf2Sync("password", "salt", 1000, 16, "sha256").toString("hex")"#);
    assert_eq!(
        result, "632c2812e46d4604102ba7618e9d6d7d",
        "pbkdf2Sync derived key mismatch"
    );
}

#[test]
fn pbkdf2_sync_accepts_digest_variants() {
    let result =
        eval_crypto(r#"pbkdf2Sync("password", "salt", 1000, 16, "SHA-256").toString("hex")"#);
    assert_eq!(
        result, "632c2812e46d4604102ba7618e9d6d7d",
        "pbkdf2Sync should accept digest variants"
    );
}

#[test]
fn pbkdf2_async_derives_expected_key() {
    let result = eval_crypto(
        r#"(() => {
        let outcome = "no-callback";
        pbkdf2("password", "salt", 1000, 16, "sha256", (err, value) => {
            outcome = err ? "threw:" + err.message : value.toString("hex");
        });
        return outcome;
    })()"#,
    );
    assert_eq!(
        result, "632c2812e46d4604102ba7618e9d6d7d",
        "pbkdf2 async derived key mismatch"
    );
}

#[test]
fn pbkdf2_sync_rejects_large_iterations() {
    let result = eval_crypto(
        r#"(() => {
        try {
            pbkdf2Sync("password", "salt", 1000001, 16, "sha256");
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("iterations must be <= 1000000"),
        "Expected pbkdf2Sync to reject large iterations, got: {result}"
    );
}

#[test]
fn scrypt_sync_derives_expected_key() {
    let result = eval_crypto(r#"scryptSync("password", "salt", 16).toString("hex")"#);
    assert_eq!(
        result, "745731af4484f323968969eda289aeee",
        "scryptSync derived key mismatch"
    );
}

#[test]
fn scrypt_sync_accepts_encoding_string() {
    let result = eval_crypto(r#"scryptSync("password", "salt", 16, "hex")"#);
    assert_eq!(
        result, "745731af4484f323968969eda289aeee",
        "scryptSync hex encoding mismatch"
    );
}

#[test]
fn scrypt_sync_accepts_options_encoding() {
    let direct = eval_crypto(r#"scryptSync("password", "salt", 16, "hex")"#);
    let result = eval_crypto(r#"scryptSync("password", "salt", 16, { encoding: "hex" })"#);
    assert_eq!(
        result, direct,
        "scryptSync options encoding should match direct encoding string"
    );
}

#[test]
fn scrypt_sync_rejects_large_n() {
    let result = eval_crypto(
        r#"(() => {
        try {
            scryptSync("password", "salt", 16, { N: 2097152 });
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("N must be <= 2^20"),
        "Expected scryptSync to reject large N, got: {result}"
    );
}

#[test]
fn scrypt_sync_rejects_excessive_memory() {
    let result = eval_crypto(
        r#"(() => {
        try {
            scryptSync("password", "salt", 16, { N: 262144, r: 16, p: 1 });
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("memory limit"),
        "Expected scryptSync to reject excessive memory, got: {result}"
    );
}

#[test]
fn scrypt_sync_rejects_non_positive_rp() {
    let result = eval_crypto(
        r#"(() => {
        try {
            scryptSync("password", "salt", 16, { r: 0 });
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("r/p must be positive"),
        "Expected scryptSync to reject r/p <= 0, got: {result}"
    );
}

#[test]
fn aes_128_gcm_matches_known_vector() {
    let result = eval_crypto(
        r#"(() => {
        const key = new Uint8Array(16);
        const iv = new Uint8Array(12);
        const plaintext = new Uint8Array(16);
        const cipher = createCipheriv("aes-128-gcm", key, iv);
        const ciphertext = cipher.update(plaintext).toString("hex") + cipher.final("hex");
        return ciphertext + "|" + cipher.getAuthTag().toString("hex");
    })()"#,
    );
    assert_eq!(
        result,
        "0388dace60b6a392f328c2b971b2fe78|ab6e47d42cec13bdf53a67b21257bddf"
    );
}

#[test]
fn aes_gcm_latin1_input_and_output_preserve_single_byte_plaintext() {
    let result = eval_crypto(
        r#"(() => {
        const key = new Uint8Array(16);
        const iv = new Uint8Array(12);
        const cipher = createCipheriv("aes-128-gcm", key, iv);
        const ciphertext = cipher.update("\u00ffA", "latin1", "hex") + cipher.final("hex");
        const tag = cipher.getAuthTag();

        const latin1Decipher = createDecipheriv("aes-128-gcm", key, iv);
        latin1Decipher.setAuthTag(tag);
        const latin1Text =
            latin1Decipher.update(ciphertext, "hex", "latin1") + latin1Decipher.final("latin1");

        const asciiDecipher = createDecipheriv("aes-128-gcm", key, iv);
        asciiDecipher.setAuthTag(tag);
        const asciiText =
            asciiDecipher.update(ciphertext, "hex", "ascii") + asciiDecipher.final("ascii");

        const latin1Codes = Array.from(latin1Text).map((ch) => ch.charCodeAt(0).toString(16)).join(",");
        const asciiCodes = Array.from(asciiText).map((ch) => ch.charCodeAt(0).toString(16)).join(",");
        return ciphertext + "|" + tag.toString("hex") + "|" + latin1Codes + "|" + asciiCodes;
    })()"#,
    );
    assert_eq!(result, "fcc9|01f639cf1075df70a828d0cba61c1992|ff,41|7f,41");
}

#[test]
fn aes_256_gcm_roundtrips_with_aad() {
    let result = eval_crypto(
        r#"(() => {
        const key = new Uint8Array(32);
        for (let i = 0; i < key.length; i++) key[i] = i;
        const iv = new Uint8Array(12);
        for (let i = 0; i < iv.length; i++) iv[i] = i + 1;
        const cipher = createCipheriv("aes-256-gcm", key, iv);
        cipher.setAAD("header");
        const ciphertext = cipher.update("hello ", "utf8", "hex") + cipher.final("hex");
        const tag = cipher.getAuthTag();

        const decipher = createDecipheriv("aes-256-gcm", key, iv);
        decipher.setAAD("header");
        decipher.setAuthTag(tag);
        return decipher.update(ciphertext, "hex").toString("utf8") + decipher.final("utf8");
    })()"#,
    );
    assert_eq!(result, "hello ");
}

#[test]
fn aes_gcm_rejects_bad_key_iv_tag_and_algorithm() {
    let result = eval_crypto(
        r#"(() => {
        const errors = [];
        try { createCipheriv("aes-256-gcm", new Uint8Array(16), new Uint8Array(12)); }
        catch (e) { errors.push(e.message); }
        try { createCipheriv("aes-256-gcm", new Uint8Array(32), new Uint8Array(16)); }
        catch (e) { errors.push(e.message); }
        try { createCipheriv("aes-192-gcm", new Uint8Array(24), new Uint8Array(12)); }
        catch (e) { errors.push(e.message); }
        try {
            const d = createDecipheriv("aes-128-gcm", new Uint8Array(16), new Uint8Array(12));
            d.setAuthTag(new Uint8Array(8));
        } catch (e) { errors.push(e.message); }
        return errors.join("|");
    })()"#,
    );
    assert!(
        result.contains("aes-256-gcm key must be exactly 32 bytes"),
        "missing bad key error: {result}"
    );
    assert!(
        result.contains("AES-GCM IV must be exactly 12 bytes"),
        "missing bad IV error: {result}"
    );
    assert!(
        result.contains("unsupported cipher algorithm 'aes-192-gcm'"),
        "missing unsupported algorithm error: {result}"
    );
    assert!(
        result.contains("requires a 16-byte tag"),
        "missing bad tag error: {result}"
    );
}

#[test]
fn aes_gcm_authentication_failure_throws() {
    let result = eval_crypto(
        r#"(() => {
        const key = new Uint8Array(16);
        const iv = new Uint8Array(12);
        const cipher = createCipheriv("aes-128-gcm", key, iv);
        const ciphertext = cipher.update("secret", "utf8", "hex") + cipher.final("hex");
        const tag = cipher.getAuthTag();
        tag[0] ^= 1;
        const decipher = createDecipheriv("aes-128-gcm", key, iv);
        decipher.setAuthTag(tag);
        try {
          decipher.update(ciphertext, "hex");
          decipher.final("utf8");
          return "no-throw";
        } catch (e) {
          return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("AES-GCM authentication failed"),
        "Expected auth failure, got: {result}"
    );
}

#[test]
fn aes_gcm_rejects_double_final_and_missing_auth_tag() {
    let result = eval_crypto(
        r#"(() => {
        const key = new Uint8Array(16);
        const iv = new Uint8Array(12);
        const errors = [];
        const cipher = createCipheriv("aes-128-gcm", key, iv);
        cipher.final("hex");
        try { cipher.final("hex"); } catch (e) { errors.push(e.message); }
        const decipher = createDecipheriv("aes-128-gcm", key, iv);
        try { decipher.final("utf8"); } catch (e) { errors.push(e.message); }
        return errors.join("|");
    })()"#,
    );
    assert!(
        result.contains("Cipher.final() already called"),
        "missing double-final error: {result}"
    );
    assert!(
        result.contains("requires setAuthTag() first"),
        "missing auth-tag-required error: {result}"
    );
}

#[test]
fn sign_ed25519_pkcs8_matches_node_vector() {
    let result = eval_crypto(
        r#"(() => {
        const privateKey = `-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIKdNGarTy3x3BLKmHN/4JHUxgXyGLoYEwCQk5lMDT0Wc
-----END PRIVATE KEY-----`;
        return sign(null, "hello", privateKey).toString("hex");
    })()"#,
    );
    assert_eq!(
        result,
        "eb5553cfbe1b7a3acf1c13577e7c1685dddb4193dd089d62123261ba4d1adf7086762ed6b53c4708ee984f6e4b6cd7788fb066d552a97ca5c25fb00efd6d3f05",
        "Ed25519 signature must match Node.js for the fixed PKCS#8 key"
    );
}

#[test]
fn verify_ed25519_spki_accepts_node_vector() {
    let result = eval_crypto(
        r#"(() => {
        const publicKey = `-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAnXpFnYYkKVmCrFuByPcsUZW6Wno7zMOjk9xlzkYRpoo=
-----END PUBLIC KEY-----`;
        const signature = Uint8Array.from("eb5553cfbe1b7a3acf1c13577e7c1685dddb4193dd089d62123261ba4d1adf7086762ed6b53c4708ee984f6e4b6cd7788fb066d552a97ca5c25fb00efd6d3f05".match(/../g).map((byte) => parseInt(byte, 16)));
        return verify(null, "hello", publicKey, signature);
    })()"#,
    );
    assert_eq!(
        result, "true",
        "Ed25519 verify must accept the Node.js vector"
    );
}

#[test]
fn verify_ed25519_spki_rejects_bad_signature() {
    let result = eval_crypto(
        r#"(() => {
        const publicKey = `-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAnXpFnYYkKVmCrFuByPcsUZW6Wno7zMOjk9xlzkYRpoo=
-----END PUBLIC KEY-----`;
        const signature = Uint8Array.from("eb5553cfbe1b7a3acf1c13577e7c1685dddb4193dd089d62123261ba4d1adf7086762ed6b53c4708ee984f6e4b6cd7788fb066d552a97ca5c25fb00efd6d3f05".match(/../g).map((byte) => parseInt(byte, 16)));
        signature[0] ^= 1;
        return verify(null, "hello", publicKey, signature);
    })()"#,
    );
    assert_eq!(
        result, "false",
        "Ed25519 verify must reject modified signatures"
    );
}

#[test]
fn sign_rejects_digest_algorithms_for_ed25519_subset() {
    let result = eval_crypto(
        r#"(() => {
        const privateKey = `-----BEGIN PRIVATE KEY-----
MC4CAQAwBQYDK2VwBCIEIKdNGarTy3x3BLKmHN/4JHUxgXyGLoYEwCQk5lMDT0Wc
-----END PRIVATE KEY-----`;
        try {
            sign("sha256", "hello", privateKey);
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("only Ed25519"),
        "Expected sign() to fail closed for digest algorithms, got: {result}"
    );
}

#[test]
fn native_crypto_bridges_cannot_be_replaced_by_extension_code() {
    let result = eval_crypto(
        r#"(() => {
        const names = [
          "__pi_crypto_aes_gcm_decrypt_native",
          "__pi_crypto_aes_gcm_encrypt_native",
          "__pi_crypto_ed25519_sign_native",
          "__pi_crypto_ed25519_verify_native",
          "__pi_crypto_hash_native",
          "__pi_crypto_hmac_native",
          "__pi_crypto_pbkdf2_native",
          "__pi_crypto_random_bytes_native",
          "__pi_crypto_random_uuid_native",
          "__pi_crypto_random_int_native",
          "__pi_crypto_scrypt_native",
          "__pi_crypto_timing_safe_equal_native",
        ];
        const originals = names.map((name) => globalThis[name]);
        for (const name of names) {
          try { globalThis[name] = undefined; } catch (_) {}
          try { Object.defineProperty(globalThis, name, { value: undefined }); } catch (_) {}
          try { delete globalThis[name]; } catch (_) {}
        }
        const intact = names.every(
          (name, index) => typeof globalThis[name] === "function" && globalThis[name] === originals[index],
        );
        const hash = createHash("sha256").update("hello").digest("hex");
        const bytes = randomBytes(8);
        const uuid = randomUUID();
        const integer = randomInt(1, 3);
        return [
          intact,
          hash === "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
          bytes.length === 8,
          /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid),
          integer >= 1 && integer < 3,
        ].join(":");
    })()"#,
    );
    assert_eq!(
        result, "true:true:true:true:true",
        "native crypto bridges must remain immutable and operational"
    );
}

#[test]
fn create_hash_without_hostcall_throws() {
    let result = eval_crypto_without_hostcalls(
        r#"(() => {
        try {
            createHash("sha256").digest("hex");
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("createHash not available"),
        "Expected clear missing-hostcall error, got: {result}"
    );
}

#[test]
fn random_bytes_without_hostcall_throws() {
    let result = eval_crypto_without_hostcalls(
        r#"(() => {
        try {
            randomBytes(8);
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("randomBytes not available"),
        "Expected clear missing-hostcall error, got: {result}"
    );
}

#[test]
fn random_uuid_without_hostcall_throws() {
    let result = eval_crypto_without_hostcalls(
        r#"(() => {
        try {
            randomUUID();
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("randomUUID not available"),
        "Expected clear missing-hostcall error, got: {result}"
    );
}

#[test]
fn random_int_without_hostcall_throws() {
    let result = eval_crypto_without_hostcalls(
        r#"(() => {
        try {
            randomInt(1, 3);
            return "no-throw";
        } catch (e) {
            return "threw:" + e.message;
        }
    })()"#,
    );
    assert!(
        result.contains("randomInt not available"),
        "Expected clear missing-hostcall error, got: {result}"
    );
}
