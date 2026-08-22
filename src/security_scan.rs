//! Agent-facing security scanner (bd-cv653.2.6): plan/run/disposition/compare
//! over deterministic rule packs, emitting SARIF v2.1.0.
//!
//! v1 is fully native: regex rule packs (versioned in-tree, project
//! overrides under `.pi/security-rules/`), a per-project disposition store
//! (`.pi/security-dispositions.json`), and an optional OSV dependency lookup
//! that degrades to a warning offline. Code never leaves the machine.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const SCAN_SCHEMA: &str = "pi.security-scan/v1";
/// SARIF version emitted by `run`.
pub const SARIF_VERSION: &str = "2.1.0";
pub const SARIF_SCHEMA_URI: &str = "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/main/sarif-2.1/schema/sarif-schema-2.1.0.json";

/// One detection rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    /// Regex matched per line (multi-line rules may embed `(?s)`).
    pub pattern: String,
    /// error | warning | note
    pub severity: String,
    pub message: String,
    #[serde(default)]
    pub fix_hint: Option<String>,
    /// File globs the rule applies to (e.g. `*.rs`); empty = all text files.
    #[serde(default)]
    pub applies_to: Vec<String>,
}

/// A named rule pack (TOML on disk).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RulePack {
    pub name: String,
    pub version: String,
    pub rules: Vec<Rule>,
}

/// One scanner hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub rule_id: String,
    pub severity: String,
    pub message: String,
    pub path: String,
    pub line: u32,
    /// Stable identity for disposition/compare: rule + path + matched text.
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
}

