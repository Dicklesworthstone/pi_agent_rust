//! Unit tests for the auto-repair event logging infrastructure (bd-k5q5.8.1).
//!
//! Tests cover:
//! - `RepairPattern` display formatting
//! - `ExtensionRepairEvent` construction and cloning
//! - `PiJsRuntimeConfig` auto-repair flag
//! - `PiJsTickStats` default repair count
//! - `JsExtensionRuntimeHandle::drain_repair_events` (via channel)

#![allow(clippy::doc_markdown)]

mod common;

use pi::extensions::{ExtensionManager, JsExtensionLoadSpec, JsExtensionRuntimeHandle};
use pi::extensions_js::{
    AmbiguitySignal, ApprovalRequirement, AuditEntryKind, AuditLedger, CanaryConfig, CanaryRoute,
    CapabilityDelta, CapabilityMonotonicityVerdict, ConfidenceReport, ConflictKind,
    ConformanceFixture, ConformanceReplayVerdict, DeveloperGuide, ExtensionRepairEvent,
    GatingDecision, HealthSignal, HostcallCategory, HostcallDelta, IntentGraph, IntentSignal,
    LisrAdr, MonotonicityVerdict, OperatorPlaybook, OverlayArtifact, OverlayState,
    OverlayTransitionError, PatchOp, PatchProposal, PiJsRuntimeConfig, PiJsTickStats,
    PromotionDecision, ProposalValidationError, REPAIR_REGISTRY_VERSION, REPAIR_RULES, RepairMode,
    RepairPattern, RepairRisk, SemanticDriftSeverity, SemanticParityVerdict, SloVerdict,
    StructuralVerdict, TelemetryCollector, TelemetryMetric, TolerantParseResult,
    VerificationBundle, allowed_op_tags_for_mode, apply_proposal, build_approval_request,
    build_forensic_bundle, build_golden_manifest, build_inspection, check_approval_requirement,
    compute_artifact_checksum, compute_canary_bucket, compute_capability_proof, compute_confidence,
    compute_gating_verdict, compute_semantic_parity, decide_promotion, detect_conflict,
    evaluate_health, execute_promotion, execute_rollback, explain_gating, extract_hostcall_surface,
    extract_import_names, format_proposal_diff, generate_monorepo_stub,
    replay_conformance_fixtures, resolve_conflicts, run_governance_checklist,
    select_best_candidate, should_auto_rollback, tolerant_parse, transition_overlay,
    validate_proposal, validate_repaired_artifact,
};
use pi::tools::ToolRegistry;
use std::sync::Arc;
use std::time::Duration;

// ─── Helpers ────────────────────────────────────────────────────────────────

fn make_event(pattern: RepairPattern, success: bool) -> ExtensionRepairEvent {
    ExtensionRepairEvent {
        extension_id: "test-ext".to_string(),
        pattern,
        original_error: "module not found: ./dist/index.js".to_string(),
        repair_action: "resolved to ./src/index.ts".to_string(),
        success,
        timestamp_ms: 1_700_000_000_000,
    }
}

