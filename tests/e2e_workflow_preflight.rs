//! Real-user workflow e2e scripts (bd-k5q5.7.13)
//!
//! End-to-end tests that verify complete user journeys through the extension
//! system: setup, preflight analysis, safe-mode execution, policy escalation,
//! and error recovery.

mod common;

use common::TestHarness;
use pi::extension_preflight::{
    FindingCategory, FindingSeverity, PREFLIGHT_SCHEMA, PreflightAnalyzer, PreflightVerdict,
};
use pi::extensions::{
    CompatibilityScanner, ExtensionPolicy, ExtensionPolicyMode, PolicyDecision, PolicyProfile,
};
use std::fs;
use std::path::Path;

// ============================================================================
// Test helpers
// ============================================================================

/// Write a minimal extension package to a temp directory.
fn write_extension_package(root: &Path, name: &str, source: &str) {
    let ext_dir = root.join("extensions");
    fs::create_dir_all(&ext_dir).expect("mkdir extensions/");

    let entry = format!("{name}.js");
    fs::write(ext_dir.join(&entry), source).expect("write extension source");

    let pkg = serde_json::json!({
        "name": name,
        "version": "1.0.0",
        "private": true,
        "pi": {
            "extensions": [format!("extensions/{entry}")]
        }
    });
    fs::write(
        root.join("package.json"),
        serde_json::to_string_pretty(&pkg).unwrap(),
    )
    .expect("write package.json");
}

/// Write a settings.json with a specific extension policy profile.
fn write_policy_settings(root: &Path, profile: &str) {
    let settings = serde_json::json!({
        "extension_policy": {
            "profile": profile
        }
    });
    let settings_dir = root.join(".pi");
    fs::create_dir_all(&settings_dir).expect("mkdir .pi/");
    fs::write(
        settings_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .expect("write settings.json");
}

// ============================================================================
// Phase 1: Setup — verify extension package creation
// ============================================================================

#[test]
fn workflow_setup_creates_valid_extension_package() {
    let harness = TestHarness::new("workflow_setup_creates_valid_extension_package");
    let root = harness.temp_dir().to_path_buf();

    let source = r#"
export default function init(pi) {
    pi.tool({
        name: "greet",
        description: "Say hello",
        schema: { type: "object", properties: { name: { type: "string" } } },
        handler: async ({ name }) => ({ display: `Hello, ${name}!` }),
    });
}
"#;
    write_extension_package(&root, "hello-ext", source);

    harness
        .log()
        .info("setup", "Extension package written".to_string());

    // Verify package.json exists and is valid
    let pkg_path = root.join("package.json");
    assert!(pkg_path.exists(), "package.json should exist");
    let pkg: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&pkg_path).unwrap()).unwrap();
    assert_eq!(pkg["name"], "hello-ext");
    assert!(
        pkg["pi"]["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("hello-ext.js"))
    );

    // Verify extension source exists
    let ext_path = root.join("extensions/hello-ext.js");
    assert!(ext_path.exists(), "extension source should exist");
    let content = fs::read_to_string(&ext_path).unwrap();
    assert!(content.contains("pi.tool"));

    harness
        .log()
        .info("setup", "Extension package validated".to_string());
}

// ============================================================================
// Phase 2: Preflight — check compatibility before loading
// ============================================================================

#[test]
fn workflow_preflight_clean_extension_passes() {
    let harness = TestHarness::new("workflow_preflight_clean_extension_passes");
    let root = harness.temp_dir().to_path_buf();

    let source = r#"
import { Type } from "@sinclair/typebox";
import path from "node:path";

export default function init(pi) {
    pi.tool({
        name: "hello",
        description: "Greet",
        schema: Type.Object({ name: Type.String() }),
        handler: async ({ name }) => ({ display: `Hello, ${name}` }),
    });
}
"#;
    write_extension_package(&root, "clean-ext", source);

    let policy = ExtensionPolicy::default();
    let analyzer = PreflightAnalyzer::new(&policy, Some("clean-ext"));
    let report = analyzer.analyze_source("clean-ext", source);

    harness
        .log()
        .info_ctx("preflight", "Preflight result", |ctx| {
            ctx.push(("verdict".to_string(), format!("{}", report.verdict)));
            ctx.push(("errors".to_string(), report.summary.errors.to_string()));
            ctx.push(("warnings".to_string(), report.summary.warnings.to_string()));
        });

    assert_eq!(report.schema, PREFLIGHT_SCHEMA);
    assert_eq!(report.verdict, PreflightVerdict::Pass);
    assert_eq!(report.summary.errors, 0);
}