/// Fingerprint a finding from its stable identity parts. Line numbers are
/// excluded so edits above a finding don't churn its disposition.
#[must_use]
pub fn fingerprint(rule_id: &str, path: &str, matched_text: &str, occurrence: usize) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(rule_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(matched_text.as_bytes());
    // Occurrence ordinal (per identical match within one file): without it,
    // two different private keys in a file share one fingerprint and a
    // single false-positive disposition would silently suppress both.
    hasher.update(b"\0");
    hasher.update(occurrence.to_le_bytes());
    let digest = hasher.finalize();
    digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut out, b| {
            use std::fmt::Write;
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// The embedded default pack (secrets + injection sinks + traversal +
/// deserialization). Project packs extend, never replace, this baseline.
const DEFAULT_PACK: &str = r#"
name = "pi-default"
version = "1"

[[rules]]
id = "secret.generic-api-key"
pattern = "(?i)(api[_-]?key|api[_-]?secret)\\s*[:=]\\s*[\"'][A-Za-z0-9_/+\\-]{20,}[\"']"
severity = "error"
message = "Hardcoded API key/secret literal"
fixHint = "Move the secret to an environment variable or the secrets vault."

[[rules]]
id = "secret.aws-access-key"
pattern = "\\bAKIA[0-9A-Z]{16}\\b"
severity = "error"
message = "AWS access key id literal"
fixHint = "Use instance roles or environment credentials."

[[rules]]
id = "secret.private-key-block"
pattern = "-----BEGIN (RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----"
severity = "error"
message = "Private key material in source"
fixHint = "Remove the key material; load keys from disk at runtime."

[[rules]]
id = "injection.shell-format"
pattern = "(?i)(Command::new|exec|system|popen)\\s*\\([^)]*format!"
severity = "warning"
message = "Shell/process invocation built with format! — injection risk"
fixHint = "Pass arguments as an argv list, never an interpolated string."

[[rules]]
id = "injection.sql-format"
pattern = "(?i)(query|execute)\\s*\\(\\s*format!"
severity = "error"
message = "SQL built with format! — injection risk"
fixHint = "Use bound parameters."

[[rules]]
id = "deserialization.unsafe-pickle"
pattern = "\\b(pickle\\.loads?|yaml\\.load\\(|marshal\\.loads?)\\b"
severity = "warning"
message = "Unsafe deserialization primitive"
fixHint = "Use yaml.safe_load / json; never unpickle untrusted data."

[[rules]]
id = "traversal.join-unsanitized"
pattern = "\\.join\\([^)]*(\\barg\\b|\\binput\\b|request|param)"
severity = "note"
message = "Path join over caller-controlled input — verify canonical confinement"
fixHint = "Canonicalize and check against allowed roots."
"#;

/// Load the default pack plus any project packs under
/// `<project>/.pi/security-rules/*.toml`. A malformed project pack is a
/// named error (fail loud — silently dropping security rules is worse).
pub fn load_rule_packs(project_root: &Path) -> Result<Vec<RulePack>> {
    let mut packs: Vec<RulePack> = vec![toml::from_str(DEFAULT_PACK).map_err(|e| {
        Error::validation(format!("embedded security rule pack failed to parse: {e}"))
    })?];
    let overrides = project_root.join(".pi").join("security-rules");
    if overrides.is_dir() {
        let mut entries: Vec<PathBuf> = fs::read_dir(&overrides)
            .map_err(|e| {
                Error::tool(
                    "security_scan",
                    format!("read {}: {e}", overrides.display()),
                )
            })?
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        entries.sort();
        for path in entries {
            let text = fs::read_to_string(&path).map_err(|e| {
                Error::tool("security_scan", format!("read {}: {e}", path.display()))
            })?;
            let pack: RulePack = toml::from_str(&text)
                .map_err(|e| Error::validation(format!("rule pack {}: {e}", path.display())))?;
            packs.push(pack);
        }
    }
    Ok(packs)
}

/// Scope a scan: explicit paths, or the project root. Directories walk
/// gitignore-respecting file lists via the repo's ignore machinery.
fn scope_files(cwd: &Path, paths: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        paths
            .iter()
            .map(|p| confine_to_cwd(cwd, p, "paths"))
            .collect::<Result<Vec<_>>>()?
    };
    for root in roots {
        // A mistyped scope must not yield a false-clean "0 findings".
        if !root.exists() {
            return Err(Error::validation(format!(
                "security_scan scope path does not exist: {}",
                root.display()
            )));
        }
        if root.is_file() {
            files.push(root);
        } else if root.is_dir() {
            // `.hidden(false)` so dotfiles are scanned: `.env`, `.npmrc`,
            // `.aws/credentials` and CI workflow files are the classic
            // secret carriers. `.git/` itself is still skipped below.
            let walker = ignore::WalkBuilder::new(&root)
                .hidden(false)
                .git_ignore(true)
                .filter_entry(|entry| entry.file_name() != ".git")
                .build();
            for entry in walker.flatten() {
                let path = entry.path();
                if path.is_file() {
                    files.push(path.to_path_buf());
                }
            }
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// A rule compiled for scanning: regex, optional glob scope, multi-line flag.
struct CompiledRule {
    rule: Rule,
    regex: regex::Regex,
    applies_to: Option<globset::GlobSet>,
    multi_line: bool,
}

impl CompiledRule {
    fn build(rule: &Rule) -> Result<Self> {
        let regex = regex::Regex::new(&rule.pattern)
            .map_err(|e| Error::validation(format!("rule {} pattern invalid: {e}", rule.id)))?;
        // Validate the severity vocabulary at load: an unknown severity
        // would count as "active" while appearing in no summary bucket.
        if !matches!(rule.severity.as_str(), "error" | "warning" | "note") {
            return Err(Error::validation(format!(
                "rule {} has invalid severity {:?}; expected error, warning, or note",
                rule.id, rule.severity
            )));
        }
        let applies_to = if rule.applies_to.is_empty() {
            None
        } else {
            let mut builder = globset::GlobSetBuilder::new();
            for glob in &rule.applies_to {
                builder.add(globset::Glob::new(glob).map_err(|e| {
                    Error::validation(format!("rule {} glob {glob:?} invalid: {e}", rule.id))
                })?);
            }
            Some(builder.build().map_err(|e| {
                Error::validation(format!("rule {} applies_to globs: {e}", rule.id))
            })?)
        };
        let multi_line = rule.pattern.contains("(?s)") || rule.pattern.contains("(?s");
        Ok(Self {
            rule: rule.clone(),
            regex,
            applies_to,
            multi_line,
        })
    }
}

/// Run compiled rules over one file, returning findings with paths relative
/// to `cwd`. Binary/undecodable files are skipped.
fn scan_file(cwd: &Path, path: &Path, rules: &[CompiledRule], findings: &mut Vec<Finding>) {
    let Ok(content) = fs::read_to_string(path) else {
        return; // binary or unreadable — regex packs only scan text
    };
    let rel = path
        .strip_prefix(cwd)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf);
    let rel_display = rel.display().to_string();
    // Per (rule, matched-text) occurrence counter within this file, so
    // identical matches get distinct fingerprints.
    let mut occurrences: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut push = |rule: &Rule, line_no: usize, matched: &str, out: &mut Vec<Finding>| {
        let counter = occurrences
            .entry((rule.id.clone(), matched.to_string()))
            .or_insert(0);
        let occurrence = *counter;
        *counter += 1;
        out.push(Finding {
            rule_id: rule.id.clone(),
            severity: rule.severity.clone(),
            message: rule.message.clone(),
            path: rel_display.clone(),
            line: u32::try_from(line_no).unwrap_or(u32::MAX),
            fingerprint: fingerprint(&rule.id, &rel_display, matched, occurrence),
            fix_hint: rule.fix_hint.clone(),
        });
    };
    for compiled in rules {
        if let Some(globs) = &compiled.applies_to {
            let name = rel.file_name().map_or("", |n| n.to_str().unwrap_or(""));
            // Match both the relative path (`src/*.rs` style globs) and the
            // bare file name (`*.rs`, `Dockerfile*` style globs).
            if !globs.is_match(&rel) && !globs.is_match(name) {
                continue;
            }
        }
        if compiled.multi_line {
            // `(?s)` rules span lines: match the whole file and derive the
            // line from the match offset (line-based iteration can never
            // fire these — a silent false-negative factory).
            for m in compiled.regex.find_iter(&content) {
                let line_no = content[..m.start()].matches('\n').count() + 1;
                push(&compiled.rule, line_no, m.as_str(), findings);
            }
        } else {
            for (idx, line) in content.lines().enumerate() {
                for m in compiled.regex.find_iter(line) {
                    push(&compiled.rule, idx + 1, m.as_str(), findings);
                }
            }
        }
    }
}

/// Compile + run the packs over the scope. Rules with un compilable patterns
/// are named errors (a broken rule silently skipping is a false-negative
/// factory).
pub fn run_scan(cwd: &Path, paths: &[String]) -> Result<Vec<Finding>> {
    let packs = load_rule_packs(cwd)?;
    let mut compiled: Vec<CompiledRule> = Vec::new();
    for pack in &packs {
        for rule in &pack.rules {
            compiled.push(CompiledRule::build(rule)?);
        }
    }
    let mut findings = Vec::new();
    for file in scope_files(cwd, paths)? {
        scan_file(cwd, &file, &compiled, &mut findings);
    }
    findings.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.rule_id.cmp(&b.rule_id))
    });
    Ok(findings)
}

