//! License detection and policy screening for Pi extension candidates.
//!
//! Screens extensions for:
//! - License type (SPDX identifier)
//! - Redistributability (can we include in our corpus?)
//! - Security red flags (suspicious patterns in code)
//!
//! License detection uses filename-based heuristics and content matching
//! against common license texts. No external API calls required.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ────────────────────────────────────────────────────────────────────────────
// Types
// ────────────────────────────────────────────────────────────────────────────

/// SPDX license identifiers we recognize.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum License {
    /// MIT License.
    Mit,
    /// Apache License 2.0.
    Apache2,
    /// ISC License.
    Isc,
    /// BSD 2-Clause.
    Bsd2,
    /// BSD 3-Clause.
    Bsd3,
    /// Mozilla Public License 2.0.
    Mpl2,
    /// GNU General Public License v2.
    Gpl2,
    /// GNU General Public License v3.
    Gpl3,
    /// GNU Affero General Public License v3.
    Agpl3,
    /// GNU Lesser General Public License v2.1.
    Lgpl21,
    /// The Unlicense.
    Unlicense,
    /// Creative Commons Zero.
    Cc0,
    /// No license detected.
    Unknown,
    /// Custom/proprietary license detected.
    Custom(String),
}

impl License {
    /// Return the SPDX identifier string.
    #[must_use]
    pub fn spdx(&self) -> &str {
        match self {
            Self::Mit => "MIT",
            Self::Apache2 => "Apache-2.0",
            Self::Isc => "ISC",
            Self::Bsd2 => "BSD-2-Clause",
            Self::Bsd3 => "BSD-3-Clause",
            Self::Mpl2 => "MPL-2.0",
            Self::Gpl2 => "GPL-2.0",
            Self::Gpl3 => "GPL-3.0",
            Self::Agpl3 => "AGPL-3.0",
            Self::Lgpl21 => "LGPL-2.1",
            Self::Unlicense => "Unlicense",
            Self::Cc0 => "CC0-1.0",
            Self::Unknown => "UNKNOWN",
            Self::Custom(s) => s.as_str(),
        }
    }
}

impl std::fmt::Display for License {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.spdx())
    }
}

/// Redistributability verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Redistributable {
    /// Can freely redistribute (MIT, Apache-2.0, ISC, BSD, Unlicense, CC0).
    Yes,
    /// Copyleft — can redistribute but must maintain license (GPL, LGPL, MPL, AGPL).
    Copyleft,
    /// Cannot determine redistributability.
    Unknown,
    /// Explicitly restricted.
    No,
}

/// Security red flag severity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecuritySeverity {
    /// Informational — not necessarily malicious.
    Info,
    /// Warning — should be reviewed.
    Warning,
    /// Critical — likely malicious.
    Critical,
}

/// A security finding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityFinding {
    pub severity: SecuritySeverity,
    pub pattern: String,
    pub description: String,
}

/// Policy verdict for a single extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyVerdict {
    pub canonical_id: String,
    pub license: String,
    pub license_source: String,
    pub redistributable: Redistributable,
    pub security_findings: Vec<SecurityFinding>,
    pub verdict: VerdictStatus,
    pub notes: String,
}

/// Overall verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictStatus {
    /// Extension passes all checks.
    Pass,
    /// Extension passes but has warnings.
    PassWithWarnings,
    /// Extension is excluded due to policy violation.
    Excluded,
    /// Insufficient information to determine.
    NeedsReview,
}

/// Full screening report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreeningReport {
    pub generated_at: String,
    pub task: String,
    pub stats: ScreeningStats,
    pub verdicts: Vec<PolicyVerdict>,
}

/// Aggregate statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreeningStats {
    pub total_screened: usize,
    pub pass: usize,
    pub pass_with_warnings: usize,
    pub excluded: usize,
    pub needs_review: usize,
    pub license_distribution: HashMap<String, usize>,
}

// ────────────────────────────────────────────────────────────────────────────
// License detection
// ────────────────────────────────────────────────────────────────────────────

