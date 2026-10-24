//! Agent-facing security scanner (bd-cv653.2.6): plan/run/disposition/compare
//! over deterministic rule packs, emitting SARIF v2.1.0.
//!
//! v1 is fully native: regex rule packs (versioned in-tree, project
//! overrides under `.pi/security-rules/`), a per-project disposition store
//! (`.pi/security-dispositions.json`). These source operations are offline;
//! the separate dependency engine owns explicit network lookups.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

mod operation;
mod storage;

const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SCAN_BYTES: usize = 256 * 1024 * 1024;
const MAX_SCOPE_FILES: usize = 20_000;
const MAX_FINDINGS: usize = 10_000;

fn scan_error(message: impl Into<String>) -> Error {
    Error::tool(
        "security_scan",
        format!("[SOURCE_SCAN_INCOMPLETE] {}", message.into()),
    )
}

// A failed read is not a binary-file classification. Check regular-file type
// and cap the read itself, not just a racy metadata length. NONBLOCK prevents
// a Unix FIFO substituted after traversal from hanging a blocking worker.
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read as _;
    operation::checkpoint()?;
    let metadata =
        fs::metadata(path).map_err(|_| scan_error(format!("cannot inspect {}", path.display())))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(scan_error(format!(
            "{} must be a regular file of at most {limit} bytes",
            path.display()
        )));
    }
    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    let file = {
        use rustix::fs::{Mode, OFlags};
        fs::File::from(
            rustix::fs::open(
                path,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(|_| scan_error(format!("cannot open {}", path.display())))?,
        )
    };
    #[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
    let file =
        fs::File::open(path).map_err(|_| scan_error(format!("cannot open {}", path.display())))?;
    let metadata = file
        .metadata()
        .map_err(|_| scan_error(format!("cannot inspect opened {}", path.display())))?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(scan_error(format!(
            "{} changed type or exceeds its byte limit",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    let mut reader = file.take(limit + 1);
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        operation::checkpoint()?;
        let count = reader
            .read(&mut chunk)
            .map_err(|_| scan_error(format!("cannot read {}", path.display())))?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    operation::checkpoint()?;
    if bytes.len() as u64 > limit {
        return Err(scan_error(format!(
            "{} grew beyond its byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

pub const SCAN_SCHEMA: &str = "pi.security-scan/v1";
/// SARIF version emitted by `run`.
pub const SARIF_VERSION: &str = "2.1.0";
pub const SARIF_SCHEMA_URI: &str = "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/main/sarif-2.1/schema/sarif-schema-2.1.0.json";

/// One detection rule.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub id: String,
    /// Regex over the complete file, with line-aware ^/$ and CRLF handling.
    /// Explicit newlines, whitespace classes and inline flags retain their
    /// regex meaning; no textual flag heuristic selects a different engine.
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
    operation::checkpoint()?;
    let mut packs: Vec<RulePack> = vec![toml::from_str(DEFAULT_PACK).map_err(|e| {
        Error::validation(format!("embedded security rule pack failed to parse: {e}"))
    })?];
    let overrides = project_root.join(".pi").join("security-rules");
    let has_overrides = match fs::symlink_metadata(&overrides) {
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_) => return Err(scan_error("project security-rules path is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => {
            return Err(scan_error(
                "cannot inspect project security-rules directory",
            ));
        }
    };
    if has_overrides {
        let reader = fs::read_dir(&overrides).map_err(|e| {
            Error::tool(
                "security_scan",
                format!("read {}: {e}", overrides.display()),
            )
        })?;
        let mut entries = Vec::new();
        for entry in reader {
            operation::checkpoint()?;
            let path = entry
                .map_err(|_| scan_error("cannot enumerate every project security rule pack"))?
                .path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                if entries.len() == 64 {
                    return Err(scan_error(
                        "project rule packs exceed 64; narrow the rule inventory",
                    ));
                }
                entries.push(path);
            }
        }
        entries.sort();
        for path in entries {
            operation::checkpoint()?;
            let text = String::from_utf8(read_bounded(&path, 1024 * 1024)?)
                .map_err(|_| scan_error(format!("rule pack {} is not UTF-8", path.display())))?;
            let pack: RulePack = toml::from_str(&text)
                .map_err(|_| scan_error(format!("rule pack {} is malformed", path.display())))?;
            packs.push(pack);
        }
    }
    Ok(packs)
}

/// Scope a scan: explicit paths, or the project root. Directories walk
/// gitignore-respecting file lists via the repo's ignore machinery.
fn scope_files(cwd: &Path, paths: &[String]) -> Result<Vec<PathBuf>> {
    operation::checkpoint()?;
    if paths.len() > 256 {
        return Err(scan_error("at most 256 scan roots may be selected"));
    }
    let mut files = BTreeSet::new();
    let roots: Vec<PathBuf> = if paths.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        paths
            .iter()
            .map(|p| confine_to_cwd(cwd, p, "paths"))
            .collect::<Result<Vec<_>>>()?
    };
    for root in roots {
        operation::checkpoint()?;
        // A mistyped scope must not yield a false-clean "0 findings".
        let metadata = fs::metadata(&root).map_err(|_| {
            scan_error(format!(
                "scope path does not exist or cannot be inspected: {}",
                root.display()
            ))
        })?;
        if metadata.is_file() {
            files.insert(root);
        } else if metadata.is_dir() {
            // `.hidden(false)` so dotfiles are scanned: `.env`, `.npmrc`,
            // `.aws/credentials` and CI workflow files are the classic
            // secret carriers. `.git/` itself is still skipped below.
            let walker = ignore::WalkBuilder::new(&root)
                .hidden(false)
                .git_ignore(true)
                .filter_entry(|entry| entry.file_name() != ".git")
                .build();
            for entry in walker {
                operation::checkpoint()?;
                let entry = entry.map_err(|_| {
                    scan_error(format!(
                        "cannot completely traverse scope {}",
                        root.display()
                    ))
                })?;
                // Walk errors can accompany an otherwise usable entry (for
                // example an invalid ignore file). Such a scope is not proven.
                if entry.error().is_some() {
                    return Err(scan_error(format!(
                        "invalid ignore rules while traversing {}",
                        root.display()
                    )));
                }
                let path = entry.path();
                let metadata = fs::metadata(path).map_err(|_| {
                    scan_error(format!("cannot inspect discovered {}", path.display()))
                })?;
                if metadata.is_file() {
                    files.insert(path.to_path_buf());
                    if files.len() > MAX_SCOPE_FILES {
                        return Err(scan_error("scan exceeds 20000 files; narrow paths"));
                    }
                } else if !metadata.is_dir() {
                    return Err(scan_error(format!(
                        "unsupported non-regular scan entry {}",
                        path.display()
                    )));
                }
            }
        } else {
            return Err(scan_error(format!(
                "scope {} is not a file or directory",
                root.display()
            )));
        }
        if files.len() > MAX_SCOPE_FILES {
            return Err(scan_error("scan exceeds 20000 files; narrow paths"));
        }
    }
    Ok(files.into_iter().collect())
}

/// A rule compiled for scanning: whole-file regex and optional glob scope.
struct CompiledRule {
    rule: Rule,
    regex: regex::Regex,
    applies_to: Option<globset::GlobSet>,
}

impl CompiledRule {
    fn build(rule: &Rule) -> Result<Self> {
        let regex = regex::RegexBuilder::new(&rule.pattern)
            .multi_line(true)
            .crlf(true)
            .build()
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
        Ok(Self {
            rule: rule.clone(),
            regex,
            applies_to,
        })
    }

    fn applies_to_path(&self, path: &Path) -> bool {
        self.applies_to.as_ref().is_none_or(|globs| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            globs.is_match(path) || globs.is_match(name)
        })
    }
}

// Component normalization keeps ./src, src/ and src equivalent without
// admitting parent traversal. Use one platform-independent separator in
// finding identities, so scan and compare do not disagree about the scope.
fn relative_key(path: &Path) -> Result<String> {
    use std::path::Component;
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| scan_error("source path is not UTF-8"))?;
                if part.chars().any(char::is_control) {
                    return Err(scan_error("source path contains control characters"));
                }
                parts.push(part);
            }
            _ => return Err(scan_error("source path must stay relative to the project")),
        }
    }
    Ok(parts.join("/"))
}