/// SARIF level for our severity vocabulary.
fn sarif_level(severity: &str) -> &'static str {
    match severity {
        "error" => "error",
        "note" => "note",
        _ => "warning",
    }
}

/// Emit a SARIF v2.1.0 document for findings (post-disposition filtering
/// happens before this — suppressed findings carry `suppressions` instead).
pub fn to_sarif(
    findings: &[Finding],
    suppressed: &[Finding],
    packs: &[RulePack],
) -> serde_json::Value {
    let rules_meta: Vec<serde_json::Value> = packs
        .iter()
        .flat_map(|p| {
            p.rules.iter().map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "shortDescription": { "text": r.message },
                    "defaultConfiguration": { "level": sarif_level(&r.severity) },
                })
            })
        })
        .collect();
    let result_json = |f: &Finding, suppressions: serde_json::Value| {
        serde_json::json!({
            "ruleId": f.rule_id,
            "level": sarif_level(&f.severity),
            "message": { "text": f.message },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": f.path },
                    "region": { "startLine": f.line },
                }
            }],
            "fingerprints": { "pi/v1": f.fingerprint },
            "suppressions": suppressions,
        })
    };
    let mut results: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| result_json(f, serde_json::json!([])))
        .collect();
    results.extend(suppressed.iter().map(|f| {
        result_json(
            f,
            serde_json::json!([{ "kind": "external", "status": "accepted" }]),
        )
    }));
    serde_json::json!({
        "version": SARIF_VERSION,
        "$schema": SARIF_SCHEMA_URI,
        "runs": [{
            "tool": {
                "driver": {
                    "name": "pi security_scan",
                    "version": env!("CARGO_PKG_VERSION"),
                    "informationUri": "https://github.com/Dicklesworthstone/pi_agent_rust",
                    "rules": rules_meta,
                }
            },
            "results": results,
        }]
    })
}

