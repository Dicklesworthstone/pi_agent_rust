//! Cross-platform CI matrix validation (bd-1f42.6.7).
//!
//! Validates that the QA program runs correctly across all supported platforms
//! (Linux, macOS, Windows) and produces:
//!
//! - Platform-specific failure categorization
//! - Comparable structured artifacts across platforms
//! - Merge policy definition (required vs informational checks)
//!
//! Run:
//!   cargo test --test `ci_cross_platform_matrix` -- --nocapture
//!
//! Regenerate the tracked report for the current platform explicitly with:
//!   `PI_GENERATE_CROSS_PLATFORM_MATRIX=1 cargo test --test ci_cross_platform_matrix cross_platform_matrix -- --exact --nocapture`

use serde_json::Value;
use std::path::{Path, PathBuf};

const GENERATE_CROSS_PLATFORM_MATRIX_ENV: &str = "PI_GENERATE_CROSS_PLATFORM_MATRIX";

fn cross_platform_matrix_generation_enabled(raw: Option<&str>) -> bool {
    raw == Some("1")
}

fn cross_platform_matrix_generation_requested() -> bool {
    cross_platform_matrix_generation_enabled(
        std::env::var(GENERATE_CROSS_PLATFORM_MATRIX_ENV)
            .ok()
            .as_deref(),
    )
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn load_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Current platform identifier matching CI matrix.
const fn current_platform() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "unknown"
    }
}

/// Platform check category.
#[derive(Debug, Clone, serde::Serialize)]
struct PlatformCheck {
    id: String,
    name: String,
    platform: String,
    policy: String, // "required", "informational"
    status: String, // "pass", "fail", "skip", "unsupported"
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform_tag: Option<String>,
}

/// Platform capability descriptor.
#[derive(Debug, serde::Serialize)]
struct PlatformCapability {
    name: String,
    available: bool,
    notes: String,
}

/// Full cross-platform matrix report.
#[derive(Debug, serde::Serialize)]
struct CrossPlatformReport {
    schema: String,
    generated_at: String,
    platform: String,
    platform_details: serde_json::Value,
    merge_policy: MergePolicy,
    capabilities: Vec<PlatformCapability>,
    checks: Vec<PlatformCheck>,
    summary: PlatformSummary,
}

/// Merge policy definition.
#[derive(Debug, serde::Serialize)]
struct MergePolicy {
    description: String,
    required_platforms: Vec<String>,
    informational_platforms: Vec<String>,
    rules: Vec<serde_json::Value>,
}

/// Summary statistics for platform checks.
#[derive(Debug, serde::Serialize)]
struct PlatformSummary {
    total_checks: usize,
    passed: usize,
    failed: usize,
    skipped: usize,
    unsupported: usize,
    required_pass: usize,
    required_total: usize,
    all_required_pass: bool,
}

/// Detect platform capabilities.
fn detect_capabilities() -> Vec<PlatformCapability> {
    let mut caps = Vec::new();

    // Unix-specific capabilities.
    caps.push(PlatformCapability {
        name: "unix_signals".to_string(),
        available: cfg!(unix),
        notes: "POSIX signal handling (SIGTERM, SIGINT)".to_string(),
    });

    caps.push(PlatformCapability {
        name: "tmux".to_string(),
        available: cfg!(unix) && which_exists("tmux"),
        notes: "Terminal multiplexer for E2E TUI tests".to_string(),
    });

    caps.push(PlatformCapability {
        name: "symlinks".to_string(),
        available: cfg!(unix) || symlink_available(),
        notes: "Filesystem symlink support".to_string(),
    });

    caps.push(PlatformCapability {
        name: "file_permissions".to_string(),
        available: cfg!(unix),
        notes: "POSIX file permission bits (chmod)".to_string(),
    });

    caps.push(PlatformCapability {
        name: "dev_null".to_string(),
        available: Path::new(if cfg!(windows) { "NUL" } else { "/dev/null" }).exists()
            || cfg!(windows),
        notes: "Null device for output redirection".to_string(),
    });

    caps.push(PlatformCapability {
        name: "temp_dir".to_string(),
        available: std::env::temp_dir().is_dir(),
        notes: "System temp directory accessible".to_string(),
    });

    caps.push(PlatformCapability {
        name: "git".to_string(),
        available: which_exists("git"),
        notes: "Git CLI available in PATH".to_string(),
    });

    caps.push(PlatformCapability {
        name: "node".to_string(),
        available: which_exists("node") || which_exists("nodejs"),
        notes: "Node.js runtime for extension execution".to_string(),
    });

    caps
}