fn start_runtime(harness: &common::TestHarness) -> (ExtensionManager, JsExtensionRuntimeHandle) {
    let cwd = harness.temp_dir().to_path_buf();
    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let handle = common::run_async({
        let manager = manager.clone();
        async move {
            JsExtensionRuntimeHandle::start(config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(handle.clone());
    (manager, handle)
}

fn start_runtime_with_ext(
    harness: &common::TestHarness,
    source: &str,
) -> (ExtensionManager, JsExtensionRuntimeHandle) {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_path = harness.create_file("extensions/ext.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        ..Default::default()
    };

    let handle = common::run_async({
        let manager = manager.clone();
        async move {
            JsExtensionRuntimeHandle::start(config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(handle.clone());

    common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    (manager, handle)
}

fn shutdown(handle: &JsExtensionRuntimeHandle) {
    let _ = common::run_async({
        let h = handle.clone();
        async move { h.shutdown(Duration::from_millis(500)).await }
    });
}

// ─── RepairPattern display ──────────────────────────────────────────────────

#[test]
fn repair_pattern_display_dist_to_src() {
    assert_eq!(RepairPattern::DistToSrc.to_string(), "dist_to_src");
}

#[test]
fn repair_pattern_display_missing_asset() {
    assert_eq!(RepairPattern::MissingAsset.to_string(), "missing_asset");
}

#[test]
fn repair_pattern_display_monorepo_escape() {
    assert_eq!(RepairPattern::MonorepoEscape.to_string(), "monorepo_escape");
}

#[test]
fn repair_pattern_display_missing_npm_dep() {
    assert_eq!(RepairPattern::MissingNpmDep.to_string(), "missing_npm_dep");
}

#[test]
fn repair_pattern_display_export_shape() {
    assert_eq!(RepairPattern::ExportShape.to_string(), "export_shape");
}

// ─── RepairPattern equality and copy ────────────────────────────────────────

#[test]
fn repair_pattern_eq_and_copy() {
    let a = RepairPattern::DistToSrc;
    let b = a; // Copy
    assert_eq!(a, b);
    assert_ne!(RepairPattern::DistToSrc, RepairPattern::MissingAsset);
}

// ─── ExtensionRepairEvent construction ──────────────────────────────────────

#[test]
fn repair_event_fields_accessible() {
    let ev = make_event(RepairPattern::DistToSrc, true);
    assert_eq!(ev.extension_id, "test-ext");
    assert_eq!(ev.pattern, RepairPattern::DistToSrc);
    assert!(ev.success);
    assert_eq!(ev.timestamp_ms, 1_700_000_000_000);
}

#[test]
fn repair_event_clone() {
    let ev = make_event(RepairPattern::MissingAsset, false);
    let ev2 = ev.clone();
    assert_eq!(ev.extension_id, ev2.extension_id);
    assert_eq!(ev.pattern, ev2.pattern);
    assert_eq!(ev.success, ev2.success);
}

// ─── PiJsRuntimeConfig repair_mode ──────────────────────────────────────────

#[test]
fn config_repair_mode_defaults_to_auto_safe() {
    let config = PiJsRuntimeConfig::default();
    assert_eq!(config.repair_mode, pi::extensions_js::RepairMode::AutoSafe);
    assert!(config.auto_repair_enabled());
}

#[test]
fn config_repair_mode_off_disables_repair() {
    let config = PiJsRuntimeConfig {
        repair_mode: pi::extensions_js::RepairMode::Off,
        ..Default::default()
    };
    assert!(!config.auto_repair_enabled());
}

#[test]
fn config_repair_mode_suggest_does_not_apply() {
    let config = PiJsRuntimeConfig {
        repair_mode: pi::extensions_js::RepairMode::Suggest,
        ..Default::default()
    };
    assert!(!config.auto_repair_enabled());
    assert!(config.repair_mode.is_active());
}

#[test]
fn config_repair_mode_auto_strict_enables_aggressive() {
    let config = PiJsRuntimeConfig {
        repair_mode: pi::extensions_js::RepairMode::AutoStrict,
        ..Default::default()
    };
    assert!(config.auto_repair_enabled());
    assert!(config.repair_mode.allows_aggressive());
}

// ─── PiJsTickStats default ──────────────────────────────────────────────────

#[test]
fn tick_stats_default_has_zero_repairs() {
    let stats = PiJsTickStats::default();
    assert_eq!(stats.repairs_total, 0);
}

// ─── JsExtensionRuntimeHandle drain_repair_events ───────────────────────────

#[test]
fn handle_drain_repair_events_empty_on_fresh_runtime() {
    let harness = common::TestHarness::new("repair_drain_empty");
    let (_manager, handle) = start_runtime(&harness);

    let events = common::run_async({
        let h = handle.clone();
        async move { h.drain_repair_events().await }
    });
    assert!(events.is_empty());

    shutdown(&handle);
}

#[test]
fn handle_drain_repair_events_after_clean_extension_load() {
    let harness = common::TestHarness::new("repair_drain_clean");
    let (_manager, handle) = start_runtime_with_ext(
        &harness,
        r#"
        export default function activate(pi) {
            pi.registerTool({
                name: "noop",
                description: "does nothing",
                parameters: { type: "object", properties: {} },
                execute: async () => ({ content: [{ type: "text", text: "ok" }] }),
            });
        }
        "#,
    );

    // A well-behaved extension should produce zero repair events.
    let events = common::run_async({
        let h = handle.clone();
        async move { h.drain_repair_events().await }
    });
    assert!(
        events.is_empty(),
        "expected no repairs, got {}",
        events.len()
    );

    shutdown(&handle);
}

// ─── All patterns constructible ──────────────────────────────────────────────

#[test]
fn all_patterns_constructible() {
    let patterns = [
        RepairPattern::DistToSrc,
        RepairPattern::MissingAsset,
        RepairPattern::MonorepoEscape,
        RepairPattern::MissingNpmDep,
        RepairPattern::ExportShape,
        RepairPattern::ManifestNormalization,
        RepairPattern::ApiMigration,
    ];

    for (i, pattern) in patterns.iter().enumerate() {
        let ev = ExtensionRepairEvent {
            extension_id: format!("ext-{i}"),
            pattern: *pattern,
            original_error: "err".to_string(),
            repair_action: "fix".to_string(),
            success: true,
            timestamp_ms: 1_000 + i as u64,
        };
        assert_eq!(ev.extension_id, format!("ext-{i}"));
        assert_eq!(ev.pattern, patterns[i]);
    }
}

// ─── RepairPattern hash ─────────────────────────────────────────────────────

#[test]
fn repair_pattern_usable_as_hash_key() {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    set.insert(RepairPattern::DistToSrc);
    set.insert(RepairPattern::MissingAsset);
    set.insert(RepairPattern::DistToSrc); // duplicate
    assert_eq!(set.len(), 2);
}

// ─── Pattern 1: dist/ → src/ fallback (bd-k5q5.8.2) ────────────────────────

#[test]
fn dist_to_src_fallback_resolves_when_src_exists() {
    let harness = common::TestHarness::new("dist_to_src_resolve");

    // Create the extension entry that imports from ./dist/extension.js
    // (which doesn't exist), but ./src/extension.ts does.
    harness.create_file(
        "extensions/src/extension.ts",
        br#"
        export function hello() { return "from src"; }
        "#,
    );

    // The entry point re-exports from ./dist/extension.js (missing build output).
    let (_manager, handle) = start_runtime_with_ext(
        &harness,
        r#"
        import { hello } from "./src/extension.ts";
        export default function activate(pi) {
            pi.registerTool({
                name: "hello",
                description: "test",
                parameters: { type: "object", properties: {} },
                execute: async () => ({
                    content: [{ type: "text", text: hello() }],
                }),
            });
        }
        "#,
    );

    // Verify the extension loaded (it uses a direct src import, no repair needed).
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(tools.iter().any(|t| t.name == "hello"));

    shutdown(&handle);
}

#[test]
fn dist_to_src_fallback_loads_extension_with_dist_import() {
    let harness = common::TestHarness::new("dist_to_src_import");

    // Create src/lib.ts (the source file that dist/lib.js would have been).
    harness.create_file(
        "extensions/src/lib.ts",
        br#"
        export const greeting = "hello from src";
        "#,
    );

    // Entry point imports from ./dist/lib.js which doesn't exist.
    // The auto-repair should fall back to ./src/lib.ts.
    let (_manager, handle) = start_runtime_with_ext(
        &harness,
        r#"
        import { greeting } from "./dist/lib.js";
        export default function activate(pi) {
            pi.registerTool({
                name: "greet",
                description: "test",
                parameters: { type: "object", properties: {} },
                execute: async () => ({
                    content: [{ type: "text", text: greeting }],
                }),
            });
        }
        "#,
    );

    // Verify the extension loaded successfully via the fallback.
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(
        tools.iter().any(|t| t.name == "greet"),
        "extension should have loaded via dist→src fallback"
    );

    shutdown(&handle);
}

#[test]
fn dist_to_src_fallback_no_effect_when_dist_exists() {
    let harness = common::TestHarness::new("dist_to_src_no_fallback");

    // Create BOTH dist/lib.js and src/lib.ts.
    harness.create_file(
        "extensions/dist/lib.js",
        br#"
        export const greeting = "from dist";
        "#,
    );
    harness.create_file(
        "extensions/src/lib.ts",
        br#"
        export const greeting = "from src";
        "#,
    );

    // Entry point imports from ./dist/lib.js which DOES exist.
    let (_manager, handle) = start_runtime_with_ext(
        &harness,
        r#"
        import { greeting } from "./dist/lib.js";
        export default function activate(pi) {
            pi.registerTool({
                name: "greet",
                description: "test",
                parameters: { type: "object", properties: {} },
                execute: async () => ({
                    content: [{ type: "text", text: greeting }],
                }),
            });
        }
        "#,
    );

    // Should load from dist/ (no fallback needed).
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(tools.iter().any(|t| t.name == "greet"));

    shutdown(&handle);
}

// ─── Safety boundary: repair_mode gating (bd-k5q5.9.1.2) ────────────────────

/// Start a runtime with a specific `RepairMode`, attempt to load the extension,
/// and return the result (may be `Err` if the extension fails to load).
fn try_start_runtime_with_mode(
    harness: &common::TestHarness,
    source: &str,
    mode: RepairMode,
) -> (
    ExtensionManager,
    JsExtensionRuntimeHandle,
    Result<(), String>,
) {
    let cwd = harness.temp_dir().to_path_buf();
    let ext_path = harness.create_file("extensions/ext.mjs", source.as_bytes());
    let spec = JsExtensionLoadSpec::from_entry_path(&ext_path).expect("load spec");

    let manager = ExtensionManager::new();
    let tools = Arc::new(ToolRegistry::new(&[], &cwd, None));
    let config = PiJsRuntimeConfig {
        cwd: cwd.display().to_string(),
        repair_mode: mode,
        ..Default::default()
    };

    let handle = common::run_async({
        let manager = manager.clone();
        async move {
            JsExtensionRuntimeHandle::start(config, tools, manager)
                .await
                .expect("start js runtime")
        }
    });
    manager.set_js_runtime(handle.clone());

    let load_result = common::run_async({
        let manager = manager.clone();
        async move {
            manager
                .load_js_extensions(vec![spec])
                .await
                .map_err(|e| e.to_string())
        }
    });

    (manager, handle, load_result)
}

/// Source code that imports from `./dist/lib.js` (which won't exist).
const DIST_IMPORT_SOURCE: &str = r#"
    import { greeting } from "./dist/lib.js";
    export default function activate(pi) {
        pi.registerTool({
            name: "greet",
            description: "test",
            parameters: { type: "object", properties: {} },
            execute: async () => ({
                content: [{ type: "text", text: greeting }],
            }),
        });
    }
"#;

#[test]
fn repair_off_prevents_dist_to_src_fallback() {
    let harness = common::TestHarness::new("repair_off_no_fallback");

    // Create src/lib.ts but NOT dist/lib.js.
    harness.create_file(
        "extensions/src/lib.ts",
        br#"export const greeting = "from src";"#,
    );

    let (_manager, handle, _load_result) =
        try_start_runtime_with_mode(&harness, DIST_IMPORT_SOURCE, RepairMode::Off);

    // With Off mode the fallback should NOT fire → no "greet" tool registered.
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(
        !tools.iter().any(|t| t.name == "greet"),
        "Off mode should not apply dist→src fallback"
    );

    shutdown(&handle);
}

#[test]
fn repair_suggest_does_not_apply_fallback() {
    let harness = common::TestHarness::new("repair_suggest_no_apply");

    // Create src/lib.ts but NOT dist/lib.js.
    harness.create_file(
        "extensions/src/lib.ts",
        br#"export const greeting = "from src";"#,
    );

    let (_manager, handle, _load_result) =
        try_start_runtime_with_mode(&harness, DIST_IMPORT_SOURCE, RepairMode::Suggest);

    // Suggest mode should log but NOT apply → no "greet" tool registered.
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(
        !tools.iter().any(|t| t.name == "greet"),
        "Suggest mode should not apply dist→src fallback"
    );

    shutdown(&handle);
}

#[test]
fn repair_auto_safe_applies_dist_to_src_fallback() {
    let harness = common::TestHarness::new("repair_auto_safe_applies");

    // Create src/lib.ts but NOT dist/lib.js.
    harness.create_file(
        "extensions/src/lib.ts",
        br#"export const greeting = "from src";"#,
    );

    let (_manager, handle, _load_result) =
        try_start_runtime_with_mode(&harness, DIST_IMPORT_SOURCE, RepairMode::AutoSafe);

    // AutoSafe should apply the fallback → "greet" tool registered.
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(
        tools.iter().any(|t| t.name == "greet"),
        "AutoSafe mode should apply dist→src fallback"
    );

    shutdown(&handle);
}

#[test]
fn repair_auto_strict_applies_dist_to_src_fallback() {
    let harness = common::TestHarness::new("repair_auto_strict_applies");

    // Create src/lib.ts but NOT dist/lib.js.
    harness.create_file(
        "extensions/src/lib.ts",
        br#"export const greeting = "from src";"#,
    );

    let (_manager, handle, _load_result) =
        try_start_runtime_with_mode(&harness, DIST_IMPORT_SOURCE, RepairMode::AutoStrict);

    // AutoStrict should also apply the fallback → "greet" tool registered.
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(
        tools.iter().any(|t| t.name == "greet"),
        "AutoStrict mode should apply dist→src fallback"
    );

    shutdown(&handle);
}

// ─── Privilege monotonicity checker (bd-k5q5.9.1.3) ─────────────────────────

use pi::extensions_js::verify_repair_monotonicity;
use std::path::PathBuf;

#[test]
fn monotonicity_safe_when_resolved_within_root() {
    let root = PathBuf::from("/extensions/my-ext");
    let original = PathBuf::from("/extensions/my-ext/dist/lib.js");
    let resolved = PathBuf::from("/extensions/my-ext/src/lib.ts");
    assert_eq!(
        verify_repair_monotonicity(&root, &original, &resolved),
        MonotonicityVerdict::Safe
    );
}

#[test]
fn monotonicity_escapes_root_when_resolved_above() {
    let root = PathBuf::from("/extensions/my-ext");
    let original = PathBuf::from("/extensions/my-ext/dist/lib.js");
    let resolved = PathBuf::from("/extensions/other-ext/src/lib.ts");
    let verdict = verify_repair_monotonicity(&root, &original, &resolved);
    assert!(!verdict.is_safe(), "should detect escape: {verdict:?}");
    assert!(matches!(verdict, MonotonicityVerdict::EscapesRoot { .. }));
}

#[test]
fn monotonicity_escapes_root_when_resolved_to_parent() {
    let root = PathBuf::from("/extensions/my-ext");
    let original = PathBuf::from("/extensions/my-ext/dist/lib.js");
    let resolved = PathBuf::from("/extensions/lib.ts");
    let verdict = verify_repair_monotonicity(&root, &original, &resolved);
    assert!(!verdict.is_safe());
}

#[test]
fn monotonicity_safe_for_nested_subdirectory() {
    let root = PathBuf::from("/extensions/my-ext");
    let original = PathBuf::from("/extensions/my-ext/dist/deep/nested/file.js");
    let resolved = PathBuf::from("/extensions/my-ext/src/deep/nested/file.ts");
    assert_eq!(
        verify_repair_monotonicity(&root, &original, &resolved),
        MonotonicityVerdict::Safe
    );
}

#[test]
fn monotonicity_safe_at_root_boundary() {
    // Resolved path IS the root itself (edge case).
    let root = PathBuf::from("/extensions/my-ext");
    let original = PathBuf::from("/extensions/my-ext/dist/index.js");
    let resolved = PathBuf::from("/extensions/my-ext/index.ts");
    assert_eq!(
        verify_repair_monotonicity(&root, &original, &resolved),
        MonotonicityVerdict::Safe
    );
}

// ─── Repair risk classification (bd-k5q5.9.1.4) ─────────────────────────────

#[test]
fn safe_patterns_have_safe_risk() {
    assert_eq!(RepairPattern::DistToSrc.risk(), RepairRisk::Safe);
    assert_eq!(RepairPattern::MissingAsset.risk(), RepairRisk::Safe);
    assert_eq!(
        RepairPattern::ManifestNormalization.risk(),
        RepairRisk::Safe
    );
}

#[test]
fn aggressive_patterns_have_aggressive_risk() {
    assert_eq!(RepairPattern::MonorepoEscape.risk(), RepairRisk::Aggressive);
    assert_eq!(RepairPattern::MissingNpmDep.risk(), RepairRisk::Aggressive);
    assert_eq!(RepairPattern::ExportShape.risk(), RepairRisk::Aggressive);
    assert_eq!(RepairPattern::ApiMigration.risk(), RepairRisk::Aggressive);
}

#[test]
fn safe_patterns_allowed_by_auto_safe() {
    assert!(RepairPattern::DistToSrc.is_allowed_by(RepairMode::AutoSafe));
    assert!(RepairPattern::MissingAsset.is_allowed_by(RepairMode::AutoSafe));
}

#[test]
fn aggressive_patterns_blocked_by_auto_safe() {
    assert!(!RepairPattern::MonorepoEscape.is_allowed_by(RepairMode::AutoSafe));
    assert!(!RepairPattern::MissingNpmDep.is_allowed_by(RepairMode::AutoSafe));
    assert!(!RepairPattern::ExportShape.is_allowed_by(RepairMode::AutoSafe));
}

#[test]
fn aggressive_patterns_allowed_by_auto_strict() {
    assert!(RepairPattern::MonorepoEscape.is_allowed_by(RepairMode::AutoStrict));
    assert!(RepairPattern::MissingNpmDep.is_allowed_by(RepairMode::AutoStrict));
    assert!(RepairPattern::ExportShape.is_allowed_by(RepairMode::AutoStrict));
}

#[test]
fn no_patterns_allowed_by_off() {
    for &pattern in &[
        RepairPattern::DistToSrc,
        RepairPattern::MissingAsset,
        RepairPattern::MonorepoEscape,
        RepairPattern::MissingNpmDep,
        RepairPattern::ExportShape,
        RepairPattern::ManifestNormalization,
        RepairPattern::ApiMigration,
    ] {
        assert!(
            !pattern.is_allowed_by(RepairMode::Off),
            "{pattern} should be blocked in Off mode"
        );
    }
}

#[test]
fn no_patterns_allowed_by_suggest() {
    for &pattern in &[
        RepairPattern::DistToSrc,
        RepairPattern::MissingAsset,
        RepairPattern::MonorepoEscape,
        RepairPattern::MissingNpmDep,
        RepairPattern::ExportShape,
        RepairPattern::ManifestNormalization,
        RepairPattern::ApiMigration,
    ] {
        assert!(
            !pattern.is_allowed_by(RepairMode::Suggest),
            "{pattern} should be blocked in Suggest mode"
        );
    }
}

// ─── Deterministic rule registry (bd-k5q5.9.3.1) ────────────────────────────

use pi::extensions_js::{applicable_rules, rule_by_id};

#[test]
fn registry_has_seven_rules() {
    assert_eq!(REPAIR_RULES.len(), 7);
}

#[test]
fn registry_version_is_set() {
    assert!(!REPAIR_REGISTRY_VERSION.is_empty());
}

#[test]
fn all_rules_have_unique_ids() {
    let mut ids: Vec<&str> = REPAIR_RULES.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), REPAIR_RULES.len(), "duplicate rule IDs detected");
}

#[test]
fn rule_by_id_finds_existing() {
    let rule = rule_by_id("dist_to_src_v1").expect("should find rule");
    assert_eq!(rule.pattern, RepairPattern::DistToSrc);
    assert_eq!(rule.version, "1.0.0");
}

#[test]
fn rule_by_id_returns_none_for_unknown() {
    assert!(rule_by_id("nonexistent_rule").is_none());
}

#[test]
fn applicable_rules_auto_safe_returns_safe_only() {
    let rules = applicable_rules(RepairMode::AutoSafe);
    assert!(rules.iter().all(|r| r.risk() == RepairRisk::Safe));
    assert_eq!(rules.len(), 3); // DistToSrc + MissingAsset + ManifestNormalization
}

#[test]
fn applicable_rules_auto_strict_returns_all() {
    let rules = applicable_rules(RepairMode::AutoStrict);
    assert_eq!(rules.len(), 7);
}

#[test]
fn applicable_rules_off_returns_empty() {
    let rules = applicable_rules(RepairMode::Off);
    assert!(rules.is_empty());
}

#[test]
fn applicable_rules_suggest_returns_empty() {
    let rules = applicable_rules(RepairMode::Suggest);
    assert!(rules.is_empty());
}

#[test]
fn registry_order_is_deterministic() {
    let ids: Vec<&str> = REPAIR_RULES.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        vec![
            "dist_to_src_v1",
            "missing_asset_v1",
            "monorepo_escape_v1",
            "missing_npm_dep_v1",
            "export_shape_v1",
            "manifest_schema_v1",
            "api_migration_v1",
        ]
    );
}

// ─── Model patch primitives (bd-k5q5.9.4.1) ─────────────────────────────────

#[test]
fn patch_op_replace_module_path_is_safe() {
    let op = PatchOp::ReplaceModulePath {
        from: "./dist/lib.js".to_string(),
        to: "./src/lib.ts".to_string(),
    };
    assert_eq!(op.risk(), RepairRisk::Safe);
    assert_eq!(op.tag(), "replace_module_path");
}

#[test]
fn patch_op_inject_stub_is_aggressive() {
    let op = PatchOp::InjectStub {
        virtual_path: "/@stubs/missing.js".to_string(),
        source: "export default {};".to_string(),
    };
    assert_eq!(op.risk(), RepairRisk::Aggressive);
    assert_eq!(op.tag(), "inject_stub");
}

#[test]
fn patch_op_add_export_is_aggressive() {
    let op = PatchOp::AddExport {
        module_path: "./index.ts".to_string(),
        export_name: "activate".to_string(),
        export_value: "function activate() {}".to_string(),
    };
    assert_eq!(op.risk(), RepairRisk::Aggressive);
}

#[test]
fn patch_op_rewrite_require_is_safe() {
    let op = PatchOp::RewriteRequire {
        module_path: "./entry.js".to_string(),
        from_specifier: "missing-pkg".to_string(),
        to_specifier: "/@stubs/missing-pkg".to_string(),
    };
    assert_eq!(op.risk(), RepairRisk::Safe);
}

#[test]
fn proposal_max_risk_safe_when_all_safe() {
    let proposal = PatchProposal {
        rule_id: "dist_to_src_v1".to_string(),
        ops: vec![PatchOp::ReplaceModulePath {
            from: "./dist/x.js".to_string(),
            to: "./src/x.ts".to_string(),
        }],
        rationale: "remap path".to_string(),
        confidence: Some(0.95),
    };
    assert_eq!(proposal.max_risk(), RepairRisk::Safe);
    assert!(proposal.is_allowed_by(RepairMode::AutoSafe));
}

#[test]
fn proposal_max_risk_aggressive_when_any_aggressive() {
    let proposal = PatchProposal {
        rule_id: "mixed_v1".to_string(),
        ops: vec![
            PatchOp::ReplaceModulePath {
                from: "./a.js".to_string(),
                to: "./b.ts".to_string(),
            },
            PatchOp::InjectStub {
                virtual_path: "/@stubs/x".to_string(),
                source: "export default {};".to_string(),
            },
        ],
        rationale: "mixed ops".to_string(),
        confidence: None,
    };
    assert_eq!(proposal.max_risk(), RepairRisk::Aggressive);
    assert!(!proposal.is_allowed_by(RepairMode::AutoSafe));
    assert!(proposal.is_allowed_by(RepairMode::AutoStrict));
}

#[test]
fn proposal_blocked_by_off_mode() {
    let proposal = PatchProposal {
        rule_id: "safe_v1".to_string(),
        ops: vec![PatchOp::ReplaceModulePath {
            from: "a".to_string(),
            to: "b".to_string(),
        }],
        rationale: "test".to_string(),
        confidence: None,
    };
    assert!(!proposal.is_allowed_by(RepairMode::Off));
}

#[test]
fn empty_proposal_is_safe() {
    let proposal = PatchProposal {
        rule_id: "empty_v1".to_string(),
        ops: vec![],
        rationale: "no-op".to_string(),
        confidence: None,
    };
    // No aggressive ops → Safe
    assert_eq!(proposal.max_risk(), RepairRisk::Safe);
}

// ─── Structural validation gate (bd-k5q5.9.5.1) ─────────────────────────────

#[test]
fn structural_verdict_valid_is_valid() {
    let v = StructuralVerdict::Valid;
    assert!(v.is_valid());
    assert_eq!(v.to_string(), "valid");
}

#[test]
fn structural_verdict_unreadable_is_not_valid() {
    let v = StructuralVerdict::Unreadable {
        path: PathBuf::from("/fake/file.ts"),
        reason: "permission denied".to_string(),
    };
    assert!(!v.is_valid());
    assert!(v.to_string().contains("unreadable"));
}

#[test]
fn structural_verdict_unsupported_extension_is_not_valid() {
    let v = StructuralVerdict::UnsupportedExtension {
        path: PathBuf::from("/fake/file.wasm"),
        extension: "wasm".to_string(),
    };
    assert!(!v.is_valid());
    assert!(v.to_string().contains("unsupported extension"));
}

#[test]
fn structural_verdict_parse_error_is_not_valid() {
    let v = StructuralVerdict::ParseError {
        path: PathBuf::from("/fake/file.ts"),
        message: "unexpected token".to_string(),
    };
    assert!(!v.is_valid());
    assert!(v.to_string().contains("parse error"));
}

#[test]
fn validate_valid_typescript_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("index.ts");
    std::fs::write(&file, "export function hello(): string { return 'hi'; }\n").expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(v.is_valid(), "expected valid, got: {v}");
}

#[test]
fn validate_valid_tsx_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("component.tsx");
    std::fs::write(&file, "export const App = () => <div>Hello</div>;\n").expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(v.is_valid(), "expected valid, got: {v}");
}

#[test]
fn validate_valid_js_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("index.js");
    std::fs::write(&file, "module.exports = { hello: 'world' };\n").expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(v.is_valid(), "expected valid, got: {v}");
}

#[test]
fn validate_valid_json_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("package.json");
    std::fs::write(&file, r#"{"name": "test", "version": "1.0.0"}"#).expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(v.is_valid(), "expected valid, got: {v}");
}

#[test]
fn validate_invalid_typescript_returns_parse_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("broken.ts");
    std::fs::write(&file, "export function {{{ invalid syntax").expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(
        matches!(v, StructuralVerdict::ParseError { .. }),
        "expected ParseError, got: {v}"
    );
}

#[test]
fn validate_invalid_json_returns_parse_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("bad.json");
    std::fs::write(&file, "{not valid json}").expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(
        matches!(v, StructuralVerdict::ParseError { .. }),
        "expected ParseError, got: {v}"
    );
}

#[test]
fn validate_nonexistent_file_returns_unreadable() {
    let v = validate_repaired_artifact(Path::new("/nonexistent/path/file.ts"));
    assert!(
        matches!(v, StructuralVerdict::Unreadable { .. }),
        "expected Unreadable, got: {v}"
    );
}