/// Disposition store entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Disposition {
    pub status: String, // false-positive | accepted | fixed
    pub reason: String,
    pub at_ms: u64,
}

/// Resolve a tool-supplied path under `cwd`, refusing absolute paths and
/// `..` escapes. Without this, `sarifOut: "/Users/x/.zshenv"` would clobber
/// arbitrary files under a single generic tool approval.
fn confine_to_cwd(cwd: &Path, raw: &str, field: &str) -> Result<PathBuf> {
    let candidate = Path::new(raw);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Error::validation(format!(
            "security_scan {field} must be a relative path inside the project (got {raw:?})"
        )));
    }
    Ok(cwd.join(candidate))
}

fn dispositions_path(project_root: &Path) -> PathBuf {
    project_root.join(".pi").join("security-dispositions.json")
}

/// Missing store → empty map; unparseable store → error.
///
/// Silently treating a corrupt store as empty would un-suppress every
/// finding and, on the next `disposition`, overwrite it — destroying all
/// prior dispositions.
pub fn load_dispositions(project_root: &Path) -> Result<BTreeMap<String, Disposition>> {
    let path = dispositions_path(project_root);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => {
            return Err(Error::tool(
                "security_scan",
                format!("read {}: {e}", path.display()),
            ));
        }
    };
    serde_json::from_str(&text).map_err(|e| {
        Error::validation(format!(
            "corrupt disposition store {} ({e}); repair or move it aside before scanning",
            path.display()
        ))
    })
}

pub fn save_dispositions(
    project_root: &Path,
    dispositions: &BTreeMap<String, Disposition>,
) -> Result<()> {
    let path = dispositions_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            Error::tool("security_scan", format!("create {}: {e}", parent.display()))
        })?;
    }
    let text = serde_json::to_string_pretty(dispositions)
        .map_err(|e| Error::validation(format!("serialize dispositions: {e}")))?;
    // Temp + rename: a crash mid-write must not leave a truncated store.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)
        .map_err(|e| Error::tool("security_scan", format!("write {}: {e}", tmp.display())))?;
    fs::rename(&tmp, &path)
        .map_err(|e| Error::tool("security_scan", format!("persist {}: {e}", path.display())))
}

/// Partition findings into active vs dispositioned (status false-positive or
/// accepted suppresses; `fixed` keeps the finding visible as a regression
/// signal when it reappears).
pub fn partition_by_disposition(
    findings: Vec<Finding>,
    dispositions: &BTreeMap<String, Disposition>,
) -> (Vec<Finding>, Vec<Finding>) {
    let mut active = Vec::new();
    let mut suppressed = Vec::new();
    for finding in findings {
        match dispositions.get(&finding.fingerprint) {
            Some(d) if d.status == "false-positive" || d.status == "accepted" => {
                suppressed.push(finding);
            }
            _ => active.push(finding),
        }
    }
    (active, suppressed)
}

/// Compare two finding sets by fingerprint: new / fixed / regressed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareReport {
    pub new: Vec<Finding>,
    pub fixed: Vec<Finding>,
    /// Was dispositioned `fixed` but present again.
    pub regressed: Vec<Finding>,
}

pub fn compare(
    previous: &[Finding],
    current: &[Finding],
    dispositions: &BTreeMap<String, Disposition>,
) -> CompareReport {
    let prev: HashMap<&str, &Finding> = previous
        .iter()
        .map(|f| (f.fingerprint.as_str(), f))
        .collect();
    let cur: HashMap<&str, &Finding> = current
        .iter()
        .map(|f| (f.fingerprint.as_str(), f))
        .collect();
    let new = current
        .iter()
        .filter(|f| !prev.contains_key(f.fingerprint.as_str()))
        .cloned()
        .collect();
    let fixed = previous
        .iter()
        .filter(|f| !cur.contains_key(f.fingerprint.as_str()))
        .cloned()
        .collect();
    let regressed = current
        .iter()
        .filter(|f| {
            dispositions
                .get(&f.fingerprint)
                .is_some_and(|d| d.status == "fixed")
        })
        .cloned()
        .collect();
    CompareReport {
        new,
        fixed,
        regressed,
    }
}