/// Run compiled rules over one file, returning findings with paths relative
/// to `cwd`. The caller has already completed a bounded UTF-8 read.
fn scan_file(
    cwd: &Path,
    path: &Path,
    content: &str,
    rules: &[CompiledRule],
    findings: &mut Vec<Finding>,
) -> Result<()> {
    let rel = path
        .strip_prefix(cwd)
        .map_or_else(|_| path.to_path_buf(), Path::to_path_buf);
    let rel_display = relative_key(&rel)?;
    for compiled in rules {
        operation::checkpoint()?;
        if !compiled.applies_to_path(Path::new(&rel_display)) {
            continue;
        }
        // Borrow match text from this file and discard the counters after
        // each rule. Long matches never create a second retained payload.
        let mut occurrences: BTreeMap<&str, usize> = BTreeMap::new();
        let mut cursor = 0;
        let mut line_no = 1_usize;
        for matched in compiled.regex.find_iter(content) {
            operation::checkpoint()?;
            if findings.len() >= MAX_FINDINGS {
                return Err(scan_error("scan exceeds 10000 findings; narrow paths"));
            }
            // Ordered match offsets let us account for each prefix byte
            // once per rule instead of rescanning from byte zero per hit.
            for index in cursor..matched.start() {
                let byte = content.as_bytes()[index];
                if byte == b'\n'
                    || byte == b'\r' && content.as_bytes().get(index + 1) != Some(&b'\n')
                {
                    line_no += 1;
                }
            }
            cursor = matched.start();
            let counter = occurrences.entry(matched.as_str()).or_insert(0);
            let occurrence = *counter;
            *counter += 1;
            let rule = &compiled.rule;
            findings.push(Finding {
                rule_id: rule.id.clone(),
                severity: rule.severity.clone(),
                message: rule.message.clone(),
                path: rel_display.clone(),
                line: u32::try_from(line_no).unwrap_or(u32::MAX),
                fingerprint: fingerprint(&rule.id, &rel_display, matched.as_str(), occurrence),
                fix_hint: rule.fix_hint.clone(),
            });
        }
    }
    Ok(())
}

struct ScanOutcome {
    findings: Vec<Finding>,
    scanned: BTreeSet<String>,
    skipped_non_utf8: Vec<String>,
    bytes_read: usize,
}

/// Compile + run the packs over the scope. Rules with un compilable patterns
/// are named errors (a broken rule silently skipping is a false-negative
/// factory).
pub fn run_scan(cwd: &Path, paths: &[String]) -> Result<Vec<Finding>> {
    let packs = load_rule_packs(cwd)?;
    Ok(scan_with_packs(cwd, paths, &packs)?.findings)
}