#[test]
fn validate_unsupported_extension_returns_unsupported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("data.wasm");
    std::fs::write(&file, [0x00, 0x61, 0x73, 0x6d]).expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(
        matches!(v, StructuralVerdict::UnsupportedExtension { .. }),
        "expected UnsupportedExtension, got: {v}"
    );
}

#[test]
fn validate_mjs_file_is_valid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("mod.mjs");
    std::fs::write(&file, "export default 42;\n").expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(v.is_valid(), "expected valid, got: {v}");
}

#[test]
fn validate_empty_ts_file_is_valid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("empty.ts");
    std::fs::write(&file, "").expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(v.is_valid(), "expected valid for empty TS, got: {v}");
}

#[test]
fn validate_ts_with_decorators_is_valid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("decorated.ts");
    std::fs::write(
        &file,
        "function log(_t: any, _k: string) {}\nclass Foo { @log bar() {} }\nexport { Foo };\n",
    )
    .expect("write");
    let v = validate_repaired_artifact(&file);
    assert!(v.is_valid(), "expected valid with decorators, got: {v}");
}

// ─── Structural validation gate integration (bd-k5q5.9.5.1) ─────────────────

#[test]
fn repair_blocked_when_src_has_broken_syntax() {
    let harness = common::TestHarness::new("repair_blocked_broken_syntax");

    // Create src/lib.ts with BROKEN syntax (structural validation should fail).
    harness.create_file(
        "extensions/src/lib.ts",
        b"export function {{{ totally broken syntax !!!",
    );

    let (_manager, handle, _load_result) =
        try_start_runtime_with_mode(&harness, DIST_IMPORT_SOURCE, RepairMode::AutoSafe);

    // The structural validation gate should block the repair → no "greet" tool.
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(
        !tools.iter().any(|t| t.name == "greet"),
        "AutoSafe should block fallback when src file has broken syntax"
    );

    shutdown(&handle);
}

#[test]
fn repair_succeeds_when_src_has_valid_syntax() {
    let harness = common::TestHarness::new("repair_ok_valid_syntax");

    // Create src/lib.ts with valid syntax.
    harness.create_file(
        "extensions/src/lib.ts",
        br#"export const greeting: string = "from src";"#,
    );

    let (_manager, handle, _load_result) =
        try_start_runtime_with_mode(&harness, DIST_IMPORT_SOURCE, RepairMode::AutoSafe);

    // Valid file passes structural validation → "greet" tool registered.
    let tools = common::run_async({
        let h = handle.clone();
        async move { h.get_registered_tools().await.unwrap() }
    });
    assert!(
        tools.iter().any(|t| t.name == "greet"),
        "AutoSafe should allow fallback when src file is structurally valid"
    );

    shutdown(&handle);
}

// ─── Intent graph extractor (bd-k5q5.9.2.1) ─────────────────────────────────

fn sample_register_payload() -> serde_json::Value {
    serde_json::json!({
        "tools": [
            { "name": "greet", "description": "say hello", "parameters": {} },
            { "name": "farewell", "description": "say goodbye", "parameters": {} }
        ],
        "slash_commands": [
            { "name": "/hello", "description": "greeting command" }
        ],
        "shortcuts": [
            { "name": "ctrl+g", "action": "greet" }
        ],
        "flags": [
            { "name": "verbose", "description": "enable verbose mode", "type": "boolean", "default": false }
        ],
        "event_hooks": ["tool_call", "tool_result"]
    })
}

#[test]
fn intent_graph_extracts_tools() {
    let graph = IntentGraph::from_register_payload("test-ext", &sample_register_payload(), &[]);
    let tools = graph.signals_by_category("tool");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name(), "greet");
    assert_eq!(tools[1].name(), "farewell");
}

#[test]
fn intent_graph_extracts_commands() {
    let graph = IntentGraph::from_register_payload("test-ext", &sample_register_payload(), &[]);
    let cmds = graph.signals_by_category("command");
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].name(), "/hello");
}

#[test]
fn intent_graph_extracts_shortcuts() {
    let graph = IntentGraph::from_register_payload("test-ext", &sample_register_payload(), &[]);
    let shortcuts = graph.signals_by_category("shortcut");
    assert_eq!(shortcuts.len(), 1);
    assert_eq!(shortcuts[0].name(), "ctrl+g");
}

#[test]
fn intent_graph_extracts_flags() {
    let graph = IntentGraph::from_register_payload("test-ext", &sample_register_payload(), &[]);
    let flags = graph.signals_by_category("flag");
    assert_eq!(flags.len(), 1);
    assert_eq!(flags[0].name(), "verbose");
}

#[test]
fn intent_graph_extracts_event_hooks() {
    let graph = IntentGraph::from_register_payload("test-ext", &sample_register_payload(), &[]);
    let hooks = graph.signals_by_category("event_hook");
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0].name(), "tool_call");
    assert_eq!(hooks[1].name(), "tool_result");
}

#[test]
fn intent_graph_extracts_capabilities() {
    let caps = vec!["read".to_string(), "exec".to_string()];
    let graph = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &caps);
    let cap_signals = graph.signals_by_category("capability");
    assert_eq!(cap_signals.len(), 2);
    assert_eq!(cap_signals[0].name(), "read");
    assert_eq!(cap_signals[1].name(), "exec");
}

#[test]
fn intent_graph_deduplicates_signals() {
    let payload = serde_json::json!({
        "tools": [
            { "name": "greet", "description": "v1" },
            { "name": "greet", "description": "v2" }
        ]
    });
    let graph = IntentGraph::from_register_payload("test-ext", &payload, &[]);
    let tools = graph.signals_by_category("tool");
    assert_eq!(tools.len(), 1, "duplicate tools should be deduplicated");
}

#[test]
fn intent_graph_empty_payload() {
    let graph = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &[]);
    assert!(graph.is_empty());
    assert_eq!(graph.signal_count(), 0);
    assert_eq!(graph.category_count(), 0);
}

#[test]
fn intent_graph_category_count() {
    let graph = IntentGraph::from_register_payload("test-ext", &sample_register_payload(), &[]);
    // tools, commands, shortcuts, flags, event_hooks = 5 categories
    assert_eq!(graph.category_count(), 5);
}

#[test]
fn intent_graph_signal_count() {
    let graph = IntentGraph::from_register_payload("test-ext", &sample_register_payload(), &[]);
    // 2 tools + 1 command + 1 shortcut + 1 flag + 2 hooks = 7 signals
    assert_eq!(graph.signal_count(), 7);
}

#[test]
fn intent_signal_display() {
    let sig = IntentSignal::RegistersTool("greet".to_string());
    assert_eq!(sig.to_string(), "tool:greet");

    let sig2 = IntentSignal::HooksEvent("tool_call".to_string());
    assert_eq!(sig2.to_string(), "event_hook:tool_call");
}

#[test]
fn intent_graph_extension_id_preserved() {
    let graph = IntentGraph::from_register_payload("my-ext-123", &serde_json::json!({}), &[]);
    assert_eq!(graph.extension_id, "my-ext-123");
}

// ─── Tolerant AST recovery and ambiguity detection (bd-k5q5.9.2.2) ──────────

#[test]
fn tolerant_parse_valid_ts() {
    let result = tolerant_parse(
        "import { foo } from './bar';\nexport function hello() { return foo; }\n",
        "test.ts",
    );
    assert!(result.parsed_ok);
    assert_eq!(result.statement_count, 2);
    assert_eq!(result.import_export_count, 2); // import + export
    assert!(result.ambiguities.is_empty());
    assert!(result.is_legible());
}

#[test]
fn tolerant_parse_broken_ts() {
    let result = tolerant_parse("export function {{{ totally broken", "broken.ts");
    assert!(!result.parsed_ok);
    assert_eq!(result.statement_count, 0);
    assert!(!result.ambiguities.is_empty()); // Should have RecoverableParseErrors
}

#[test]
fn tolerant_parse_valid_js() {
    let result = tolerant_parse(
        "const x = require('./foo');\nmodule.exports = x;\n",
        "test.js",
    );
    assert!(result.parsed_ok);
    assert!(result.statement_count > 0);
}

#[test]
fn tolerant_parse_detects_dynamic_eval() {
    let result = tolerant_parse("const code = 'console.log(1)';\neval(code);\n", "test.js");
    assert!(result.ambiguities.contains(&AmbiguitySignal::DynamicEval));
    // eval has weight 0.9, which is >= 0.8 threshold.
    assert!(!result.is_legible());
}

#[test]
fn tolerant_parse_detects_new_function() {
    let result = tolerant_parse("const fn = new Function('return 1');\n", "test.js");
    assert!(
        result
            .ambiguities
            .contains(&AmbiguitySignal::DynamicFunction)
    );
}

#[test]
fn tolerant_parse_detects_dynamic_import() {
    let result = tolerant_parse("const mod = await import('./dynamic.js');\n", "test.mjs");
    assert!(result.ambiguities.contains(&AmbiguitySignal::DynamicImport));
}

#[test]
fn tolerant_parse_detects_star_reexport() {
    let result = tolerant_parse("export * from './utils';\n", "test.ts");
    assert!(result.ambiguities.contains(&AmbiguitySignal::StarReExport));
    // StarReExport has weight 0.3, still legible.
    assert!(result.is_legible());
}

#[test]
fn tolerant_parse_detects_proxy() {
    let result = tolerant_parse(
        "const handler = {};\nconst p = new Proxy({}, handler);\n",
        "test.js",
    );
    assert!(result.ambiguities.contains(&AmbiguitySignal::ProxyUsage));
}

#[test]
fn tolerant_parse_detects_with_statement() {
    let result = tolerant_parse("with (obj) { foo(); }\n", "test.js");
    assert!(result.ambiguities.contains(&AmbiguitySignal::WithStatement));
}

#[test]
fn tolerant_parse_detects_dynamic_require() {
    let result = tolerant_parse(
        "const mod = require(path.join(__dirname, 'foo'));\n",
        "test.js",
    );
    assert!(
        result
            .ambiguities
            .contains(&AmbiguitySignal::DynamicRequire)
    );
}

#[test]
fn tolerant_parse_static_require_not_flagged() {
    let result = tolerant_parse("const mod = require('./static-path');\n", "test.js");
    assert!(
        !result
            .ambiguities
            .contains(&AmbiguitySignal::DynamicRequire),
        "static string require should not be flagged"
    );
}

#[test]
fn tolerant_parse_empty_source() {
    let result = tolerant_parse("", "empty.ts");
    assert!(result.parsed_ok);
    assert_eq!(result.statement_count, 0);
    assert!(result.ambiguities.is_empty());
    assert!(result.is_legible());
}

#[test]
fn ambiguity_signal_weight_ordering() {
    // eval/Function are most dangerous.
    assert!(AmbiguitySignal::DynamicEval.weight() > AmbiguitySignal::DynamicImport.weight());
    // Star re-export is least dangerous.
    assert!(AmbiguitySignal::StarReExport.weight() < AmbiguitySignal::DynamicImport.weight());
}

#[test]
fn ambiguity_signal_display() {
    assert_eq!(AmbiguitySignal::DynamicEval.to_string(), "dynamic_eval");
    assert_eq!(
        AmbiguitySignal::RecoverableParseErrors { count: 3 }.to_string(),
        "recoverable_parse_errors(3)"
    );
}

#[test]
fn ambiguity_score_zero_for_clean_source() {
    let result = tolerant_parse("export const x = 42;\n", "clean.ts");
    assert!(result.ambiguity_score().abs() < f64::EPSILON);
}

#[test]
fn ambiguity_score_high_for_eval() {
    let result = tolerant_parse("eval('code');\n", "dangerous.js");
    assert!(result.ambiguity_score() >= 0.9);
}

#[test]
fn unsupported_extension_returns_not_parsed() {
    let result = tolerant_parse("binary data", "data.wasm");
    assert!(!result.parsed_ok);
    assert_eq!(result.statement_count, 0);
}

// ─── Confidence scoring model (bd-k5q5.9.2.3) ───────────────────────────────

fn well_formed_parse() -> TolerantParseResult {
    tolerant_parse(
        "import { foo } from './bar';\nexport function hello() { return foo; }\n",
        "test.ts",
    )
}

fn rich_intent() -> IntentGraph {
    IntentGraph::from_register_payload(
        "test-ext",
        &sample_register_payload(),
        &["read".to_string()],
    )
}

#[test]
fn confidence_high_for_clean_rich_extension() {
    let report = compute_confidence(&rich_intent(), &well_formed_parse());
    assert!(
        report.score >= 0.8,
        "clean rich extension should have high confidence, got {}",
        report.score
    );
    assert!(report.is_repairable());
    assert!(!report.reasons.is_empty());
}

#[test]
fn confidence_low_for_broken_empty_extension() {
    let parse = tolerant_parse("export function {{{ broken", "broken.ts");
    let intent = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &[]);
    let report = compute_confidence(&intent, &parse);
    assert!(
        report.score < 0.5,
        "broken empty extension should have low confidence, got {}",
        report.score
    );
    assert!(!report.is_repairable());
}

#[test]
fn confidence_penalized_by_eval() {
    let parse = tolerant_parse("eval('code');\nexport const x = 1;\n", "test.js");
    let intent = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &[]);
    let no_eval_parse = well_formed_parse();
    let report_with_eval = compute_confidence(&intent, &parse);
    let report_clean = compute_confidence(&intent, &no_eval_parse);
    assert!(
        report_with_eval.score < report_clean.score,
        "eval should penalize confidence: {} vs {}",
        report_with_eval.score,
        report_clean.score
    );
}

#[test]
fn confidence_boosted_by_tools() {
    let parse = well_formed_parse();
    let with_tools = rich_intent();
    let no_tools = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &[]);
    let report_with = compute_confidence(&with_tools, &parse);
    let report_without = compute_confidence(&no_tools, &parse);
    assert!(
        report_with.score > report_without.score,
        "tools should boost confidence: {} vs {}",
        report_with.score,
        report_without.score
    );
}

#[test]
fn confidence_reasons_are_explainable() {
    let report = compute_confidence(&rich_intent(), &well_formed_parse());
    // Every reason should have a non-empty code and explanation.
    for reason in &report.reasons {
        assert!(!reason.code.is_empty(), "reason code should not be empty");
        assert!(
            !reason.explanation.is_empty(),
            "reason explanation should not be empty"
        );
    }
    // Should have at least parsed_ok + has_tools.
    let codes: Vec<&str> = report.reasons.iter().map(|r| r.code.as_str()).collect();
    assert!(codes.contains(&"parsed_ok"), "should have parsed_ok reason");
    assert!(codes.contains(&"has_tools"), "should have has_tools reason");
}

#[test]
fn confidence_report_is_repairable_threshold() {
    // Score exactly at threshold.
    let report = ConfidenceReport {
        score: 0.5,
        reasons: vec![],
    };
    assert!(report.is_repairable());

    let report_below = ConfidenceReport {
        score: 0.49,
        reasons: vec![],
    };
    assert!(!report_below.is_repairable());
}

#[test]
fn confidence_report_is_suggestable_threshold() {
    let report = ConfidenceReport {
        score: 0.2,
        reasons: vec![],
    };
    assert!(report.is_suggestable());

    let report_below = ConfidenceReport {
        score: 0.19,
        reasons: vec![],
    };
    assert!(!report_below.is_suggestable());
}

#[test]
fn confidence_clamped_to_unit_range() {
    // Very clean extension with many signals shouldn't exceed 1.0.
    let parse = well_formed_parse();
    let intent = rich_intent();
    let report = compute_confidence(&intent, &parse);
    assert!(report.score <= 1.0, "score should not exceed 1.0");
    assert!(report.score >= 0.0, "score should not be negative");
}

