//! Comprehensive unit test suite for all auto-repair patterns (bd-k5q5.8.9).
//!
//! Tests individual repair functions in isolation with synthetic inputs.
//! Organized by pattern with shared fixtures and assertion helpers.

#![allow(clippy::doc_markdown)]

mod common;

use pi::extensions::{
    ExtensionEventName, ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle,
};
use pi::extensions_js::{
    PiJsRuntimeConfig, RepairMode, RepairPattern, extract_import_names, generate_monorepo_stub,
};
use pi::tools::ToolRegistry;
use std::sync::Arc;

// ─── Test Infrastructure ─────────────────────────────────────────────────────

fn create_repair_runtime(
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

fn load_ext(
    _harness: &common::TestHarness,
    manager: &ExtensionManager,
    entry_path: &std::path::Path,
) {
    let spec = JsExtensionLoadSpec::from_entry_path(entry_path).expect("load spec");
    common::run_async({
        let mgr = manager.clone();
        async move {
            mgr.load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });
}

fn dispatch_and_get_result(manager: ExtensionManager) -> String {
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

/// Fixture: minimal extension with default export
const FIXTURE_NORMAL_DEFAULT: &str = r#"
export default function(pi) {
    pi.on("agent_start", () => ({ result: "normal_default" }));
}
"#;

/// Fixture: double-wrapped CJS default
const FIXTURE_DOUBLE_WRAP: &str = r#"
const fn = function(pi) {
    pi.on("agent_start", () => ({ result: "unwrapped" }));
};
export default { default: fn };
"#;

/// Fixture: named activate export
const FIXTURE_NAMED_ACTIVATE: &str = r#"
export function activate(pi) {
    pi.on("agent_start", () => ({ result: "activate" }));
}
"#;

/// Fixture: default object with activate method
const FIXTURE_OBJECT_ACTIVATE: &str = r#"
export default {
    activate(pi) {
        pi.on("agent_start", () => ({ result: "obj_activate" }));
    }
};
"#;

// ═══════════════════════════════════════════════════════════════════════════════
// Pattern 1: dist/ → src/ fallback
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn p1_dist_to_src_ts_fallback() {
    let harness = common::TestHarness::new("p1_ts");
    let cwd = harness.temp_dir().to_path_buf();

    // Create extension that imports from ./dist/ but only ./src/ exists
    let ext_dir = cwd.join("extensions").join("p1-ext");
    let src_dir = ext_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import helper from "./dist/helper.js";
export default function(pi) {
    pi.on("agent_start", () => ({ result: helper() }));
}
"#,
    )
    .unwrap();
    std::fs::write(
        src_dir.join("helper.ts"),
        r#"export default function() { return "from_src"; }"#,
    )
    .unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(
        result, "from_src",
        "dist→src fallback should resolve to src/helper.ts"
    );
}

#[test]
fn p1_dist_to_src_disabled_in_off_mode() {
    let harness = common::TestHarness::new("p1_off");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p1-off");
    let src_dir = ext_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"import h from "./dist/h.js"; export default function(pi) {}"#,
    )
    .unwrap();
    std::fs::write(src_dir.join("h.ts"), r"export default 1;").unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::Off);
    manager.set_js_runtime(runtime);

    let spec = JsExtensionLoadSpec::from_entry_path(ext_dir.join("index.mjs")).expect("load spec");
    let result = common::run_async(async move { manager.load_js_extensions(vec![spec]).await });

    assert!(result.is_err(), "dist→src should fail with repair mode Off");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pattern 2: missing asset fallback
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn p2_missing_html_in_ext_root() {
    let harness = common::TestHarness::new("p2_html");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p2-ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import fs from "node:fs";
export default function(pi) {
    pi.on("agent_start", () => {
        try {
            const html = fs.readFileSync("extensions/p2-ext/template.html", "utf8");
            return { result: html.includes("DOCTYPE") ? "html_fallback" : "other" };
        } catch (e) {
            return { result: "error:" + e.message };
        }
    });
}
"#,
    )
    .unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(result, "html_fallback");
}