/// Detect license from a license file's content.
#[must_use]
pub fn detect_license_from_content(content: &str) -> License {
    let lower = content.to_lowercase();

    // MIT — most common for Pi extensions.
    if lower.contains("permission is hereby granted, free of charge")
        && lower.contains("the software is provided \"as is\"")
    {
        return License::Mit;
    }
    if lower.contains("mit license") && lower.contains("permission is hereby granted") {
        return License::Mit;
    }

    // Apache 2.0
    if lower.contains("apache license") && lower.contains("version 2.0") {
        return License::Apache2;
    }

    // ISC
    if lower.contains("isc license")
        || (lower.contains("permission to use, copy, modify") && lower.contains("isc"))
    {
        return License::Isc;
    }

    // BSD 3-Clause
    if lower.contains("redistribution and use in source and binary forms")
        && lower.contains("neither the name")
    {
        return License::Bsd3;
    }

    // BSD 2-Clause
    if lower.contains("redistribution and use in source and binary forms")
        && !lower.contains("neither the name")
    {
        return License::Bsd2;
    }

    // GPL-3.0
    if lower.contains("gnu general public license") && lower.contains("version 3") {
        return License::Gpl3;
    }

    // GPL-2.0
    if lower.contains("gnu general public license") && lower.contains("version 2") {
        return License::Gpl2;
    }

    // AGPL-3.0
    if lower.contains("gnu affero general public license") {
        return License::Agpl3;
    }

    // LGPL-2.1
    if lower.contains("gnu lesser general public license") {
        return License::Lgpl21;
    }

    // MPL-2.0
    if lower.contains("mozilla public license") && lower.contains("2.0") {
        return License::Mpl2;
    }

    // Unlicense
    if lower.contains("this is free and unencumbered software") {
        return License::Unlicense;
    }

    // CC0
    if lower.contains("cc0") || lower.contains("creative commons zero") {
        return License::Cc0;
    }

    License::Unknown
}

/// Detect license from a `package.json` license field.
#[must_use]
pub fn detect_license_from_spdx(spdx: &str) -> License {
    match spdx.trim().to_uppercase().as_str() {
        "MIT" => License::Mit,
        "APACHE-2.0" | "APACHE 2.0" => License::Apache2,
        "ISC" => License::Isc,
        "BSD-2-CLAUSE" => License::Bsd2,
        "BSD-3-CLAUSE" => License::Bsd3,
        "MPL-2.0" => License::Mpl2,
        "GPL-2.0" | "GPL-2.0-ONLY" | "GPL-2.0-OR-LATER" => License::Gpl2,
        "GPL-3.0" | "GPL-3.0-ONLY" | "GPL-3.0-OR-LATER" => License::Gpl3,
        "AGPL-3.0" | "AGPL-3.0-ONLY" | "AGPL-3.0-OR-LATER" => License::Agpl3,
        "LGPL-2.1" | "LGPL-2.1-ONLY" | "LGPL-2.1-OR-LATER" => License::Lgpl21,
        "UNLICENSE" => License::Unlicense,
        "CC0-1.0" | "CC0" => License::Cc0,
        "UNKNOWN" | "" => License::Unknown,
        other => License::Custom(other.to_string()),
    }
}