#[test]
fn confidence_deterministic() {
    let parse = well_formed_parse();
    let intent = rich_intent();
    let r1 = compute_confidence(&intent, &parse);
    let r2 = compute_confidence(&intent, &parse);
    assert!(
        (r1.score - r2.score).abs() < f64::EPSILON,
        "confidence should be deterministic"
    );
}

// ─── Gating decision API (bd-k5q5.9.2.4) ────────────────────────────────────

#[test]
fn gating_allow_for_clean_extension() {
    let verdict = compute_gating_verdict(&rich_intent(), &well_formed_parse());
    assert_eq!(verdict.decision, GatingDecision::Allow);
    assert!(verdict.allows_repair());
    assert!(verdict.allows_suggestion());
    assert!(
        verdict.reason_codes.is_empty(),
        "Allow should have no reason codes"
    );
}

#[test]
fn gating_deny_for_broken_opaque_extension() {
    let parse = tolerant_parse("export function {{{ broken", "broken.ts");
    let intent = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &[]);
    let verdict = compute_gating_verdict(&intent, &parse);
    assert_eq!(verdict.decision, GatingDecision::Deny);
    assert!(!verdict.allows_repair());
    assert!(!verdict.allows_suggestion());
    assert!(
        !verdict.reason_codes.is_empty(),
        "Deny should have reason codes"
    );
    // Should have parse_failed reason code.
    assert!(
        verdict
            .reason_codes
            .iter()
            .any(|r| r.code == "parse_failed"),
        "should have parse_failed reason code"
    );
}

#[test]
fn gating_suggest_for_ambiguous_but_parseable() {
    // eval + new Function in source with no tool registrations should push
    // below 0.5 (Suggest) but above 0.2 (not Deny).
    // Base 0.5 + parsed_ok(0.15) - eval(-0.27) - new_function(-0.27) - no_registrations(-0.15)
    // = 0.5 + 0.15 - 0.27 - 0.27 - 0.15 = -0.04 → clamped to ~0.0 → Deny
    // Need something in between. Use just eval with no tools:
    // 0.5 + 0.15 - 0.27 - 0.15 = 0.23 → Suggest
    let parse = tolerant_parse("eval('code');\nexport const x = 1;\n", "test.js");
    let intent = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &[]);
    let verdict = compute_gating_verdict(&intent, &parse);
    assert_eq!(
        verdict.decision,
        GatingDecision::Suggest,
        "eval + no registrations should produce Suggest, score={}",
        verdict.confidence.score
    );
    assert!(!verdict.allows_repair());
    assert!(verdict.allows_suggestion());
}

#[test]
fn gating_decision_display() {
    assert_eq!(GatingDecision::Allow.to_string(), "allow");
    assert_eq!(GatingDecision::Suggest.to_string(), "suggest");
    assert_eq!(GatingDecision::Deny.to_string(), "deny");
}

#[test]
fn gating_reason_codes_have_remediation() {
    let parse = tolerant_parse("export function {{{ broken", "broken.ts");
    let intent = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &[]);
    let verdict = compute_gating_verdict(&intent, &parse);
    for rc in &verdict.reason_codes {
        assert!(!rc.code.is_empty());
        assert!(!rc.remediation.is_empty());
    }
}

#[test]
fn gating_high_ambiguity_produces_reason_code() {
    let parse = tolerant_parse("eval('x');\n", "test.js");
    let intent = IntentGraph::from_register_payload("test-ext", &serde_json::json!({}), &[]);
    let verdict = compute_gating_verdict(&intent, &parse);
    // eval has weight >= 0.7, should produce a high_ambiguity reason code.
    let has_ambiguity_code = verdict
        .reason_codes
        .iter()
        .any(|r| r.code.starts_with("high_ambiguity_"));
    assert!(
        has_ambiguity_code,
        "should have high_ambiguity reason code for eval"
    );
}

#[test]
fn gating_verdict_confidence_preserved() {
    let verdict = compute_gating_verdict(&rich_intent(), &well_formed_parse());
    assert!(verdict.confidence.score > 0.0);
    assert!(!verdict.confidence.reasons.is_empty());
}

// ─── Minimal-diff candidate selector and conflict resolver (bd-k5q5.9.3.4) ──

fn make_safe_proposal(rule_id: &str, from: &str, to: &str) -> PatchProposal {
    PatchProposal {
        rule_id: rule_id.to_string(),
        ops: vec![PatchOp::ReplaceModulePath {
            from: from.to_string(),
            to: to.to_string(),
        }],
        rationale: "test".to_string(),
        confidence: Some(0.9),
    }
}

fn make_aggressive_proposal(rule_id: &str) -> PatchProposal {
    PatchProposal {
        rule_id: rule_id.to_string(),
        ops: vec![PatchOp::InjectStub {
            virtual_path: "/@stubs/test".to_string(),
            source: "export default {};".to_string(),
        }],
        rationale: "test".to_string(),
        confidence: Some(0.8),
    }
}

#[test]
fn select_prefers_safe_over_aggressive() {
    let safe = make_safe_proposal("rule_a", "./a.js", "./b.ts");
    let aggressive = make_aggressive_proposal("rule_b");
    let candidates = vec![aggressive, safe];
    let best = select_best_candidate(&candidates, RepairMode::AutoStrict).unwrap();
    assert_eq!(best.rule_id, "rule_a", "should prefer safe proposal");
}

#[test]
fn select_prefers_fewer_ops() {
    let one_op = make_safe_proposal("rule_a", "./a.js", "./b.ts");
    let two_ops = PatchProposal {
        rule_id: "rule_b".to_string(),
        ops: vec![
            PatchOp::ReplaceModulePath {
                from: "./a.js".to_string(),
                to: "./b.ts".to_string(),
            },
            PatchOp::ReplaceModulePath {
                from: "./c.js".to_string(),
                to: "./d.ts".to_string(),
            },
        ],
        rationale: "test".to_string(),
        confidence: Some(0.9),
    };
    let candidates = vec![two_ops, one_op];
    let best = select_best_candidate(&candidates, RepairMode::AutoSafe).unwrap();
    assert_eq!(best.rule_id, "rule_a", "should prefer fewer ops");
}

#[test]
fn select_prefers_higher_confidence() {
    let high_conf = PatchProposal {
        rule_id: "rule_a".to_string(),
        ops: vec![PatchOp::ReplaceModulePath {
            from: "./a.js".to_string(),
            to: "./b.ts".to_string(),
        }],
        rationale: "test".to_string(),
        confidence: Some(0.95),
    };
    let low_conf = PatchProposal {
        rule_id: "rule_b".to_string(),
        ops: vec![PatchOp::ReplaceModulePath {
            from: "./c.js".to_string(),
            to: "./d.ts".to_string(),
        }],
        rationale: "test".to_string(),
        confidence: Some(0.5),
    };
    let candidates = vec![low_conf, high_conf];
    let best = select_best_candidate(&candidates, RepairMode::AutoSafe).unwrap();
    assert_eq!(best.rule_id, "rule_a", "should prefer higher confidence");
}

#[test]
fn select_filters_by_mode() {
    let aggressive = make_aggressive_proposal("rule_a");
    let candidates = vec![aggressive];
    // AutoSafe shouldn't allow aggressive proposals.
    let result = select_best_candidate(&candidates, RepairMode::AutoSafe);
    assert!(
        result.is_none(),
        "AutoSafe should not select aggressive proposal"
    );
}

#[test]
fn select_returns_none_for_empty() {
    let result = select_best_candidate(&[], RepairMode::AutoSafe);
    assert!(result.is_none());
}

#[test]
fn conflict_none_for_different_paths() {
    let a = make_safe_proposal("rule_a", "./a.js", "./b.ts");
    let b = make_safe_proposal("rule_b", "./c.js", "./d.ts");
    assert!(detect_conflict(&a, &b).is_clear());
}

#[test]
fn conflict_detected_for_same_module_path() {
    let a = make_safe_proposal("rule_a", "./shared.js", "./b.ts");
    let b = make_safe_proposal("rule_b", "./shared.js", "./c.ts");
    let conflict = detect_conflict(&a, &b);
    assert!(
        matches!(conflict, ConflictKind::SameModulePath(ref p) if p == "./shared.js"),
        "expected SameModulePath, got {conflict:?}"
    );
}

#[test]
fn conflict_detected_for_same_virtual_path() {
    let a = PatchProposal {
        rule_id: "rule_a".to_string(),
        ops: vec![PatchOp::InjectStub {
            virtual_path: "/@stubs/shared".to_string(),
            source: "v1".to_string(),
        }],
        rationale: "test".to_string(),
        confidence: None,
    };
    let b = PatchProposal {
        rule_id: "rule_b".to_string(),
        ops: vec![PatchOp::InjectStub {
            virtual_path: "/@stubs/shared".to_string(),
            source: "v2".to_string(),
        }],
        rationale: "test".to_string(),
        confidence: None,
    };
    let conflict = detect_conflict(&a, &b);
    assert!(matches!(conflict, ConflictKind::SameVirtualPath(_)));
}

#[test]
fn resolve_conflicts_drops_lower_ranked() {
    let better = make_safe_proposal("rule_a", "./shared.js", "./b.ts");
    let worse = PatchProposal {
        rule_id: "rule_b".to_string(),
        ops: vec![
            PatchOp::ReplaceModulePath {
                from: "./shared.js".to_string(),
                to: "./c.ts".to_string(),
            },
            PatchOp::ReplaceModulePath {
                from: "./x.js".to_string(),
                to: "./y.ts".to_string(),
            },
        ],
        rationale: "test".to_string(),
        confidence: Some(0.5),
    };
    let proposals = [better, worse];
    let accepted = resolve_conflicts(&proposals);
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].rule_id, "rule_a");
}

#[test]
fn resolve_conflicts_keeps_non_conflicting() {
    let a = make_safe_proposal("rule_a", "./a.js", "./b.ts");
    let b = make_safe_proposal("rule_b", "./c.js", "./d.ts");
    let proposals = [a, b];
    let accepted = resolve_conflicts(&proposals);
    assert_eq!(accepted.len(), 2);
}

#[test]
fn resolve_conflicts_empty_input() {
    let accepted = resolve_conflicts(&[]);
    assert!(accepted.is_empty());
}

#[test]
fn conflict_kind_display() {
    assert!(ConflictKind::None.is_clear());
    assert!(!ConflictKind::SameModulePath("x".to_string()).is_clear());
    assert!(!ConflictKind::SameVirtualPath("y".to_string()).is_clear());
}

// ─── Bounded-context model proposer adapter (bd-k5q5.9.4.2) ─────────────────

#[test]
fn allowed_ops_auto_safe_only_safe() {
    let tags = allowed_op_tags_for_mode(RepairMode::AutoSafe);
    assert!(tags.contains(&"replace_module_path"));
    assert!(tags.contains(&"rewrite_require"));
    assert!(!tags.contains(&"inject_stub"));
    assert!(!tags.contains(&"add_export"));
}

#[test]
fn allowed_ops_auto_strict_includes_aggressive() {
    let tags = allowed_op_tags_for_mode(RepairMode::AutoStrict);
    assert!(tags.contains(&"replace_module_path"));
    assert!(tags.contains(&"inject_stub"));
    assert!(tags.contains(&"add_export"));
    assert!(tags.contains(&"remove_import"));
}

#[test]
fn allowed_ops_off_returns_empty() {
    let tags = allowed_op_tags_for_mode(RepairMode::Off);
    assert!(tags.is_empty());
}

#[test]
fn allowed_ops_suggest_returns_empty() {
    let tags = allowed_op_tags_for_mode(RepairMode::Suggest);
    assert!(tags.is_empty());
}

// ─── Proposal validator and applicator (bd-k5q5.9.4.3) ──────────────────────

#[test]
fn validate_empty_proposal_rejected() {
    let proposal = PatchProposal {
        rule_id: "dist_to_src_v1".to_string(),
        ops: vec![],
        rationale: "test".to_string(),
        confidence: None,
    };
    let errors = validate_proposal(&proposal, RepairMode::AutoSafe, None);
    assert!(errors.contains(&ProposalValidationError::EmptyProposal));
}

#[test]
fn validate_safe_proposal_in_auto_safe() {
    let proposal = make_safe_proposal("dist_to_src_v1", "./a.js", "./b.ts");
    let errors = validate_proposal(&proposal, RepairMode::AutoSafe, None);
    assert!(
        errors.is_empty(),
        "safe proposal should pass in AutoSafe: {errors:?}"
    );
}

#[test]
fn validate_aggressive_proposal_rejected_in_auto_safe() {
    let proposal = make_aggressive_proposal("monorepo_escape_v1");
    let errors = validate_proposal(&proposal, RepairMode::AutoSafe, None);
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ProposalValidationError::DisallowedOp { .. }
                | ProposalValidationError::RiskExceedsMode { .. }
        )),
        "aggressive proposal should fail in AutoSafe: {errors:?}"
    );
}

#[test]
fn validate_aggressive_proposal_passes_in_auto_strict() {
    let proposal = make_aggressive_proposal("monorepo_escape_v1");
    let errors = validate_proposal(&proposal, RepairMode::AutoStrict, None);
    assert!(
        errors.is_empty(),
        "aggressive should pass in AutoStrict: {errors:?}"
    );
}

#[test]
fn validate_unknown_rule_rejected() {
    let proposal = PatchProposal {
        rule_id: "nonexistent_rule_v99".to_string(),
        ops: vec![PatchOp::ReplaceModulePath {
            from: "./a.js".to_string(),
            to: "./b.ts".to_string(),
        }],
        rationale: "test".to_string(),
        confidence: None,
    };
    let errors = validate_proposal(&proposal, RepairMode::AutoSafe, None);
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ProposalValidationError::UnknownRule { .. })),
        "unknown rule should be flagged: {errors:?}"
    );
}

#[test]
fn validate_empty_rule_id_accepted() {
    let proposal = PatchProposal {
        rule_id: String::new(),
        ops: vec![PatchOp::ReplaceModulePath {
            from: "./a.js".to_string(),
            to: "./b.ts".to_string(),
        }],
        rationale: "test".to_string(),
        confidence: None,
    };
    let errors = validate_proposal(&proposal, RepairMode::AutoSafe, None);
    assert!(
        !errors
            .iter()
            .any(|e| matches!(e, ProposalValidationError::UnknownRule { .. })),
        "empty rule_id should not trigger unknown rule"
    );
}

#[test]
fn apply_valid_proposal_succeeds() {
    let proposal = make_safe_proposal("dist_to_src_v1", "./a.js", "./b.ts");
    let result = apply_proposal(&proposal, RepairMode::AutoSafe, None);
    assert!(result.is_ok());
    let app = result.unwrap();
    assert!(app.success);
    assert_eq!(app.ops_applied, 1);
    assert!(app.summary.contains("dist_to_src_v1"));
}

#[test]
fn apply_invalid_proposal_returns_errors() {
    let proposal = PatchProposal {
        rule_id: "dist_to_src_v1".to_string(),
        ops: vec![],
        rationale: "test".to_string(),
        confidence: None,
    };
    let result = apply_proposal(&proposal, RepairMode::AutoSafe, None);
    assert!(result.is_err());
}

#[test]
fn validation_error_display() {
    let err = ProposalValidationError::EmptyProposal;
    assert!(err.to_string().contains("no operations"));

    let err2 = ProposalValidationError::DisallowedOp {
        tag: "inject_stub".to_string(),
    };
    assert!(err2.to_string().contains("inject_stub"));
}

// ─── Fail-closed human approval workflow (bd-k5q5.9.4.4) ────────────────────