#[test]
fn p2_missing_css_in_ext_root() {
    let harness = common::TestHarness::new("p2_css");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p2-css");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import fs from "node:fs";
export default function(pi) {
    pi.on("agent_start", () => {
        try {
            const css = fs.readFileSync("extensions/p2-css/theme.css", "utf8");
            return { result: css.includes("stylesheet") ? "css_fallback" : "other" };
        } catch (e) {
            return { result: "error:" + e.message };
        }
    });
}
"#,
    )
    .unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(result, "css_fallback");
}

#[test]
fn p2_missing_json_no_fallback() {
    let harness = common::TestHarness::new("p2_json");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p2-json");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import fs from "node:fs";
export default function(pi) {
    pi.on("agent_start", () => {
        try {
            fs.readFileSync("extensions/p2-json/config.json", "utf8");
            return { result: "read_ok" };
        } catch (e) {
            return { result: "json_error" };
        }
    });
}
"#,
    )
    .unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(result, "json_error", ".json should NOT get a fallback");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pattern 3: monorepo escape stubs (unit tests)
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn p3_extract_imports_multiline() {
    let source = r#"
import {
    getConfig,
    isReady,
    BEEP_SOUNDS
} from "../../shared";
"#;
    let names = extract_import_names(source, "../../shared");
    assert_eq!(names, vec!["BEEP_SOUNDS", "getConfig", "isReady"]);
}

#[test]
fn p3_extract_imports_single_import() {
    let source = r#"import { foo } from "../../shared";"#;
    let names = extract_import_names(source, "../../shared");
    assert_eq!(names, vec!["foo"]);
}

#[test]
fn p3_extract_imports_mixed_esm_cjs() {
    let source = r#"
import { alpha } from "../../shared";
const { beta } = require("../../shared");
"#;
    let names = extract_import_names(source, "../../shared");
    assert_eq!(names, vec!["alpha", "beta"]);
}

#[test]
fn p3_stub_plays_beep_noop() {
    let stub = generate_monorepo_stub(&["playBeep".to_string()]);
    assert!(stub.contains("export const playBeep = () => {};"));
}

#[test]
fn p3_stub_is_ready_returns_false() {
    let stub = generate_monorepo_stub(&["isReady".to_string()]);
    assert!(stub.contains("export const isReady = () => false;"));
}

#[test]
fn p3_stub_get_config_returns_object() {
    let stub = generate_monorepo_stub(&["getConfig".to_string()]);
    assert!(stub.contains("export const getConfig = () => ({});"));
}

#[test]
fn p3_stub_constant_returns_array() {
    let stub = generate_monorepo_stub(&["BEEP_SOUNDS".to_string()]);
    assert!(stub.contains("export const BEEP_SOUNDS = [];"));
}

#[test]
fn p3_stub_class_name() {
    let stub = generate_monorepo_stub(&["ProcessManager".to_string()]);
    assert!(stub.contains("export class ProcessManager {}"));
}

// ─── Pattern 3 integration ──────────────────────────────────────────────────

#[test]
fn p3_monorepo_escape_loads_with_strict() {
    let harness = common::TestHarness::new("p3_int");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p3-ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"
import { getConfig } from "../../shared";
export default function(pi) {
    pi.on("agent_start", () => {
        const c = getConfig();
        return { result: JSON.stringify(c) };
    });
}
"#,
    )
    .unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoStrict);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(result, "{}", "getConfig() stub should return {{}}");
}