/// Read a prior SARIF document back into findings (for `compare`).
pub fn findings_from_sarif(text: &str) -> Vec<Finding> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let Some(results) = doc.pointer("/runs/0/results").and_then(|r| r.as_array()) else {
        return out;
    };
    for result in results {
        let Some(rule_id) = result.get("ruleId").and_then(|v| v.as_str()) else {
            continue;
        };
        let path = result
            .pointer("/locations/0/physicalLocation/artifactLocation/uri")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let line = result
            .pointer("/locations/0/physicalLocation/region/startLine")
            .and_then(serde_json::Value::as_u64)
            .map_or(0, |v| u32::try_from(v).unwrap_or(u32::MAX));
        let message = result
            .pointer("/message/text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let fingerprint_value = result
            .pointer("/fingerprints/pi~1v1")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.push(Finding {
            rule_id: rule_id.to_string(),
            severity: result
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or("warning")
                .to_string(),
            message,
            path,
            line,
            fingerprint: fingerprint_value,
            fix_hint: None,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Tool surface
// ---------------------------------------------------------------------------

use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput};
use serde_json::{Value, json};

fn scan_text_output(text: String, details: Value) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: false,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScanInput {
    /// plan | run | disposition | compare
    op: String,
    /// Paths (files or dirs, relative to cwd) to scope; empty = project.
    #[serde(default)]
    paths: Vec<String>,
    /// run: SARIF output file (default `.pi/security-scan.sarif`).
    sarif_out: Option<String>,
    /// disposition: finding fingerprint to mark.
    fingerprint: Option<String>,
    /// disposition: false-positive | accepted | fixed.
    status: Option<String>,
    /// disposition: free-text reason.
    reason: Option<String>,
    /// compare: prior SARIF file (default `.pi/security-scan.sarif`).
    baseline: Option<String>,
}

/// Agent-facing security scanner (bd-cv653.2.6). Opt-in: enable with
/// `--tools ...,security_scan`. Fully offline-capable; no code leaves the
/// machine.
pub struct SecurityScanTool {
    cwd: PathBuf,
}

impl SecurityScanTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SecurityScanTool {
    fn name(&self) -> &'static str {
        "security_scan"
    }

    fn label(&self) -> &'static str {
        "security scan"
    }

    fn description(&self) -> &'static str {
        "Plan and run a native security review over the repo. Ops: `plan` \
         (scope files → applicable rule packs), `run` (deterministic regex \
         rule packs: secrets, injection sinks, unsafe deserialization, path \
         traversal → SARIF v2.1.0 written to file + summary), `disposition` \
         (mark a finding false-positive/accepted/fixed with a reason; \
         re-runs suppress false-positive/accepted, `fixed` reappearing is a \
         regression), `compare` (vs a prior SARIF: new/fixed/regressed). \
         Findings carry stable fingerprints (rule+path+matched text) that \
         survive line shifts. Dispositions persist per project under .pi/."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": {
                    "type": "string",
                    "enum": ["plan", "run", "disposition", "compare"],
                    "description": "plan: scope+checklist; run: scan to SARIF; disposition: mark a finding; compare: delta vs prior SARIF"
                },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Files/dirs to scope (relative to cwd); default: whole project"
                },
                "sarifOut": { "type": "string", "description": "run: SARIF output path (default .pi/security-scan.sarif)" },
                "fingerprint": { "type": "string", "description": "disposition: finding fingerprint" },
                "status": {
                    "type": "string",
                    "enum": ["false-positive", "accepted", "fixed"],
                    "description": "disposition: status"
                },
                "reason": { "type": "string", "description": "disposition: reason (required)" },
                "baseline": { "type": "string", "description": "compare: prior SARIF file (default .pi/security-scan.sarif)" }
            },
            "required": ["op"]
        })
    }

    fn effects(&self) -> ToolEffects {
        // run/disposition write SARIF + the disposition store under .pi/;
        // plan/compare are pure reads. The union keeps the write honest.
        ToolEffects::read().union(ToolEffects::write())
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: Value,
        _on_update: Option<Box<dyn Fn(crate::tools::ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let parsed: ScanInput = serde_json::from_value(input)
            .map_err(|e| Error::validation(format!("security_scan input: {e}")))?;
        let op = parsed.op.trim().to_ascii_lowercase();
        // plan/run/compare walk and read the whole scope synchronously —
        // off the async runtime so a large repo doesn't stall the agent
        // loop mid-scan.
        let tool = Self {
            cwd: self.cwd.clone(),
        };
        asupersync::runtime::spawn_blocking(move || match op.as_str() {
            "plan" => tool.op_plan(&parsed),
            "run" => tool.op_run(&parsed),
            "disposition" => tool.op_disposition(&parsed),
            "compare" => tool.op_compare(&parsed),
            other => Err(Error::validation(format!(
                "Unknown security_scan op '{other}'; expected plan, run, disposition, or compare"
            ))),
        })
        .await
    }
}