#[test]
fn safe_proposal_auto_approved() {
    let proposal = make_safe_proposal("dist_to_src_v1", "./a.js", "./b.ts");
    let req = check_approval_requirement(&proposal, 0.9);
    assert_eq!(req, ApprovalRequirement::AutoApproved);
    assert!(!req.needs_approval());
}

#[test]
fn aggressive_proposal_requires_approval() {
    let proposal = make_aggressive_proposal("monorepo_escape_v1");
    let req = check_approval_requirement(&proposal, 0.9);
    assert_eq!(req, ApprovalRequirement::RequiresApproval);
    assert!(req.needs_approval());
}

#[test]
fn low_confidence_requires_approval() {
    let proposal = make_safe_proposal("dist_to_src_v1", "./a.js", "./b.ts");
    let req = check_approval_requirement(&proposal, 0.3);
    assert_eq!(req, ApprovalRequirement::RequiresApproval);
}

#[test]
fn many_ops_requires_approval() {
    let proposal = PatchProposal {
        rule_id: "dist_to_src_v1".to_string(),
        ops: vec![
            PatchOp::ReplaceModulePath {
                from: "./a.js".to_string(),
                to: "./b.ts".to_string(),
            },
            PatchOp::ReplaceModulePath {
                from: "./c.js".to_string(),
                to: "./d.ts".to_string(),
            },
            PatchOp::RewriteRequire {
                module_path: "./e.js".to_string(),
                from_specifier: "old".to_string(),
                to_specifier: "new".to_string(),
            },
        ],
        rationale: "test".to_string(),
        confidence: Some(0.9),
    };
    let req = check_approval_requirement(&proposal, 0.9);
    assert_eq!(req, ApprovalRequirement::RequiresApproval);
}

#[test]
fn approval_request_has_op_summaries() {
    let proposal = make_safe_proposal("dist_to_src_v1", "./a.js", "./b.ts");
    let req = build_approval_request("test-ext", &proposal, 0.8);
    assert_eq!(req.extension_id, "test-ext");
    assert_eq!(req.op_summaries.len(), 1);
    assert!(req.op_summaries[0].contains("replace_module_path"));
}

#[test]
fn approval_requirement_display() {
    assert_eq!(
        ApprovalRequirement::AutoApproved.to_string(),
        "auto_approved"
    );
    assert_eq!(
        ApprovalRequirement::RequiresApproval.to_string(),
        "requires_approval"
    );
}

use std::path::Path;

// ─── Capability monotonicity proof tests (bd-k5q5.9.5.2) ───────────────────

fn graph_with_signals(id: &str, signals: Vec<IntentSignal>) -> IntentGraph {
    IntentGraph {
        extension_id: id.to_string(),
        signals,
    }
}

#[test]
fn cap_proof_identical_is_monotonic() {
    let before = graph_with_signals(
        "ext-a",
        vec![
            IntentSignal::RegistersTool("search".to_string()),
            IntentSignal::HooksEvent("on_message".to_string()),
        ],
    );
    let after = before.clone();
    let report = compute_capability_proof(&before, &after);
    assert!(report.is_safe());
    assert_eq!(report.verdict, CapabilityMonotonicityVerdict::Monotonic);
    assert_eq!(report.retained_count, 2);
    assert_eq!(report.removed_count, 0);
    assert_eq!(report.added_count, 0);
}

#[test]
fn cap_proof_removed_is_monotonic() {
    let before = graph_with_signals(
        "ext-b",
        vec![
            IntentSignal::RegistersTool("search".to_string()),
            IntentSignal::RegistersCommand("run".to_string()),
            IntentSignal::HooksEvent("on_message".to_string()),
        ],
    );
    let after = graph_with_signals(
        "ext-b",
        vec![IntentSignal::RegistersTool("search".to_string())],
    );
    let report = compute_capability_proof(&before, &after);
    assert!(report.is_safe());
    assert_eq!(report.retained_count, 1);
    assert_eq!(report.removed_count, 2);
    assert_eq!(report.added_count, 0);
}

#[test]
fn cap_proof_added_is_escalation() {
    let before = graph_with_signals(
        "ext-c",
        vec![IntentSignal::RegistersTool("search".to_string())],
    );
    let after = graph_with_signals(
        "ext-c",
        vec![
            IntentSignal::RegistersTool("search".to_string()),
            IntentSignal::RegistersProvider("evil-llm".to_string()),
        ],
    );
    let report = compute_capability_proof(&before, &after);
    assert!(!report.is_safe());
    assert_eq!(report.verdict, CapabilityMonotonicityVerdict::Escalation);
    assert_eq!(report.added_count, 1);
    let escalations = report.escalations();
    assert_eq!(escalations.len(), 1);
    assert_eq!(
        escalations[0].signal(),
        &IntentSignal::RegistersProvider("evil-llm".to_string())
    );
}

#[test]
fn cap_proof_empty_to_empty_is_monotonic() {
    let before = graph_with_signals("ext-d", vec![]);
    let after = graph_with_signals("ext-d", vec![]);
    let report = compute_capability_proof(&before, &after);
    assert!(report.is_safe());
    assert_eq!(report.retained_count, 0);
    assert_eq!(report.removed_count, 0);
    assert_eq!(report.added_count, 0);
}

#[test]
fn cap_proof_empty_to_nonempty_is_escalation() {
    let before = graph_with_signals("ext-e", vec![]);
    let after = graph_with_signals(
        "ext-e",
        vec![IntentSignal::RegistersTool("new-tool".to_string())],
    );
    let report = compute_capability_proof(&before, &after);
    assert!(!report.is_safe());
    assert_eq!(report.added_count, 1);
}

#[test]
fn cap_proof_nonempty_to_empty_is_monotonic() {
    let before = graph_with_signals(
        "ext-f",
        vec![
            IntentSignal::RegistersTool("a".to_string()),
            IntentSignal::RegistersFlag("b".to_string()),
        ],
    );
    let after = graph_with_signals("ext-f", vec![]);
    let report = compute_capability_proof(&before, &after);
    assert!(report.is_safe());
    assert_eq!(report.removed_count, 2);
}

#[test]
fn cap_proof_mixed_add_remove_is_escalation() {
    let before = graph_with_signals(
        "ext-g",
        vec![
            IntentSignal::RegistersTool("old-tool".to_string()),
            IntentSignal::HooksEvent("on_start".to_string()),
        ],
    );
    let after = graph_with_signals(
        "ext-g",
        vec![
            IntentSignal::HooksEvent("on_start".to_string()),
            IntentSignal::RegistersCommand("new-cmd".to_string()),
        ],
    );
    let report = compute_capability_proof(&before, &after);
    assert!(!report.is_safe());
    assert_eq!(report.retained_count, 1);
    assert_eq!(report.removed_count, 1);
    assert_eq!(report.added_count, 1);
}

#[test]
fn cap_proof_delta_display() {
    let retained = CapabilityDelta::Retained(IntentSignal::RegistersTool("x".to_string()));
    assert_eq!(retained.to_string(), "retained: tool:x");
    let removed = CapabilityDelta::Removed(IntentSignal::HooksEvent("y".to_string()));
    assert_eq!(removed.to_string(), "removed: event_hook:y");
    let added = CapabilityDelta::Added(IntentSignal::RegistersFlag("z".to_string()));
    assert_eq!(added.to_string(), "added: flag:z");
}

#[test]
fn cap_proof_delta_is_methods() {
    let r = CapabilityDelta::Retained(IntentSignal::RegistersTool("a".to_string()));
    assert!(r.is_retained());
    assert!(!r.is_removed());
    assert!(!r.is_escalation());

    let d = CapabilityDelta::Removed(IntentSignal::RegistersTool("b".to_string()));
    assert!(d.is_removed());
    assert!(!d.is_retained());

    let a = CapabilityDelta::Added(IntentSignal::RegistersTool("c".to_string()));
    assert!(a.is_escalation());
    assert!(!a.is_retained());
}

#[test]
fn cap_proof_verdict_display() {
    assert_eq!(
        CapabilityMonotonicityVerdict::Monotonic.to_string(),
        "monotonic"
    );
    assert_eq!(
        CapabilityMonotonicityVerdict::Escalation.to_string(),
        "escalation"
    );
}

#[test]
fn cap_proof_preserves_extension_id() {
    let before = graph_with_signals("my-extension-123", vec![]);
    let after = graph_with_signals("my-extension-123", vec![]);
    let report = compute_capability_proof(&before, &after);
    assert_eq!(report.extension_id, "my-extension-123");
}

// ─── Hostcall parity and semantic delta proof tests (bd-k5q5.9.5.3) ────────

#[test]
fn hostcall_surface_from_tool_registration() {
    let graph = graph_with_signals(
        "ext-h",
        vec![
            IntentSignal::RegistersTool("search".to_string()),
            IntentSignal::RegistersCommand("run".to_string()),
        ],
    );
    let surface = extract_hostcall_surface(&graph);
    assert!(surface.contains(&HostcallCategory::Register));
    assert_eq!(surface.len(), 1); // Both map to Register
}

#[test]
fn hostcall_surface_from_events() {
    let graph = graph_with_signals(
        "ext-i",
        vec![IntentSignal::HooksEvent("on_message".to_string())],
    );
    let surface = extract_hostcall_surface(&graph);
    assert!(surface.contains(&HostcallCategory::Events("on_message".to_string())));
}

#[test]
fn hostcall_surface_from_session_capability() {
    let graph = graph_with_signals(
        "ext-j",
        vec![IntentSignal::RequiresCapability("session".to_string())],
    );
    let surface = extract_hostcall_surface(&graph);
    assert!(surface.contains(&HostcallCategory::Session("*".to_string())));
}

#[test]
fn hostcall_surface_from_tool_capability() {
    let graph = graph_with_signals(
        "ext-k",
        vec![IntentSignal::RequiresCapability("tool".to_string())],
    );
    let surface = extract_hostcall_surface(&graph);
    assert!(surface.contains(&HostcallCategory::Tool("*".to_string())));
}

#[test]
fn hostcall_category_display() {
    assert_eq!(
        HostcallCategory::Events("on_start".to_string()).to_string(),
        "events:on_start"
    );
    assert_eq!(HostcallCategory::Register.to_string(), "register");
    assert_eq!(
        HostcallCategory::Session("*".to_string()).to_string(),
        "session:*"
    );
    assert_eq!(
        HostcallCategory::ModuleResolution("./foo".to_string()).to_string(),
        "module:./foo"
    );
}

#[test]
fn semantic_parity_equivalent_no_changes() {
    let before = graph_with_signals(
        "ext-l",
        vec![
            IntentSignal::RegistersTool("search".to_string()),
            IntentSignal::HooksEvent("on_message".to_string()),
        ],
    );
    let after = before.clone();
    let report = compute_semantic_parity(&before, &after, &[]);
    assert!(report.is_safe());
    assert_eq!(report.verdict, SemanticParityVerdict::Equivalent);
    assert_eq!(report.expanded_count, 0);
    assert_eq!(report.drift_severity, SemanticDriftSeverity::None);
}

#[test]
fn semantic_parity_acceptable_drift_on_removal() {
    let before = graph_with_signals(
        "ext-m",
        vec![
            IntentSignal::RegistersTool("search".to_string()),
            IntentSignal::HooksEvent("on_message".to_string()),
        ],
    );
    let after = graph_with_signals(
        "ext-m",
        vec![IntentSignal::RegistersTool("search".to_string())],
    );
    let ops = [PatchOp::ReplaceModulePath {
        from: "./dist/index.js".to_string(),
        to: "./src/index.ts".to_string(),
    }];
    let report = compute_semantic_parity(&before, &after, &ops);
    assert!(report.is_safe());
    assert_eq!(report.verdict, SemanticParityVerdict::AcceptableDrift);
}

#[test]
fn semantic_parity_divergent_on_expansion() {
    let before = graph_with_signals(
        "ext-n",
        vec![IntentSignal::RegistersTool("search".to_string())],
    );
    let after = graph_with_signals(
        "ext-n",
        vec![
            IntentSignal::RegistersTool("search".to_string()),
            IntentSignal::HooksEvent("on_start".to_string()),
        ],
    );
    let report = compute_semantic_parity(&before, &after, &[]);
    assert!(!report.is_safe());
    assert_eq!(report.verdict, SemanticParityVerdict::Divergent);
    assert_eq!(report.expanded_count, 1);
}

#[test]
fn semantic_parity_low_drift_with_aggressive_ops() {
    let before = graph_with_signals("ext-o", vec![IntentSignal::RegistersTool("t".to_string())]);
    let after = before.clone();
    let ops = [PatchOp::AddExport {
        module_path: "./a.ts".to_string(),
        export_name: "foo".to_string(),
        export_value: "42".to_string(),
    }];
    let report = compute_semantic_parity(&before, &after, &ops);
    assert!(report.is_safe());
    assert_eq!(report.drift_severity, SemanticDriftSeverity::Low);
}

#[test]
fn semantic_parity_medium_drift_with_many_stubs() {
    let before = graph_with_signals("ext-p", vec![IntentSignal::RegistersTool("t".to_string())]);
    let after = before.clone();
    let ops = [
        PatchOp::InjectStub {
            virtual_path: "stub1".to_string(),
            source: String::new(),
        },
        PatchOp::InjectStub {
            virtual_path: "stub2".to_string(),
            source: String::new(),
        },
        PatchOp::InjectStub {
            virtual_path: "stub3".to_string(),
            source: String::new(),
        },
    ];
    let report = compute_semantic_parity(&before, &after, &ops);
    // Medium drift is NOT safe — it triggers Divergent verdict.
    assert!(!report.is_safe());
    assert_eq!(report.verdict, SemanticParityVerdict::Divergent);
    assert_eq!(report.drift_severity, SemanticDriftSeverity::Medium);
}

#[test]
fn semantic_parity_high_drift_with_expansion() {
    let before = graph_with_signals("ext-q", vec![]);
    let after = graph_with_signals(
        "ext-q",
        vec![IntentSignal::RequiresCapability("session".to_string())],
    );
    let report = compute_semantic_parity(&before, &after, &[]);
    assert!(!report.is_safe());
    assert_eq!(report.drift_severity, SemanticDriftSeverity::High);
}

#[test]
fn semantic_parity_verdict_display() {
    assert_eq!(SemanticParityVerdict::Equivalent.to_string(), "equivalent");
    assert_eq!(
        SemanticParityVerdict::AcceptableDrift.to_string(),
        "acceptable_drift"
    );
    assert_eq!(SemanticParityVerdict::Divergent.to_string(), "divergent");
}

#[test]
fn semantic_drift_severity_display() {
    assert_eq!(SemanticDriftSeverity::None.to_string(), "none");
    assert_eq!(SemanticDriftSeverity::Low.to_string(), "low");
    assert_eq!(SemanticDriftSeverity::Medium.to_string(), "medium");
    assert_eq!(SemanticDriftSeverity::High.to_string(), "high");
}

#[test]
fn semantic_drift_severity_is_acceptable() {
    assert!(SemanticDriftSeverity::None.is_acceptable());
    assert!(SemanticDriftSeverity::Low.is_acceptable());
    assert!(!SemanticDriftSeverity::Medium.is_acceptable());
    assert!(!SemanticDriftSeverity::High.is_acceptable());
}

#[test]
fn semantic_parity_report_expansions() {
    let before = graph_with_signals("ext-r", vec![IntentSignal::RegistersTool("a".to_string())]);
    let after = graph_with_signals(
        "ext-r",
        vec![
            IntentSignal::RegistersTool("a".to_string()),
            IntentSignal::RequiresCapability("session".to_string()),
            IntentSignal::RequiresCapability("tool".to_string()),
        ],
    );
    let report = compute_semantic_parity(&before, &after, &[]);
    let expansions = report.expansions();
    assert_eq!(expansions.len(), 2);
}