#[test]
fn workflow_preflight_dangerous_extension_warns_or_fails() {
    let harness = TestHarness::new("workflow_preflight_dangerous_extension_warns");
    let root = harness.temp_dir().to_path_buf();

    let source = r#"
import { exec } from "child_process";
import net from "node:net";

export default function init(pi) {
    pi.tool({
        name: "run-cmd",
        description: "Execute a command",
        schema: { type: "object", properties: { cmd: { type: "string" } } },
        handler: async ({ cmd }) => {
            const key = process.env.API_KEY;
            return { display: await exec(cmd) };
        },
    });
}
"#;
    write_extension_package(&root, "dangerous-ext", source);

    // Under safe policy
    let safe_policy = PolicyProfile::Safe.to_policy();
    let analyzer = PreflightAnalyzer::new(&safe_policy, Some("dangerous-ext"));
    let report = analyzer.analyze_source("dangerous-ext", source);

    harness
        .log()
        .info_ctx("preflight", "Safe policy result", |ctx| {
            ctx.push(("verdict".to_string(), format!("{}", report.verdict)));
            ctx.push(("errors".to_string(), report.summary.errors.to_string()));
        });

    assert_eq!(
        report.verdict,
        PreflightVerdict::Fail,
        "Dangerous extension should fail preflight under safe policy"
    );

    // Should have findings for exec, env, and node:net
    assert!(
        report.summary.errors >= 2,
        "Should have errors for denied capabilities and unsupported modules"
    );

    // Should have specific module finding for node:net
    let net_finding = report
        .findings
        .iter()
        .find(|f| f.category == FindingCategory::ModuleCompat && f.message.contains("node:net"));
    assert!(
        net_finding.is_some(),
        "Should flag node:net as unsupported module"
    );

    // Should have remediation suggestions
    let has_remediation = report.findings.iter().any(|f| f.remediation.is_some());
    assert!(has_remediation, "Findings should include remediation");
}

#[test]
fn workflow_preflight_partial_module_warns() {
    let harness = TestHarness::new("workflow_preflight_partial_module_warns");

    let source = r#"
import { readFile } from "fs/promises";
import crypto from "node:crypto";

export default function init(pi) {
    pi.tool({
        name: "read",
        description: "Read a file",
        schema: { type: "object", properties: { path: { type: "string" } } },
        handler: async ({ path }) => {
            const content = await readFile(path, "utf-8");
            const hash = crypto.createHash("sha256").update(content).digest("hex");
            return { display: hash };
        },
    });
}
"#;

    let policy = ExtensionPolicy::default();
    let analyzer = PreflightAnalyzer::new(&policy, None);
    let report = analyzer.analyze_source("partial-ext", source);

    harness
        .log()
        .info_ctx("preflight", "Partial module result", |ctx| {
            ctx.push(("verdict".to_string(), format!("{}", report.verdict)));
            ctx.push(("warnings".to_string(), report.summary.warnings.to_string()));
        });

    assert_eq!(
        report.verdict,
        PreflightVerdict::Warn,
        "Extension with partial modules should warn"
    );
    assert!(report.summary.warnings > 0);
}

// ============================================================================
// Phase 3: Safe-mode run — verify policy enforcement
// ============================================================================

#[test]
fn workflow_safe_mode_denies_exec_and_env() {
    let harness = TestHarness::new("workflow_safe_mode_denies_exec_and_env");

    // Simulate what happens when an extension uses exec/env under Safe policy
    let safe_policy = PolicyProfile::Safe.to_policy();

    // exec should be denied
    let exec_check = safe_policy.evaluate("exec");
    assert_eq!(exec_check.decision, PolicyDecision::Deny);
    harness.log().info_ctx("safe_mode", "exec check", |ctx| {
        ctx.push(("decision".to_string(), format!("{:?}", exec_check.decision)));
        ctx.push(("reason".to_string(), exec_check.reason.clone()));
    });

    // env should be denied
    let env_check = safe_policy.evaluate("env");
    assert_eq!(env_check.decision, PolicyDecision::Deny);

    // read/write/http should be allowed
    for cap in &["read", "write", "http", "events", "session"] {
        let check = safe_policy.evaluate(cap);
        assert_eq!(
            check.decision,
            PolicyDecision::Allow,
            "{cap} should be allowed in safe mode"
        );
    }
}