impl SecurityScanTool {
    fn op_plan(&self, input: &ScanInput) -> Result<ToolOutput> {
        let packs = load_rule_packs(&self.cwd)?;
        let files = scope_files(&self.cwd, &input.paths)?;
        let rule_count: usize = packs.iter().map(|p| p.rules.len()).sum();
        let details = json!({
            "schema": SCAN_SCHEMA,
            "packs": packs.iter().map(|p| json!({
                "name": p.name,
                "version": p.version,
                "rules": p.rules.iter().map(|r| json!({
                    "id": r.id,
                    "severity": r.severity,
                    "message": r.message,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "scopedFiles": files.len(),
        });
        let pack_lines: Vec<String> = packs
            .iter()
            .map(|p| format!("{} v{} ({} rules)", p.name, p.version, p.rules.len()))
            .collect();
        Ok(scan_text_output(
            format!(
                "Security review plan: {} file(s) in scope, {} rule(s) across {} pack(s):\n{}",
                files.len(),
                rule_count,
                packs.len(),
                pack_lines.join("\n")
            ),
            details,
        ))
    }

    fn op_run(&self, input: &ScanInput) -> Result<ToolOutput> {
        use std::fmt::Write as _;
        let packs = load_rule_packs(&self.cwd)?;
        let findings = run_scan(&self.cwd, &input.paths)?;
        let dispositions = load_dispositions(&self.cwd)?;
        let (active, suppressed) = partition_by_disposition(findings, &dispositions);
        let sarif = to_sarif(&active, &suppressed, &packs);
        let out_path = match input.sarif_out.as_deref() {
            None => self.cwd.join(".pi/security-scan.sarif"),
            Some(p) => confine_to_cwd(&self.cwd, p, "sarifOut")?,
        };
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                Error::tool("security_scan", format!("create {}: {e}", parent.display()))
            })?;
        }
        fs::write(
            &out_path,
            serde_json::to_string_pretty(&sarif)
                .map_err(|e| Error::validation(format!("serialize SARIF: {e}")))?,
        )
        .map_err(|e| {
            Error::tool(
                "security_scan",
                format!("write {}: {e}", out_path.display()),
            )
        })?;
        let by_severity = |sev: &str| active.iter().filter(|f| f.severity == sev).count();
        let details = json!({
            "schema": SCAN_SCHEMA,
            "sarif": out_path.display().to_string(),
            "active": active.len(),
            "suppressed": suppressed.len(),
            "errors": by_severity("error"),
            "warnings": by_severity("warning"),
            "notes": by_severity("note"),
            "findings": active,
        });
        let mut text = format!(
            "Security scan: {} active finding(s) ({} error, {} warning, {} note), {} suppressed. SARIF: {}",
            active.len(),
            by_severity("error"),
            by_severity("warning"),
            by_severity("note"),
            suppressed.len(),
            out_path.display()
        );
        for finding in active.iter().take(20) {
            let _ = write!(
                text,
                "\n[{}] {} {}:{} — {} ({})",
                finding.severity,
                finding.rule_id,
                finding.path,
                finding.line,
                finding.message,
                finding.fingerprint
            );
        }
        if active.len() > 20 {
            let _ = write!(text, "\n… {} more in the SARIF file.", active.len() - 20);
        }
        Ok(scan_text_output(text, details))
    }

    fn op_disposition(&self, input: &ScanInput) -> Result<ToolOutput> {
        let fingerprint = input
            .fingerprint
            .clone()
            .filter(|f| !f.trim().is_empty())
            .ok_or_else(|| Error::validation("security_scan disposition requires fingerprint"))?;
        let status = input
            .status
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| Error::validation("security_scan disposition requires status"))?;
        if !matches!(status.as_str(), "false-positive" | "accepted" | "fixed") {
            return Err(Error::validation(format!(
                "disposition status must be false-positive, accepted, or fixed (got '{status}')"
            )));
        }
        let reason = input
            .reason
            .clone()
            .filter(|r| !r.trim().is_empty())
            .ok_or_else(|| Error::validation("security_scan disposition requires reason"))?;
        let mut dispositions = load_dispositions(&self.cwd)?;
        dispositions.insert(
            fingerprint.clone(),
            Disposition {
                status: status.clone(),
                reason,
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            },
        );
        save_dispositions(&self.cwd, &dispositions)?;
        Ok(scan_text_output(
            format!("Disposition recorded: {fingerprint} → {status}."),
            json!({
                "schema": SCAN_SCHEMA,
                "fingerprint": fingerprint,
                "status": status,
                "dispositions": dispositions.len(),
            }),
        ))
    }

    fn op_compare(&self, input: &ScanInput) -> Result<ToolOutput> {
        let baseline_path = match input.baseline.as_deref() {
            None => self.cwd.join(".pi/security-scan.sarif"),
            Some(p) => confine_to_cwd(&self.cwd, p, "baseline")?,
        };
        let text = fs::read_to_string(&baseline_path).map_err(|e| {
            Error::tool(
                "security_scan",
                format!("read baseline {}: {e}", baseline_path.display()),
            )
        })?;
        let mut previous = findings_from_sarif(&text);
        // Scope-narrowed compare: baseline findings outside the requested
        // paths are invisible to the current scan and would all read as
        // "fixed". Restrict the baseline to the same scope.
        if !input.paths.is_empty() {
            let scope: Vec<String> = input
                .paths
                .iter()
                .map(|p| p.trim_end_matches('/').to_string())
                .collect();
            previous.retain(|finding| {
                scope.iter().any(|prefix| {
                    finding.path == *prefix || finding.path.starts_with(&format!("{prefix}/"))
                })
            });
        }
        let current = run_scan(&self.cwd, &input.paths)?;
        let dispositions = load_dispositions(&self.cwd)?;
        let report = compare(&previous, &current, &dispositions);
        let details = serde_json::to_value(&report)
            .map_err(|e| Error::validation(format!("serialize compare: {e}")))?;
        Ok(scan_text_output(
            format!(
                "Compare vs {}: {} new, {} fixed, {} regressed.",
                baseline_path.display(),
                report.new.len(),
                report.fixed.len(),
                report.regressed.len()
            ),
            json!({ "schema": SCAN_SCHEMA, "report": details }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packs() -> Vec<RulePack> {
        load_rule_packs(Path::new("/nonexistent-root")).expect("default pack parses")
    }

    #[test]
    fn default_pack_parses_with_unique_rule_ids() {
        let packs = packs();
        assert_eq!(packs[0].name, "pi-default");
        assert!(packs[0].rules.len() >= 6);
        let mut ids: Vec<&str> = packs[0].rules.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate rule ids in default pack");
        for rule in &packs[0].rules {
            regex::Regex::new(&rule.pattern)
                .unwrap_or_else(|e| panic!("rule {} pattern invalid: {e}", rule.id));
            assert!(matches!(
                rule.severity.as_str(),
                "error" | "warning" | "note"
            ));
        }
    }

    #[test]
    fn fingerprint_is_stable_and_line_independent() {
        let a = fingerprint("rule.x", "src/a.rs", "matched", 0);
        let b = fingerprint("rule.x", "src/a.rs", "matched", 0);
        let c = fingerprint("rule.x", "src/b.rs", "matched", 0);
        // Same match text, different occurrence → distinct fingerprint (a
        // disposition on one private key must not suppress a second one).
        let d = fingerprint("rule.x", "src/a.rs", "matched", 1);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn disposition_partition_suppresses_false_positive_only() {
        let finding = Finding {
            rule_id: "r".into(),
            severity: "error".into(),
            message: "m".into(),
            path: "p".into(),
            line: 1,
            fingerprint: "fp1".into(),
            fix_hint: None,
        };
        let mut dispositions = BTreeMap::new();
        dispositions.insert(
            "fp1".to_string(),
            Disposition {
                status: "false-positive".into(),
                reason: "test fixture".into(),
                at_ms: 1,
            },
        );
        let (active, suppressed) = partition_by_disposition(vec![finding.clone()], &dispositions);
        assert!(active.is_empty());
        assert_eq!(suppressed.len(), 1);
        // `fixed` does NOT suppress — the finding stays visible (regression).
        dispositions.get_mut("fp1").expect("entry").status = "fixed".into();
        let (active, suppressed) = partition_by_disposition(vec![finding], &dispositions);
        assert_eq!(active.len(), 1);
        assert!(suppressed.is_empty());
    }

    #[test]
    fn compare_reports_new_fixed_regressed() {
        let mk = |fp: &str| Finding {
            rule_id: "r".into(),
            severity: "warning".into(),
            message: "m".into(),
            path: "p".into(),
            line: 1,
            fingerprint: fp.to_string(),
            fix_hint: None,
        };
        let previous = vec![mk("a"), mk("b")];
        let current = vec![mk("b"), mk("c")];
        let mut dispositions = BTreeMap::new();
        dispositions.insert(
            "b".to_string(),
            Disposition {
                status: "fixed".into(),
                reason: "patched".into(),
                at_ms: 1,
            },
        );
        let report = compare(&previous, &current, &dispositions);
        assert_eq!(report.new.len(), 1);
        assert_eq!(report.new[0].fingerprint, "c");
        assert_eq!(report.fixed.len(), 1);
        assert_eq!(report.fixed[0].fingerprint, "a");
        assert_eq!(report.regressed.len(), 1);
        assert_eq!(report.regressed[0].fingerprint, "b");
    }

    #[test]
    fn sarif_round_trip_through_findings_from_sarif() {
        let packs = packs();
        let finding = Finding {
            rule_id: "secret.aws-access-key".into(),
            severity: "error".into(),
            message: "AWS access key id literal".into(),
            path: "src/main.rs".into(),
            line: 42,
            fingerprint: fingerprint(
                "secret.aws-access-key",
                "src/main.rs",
                concat!("AKIA", "IOSFODNN7EXAMPLE"),
                0,
            ),
            fix_hint: None,
        };
        let sarif = to_sarif(std::slice::from_ref(&finding), &[], &packs);
        assert_eq!(sarif["version"].as_str(), Some(SARIF_VERSION));
        let text = serde_json::to_string(&sarif).expect("serialize");
        let back = findings_from_sarif(&text);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].rule_id, finding.rule_id);
        assert_eq!(back[0].fingerprint, finding.fingerprint);
        assert_eq!(back[0].line, 42);
    }

    #[test]
    fn scan_finds_seeded_secret_and_aws_key() {
        let temp = std::env::temp_dir().join(format!("pi-secscan-test-{}", std::process::id()));
        fs::create_dir_all(&temp).expect("mkdir");
        fs::write(
            temp.join("leak.py"),
            concat!(
                "AWS_KEY = \"AKIA",
                "IOSFODNN7EXAMPLE\"\nimport pickle\npickle.loads(data)\n"
            ),
        )
        .expect("write fixture");
        let findings = run_scan(&temp, &[]).expect("scan");
        let ids: Vec<&str> = findings.iter().map(|f| f.rule_id.as_str()).collect();
        assert!(
            ids.contains(&"secret.aws-access-key"),
            "missing aws rule hit: {ids:?}"
        );
        assert!(
            ids.contains(&"deserialization.unsafe-pickle"),
            "missing pickle rule hit: {ids:?}"
        );
        let _ = fs::remove_dir_all(&temp);
    }
}