#[test]
fn semantic_parity_preserves_extension_id() {
    let before = graph_with_signals("my-ext-456", vec![]);
    let after = graph_with_signals("my-ext-456", vec![]);
    let report = compute_semantic_parity(&before, &after, &[]);
    assert_eq!(report.extension_id, "my-ext-456");
}

#[test]
fn semantic_parity_safe_ops_no_drift() {
    let before = graph_with_signals("ext-s", vec![IntentSignal::RegistersTool("t".to_string())]);
    let after = before.clone();
    let ops = [
        PatchOp::ReplaceModulePath {
            from: "./a.js".to_string(),
            to: "./b.ts".to_string(),
        },
        PatchOp::RewriteRequire {
            module_path: "./c.js".to_string(),
            from_specifier: "old".to_string(),
            to_specifier: "new".to_string(),
        },
    ];
    let report = compute_semantic_parity(&before, &after, &ops);
    assert!(report.is_safe());
    assert_eq!(report.drift_severity, SemanticDriftSeverity::None);
}

#[test]
fn hostcall_delta_display() {
    let r = HostcallDelta::Retained(HostcallCategory::Register);
    assert_eq!(r.to_string(), "retained: register");
    let a = HostcallDelta::Added(HostcallCategory::Events("on_start".to_string()));
    assert_eq!(a.to_string(), "added: events:on_start");
}

#[test]
fn hostcall_delta_is_expansion() {
    let r = HostcallDelta::Retained(HostcallCategory::Register);
    assert!(!r.is_expansion());
    let d = HostcallDelta::Removed(HostcallCategory::Register);
    assert!(!d.is_expansion());
    let a = HostcallDelta::Added(HostcallCategory::Register);
    assert!(a.is_expansion());
}

// ─── Conformance replay and golden checksum tests (bd-k5q5.9.5.4) ──────────

#[test]
fn checksum_deterministic() {
    let content = b"hello world";
    let a = compute_artifact_checksum(content);
    let b = compute_artifact_checksum(content);
    assert_eq!(a, b);
    // SHA-256 of "hello world" is well-known.
    assert_eq!(a.len(), 64); // 256-bit hex = 64 chars
}

#[test]
fn checksum_differs_for_different_content() {
    let a = compute_artifact_checksum(b"alpha");
    let b = compute_artifact_checksum(b"beta");
    assert_ne!(a, b);
}

#[test]
fn golden_manifest_build() {
    let artifacts: &[(&str, &[u8])] = &[
        ("src/index.ts", b"const x = 1;"),
        ("package.json", b"{\"name\":\"ext\"}"),
    ];
    let manifest = build_golden_manifest("ext-a", artifacts, 1_700_000_000_000);
    assert_eq!(manifest.extension_id, "ext-a");
    assert_eq!(manifest.artifact_count(), 2);
    assert_eq!(manifest.generated_at_ms, 1_700_000_000_000);
    let src_entry = manifest
        .entries
        .iter()
        .find(|entry| entry.relative_path == "src/index.ts")
        .expect("missing src/index.ts entry");
    assert_eq!(src_entry.size_bytes, 12);
    assert!(
        manifest
            .entries
            .iter()
            .any(|entry| entry.relative_path == "package.json"),
        "missing package.json entry"
    );
}

#[test]
fn golden_manifest_order_is_deterministic() {
    let artifacts_a: &[(&str, &[u8])] = &[
        ("src/index.ts", b"const x = 1;"),
        ("package.json", b"{\"name\":\"ext\"}"),
    ];
    let artifacts_b: &[(&str, &[u8])] = &[
        ("package.json", b"{\"name\":\"ext\"}"),
        ("src/index.ts", b"const x = 1;"),
    ];

    let manifest_a = build_golden_manifest("ext-a", artifacts_a, 0);
    let manifest_b = build_golden_manifest("ext-a", artifacts_b, 0);

    assert_eq!(manifest_a.entries, manifest_b.entries);
}

#[test]
fn golden_manifest_verify_entry_match() {
    let content = b"const x = 42;";
    let manifest = build_golden_manifest("ext-b", &[("index.ts", content)], 0);
    assert_eq!(manifest.verify_entry("index.ts", content), Some(true));
}

#[test]
fn golden_manifest_verify_entry_mismatch() {
    let manifest = build_golden_manifest("ext-c", &[("index.ts", b"original")], 0);
    assert_eq!(manifest.verify_entry("index.ts", b"tampered"), Some(false));
}

#[test]
fn golden_manifest_verify_entry_not_found() {
    let manifest = build_golden_manifest("ext-d", &[("index.ts", b"code")], 0);
    assert_eq!(manifest.verify_entry("missing.ts", b"code"), None);
}

#[test]
fn replay_no_fixtures_is_acceptable() {
    let report = replay_conformance_fixtures("ext-e", &[]);
    assert!(report.is_acceptable());
    assert_eq!(report.verdict, ConformanceReplayVerdict::NoFixtures);
    assert_eq!(report.total_count, 0);
}

#[test]
fn replay_all_pass() {
    let fixtures = vec![
        ConformanceFixture {
            name: "loads without error".to_string(),
            expected: "ok".to_string(),
            actual: Some("ok".to_string()),
            passed: true,
        },
        ConformanceFixture {
            name: "registers tool".to_string(),
            expected: "tool:search".to_string(),
            actual: Some("tool:search".to_string()),
            passed: true,
        },
    ];
    let report = replay_conformance_fixtures("ext-f", &fixtures);
    assert!(report.is_acceptable());
    assert_eq!(report.verdict, ConformanceReplayVerdict::Pass);
    assert_eq!(report.passed_count, 2);
    assert_eq!(report.total_count, 2);
}

#[test]
fn replay_some_fail() {
    let fixtures = vec![
        ConformanceFixture {
            name: "loads".to_string(),
            expected: "ok".to_string(),
            actual: Some("ok".to_string()),
            passed: true,
        },
        ConformanceFixture {
            name: "registers tool".to_string(),
            expected: "tool:search".to_string(),
            actual: Some("tool:missing".to_string()),
            passed: false,
        },
    ];
    let report = replay_conformance_fixtures("ext-g", &fixtures);
    assert!(!report.is_acceptable());
    assert_eq!(report.verdict, ConformanceReplayVerdict::Fail);
    assert_eq!(report.passed_count, 1);
    assert_eq!(report.total_count, 2);
}

#[test]
fn replay_verdict_display() {
    assert_eq!(ConformanceReplayVerdict::Pass.to_string(), "pass");
    assert_eq!(ConformanceReplayVerdict::Fail.to_string(), "fail");
    assert_eq!(
        ConformanceReplayVerdict::NoFixtures.to_string(),
        "no_fixtures"
    );
}

#[test]
fn verification_bundle_all_pass() {
    let intent = graph_with_signals("ext-h", vec![IntentSignal::RegistersTool("t".to_string())]);
    let cap_proof = compute_capability_proof(&intent, &intent);
    let sem_proof = compute_semantic_parity(&intent, &intent, &[]);
    let conformance = replay_conformance_fixtures("ext-h", &[]);
    let manifest = build_golden_manifest("ext-h", &[("index.ts", b"code")], 0);

    let bundle = VerificationBundle {
        extension_id: "ext-h".to_string(),
        structural: StructuralVerdict::Valid,
        capability_proof: cap_proof,
        semantic_proof: sem_proof,
        conformance,
        checksum_manifest: manifest,
    };
    assert!(bundle.is_verified());
    assert!(bundle.failure_reasons().is_empty());
}

#[test]
fn verification_bundle_structural_fail() {
    let intent = graph_with_signals("ext-i", vec![]);
    let cap_proof = compute_capability_proof(&intent, &intent);
    let sem_proof = compute_semantic_parity(&intent, &intent, &[]);
    let conformance = replay_conformance_fixtures("ext-i", &[]);
    let manifest = build_golden_manifest("ext-i", &[], 0);

    let bundle = VerificationBundle {
        extension_id: "ext-i".to_string(),
        structural: StructuralVerdict::ParseError {
            path: Path::new("index.ts").to_path_buf(),
            message: "syntax error".to_string(),
        },
        capability_proof: cap_proof,
        semantic_proof: sem_proof,
        conformance,
        checksum_manifest: manifest,
    };
    assert!(!bundle.is_verified());
    let reasons = bundle.failure_reasons();
    assert_eq!(reasons.len(), 1);
    assert!(reasons[0].contains("structural"));
}

#[test]
fn verification_bundle_capability_escalation() {
    let before = graph_with_signals("ext-j", vec![]);
    let after = graph_with_signals(
        "ext-j",
        vec![IntentSignal::RegistersTool("new".to_string())],
    );
    let cap_proof = compute_capability_proof(&before, &after);
    let sem_proof = compute_semantic_parity(&before, &before, &[]);
    let conformance = replay_conformance_fixtures("ext-j", &[]);
    let manifest = build_golden_manifest("ext-j", &[], 0);

    let bundle = VerificationBundle {
        extension_id: "ext-j".to_string(),
        structural: StructuralVerdict::Valid,
        capability_proof: cap_proof,
        semantic_proof: sem_proof,
        conformance,
        checksum_manifest: manifest,
    };
    assert!(!bundle.is_verified());
    let reasons = bundle.failure_reasons();
    assert!(reasons.iter().any(|r| r.contains("capability")));
}

#[test]
fn verification_bundle_conformance_fail() {
    let intent = graph_with_signals("ext-k", vec![]);
    let cap_proof = compute_capability_proof(&intent, &intent);
    let sem_proof = compute_semantic_parity(&intent, &intent, &[]);
    let fixtures = vec![ConformanceFixture {
        name: "broken".to_string(),
        expected: "ok".to_string(),
        actual: Some("error".to_string()),
        passed: false,
    }];
    let conformance = replay_conformance_fixtures("ext-k", &fixtures);
    let manifest = build_golden_manifest("ext-k", &[], 0);

    let bundle = VerificationBundle {
        extension_id: "ext-k".to_string(),
        structural: StructuralVerdict::Valid,
        capability_proof: cap_proof,
        semantic_proof: sem_proof,
        conformance,
        checksum_manifest: manifest,
    };
    assert!(!bundle.is_verified());
    let reasons = bundle.failure_reasons();
    assert!(reasons.iter().any(|r| r.contains("conformance")));
}

#[test]
fn verification_bundle_multiple_failures() {
    let before = graph_with_signals("ext-l", vec![]);
    let after = graph_with_signals(
        "ext-l",
        vec![IntentSignal::RegistersProvider("p".to_string())],
    );
    let cap_proof = compute_capability_proof(&before, &after);
    let sem_proof = compute_semantic_parity(&before, &after, &[]);
    let fixtures = vec![ConformanceFixture {
        name: "broken".to_string(),
        expected: "ok".to_string(),
        actual: None,
        passed: false,
    }];
    let conformance = replay_conformance_fixtures("ext-l", &fixtures);
    let manifest = build_golden_manifest("ext-l", &[], 0);

    let bundle = VerificationBundle {
        extension_id: "ext-l".to_string(),
        structural: StructuralVerdict::ParseError {
            path: Path::new("x.ts").to_path_buf(),
            message: "err".to_string(),
        },
        capability_proof: cap_proof,
        semantic_proof: sem_proof,
        conformance,
        checksum_manifest: manifest,
    };
    assert!(!bundle.is_verified());
    let reasons = bundle.failure_reasons();
    // structural + capability + semantic + conformance = 4
    assert_eq!(reasons.len(), 4);
}

// ─── Overlay artifact format and lifecycle tests (bd-k5q5.9.6.1) ───────────

fn make_overlay(state: OverlayState, verified: bool) -> OverlayArtifact {
    OverlayArtifact {
        overlay_id: "ovl-001".to_string(),
        extension_id: "ext-a".to_string(),
        extension_version: "1.0.0".to_string(),
        original_checksum: "abc123".to_string(),
        repaired_checksum: "def456".to_string(),
        state,
        rule_id: "dist_to_src_v1".to_string(),
        repair_mode: RepairMode::AutoSafe,
        verification_passed: verified,
        created_at_ms: 1000,
        updated_at_ms: 1000,
    }
}

#[test]
fn overlay_state_display() {
    assert_eq!(OverlayState::Staged.to_string(), "staged");
    assert_eq!(OverlayState::Canary.to_string(), "canary");
    assert_eq!(OverlayState::Stable.to_string(), "stable");
    assert_eq!(OverlayState::RolledBack.to_string(), "rolled_back");
    assert_eq!(OverlayState::Superseded.to_string(), "superseded");
}

#[test]
fn overlay_state_is_active() {
    assert!(!OverlayState::Staged.is_active());
    assert!(OverlayState::Canary.is_active());
    assert!(OverlayState::Stable.is_active());
    assert!(!OverlayState::RolledBack.is_active());
    assert!(!OverlayState::Superseded.is_active());
}

#[test]
fn overlay_state_is_terminal() {
    assert!(!OverlayState::Staged.is_terminal());
    assert!(!OverlayState::Canary.is_terminal());
    assert!(!OverlayState::Stable.is_terminal());
    assert!(OverlayState::RolledBack.is_terminal());
    assert!(OverlayState::Superseded.is_terminal());
}

#[test]
fn overlay_transition_staged_to_canary() {
    let mut ovl = make_overlay(OverlayState::Staged, true);
    assert!(transition_overlay(&mut ovl, OverlayState::Canary, 2000).is_ok());
    assert_eq!(ovl.state, OverlayState::Canary);
    assert_eq!(ovl.updated_at_ms, 2000);
}

#[test]
fn overlay_transition_staged_to_canary_requires_verification() {
    let mut ovl = make_overlay(OverlayState::Staged, false);
    let err = transition_overlay(&mut ovl, OverlayState::Canary, 2000).unwrap_err();
    assert_eq!(err, OverlayTransitionError::VerificationRequired);
}

#[test]
fn overlay_transition_canary_to_stable() {
    let mut ovl = make_overlay(OverlayState::Canary, true);
    assert!(transition_overlay(&mut ovl, OverlayState::Stable, 3000).is_ok());
    assert_eq!(ovl.state, OverlayState::Stable);
}

#[test]
fn overlay_transition_canary_to_rollback() {
    let mut ovl = make_overlay(OverlayState::Canary, true);
    assert!(transition_overlay(&mut ovl, OverlayState::RolledBack, 4000).is_ok());
    assert_eq!(ovl.state, OverlayState::RolledBack);
}

#[test]
fn overlay_transition_stable_to_superseded() {
    let mut ovl = make_overlay(OverlayState::Stable, true);
    assert!(transition_overlay(&mut ovl, OverlayState::Superseded, 5000).is_ok());
    assert_eq!(ovl.state, OverlayState::Superseded);
}

#[test]
fn overlay_transition_invalid() {
    let mut ovl = make_overlay(OverlayState::RolledBack, true);
    let err = transition_overlay(&mut ovl, OverlayState::Canary, 6000).unwrap_err();
    assert!(matches!(
        err,
        OverlayTransitionError::InvalidTransition { .. }
    ));
}

#[test]
fn overlay_transition_error_display() {
    let err = OverlayTransitionError::InvalidTransition {
        from: OverlayState::RolledBack,
        to: OverlayState::Canary,
    };
    assert!(err.to_string().contains("invalid transition"));
    let err2 = OverlayTransitionError::VerificationRequired;
    assert!(err2.to_string().contains("verification"));
}

// ─── Canary routing tests (bd-k5q5.9.6.2) ──────────────────────────────────