#[test]
fn workflow_safe_mode_compat_scan_with_policy() {
    let harness = TestHarness::new("workflow_safe_mode_compat_scan_with_policy");
    let root = harness.temp_dir().to_path_buf();

    // Extension that needs exec capability
    let source = r#"
import { execSync } from "child_process";

export default function init(pi) {
    pi.tool({
        name: "ls",
        description: "List files",
        schema: { type: "object", properties: {} },
        handler: async () => {
            const output = execSync("ls -la").toString();
            return { display: output };
        },
    });
}
"#;
    write_extension_package(&root, "ls-ext", source);

    // Run compat scan
    let scanner = CompatibilityScanner::new(root.clone());
    let ledger = scanner
        .scan_path(&root.join("extensions"))
        .expect("scan extension");

    harness
        .log()
        .info_ctx("safe_mode", "Compat scan result", |ctx| {
            ctx.push((
                "capabilities".to_string(),
                ledger.capabilities.len().to_string(),
            ));
            ctx.push(("rewrites".to_string(), ledger.rewrites.len().to_string()));
            ctx.push(("forbidden".to_string(), ledger.forbidden.len().to_string()));
        });

    // Should detect exec capability requirement
    let needs_exec = ledger.capabilities.iter().any(|c| c.capability == "exec");
    assert!(
        needs_exec,
        "Scanner should detect exec capability requirement"
    );

    // Now check against Safe policy
    let safe_policy = PolicyProfile::Safe.to_policy();
    let exec_decision = safe_policy.evaluate("exec");
    assert_eq!(
        exec_decision.decision,
        PolicyDecision::Deny,
        "exec should be denied under Safe policy"
    );
}

// ============================================================================
// Phase 4: Escalation — upgrade from Safe to Standard/Permissive
// ============================================================================

#[test]
fn workflow_escalation_safe_to_standard() {
    let harness = TestHarness::new("workflow_escalation_safe_to_standard");

    // Compare exec decision across policy escalation
    let safe = PolicyProfile::Safe.to_policy();
    let standard = ExtensionPolicy::default(); // Standard is default
    let permissive = PolicyProfile::Permissive.to_policy();

    // Safe: exec = Deny
    assert_eq!(safe.evaluate("exec").decision, PolicyDecision::Deny);

    // Standard (default): exec is in deny_caps → Deny
    assert_eq!(standard.evaluate("exec").decision, PolicyDecision::Deny);

    // Permissive: exec = Allow
    assert_eq!(permissive.evaluate("exec").decision, PolicyDecision::Allow);

    harness
        .log()
        .info_ctx("escalation", "Policy escalation matrix", |ctx| {
            ctx.push((
                "safe_exec".to_string(),
                format!("{:?}", safe.evaluate("exec").decision),
            ));
            ctx.push((
                "standard_exec".to_string(),
                format!("{:?}", standard.evaluate("exec").decision),
            ));
            ctx.push((
                "permissive_exec".to_string(),
                format!("{:?}", permissive.evaluate("exec").decision),
            ));
        });

    // Verify preflight changes with escalation
    let dangerous_source = r#"
import { exec } from "child_process";
const key = process.env.API_KEY;
"#;

    let safe_analyzer = PreflightAnalyzer::new(&safe, None);
    let safe_report = safe_analyzer.analyze_source("esc-ext", dangerous_source);

    let permissive_analyzer = PreflightAnalyzer::new(&permissive, None);
    let perm_report = permissive_analyzer.analyze_source("esc-ext", dangerous_source);

    assert_eq!(safe_report.verdict, PreflightVerdict::Fail);
    // Under permissive, capabilities are allowed but module issues may remain
    assert_ne!(
        perm_report.verdict,
        PreflightVerdict::Fail,
        "Permissive policy should not fail on capability issues alone"
    );

    harness
        .log()
        .info_ctx("escalation", "Verdicts after escalation", |ctx| {
            ctx.push((
                "safe_verdict".to_string(),
                format!("{}", safe_report.verdict),
            ));
            ctx.push((
                "permissive_verdict".to_string(),
                format!("{}", perm_report.verdict),
            ));
        });
}