fn compile_rules(packs: &[RulePack]) -> Result<Vec<CompiledRule>> {
    let mut compiled: Vec<CompiledRule> = Vec::new();
    let mut ids = BTreeSet::new();
    for pack in packs {
        for rule in &pack.rules {
            operation::checkpoint()?;
            if compiled.len() >= 1024 || rule.id.trim().is_empty() || !ids.insert(&rule.id) {
                return Err(scan_error(
                    "rule inventory exceeds 1024 or contains empty/duplicate IDs",
                ));
            }
            compiled.push(CompiledRule::build(rule)?);
        }
    }
    Ok(compiled)
}

// The same loaded packs drive detection and SARIF metadata. Loading twice
// could attribute findings to different rules after a concurrent pack change.
fn scan_with_packs(cwd: &Path, paths: &[String], packs: &[RulePack]) -> Result<ScanOutcome> {
    let compiled = compile_rules(packs)?;
    let mut outcome = ScanOutcome {
        findings: Vec::new(),
        scanned: BTreeSet::new(),
        skipped_non_utf8: Vec::new(),
        bytes_read: 0,
    };
    for file in scope_files(cwd, paths)? {
        operation::checkpoint()?;
        let bytes = read_bounded(&file, MAX_SOURCE_BYTES)?;
        outcome.bytes_read = outcome
            .bytes_read
            .checked_add(bytes.len())
            .filter(|total| *total <= MAX_SCAN_BYTES)
            .ok_or_else(|| scan_error("scan exceeds 256 MiB; narrow paths"))?;
        let relative = relative_key(file.strip_prefix(cwd).unwrap_or(&file))?;
        let Ok(content) = std::str::from_utf8(&bytes) else {
            outcome.skipped_non_utf8.push(relative);
            continue;
        };
        scan_file(cwd, &file, content, &compiled, &mut outcome.findings)?;
        outcome.scanned.insert(relative);
    }
    outcome.findings.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then(a.line.cmp(&b.line))
            .then(a.rule_id.cmp(&b.rule_id))
    });
    Ok(outcome)
}

/// SARIF level for our severity vocabulary.
fn sarif_level(severity: &str) -> &'static str {
    match severity {
        "error" => "error",
        "note" => "note",
        _ => "warning",
    }
}