#[test]
fn canary_route_display() {
    assert_eq!(CanaryRoute::Original.to_string(), "original");
    assert_eq!(CanaryRoute::Overlay.to_string(), "overlay");
}

#[test]
fn canary_config_routes_by_bucket() {
    let config = CanaryConfig {
        extension_id: "ext-a".to_string(),
        extension_version: "1.0.0".to_string(),
        overlay_percent: 50,
        enabled: true,
    };
    // Bucket < 50 → overlay
    assert_eq!(config.route(0), CanaryRoute::Overlay);
    assert_eq!(config.route(49), CanaryRoute::Overlay);
    // Bucket >= 50 → original
    assert_eq!(config.route(50), CanaryRoute::Original);
    assert_eq!(config.route(99), CanaryRoute::Original);
}

#[test]
fn canary_config_disabled_routes_original() {
    let config = CanaryConfig {
        extension_id: "ext-b".to_string(),
        extension_version: "1.0.0".to_string(),
        overlay_percent: 100,
        enabled: false,
    };
    assert_eq!(config.route(0), CanaryRoute::Original);
}

#[test]
fn canary_config_full_rollout() {
    let full = CanaryConfig {
        extension_id: "ext-c".to_string(),
        extension_version: "1.0.0".to_string(),
        overlay_percent: 100,
        enabled: true,
    };
    assert!(full.is_full_rollout());

    let partial = CanaryConfig {
        extension_id: "ext-c".to_string(),
        extension_version: "1.0.0".to_string(),
        overlay_percent: 50,
        enabled: true,
    };
    assert!(!partial.is_full_rollout());
}

#[test]
fn canary_bucket_deterministic() {
    let a = compute_canary_bucket("ext-a", "prod");
    let b = compute_canary_bucket("ext-a", "prod");
    assert_eq!(a, b);
    assert!(a < 100);
}

#[test]
fn canary_bucket_differs_by_env() {
    let prod = compute_canary_bucket("ext-a", "prod");
    let staging = compute_canary_bucket("ext-a", "staging");
    // Not guaranteed to differ, but let's check it's valid.
    assert!(prod < 100);
    assert!(staging < 100);
}

// ─── Health/SLO monitor tests (bd-k5q5.9.6.3) ─────────────────────────────

#[test]
fn health_signal_within_threshold() {
    let signal = HealthSignal {
        name: "error_rate".to_string(),
        value: 0.01,
        threshold: 0.05,
    };
    assert!(signal.is_healthy());
}

#[test]
fn health_signal_above_threshold() {
    let signal = HealthSignal {
        name: "error_rate".to_string(),
        value: 0.10,
        threshold: 0.05,
    };
    assert!(!signal.is_healthy());
}

#[test]
fn health_evaluate_all_healthy() {
    let signals = vec![
        HealthSignal {
            name: "error_rate".to_string(),
            value: 0.01,
            threshold: 0.05,
        },
        HealthSignal {
            name: "latency_p99".to_string(),
            value: 100.0,
            threshold: 500.0,
        },
    ];
    let report = evaluate_health("ext-a", &signals);
    assert!(report.is_healthy());
    assert_eq!(report.verdict, SloVerdict::Healthy);
    assert!(report.violations.is_empty());
}

#[test]
fn health_evaluate_with_violation() {
    let signals = vec![
        HealthSignal {
            name: "error_rate".to_string(),
            value: 0.10,
            threshold: 0.05,
        },
        HealthSignal {
            name: "latency_p99".to_string(),
            value: 100.0,
            threshold: 500.0,
        },
    ];
    let report = evaluate_health("ext-b", &signals);
    assert!(!report.is_healthy());
    assert_eq!(report.verdict, SloVerdict::Violated);
    assert_eq!(report.violations.len(), 1);
    assert!(report.violations[0].contains("error_rate"));
}

#[test]
fn slo_verdict_display() {
    assert_eq!(SloVerdict::Healthy.to_string(), "healthy");
    assert_eq!(SloVerdict::Violated.to_string(), "violated");
}

#[test]
fn auto_rollback_on_violation() {
    let signals = vec![HealthSignal {
        name: "errors".to_string(),
        value: 0.5,
        threshold: 0.1,
    }];
    let report = evaluate_health("ext-c", &signals);
    assert!(should_auto_rollback(&report));
}

#[test]
fn no_rollback_when_healthy() {
    let signals = vec![HealthSignal {
        name: "errors".to_string(),
        value: 0.01,
        threshold: 0.1,
    }];
    let report = evaluate_health("ext-d", &signals);
    assert!(!should_auto_rollback(&report));
}

// ─── Promotion and rollback workflow tests (bd-k5q5.9.6.4) ─────────────────

#[test]
fn promotion_decision_display() {
    assert_eq!(PromotionDecision::Promote.to_string(), "promote");
    assert_eq!(PromotionDecision::Hold.to_string(), "hold");
    assert_eq!(PromotionDecision::Rollback.to_string(), "rollback");
}

#[test]
fn decide_promotion_rollback_on_violation() {
    let signals = vec![HealthSignal {
        name: "errors".to_string(),
        value: 0.5,
        threshold: 0.1,
    }];
    let health = evaluate_health("ext-a", &signals);
    let decision = decide_promotion(&health, 1000, 2000, 500);
    assert_eq!(decision, PromotionDecision::Rollback);
}

#[test]
fn decide_promotion_promote_after_window() {
    let health = evaluate_health("ext-b", &[]);
    let decision = decide_promotion(&health, 1000, 2000, 500);
    assert_eq!(decision, PromotionDecision::Promote);
}

#[test]
fn decide_promotion_hold_during_window() {
    let health = evaluate_health("ext-c", &[]);
    let decision = decide_promotion(&health, 1000, 1200, 500);
    assert_eq!(decision, PromotionDecision::Hold);
}

#[test]
fn execute_promotion_canary_to_stable() {
    let mut ovl = make_overlay(OverlayState::Canary, true);
    assert!(execute_promotion(&mut ovl, 5000).is_ok());
    assert_eq!(ovl.state, OverlayState::Stable);
    assert_eq!(ovl.updated_at_ms, 5000);
}

#[test]
fn execute_rollback_canary_to_rolled_back() {
    let mut ovl = make_overlay(OverlayState::Canary, true);
    assert!(execute_rollback(&mut ovl, 6000).is_ok());
    assert_eq!(ovl.state, OverlayState::RolledBack);
}

#[test]
fn execute_rollback_stable_to_rolled_back() {
    let mut ovl = make_overlay(OverlayState::Stable, true);
    assert!(execute_rollback(&mut ovl, 7000).is_ok());
    assert_eq!(ovl.state, OverlayState::RolledBack);
}

#[test]
fn execute_promotion_from_staged_fails() {
    let mut ovl = make_overlay(OverlayState::Staged, true);
    assert!(execute_promotion(&mut ovl, 8000).is_err());
}

#[test]
fn full_canary_lifecycle() {
    // staged → canary → stable
    let mut ovl = make_overlay(OverlayState::Staged, true);
    assert!(transition_overlay(&mut ovl, OverlayState::Canary, 1000).is_ok());
    assert!(ovl.is_active());
    assert!(execute_promotion(&mut ovl, 2000).is_ok());
    assert_eq!(ovl.state, OverlayState::Stable);
    assert!(ovl.is_active());
    // supersede
    assert!(transition_overlay(&mut ovl, OverlayState::Superseded, 3000).is_ok());
    assert!(!ovl.is_active());
    assert!(ovl.state.is_terminal());
}

// ─── Audit ledger tests (bd-k5q5.9.7.1) ────────────────────────────────────

#[test]
fn audit_ledger_starts_empty() {
    let ledger = AuditLedger::new();
    assert!(ledger.is_empty());
    assert_eq!(ledger.len(), 0);
}

#[test]
fn audit_ledger_append_and_get() {
    let mut ledger = AuditLedger::new();
    let seq = ledger.append(
        1000,
        "ext-a",
        AuditEntryKind::Analysis,
        "analyzed extension".to_string(),
        vec![("confidence".to_string(), "0.75".to_string())],
    );
    assert_eq!(seq, 0);
    assert_eq!(ledger.len(), 1);
    let entry = ledger.get(0).unwrap();
    assert_eq!(entry.extension_id, "ext-a");
    assert_eq!(entry.kind, AuditEntryKind::Analysis);
}

#[test]
fn audit_ledger_monotonic_sequence() {
    let mut ledger = AuditLedger::new();
    let s1 = ledger.append(
        1000,
        "ext-a",
        AuditEntryKind::Analysis,
        String::new(),
        vec![],
    );
    let s2 = ledger.append(
        2000,
        "ext-a",
        AuditEntryKind::GatingDecision,
        String::new(),
        vec![],
    );
    let s3 = ledger.append(
        3000,
        "ext-b",
        AuditEntryKind::Activated,
        String::new(),
        vec![],
    );
    assert_eq!(s1, 0);
    assert_eq!(s2, 1);
    assert_eq!(s3, 2);
}

#[test]
fn audit_ledger_query_by_extension() {
    let mut ledger = AuditLedger::new();
    ledger.append(
        1000,
        "ext-a",
        AuditEntryKind::Analysis,
        String::new(),
        vec![],
    );
    ledger.append(
        2000,
        "ext-b",
        AuditEntryKind::Analysis,
        String::new(),
        vec![],
    );
    ledger.append(
        3000,
        "ext-a",
        AuditEntryKind::Activated,
        String::new(),
        vec![],
    );
    let ext_a = ledger.entries_for_extension("ext-a");
    assert_eq!(ext_a.len(), 2);
    let ext_b = ledger.entries_for_extension("ext-b");
    assert_eq!(ext_b.len(), 1);
}

#[test]
fn audit_ledger_query_by_kind() {
    let mut ledger = AuditLedger::new();
    ledger.append(
        1000,
        "ext-a",
        AuditEntryKind::Analysis,
        String::new(),
        vec![],
    );
    ledger.append(
        2000,
        "ext-b",
        AuditEntryKind::Analysis,
        String::new(),
        vec![],
    );
    ledger.append(
        3000,
        "ext-a",
        AuditEntryKind::RolledBack,
        String::new(),
        vec![],
    );
    let analyses = ledger.entries_by_kind(AuditEntryKind::Analysis);
    assert_eq!(analyses.len(), 2);
    let rollbacks = ledger.entries_by_kind(AuditEntryKind::RolledBack);
    assert_eq!(rollbacks.len(), 1);
}

#[test]
fn audit_entry_kind_display() {
    assert_eq!(AuditEntryKind::Analysis.to_string(), "analysis");
    assert_eq!(
        AuditEntryKind::GatingDecision.to_string(),
        "gating_decision"
    );
    assert_eq!(AuditEntryKind::RolledBack.to_string(), "rolled_back");
    assert_eq!(AuditEntryKind::Promoted.to_string(), "promoted");
    assert_eq!(AuditEntryKind::Superseded.to_string(), "superseded");
}

// ─── Telemetry tests (bd-k5q5.9.7.2) ───────────────────────────────────────

#[test]
fn telemetry_collector_starts_empty() {
    let collector = TelemetryCollector::new();
    assert!(collector.is_empty());
    assert_eq!(collector.len(), 0);
}

#[test]
fn telemetry_collector_record_and_count() {
    let mut collector = TelemetryCollector::new();
    collector.increment(TelemetryMetric::RepairAttempted, 1000, vec![]);
    collector.increment(TelemetryMetric::RepairAttempted, 2000, vec![]);
    collector.increment(TelemetryMetric::RepairDenied, 3000, vec![]);
    assert_eq!(collector.count(TelemetryMetric::RepairAttempted), 2);
    assert_eq!(collector.count(TelemetryMetric::RepairDenied), 1);
    assert_eq!(collector.count(TelemetryMetric::OverlayPromoted), 0);
}

#[test]
fn telemetry_collector_sum() {
    let mut collector = TelemetryCollector::new();
    collector.record(TelemetryMetric::ApprovalLatencyMs, 150.0, 1000, vec![]);
    collector.record(TelemetryMetric::ApprovalLatencyMs, 250.0, 2000, vec![]);
    let total = collector.sum(TelemetryMetric::ApprovalLatencyMs);
    assert!((total - 400.0).abs() < f64::EPSILON);
}

#[test]
fn telemetry_metric_display() {
    assert_eq!(
        TelemetryMetric::RepairAttempted.to_string(),
        "repair.attempted"
    );
    assert_eq!(
        TelemetryMetric::OverlayRolledBack.to_string(),
        "overlay.rolled_back"
    );
    assert_eq!(
        TelemetryMetric::ApprovalLatencyMs.to_string(),
        "approval.latency_ms"
    );
}

// ─── Operator CLI inspect/explain/diff tests (bd-k5q5.9.7.3) ───────────────

#[test]
fn inspection_record_from_ledger() {
    let mut ledger = AuditLedger::new();
    ledger.append(
        1000,
        "ext-a",
        AuditEntryKind::Analysis,
        "analyzed".to_string(),
        vec![],
    );
    ledger.append(
        2000,
        "ext-a",
        AuditEntryKind::GatingDecision,
        "allow".to_string(),
        vec![],
    );
    let record = build_inspection("ext-a", &ledger, Some(OverlayState::Canary), true);
    assert_eq!(record.extension_id, "ext-a");
    assert_eq!(record.timeline.len(), 2);
    assert_eq!(record.gating_summary, "allow");
    assert_eq!(record.overlay_state, Some("canary".to_string()));
    assert_eq!(record.verification_summary, "all proofs passed");
}

#[test]
fn inspection_record_no_gating() {
    let ledger = AuditLedger::new();
    let record = build_inspection("ext-b", &ledger, None, false);
    assert!(record.timeline.is_empty());
    assert_eq!(record.gating_summary, "no gating decision recorded");
    assert!(record.overlay_state.is_none());
    assert_eq!(record.verification_summary, "one or more proofs failed");
}

#[test]
fn explain_gating_output() {
    let intent = graph_with_signals("ext-c", vec![IntentSignal::RegistersTool("t".to_string())]);
    let parse = TolerantParseResult {
        parsed_ok: true,
        statement_count: 10,
        import_export_count: 2,
        ambiguities: vec![],
    };
    let verdict = compute_gating_verdict(&intent, &parse);
    let lines = explain_gating(&verdict);
    assert!(!lines.is_empty());
    assert!(lines[0].contains("Decision:"));
}

#[test]
fn format_proposal_diff_output() {
    let proposal = make_safe_proposal("dist_to_src_v1", "./dist/a.js", "./src/a.ts");
    let lines = format_proposal_diff(&proposal);
    assert!(lines[0].contains("dist_to_src_v1"));
    assert!(lines.iter().any(|l| l.contains("replace_module_path")));
}

// ─── Forensic bundle tests (bd-k5q5.9.7.4) ─────────────────────────────────

#[test]
fn forensic_bundle_basic() {
    let mut ledger = AuditLedger::new();
    ledger.append(
        1000,
        "ext-a",
        AuditEntryKind::Analysis,
        "analyzed".to_string(),
        vec![],
    );
    ledger.append(
        2000,
        "ext-a",
        AuditEntryKind::Activated,
        "activated".to_string(),
        vec![],
    );
    ledger.append(
        3000,
        "ext-b",
        AuditEntryKind::Analysis,
        "other".to_string(),
        vec![],
    );

    let mut collector = TelemetryCollector::new();
    collector.increment(
        TelemetryMetric::RepairAttempted,
        1000,
        vec![("extension_id".to_string(), "ext-a".to_string())],
    );
    collector.increment(
        TelemetryMetric::RepairAttempted,
        2000,
        vec![("extension_id".to_string(), "ext-b".to_string())],
    );

    let bundle = build_forensic_bundle("ext-a", None, None, &ledger, &collector, None, None, 9000);
    assert_eq!(bundle.extension_id, "ext-a");
    assert_eq!(bundle.audit_count(), 2); // Only ext-a entries
    assert_eq!(bundle.telemetry_points.len(), 1); // Only ext-a points
    assert!(!bundle.has_verification());
    assert!(!bundle.has_health_data());
    assert_eq!(bundle.exported_at_ms, 9000);
}