#[test]
fn workflow_escalation_per_extension_override() {
    let harness = TestHarness::new("workflow_escalation_per_extension_override");

    // Start with strict mode but use per-extension allow for specific ext
    let mut per_ext = std::collections::HashMap::new();
    per_ext.insert(
        "trusted-ext".to_string(),
        pi::extensions::ExtensionOverride {
            mode: None,
            allow: vec!["exec".to_string(), "env".to_string()],
            deny: vec![],
        },
    );

    let policy = ExtensionPolicy {
        mode: ExtensionPolicyMode::Strict,
        max_memory_mb: 256,
        default_caps: vec![
            "read".to_string(),
            "write".to_string(),
            "http".to_string(),
            "events".to_string(),
            "session".to_string(),
        ],
        deny_caps: vec![], // Not in global deny — use per-ext allow
        per_extension: per_ext,
    };

    // trusted-ext gets exec
    let trusted_check = policy.evaluate_for("exec", Some("trusted-ext"));
    assert_eq!(
        trusted_check.decision,
        PolicyDecision::Allow,
        "trusted-ext should get exec"
    );

    // untrusted-ext does not
    let untrusted_check = policy.evaluate_for("exec", Some("untrusted-ext"));
    assert_eq!(
        untrusted_check.decision,
        PolicyDecision::Deny,
        "untrusted-ext should be denied exec"
    );

    harness
        .log()
        .info_ctx("escalation", "Per-extension override", |ctx| {
            ctx.push((
                "trusted_exec".to_string(),
                format!("{:?}", trusted_check.decision),
            ));
            ctx.push((
                "untrusted_exec".to_string(),
                format!("{:?}", untrusted_check.decision),
            ));
        });

    // Preflight should reflect per-extension context
    let source = r#"
import { exec } from "child_process";
pi.exec("ls");
"#;
    let trusted_analyzer = PreflightAnalyzer::new(&policy, Some("trusted-ext"));
    let trusted_report = trusted_analyzer.analyze_source("trusted-ext", source);
    let exec_errors = trusted_report
        .findings
        .iter()
        .filter(|f| {
            f.category == FindingCategory::CapabilityPolicy
                && f.severity == FindingSeverity::Error
                && f.message.contains("exec")
        })
        .count();
    assert_eq!(exec_errors, 0, "trusted-ext should not have exec errors");

    let untrusted_analyzer = PreflightAnalyzer::new(&policy, Some("untrusted-ext"));
    let untrusted_report = untrusted_analyzer.analyze_source("untrusted-ext", source);
    let exec_errors_u = untrusted_report
        .findings
        .iter()
        .filter(|f| {
            f.category == FindingCategory::CapabilityPolicy
                && f.severity == FindingSeverity::Error
                && f.message.contains("exec")
        })
        .count();
    assert!(exec_errors_u > 0, "untrusted-ext should have exec errors");
}

// ============================================================================
// Phase 5: Recovery — error handling and graceful degradation
// ============================================================================

#[test]
fn workflow_recovery_missing_module_shows_remediation() {
    let harness = TestHarness::new("workflow_recovery_missing_module_shows_remediation");

    let source = r#"
import net from "node:net";
import tls from "node:tls";
import dns from "node:dns";
"#;

    let policy = ExtensionPolicy::default();
    let analyzer = PreflightAnalyzer::new(&policy, None);
    let report = analyzer.analyze_source("net-ext", source);

    assert_eq!(report.verdict, PreflightVerdict::Fail);

    // Each missing module should have a remediation suggestion
    for module in &["node:net", "node:tls", "node:dns"] {
        let finding = report.findings.iter().find(|f| f.message.contains(module));
        assert!(finding.is_some(), "Should have finding for {module}");
        assert!(
            finding.unwrap().remediation.is_some(),
            "Finding for {module} should include remediation"
        );
    }

    harness
        .log()
        .info_ctx("recovery", "Module remediation", |ctx| {
            for f in &report.findings {
                if let Some(rem) = &f.remediation {
                    ctx.push((f.message.clone(), rem.clone()));
                }
            }
        });
}

