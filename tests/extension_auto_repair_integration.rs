//! Integration tests: verify auto-repair with real-ish extension patterns (bd-k5q5.8.7).
//!
//! These tests simulate the structural patterns found in real community
//! extensions (monorepo escape, missing assets, dist→src fallback) using
//! synthetic extensions that mirror the real failure modes.

#![allow(clippy::doc_markdown)]

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::{PiJsRuntimeConfig, RepairMode};
use pi::tools::ToolRegistry;
use std::sync::Arc;

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn create_runtime(
    harness: &common::TestHarness,
    mode: RepairMode,
) -> (ExtensionManager, JsExtensionRuntimeHandle) {
    let cwd = harness.temp_dir().to_path_buf();
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        repair_mode: mode,
        ..Default::default()
    };

    let runtime = common::run_async({
        let manager = manager.clone();
        let tools = Arc::clone(&tools);
        async move {
            JsExtensionRuntimeHandle::start(config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });

    (manager, runtime)
}

fn load_and_verify(manager: &ExtensionManager, entry: &std::path::Path) {
    let spec = JsExtensionLoadSpec::from_entry_path(entry).expect("load spec");
    common::run_async({
        let mgr = manager.clone();
        async move {
            mgr.load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });
}

fn dispatch_result(manager: ExtensionManager) -> String {
    let response = common::run_async(async move {
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });

    response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Integration: Monorepo Escape (like qualisero-background-notify)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn integration_monorepo_escape_like_qualisero() {
    // Simulate qualisero-background-notify's pattern:
    // - Extension at community/ext-name/index.ts
    // - Imports { getConfig, isBackground, BEEP_SOUNDS } from "../../shared"
    // - No shared/ stub exists
    // With AutoStrict, monorepo escape auto-repair generates a stub.
    let harness = common::TestHarness::new("int_monorepo");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("community").join("qualisero-bg-notify-sim");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import {
    getBackgroundNotifyConfig,
    isTerminalInBackground,
    detectTerminalInfo,
    playBeep,
    BEEP_SOUNDS
} from "../../shared";

export default function activate(pi) {
    pi.on("agent_start", (event, ctx) => {
        const cfg = getBackgroundNotifyConfig();
        const bg = isTerminalInBackground();
        const info = detectTerminalInfo();
        return {
            result: `cfg=${typeof cfg},bg=${bg},info=${typeof info},beeps=${Array.isArray(BEEP_SOUNDS)}`
        };
    });
}
"#,
    )
    .unwrap();

    let (manager, runtime) = create_runtime(&harness, RepairMode::AutoStrict);
    manager.set_js_runtime(runtime);
    load_and_verify(&manager, &ext_dir.join("index.mjs"));

    let result = dispatch_result(manager);
    // Stub heuristics: getX→{}, isX→false, detectX→{}, BEEP_SOUNDS→[]
    assert!(
        result.contains("cfg=object"),
        "getBackgroundNotifyConfig should return object: {result}"
    );
    assert!(
        result.contains("bg=false"),
        "isTerminalInBackground should return false: {result}"
    );
    assert!(
        result.contains("beeps=true"),
        "BEEP_SOUNDS should be array: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Integration: Missing Asset Fallback (like nicobailon-interview-tool)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn integration_missing_assets_like_interview_tool() {
    // Simulate interview-tool pattern:
    // - Extension reads form/index.html, form/styles.css, form/script.js
    // - Those files don't exist
    // With AutoSafe, readFileSync returns empty fallback content.
    let harness = common::TestHarness::new("int_missing");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("interview-sim");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import fs from "node:fs";

export default function activate(pi) {
    pi.on("agent_start", (event, ctx) => {
        try {
            const html = fs.readFileSync("extensions/interview-sim/form.html", "utf8");
            const css = fs.readFileSync("extensions/interview-sim/styles.css", "utf8");
            const js = fs.readFileSync("extensions/interview-sim/script.js", "utf8");
            return {
                result: `html=${html.length > 0},css=${css.length > 0},js=${js.length > 0}`
            };
        } catch (e) {
            return { result: "error:" + e.message };
        }
    });
}
"#,
    )
    .unwrap();

    let (manager, runtime) = create_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_and_verify(&manager, &ext_dir.join("index.mjs"));

    let result = dispatch_result(manager);
    // All three should get fallback content (non-empty for html/css/js)
    assert!(
        result.contains("html=true"),
        "HTML should have fallback content: {result}"
    );
    assert!(
        result.contains("css=true"),
        "CSS should have fallback content: {result}"
    );
    assert!(
        result.contains("js=true"),
        "JS should have fallback content: {result}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Integration: dist→src Fallback (like npm builds)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn integration_dist_to_src_like_npm_package() {
    // Simulate an npm extension that imports from ./dist/helper.js
    // but only ./src/helper.ts exists.
    let harness = common::TestHarness::new("int_dist");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("npm-sim");
    let src_dir = ext_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import { version } from "./dist/metadata.js";
export default function activate(pi) {
    pi.on("agent_start", () => ({ result: "v=" + version }));
}
"#,
    )
    .unwrap();
    std::fs::write(
        src_dir.join("metadata.ts"),
        r#"export const version = "1.0.0";"#,
    )
    .unwrap();

    let (manager, runtime) = create_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_and_verify(&manager, &ext_dir.join("index.mjs"));

    let result = dispatch_result(manager);
    assert_eq!(result, "v=1.0.0", "dist→src should resolve metadata");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Integration: Export Shape Normalization
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn integration_cjs_double_wrap_loads() {
    // Simulate a CJS extension that produces a double-wrapped default.
    let harness = common::TestHarness::new("int_cjs");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("cjs-sim");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
const fn = function activate(pi) {
    pi.on("agent_start", () => ({ result: "cjs_ok" }));
};
export default { default: fn };
"#,
    )
    .unwrap();

    let (manager, runtime) = create_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_and_verify(&manager, &ext_dir.join("index.mjs"));

    let result = dispatch_result(manager);
    assert_eq!(result, "cjs_ok");
}

#[test]
fn integration_named_activate_loads() {
    let harness = common::TestHarness::new("int_named");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("named-sim");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
export function activate(pi) {
    pi.on("agent_start", () => ({ result: "named_ok" }));
}
"#,
    )
    .unwrap();

    let (manager, runtime) = create_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_and_verify(&manager, &ext_dir.join("index.mjs"));

    let result = dispatch_result(manager);
    assert_eq!(result, "named_ok");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Integration: Multiple Patterns Combined
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn integration_combined_dist_src_and_missing_asset() {
    // Extension imports from dist/ (fallback to src/) AND reads a missing file
    let harness = common::TestHarness::new("int_combined");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("combined-sim");
    let src_dir = ext_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import { greet } from "./dist/helpers.js";
import fs from "node:fs";

export default function activate(pi) {
    pi.on("agent_start", () => {
        const greeting = greet();
        let tmpl;
        try {
            tmpl = fs.readFileSync("extensions/combined-sim/template.html", "utf8");
        } catch (e) {
            tmpl = "fallback_failed";
        }
        return {
            result: `greet=${greeting},tmpl=${tmpl.includes("DOCTYPE") ? "html" : "other"}`
        };
    });
}
"#,
    )
    .unwrap();
    std::fs::write(
        src_dir.join("helpers.ts"),
        r#"export function greet() { return "hello"; }"#,
    )
    .unwrap();

    let (manager, runtime) = create_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_and_verify(&manager, &ext_dir.join("index.mjs"));

    let result = dispatch_result(manager);
    assert!(result.contains("greet=hello"), "dist→src repair: {result}");
    assert!(
        result.contains("tmpl=html"),
        "missing asset repair: {result}"
    );
}