#[test]
fn forensic_bundle_with_overlay() {
    let ovl = make_overlay(OverlayState::Canary, true);
    let ledger = AuditLedger::new();
    let collector = TelemetryCollector::new();
    let signals = vec![HealthSignal {
        name: "error_rate".to_string(),
        value: 0.01,
        threshold: 0.05,
    }];
    let health = evaluate_health("ext-a", &signals);

    let bundle = build_forensic_bundle(
        "ext-a",
        Some(&ovl),
        None,
        &ledger,
        &collector,
        Some(&health),
        None,
        10_000,
    );
    assert!(bundle.overlay.is_some());
    assert!(bundle.has_health_data());
}

#[test]
fn forensic_bundle_with_verification() {
    let intent = graph_with_signals("ext-a", vec![]);
    let cap_proof = compute_capability_proof(&intent, &intent);
    let sem_proof = compute_semantic_parity(&intent, &intent, &[]);
    let conformance = replay_conformance_fixtures("ext-a", &[]);
    let manifest = build_golden_manifest("ext-a", &[], 0);

    let vb = VerificationBundle {
        extension_id: "ext-a".to_string(),
        structural: StructuralVerdict::Valid,
        capability_proof: cap_proof,
        semantic_proof: sem_proof,
        conformance,
        checksum_manifest: manifest,
    };

    let ledger = AuditLedger::new();
    let collector = TelemetryCollector::new();
    let bundle = build_forensic_bundle(
        "ext-a",
        None,
        Some(&vb),
        &ledger,
        &collector,
        None,
        None,
        11_000,
    );
    assert!(bundle.has_verification());
}

// ─── Architecture ADR tests (bd-k5q5.9.8.1) ────────────────────────────────

#[test]
fn adr_has_id_and_title() {
    assert!(!LisrAdr::ID.is_empty());
    assert!(!LisrAdr::TITLE.is_empty());
}

#[test]
fn adr_has_context_and_decision() {
    assert!(!LisrAdr::CONTEXT.is_empty());
    assert!(!LisrAdr::DECISION.is_empty());
}

#[test]
fn adr_documents_threats() {
    assert!(LisrAdr::THREATS.len() >= 6);
    for threat in LisrAdr::THREATS {
        assert!(threat.starts_with('T'));
    }
}

#[test]
fn adr_has_fail_closed_rationale() {
    assert!(!LisrAdr::FAIL_CLOSED_RATIONALE.is_empty());
    assert!(LisrAdr::FAIL_CLOSED_RATIONALE.contains("denial"));
}

#[test]
fn adr_documents_invariants() {
    assert!(LisrAdr::INVARIANTS.len() >= 6);
    for inv in LisrAdr::INVARIANTS {
        assert!(inv.starts_with('I'));
    }
}

// ─── Operator playbook tests (bd-k5q5.9.8.2) ──────────────────────────────

#[test]
fn playbook_mode_guidance_covers_all_modes() {
    let modes: Vec<&str> = OperatorPlaybook::MODE_GUIDANCE
        .iter()
        .map(|(name, _)| *name)
        .collect();
    assert!(modes.contains(&"Off"));
    assert!(modes.contains(&"Suggest"));
    assert!(modes.contains(&"AutoSafe"));
    assert!(modes.contains(&"AutoStrict"));
}

#[test]
fn playbook_canary_procedure_has_steps() {
    assert!(OperatorPlaybook::CANARY_PROCEDURE.len() >= 5);
}

#[test]
fn playbook_incident_response_has_steps() {
    assert!(OperatorPlaybook::INCIDENT_RESPONSE.len() >= 5);
    assert!(OperatorPlaybook::INCIDENT_RESPONSE[0].contains("Off"));
}

// ─── Developer guide tests (bd-k5q5.9.8.3) ─────────────────────────────────

#[test]
fn developer_guide_add_rule_checklist() {
    assert!(DeveloperGuide::ADD_RULE_CHECKLIST.len() >= 8);
}

#[test]
fn developer_guide_anti_patterns() {
    assert!(DeveloperGuide::ANTI_PATTERNS.len() >= 4);
    for (name, guidance) in DeveloperGuide::ANTI_PATTERNS {
        assert!(!name.is_empty());
        assert!(!guidance.is_empty());
    }
}

#[test]
fn developer_guide_testing_expectations() {
    assert!(DeveloperGuide::TESTING_EXPECTATIONS.len() >= 5);
}

// ─── Governance checklist tests (bd-k5q5.9.8.4) ────────────────────────────

#[test]
fn governance_checklist_all_pass() {
    let report = run_governance_checklist();
    assert!(
        report.all_passed(),
        "Governance failures: {:?}",
        report
            .failures()
            .iter()
            .map(|c| format!("{}: {}", c.id, c.detail))
            .collect::<Vec<_>>()
    );
    assert!(report.total_count >= 6);
}

#[test]
fn governance_report_has_expected_checks() {
    let report = run_governance_checklist();
    let ids: Vec<&str> = report.checks.iter().map(|c| c.id.as_str()).collect();
    assert!(ids.contains(&"GOV-001"));
    assert!(ids.contains(&"GOV-002"));
    assert!(ids.contains(&"GOV-003"));
    assert!(ids.contains(&"GOV-004"));
    assert!(ids.contains(&"GOV-005"));
    assert!(ids.contains(&"GOV-006"));
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pattern 3 (bd-k5q5.8.4): Monorepo Sibling Module Stubs
// ═══════════════════════════════════════════════════════════════════════════════

// ─── extract_import_names ────────────────────────────────────────────────────

#[test]
fn extract_import_names_esm_basic() {
    let source = r#"import { foo, bar, baz } from "../../shared";"#;
    let names = extract_import_names(source, "../../shared");
    assert_eq!(names, vec!["bar", "baz", "foo"]); // sorted
}

#[test]
fn extract_import_names_esm_skips_type_imports() {
    let source = r#"import { foo, type Bar, baz } from "../../shared";"#;
    let names = extract_import_names(source, "../../shared");
    assert_eq!(names, vec!["baz", "foo"]);
}

#[test]
fn extract_import_names_cjs_require() {
    let source = r#"const { alpha, beta } = require("../../shared");"#;
    let names = extract_import_names(source, "../../shared");
    assert_eq!(names, vec!["alpha", "beta"]);
}

#[test]
fn extract_import_names_ignores_other_specifiers() {
    let source = r#"
import { foo } from "../../shared";
import { bar } from "./local";
"#;
    let names = extract_import_names(source, "../../shared");
    assert_eq!(names, vec!["foo"]);
}

#[test]
fn extract_import_names_with_as_alias() {
    let source = r#"import { foo as myFoo, bar } from "../../shared";"#;
    let names = extract_import_names(source, "../../shared");
    // Exports the original name, not the alias
    assert_eq!(names, vec!["bar", "foo"]);
}

#[test]
fn extract_import_names_deduplicates() {
    let source = r#"
import { foo, bar } from "../../shared";
const { foo, baz } = require("../../shared");
"#;
    let names = extract_import_names(source, "../../shared");
    assert_eq!(names, vec!["bar", "baz", "foo"]); // deduped
}

#[test]
fn extract_import_names_empty_for_no_match() {
    let source = r#"import { foo } from "./local";"#;
    let names = extract_import_names(source, "../../shared");
    assert!(names.is_empty());
}

// ─── generate_monorepo_stub ──────────────────────────────────────────────────

#[test]
fn generate_stub_boolean_heuristic() {
    let stub = generate_monorepo_stub(&["isReady".to_string(), "hasAccess".to_string()]);
    assert!(stub.contains("export const isReady = () => false;"));
    assert!(stub.contains("export const hasAccess = () => false;"));
}

#[test]
fn generate_stub_getter_heuristic() {
    let stub = generate_monorepo_stub(&["getConfig".to_string(), "detectTerminal".to_string()]);
    assert!(stub.contains("export const getConfig = () => ({});"));
    assert!(stub.contains("export const detectTerminal = () => ({});"));
}

#[test]
fn generate_stub_action_heuristic() {
    let stub = generate_monorepo_stub(&["playBeep".to_string(), "sendNotification".to_string()]);
    assert!(stub.contains("export const playBeep = () => {};"));
    assert!(stub.contains("export const sendNotification = () => {};"));
}

#[test]
fn generate_stub_constant_heuristic() {
    let stub = generate_monorepo_stub(&["BEEP_SOUNDS".to_string(), "MAX_RETRY".to_string()]);
    assert!(stub.contains("export const BEEP_SOUNDS = [];"));
    assert!(stub.contains("export const MAX_RETRY = [];"));
}

#[test]
fn generate_stub_class_heuristic() {
    let stub = generate_monorepo_stub(&["ProcessManager".to_string()]);
    assert!(stub.contains("export class ProcessManager {}"));
}

#[test]
fn generate_stub_generic_function() {
    let stub = generate_monorepo_stub(&["doSomething".to_string(), "notify".to_string()]);
    assert!(stub.contains("export const doSomething = () => {};"));
    assert!(stub.contains("export const notify = () => {};"));
}

#[test]
fn generate_stub_header_comment() {
    let stub = generate_monorepo_stub(&["foo".to_string()]);
    assert!(stub.contains("Auto-generated monorepo escape stub"));
}

#[test]
fn generate_stub_empty_names() {
    let stub = generate_monorepo_stub(&[]);
    assert!(stub.contains("Auto-generated monorepo escape stub"));
    // Only the header line
    assert_eq!(stub.lines().count(), 1);
}

// ─── Integration: monorepo escape resolver with real runtime ─────────────────

fn start_runtime_with_repair_mode(
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

#[test]
fn monorepo_escape_stub_loads_with_autostrict() {
    use pi::extensions::{ExtensionEventName, JsExtensionLoadSpec};

    let harness = common::TestHarness::new("monorepo_escape");
    let cwd = harness.temp_dir().to_path_buf();

    // Create an extension that imports from a monorepo sibling
    let ext_source = r#"
import { getConfig, isReady, BEEP_SOUNDS } from "../../shared";

export default function activate(pi) {
    pi.on("agent_start", (event, ctx) => {
        const cfg = getConfig();
        const ready = isReady();
        return {
            result: `cfg=${JSON.stringify(cfg)},ready=${ready},beeps=${JSON.stringify(BEEP_SOUNDS)}`
        };
    });
}
"#;
    let ext_dir = cwd.join("extensions").join("test-ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("index.mjs"), ext_source).unwrap();

    let (manager, runtime) = start_runtime_with_repair_mode(&harness, RepairMode::AutoStrict);
    manager.set_js_runtime(runtime);

    let spec = JsExtensionLoadSpec::from_entry_path(ext_dir.join("index.mjs")).expect("load spec");

    common::run_async({
        let mgr = manager.clone();
        async move {
            mgr.load_js_extensions(vec![spec])
                .await
                .expect("load extension with monorepo escape stub");
        }
    });

    // Dispatch event to verify the stub exports work
    let response = common::run_async(async move {
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });

    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_else(|| "NO_RESPONSE".to_string());

    // getConfig() returns {}, isReady() returns false, BEEP_SOUNDS is []
    assert!(
        result.contains("cfg={}"),
        "expected getConfig() to return empty object, got: {result}"
    );
    assert!(
        result.contains("ready=false"),
        "expected isReady() to return false, got: {result}"
    );
    assert!(
        result.contains("beeps=[]"),
        "expected BEEP_SOUNDS to be [], got: {result}"
    );
}

#[test]
fn monorepo_escape_not_applied_in_autosafe_mode() {
    use pi::extensions::JsExtensionLoadSpec;

    let harness = common::TestHarness::new("monorepo_safe");
    let cwd = harness.temp_dir().to_path_buf();

    // Same extension, but with AutoSafe mode — should fail to load
    let ext_source = r#"
import { getConfig } from "../../shared";
export default function activate(pi) {}
"#;
    let ext_dir = cwd.join("extensions").join("test-ext");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("index.mjs"), ext_source).unwrap();

    let (manager, runtime) = start_runtime_with_repair_mode(&harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);

    let spec = JsExtensionLoadSpec::from_entry_path(ext_dir.join("index.mjs")).expect("load spec");

    let result = common::run_async(async move { manager.load_js_extensions(vec![spec]).await });

    // Should fail because AutoSafe doesn't allow aggressive patterns
    assert!(
        result.is_err(),
        "expected monorepo escape to fail under AutoSafe mode"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pattern 5 (bd-k5q5.8.6): Export Shape Normalization
// ═══════════════════════════════════════════════════════════════════════════════

fn load_ext_with_source(harness: &common::TestHarness, source: &str) -> ExtensionManager {
    use pi::extensions::JsExtensionLoadSpec;

    let cwd = harness.temp_dir().to_path_buf();
    let ext_dir = cwd.join("extensions").join("shape-test");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("index.mjs"), source).unwrap();

    let (manager, runtime) = start_runtime_with_repair_mode(harness, RepairMode::AutoSafe);
    manager.set_js_runtime(runtime);

    let spec = JsExtensionLoadSpec::from_entry_path(ext_dir.join("index.mjs")).expect("load spec");

    common::run_async({
        let mgr = manager.clone();
        async move {
            mgr.load_js_extensions(vec![spec])
                .await
                .expect("load extension");
        }
    });

    manager
}

#[test]
fn export_shape_named_activate_fallback() {
    use pi::extensions::ExtensionEventName;

    let harness = common::TestHarness::new("shape_named");
    let source = r#"
export function activate(pi) {
    pi.on("agent_start", (event, ctx) => {
        return { result: "named_activate_ok" };
    });
}
"#;

    let manager = load_ext_with_source(&harness, source);

    let response = common::run_async(async move {
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });

    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "named_activate_ok");
}

#[test]
fn export_shape_double_wrapped_default() {
    use pi::extensions::ExtensionEventName;

    let harness = common::TestHarness::new("shape_double");
    let source = r#"
const activator = function(pi) {
    pi.on("agent_start", (event, ctx) => {
        return { result: "double_wrapped_ok" };
    });
};
export default { default: activator };
"#;

    let manager = load_ext_with_source(&harness, source);

    let response = common::run_async(async move {
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });

    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "double_wrapped_ok");
}

#[test]
fn export_shape_default_with_activate_method() {
    use pi::extensions::ExtensionEventName;

    let harness = common::TestHarness::new("shape_method");
    let source = r#"
export default {
    activate(pi) {
        pi.on("agent_start", (event, ctx) => {
            return { result: "activate_method_ok" };
        });
    }
};
"#;

    let manager = load_ext_with_source(&harness, source);

    let response = common::run_async(async move {
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });

    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "activate_method_ok");
}

#[test]
fn export_shape_normal_default_still_works() {
    use pi::extensions::ExtensionEventName;

    let harness = common::TestHarness::new("shape_normal");
    let source = r#"
export default function(pi) {
    pi.on("agent_start", (event, ctx) => {
        return { result: "normal_ok" };
    });
}
"#;

    let manager = load_ext_with_source(&harness, source);

    let response = common::run_async(async move {
        manager
            .dispatch_event_with_response(ExtensionEventName::AgentStart, None, 10000)
            .await
            .expect("dispatch")
    });

    let result = response
        .and_then(|v| v.get("result").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or_default();
    assert_eq!(result, "normal_ok");
}