fn rule_digest(rule: &Rule) -> String {
    use sha2::Digest as _;
    // Bind executable matching semantics, not just a user-reused rule ID.
    // Serialize a tuple so map ordering cannot change the commitment.
    let bytes = serde_json::to_vec(&(
        "pi-source-whole-file-regex/v1",
        &rule.id,
        &rule.pattern,
        &rule.severity,
        &rule.message,
        &rule.fix_hint,
        &rule.applies_to,
    ))
    .expect("rule fields are strings and string lists");
    crate::package_manager::hex_encode(&sha2::Sha256::digest(bytes))
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
                    "properties": { "piSourceRuleDigest": rule_digest(r) },
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

/// Missing store → empty map; unparseable store → error.
///
/// Silently treating a corrupt store as empty would un-suppress every
/// finding and, on the next `disposition`, overwrite it — destroying all
/// prior dispositions.
pub fn load_dispositions(project_root: &Path) -> Result<BTreeMap<String, Disposition>> {
    storage::Storage::open(project_root)?.load()
}

/// Atomically replace a complete validated snapshot. For concurrent individual
/// decisions use the tool's `disposition` operation, which holds a lock across
/// read/merge/publication. This SDK snapshot API is intentionally replacement.
pub fn save_dispositions(
    project_root: &Path,
    dispositions: &BTreeMap<String, Disposition>,
) -> Result<()> {
    storage::Storage::open(project_root)?.save(dispositions)
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

// Typed native fields reject duplicate JSON keys before they can collapse to
// a different identity. Unrelated SARIF metadata is deliberately not projected
// into findings or error diagnostics.
#[derive(Deserialize)]
struct SourceDocument {
    version: String,
    runs: Vec<SourceRun>,
}
#[derive(Deserialize)]
struct SourceRun {
    tool: SourceTool,
    results: Vec<SourceResult>,
    #[serde(default)]
    invocations: Vec<SourceInvocation>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceInvocation {
    execution_successful: bool,
}
#[derive(Deserialize)]
struct SourceTool {
    driver: SourceDriver,
}
#[derive(Deserialize)]
struct SourceDriver {
    name: String,
    rules: Vec<SourceRule>,
}
#[derive(Deserialize)]
struct SourceRule {
    id: String,
    properties: Option<SourceRuleProperties>,
}
#[derive(Deserialize)]
struct SourceRuleProperties {
    #[serde(rename = "piSourceRuleDigest")]
    digest: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceResult {
    rule_id: String,
    level: String,
    message: SourceMessage,
    locations: Vec<SourceLocation>,
    fingerprints: SourceFingerprints,
}
#[derive(Deserialize)]
struct SourceMessage {
    text: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceLocation {
    physical_location: SourcePhysicalLocation,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourcePhysicalLocation {
    artifact_location: SourceArtifactLocation,
    region: SourceRegion,
}
#[derive(Deserialize)]
struct SourceArtifactLocation {
    uri: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceRegion {
    start_line: u32,
}
#[derive(Deserialize)]
struct SourceFingerprints {
    #[serde(rename = "pi/v1")]
    identity: String,
}

struct SourceBaseline {
    findings: Vec<Finding>,
    rules: BTreeMap<String, Option<String>>,
}

fn invalid_baseline() -> Error {
    Error::validation(
        "[SOURCE_BASELINE_INVALID] expected one bounded native source SARIF run with valid, unique finding identities; regenerate the baseline instead of treating it as empty",
    )
}

fn lowercase_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn source_baseline(text: &str) -> Result<SourceBaseline> {
    if text.len() as u64 > MAX_SOURCE_BYTES {
        return Err(invalid_baseline());
    }
    let doc: SourceDocument = serde_json::from_str(text).map_err(|_| invalid_baseline())?;
    if doc.version != SARIF_VERSION || doc.runs.len() != 1 {
        return Err(invalid_baseline());
    }
    let run = doc.runs.into_iter().next().ok_or_else(invalid_baseline)?;
    if run.tool.driver.name != "pi security_scan"
        || run.results.len() > MAX_FINDINGS
        || run.tool.driver.rules.len() > 1024
        || run
            .invocations
            .iter()
            .any(|invocation| !invocation.execution_successful)
    {
        return Err(invalid_baseline());
    }
    let mut rules = BTreeMap::new();
    for rule in run.tool.driver.rules {
        let digest = rule.properties.and_then(|properties| properties.digest);
        if rule.id.trim().is_empty()
            || digest
                .as_ref()
                .is_some_and(|digest| !lowercase_hex(digest, 64))
            || rules.insert(rule.id, digest).is_some()
        {
            return Err(invalid_baseline());
        }
    }
    let mut identities = BTreeSet::new();
    let mut findings = Vec::new();
    for result in run.results {
        if !rules.contains_key(&result.rule_id)
            || result.locations.len() != 1
            || !matches!(result.level.as_str(), "error" | "warning" | "note")
            || !lowercase_hex(&result.fingerprints.identity, 16)
            || !identities.insert(result.fingerprints.identity.clone())
        {
            return Err(invalid_baseline());
        }
        let location = result
            .locations
            .into_iter()
            .next()
            .ok_or_else(invalid_baseline)?
            .physical_location;
        let path = relative_key(Path::new(&location.artifact_location.uri))
            .map_err(|_| invalid_baseline())?;
        if path.is_empty() || path.len() > 4096 || location.region.start_line == 0 {
            return Err(invalid_baseline());
        }
        findings.push(Finding {
            rule_id: result.rule_id,
            severity: result.level,
            message: result.message.text,
            path,
            line: location.region.start_line,
            fingerprint: result.fingerprints.identity,
            fix_hint: None,
        });
    }
    Ok(SourceBaseline { findings, rules })
}

/// Read a native source SARIF document. Malformed, partial, multi-run or
/// dependency reports are errors, never an empty successful finding set.
pub fn findings_from_sarif(text: &str) -> Result<Vec<Finding>> {
    Ok(source_baseline(text)?.findings)
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
                "sarifOut": { "type": "string", "description": "run: NEW workspace-relative .sarif export, never overwritten; omit to atomically replace .pi/security-scan.sarif" },
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
        operation::run(move || match op.as_str() {
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
        compile_rules(&packs)?;
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
        // Pin the publication workspace before scanning or other long work.
        let storage = storage::Storage::open(&self.cwd)?;
        let relative = storage::report_path(
            input
                .sarif_out
                .as_deref()
                .unwrap_or(storage::DEFAULT_REPORT),
        )?;
        let packs = load_rule_packs(&self.cwd)?;
        let outcome = scan_with_packs(&self.cwd, &input.paths, &packs)?;
        let dispositions = storage.load()?;
        let (active, suppressed) = partition_by_disposition(outcome.findings, &dispositions);
        let mut sarif = to_sarif(&active, &suppressed, &packs);
        sarif["runs"][0]["invocations"] = json!([{ "executionSuccessful": true }]);
        sarif["runs"][0]["properties"] = json!({
            "scannedTextFiles": outcome.scanned.len(),
            "skippedNonUtf8Files": outcome.skipped_non_utf8,
            "bytesRead": outcome.bytes_read,
        });
        let out_path = self.cwd.join(relative);
        let encoded = serde_json::to_vec_pretty(&sarif)
            .map_err(|_| scan_error("source report could not be serialized"))?;
        if encoded.len() as u64 > MAX_SOURCE_BYTES {
            return Err(scan_error("source report exceeds 16 MiB; narrow paths"));
        }
        storage.write_report(input.sarif_out.as_deref(), &encoded)?;
        let by_severity = |sev: &str| active.iter().filter(|f| f.severity == sev).count();
        let details = json!({
            "schema": SCAN_SCHEMA,
            "sarif": out_path.display().to_string(),
            "active": active.len(),
            "suppressed": suppressed.len(),
            "errors": by_severity("error"),
            "warnings": by_severity("warning"),
            "notes": by_severity("note"),
            "findings": active.iter().take(50).collect::<Vec<_>>(),
            "findingsTruncated": active.len() > 50,
            "scannedTextFiles": outcome.scanned.len(),
            "skippedNonUtf8Files": outcome.skipped_non_utf8.len(),
            "bytesRead": outcome.bytes_read,
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
        let _ = write!(
            text,
            "\nScanned {} UTF-8 file(s); {} non-UTF-8 file(s) were not assessed (listed in SARIF).",
            outcome.scanned.len(),
            outcome.skipped_non_utf8.len()
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
        let count = storage::Storage::open(&self.cwd)?.update(
            fingerprint.clone(),
            Disposition {
                status: status.clone(),
                reason,
                at_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            },
        )?;
        Ok(scan_text_output(
            format!("Disposition recorded: {fingerprint} → {status}."),
            json!({
                "schema": SCAN_SCHEMA,
                "fingerprint": fingerprint,
                "status": status,
                "dispositions": count,
            }),
        ))
    }

    fn op_compare(&self, input: &ScanInput) -> Result<ToolOutput> {
        let storage = storage::Storage::open(&self.cwd)?;
        let selected = input.baseline.as_deref().unwrap_or(storage::DEFAULT_REPORT);
        let baseline_path = self.cwd.join(storage::report_path(selected)?);
        let text =
            String::from_utf8(storage.read_report(selected)?).map_err(|_| invalid_baseline())?;
        let baseline = source_baseline(&text)?;
        let mut previous = baseline.findings;
        // Scope-narrowed compare: baseline findings outside the requested
        // paths are invisible to the current scan and would all read as
        // "fixed". Restrict the baseline to the same scope.
        if !input.paths.is_empty() {
            let scope: Vec<String> = input
                .paths
                .iter()
                .map(|p| relative_key(Path::new(p)))
                .collect::<Result<_>>()?;
            previous.retain(|finding| {
                scope.iter().any(|prefix| {
                    prefix.is_empty()
                        || finding.path == *prefix
                        || finding.path.starts_with(&format!("{prefix}/"))
                })
            });
        }
        let packs = load_rule_packs(&self.cwd)?;
        let current = scan_with_packs(&self.cwd, &input.paths, &packs)?;
        let dispositions = storage.load()?;
        let mut report = compare(&previous, &current.findings, &dispositions);
        let current_rules: BTreeMap<_, _> = compile_rules(&packs)?
            .into_iter()
            .map(|rule| {
                let digest = rule_digest(&rule.rule);
                (rule.rule.id.clone(), (rule, digest))
            })
            .collect();
        // Absence is meaningful only when we actually inspected this file
        // with the same rule. Encoding changes, ignore changes, removed files,
        // removed/glob-narrowed rules and legacy unbound rules are unassessed,
        // not evidence of a fix. Persistent matches remain visible either way.
        let (fixed, unassessed): (Vec<_>, Vec<_>) = report.fixed.into_iter().partition(|finding| {
            current.scanned.contains(&finding.path)
                && current_rules
                    .get(&finding.rule_id)
                    .is_some_and(|(rule, digest)| {
                        rule.applies_to_path(Path::new(&finding.path))
                            && baseline
                                .rules
                                .get(&finding.rule_id)
                                .and_then(Option::as_ref)
                                == Some(digest)
                    })
        });
        report.fixed = fixed;
        // Computed outside `json!`: inside the macro a leading `[...]` is parsed
        // as a JSON array literal, so a method call on it does not compile.
        let truncated = [
            report.new.len(),
            report.fixed.len(),
            report.regressed.len(),
            unassessed.len(),
        ]
        .into_iter()
        .any(|count| count > 50);
        let details = json!({
            "new": report.new.iter().take(50).collect::<Vec<_>>(),
            "fixed": report.fixed.iter().take(50).collect::<Vec<_>>(),
            "regressed": report.regressed.iter().take(50).collect::<Vec<_>>(),
            "unassessed": unassessed.iter().take(50).collect::<Vec<_>>(),
            "counts": { "new": report.new.len(), "fixed": report.fixed.len(),
                "regressed": report.regressed.len(), "unassessed": unassessed.len() },
            "truncated": truncated,
        });
        let mut output = scan_text_output(
            format!(
                "Compare vs {}: {} new, {} fixed, {} regressed, {} unassessed. Fixed means no longer matched in a successfully read file under the unchanged rule, not proof of exploitability or remediation.",
                baseline_path.display(),
                report.new.len(),
                report.fixed.len(),
                report.regressed.len(),
                unassessed.len(),
            ),
            json!({ "schema": SCAN_SCHEMA, "comparisonComplete": unassessed.is_empty(), "report": details }),
        );
        output.is_error = !unassessed.is_empty();
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_input(value: Value) -> ScanInput {
        serde_json::from_value(value).unwrap()
    }

    fn pattern_rule(pattern: &str) -> CompiledRule {
        CompiledRule::build(&Rule {
            id: "test.pattern".into(),
            pattern: pattern.into(),
            severity: "warning".into(),
            message: "test match".into(),
            fix_hint: None,
            applies_to: vec![],
        })
        .unwrap()
    }

    fn pattern_findings(pattern: &str, content: &str) -> Vec<Finding> {
        let mut findings = Vec::new();
        scan_file(
            Path::new("."),
            Path::new("./code.rs"),
            content,
            &[pattern_rule(pattern)],
            &mut findings,
        )
        .unwrap();
        findings
    }

    #[test]
    fn default_rules_detect_multiline_formatted_calls_and_secret_assignments() {
        let dir = tempfile::tempdir().unwrap();
        let secret = "a".repeat(32);
        let content = format!(
            concat!(
                "// 🦀 heading\r\n",
                "api_key =\r\n",
                "\"{secret}\";\r\n",
                "execute(\r\n",
                "format!(\"SELECT {{}}\", request));\r\n",
                "Command::new(\r\n",
                "format!(\"{{}}\", request));\r\n",
                "path.join(\r\n",
                "request);\r\n",
            ),
            secret = secret
        );
        fs::write(dir.path().join("code.rs"), content).unwrap();
        let findings = run_scan(dir.path(), &["code.rs".into()]).unwrap();
        for (id, line) in [
            ("secret.generic-api-key", 2),
            ("injection.sql-format", 4),
            ("injection.shell-format", 6),
            ("traversal.join-unsanitized", 8),
        ] {
            assert!(
                findings
                    .iter()
                    .any(|finding| finding.rule_id == id && finding.line == line),
                "missing {id} at {line}: {findings:?}"
            );
        }
    }

    #[test]
    fn newline_classes_and_equivalent_flag_spellings_use_one_matching_engine() {
        for pattern in [
            r"alpha\nbeta",
            r"alpha\s+beta",
            r"alpha[^Z]+beta",
            r"(?is:alpha.*beta)",
            r"(?si:alpha.*beta)",
        ] {
            let findings = pattern_findings(pattern, "heading\nalpha\nbeta\ntrailer");
            assert_eq!(findings.len(), 1, "{pattern}");
            assert_eq!(findings[0].line, 2, "{pattern}");
        }
        assert!(pattern_findings(r"\Aalpha", "heading\nalpha").is_empty());
        assert!(pattern_findings(r"(?-m:^alpha)", "heading\nalpha").is_empty());
        assert!(pattern_findings(r"beta\z", "beta\ntrailer").is_empty());
        assert_eq!(
            pattern_findings(r"^alpha$", "heading\nalpha\ntrailer").len(),
            1
        );
    }

    #[test]
    fn line_mapping_and_fingerprints_are_stable_across_unicode_and_line_endings() {
        for separator in ["\n", "\r\n", "\r"] {
            let content = ["🦀 heading", "hit", "hit", "tail"].join(separator);
            let findings = pattern_findings(r"^hit$", &content);
            assert_eq!(
                findings
                    .iter()
                    .map(|finding| finding.line)
                    .collect::<Vec<_>>(),
                [2, 3]
            );
            for (occurrence, finding) in findings.iter().enumerate() {
                assert_eq!(finding.path, "code.rs");
                assert_eq!(
                    finding.fingerprint,
                    fingerprint("test.pattern", "code.rs", "hit", occurrence)
                );
            }
        }
        let many = pattern_findings("hit", &"🦀 hit\r\n".repeat(1000));
        assert_eq!(many.len(), 1000);
        assert_eq!(many[999].line, 1000);
        assert_eq!(
            many[999].fingerprint,
            fingerprint("test.pattern", "code.rs", "hit", 999)
        );
    }

    fn native_baseline() -> Value {
        let packs = packs();
        let finding = Finding {
            rule_id: "deserialization.unsafe-pickle".into(),
            severity: "warning".into(),
            message: "review".into(),
            path: "src/code.py".into(),
            line: 1,
            fingerprint: fingerprint(
                "deserialization.unsafe-pickle",
                "src/code.py",
                "pickle.loads",
                0,
            ),
            fix_hint: None,
        };
        to_sarif(&[finding], &[], &packs)
    }

    #[test]
    fn native_baseline_rejects_malformed_rows_failed_runs_and_wrong_namespaces() {
        for (pointer, replacement) in [
            ("/version", json!("1.0")),
            ("/runs/0/tool/driver/name", json!("pi dependency audit")),
            ("/runs/0/results", Value::Null),
            ("/runs/0/results/0/ruleId", json!("unknown")),
            ("/runs/0/results/0/level", json!("none")),
            (
                "/runs/0/results/0/fingerprints/pi~1v1",
                json!("PRIVATE-CANARY"),
            ),
            (
                "/runs/0/results/0/locations/0/physicalLocation/region/startLine",
                json!(0),
            ),
            (
                "/runs/0/results/0/locations/0/physicalLocation/artifactLocation/uri",
                json!("../escape"),
            ),
        ] {
            let mut doc = native_baseline();
            *doc.pointer_mut(pointer).unwrap() = replacement;
            let failure = findings_from_sarif(&doc.to_string())
                .unwrap_err()
                .to_string();
            assert!(failure.contains("SOURCE_BASELINE_INVALID"));
            assert!(!failure.contains("PRIVATE-CANARY"));
        }
        for text in ["PRIVATE-CANARY", "{}", "{\"runs\":[]}"] {
            assert!(findings_from_sarif(text).is_err());
        }
        let mut doc = native_baseline();
        doc["runs"][0]["invocations"] = json!([{"executionSuccessful":false}]);
        assert!(findings_from_sarif(&doc.to_string()).is_err());
        let mut duplicate = native_baseline();
        let result = duplicate["runs"][0]["results"][0].clone();
        duplicate["runs"][0]["results"]
            .as_array_mut()
            .unwrap()
            .push(result);
        assert!(findings_from_sarif(&duplicate.to_string()).is_err());
        let mut multiple = native_baseline();
        let run = multiple["runs"][0].clone();
        multiple["runs"].as_array_mut().unwrap().push(run);
        assert!(findings_from_sarif(&multiple.to_string()).is_err());
        assert!(
            findings_from_sarif(&to_sarif(&[], &[], &packs()).to_string())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn duplicate_native_json_fields_cannot_collapse_to_an_accepted_identity() {
        let text = native_baseline().to_string();
        for (needle, injected) in [
            ("\"runs\":", "\"runs\":[],\"runs\":"),
            ("\"results\":", "\"results\":[],\"results\":"),
            ("\"ruleId\":", "\"ruleId\":\"PRIVATE-CANARY\",\"ruleId\":"),
            ("\"pi/v1\":", "\"pi/v1\":\"PRIVATE-CANARY\",\"pi/v1\":"),
        ] {
            assert!(text.contains(needle));
            let duplicate = text.replacen(needle, injected, 1);
            let failure = findings_from_sarif(&duplicate).unwrap_err().to_string();
            assert!(!failure.contains("PRIVATE-CANARY"));
        }
    }

    fn comparison_fixture() -> (tempfile::TempDir, SecurityScanTool) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/code.py"), "pickle.loads(data)\n").unwrap();
        let tool = SecurityScanTool::new(dir.path());
        tool.op_run(&scan_input(
            json!({"op":"run","paths":["src"],"sarifOut":"baseline.sarif"}),
        ))
        .unwrap();
        (dir, tool)
    }

    #[test]
    fn compare_does_not_call_encoding_exclusions_ignored_or_removed_files_fixed() {
        for kind in ["encoding", "ignored", "removed"] {
            let (dir, tool) = comparison_fixture();
            match kind {
                "encoding" => fs::write(dir.path().join("src/code.py"), [0xff, 0xfe]).unwrap(),
                "ignored" => fs::write(dir.path().join("src/.ignore"), "code.py\n").unwrap(),
                _ => {
                    fs::rename(dir.path().join("src/code.py"), dir.path().join("saved.py"))
                        .unwrap();
                }
            }
            let output = tool
                .op_compare(&scan_input(json!({
                    "op":"compare","paths":["./src/"],"baseline":"baseline.sarif"
                })))
                .unwrap();
            assert!(output.is_error, "{kind}");
            let details = output.details.unwrap();
            assert_eq!(details["report"]["counts"]["fixed"], 0, "{kind}");
            assert_eq!(details["report"]["counts"]["unassessed"], 1, "{kind}");
            assert_eq!(details["comparisonComplete"], false);
        }
    }

    #[test]
    fn completed_scan_under_the_same_rules_confirms_absence_with_normalized_scope() {
        let (dir, tool) = comparison_fixture();
        fs::write(dir.path().join("src/code.py"), "pass\n").unwrap();
        for paths in [json!(["src"]), json!(["./src/"]), json!(["."]), json!([])] {
            let output = tool
                .op_compare(&scan_input(json!({
                    "op":"compare","paths":paths,"baseline":"baseline.sarif"
                })))
                .unwrap();
            assert!(!output.is_error);
            let details = output.details.unwrap();
            assert_eq!(details["report"]["counts"]["fixed"], 1);
            assert_eq!(details["report"]["counts"]["unassessed"], 0);
            assert_eq!(details["comparisonComplete"], true);
        }
    }

    #[test]
    fn changed_or_unbound_rule_definitions_cannot_establish_a_fix() {
        let (dir, tool) = comparison_fixture();
        fs::write(dir.path().join("src/code.py"), "pass\n").unwrap();
        let baseline_path = dir.path().join("baseline.sarif");
        let original: Value = serde_json::from_slice(&fs::read(&baseline_path).unwrap()).unwrap();
        for digest in [Some("0".repeat(64)), None] {
            let mut doc = original.clone();
            for rule in doc["runs"][0]["tool"]["driver"]["rules"]
                .as_array_mut()
                .unwrap()
            {
                if rule["id"] == "deserialization.unsafe-pickle" {
                    rule["properties"] = digest
                        .as_ref()
                        .map_or_else(|| json!({}), |digest| json!({"piSourceRuleDigest":digest}));
                }
            }
            fs::write(&baseline_path, doc.to_string()).unwrap();
            let output = tool
                .op_compare(&scan_input(json!({
                    "op":"compare","paths":["src"],"baseline":"baseline.sarif"
                })))
                .unwrap();
            let details = output.details.unwrap();
            assert_eq!(details["report"]["counts"]["fixed"], 0);
            assert_eq!(details["report"]["counts"]["unassessed"], 1);
        }
        let mut rule = pattern_rule("first").rule;
        let before = rule_digest(&rule);
        rule.pattern = "second".into();
        assert_ne!(rule_digest(&rule), before);
        rule.pattern = "first".into();
        rule.applies_to.push("*.py".into());
        assert_ne!(rule_digest(&rule), before);
    }

    #[test]
    fn public_source_tool_scan_and_compare_preserve_uncertainty_and_baseline_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("code.py");
        fs::write(&source, "pickle.loads(data)\n").unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .worker_threads(2)
            .blocking_threads(1, 8)
            .build()
            .unwrap();
        let tool = crate::security_scan::SecurityScanTool::new(dir.path());
        let first = runtime
            .block_on(tool.execute(
                "scan",
                json!({
                    "op":"run","paths":["code.py"],"sarifOut":"baseline.sarif"
                }),
                None,
            ))
            .unwrap();
        assert_eq!(first.details.unwrap()["active"], 1);
        let baseline = fs::read(dir.path().join("baseline.sarif")).unwrap();
        fs::write(&source, [0xff, 0xfe]).unwrap();
        let second = runtime
            .block_on(tool.execute(
                "compare",
                json!({
                    "op":"compare","paths":["code.py"],"baseline":"baseline.sarif"
                }),
                None,
            ))
            .unwrap();
        assert!(second.is_error);
        assert_eq!(second.details.unwrap()["report"]["counts"]["unassessed"], 1);
        assert_eq!(
            fs::read(dir.path().join("baseline.sarif")).unwrap(),
            baseline
        );
    }

    #[test]
    fn bounded_reads_distinguish_io_failures_from_non_utf8_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        fs::write(&path, [0xff, 0x00, 0x41, 0x42]).unwrap();
        assert_eq!(read_bounded(&path, 4).unwrap(), [0xff, 0x00, 0x41, 0x42]);
        assert!(read_bounded(&path, 3).is_err());
        assert!(read_bounded(dir.path(), 100).is_err());
        fs::rename(&path, dir.path().join("moved.bin")).unwrap();
        let failure = read_bounded(&path, 100).unwrap_err().to_string();
        assert!(failure.contains("SOURCE_SCAN_INCOMPLETE"));
    }

    #[test]
    fn source_scan_reports_unassessed_encoding_in_summary_and_sarif() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("clean.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.path().join("opaque.bin"), [0xff, 0xfe, 0x00]).unwrap();
        let tool = SecurityScanTool::new(dir.path());
        let output = tool
            .op_run(&scan_input(json!({"op":"run",
            "paths":["clean.rs","opaque.bin"], "sarifOut":"report.sarif"})))
            .unwrap();
        let details = output.details.unwrap();
        assert_eq!(details["scannedTextFiles"], 1);
        assert_eq!(details["skippedNonUtf8Files"], 1);
        assert_eq!(details["active"], 0);
        let saved: Value =
            serde_json::from_slice(&fs::read(dir.path().join("report.sarif")).unwrap()).unwrap();
        assert_eq!(
            saved["runs"][0]["properties"]["skippedNonUtf8Files"],
            json!(["opaque.bin"])
        );
        assert_eq!(saved["runs"][0]["properties"]["scannedTextFiles"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn broken_discovered_entry_cannot_replace_an_existing_success_report() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/clean.rs"), "fn main() {}\n").unwrap();
        std::os::unix::fs::symlink("missing.rs", dir.path().join("src/unreadable.rs")).unwrap();
        let report = dir.path().join("report.sarif");
        fs::write(&report, "preserve previous completed scan").unwrap();
        let tool = SecurityScanTool::new(dir.path());
        for op in ["plan", "run"] {
            let input = scan_input(json!({"op":op,"paths":["src"],"sarifOut":"report.sarif"}));
            let result = if op == "run" {
                tool.op_run(&input)
            } else {
                tool.op_plan(&input)
            };
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("SOURCE_SCAN_INCOMPLETE")
            );
        }
        assert_eq!(
            fs::read_to_string(&report).unwrap(),
            "preserve previous completed scan"
        );
    }

    #[test]
    fn invalid_rule_directory_and_malformed_pack_never_fall_back_to_default_rules() {
        let dir = tempfile::tempdir().unwrap();
        let pi = dir.path().join(".pi");
        fs::create_dir(&pi).unwrap();
        fs::write(pi.join("security-rules"), "not a directory").unwrap();
        assert!(load_rule_packs(dir.path()).is_err());
        fs::rename(pi.join("security-rules"), pi.join("saved-invalid-rules")).unwrap();
        fs::create_dir(pi.join("security-rules")).unwrap();
        fs::write(
            pi.join("security-rules/bad.toml"),
            "name = PRIVATE-CANARY [",
        )
        .unwrap();
        let failure = load_rule_packs(dir.path()).unwrap_err().to_string();
        assert!(failure.contains("malformed"));
        assert!(!failure.contains("PRIVATE-CANARY"));
    }

    #[test]
    fn project_rule_ids_cannot_override_or_collide_with_the_embedded_pack() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join(".pi/security-rules");
        fs::create_dir_all(&rules).unwrap();
        fs::write(
            rules.join("duplicate.toml"),
            r#"
name = "collision"
version = "1"
[[rules]]
id = "secret.aws-access-key"
pattern = "never-match"
severity = "note"
message = "collision"
"#,
        )
        .unwrap();
        let tool = SecurityScanTool::new(dir.path());
        assert!(tool.op_plan(&scan_input(json!({"op":"plan"}))).is_err());
        assert!(run_scan(dir.path(), &[]).is_err());
    }

    #[test]
    fn oversized_source_and_finding_budget_never_publish_a_partial_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.py");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();
        drop(file);
        let input = scan_input(json!({"op":"run","paths":["large.py"],"sarifOut":"result.sarif"}));
        let tool = SecurityScanTool::new(dir.path());
        assert!(tool.op_run(&input).is_err());
        assert!(!dir.path().join("result.sarif").exists());
        fs::write(&path, "pickle.loads(data)\n".repeat(MAX_FINDINGS + 1)).unwrap();
        assert!(
            tool.op_run(&input)
                .unwrap_err()
                .to_string()
                .contains("10000 findings")
        );
        assert!(!dir.path().join("result.sarif").exists());
    }

    #[test]
    fn tool_preview_is_bounded_while_saved_findings_remain_complete() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("code.py"),
            "pickle.loads(data)\n".repeat(51),
        )
        .unwrap();
        let output = SecurityScanTool::new(dir.path())
            .op_run(&scan_input(json!({
                "op":"run","paths":["code.py"],"sarifOut":"report.sarif"
            })))
            .unwrap();
        let details = output.details.unwrap();
        assert_eq!(details["active"], 51);
        assert_eq!(details["findings"].as_array().unwrap().len(), 50);
        assert_eq!(details["findingsTruncated"], true);
        let saved: Value =
            serde_json::from_slice(&fs::read(dir.path().join("report.sarif")).unwrap()).unwrap();
        assert_eq!(saved["runs"][0]["results"].as_array().unwrap().len(), 51);
    }

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
        let back = findings_from_sarif(&text).expect("valid native source report");
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