#[test]
fn workflow_recovery_forbidden_pattern_detected() {
    let harness = TestHarness::new("workflow_recovery_forbidden_pattern_detected");
    let root = harness.temp_dir().to_path_buf();

    let source = r#"
export default function init(pi) {
    // This should be detected as forbidden
    const binding = process.binding("natives");
    pi.tool({
        name: "bad",
        description: "Uses forbidden API",
        schema: { type: "object", properties: {} },
        handler: async () => ({ display: "bad" }),
    });
}
"#;
    write_extension_package(&root, "forbidden-ext", source);

    // Scan via CompatibilityScanner
    let scanner = CompatibilityScanner::new(root.clone());
    let ledger = scanner.scan_path(&root.join("extensions")).expect("scan");

    assert!(
        !ledger.forbidden.is_empty(),
        "Should detect forbidden process.binding usage"
    );

    // Now preflight via file path
    let policy = ExtensionPolicy::default();
    let analyzer = PreflightAnalyzer::new(&policy, Some("forbidden-ext"));
    let report = analyzer.analyze(&root.join("extensions"));

    assert_eq!(report.verdict, PreflightVerdict::Fail);
    let forbidden_finding = report
        .findings
        .iter()
        .find(|f| f.category == FindingCategory::ForbiddenPattern);
    assert!(
        forbidden_finding.is_some(),
        "Should have forbidden pattern finding"
    );

    harness.log().info(
        "recovery",
        "Forbidden pattern detected and reported".to_string(),
    );
}

#[test]
fn workflow_recovery_flagged_eval_warns() {
    let harness = TestHarness::new("workflow_recovery_flagged_eval_warns");
    let root = harness.temp_dir().to_path_buf();

    let source = r#"
export default function init(pi) {
    const code = "1 + 1";
    const result = eval(code);
    pi.tool({
        name: "calc",
        description: "Calculate",
        schema: { type: "object", properties: {} },
        handler: async () => ({ display: String(result) }),
    });
}
"#;
    write_extension_package(&root, "eval-ext", source);

    let policy = ExtensionPolicy::default();
    let analyzer = PreflightAnalyzer::new(&policy, Some("eval-ext"));
    let report = analyzer.analyze(&root.join("extensions"));

    // eval should produce a warning, not an error
    let flagged = report
        .findings
        .iter()
        .find(|f| f.category == FindingCategory::FlaggedPattern);
    assert!(flagged.is_some(), "Should detect eval as flagged pattern");
    assert_eq!(
        flagged.unwrap().severity,
        FindingSeverity::Warning,
        "eval should be a warning, not an error"
    );

    harness
        .log()
        .info("recovery", "eval pattern flagged with warning".to_string());
}

// ============================================================================
// Full journey: Setup → Preflight → Safe → Escalate → Recovery
// ============================================================================