/// Determine redistributability from a license.
#[must_use]
pub const fn redistributable(license: &License) -> Redistributable {
    match license {
        License::Mit
        | License::Apache2
        | License::Isc
        | License::Bsd2
        | License::Bsd3
        | License::Unlicense
        | License::Cc0 => Redistributable::Yes,
        License::Gpl2 | License::Gpl3 | License::Agpl3 | License::Lgpl21 | License::Mpl2 => {
            Redistributable::Copyleft
        }
        License::Unknown | License::Custom(_) => Redistributable::Unknown,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Security screening
// ────────────────────────────────────────────────────────────────────────────

/// Known suspicious patterns in extension source code.
const SECURITY_PATTERNS: &[(&str, SecuritySeverity, &str)] = &[
    (
        "eval(",
        SecuritySeverity::Warning,
        "Dynamic code evaluation via eval()",
    ),
    (
        "new Function(",
        SecuritySeverity::Warning,
        "Dynamic function construction",
    ),
    (
        "child_process",
        SecuritySeverity::Info,
        "Uses child_process module (common in extensions)",
    ),
    (
        "crypto.createHash",
        SecuritySeverity::Info,
        "Uses cryptographic hashing",
    ),
    (".env", SecuritySeverity::Info, "References .env files"),
    (
        "process.env.API_KEY",
        SecuritySeverity::Warning,
        "Accesses API key from environment",
    ),
    (
        "fetch(\"http://",
        SecuritySeverity::Warning,
        "HTTP (non-HTTPS) fetch",
    ),
    (
        "XMLHttpRequest",
        SecuritySeverity::Info,
        "Uses XMLHttpRequest",
    ),
    (
        "document.cookie",
        SecuritySeverity::Critical,
        "Accesses browser cookies",
    ),
    (
        "localStorage",
        SecuritySeverity::Warning,
        "Accesses localStorage",
    ),
    (
        "Buffer.from(",
        SecuritySeverity::Info,
        "Binary buffer operations",
    ),
];

/// Scan source content for security red flags.
#[must_use]
pub fn scan_security(content: &str) -> Vec<SecurityFinding> {
    let mut findings = Vec::new();
    for (pattern, severity, description) in SECURITY_PATTERNS {
        if content.contains(pattern) {
            findings.push(SecurityFinding {
                severity: severity.clone(),
                pattern: (*pattern).to_string(),
                description: (*description).to_string(),
            });
        }
    }
    findings
}

// ────────────────────────────────────────────────────────────────────────────
// Policy screening pipeline
// ────────────────────────────────────────────────────────────────────────────

/// Input for policy screening: an extension with its known license info.
#[derive(Debug, Clone)]
pub struct ScreeningInput {
    pub canonical_id: String,
    pub known_license: Option<String>,
    pub source_tier: Option<String>,
}

/// Screen a batch of extensions and produce a report.
#[must_use]
pub fn screen_extensions(inputs: &[ScreeningInput], task_id: &str) -> ScreeningReport {
    let mut verdicts = Vec::new();
    let mut license_dist: HashMap<String, usize> = HashMap::new();

    for input in inputs {
        let license = input
            .known_license
            .as_deref()
            .map_or(License::Unknown, detect_license_from_spdx);

        let redist = redistributable(&license);
        let spdx = license.spdx().to_string();

        *license_dist.entry(spdx.clone()).or_insert(0) += 1;

        let verdict = match redist {
            Redistributable::Yes => VerdictStatus::Pass,
            Redistributable::Copyleft => VerdictStatus::PassWithWarnings,
            Redistributable::Unknown => VerdictStatus::NeedsReview,
            Redistributable::No => VerdictStatus::Excluded,
        };

        let notes = match redist {
            Redistributable::Yes => format!("{spdx}: permissive, freely redistributable"),
            Redistributable::Copyleft => {
                format!("{spdx}: copyleft, must preserve license in redistribution")
            }
            Redistributable::Unknown => "License unknown; manual review required".to_string(),
            Redistributable::No => "Restricted license; excluded from corpus".to_string(),
        };

        verdicts.push(PolicyVerdict {
            canonical_id: input.canonical_id.clone(),
            license: spdx,
            license_source: input
                .known_license
                .as_deref()
                .map_or("none", |_| "candidate_pool")
                .to_string(),
            redistributable: redist,
            security_findings: Vec::new(),
            verdict,
            notes,
        });
    }

    // Sort for stable output.
    verdicts.sort_by(|a, b| a.canonical_id.cmp(&b.canonical_id));

    let pass = verdicts
        .iter()
        .filter(|v| v.verdict == VerdictStatus::Pass)
        .count();
    let pass_warn = verdicts
        .iter()
        .filter(|v| v.verdict == VerdictStatus::PassWithWarnings)
        .count();
    let excluded = verdicts
        .iter()
        .filter(|v| v.verdict == VerdictStatus::Excluded)
        .count();
    let needs_review = verdicts
        .iter()
        .filter(|v| v.verdict == VerdictStatus::NeedsReview)
        .count();

    ScreeningReport {
        generated_at: crate::extension_validation::chrono_now_iso(),
        task: task_id.to_string(),
        stats: ScreeningStats {
            total_screened: verdicts.len(),
            pass,
            pass_with_warnings: pass_warn,
            excluded,
            needs_review,
            license_distribution: license_dist,
        },
        verdicts,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_mit_license() {
        let content = "MIT License\n\nPermission is hereby granted, free of charge...\nTHE SOFTWARE IS PROVIDED \"AS IS\"";
        assert_eq!(detect_license_from_content(content), License::Mit);
    }

    #[test]
    fn detect_apache2_license() {
        let content = "Apache License\nVersion 2.0, January 2004";
        assert_eq!(detect_license_from_content(content), License::Apache2);
    }

    #[test]
    fn detect_gpl3_license() {
        let content = "GNU GENERAL PUBLIC LICENSE\nVersion 3, 29 June 2007";
        assert_eq!(detect_license_from_content(content), License::Gpl3);
    }

    #[test]
    fn detect_unknown_license() {
        let content = "Some random text that doesn't match any license";
        assert_eq!(detect_license_from_content(content), License::Unknown);
    }

    #[test]
    fn spdx_mit() {
        assert_eq!(detect_license_from_spdx("MIT"), License::Mit);
    }

    #[test]
    fn spdx_apache() {
        assert_eq!(detect_license_from_spdx("Apache-2.0"), License::Apache2);
    }

    #[test]
    fn spdx_unknown() {
        assert_eq!(detect_license_from_spdx(""), License::Unknown);
    }

    #[test]
    fn spdx_custom() {
        assert_eq!(
            detect_license_from_spdx("WTFPL"),
            License::Custom("WTFPL".to_string())
        );
    }

    #[test]
    fn redistributable_permissive() {
        assert_eq!(redistributable(&License::Mit), Redistributable::Yes);
        assert_eq!(redistributable(&License::Apache2), Redistributable::Yes);
        assert_eq!(redistributable(&License::Isc), Redistributable::Yes);
        assert_eq!(redistributable(&License::Bsd2), Redistributable::Yes);
        assert_eq!(redistributable(&License::Bsd3), Redistributable::Yes);
        assert_eq!(redistributable(&License::Unlicense), Redistributable::Yes);
        assert_eq!(redistributable(&License::Cc0), Redistributable::Yes);
    }

    #[test]
    fn redistributable_copyleft() {
        assert_eq!(redistributable(&License::Gpl2), Redistributable::Copyleft);
        assert_eq!(redistributable(&License::Gpl3), Redistributable::Copyleft);
        assert_eq!(redistributable(&License::Agpl3), Redistributable::Copyleft);
        assert_eq!(redistributable(&License::Lgpl21), Redistributable::Copyleft);
        assert_eq!(redistributable(&License::Mpl2), Redistributable::Copyleft);
    }

    #[test]
    fn redistributable_unknown() {
        assert_eq!(redistributable(&License::Unknown), Redistributable::Unknown);
    }

    #[test]
    fn security_scan_clean() {
        let content = "function hello() { console.log('world'); }";
        assert!(scan_security(content).is_empty());
    }

    #[test]
    fn security_scan_eval() {
        let content = "eval(userInput)";
        let findings = scan_security(content);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Warning);
    }

    #[test]
    fn security_scan_cookie() {
        let content = "const token = document.cookie;";
        let findings = scan_security(content);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == SecuritySeverity::Critical)
        );
    }

    #[test]
    fn screen_extensions_basic() {
        let inputs = vec![
            ScreeningInput {
                canonical_id: "alice/ext-a".to_string(),
                known_license: Some("MIT".to_string()),
                source_tier: Some("community".to_string()),
            },
            ScreeningInput {
                canonical_id: "bob/ext-b".to_string(),
                known_license: None,
                source_tier: Some("third-party-github".to_string()),
            },
            ScreeningInput {
                canonical_id: "carol/ext-c".to_string(),
                known_license: Some("GPL-3.0".to_string()),
                source_tier: Some("community".to_string()),
            },
        ];

        let report = screen_extensions(&inputs, "test");

        assert_eq!(report.stats.total_screened, 3);
        assert_eq!(report.stats.pass, 1);
        assert_eq!(report.stats.pass_with_warnings, 1);
        assert_eq!(report.stats.needs_review, 1);

        let alice = report
            .verdicts
            .iter()
            .find(|v| v.canonical_id == "alice/ext-a")
            .unwrap();
        assert_eq!(alice.verdict, VerdictStatus::Pass);

        let bob = report
            .verdicts
            .iter()
            .find(|v| v.canonical_id == "bob/ext-b")
            .unwrap();
        assert_eq!(bob.verdict, VerdictStatus::NeedsReview);

        let carol = report
            .verdicts
            .iter()
            .find(|v| v.canonical_id == "carol/ext-c")
            .unwrap();
        assert_eq!(carol.verdict, VerdictStatus::PassWithWarnings);
    }

    #[test]
    fn verdict_serde_round_trip() {
        let v = PolicyVerdict {
            canonical_id: "test/ext".to_string(),
            license: "MIT".to_string(),
            license_source: "candidate_pool".to_string(),
            redistributable: Redistributable::Yes,
            security_findings: vec![],
            verdict: VerdictStatus::Pass,
            notes: "MIT: permissive".to_string(),
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: PolicyVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(back.canonical_id, "test/ext");
        assert_eq!(back.verdict, VerdictStatus::Pass);
    }

    #[test]
    fn license_display() {
        assert_eq!(License::Mit.to_string(), "MIT");
        assert_eq!(License::Apache2.to_string(), "Apache-2.0");
        assert_eq!(License::Custom("WTFPL".to_string()).to_string(), "WTFPL");
    }
}
