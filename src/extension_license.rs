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
    pub const fn spdx(&self) -> &str {
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
        let content = "const token  = document.cookie;";
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

    // -----------------------------------------------------------------------
    // detect_license_from_content — all license types
    // -----------------------------------------------------------------------

    #[test]
    fn detect_mit_alt_path() {
        // The second MIT detection path: "mit license" + "permission is hereby granted"
        let content = "MIT License\n\nCopyright (c) 2025\n\nPermission is hereby granted...";
        assert_eq!(detect_license_from_content(content), License::Mit);
    }

    #[test]
    fn detect_isc_license_content() {
        let content = "ISC License\n\nCopyright (c) 2025 Author\n\nPermission to use...";
        assert_eq!(detect_license_from_content(content), License::Isc);
    }

    #[test]
    fn detect_isc_alt_path() {
        let content = "Permission to use, copy, modify, and distribute... ISC";
        assert_eq!(detect_license_from_content(content), License::Isc);
    }

    #[test]
    fn detect_bsd3_content() {
        let content = "Redistribution and use in source and binary forms, with or without modification...\nNeither the name of the copyright holder...";
        assert_eq!(detect_license_from_content(content), License::Bsd3);
    }

    #[test]
    fn detect_bsd2_content() {
        let content =
            "Redistribution and use in source and binary forms, with or without modification...";
        assert_eq!(detect_license_from_content(content), License::Bsd2);
    }

    #[test]
    fn detect_gpl2_content() {
        let content = "GNU General Public License\nVersion 2, June 1991";
        assert_eq!(detect_license_from_content(content), License::Gpl2);
    }

    #[test]
    fn detect_agpl3_content() {
        let content = "GNU AFFERO GENERAL PUBLIC LICENSE\nVersion 3, 19 November 2007";
        assert_eq!(detect_license_from_content(content), License::Agpl3);
    }

    #[test]
    fn detect_lgpl21_content() {
        let content = "GNU Lesser General Public License v2.1";
        assert_eq!(detect_license_from_content(content), License::Lgpl21);
    }

    #[test]
    fn detect_mpl2_content() {
        let content = "Mozilla Public License Version 2.0";
        assert_eq!(detect_license_from_content(content), License::Mpl2);
    }

    #[test]
    fn detect_unlicense_content() {
        let content = "This is free and unencumbered software released into the public domain.";
        assert_eq!(detect_license_from_content(content), License::Unlicense);
    }

    #[test]
    fn detect_cc0_content() {
        let content = "Creative Commons Zero v1.0 Universal";
        assert_eq!(detect_license_from_content(content), License::Cc0);
    }

    #[test]
    fn detect_cc0_short() {
        let content = "Licensed under CC0";
        assert_eq!(detect_license_from_content(content), License::Cc0);
    }

    // -----------------------------------------------------------------------
    // detect_license_from_spdx — all variants + case insensitivity
    // -----------------------------------------------------------------------

    #[test]
    fn spdx_isc() {
        assert_eq!(detect_license_from_spdx("ISC"), License::Isc);
    }

    #[test]
    fn spdx_bsd2() {
        assert_eq!(detect_license_from_spdx("BSD-2-Clause"), License::Bsd2);
    }

    #[test]
    fn spdx_bsd3() {
        assert_eq!(detect_license_from_spdx("BSD-3-Clause"), License::Bsd3);
    }

    #[test]
    fn spdx_mpl2() {
        assert_eq!(detect_license_from_spdx("MPL-2.0"), License::Mpl2);
    }

    #[test]
    fn spdx_gpl2_variants() {
        assert_eq!(detect_license_from_spdx("GPL-2.0"), License::Gpl2);
        assert_eq!(detect_license_from_spdx("GPL-2.0-only"), License::Gpl2);
        assert_eq!(detect_license_from_spdx("GPL-2.0-or-later"), License::Gpl2);
    }

    #[test]
    fn spdx_gpl3_variants() {
        assert_eq!(detect_license_from_spdx("GPL-3.0"), License::Gpl3);
        assert_eq!(detect_license_from_spdx("GPL-3.0-only"), License::Gpl3);
        assert_eq!(detect_license_from_spdx("GPL-3.0-or-later"), License::Gpl3);
    }

    #[test]
    fn spdx_agpl3_variants() {
        assert_eq!(detect_license_from_spdx("AGPL-3.0"), License::Agpl3);
        assert_eq!(detect_license_from_spdx("AGPL-3.0-only"), License::Agpl3);
        assert_eq!(
            detect_license_from_spdx("AGPL-3.0-or-later"),
            License::Agpl3
        );
    }

    #[test]
    fn spdx_lgpl21_variants() {
        assert_eq!(detect_license_from_spdx("LGPL-2.1"), License::Lgpl21);
        assert_eq!(detect_license_from_spdx("LGPL-2.1-only"), License::Lgpl21);
        assert_eq!(
            detect_license_from_spdx("LGPL-2.1-or-later"),
            License::Lgpl21
        );
    }

    #[test]
    fn spdx_unlicense() {
        assert_eq!(detect_license_from_spdx("Unlicense"), License::Unlicense);
    }

    #[test]
    fn spdx_cc0_variants() {
        assert_eq!(detect_license_from_spdx("CC0-1.0"), License::Cc0);
        assert_eq!(detect_license_from_spdx("CC0"), License::Cc0);
    }

    #[test]
    fn spdx_case_insensitive() {
        assert_eq!(detect_license_from_spdx("mit"), License::Mit);
        assert_eq!(detect_license_from_spdx("apache-2.0"), License::Apache2);
        assert_eq!(detect_license_from_spdx("  MIT  "), License::Mit);
    }

    #[test]
    fn spdx_apache_space_variant() {
        assert_eq!(detect_license_from_spdx("Apache 2.0"), License::Apache2);
    }

    #[test]
    fn spdx_unknown_explicit() {
        assert_eq!(detect_license_from_spdx("UNKNOWN"), License::Unknown);
    }

    // -----------------------------------------------------------------------
    // License::spdx() — all variants
    // -----------------------------------------------------------------------

    #[test]
    fn spdx_identifiers_all_variants() {
        assert_eq!(License::Mit.spdx(), "MIT");
        assert_eq!(License::Apache2.spdx(), "Apache-2.0");
        assert_eq!(License::Isc.spdx(), "ISC");
        assert_eq!(License::Bsd2.spdx(), "BSD-2-Clause");
        assert_eq!(License::Bsd3.spdx(), "BSD-3-Clause");
        assert_eq!(License::Mpl2.spdx(), "MPL-2.0");
        assert_eq!(License::Gpl2.spdx(), "GPL-2.0");
        assert_eq!(License::Gpl3.spdx(), "GPL-3.0");
        assert_eq!(License::Agpl3.spdx(), "AGPL-3.0");
        assert_eq!(License::Lgpl21.spdx(), "LGPL-2.1");
        assert_eq!(License::Unlicense.spdx(), "Unlicense");
        assert_eq!(License::Cc0.spdx(), "CC0-1.0");
        assert_eq!(License::Unknown.spdx(), "UNKNOWN");
        assert_eq!(License::Custom("WTFPL".to_string()).spdx(), "WTFPL");
    }

    // -----------------------------------------------------------------------
    // redistributable — custom variant
    // -----------------------------------------------------------------------

    #[test]
    fn redistributable_custom_is_unknown() {
        assert_eq!(
            redistributable(&License::Custom("proprietary".to_string())),
            Redistributable::Unknown
        );
    }

    // -----------------------------------------------------------------------
    // scan_security — comprehensive pattern coverage
    // -----------------------------------------------------------------------

    #[test]
    fn security_scan_new_function() {
        let findings = scan_security("const fn = new Function('return 1')");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Warning);
        assert!(findings[0].pattern.contains("new Function("));
    }

    #[test]
    fn security_scan_child_process() {
        let findings = scan_security("const cp = require('child_process')");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Info);
    }

    #[test]
    fn security_scan_crypto_hash() {
        let findings = scan_security("crypto.createHash('sha256')");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Info);
    }

    #[test]
    fn security_scan_env_file() {
        let findings = scan_security("fs.readFileSync('.env')");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Info);
    }

    #[test]
    fn security_scan_api_key_env() {
        let findings = scan_security("const key = process.env.API_KEY;");
        // Matches both ".env" and "process.env.API_KEY"
        assert!(!findings.is_empty());
        assert!(findings.iter().any(|f| f.pattern == "process.env.API_KEY"));
    }

    #[test]
    fn security_scan_http_fetch() {
        let findings = scan_security(r#"fetch("http://evil.com")"#);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Warning);
    }

    #[test]
    fn security_scan_localstorage() {
        let findings = scan_security("localStorage.setItem('key', 'value')");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Warning);
    }

    #[test]
    fn security_scan_buffer_from() {
        let findings = scan_security("const b = Buffer.from('hello')");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Info);
    }

    #[test]
    fn security_scan_xmlhttprequest() {
        let findings = scan_security("new XMLHttpRequest()");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, SecuritySeverity::Info);
    }

    #[test]
    fn security_scan_multiple_findings() {
        let content = "eval(x); document.cookie; localStorage.getItem('k')";
        let findings = scan_security(content);
        assert!(findings.len() >= 3);
        assert!(
            findings
                .iter()
                .any(|f| f.severity == SecuritySeverity::Critical)
        );
        assert!(
            findings
                .iter()
                .any(|f| f.severity == SecuritySeverity::Warning)
        );
    }

    // -----------------------------------------------------------------------
    // screen_extensions — edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn screen_extensions_empty_input() {
        let report = screen_extensions(&[], "empty-test");
        assert_eq!(report.stats.total_screened, 0);
        assert_eq!(report.stats.pass, 0);
        assert!(report.verdicts.is_empty());
        assert_eq!(report.task, "empty-test");
    }

    #[test]
    fn screen_extensions_sorted_output() {
        let inputs = vec![
            ScreeningInput {
                canonical_id: "zzz/ext".to_string(),
                known_license: Some("MIT".to_string()),
                source_tier: None,
            },
            ScreeningInput {
                canonical_id: "aaa/ext".to_string(),
                known_license: Some("MIT".to_string()),
                source_tier: None,
            },
        ];
        let report = screen_extensions(&inputs, "sort-test");
        assert_eq!(report.verdicts[0].canonical_id, "aaa/ext");
        assert_eq!(report.verdicts[1].canonical_id, "zzz/ext");
    }

    #[test]
    fn screen_extensions_license_distribution() {
        let inputs = vec![
            ScreeningInput {
                canonical_id: "a".to_string(),
                known_license: Some("MIT".to_string()),
                source_tier: None,
            },
            ScreeningInput {
                canonical_id: "b".to_string(),
                known_license: Some("MIT".to_string()),
                source_tier: None,
            },
            ScreeningInput {
                canonical_id: "c".to_string(),
                known_license: Some("Apache-2.0".to_string()),
                source_tier: None,
            },
        ];
        let report = screen_extensions(&inputs, "dist-test");
        assert_eq!(report.stats.license_distribution["MIT"], 2);
        assert_eq!(report.stats.license_distribution["Apache-2.0"], 1);
    }

    #[test]
    fn screen_extensions_notes_content() {
        let inputs = vec![
            ScreeningInput {
                canonical_id: "a".to_string(),
                known_license: Some("MIT".to_string()),
                source_tier: None,
            },
            ScreeningInput {
                canonical_id: "b".to_string(),
                known_license: Some("GPL-3.0".to_string()),
                source_tier: None,
            },
            ScreeningInput {
                canonical_id: "c".to_string(),
                known_license: None,
                source_tier: None,
            },
        ];
        let report = screen_extensions(&inputs, "notes-test");
        let a = report
            .verdicts
            .iter()
            .find(|v| v.canonical_id == "a")
            .unwrap();
        assert!(a.notes.contains("permissive"));
        let b = report
            .verdicts
            .iter()
            .find(|v| v.canonical_id == "b")
            .unwrap();
        assert!(b.notes.contains("copyleft"));
        let c = report
            .verdicts
            .iter()
            .find(|v| v.canonical_id == "c")
            .unwrap();
        assert!(c.notes.contains("manual review"));
    }

    // -----------------------------------------------------------------------
    // Serde round-trips for enums
    // -----------------------------------------------------------------------

    #[test]
    fn redistributable_serde_roundtrip() {
        for variant in &[
            Redistributable::Yes,
            Redistributable::Copyleft,
            Redistributable::Unknown,
            Redistributable::No,
        ] {
            let json = serde_json::to_string(variant).unwrap();
            let back: Redistributable = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, variant);
        }
    }

    #[test]
    fn verdict_status_serde_roundtrip() {
        for variant in &[
            VerdictStatus::Pass,
            VerdictStatus::PassWithWarnings,
            VerdictStatus::Excluded,
            VerdictStatus::NeedsReview,
        ] {
            let json = serde_json::to_string(variant).unwrap();
            let back: VerdictStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, variant);
        }
    }

    #[test]
    fn security_severity_serde_roundtrip() {
        for variant in &[
            SecuritySeverity::Info,
            SecuritySeverity::Warning,
            SecuritySeverity::Critical,
        ] {
            let json = serde_json::to_string(variant).unwrap();
            let back: SecuritySeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, variant);
        }
    }

    #[test]
    fn license_serde_roundtrip() {
        let licenses = vec![
            License::Mit,
            License::Apache2,
            License::Isc,
            License::Bsd2,
            License::Bsd3,
            License::Mpl2,
            License::Gpl2,
            License::Gpl3,
            License::Agpl3,
            License::Lgpl21,
            License::Unlicense,
            License::Cc0,
            License::Unknown,
            License::Custom("WTFPL".to_string()),
        ];
        for lic in &licenses {
            let json = serde_json::to_string(lic).unwrap();
            let back: License = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, lic);
        }
    }

    #[test]
    fn screening_report_serde_roundtrip() {
        let report = ScreeningReport {
            generated_at: "2026-01-01T00:00:00Z".to_string(),
            task: "test".to_string(),
            stats: ScreeningStats {
                total_screened: 1,
                pass: 1,
                pass_with_warnings: 0,
                excluded: 0,
                needs_review: 0,
                license_distribution: std::iter::once(("MIT".to_string(), 1)).collect(),
            },
            verdicts: vec![PolicyVerdict {
                canonical_id: "test/ext".to_string(),
                license: "MIT".to_string(),
                license_source: "candidate_pool".to_string(),
                redistributable: Redistributable::Yes,
                security_findings: vec![SecurityFinding {
                    severity: SecuritySeverity::Info,
                    pattern: "child_process".to_string(),
                    description: "test".to_string(),
                }],
                verdict: VerdictStatus::Pass,
                notes: "ok".to_string(),
            }],
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: ScreeningReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.stats.total_screened, 1);
        assert_eq!(back.verdicts.len(), 1);
        assert_eq!(back.verdicts[0].security_findings.len(), 1);
    }

    mod proptest_extension_license {
        use super::*;
        use proptest::prelude::*;

        /// All known licenses (excludes Custom).
        fn all_known_licenses() -> Vec<License> {
            vec![
                License::Mit,
                License::Apache2,
                License::Isc,
                License::Bsd2,
                License::Bsd3,
                License::Mpl2,
                License::Gpl2,
                License::Gpl3,
                License::Agpl3,
                License::Lgpl21,
                License::Unlicense,
                License::Cc0,
                License::Unknown,
            ]
        }

        proptest! {
            /// `detect_license_from_content` never panics on arbitrary input.
            #[test]
            fn detect_content_never_panics(s in "(?s).{0,500}") {
                let _ = detect_license_from_content(&s);
            }

            /// `detect_license_from_spdx` never panics on arbitrary input.
            #[test]
            fn detect_spdx_never_panics(s in ".*") {
                let _ = detect_license_from_spdx(&s);
            }

            /// SPDX identifiers of known licenses roundtrip through `detect_license_from_spdx`.
            #[test]
            fn known_license_spdx_roundtrip(idx in 0..13usize) {
                let license = &all_known_licenses()[idx];
                let spdx = license.spdx();
                let back = detect_license_from_spdx(spdx);
                assert_eq!(*license, back, "roundtrip failed for {spdx}");
            }

            /// `detect_license_from_spdx` is case-insensitive.
            #[test]
            fn spdx_case_insensitive(idx in 0..13usize) {
                let license = &all_known_licenses()[idx];
                let spdx = license.spdx();
                let upper = detect_license_from_spdx(&spdx.to_uppercase());
                let lower = detect_license_from_spdx(&spdx.to_lowercase());
                assert_eq!(upper, lower, "case mismatch for {spdx}");
            }

            /// All SPDX strings are non-empty and have no leading/trailing whitespace.
            #[test]
            fn spdx_strings_are_clean(idx in 0..13usize) {
                let licenses = all_known_licenses();
                let spdx = licenses[idx].spdx();
                assert!(!spdx.is_empty());
                assert_eq!(spdx, spdx.trim());
            }

            /// `License::to_string()` equals `License::spdx()`.
            #[test]
            fn display_equals_spdx(idx in 0..13usize) {
                let license = &all_known_licenses()[idx];
                assert_eq!(license.to_string(), license.spdx());
            }

            /// `redistributable` never panics and covers all known licenses.
            #[test]
            fn redistributable_never_panics(idx in 0..13usize) {
                let license = &all_known_licenses()[idx];
                let _ = redistributable(license);
            }

            /// `Custom` licenses always map to `Redistributable::Unknown`.
            #[test]
            fn custom_always_unknown(s in "[a-zA-Z0-9 -]{1,50}") {
                let license = License::Custom(s);
                assert_eq!(redistributable(&license), Redistributable::Unknown);
            }

            /// Permissive licenses all map to `Redistributable::Yes`.
            #[test]
            fn permissive_licenses_redistributable(idx in prop::sample::select(vec![0usize, 2, 3, 4, 10, 11])) {
                let license = &all_known_licenses()[idx];
                assert_eq!(redistributable(license), Redistributable::Yes);
            }

            /// `scan_security` never panics on arbitrary input.
            #[test]
            fn scan_security_never_panics(s in "(?s).{0,500}") {
                let _ = scan_security(&s);
            }

            /// When a known security pattern is present, `scan_security` finds it.
            #[test]
            fn scan_security_finds_known_patterns(
                idx in 0..SECURITY_PATTERNS.len(),
                prefix in "[a-zA-Z ]{0,20}",
                suffix in "[a-zA-Z ]{0,20}"
            ) {
                let (pattern, _, _) = SECURITY_PATTERNS[idx];
                let content = format!("{prefix}{pattern}{suffix}");
                let findings = scan_security(&content);
                assert!(
                    findings.iter().any(|f| f.pattern == pattern),
                    "pattern '{pattern}' not found in findings"
                );
            }

            /// `screen_extensions` verdict count invariant: sum of categories = total.
            #[test]
            fn screen_report_count_invariant(
                n in 0..20usize,
                task in "[a-z]{5,10}"
            ) {
                let inputs: Vec<ScreeningInput> = (0..n)
                    .map(|i| ScreeningInput {
                        canonical_id: format!("ext-{i}"),
                        known_license: if i % 3 == 0 { Some("MIT".to_string()) } else { None },
                        source_tier: None,
                    })
                    .collect();
                let report = screen_extensions(&inputs, &task);
                assert_eq!(report.stats.total_screened, n);
                assert_eq!(
                    report.stats.pass + report.stats.pass_with_warnings
                        + report.stats.excluded + report.stats.needs_review,
                    n
                );
            }

            /// `screen_extensions` verdicts are sorted by `canonical_id`.
            #[test]
            fn screen_report_sorted(n in 0..20usize) {
                let inputs: Vec<ScreeningInput> = (0..n)
                    .map(|i| ScreeningInput {
                        canonical_id: format!("ext-{}", n - i),
                        known_license: Some("MIT".to_string()),
                        source_tier: None,
                    })
                    .collect();
                let report = screen_extensions(&inputs, "test");
                for w in report.verdicts.windows(2) {
                    assert!(w[0].canonical_id <= w[1].canonical_id);
                }
            }

            /// `screen_extensions` preserves task ID.
            #[test]
            fn screen_report_task_preserved(task in "[a-z0-9]{1,20}") {
                let report = screen_extensions(&[], &task);
                assert_eq!(report.task, task);
            }

            /// License serde roundtrip for all known variants.
            #[test]
            fn license_serde_roundtrip(idx in 0..13usize) {
                let license = all_known_licenses()[idx].clone();
                let json = serde_json::to_string(&license).unwrap();
                let back: License = serde_json::from_str(&json).unwrap();
                assert_eq!(license, back);
            }

            /// `Redistributable` serde roundtrip.
            #[test]
            fn redistributable_serde_roundtrip(idx in 0..4usize) {
                let variants = [
                    Redistributable::Yes,
                    Redistributable::Copyleft,
                    Redistributable::Unknown,
                    Redistributable::No,
                ];
                let v = variants[idx];
                let json = serde_json::to_string(&v).unwrap();
                let back: Redistributable = serde_json::from_str(&json).unwrap();
                assert_eq!(v, back);
            }
        }
    }
}