#[test]
fn workflow_full_user_journey() {
    let harness = TestHarness::new("workflow_full_user_journey");
    let root = harness.temp_dir().to_path_buf();

    // ── Step 1: Setup ──
    harness
        .log()
        .info("journey", "Step 1: Setup extension package".to_string());

    let source = r#"
import path from "node:path";
import fs from "node:fs";
import { exec } from "child_process";

export default function init(pi) {
    pi.tool({
        name: "file-info",
        description: "Get file info",
        schema: { type: "object", properties: { file: { type: "string" } } },
        handler: async ({ file }) => {
            const resolved = path.resolve(file);
            const exists = fs.existsSync(resolved);
            const key = process.env.HOME;
            return { display: `${resolved}: exists=${exists}` };
        },
    });
}
"#;
    write_extension_package(&root, "file-info", source);

    // ── Step 2: Preflight under Safe policy ──
    harness
        .log()
        .info("journey", "Step 2: Preflight under Safe policy".to_string());

    let safe_policy = PolicyProfile::Safe.to_policy();
    let safe_analyzer = PreflightAnalyzer::new(&safe_policy, Some("file-info"));
    let safe_report = safe_analyzer.analyze_source("file-info", source);

    assert_eq!(
        safe_report.verdict,
        PreflightVerdict::Fail,
        "Should fail under safe policy (needs exec + env)"
    );

    let denied_caps: Vec<&str> = safe_report
        .findings
        .iter()
        .filter(|f| {
            f.category == FindingCategory::CapabilityPolicy && f.severity == FindingSeverity::Error
        })
        .filter_map(|f| {
            if f.message.contains("exec") {
                Some("exec")
            } else if f.message.contains("env") {
                Some("env")
            } else {
                None
            }
        })
        .collect();

    harness
        .log()
        .info_ctx("journey", "Safe preflight denied", |ctx| {
            ctx.push(("denied".to_string(), denied_caps.join(", ")));
        });

    assert!(
        denied_caps.contains(&"exec") || denied_caps.contains(&"env"),
        "Should deny exec or env under safe policy"
    );

    // ── Step 3: Escalate to Permissive ──
    harness.log().info(
        "journey",
        "Step 3: Escalate to Permissive policy".to_string(),
    );

    let permissive_policy = PolicyProfile::Permissive.to_policy();
    let perm_analyzer = PreflightAnalyzer::new(&permissive_policy, Some("file-info"));
    let perm_report = perm_analyzer.analyze_source("file-info", source);

    harness
        .log()
        .info_ctx("journey", "Permissive preflight", |ctx| {
            ctx.push(("verdict".to_string(), format!("{}", perm_report.verdict)));
            ctx.push(("errors".to_string(), perm_report.summary.errors.to_string()));
        });

    // Under permissive, capability issues go away but module issues remain
    let cap_errors = perm_report
        .findings
        .iter()
        .filter(|f| {
            f.category == FindingCategory::CapabilityPolicy && f.severity == FindingSeverity::Error
        })
        .count();
    assert_eq!(
        cap_errors, 0,
        "No capability errors under permissive policy"
    );

    // ── Step 4: Recovery check — markdown report ──
    harness
        .log()
        .info("journey", "Step 4: Generate recovery report".to_string());

    let md = safe_report.render_markdown();
    assert!(md.contains("FAIL"), "Report should show FAIL verdict");
    assert!(
        md.contains("Remediation"),
        "Report should include remediation advice"
    );

    // JSON roundtrip
    let json = safe_report.to_json().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["schema"], PREFLIGHT_SCHEMA);
    assert_eq!(parsed["verdict"], "fail");

    harness
        .log()
        .info("journey", "Full user journey completed".to_string());
}

#[test]
fn workflow_full_journey_with_settings_escalation() {
    let harness = TestHarness::new("workflow_full_journey_with_settings_escalation");
    let root = harness.temp_dir().to_path_buf();

    // ── Setup ──
    let source = r#"
import path from "node:path";
export default function init(pi) {
    pi.tool({
        name: "resolve",
        description: "Resolve path",
        schema: { type: "object", properties: { p: { type: "string" } } },
        handler: async ({ p }) => ({ display: path.resolve(p) }),
    });
}
"#;
    write_extension_package(&root, "path-ext", source);

    // ── Phase: Safe settings ──
    write_policy_settings(&root, "safe");
    let safe = PolicyProfile::Safe.to_policy();
    let analyzer = PreflightAnalyzer::new(&safe, Some("path-ext"));
    let safe_report = analyzer.analyze_source("path-ext", source);
    assert_eq!(safe_report.verdict, PreflightVerdict::Pass);

    harness.log().info_ctx("journey", "Safe phase", |ctx| {
        ctx.push(("verdict".to_string(), format!("{}", safe_report.verdict)));
    });

    // ── Phase: Escalate settings to permissive ──
    write_policy_settings(&root, "permissive");
    let permissive = PolicyProfile::Permissive.to_policy();
    let perm_analyzer = PreflightAnalyzer::new(&permissive, Some("path-ext"));
    let perm_report = perm_analyzer.analyze_source("path-ext", source);
    assert_eq!(perm_report.verdict, PreflightVerdict::Pass);

    harness
        .log()
        .info_ctx("journey", "Permissive phase", |ctx| {
            ctx.push(("verdict".to_string(), format!("{}", perm_report.verdict)));
        });

    // Both should pass for a clean extension — the difference is for dangerous ones
}

// ============================================================================
// Multi-extension workflow
// ============================================================================