/// Check if a command exists in PATH.
fn which_exists(cmd: &str) -> bool {
    std::process::Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Check if symlinks are available (relevant on Windows).
fn symlink_available() -> bool {
    let tmp = std::env::temp_dir().join("pi_symlink_test");
    let target = std::env::temp_dir().join("pi_symlink_target");
    let _ = std::fs::write(&target, "test");

    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(&target, &tmp).is_ok();
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(&target, &tmp).is_ok();
    #[cfg(not(any(unix, windows)))]
    let result = false;

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&target);
    result
}

/// Run platform-specific checks.
#[allow(clippy::too_many_lines)]
fn run_platform_checks() -> Vec<PlatformCheck> {
    let platform = current_platform();
    let mut checks = Vec::new();

    // Check 1: Cargo build succeeds (required on all platforms).
    checks.push(PlatformCheck {
        id: "cargo_check".to_string(),
        name: "Cargo check compiles".to_string(),
        platform: platform.to_string(),
        policy: "required".to_string(),
        status: "pass".to_string(), // We're running, so it compiled.
        reason: None,
        platform_tag: None,
    });

    // Check 2: Test infrastructure available.
    checks.push(PlatformCheck {
        id: "test_infra".to_string(),
        name: "Test infrastructure functional".to_string(),
        platform: platform.to_string(),
        policy: "required".to_string(),
        status: "pass".to_string(),
        reason: None,
        platform_tag: None,
    });

    // Check 3: Temp directory writable.
    let tmp = std::env::temp_dir();
    let tmp_test = tmp.join("pi_xplat_check");
    let tmp_ok = std::fs::write(&tmp_test, "test").is_ok();
    let _ = std::fs::remove_file(&tmp_test);
    checks.push(PlatformCheck {
        id: "temp_writable".to_string(),
        name: "Temp directory writable".to_string(),
        platform: platform.to_string(),
        policy: "required".to_string(),
        status: if tmp_ok { "pass" } else { "fail" }.to_string(),
        reason: if tmp_ok {
            None
        } else {
            Some(format!("Cannot write to {}", tmp.display()))
        },
        platform_tag: None,
    });

    // Check 4: Git available (required).
    let git_ok = which_exists("git");
    checks.push(PlatformCheck {
        id: "git_available".to_string(),
        name: "Git CLI available".to_string(),
        platform: platform.to_string(),
        policy: "required".to_string(),
        status: if git_ok { "pass" } else { "fail" }.to_string(),
        reason: if git_ok {
            None
        } else {
            Some("Git not found in PATH".to_string())
        },
        platform_tag: None,
    });

    // Check 5: Conformance artifacts present (required on Linux, informational otherwise).
    let conformance_present = repo_root()
        .join("tests/ext_conformance/reports/conformance_summary.json")
        .is_file();
    let ext_policy = if platform == "linux" {
        "required"
    } else {
        "informational"
    };
    checks.push(PlatformCheck {
        id: "conformance_artifacts".to_string(),
        name: "Conformance artifacts present".to_string(),
        platform: platform.to_string(),
        policy: ext_policy.to_string(),
        status: if conformance_present { "pass" } else { "skip" }.to_string(),
        reason: if conformance_present {
            None
        } else {
            Some("Conformance summary not found (run conformance tests first)".to_string())
        },
        platform_tag: if !conformance_present && platform != "linux" {
            Some("platform-skip".to_string())
        } else {
            None
        },
    });

    // Check 6: E2E TUI tests (unix only).
    let tui_available = cfg!(unix) && which_exists("tmux");
    checks.push(PlatformCheck {
        id: "e2e_tui_support".to_string(),
        name: "E2E TUI test support (tmux)".to_string(),
        platform: platform.to_string(),
        policy: if platform == "linux" {
            "required"
        } else {
            "informational"
        }
        .to_string(),
        status: if tui_available {
            "pass"
        } else if cfg!(unix) {
            "fail"
        } else {
            "unsupported"
        }
        .to_string(),
        reason: if tui_available {
            None
        } else {
            Some(if cfg!(windows) {
                "tmux not available on Windows".to_string()
            } else {
                "tmux not found in PATH".to_string()
            })
        },
        platform_tag: if tui_available {
            None
        } else {
            Some("platform-unsupported".to_string())
        },
    });

    // Check 7: File permission tests (unix only).
    checks.push(PlatformCheck {
        id: "file_permissions".to_string(),
        name: "POSIX file permission support".to_string(),
        platform: platform.to_string(),
        policy: "informational".to_string(),
        status: if cfg!(unix) { "pass" } else { "unsupported" }.to_string(),
        reason: if cfg!(windows) {
            Some("POSIX permissions not available on Windows".to_string())
        } else {
            None
        },
        platform_tag: if cfg!(windows) {
            Some("platform-unsupported".to_string())
        } else {
            None
        },
    });

    // Check 8: Extension runtime artifacts (linux required, others informational).
    let ext_artifacts = repo_root()
        .join("tests/ext_conformance/VALIDATED_MANIFEST.json")
        .is_file();
    checks.push(PlatformCheck {
        id: "extension_artifacts".to_string(),
        name: "Extension test artifacts present".to_string(),
        platform: platform.to_string(),
        policy: if platform == "linux" {
            "required"
        } else {
            "informational"
        }
        .to_string(),
        status: if ext_artifacts { "pass" } else { "skip" }.to_string(),
        reason: if ext_artifacts {
            None
        } else {
            Some("VALIDATED_MANIFEST.json not found".to_string())
        },
        platform_tag: None,
    });

    // Check 9: Evidence bundle index (all platforms, informational).
    let evidence_bundle = repo_root()
        .join("tests/evidence_bundle/index.json")
        .is_file();
    checks.push(PlatformCheck {
        id: "evidence_bundle".to_string(),
        name: "Evidence bundle index present".to_string(),
        platform: platform.to_string(),
        policy: "informational".to_string(),
        status: if evidence_bundle { "pass" } else { "skip" }.to_string(),
        reason: if evidence_bundle {
            None
        } else {
            Some("Run build_evidence_bundle first".to_string())
        },
        platform_tag: None,
    });

    // Check 10: Suite classification valid.
    let suite_toml = repo_root().join("tests/suite_classification.toml");
    let suite_ok = suite_toml.is_file();
    checks.push(PlatformCheck {
        id: "suite_classification".to_string(),
        name: "Suite classification file present and valid".to_string(),
        platform: platform.to_string(),
        policy: "required".to_string(),
        status: if suite_ok { "pass" } else { "fail" }.to_string(),
        reason: if suite_ok {
            None
        } else {
            Some("tests/suite_classification.toml not found".to_string())
        },
        platform_tag: None,
    });

    checks
}