#[test]
fn p3_monorepo_escape_rejects_in_safe_mode() {
    let harness = common::TestHarness::new("p3_safe");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p3-safe");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(
        ext_dir.join("index.mjs"),
        r#"import { x } from "../../shared"; export default function(pi) {}"#,
    )
    .unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);

    let spec = JsExtensionLoadSpec::from_entry_path(ext_dir.join("index.mjs")).expect("load spec");
    let result = common::run_async(async move { manager.load_js_extensions(vec![spec]).await });

    assert!(
        result.is_err(),
        "monorepo escape should fail in AutoSafe mode"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pattern 5: export shape normalization
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn p5_normal_default_export() {
    let harness = common::TestHarness::new("p5_normal");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p5-normal");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("index.mjs"), FIXTURE_NORMAL_DEFAULT).unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(result, "normal_default");
}

#[test]
fn p5_double_wrapped_default() {
    let harness = common::TestHarness::new("p5_double");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p5-double");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("index.mjs"), FIXTURE_DOUBLE_WRAP).unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(result, "unwrapped");
}

#[test]
fn p5_named_activate() {
    let harness = common::TestHarness::new("p5_named");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p5-named");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("index.mjs"), FIXTURE_NAMED_ACTIVATE).unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(result, "activate");
}

#[test]
fn p5_object_with_activate() {
    let harness = common::TestHarness::new("p5_obj");
    let cwd = harness.temp_dir().to_path_buf();

    let ext_dir = cwd.join("extensions").join("p5-obj");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("index.mjs"), FIXTURE_OBJECT_ACTIVATE).unwrap();

    let (manager, runtime) = create_repair_runtime(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);
    load_ext(&harness, &manager, &ext_dir.join("index.mjs"));

    let result = dispatch_and_get_result(manager);
    assert_eq!(result, "obj_activate");
}

// ═══════════════════════════════════════════════════════════════════════════════
// Cross-pattern: repair mode gating
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn repair_mode_off_disables_all_repairs() {
    assert!(!RepairMode::Off.should_apply());
    assert!(!RepairMode::Off.is_active());
    assert!(!RepairMode::Off.allows_aggressive());
}

#[test]
fn repair_mode_suggest_does_not_apply() {
    assert!(!RepairMode::Suggest.should_apply());
    assert!(RepairMode::Suggest.is_active());
    assert!(!RepairMode::Suggest.allows_aggressive());
}

#[test]
fn repair_mode_auto_safe_applies_safe_only() {
    assert!(RepairMode::AutoSafe.should_apply());
    assert!(RepairMode::AutoSafe.is_active());
    assert!(!RepairMode::AutoSafe.allows_aggressive());
}

#[test]
fn repair_mode_auto_strict_applies_all() {
    assert!(RepairMode::AutoStrict.should_apply());
    assert!(RepairMode::AutoStrict.is_active());
    assert!(RepairMode::AutoStrict.allows_aggressive());
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pattern classification
// ═══════════════════════════════════════════════════════════════════════════════

#[test]
fn safe_patterns_in_auto_safe() {
    use pi::extensions_js::RepairRisk;
    assert_eq!(RepairPattern::DistToSrc.risk(), RepairRisk::Safe);
    assert_eq!(RepairPattern::MissingAsset.risk(), RepairRisk::Safe);
    assert_eq!(
        RepairPattern::ManifestNormalization.risk(),
        RepairRisk::Safe
    );
}

#[test]
fn aggressive_patterns_require_strict() {
    use pi::extensions_js::RepairRisk;
    assert_eq!(RepairPattern::MonorepoEscape.risk(), RepairRisk::Aggressive);
    assert_eq!(RepairPattern::MissingNpmDep.risk(), RepairRisk::Aggressive);
    assert_eq!(RepairPattern::ExportShape.risk(), RepairRisk::Aggressive);
}

#[test]
fn safe_patterns_allowed_by_auto_safe() {
    assert!(RepairPattern::DistToSrc.is_allowed_by(RepairMode::AutoSafe));
    assert!(RepairPattern::MissingAsset.is_allowed_by(RepairMode::AutoSafe));
}

#[test]
fn aggressive_patterns_denied_by_auto_safe() {
    assert!(!RepairPattern::MonorepoEscape.is_allowed_by(RepairMode::AutoSafe));
    assert!(!RepairPattern::MissingNpmDep.is_allowed_by(RepairMode::AutoSafe));
    assert!(!RepairPattern::ExportShape.is_allowed_by(RepairMode::AutoSafe));
}

#[test]
fn aggressive_patterns_allowed_by_strict() {
    assert!(RepairPattern::MonorepoEscape.is_allowed_by(RepairMode::AutoStrict));
    assert!(RepairPattern::MissingNpmDep.is_allowed_by(RepairMode::AutoStrict));
    assert!(RepairPattern::ExportShape.is_allowed_by(RepairMode::AutoStrict));
}