#[test]
fn workflow_multi_extension_mixed_verdicts() {
    let harness = TestHarness::new("workflow_multi_extension_mixed_verdicts");

    let extensions = vec![
        (
            "safe-ext",
            r#"
import path from "node:path";
export default function(pi) { pi.tool({ name: "safe", schema: {} }); }
"#,
        ),
        (
            "partial-ext",
            r#"
import crypto from "node:crypto";
export default function(pi) { pi.tool({ name: "hash", schema: {} }); }
"#,
        ),
        (
            "dangerous-ext",
            r#"
import net from "node:net";
const key = process.env.SECRET;
export default function(pi) { pi.tool({ name: "dial", schema: {} }); }
"#,
        ),
    ];

    let policy = PolicyProfile::Safe.to_policy();

    let mut results: Vec<(&str, PreflightVerdict)> = Vec::new();

    for (name, source) in &extensions {
        let analyzer = PreflightAnalyzer::new(&policy, Some(name));
        let report = analyzer.analyze_source(name, source);
        results.push((name, report.verdict));

        harness
            .log()
            .info_ctx("multi", format!("Extension: {name}"), |ctx| {
                ctx.push(("verdict".to_string(), format!("{}", report.verdict)));
                ctx.push(("errors".to_string(), report.summary.errors.to_string()));
                ctx.push(("warnings".to_string(), report.summary.warnings.to_string()));
            });
    }

    // Verify expected verdicts
    assert_eq!(
        results[0],
        ("safe-ext", PreflightVerdict::Pass),
        "safe extension should pass"
    );
    assert_eq!(
        results[1],
        ("partial-ext", PreflightVerdict::Warn),
        "partial extension should warn"
    );
    assert_eq!(
        results[2],
        ("dangerous-ext", PreflightVerdict::Fail),
        "dangerous extension should fail"
    );

    harness
        .log()
        .info("multi", "Multi-extension workflow completed".to_string());
}

// ============================================================================
// Report output verification
// ============================================================================

#[test]
fn workflow_report_json_schema_valid() {
    let harness = TestHarness::new("workflow_report_json_schema_valid");

    let source = r#"
import chokidar from "chokidar";
import { exec } from "child_process";
"#;

    let policy = PolicyProfile::Safe.to_policy();
    let analyzer = PreflightAnalyzer::new(&policy, None);
    let report = analyzer.analyze_source("test-ext", source);

    let json = report.to_json().expect("JSON serialization");
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    // Schema compliance
    assert_eq!(parsed["schema"], PREFLIGHT_SCHEMA);
    assert!(parsed["extension_id"].is_string());
    assert!(parsed["verdict"].is_string());
    assert!(parsed["findings"].is_array());
    assert!(parsed["summary"].is_object());
    assert!(parsed["summary"]["errors"].is_number());
    assert!(parsed["summary"]["warnings"].is_number());
    assert!(parsed["summary"]["info"].is_number());

    // Each finding should have required fields
    for finding in parsed["findings"].as_array().unwrap() {
        assert!(finding["severity"].is_string());
        assert!(finding["category"].is_string());
        assert!(finding["message"].is_string());
    }

    harness
        .log()
        .info("report", "JSON schema validation passed".to_string());
}

#[test]
fn workflow_report_markdown_human_readable() {
    let harness = TestHarness::new("workflow_report_markdown_human_readable");

    let source = r#"
import net from "node:net";
import chokidar from "chokidar";
const key = process.env.API_KEY;
"#;

    let policy = PolicyProfile::Safe.to_policy();
    let analyzer = PreflightAnalyzer::new(&policy, None);
    let report = analyzer.analyze_source("md-ext", source);

    let md = report.render_markdown();

    // Should contain expected sections
    assert!(md.contains("# Preflight Report: md-ext"));
    assert!(md.contains("**Verdict**:"));
    assert!(md.contains("## Findings"));
    assert!(md.contains("Errors"));
    assert!(md.contains("Warnings"));

    // Should have at least some findings with icons
    assert!(
        md.contains("[x]") || md.contains("[!]"),
        "Should have error or warning icons"
    );

    // Save to artifact
    let md_path = harness.temp_path("preflight_report.md");
    fs::write(&md_path, &md).expect("write report");
    harness.record_artifact("preflight_report", &md_path);

    harness
        .log()
        .info("report", "Markdown report generated and saved".to_string());
}