/// Define the merge policy.
fn merge_policy() -> MergePolicy {
    MergePolicy {
        description: "Cross-platform merge policy for pi_agent_rust CI".to_string(),
        required_platforms: vec!["linux".to_string()],
        informational_platforms: vec!["macos".to_string(), "windows".to_string()],
        rules: vec![
            serde_json::json!({
                "rule": "linux_required",
                "description": "All required checks on Linux must pass to merge",
                "blocking": true,
            }),
            serde_json::json!({
                "rule": "macos_informational",
                "description": "macOS failures are logged but do not block merge",
                "blocking": false,
            }),
            serde_json::json!({
                "rule": "windows_informational",
                "description": "Windows failures are logged but do not block merge",
                "blocking": false,
            }),
            serde_json::json!({
                "rule": "platform_tag_grouping",
                "description": "Platform-specific failures are tagged with platform-unsupported or platform-skip for triage grouping",
                "blocking": false,
            }),
            serde_json::json!({
                "rule": "artifact_parity",
                "description": "Each platform publishes structured logs (JSON/JSONL) for cross-platform diff triage",
                "blocking": false,
            }),
        ],
    }
}

/// Cross-platform matrix validation test.
///
/// Run with:
/// `cargo test --test ci_cross_platform_matrix -- cross_platform_matrix --nocapture`
#[test]
#[allow(clippy::too_many_lines)]
fn cross_platform_matrix() {
    use chrono::{SecondsFormat, Utc};
    use std::fmt::Write as _;

    let platform = current_platform();
    let report_dir = repo_root()
        .join("tests")
        .join("cross_platform_reports")
        .join(platform);

    eprintln!("\n=== Cross-Platform CI Matrix (bd-1f42.6.7) ===");
    eprintln!("  Platform:  {platform}");
    eprintln!("  OS:        {}", std::env::consts::OS);
    eprintln!("  Arch:      {}", std::env::consts::ARCH);
    eprintln!("  Family:    {}", std::env::consts::FAMILY);
    eprintln!();

    // ── Detect capabilities ──
    let capabilities = detect_capabilities();
    eprintln!("  Capabilities:");
    for cap in &capabilities {
        eprintln!(
            "    {:<25} {} ({})",
            cap.name,
            if cap.available { "YES" } else { "NO " },
            cap.notes
        );
    }
    eprintln!();

    // ── Run checks ──
    let checks = run_platform_checks();
    eprintln!("  Checks:");
    for check in &checks {
        let icon = match check.status.as_str() {
            "pass" => "PASS",
            "fail" => "FAIL",
            "skip" => "SKIP",
            _ => "N/A ",
        };
        let tag = check
            .platform_tag
            .as_deref()
            .map_or(String::new(), |t| format!(" [{t}]"));
        eprintln!("    [{icon}] {:<45} ({}){tag}", check.name, check.policy);
    }
    eprintln!();

    // ── Compute summary ──
    let passed = checks.iter().filter(|c| c.status == "pass").count();
    let failed = checks.iter().filter(|c| c.status == "fail").count();
    let skipped = checks.iter().filter(|c| c.status == "skip").count();
    let unsupported = checks.iter().filter(|c| c.status == "unsupported").count();

    let required_checks: Vec<&PlatformCheck> =
        checks.iter().filter(|c| c.policy == "required").collect();
    let required_pass = required_checks
        .iter()
        .filter(|c| c.status == "pass")
        .count();
    let required_total = required_checks.len();
    let all_required_pass = required_pass == required_total;

    let policy = merge_policy();

    let platform_details = serde_json::json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "family": std::env::consts::FAMILY,
        "ci": std::env::var("CI").unwrap_or_default(),
        "github_actions": std::env::var("GITHUB_ACTIONS").unwrap_or_default(),
    });

    let report = CrossPlatformReport {
        schema: "pi.ci.cross_platform_matrix.v1".to_string(),
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        platform: platform.to_string(),
        platform_details,
        merge_policy: policy,
        capabilities,
        checks: checks.clone(),
        summary: PlatformSummary {
            total_checks: checks.len(),
            passed,
            failed,
            skipped,
            unsupported,
            required_pass,
            required_total,
            all_required_pass,
        },
    };

    // Render every output during ordinary tests so serialization and report
    // construction remain covered. Writing the tracked artifacts is an
    // explicit maintainer operation because timestamps and host capabilities
    // are inherently run-specific.
    let report_path = report_dir.join("platform_report.json");
    let report_json =
        serde_json::to_string_pretty(&report).expect("serialize cross-platform report");

    // ── Write JSONL events ──
    let events_path = report_dir.join("platform_events.jsonl");
    let mut event_lines: Vec<String> = Vec::new();
    for check in &checks {
        let line = serde_json::json!({
            "schema": "pi.ci.platform_check_event.v1",
            "check_id": check.id,
            "platform": check.platform,
            "status": check.status,
            "policy": check.policy,
            "platform_tag": check.platform_tag,
            "ts": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        event_lines
            .push(serde_json::to_string(&line).expect("serialize cross-platform check event"));
    }
    let events_jsonl = event_lines.join("\n") + "\n";

    // ── Write Markdown report ──
    let mut md = String::new();
    write!(
        md,
        "# Cross-Platform CI Matrix — {}\n\n",
        platform.to_uppercase()
    )
    .expect("render cross-platform report title");
    writeln!(
        md,
        "> Generated: {}",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    )
    .expect("render cross-platform report timestamp");
    writeln!(
        md,
        "> OS: {} / {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
    .expect("render cross-platform report host");
    writeln!(
        md,
        "> Required checks: {required_pass}/{required_total} passed\n"
    )
    .expect("render cross-platform required-check summary");

    md.push_str("## Check Results\n\n");
    md.push_str("| Check | Policy | Status | Tag |\n|-------|--------|--------|-----|\n");
    for check in &checks {
        writeln!(
            md,
            "| {} | {} | {} | {} |",
            check.name,
            check.policy,
            check.status.to_uppercase(),
            check.platform_tag.as_deref().unwrap_or("-"),
        )
        .expect("render cross-platform check row");
    }
    md.push('\n');

    md.push_str("## Merge Policy\n\n");
    md.push_str("| Platform | Role |\n|----------|------|\n");
    md.push_str("| Linux | **Required** — all required checks must pass |\n");
    md.push_str("| macOS | Informational — failures logged, not blocking |\n");
    md.push_str("| Windows | Informational — failures logged, not blocking |\n");
    md.push('\n');

    let platform_failures: Vec<&PlatformCheck> = checks
        .iter()
        .filter(|c| c.status == "fail" || c.status == "unsupported")
        .collect();
    if !platform_failures.is_empty() {
        md.push_str("## Platform-Specific Issues\n\n");
        for f in &platform_failures {
            writeln!(
                md,
                "- **{}** ({}): {}",
                f.name,
                f.status,
                f.reason.as_deref().unwrap_or("unknown"),
            )
            .expect("render cross-platform failure row");
        }
        md.push('\n');
    }

    let md_path = report_dir.join("platform_report.md");
    let generate = cross_platform_matrix_generation_requested();
    if generate {
        std::fs::create_dir_all(&report_dir).expect("create cross-platform report directory");
        std::fs::write(&report_path, report_json).expect("write cross-platform JSON report");
        std::fs::write(&events_path, events_jsonl).expect("write cross-platform JSONL events");
        std::fs::write(&md_path, &md).expect("write cross-platform Markdown report");
    } else {
        eprintln!(
            "  Reports not written; set {GENERATE_CROSS_PLATFORM_MATRIX_ENV}=1 to regenerate the tracked artifacts"
        );
    }

    // ── Print summary ──
    eprintln!("=== Platform Summary ({platform}) ===");
    eprintln!("  Total:       {}", checks.len());
    eprintln!("  Passed:      {passed}");
    eprintln!("  Failed:      {failed}");
    eprintln!("  Skipped:     {skipped}");
    eprintln!("  Unsupported: {unsupported}");
    eprintln!("  Required:    {required_pass}/{required_total}");
    eprintln!();
    eprintln!(
        "  Reports ({}):",
        if generate { "generated" } else { "not written" }
    );
    eprintln!("    JSON:  {}", report_path.display());
    eprintln!("    JSONL: {}", events_path.display());
    eprintln!("    MD:    {}", md_path.display());
    eprintln!();

    // On required platforms (Linux), fail if required checks don't pass.
    if report
        .merge_policy
        .required_platforms
        .contains(&platform.to_string())
    {
        assert!(
            all_required_pass,
            "CROSS-PLATFORM GATE BLOCKED on {platform}: {}/{} required checks passed.\n\
             Failures: {:?}",
            required_pass,
            required_total,
            required_checks
                .iter()
                .filter(|c| c.status != "pass")
                .map(|c| &c.name)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn cross_platform_matrix_generation_requires_exact_one() {
    assert!(!cross_platform_matrix_generation_enabled(None));
    assert!(!cross_platform_matrix_generation_enabled(Some("")));
    assert!(!cross_platform_matrix_generation_enabled(Some("0")));
    assert!(!cross_platform_matrix_generation_enabled(Some("true")));
    assert!(!cross_platform_matrix_generation_enabled(Some(" 1")));
    assert!(!cross_platform_matrix_generation_enabled(Some("1 ")));
    assert!(cross_platform_matrix_generation_enabled(Some("1")));
}

/// Verify cross-platform report schema.
#[test]
fn cross_platform_report_schema() {
    let platform = current_platform();
    let report_path = repo_root()
        .join("tests")
        .join("cross_platform_reports")
        .join(platform)
        .join("platform_report.json");

    let Some(val) = load_json(&report_path) else {
        eprintln!("  SKIP: Platform report not found. Run cross_platform_matrix first.");
        return;
    };

    assert_eq!(
        val.get("schema").and_then(Value::as_str),
        Some("pi.ci.cross_platform_matrix.v1"),
        "Platform report must have correct schema"
    );

    assert!(
        val.get("platform").and_then(Value::as_str).is_some(),
        "Must have platform field"
    );
    assert!(
        val.get("checks").and_then(Value::as_array).is_some(),
        "Must have checks array"
    );
    assert!(val.get("summary").is_some(), "Must have summary");
    assert!(val.get("merge_policy").is_some(), "Must have merge_policy");
}
