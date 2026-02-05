//! Extension protocol, policy, and runtime scaffolding.
//!
//! This module defines the versioned extension protocol and provides
//! validation utilities plus a minimal WASM host scaffold.

use crate::agent::AgentEvent;
use crate::connectors::Connector;
use crate::connectors::http::HttpConnector;
use crate::error::{Error, Result};
use crate::extension_events::{ToolCallEventResult, ToolResultEventResult};
use crate::extensions_js::{
    ExtensionToolDef, HostcallKind, HostcallRequest, PiJsRuntime, PiJsRuntimeConfig, js_to_json,
    json_to_js,
};
use crate::scheduler::HostcallOutcome;
use crate::session::SessionMessage;
use crate::tools::ToolRegistry;
use asupersync::Cx;
use asupersync::channel::{mpsc, oneshot};
use asupersync::runtime::RuntimeBuilder;
#[cfg(feature = "wasm-host")]
use asupersync::sync::Mutex as AsyncMutex;
use asupersync::time::{sleep, timeout, wall_now};
use async_trait::async_trait;
use base64::Engine as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
#[cfg(feature = "wasm-host")]
use std::sync::Weak;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "1.0";
pub const LOG_SCHEMA_VERSION: &str = "pi.ext.log.v1";
pub const COMPAT_LEDGER_SCHEMA_VERSION: &str = "pi.ext.compat_ledger.v1";

// ============================================================================
// Compatibility Scanner (bd-3bs)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatEvidence {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub snippet: String,
}

impl CompatEvidence {
    #[must_use]
    pub const fn new(file: String, line: usize, column: usize, snippet: String) -> Self {
        Self {
            file,
            line,
            column,
            snippet,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatCapabilityEvidence {
    pub capability: String,
    pub reason: String,
    pub evidence: Vec<CompatEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatRewriteEvidence {
    pub from: String,
    pub to: String,
    pub evidence: Vec<CompatEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatIssueEvidence {
    pub rule: String,
    pub message: String,
    pub evidence: Vec<CompatEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompatLedger {
    pub schema: String,
    pub capabilities: Vec<CompatCapabilityEvidence>,
    pub rewrites: Vec<CompatRewriteEvidence>,
    pub forbidden: Vec<CompatIssueEvidence>,
    pub flagged: Vec<CompatIssueEvidence>,
}

impl CompatLedger {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            schema: COMPAT_LEDGER_SCHEMA_VERSION.to_string(),
            capabilities: Vec::new(),
            rewrites: Vec::new(),
            forbidden: Vec::new(),
            flagged: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.capabilities.is_empty()
            && self.rewrites.is_empty()
            && self.forbidden.is_empty()
            && self.flagged.is_empty()
    }

    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

#[derive(Debug, Clone)]
pub struct CompatibilityScanner {
    root: PathBuf,
}

impl CompatibilityScanner {
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn scan_path(&self, path: &Path) -> Result<CompatLedger> {
        let files = collect_js_like_files(path)?;
        Ok(self.scan_files(&files))
    }

    pub fn scan_root(&self) -> Result<CompatLedger> {
        self.scan_path(&self.root)
    }

    fn scan_files(&self, files: &[PathBuf]) -> CompatLedger {
        let mut caps: BTreeMap<(String, String, String), Vec<CompatEvidence>> = BTreeMap::new();
        let mut rewrites: BTreeMap<(String, String), Vec<CompatEvidence>> = BTreeMap::new();
        let mut forbidden: BTreeMap<(String, String, String), Vec<CompatEvidence>> =
            BTreeMap::new();
        let mut flagged: BTreeMap<(String, String, String), Vec<CompatEvidence>> = BTreeMap::new();

        for path in files {
            self.scan_file(path, &mut caps, &mut rewrites, &mut forbidden, &mut flagged);
        }

        let capabilities = caps
            .into_iter()
            .map(|((capability, reason, remediation), mut evidence)| {
                sort_evidence(&mut evidence);
                CompatCapabilityEvidence {
                    capability,
                    reason,
                    evidence,
                    remediation: if remediation.is_empty() {
                        None
                    } else {
                        Some(remediation)
                    },
                }
            })
            .collect();

        let rewrites = rewrites
            .into_iter()
            .map(|((from, to), mut evidence)| {
                sort_evidence(&mut evidence);
                CompatRewriteEvidence { from, to, evidence }
            })
            .collect();

        let forbidden = forbidden
            .into_iter()
            .map(|((rule, message, remediation), mut evidence)| {
                sort_evidence(&mut evidence);
                CompatIssueEvidence {
                    rule,
                    message,
                    evidence,
                    remediation: if remediation.is_empty() {
                        None
                    } else {
                        Some(remediation)
                    },
                }
            })
            .collect();

        let flagged = flagged
            .into_iter()
            .map(|((rule, message, remediation), mut evidence)| {
                sort_evidence(&mut evidence);
                CompatIssueEvidence {
                    rule,
                    message,
                    evidence,
                    remediation: if remediation.is_empty() {
                        None
                    } else {
                        Some(remediation)
                    },
                }
            })
            .collect();

        CompatLedger {
            schema: COMPAT_LEDGER_SCHEMA_VERSION.to_string(),
            capabilities,
            rewrites,
            forbidden,
            flagged,
        }
    }

    fn scan_file(
        &self,
        path: &Path,
        caps: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
        rewrites: &mut BTreeMap<(String, String), Vec<CompatEvidence>>,
        forbidden: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
        flagged: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
    ) {
        let Ok(content) = fs::read_to_string(path) else {
            return;
        };

        let rel = relative_posix(&self.root, path);

        for (idx, line) in content.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = line.trim_end().to_string();
            if trimmed.is_empty() {
                continue;
            }

            Self::scan_imports_in_line(&rel, line_no, &trimmed, caps, rewrites, forbidden, flagged);
            Self::scan_pi_apis_in_line(&rel, line_no, &trimmed, caps);
            Self::scan_flagged_apis_in_line(&rel, line_no, &trimmed, flagged);
            Self::scan_forbidden_patterns_in_line(&rel, line_no, &trimmed, forbidden);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_imports_in_line(
        file: &str,
        line: usize,
        text: &str,
        caps: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
        rewrites: &mut BTreeMap<(String, String), Vec<CompatEvidence>>,
        forbidden: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
        flagged: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
    ) {
        for (specifier, column) in extract_import_specifiers(text) {
            let evidence = CompatEvidence::new(file.to_string(), line, column, text.to_string());
            Self::classify_import(&specifier, evidence, caps, rewrites, forbidden, flagged);
        }

        for (specifier, column) in extract_require_specifiers(text) {
            let evidence = CompatEvidence::new(file.to_string(), line, column, text.to_string());
            Self::classify_import(&specifier, evidence, caps, rewrites, forbidden, flagged);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn classify_import(
        specifier: &str,
        evidence: CompatEvidence,
        caps: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
        rewrites: &mut BTreeMap<(String, String), Vec<CompatEvidence>>,
        forbidden: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
        flagged: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
    ) {
        let specifier = specifier.trim();
        if specifier.is_empty() {
            return;
        }

        let normalized = specifier.strip_prefix("node:").unwrap_or(specifier);
        let module_root = normalized.split('/').next().unwrap_or(normalized);

        if let Some(forbidden_reason) = forbidden_builtin_reason(module_root) {
            forbidden
                .entry((
                    "forbidden_import".to_string(),
                    format!("import of forbidden builtin `{specifier}`"),
                    forbidden_reason.to_string(),
                ))
                .or_default()
                .push(evidence);
            return;
        }

        if let Some((to, inferred_caps, hint)) = rewrite_target_and_caps(normalized) {
            rewrites
                .entry((specifier.to_string(), to.to_string()))
                .or_default()
                .push(evidence.clone());

            for cap in inferred_caps {
                caps.entry((
                    cap.to_string(),
                    format!("import:{normalized}"),
                    hint.to_string(),
                ))
                .or_default()
                .push(evidence.clone());
            }
            return;
        }

        if looks_like_node_builtin(module_root) {
            flagged
                .entry((
                    "unsupported_import".to_string(),
                    format!("import of unsupported builtin `{specifier}`"),
                    "No extc rewrite contract entry; replace with pi APIs or add a generic rewrite rule."
                        .to_string(),
                ))
                .or_default()
                .push(evidence);
        }
    }

    fn scan_pi_apis_in_line(
        file: &str,
        line: usize,
        text: &str,
        caps: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
    ) {
        for (cap, reason, column) in extract_pi_capabilities(text) {
            let evidence = CompatEvidence::new(file.to_string(), line, column, text.to_string());
            caps.entry((cap, reason, String::new()))
                .or_default()
                .push(evidence);
        }

        if let Some(column) = find_substring_column(text, "process.env") {
            let evidence = CompatEvidence::new(file.to_string(), line, column, text.to_string());
            caps.entry((
                "env".to_string(),
                "process.env".to_string(),
                "Declare `env` capability (scoped) or avoid reading host env vars.".to_string(),
            ))
            .or_default()
            .push(evidence);
        }
    }

    fn scan_flagged_apis_in_line(
        file: &str,
        line: usize,
        text: &str,
        flagged: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
    ) {
        if let Some(column) = find_regex_column(text, new_function_regex()) {
            let evidence = CompatEvidence::new(file.to_string(), line, column, text.to_string());
            flagged
                .entry((
                    "flagged_api".to_string(),
                    "new Function(...)".to_string(),
                    "Avoid dynamic code generation when possible; prefer static bundling. If required, ensure the function body is a literal and keep it minimal."
                        .to_string(),
                ))
                .or_default()
                .push(evidence);
        }

        if let Some(column) = find_regex_column(text, eval_regex()) {
            let evidence = CompatEvidence::new(file.to_string(), line, column, text.to_string());
            flagged
                .entry((
                    "flagged_api".to_string(),
                    "eval(...)".to_string(),
                    "Avoid eval; prefer parsing/dispatch on structured data. If unavoidable, keep the evaluated string literal and log evidence."
                        .to_string(),
                ))
                .or_default()
                .push(evidence);
        }
    }

    fn scan_forbidden_patterns_in_line(
        file: &str,
        line: usize,
        text: &str,
        forbidden: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
    ) {
        for (pattern, message, remediation) in forbidden_inline_patterns() {
            if let Some(column) = find_substring_column(text, pattern) {
                let evidence =
                    CompatEvidence::new(file.to_string(), line, column, text.to_string());
                forbidden
                    .entry((
                        "forbidden_api".to_string(),
                        message.to_string(),
                        remediation.to_string(),
                    ))
                    .or_default()
                    .push(evidence);
            }
        }
    }
}

fn collect_js_like_files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        if is_js_like(path) {
            return Ok(vec![path.to_path_buf()]);
        }
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    collect_js_like_files_recursive(path, &mut out)?;
    out.sort_by_key(|entry| relative_posix(path, entry));
    Ok(out)
}

fn collect_js_like_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            if should_ignore_dir(&path) {
                continue;
            }
            collect_js_like_files_recursive(&path, out)?;
        } else if file_type.is_file() && is_js_like(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn should_ignore_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(name, "node_modules" | "target" | "dist" | ".git")
}

fn is_js_like(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(ext, "ts" | "js" | "tsx" | "jsx" | "mts" | "cts")
}

fn relative_posix(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn sort_evidence(evidence: &mut [CompatEvidence]) {
    evidence.sort_by(|left, right| {
        (&left.file, left.line, left.column, &left.snippet).cmp(&(
            &right.file,
            right.line,
            right.column,
            &right.snippet,
        ))
    });
}

fn find_substring_column(haystack: &str, needle: &str) -> Option<usize> {
    haystack.find(needle).map(|idx| idx + 1)
}

fn find_regex_column(haystack: &str, regex: &Regex) -> Option<usize> {
    regex.find(haystack).map(|m| m.start() + 1)
}

fn import_from_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"^\s*import(?:\s+type)?\s+[^;]*?\s+from\s+["']([^"']+)["']"#)
            .expect("import from regex")
    })
}

fn import_side_effect_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"^\s*import\s+["']([^"']+)["']"#).expect("import regex"))
}

fn import_dynamic_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bimport\s*\(\s*["']([^"']+)["']\s*\)"#).expect("import()"))
}

fn require_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\brequire\s*\(\s*["']([^"']+)["']\s*\)"#).expect("require"))
}

fn new_function_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bnew\s+Function\s*\(").expect("new Function"))
}

fn eval_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\beval\s*\(").expect("eval"))
}

fn pi_tool_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bpi\.tool\s*\(\s*["']([^"']+)["']"#).expect("pi.tool"))
}

fn pi_exec_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bpi\.exec\s*\(").expect("pi.exec"))
}

fn pi_http_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bpi\.http\s*\(").expect("pi.http"))
}

fn pi_log_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bpi\.log\s*\(").expect("pi.log"))
}

fn pi_session_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bpi\.session\.").expect("pi.session"))
}

fn pi_ui_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bpi\.ui\.").expect("pi.ui"))
}

fn extract_import_specifiers(line: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();

    if let Some(caps) = import_from_regex().captures(line) {
        if let Some(m) = caps.get(1) {
            out.push((m.as_str().to_string(), m.start() + 1));
        }
    }

    if let Some(caps) = import_side_effect_regex().captures(line) {
        if let Some(m) = caps.get(1) {
            out.push((m.as_str().to_string(), m.start() + 1));
        }
    }

    for caps in import_dynamic_regex().captures_iter(line) {
        if let Some(m) = caps.get(1) {
            out.push((m.as_str().to_string(), m.start() + 1));
        }
    }

    out
}

fn extract_require_specifiers(line: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for caps in require_regex().captures_iter(line) {
        if let Some(m) = caps.get(1) {
            out.push((m.as_str().to_string(), m.start() + 1));
        }
    }
    out
}

fn extract_pi_capabilities(line: &str) -> Vec<(String, String, usize)> {
    let mut out = Vec::new();

    for caps in pi_tool_regex().captures_iter(line) {
        let Some(tool) = caps.get(1) else { continue };
        let tool_name = tool.as_str().trim().to_ascii_lowercase();
        let (capability, reason) = match tool_name.as_str() {
            "read" | "grep" | "find" | "ls" => ("read", format!("pi.tool({tool_name})")),
            "write" | "edit" => ("write", format!("pi.tool({tool_name})")),
            "bash" => ("exec", "pi.tool(bash)".to_string()),
            _ => ("tool", format!("pi.tool({tool_name})")),
        };
        out.push((capability.to_string(), reason, tool.start() + 1));
    }

    if let Some(column) = find_regex_column(line, pi_exec_regex()) {
        out.push(("exec".to_string(), "pi.exec".to_string(), column));
    }

    if let Some(column) = find_regex_column(line, pi_http_regex()) {
        out.push(("http".to_string(), "pi.http".to_string(), column));
    }

    if let Some(column) = find_regex_column(line, pi_log_regex()) {
        out.push(("log".to_string(), "pi.log".to_string(), column));
    }

    if let Some(column) = find_regex_column(line, pi_session_regex()) {
        out.push(("session".to_string(), "pi.session.*".to_string(), column));
    }

    if let Some(column) = find_regex_column(line, pi_ui_regex()) {
        out.push(("ui".to_string(), "pi.ui.*".to_string(), column));
    }

    out
}

fn forbidden_builtin_reason(module_root: &str) -> Option<&'static str> {
    match module_root {
        "vm" => Some("Arbitrary code execution; use hostcalls only."),
        "worker_threads" | "cluster" => Some("Unsupported concurrency model; use PiJS scheduler."),
        "dgram" => Some("Raw UDP sockets are not supported."),
        "net" | "tls" => Some("Raw sockets bypass HTTP policy; use fetch/pi.http."),
        "inspector" => Some("Debugger access is not allowed."),
        "perf_hooks" => Some("Timing oracle; use host-provided timing APIs if needed."),
        "v8" => Some("Engine internals are not allowed."),
        "repl" => Some("Interactive eval is not allowed."),
        _ => None,
    }
}

fn rewrite_target_and_caps(
    normalized: &str,
) -> Option<(&'static str, Vec<&'static str>, &'static str)> {
    match normalized {
        "fs" | "node:fs" => Some((
            "pi:node/fs",
            vec!["read", "write"],
            "Extc rewrites to `pi:node/fs`; declare `read`/`write` capabilities or use `pi.tool(...)` directly.",
        )),
        "fs/promises" | "node:fs/promises" => Some((
            "pi:node/fs_promises",
            vec!["read", "write"],
            "Extc rewrites to `pi:node/fs_promises`; declare `read`/`write` capabilities or use `pi.tool(...)` directly.",
        )),
        "path" | "node:path" => Some((
            "pi:node/path",
            Vec::new(),
            "Extc rewrites to `pi:node/path` (pure).",
        )),
        "os" | "node:os" => Some((
            "pi:node/os",
            vec!["env"],
            "Extc rewrites to `pi:node/os`; declare `env` capability (scoped) when reading host-derived values.",
        )),
        "url" | "node:url" => Some((
            "pi:node/url",
            Vec::new(),
            "Extc rewrites to `pi:node/url` (pure).",
        )),
        "crypto" | "node:crypto" => Some((
            "pi:node/crypto",
            Vec::new(),
            "Extc rewrites to `pi:node/crypto` (pure).",
        )),
        "child_process" | "node:child_process" => Some((
            "pi:node/child_process",
            vec!["exec"],
            "Extc rewrites to `pi:node/child_process`; declare `exec` or use `pi.exec(...)`.",
        )),
        "module" | "node:module" => Some((
            "pi:node/module",
            Vec::new(),
            "Extc rewrites to `pi:node/module`.",
        )),
        _ => None,
    }
}

fn looks_like_node_builtin(module_root: &str) -> bool {
    // Heuristic: common Node builtin module names. If it matches, we treat it as a builtin.
    // This keeps the scanner conservative without needing a full Node builtin registry.
    matches!(
        module_root,
        "assert"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "dns"
            | "domain"
            | "events"
            | "fs"
            | "http"
            | "https"
            | "inspector"
            | "module"
            | "net"
            | "os"
            | "path"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "repl"
            | "stream"
            | "string_decoder"
            | "sys"
            | "timers"
            | "tls"
            | "tty"
            | "url"
            | "util"
            | "v8"
            | "vm"
            | "worker_threads"
            | "zlib"
    )
}

fn forbidden_inline_patterns() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "process.binding(",
            "process.binding(...)",
            "Native module access is forbidden; remove this usage.",
        ),
        (
            "process.dlopen(",
            "process.dlopen(...)",
            "Native addon loading is forbidden; remove this usage.",
        ),
    ]
}

// ============================================================================
// Policy
// ============================================================================

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionPolicyMode {
    Strict,
    Prompt,
    Permissive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionPolicy {
    pub mode: ExtensionPolicyMode,
    pub max_memory_mb: u32,
    pub default_caps: Vec<String>,
    pub deny_caps: Vec<String>,
}

impl Default for ExtensionPolicy {
    fn default() -> Self {
        Self {
            mode: ExtensionPolicyMode::Prompt,
            max_memory_mb: 256,
            default_caps: vec!["read".to_string(), "write".to_string(), "http".to_string()],
            deny_caps: vec!["exec".to_string(), "env".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Prompt,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheck {
    pub decision: PolicyDecision,
    pub capability: String,
    pub reason: String,
}

impl ExtensionPolicy {
    pub fn evaluate(&self, capability: &str) -> PolicyCheck {
        let normalized = capability.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return PolicyCheck {
                decision: PolicyDecision::Deny,
                capability: String::new(),
                reason: "empty_capability".to_string(),
            };
        }

        if self
            .deny_caps
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case(&normalized))
        {
            return PolicyCheck {
                decision: PolicyDecision::Deny,
                capability: normalized,
                reason: "deny_caps".to_string(),
            };
        }

        let in_default_caps = self
            .default_caps
            .iter()
            .any(|cap| cap.eq_ignore_ascii_case(&normalized));

        match self.mode {
            ExtensionPolicyMode::Strict => PolicyCheck {
                decision: if in_default_caps {
                    PolicyDecision::Allow
                } else {
                    PolicyDecision::Deny
                },
                capability: normalized,
                reason: if in_default_caps {
                    "default_caps".to_string()
                } else {
                    "not_in_default_caps".to_string()
                },
            },
            ExtensionPolicyMode::Prompt => PolicyCheck {
                decision: if in_default_caps {
                    PolicyDecision::Allow
                } else {
                    PolicyDecision::Prompt
                },
                capability: normalized,
                reason: if in_default_caps {
                    "default_caps".to_string()
                } else {
                    "prompt_required".to_string()
                },
            },
            ExtensionPolicyMode::Permissive => PolicyCheck {
                decision: PolicyDecision::Allow,
                capability: normalized,
                reason: "permissive".to_string(),
            },
        }
    }
}

pub fn required_capability_for_host_call(call: &HostCallPayload) -> Option<String> {
    let method = call.method.trim().to_ascii_lowercase();
    if method.is_empty() {
        return None;
    }

    match method.as_str() {
        "fs" => {
            let op = call
                .params
                .get("op")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            let op = FsOp::parse(op)?;
            Some(op.required_capability().to_string())
        }
        "tool" => {
            let tool_name = call
                .params
                .get("name")
                .and_then(Value::as_str)
                .map(|name| name.trim().to_ascii_lowercase())?;
            if tool_name.is_empty() {
                return None;
            }
            match tool_name.as_str() {
                "read" | "grep" | "find" | "ls" => Some("read".to_string()),
                "write" | "edit" => Some("write".to_string()),
                "bash" => Some("exec".to_string()),
                _ => Some("tool".to_string()),
            }
        }
        "exec" => Some("exec".to_string()),
        "env" => Some("env".to_string()),
        "http" => Some("http".to_string()),
        "session" => Some("session".to_string()),
        "ui" => Some("ui".to_string()),
        "events" => Some("events".to_string()),
        "log" => Some("log".to_string()),
        _ => None,
    }
}

// ============================================================================
// Connectors
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsOp {
    Read,
    Write,
    List,
    Stat,
    Mkdir,
    Delete,
}

impl FsOp {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "list" | "readdir" => Some(Self::List),
            "stat" => Some(Self::Stat),
            "mkdir" => Some(Self::Mkdir),
            "delete" | "remove" | "rm" => Some(Self::Delete),
            _ => None,
        }
    }

    const fn required_capability(self) -> &'static str {
        match self {
            Self::Read | Self::List | Self::Stat => "read",
            Self::Write | Self::Mkdir | Self::Delete => "write",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FsScopes {
    read_declared: bool,
    write_declared: bool,
    read_roots: Vec<PathBuf>,
    write_roots: Vec<PathBuf>,
}

impl FsScopes {
    pub fn for_cwd(cwd: &Path) -> Result<Self> {
        let root = canonicalize_root(cwd)?;
        Ok(Self {
            read_declared: true,
            write_declared: true,
            read_roots: vec![root.clone()],
            write_roots: vec![root],
        })
    }

    pub fn from_manifest(manifest: Option<&CapabilityManifest>, cwd: &Path) -> Result<Self> {
        let Some(manifest) = manifest else {
            return Self::for_cwd(cwd);
        };

        let mut read_declared = false;
        let mut write_declared = false;
        let mut read_roots = Vec::new();
        let mut write_roots = Vec::new();

        for req in &manifest.capabilities {
            let cap = req.capability.trim().to_ascii_lowercase();
            if cap != "read" && cap != "write" {
                continue;
            }
            if cap == "read" {
                read_declared = true;
            } else {
                write_declared = true;
            }
            let Some(scope) = &req.scope else {
                continue;
            };
            let Some(paths) = &scope.paths else {
                continue;
            };

            for raw in paths {
                let root = resolve_scoped_root(raw, cwd)?;
                if cap == "read" {
                    read_roots.push(root);
                } else {
                    write_roots.push(root);
                }
            }
        }

        let fallback = canonicalize_root(cwd)?;
        if read_declared && read_roots.is_empty() {
            read_roots.push(fallback.clone());
        }
        if write_declared && write_roots.is_empty() {
            write_roots.push(fallback);
        }

        Ok(Self {
            read_declared,
            write_declared,
            read_roots,
            write_roots,
        })
    }

    fn roots_for_capability(&self, capability: &str) -> &[PathBuf] {
        if capability.eq_ignore_ascii_case("read") {
            if self.read_declared {
                &self.read_roots
            } else {
                &[]
            }
        } else if self.write_declared {
            &self.write_roots
        } else {
            &[]
        }
    }
}

#[derive(Debug, Clone)]
pub struct FsConnector {
    cwd: PathBuf,
    policy: ExtensionPolicy,
    scopes: FsScopes,
}

impl FsConnector {
    pub fn new(cwd: impl AsRef<Path>, policy: ExtensionPolicy, scopes: FsScopes) -> Result<Self> {
        let cwd = canonicalize_root(cwd.as_ref())?;
        Ok(Self {
            cwd,
            policy,
            scopes,
        })
    }

    pub fn handle_host_call(&self, call: &HostCallPayload) -> HostResultPayload {
        if !call.method.trim().eq_ignore_ascii_case("fs") {
            return HostResultPayload {
                call_id: call.call_id.clone(),
                output: json!({}),
                is_error: true,
                error: Some(HostCallError {
                    code: HostCallErrorCode::InvalidRequest,
                    message: "Unsupported hostcall method for FsConnector".to_string(),
                    details: Some(json!({ "method": call.method })),
                    retryable: None,
                }),
                chunk: None,
            };
        }

        let result = self.handle_fs_params(&call.params);
        match result {
            Ok(output) => HostResultPayload {
                call_id: call.call_id.clone(),
                output,
                is_error: false,
                error: None,
                chunk: None,
            },
            Err(error) => HostResultPayload {
                call_id: call.call_id.clone(),
                output: json!({}),
                is_error: true,
                error: Some(error),
                chunk: None,
            },
        }
    }

    fn handle_fs_params(&self, params: &Value) -> std::result::Result<Value, HostCallError> {
        let op = params
            .get("op")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let op = FsOp::parse(op).ok_or_else(|| HostCallError {
            code: HostCallErrorCode::InvalidRequest,
            message: "Invalid fs op".to_string(),
            details: Some(json!({ "op": op })),
            retryable: None,
        })?;

        let capability = op.required_capability();
        let policy_check = self.policy.evaluate(capability);
        if policy_check.decision != PolicyDecision::Allow {
            return Err(HostCallError {
                code: HostCallErrorCode::Denied,
                message: "Capability denied by policy".to_string(),
                details: Some(json!({
                    "capability": policy_check.capability,
                    "decision": format!("{:?}", policy_check.decision),
                    "reason": policy_check.reason,
                })),
                retryable: None,
            });
        }

        let roots = self.scopes.roots_for_capability(capability);
        if roots.is_empty() {
            return Err(HostCallError {
                code: HostCallErrorCode::Denied,
                message: "No allowed roots configured".to_string(),
                details: Some(json!({ "capability": capability })),
                retryable: None,
            });
        }

        let path_str = params
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .ok_or_else(|| HostCallError {
                code: HostCallErrorCode::InvalidRequest,
                message: "Missing fs path".to_string(),
                details: None,
                retryable: None,
            })?;

        let target = resolve_target_path(&self.cwd, path_str)?;

        let canonical_target = match op {
            FsOp::Read | FsOp::List | FsOp::Stat | FsOp::Delete => canonicalize_existing(&target),
            FsOp::Write | FsOp::Mkdir => canonicalize_for_create(&target),
        }?;

        let matched_root = roots.iter().find(|root| canonical_target.starts_with(root));

        if matched_root.is_none() {
            let root_hashes = roots.iter().map(|root| hash_path(root)).collect::<Vec<_>>();
            tracing::warn!(
                event = "ext.fs.denied",
                op = ?op,
                capability = capability,
                path_hash = %hash_path(&canonical_target),
                scope_roots = ?root_hashes,
                "Denied fs operation outside allowlist",
            );
            return Err(HostCallError {
                code: HostCallErrorCode::Denied,
                message: "Path outside allowed scope".to_string(),
                details: Some(json!({
                    "capability": capability,
                    "path_hash": hash_path(&canonical_target),
                    "scope_roots": root_hashes,
                })),
                retryable: None,
            });
        }

        let matched_root_hash = matched_root.map(|root| hash_path(root)).unwrap_or_default();
        tracing::info!(
            event = "ext.fs.call",
            op = ?op,
            capability = capability,
            path_hash = %hash_path(&canonical_target),
            scope_root = %matched_root_hash,
            "Executing fs operation",
        );

        match op {
            FsOp::Read => fs_op_read(params, &canonical_target),
            FsOp::Write => fs_op_write(params, &canonical_target),
            FsOp::List => fs_op_list(&canonical_target),
            FsOp::Stat => fs_op_stat(params, &canonical_target),
            FsOp::Mkdir => fs_op_mkdir(&canonical_target),
            FsOp::Delete => fs_op_delete(params, &canonical_target),
        }
    }
}

fn resolve_target_path(cwd: &Path, raw: &str) -> std::result::Result<PathBuf, HostCallError> {
    if raw.is_empty() {
        return Err(HostCallError {
            code: HostCallErrorCode::InvalidRequest,
            message: "Path is empty".to_string(),
            details: None,
            retryable: None,
        });
    }

    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

fn canonicalize_root(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|err| Error::extension(format!("canonicalize: {err}")))
}

fn resolve_scoped_root(raw: &str, cwd: &Path) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::validation("Capability scope path is empty"));
    }

    let path = Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    canonicalize_root(&resolved)
}

fn canonicalize_existing(path: &Path) -> std::result::Result<PathBuf, HostCallError> {
    std::fs::canonicalize(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("canonicalize: {err}"),
        details: Some(json!({ "path": path.display().to_string() })),
        retryable: None,
    })
}

fn canonicalize_for_create(path: &Path) -> std::result::Result<PathBuf, HostCallError> {
    // For non-existing paths, canonicalize the nearest existing ancestor and re-append suffix.
    let mut ancestor = path.to_path_buf();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .ok_or_else(|| HostCallError {
                code: HostCallErrorCode::InvalidRequest,
                message: "Path has no existing ancestor".to_string(),
                details: Some(json!({ "path": path.display().to_string() })),
                retryable: None,
            })?
            .to_path_buf();
    }

    let canonical_ancestor = std::fs::canonicalize(&ancestor).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("canonicalize: {err}"),
        details: Some(json!({ "path": ancestor.display().to_string() })),
        retryable: None,
    })?;

    let suffix = path.strip_prefix(&ancestor).map_err(|_| HostCallError {
        code: HostCallErrorCode::Internal,
        message: "Failed to compute path suffix".to_string(),
        details: Some(json!({
            "path": path.display().to_string(),
            "ancestor": ancestor.display().to_string(),
        })),
        retryable: None,
    })?;

    Ok(canonical_ancestor.join(suffix))
}

fn hash_path(path: &Path) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    format!("{digest:x}")
}

fn fs_op_read(params: &Value, path: &Path) -> std::result::Result<Value, HostCallError> {
    let encoding = params
        .get("encoding")
        .and_then(Value::as_str)
        .map_or("utf8", str::trim);

    let bytes = fs::read(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("read: {err}"),
        details: None,
        retryable: None,
    })?;

    match encoding.to_ascii_lowercase().as_str() {
        "utf8" | "utf-8" => {
            let text = String::from_utf8(bytes).map_err(|_| HostCallError {
                code: HostCallErrorCode::InvalidRequest,
                message: "File is not valid UTF-8; use base64 encoding".to_string(),
                details: Some(json!({ "encoding": "base64" })),
                retryable: None,
            })?;
            Ok(json!({ "encoding": "utf8", "text": text }))
        }
        "base64" => {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            Ok(json!({ "encoding": "base64", "data": data }))
        }
        other => Err(HostCallError {
            code: HostCallErrorCode::InvalidRequest,
            message: "Invalid encoding".to_string(),
            details: Some(json!({ "encoding": other })),
            retryable: None,
        }),
    }
}

fn fs_op_write(params: &Value, path: &Path) -> std::result::Result<Value, HostCallError> {
    let encoding = params
        .get("encoding")
        .and_then(Value::as_str)
        .map_or("utf8", str::trim);

    let data = params
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| HostCallError {
            code: HostCallErrorCode::InvalidRequest,
            message: "Missing write data".to_string(),
            details: None,
            retryable: None,
        })?;

    let bytes = match encoding.to_ascii_lowercase().as_str() {
        "utf8" | "utf-8" => data.as_bytes().to_vec(),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|err| HostCallError {
                code: HostCallErrorCode::InvalidRequest,
                message: format!("Invalid base64: {err}"),
                details: None,
                retryable: None,
            })?,
        other => {
            return Err(HostCallError {
                code: HostCallErrorCode::InvalidRequest,
                message: "Invalid encoding".to_string(),
                details: Some(json!({ "encoding": other })),
                retryable: None,
            });
        }
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("mkdir parent: {err}"),
            details: None,
            retryable: None,
        })?;
    }

    fs::write(path, &bytes).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("write: {err}"),
        details: None,
        retryable: None,
    })?;

    Ok(json!({ "bytes_written": bytes.len() }))
}

fn fs_op_list(path: &Path) -> std::result::Result<Value, HostCallError> {
    let read_dir = fs::read_dir(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("read_dir: {err}"),
        details: None,
        retryable: None,
    })?;

    let mut entries = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("read_dir entry: {err}"),
            details: None,
            retryable: None,
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = fs::symlink_metadata(entry.path()).map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("metadata: {err}"),
            details: None,
            retryable: None,
        })?;
        let kind = if meta.file_type().is_symlink() {
            "symlink"
        } else if meta.is_dir() {
            "dir"
        } else if meta.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(json!({ "name": name, "kind": kind }));
    }

    Ok(json!({ "entries": entries }))
}

fn fs_op_stat(params: &Value, path: &Path) -> std::result::Result<Value, HostCallError> {
    let follow = params
        .get("follow_symlinks")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let meta = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    }
    .map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("stat: {err}"),
        details: None,
        retryable: None,
    })?;

    Ok(json!({
        "is_file": meta.is_file(),
        "is_dir": meta.is_dir(),
        "len": meta.len(),
    }))
}

fn fs_op_mkdir(path: &Path) -> std::result::Result<Value, HostCallError> {
    fs::create_dir_all(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("mkdir: {err}"),
        details: None,
        retryable: None,
    })?;
    Ok(json!({ "created": true }))
}

fn fs_op_delete(params: &Value, path: &Path) -> std::result::Result<Value, HostCallError> {
    let recursive = params
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let meta = fs::symlink_metadata(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("stat: {err}"),
        details: None,
        retryable: None,
    })?;

    if meta.is_dir() && !meta.file_type().is_symlink() {
        if recursive {
            fs::remove_dir_all(path)
        } else {
            fs::remove_dir(path)
        }
        .map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("remove_dir: {err}"),
            details: None,
            retryable: None,
        })?;
        return Ok(json!({ "deleted": true, "kind": "dir" }));
    }

    fs::remove_file(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("remove_file: {err}"),
        details: None,
        retryable: None,
    })?;

    Ok(json!({ "deleted": true, "kind": "file" }))
}

// ============================================================================
// Protocol (v1)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionMessage {
    pub id: String,
    pub version: String,
    #[serde(flatten)]
    pub body: ExtensionBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum ExtensionBody {
    Register(RegisterPayload),
    ToolCall(ToolCallPayload),
    ToolResult(ToolResultPayload),
    SlashCommand(SlashCommandPayload),
    SlashResult(SlashResultPayload),
    EventHook(EventHookPayload),
    HostCall(HostCallPayload),
    HostResult(HostResultPayload),
    Log(Box<LogPayload>),
    Error(ErrorPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterPayload {
    pub name: String,
    pub version: String,
    pub api_version: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_manifest: Option<CapabilityManifest>,
    #[serde(default)]
    pub tools: Vec<Value>,
    #[serde(default)]
    pub slash_commands: Vec<Value>,
    #[serde(default)]
    pub event_hooks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityManifest {
    pub schema: String,
    pub capabilities: Vec<CapabilityRequirement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityRequirement {
    pub capability: String,
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<CapabilityScope>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallPayload {
    pub call_id: String,
    pub name: String,
    pub input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultPayload {
    pub call_id: String,
    pub output: Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCallPayload {
    pub call_id: String,
    pub capability: String,
    pub method: String,
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostCallErrorCode {
    Timeout,
    Denied,
    Io,
    InvalidRequest,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCallError {
    pub code: HostCallErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStreamChunk {
    pub index: u64,
    pub is_last: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backpressure: Option<HostStreamBackpressure>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStreamBackpressure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostResultPayload {
    pub call_id: String,
    pub output: Value,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<HostCallError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<HostStreamChunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandPayload {
    pub name: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashResultPayload {
    pub output: Value,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventHookPayload {
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogPayload {
    pub schema: String,
    pub ts: String,
    pub level: LogLevel,
    pub event: String,
    pub message: String,
    pub correlation: LogCorrelation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<LogSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogCorrelation {
    pub extension_id: String,
    pub scenario_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash_command_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogSource {
    pub component: LogComponent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogComponent {
    Capture,
    Harness,
    Runtime,
    Extension,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

// ============================================================================
// Extension UI + Session Bridge
// ============================================================================

/// Extension UI request payload (host -> UI surface).
#[derive(Debug, Clone)]
pub struct ExtensionUiRequest {
    pub id: String,
    pub method: String,
    pub payload: Value,
    pub timeout_ms: Option<u64>,
}

impl ExtensionUiRequest {
    pub fn new(id: impl Into<String>, method: impl Into<String>, payload: Value) -> Self {
        Self {
            id: id.into(),
            method: method.into(),
            payload,
            timeout_ms: None,
        }
    }

    pub fn expects_response(&self) -> bool {
        matches!(
            self.method.as_str(),
            "select" | "confirm" | "input" | "editor"
        )
    }

    pub fn effective_timeout_ms(&self) -> Option<u64> {
        self.timeout_ms.or_else(|| {
            self.payload
                .get("timeout")
                .and_then(serde_json::Value::as_u64)
        })
    }

    pub fn to_rpc_event(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "type".to_string(),
            Value::String("extension_ui_request".to_string()),
        );
        map.insert("id".to_string(), Value::String(self.id.clone()));
        map.insert("method".to_string(), Value::String(self.method.clone()));

        match &self.payload {
            Value::Object(obj) => {
                for (key, value) in obj {
                    map.insert(key.clone(), value.clone());
                }
            }
            other => {
                map.insert("payload".to_string(), other.clone());
            }
        }

        Value::Object(map)
    }
}

/// Extension UI response payload (UI surface -> host).
#[derive(Debug, Clone)]
pub struct ExtensionUiResponse {
    pub id: String,
    pub value: Option<Value>,
    pub cancelled: bool,
}

/// Minimal session access for extensions (hostcalls).
#[async_trait]
pub trait ExtensionSession: Send + Sync {
    async fn get_state(&self) -> Value;
    async fn get_messages(&self) -> Vec<SessionMessage>;
    async fn get_entries(&self) -> Vec<Value>;
    async fn get_branch(&self) -> Vec<Value>;
    async fn set_name(&self, name: String) -> Result<()>;
    async fn append_message(&self, message: SessionMessage) -> Result<()>;
    async fn append_custom_entry(&self, custom_type: String, data: Option<Value>) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionDeliverAs {
    Steer,
    FollowUp,
    NextTurn,
}

impl ExtensionDeliverAs {
    fn parse(value: Option<&str>) -> Option<Self> {
        let value = value?.trim();
        if value.is_empty() {
            return None;
        }
        match value {
            "steer" => Some(Self::Steer),
            "followUp" | "follow_up" | "follow-up" => Some(Self::FollowUp),
            "nextTurn" | "next_turn" | "next-turn" => Some(Self::NextTurn),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExtensionSendMessage {
    pub extension_id: Option<String>,
    pub custom_type: String,
    pub content: String,
    pub display: bool,
    pub details: Option<Value>,
    pub deliver_as: Option<ExtensionDeliverAs>,
    pub trigger_turn: bool,
}

#[derive(Debug, Clone)]
pub struct ExtensionSendUserMessage {
    pub extension_id: Option<String>,
    pub text: String,
    pub deliver_as: Option<ExtensionDeliverAs>,
}

#[async_trait]
pub trait ExtensionHostActions: Send + Sync {
    async fn send_message(&self, message: ExtensionSendMessage) -> Result<()>;
    async fn send_user_message(&self, message: ExtensionSendUserMessage) -> Result<()>;
}

impl ExtensionMessage {
    pub fn parse_and_validate(json: &str) -> Result<Self> {
        let msg: Self = serde_json::from_str(json)?;
        msg.validate()?;
        Ok(msg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            return Err(Error::validation("Extension message id is empty"));
        }
        if self.version != PROTOCOL_VERSION {
            return Err(Error::validation(format!(
                "Unsupported extension protocol version: {}",
                self.version
            )));
        }

        match &self.body {
            ExtensionBody::Register(payload) => validate_register(payload),
            ExtensionBody::ToolCall(payload) => validate_tool_call(payload),
            ExtensionBody::ToolResult(payload) => validate_tool_result(payload),
            ExtensionBody::SlashCommand(payload) => validate_slash_command(payload),
            ExtensionBody::SlashResult(_) => Ok(()),
            ExtensionBody::EventHook(payload) => validate_event_hook(payload),
            ExtensionBody::HostCall(payload) => validate_host_call(payload),
            ExtensionBody::HostResult(payload) => validate_host_result(payload),
            ExtensionBody::Log(payload) => validate_log(payload),
            ExtensionBody::Error(payload) => validate_error(payload),
        }
    }
}

fn validate_register(payload: &RegisterPayload) -> Result<()> {
    if payload.name.trim().is_empty() {
        return Err(Error::validation("Extension name is empty"));
    }
    if payload.version.trim().is_empty() {
        return Err(Error::validation("Extension version is empty"));
    }
    if payload.api_version.trim().is_empty() {
        return Err(Error::validation("Extension api_version is empty"));
    }

    if let Some(manifest) = &payload.capability_manifest {
        if manifest.schema != "pi.ext.cap.v1" {
            return Err(Error::validation(format!(
                "Unsupported capability manifest schema: {}",
                manifest.schema
            )));
        }

        for req in &manifest.capabilities {
            if req.capability.trim().is_empty() {
                return Err(Error::validation(
                    "Capability manifest includes empty capability",
                ));
            }
        }
    }
    Ok(())
}

fn validate_tool_call(payload: &ToolCallPayload) -> Result<()> {
    if payload.call_id.trim().is_empty() {
        return Err(Error::validation("Tool call_id is empty"));
    }
    if payload.name.trim().is_empty() {
        return Err(Error::validation("Tool name is empty"));
    }
    Ok(())
}

fn validate_tool_result(payload: &ToolResultPayload) -> Result<()> {
    if payload.call_id.trim().is_empty() {
        return Err(Error::validation("Tool result call_id is empty"));
    }
    Ok(())
}

fn validate_host_call(payload: &HostCallPayload) -> Result<()> {
    if payload.call_id.trim().is_empty() {
        return Err(Error::validation("Host call_id is empty"));
    }

    if !payload.params.is_object() {
        return Err(Error::validation("Host call params must be an object"));
    }

    let declared_capability = payload.capability.trim().to_ascii_lowercase();
    if declared_capability.is_empty() {
        return Err(Error::validation("Host call capability is empty"));
    }

    if payload.method.trim().is_empty() {
        return Err(Error::validation("Host call method is empty"));
    }

    let required = required_capability_for_host_call(payload).ok_or_else(|| {
        Error::validation(format!(
            "Unknown or invalid host call method: {}",
            payload.method
        ))
    })?;

    if declared_capability != required {
        return Err(Error::validation(format!(
            "Host call capability mismatch: declared {declared_capability}, required {required}"
        )));
    }
    Ok(())
}

fn validate_host_result(payload: &HostResultPayload) -> Result<()> {
    if payload.call_id.trim().is_empty() {
        return Err(Error::validation("Host result call_id is empty"));
    }
    if !payload.output.is_object() {
        return Err(Error::validation("Host result output must be an object"));
    }
    if payload.is_error {
        if payload.error.is_none() {
            return Err(Error::validation(
                "Host result marked is_error=true but error payload is missing",
            ));
        }
    } else if payload.error.is_some() {
        return Err(Error::validation(
            "Host result includes error payload but is_error=false",
        ));
    }
    Ok(())
}

fn validate_slash_command(payload: &SlashCommandPayload) -> Result<()> {
    if payload.name.trim().is_empty() {
        return Err(Error::validation("Slash command name is empty"));
    }
    Ok(())
}

fn validate_event_hook(payload: &EventHookPayload) -> Result<()> {
    if payload.event.trim().is_empty() {
        return Err(Error::validation("Event hook name is empty"));
    }
    Ok(())
}

fn validate_log(payload: &LogPayload) -> Result<()> {
    if payload.schema != LOG_SCHEMA_VERSION {
        return Err(Error::validation(format!(
            "Unsupported log schema: {}",
            payload.schema
        )));
    }
    if payload.ts.trim().is_empty() {
        return Err(Error::validation("Log timestamp is empty"));
    }
    if payload.event.trim().is_empty() {
        return Err(Error::validation("Log event is empty"));
    }
    if payload.message.trim().is_empty() {
        return Err(Error::validation("Log message is empty"));
    }
    if payload.correlation.extension_id.trim().is_empty() {
        return Err(Error::validation("Log correlation extension_id is empty"));
    }
    if payload.correlation.scenario_id.trim().is_empty() {
        return Err(Error::validation("Log correlation scenario_id is empty"));
    }
    Ok(())
}

fn validate_error(payload: &ErrorPayload) -> Result<()> {
    if payload.code.trim().is_empty() {
        return Err(Error::validation("Error code is empty"));
    }
    if payload.message.trim().is_empty() {
        return Err(Error::validation("Error message is empty"));
    }
    Ok(())
}

// ============================================================================
// WASM Host Scaffold (minimal)
// ============================================================================

#[cfg(feature = "wasm-host")]
#[derive(Debug, Clone)]
pub struct WasmExtension {
    pub path: PathBuf,
}

#[cfg(feature = "wasm-host")]
#[allow(clippy::trait_duplication_in_bounds)]
mod wasm_host {
    use super::*;

    use crate::connectors::http::{HttpConnector, HttpConnectorConfig};
    use std::collections::BTreeSet;
    use wasmtime::component::{Component, Linker};

    wasmtime::component::bindgen!({
        path: "docs/wit/extension.wit",
        world: "pi-extension",
        async: true,
    });

    use self::pi::extension::host;

    pub(super) struct HostState {
        policy: ExtensionPolicy,
        cwd: PathBuf,
        tools: Arc<crate::tools::ToolRegistry>,
        manager: Option<ExtensionManagerHandle>,
        http: HttpConnector,
        fs: FsConnector,
        env_allowlist: BTreeSet<String>,
        extension_id: Option<String>,
    }

    impl HostState {
        pub(super) fn new(policy: ExtensionPolicy, cwd: PathBuf) -> Result<Self> {
            let tools = Arc::new(crate::tools::ToolRegistry::new(
                &["read", "bash", "edit", "write", "grep", "find", "ls"],
                &cwd,
                None,
            ));
            Self::new_with_tools(policy, cwd, tools, None)
        }

        pub(super) fn new_with_tools(
            policy: ExtensionPolicy,
            cwd: PathBuf,
            tools: Arc<crate::tools::ToolRegistry>,
            manager: Option<ExtensionManagerHandle>,
        ) -> Result<Self> {
            let scopes = FsScopes::for_cwd(&cwd)?;
            let fs = FsConnector::new(&cwd, policy.clone(), scopes)?;
            Ok(Self {
                policy,
                cwd,
                tools,
                manager,
                http: HttpConnector::with_defaults(),
                fs,
                env_allowlist: BTreeSet::new(),
                extension_id: None,
            })
        }

        fn env_allowlist_from_manifest(manifest: Option<&CapabilityManifest>) -> BTreeSet<String> {
            let Some(manifest) = manifest else {
                return BTreeSet::new();
            };

            let mut out = BTreeSet::new();
            for req in &manifest.capabilities {
                if !req.capability.trim().eq_ignore_ascii_case("env") {
                    continue;
                }
                let Some(scope) = req.scope.as_ref() else {
                    continue;
                };
                let Some(env) = scope.env.as_ref() else {
                    continue;
                };
                for key in env {
                    let key = key.trim();
                    if !key.is_empty() {
                        out.insert(key.to_string());
                    }
                }
            }
            out
        }

        fn http_allowlist_from_manifest(manifest: Option<&CapabilityManifest>) -> Vec<String> {
            let Some(manifest) = manifest else {
                return Vec::new();
            };

            let mut out = Vec::new();
            for req in &manifest.capabilities {
                if !req.capability.trim().eq_ignore_ascii_case("http") {
                    continue;
                }
                let Some(scope) = req.scope.as_ref() else {
                    continue;
                };
                let Some(hosts) = scope.hosts.as_ref() else {
                    continue;
                };
                for host in hosts {
                    let host = host.trim();
                    if !host.is_empty() {
                        out.push(host.to_string());
                    }
                }
            }
            out
        }

        pub fn apply_registration(&mut self, registration: &RegisterPayload) -> Result<()> {
            if !registration.name.trim().is_empty() {
                self.extension_id = Some(registration.name.trim().to_string());
            }

            let manifest = registration.capability_manifest.as_ref();

            self.env_allowlist = Self::env_allowlist_from_manifest(manifest);

            let fs_scopes = FsScopes::from_manifest(manifest, &self.cwd)?;
            self.fs = FsConnector::new(&self.cwd, self.policy.clone(), fs_scopes)?;

            let http_allowlist = Self::http_allowlist_from_manifest(manifest);
            self.http = HttpConnector::new(HttpConnectorConfig {
                allowlist: http_allowlist,
                ..Default::default()
            });

            Ok(())
        }

        fn manager(&self) -> Option<ExtensionManager> {
            self.manager
                .as_ref()
                .and_then(ExtensionManagerHandle::upgrade)
        }

        fn hostcall_op(params: &Value) -> Option<String> {
            params
                .get("op")
                .or_else(|| params.get("method"))
                .or_else(|| params.get("name"))
                .and_then(Value::as_str)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        }

        fn host_error_json(
            code: HostCallErrorCode,
            message: impl Into<String>,
            details: Option<Value>,
            retryable: Option<bool>,
        ) -> String {
            let payload = HostCallError {
                code,
                message: message.into(),
                details,
                retryable,
            };
            serde_json::to_string(&payload).unwrap_or_else(|_| {
                format!(
                    "{{\"code\":\"internal\",\"message\":\"failed to serialize error: {}\"}}",
                    payload.message
                )
            })
        }

        fn hostcall_outcome_code(code: &str) -> HostCallErrorCode {
            match code {
                "timeout" => HostCallErrorCode::Timeout,
                "denied" => HostCallErrorCode::Denied,
                "io" => HostCallErrorCode::Io,
                "invalid_request" => HostCallErrorCode::InvalidRequest,
                _ => HostCallErrorCode::Internal,
            }
        }

        fn hostcall_outcome_to_result(
            outcome: HostcallOutcome,
        ) -> std::result::Result<String, String> {
            match outcome {
                HostcallOutcome::Success(value) => serde_json::to_string(&value).map_err(|err| {
                    Self::host_error_json(
                        HostCallErrorCode::Internal,
                        format!("Failed to serialize hostcall output: {err}"),
                        None,
                        None,
                    )
                }),
                HostcallOutcome::Error { code, message } => Err(Self::host_error_json(
                    Self::hostcall_outcome_code(&code),
                    message,
                    None,
                    None,
                )),
            }
        }

        async fn resolve_policy_decision(
            &self,
            required: &str,
        ) -> (PolicyDecision, String, String) {
            const UNKNOWN_EXTENSION_ID: &str = "<unknown>";
            let PolicyCheck {
                decision,
                capability,
                reason,
            } = self.policy.evaluate(required);

            if decision != PolicyDecision::Prompt {
                return (decision, reason, capability);
            }

            let Some(manager) = self.manager() else {
                return (
                    PolicyDecision::Deny,
                    "prompt_required_no_manager".to_string(),
                    capability,
                );
            };

            if let Some(extension_id) = self.extension_id.as_deref() {
                if let Some(allow) =
                    manager.cached_policy_prompt_decision(extension_id, &capability)
                {
                    let decision = if allow {
                        PolicyDecision::Allow
                    } else {
                        PolicyDecision::Deny
                    };
                    let reason = if allow {
                        "prompt_cache_allow".to_string()
                    } else {
                        "prompt_cache_deny".to_string()
                    };
                    return (decision, reason, capability);
                }
            }

            let prompt_extension_id = self.extension_id.as_deref().unwrap_or(UNKNOWN_EXTENSION_ID);
            let allow = prompt_capability_once(&manager, prompt_extension_id, &capability).await;
            if let Some(extension_id) = self.extension_id.as_deref() {
                manager.cache_policy_prompt_decision(extension_id, &capability, allow);
            }
            let decision = if allow {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Deny
            };
            let reason = if allow {
                "prompt_user_allow".to_string()
            } else {
                "prompt_user_deny".to_string()
            };
            (decision, reason, capability)
        }

        async fn dispatch_tool(
            &self,
            call: &HostCallPayload,
        ) -> std::result::Result<String, String> {
            let params = &call.params;
            let call_timeout_ms = call.timeout_ms.filter(|ms| *ms > 0);
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .ok_or_else(|| {
                    Self::host_error_json(
                        HostCallErrorCode::InvalidRequest,
                        "Missing tool name",
                        None,
                        None,
                    )
                })?;
            let mut input = params
                .get("input")
                .cloned()
                .unwrap_or_else(|| Value::Object(serde_json::Map::default()));

            if tool_name.eq_ignore_ascii_case("bash") && input.get("timeout").is_none() {
                if let Some(timeout_ms) = call_timeout_ms {
                    let timeout_secs = timeout_ms.div_ceil(1000);
                    if let Some(obj) = input.as_object_mut() {
                        obj.insert("timeout".to_string(), json!(timeout_secs));
                    }
                }
            }

            let tool = self.tools.get(tool_name).ok_or_else(|| {
                Self::host_error_json(
                    HostCallErrorCode::InvalidRequest,
                    format!("Unknown tool: {tool_name}"),
                    Some(json!({ "tool": tool_name })),
                    None,
                )
            })?;

            let execute = tool.execute(&call.call_id, input, None);
            let output = if let Some(timeout_ms) = call_timeout_ms {
                match timeout(
                    wall_now(),
                    Duration::from_millis(timeout_ms),
                    Box::pin(execute),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => {
                        return Err(Self::host_error_json(
                            HostCallErrorCode::Timeout,
                            format!("Tool execution timed out after {timeout_ms}ms"),
                            Some(json!({ "tool": tool_name, "timeout_ms": timeout_ms })),
                            Some(true),
                        ));
                    }
                }
            } else {
                execute.await
            }
            .map_err(|err| match &err {
                Error::Validation(_) => Self::host_error_json(
                    HostCallErrorCode::InvalidRequest,
                    err.to_string(),
                    Some(json!({ "tool": tool_name })),
                    None,
                ),
                Error::Tool { .. } | Error::Io(_) => Self::host_error_json(
                    HostCallErrorCode::Io,
                    err.to_string(),
                    Some(json!({ "tool": tool_name })),
                    None,
                ),
                Error::Aborted => Self::host_error_json(
                    HostCallErrorCode::Timeout,
                    "Tool execution aborted",
                    Some(json!({ "tool": tool_name })),
                    Some(true),
                ),
                _ => Self::host_error_json(
                    HostCallErrorCode::Internal,
                    err.to_string(),
                    Some(json!({ "tool": tool_name })),
                    None,
                ),
            })?;

            serde_json::to_string(&output).map_err(|err| {
                Self::host_error_json(
                    HostCallErrorCode::Internal,
                    format!("Failed to serialize tool output: {err}"),
                    Some(json!({ "tool": tool_name })),
                    None,
                )
            })
        }

        async fn dispatch_http(
            &self,
            call: &HostCallPayload,
        ) -> std::result::Result<String, String> {
            let connector_call = crate::connectors::HostCallPayload {
                call_id: call.call_id.clone(),
                capability: call.capability.clone(),
                method: call.method.clone(),
                params: call.params.clone(),
                timeout_ms: call.timeout_ms,
                cancel_token: call.cancel_token.clone(),
                context: call.context.clone(),
            };

            let result = self.http.dispatch(&connector_call).await.map_err(|err| {
                Self::host_error_json(HostCallErrorCode::Internal, err.to_string(), None, None)
            })?;

            if result.is_error {
                let error = result.error.as_ref().map_or_else(
                    || {
                        Self::host_error_json(
                            HostCallErrorCode::Internal,
                            "HTTP connector returned is_error=true but no error payload",
                            None,
                            None,
                        )
                    },
                    |payload| {
                        let code = match payload.code {
                            crate::connectors::HostCallErrorCode::Timeout => {
                                HostCallErrorCode::Timeout
                            }
                            crate::connectors::HostCallErrorCode::Denied => {
                                HostCallErrorCode::Denied
                            }
                            crate::connectors::HostCallErrorCode::Io => HostCallErrorCode::Io,
                            crate::connectors::HostCallErrorCode::InvalidRequest => {
                                HostCallErrorCode::InvalidRequest
                            }
                            crate::connectors::HostCallErrorCode::Internal => {
                                HostCallErrorCode::Internal
                            }
                        };

                        Self::host_error_json(
                            code,
                            payload.message.clone(),
                            payload.details.clone(),
                            payload.retryable,
                        )
                    },
                );
                return Err(error);
            }

            serde_json::to_string(&result.output).map_err(|err| {
                Self::host_error_json(
                    HostCallErrorCode::Internal,
                    format!("Failed to serialize HTTP output: {err}"),
                    None,
                    None,
                )
            })
        }

        async fn dispatch_exec(
            &self,
            call: &HostCallPayload,
        ) -> std::result::Result<String, String> {
            // Minimal: map exec -> bash tool (same sandbox semantics).
            let mut params = call.params.clone();
            if params.get("command").is_none() {
                let cmd = params
                    .get("cmd")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                let args_str = params.get("args").and_then(Value::as_array).map(|args| {
                    args.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                if let (Some(cmd), Some(args_str)) = (cmd, args_str) {
                    let command = format!("{cmd} {args_str}");
                    if let Some(obj) = params.as_object_mut() {
                        obj.insert("command".to_string(), Value::String(command));
                        obj.remove("cmd");
                        obj.remove("args");
                    } else {
                        params = json!({ "command": command });
                    }
                }
            }

            let bash_call = HostCallPayload {
                call_id: call.call_id.clone(),
                capability: call.capability.clone(),
                method: "tool".to_string(),
                params: json!({ "name": "bash", "input": params }),
                timeout_ms: call.timeout_ms,
                cancel_token: call.cancel_token.clone(),
                context: call.context.clone(),
            };

            self.dispatch_tool(&bash_call).await
        }

        async fn dispatch_fs(&self, call: &HostCallPayload) -> std::result::Result<String, String> {
            let result = self.fs.handle_host_call(call);

            if result.is_error {
                let error = result.error.as_ref().map_or_else(
                    || {
                        Self::host_error_json(
                            HostCallErrorCode::Internal,
                            "FS connector returned is_error=true but no error payload",
                            None,
                            None,
                        )
                    },
                    |payload| {
                        Self::host_error_json(
                            payload.code,
                            payload.message.clone(),
                            payload.details.clone(),
                            payload.retryable,
                        )
                    },
                );
                return Err(error);
            }

            serde_json::to_string(&result.output).map_err(|err| {
                Self::host_error_json(
                    HostCallErrorCode::Internal,
                    format!("Failed to serialize fs output: {err}"),
                    None,
                    None,
                )
            })
        }

        fn sha256_hex(input: &str) -> String {
            let mut hasher = sha2::Sha256::new();
            hasher.update(input.as_bytes());
            let digest = hasher.finalize();
            format!("{digest:x}")
        }

        fn canonicalize_json(value: &Value) -> Value {
            match value {
                Value::Object(map) => {
                    let mut keys = map.keys().cloned().collect::<Vec<_>>();
                    keys.sort();
                    let mut out = serde_json::Map::new();
                    for key in keys {
                        if let Some(value) = map.get(&key) {
                            out.insert(key, Self::canonicalize_json(value));
                        }
                    }
                    Value::Object(out)
                }
                Value::Array(items) => {
                    Value::Array(items.iter().map(Self::canonicalize_json).collect())
                }
                other => other.clone(),
            }
        }

        fn hostcall_params_hash(method: &str, params: &Value) -> String {
            let canonical = Self::canonicalize_json(&json!({ "method": method, "params": params }));
            let encoded = serde_json::to_string(&canonical)
                .unwrap_or_else(|_| "{\"error\":\"canonical_hostcall_failed\"}".to_string());
            Self::sha256_hex(&encoded)
        }

        async fn dispatch_env(
            &self,
            call: &HostCallPayload,
        ) -> std::result::Result<String, String> {
            let params = &call.params;
            let mut names = Vec::new();

            if let Some(name) = params.get("name").and_then(Value::as_str) {
                let name = name.trim();
                if !name.is_empty() {
                    names.push(name.to_string());
                }
            } else if let Some(items) = params.get("names").and_then(Value::as_array) {
                for item in items {
                    if let Some(name) = item.as_str() {
                        let name = name.trim();
                        if !name.is_empty() {
                            names.push(name.to_string());
                        }
                    }
                }
            }

            if names.is_empty() {
                return Err(Self::host_error_json(
                    HostCallErrorCode::InvalidRequest,
                    "Missing env var name(s)",
                    None,
                    None,
                ));
            }

            if self.env_allowlist.is_empty() {
                return Err(Self::host_error_json(
                    HostCallErrorCode::Denied,
                    "Env access not configured (no allowlist)",
                    Some(json!({ "capability": "env" })),
                    None,
                ));
            }

            let mut denied_hashes = Vec::new();
            for name in &names {
                if !self.env_allowlist.contains(name) {
                    denied_hashes.push(Self::sha256_hex(name));
                }
            }

            if !denied_hashes.is_empty() {
                return Err(Self::host_error_json(
                    HostCallErrorCode::Denied,
                    "Env var not allowed by scope",
                    Some(json!({ "denied_hashes": denied_hashes })),
                    None,
                ));
            }

            let mut values = serde_json::Map::new();
            for name in names {
                match std::env::var_os(&name) {
                    None => {
                        values.insert(name, Value::Null);
                    }
                    Some(value) => match value.into_string() {
                        Ok(value) => {
                            values.insert(name, Value::String(value));
                        }
                        Err(_) => {
                            return Err(Self::host_error_json(
                                HostCallErrorCode::Io,
                                "Env var value is not valid UTF-8",
                                Some(json!({ "name_hash": Self::sha256_hex(&name) })),
                                None,
                            ));
                        }
                    },
                }
            }

            let output = json!({ "values": Value::Object(values) });
            serde_json::to_string(&output).map_err(|err| {
                Self::host_error_json(
                    HostCallErrorCode::Internal,
                    format!("Failed to serialize env output: {err}"),
                    None,
                    None,
                )
            })
        }
    }

    impl host::Host for HostState {
        #[allow(clippy::too_many_lines)]
        async fn call(
            &mut self,
            name: String,
            input_json: String,
        ) -> std::result::Result<String, String> {
            let payload: HostCallPayload = match serde_json::from_str(&input_json) {
                Ok(value) => value,
                Err(err) => {
                    return Err(Self::host_error_json(
                        HostCallErrorCode::InvalidRequest,
                        format!("Invalid host_call JSON: {err}"),
                        None,
                        None,
                    ));
                }
            };

            if !name.trim().is_empty() && !payload.method.eq_ignore_ascii_case(name.trim()) {
                return Err(Self::host_error_json(
                    HostCallErrorCode::InvalidRequest,
                    "host.call name must match host_call.method",
                    Some(json!({ "name": name, "method": payload.method })),
                    None,
                ));
            }

            let Some(required) = required_capability_for_host_call(&payload) else {
                return Err(Self::host_error_json(
                    HostCallErrorCode::InvalidRequest,
                    format!("Unknown host_call method: {}", payload.method),
                    Some(json!({ "method": payload.method })),
                    None,
                ));
            };

            if !payload.capability.trim().eq_ignore_ascii_case(&required) {
                return Err(Self::host_error_json(
                    HostCallErrorCode::InvalidRequest,
                    "Capability mismatch: declared capability does not match derived capability",
                    Some(json!({
                        "declared": payload.capability,
                        "required": required,
                        "method": payload.method,
                    })),
                    None,
                ));
            }

            let call_timeout_ms = payload.timeout_ms.filter(|ms| *ms > 0);
            let params_hash = Self::hostcall_params_hash(&payload.method, &payload.params);
            let started_at = Instant::now();

            tracing::info!(
                event = "host_call.start",
                runtime = "wasm",
                call_id = %payload.call_id,
                extension_id = ?self.extension_id.as_deref(),
                capability = %required,
                method = %payload.method,
                params_hash = %params_hash,
                timeout_ms = call_timeout_ms,
                "Hostcall start"
            );

            let (decision, reason, capability) = self.resolve_policy_decision(&required).await;
            if decision == PolicyDecision::Allow {
                tracing::info!(
                    event = "policy.decision",
                    runtime = "wasm",
                    call_id = %payload.call_id,
                    extension_id = ?self.extension_id.as_deref(),
                    capability = %capability,
                    decision = ?decision,
                    reason = %reason,
                    params_hash = %params_hash,
                    "Hostcall allowed by policy"
                );
            } else {
                tracing::warn!(
                    event = "policy.decision",
                    runtime = "wasm",
                    call_id = %payload.call_id,
                    extension_id = ?self.extension_id.as_deref(),
                    capability = %capability,
                    decision = ?decision,
                    reason = %reason,
                    params_hash = %params_hash,
                    "Hostcall denied by policy"
                );
            }

            let method = payload.method.trim().to_ascii_lowercase();
            let outcome = if decision == PolicyDecision::Allow {
                let dispatch = async {
                    match method.as_str() {
                        "tool" => self.dispatch_tool(&payload).await,
                        "http" => self.dispatch_http(&payload).await,
                        "exec" => self.dispatch_exec(&payload).await,
                        "fs" => self.dispatch_fs(&payload).await,
                        "env" => self.dispatch_env(&payload).await,
                        "session" | "ui" | "events" => {
                            let op = Self::hostcall_op(&payload.params).ok_or_else(|| {
                                Self::host_error_json(
                                    HostCallErrorCode::InvalidRequest,
                                    format!("Missing host_call op for {method}"),
                                    Some(json!({ "method": method })),
                                    None,
                                )
                            })?;
                            let manager = self.manager().ok_or_else(|| {
                                Self::host_error_json(
                                    HostCallErrorCode::Denied,
                                    "No extension manager configured for host_call",
                                    Some(json!({ "method": method })),
                                    None,
                                )
                            })?;
                            let outcome = match method.as_str() {
                                "session" => {
                                    dispatch_hostcall_session(
                                        &payload.call_id,
                                        &manager,
                                        &op,
                                        payload.params.clone(),
                                    )
                                    .await
                                }
                                "ui" => {
                                    dispatch_hostcall_ui(
                                        &payload.call_id,
                                        &manager,
                                        &op,
                                        payload.params.clone(),
                                    )
                                    .await
                                }
                                "events" => {
                                    dispatch_hostcall_events(
                                        &payload.call_id,
                                        &manager,
                                        &op,
                                        payload.params.clone(),
                                    )
                                    .await
                                }
                                _ => HostcallOutcome::Error {
                                    code: "invalid_request".to_string(),
                                    message: format!("Unsupported host_call method: {method}"),
                                },
                            };
                            Self::hostcall_outcome_to_result(outcome)
                        }
                        _ => Err(Self::host_error_json(
                            HostCallErrorCode::InvalidRequest,
                            format!("Unsupported host_call method: {method}"),
                            Some(json!({ "method": method })),
                            None,
                        )),
                    }
                };

                match call_timeout_ms {
                    Some(timeout_ms) => timeout(
                        wall_now(),
                        Duration::from_millis(timeout_ms),
                        Box::pin(dispatch),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(Self::host_error_json(
                            HostCallErrorCode::Timeout,
                            format!("Hostcall timed out after {timeout_ms}ms"),
                            Some(json!({ "capability": required, "method": method })),
                            Some(true),
                        ))
                    }),
                    None => dispatch.await,
                }
            } else {
                Err(Self::host_error_json(
                    HostCallErrorCode::Denied,
                    format!("Capability '{capability}' denied by policy ({reason})"),
                    Some(json!({
                        "capability": capability,
                        "decision": format!("{:?}", decision),
                        "reason": reason,
                    })),
                    None,
                ))
            };

            let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            let (is_error, error_code) = match &outcome {
                Ok(_) => (false, None),
                Err(err_json) => (
                    true,
                    serde_json::from_str::<HostCallError>(err_json)
                        .ok()
                        .map(|err| err.code),
                ),
            };

            if is_error {
                tracing::warn!(
                    event = "host_call.end",
                    runtime = "wasm",
                    call_id = %payload.call_id,
                    extension_id = ?self.extension_id.as_deref(),
                    capability = %required,
                    method = %payload.method,
                    params_hash = %params_hash,
                    duration_ms,
                    error_code = ?error_code,
                    "Hostcall end (error)"
                );
            } else {
                tracing::info!(
                    event = "host_call.end",
                    runtime = "wasm",
                    call_id = %payload.call_id,
                    extension_id = ?self.extension_id.as_deref(),
                    capability = %required,
                    method = %payload.method,
                    params_hash = %params_hash,
                    duration_ms,
                    "Hostcall end (success)"
                );
            }

            outcome
        }
    }

    pub struct Instance {
        store: wasmtime::Store<HostState>,
        bindings: PiExtension,
    }

    impl Instance {
        pub(super) async fn instantiate(
            engine: &wasmtime::Engine,
            path: &Path,
            state: HostState,
        ) -> Result<Self> {
            let component = Component::from_file(engine, path).map_err(|err| {
                Error::extension(format!(
                    "Failed to load WASM component {}: {err}",
                    path.display()
                ))
            })?;

            let mut linker = Linker::<HostState>::new(engine);
            host::add_to_linker(&mut linker, |data| data).map_err(|err| {
                Error::extension(format!("Failed to link WASM host imports: {err}"))
            })?;

            let mut store = wasmtime::Store::new(engine, state);
            let bindings = PiExtension::instantiate_async(&mut store, &component, &linker)
                .await
                .map_err(|err| {
                    Error::extension(format!("Failed to instantiate WASM extension: {err}"))
                })?;

            Ok(Self { store, bindings })
        }

        pub async fn init(&mut self, manifest_json: &str) -> Result<String> {
            let result = self
                .bindings
                .interface0
                .call_init(&mut self.store, manifest_json)
                .await
                .map_err(|err| Error::extension(format!("WASM init failed: {err}")))?;

            let registration_json = result.map_err(Error::extension)?;
            let registration: RegisterPayload =
                serde_json::from_str(&registration_json).map_err(|err| {
                    Error::extension(format!(
                        "WASM init returned invalid registration payload: {err}"
                    ))
                })?;
            validate_register(&registration)?;
            self.store.data_mut().apply_registration(&registration)?;

            Ok(registration_json)
        }

        pub async fn handle_tool(&mut self, name: &str, input_json: &str) -> Result<String> {
            let result = self
                .bindings
                .interface0
                .call_handle_tool(&mut self.store, name, input_json)
                .await
                .map_err(|err| Error::extension(format!("WASM handle-tool failed: {err}")))?;

            result.map_err(Error::extension)
        }

        pub async fn handle_slash(
            &mut self,
            command: &str,
            args: &[String],
            input_json: &str,
        ) -> Result<String> {
            let result = self
                .bindings
                .interface0
                .call_handle_slash(&mut self.store, command, args, input_json)
                .await
                .map_err(|err| Error::extension(format!("WASM handle-slash failed: {err}")))?;

            result.map_err(Error::extension)
        }

        pub async fn handle_event(&mut self, event_json: &str) -> Result<String> {
            let result = self
                .bindings
                .interface0
                .call_handle_event(&mut self.store, event_json)
                .await
                .map_err(|err| Error::extension(format!("WASM handle-event failed: {err}")))?;

            result.map_err(Error::extension)
        }

        pub async fn shutdown(&mut self) -> Result<()> {
            self.bindings
                .interface0
                .call_shutdown(&mut self.store)
                .await
                .map_err(|err| Error::extension(format!("WASM shutdown failed: {err}")))?;
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::connectors::http::HttpConnectorConfig;
        use crate::model::ContentBlock;
        use crate::tools::{Tool, ToolOutput, ToolRegistry, ToolUpdate};
        use asupersync::runtime::RuntimeBuilder;
        use asupersync::time::{sleep, wall_now};
        use async_trait::async_trait;
        use serde_json::json;
        use std::collections::BTreeMap;
        use std::future::Future;
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};
        use std::thread;
        use std::time::Duration;
        use tempfile::tempdir;

        fn run_async<T, Fut>(future: Fut) -> T
        where
            Fut: Future<Output = T>,
        {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("build asupersync runtime");
            runtime.block_on(future)
        }

        fn permissive_policy() -> ExtensionPolicy {
            ExtensionPolicy {
                mode: ExtensionPolicyMode::Permissive,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
            }
        }

        fn strict_policy(default_caps: &[&str], deny_caps: &[&str]) -> ExtensionPolicy {
            ExtensionPolicy {
                mode: ExtensionPolicyMode::Strict,
                max_memory_mb: 256,
                default_caps: default_caps.iter().map(|cap| (*cap).to_string()).collect(),
                deny_caps: deny_caps.iter().map(|cap| (*cap).to_string()).collect(),
            }
        }

        fn registration_payload() -> RegisterPayload {
            RegisterPayload {
                name: "ext.test".to_string(),
                version: "0.1.0".to_string(),
                api_version: PROTOCOL_VERSION.to_string(),
                capabilities: Vec::new(),
                capability_manifest: Some(CapabilityManifest {
                    schema: "pi.ext.cap.v1".to_string(),
                    capabilities: vec![
                        CapabilityRequirement {
                            capability: "env".to_string(),
                            methods: vec!["env".to_string()],
                            scope: Some(CapabilityScope {
                                env: Some(vec!["PI_TEST_ENV".to_string()]),
                                paths: None,
                                hosts: None,
                            }),
                        },
                        CapabilityRequirement {
                            capability: "read".to_string(),
                            methods: vec!["fs".to_string()],
                            scope: Some(CapabilityScope {
                                paths: Some(vec![".".to_string()]),
                                hosts: None,
                                env: None,
                            }),
                        },
                    ],
                }),
                tools: Vec::new(),
                slash_commands: Vec::new(),
                event_hooks: Vec::new(),
            }
        }

        fn registration_payload_with_write_scope() -> RegisterPayload {
            let mut payload = registration_payload();
            let CapabilityManifest { capabilities, .. } = payload
                .capability_manifest
                .get_or_insert_with(|| CapabilityManifest {
                    schema: "pi.ext.cap.v1".to_string(),
                    capabilities: Vec::new(),
                });
            capabilities.push(CapabilityRequirement {
                capability: "write".to_string(),
                methods: vec!["fs".to_string()],
                scope: Some(CapabilityScope {
                    paths: Some(vec![".".to_string()]),
                    hosts: None,
                    env: None,
                }),
            });
            payload
        }

        #[derive(Debug, Clone)]
        struct CapturedEvent {
            level: tracing::Level,
            fields: BTreeMap<String, String>,
        }

        #[derive(Clone, Default)]
        struct CaptureLayer {
            events: Arc<Mutex<Vec<CapturedEvent>>>,
        }

        impl CaptureLayer {
            fn snapshot(&self) -> Vec<CapturedEvent> {
                self.events
                    .lock()
                    .expect("events mutex")
                    .iter()
                    .cloned()
                    .collect()
            }
        }

        struct FieldVisitor<'a> {
            fields: &'a mut BTreeMap<String, String>,
        }

        impl tracing::field::Visit for FieldVisitor<'_> {
            fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }

            fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }

            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }

            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }

            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.fields
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }

        impl<S> tracing_subscriber::Layer<S> for CaptureLayer
        where
            S: tracing::Subscriber,
        {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut fields = BTreeMap::new();
                let mut visitor = FieldVisitor {
                    fields: &mut fields,
                };
                event.record(&mut visitor);
                self.events
                    .lock()
                    .expect("events mutex")
                    .push(CapturedEvent {
                        level: *event.metadata().level(),
                        fields,
                    });
            }
        }

        fn capture_tracing_events<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedEvent>) {
            use tracing_subscriber::layer::SubscriberExt as _;

            let capture = CaptureLayer::default();
            let subscriber = tracing_subscriber::registry().with(capture.clone());
            let result = tracing::subscriber::with_default(subscriber, f);
            (result, capture.snapshot())
        }

        fn find_policy_decisions<'a>(
            events: &'a [CapturedEvent],
            call_id: &str,
        ) -> Vec<&'a CapturedEvent> {
            events
                .iter()
                .filter(|event| {
                    event
                        .fields
                        .get("event")
                        .is_some_and(|value| value == "policy.decision")
                        && event
                            .fields
                            .get("call_id")
                            .is_some_and(|value| value == call_id)
                })
                .collect()
        }

        fn assert_policy_decision_logged(
            events: &[CapturedEvent],
            call_id: &str,
            capability: &str,
            decision: &str,
        ) {
            let matching = find_policy_decisions(events, call_id);
            assert!(
                !matching.is_empty(),
                "expected policy.decision log for call_id={call_id}; got events: {events:#?}"
            );
            assert!(
                matching.iter().any(|event| {
                    event
                        .fields
                        .get("capability")
                        .is_some_and(|value| value == capability)
                        && event
                            .fields
                            .get("decision")
                            .is_some_and(|value| value == decision)
                        && event
                            .fields
                            .get("extension_id")
                            .is_some_and(|value| value.contains("ext.test"))
                }),
                "expected policy.decision with capability={capability} decision={decision} extension_id=ext.test; got: {matching:#?}"
            );
        }

        #[derive(Debug)]
        struct SleepTool;

        #[async_trait]
        impl Tool for SleepTool {
            fn name(&self) -> &'static str {
                "sleep"
            }

            fn label(&self) -> &'static str {
                "sleep"
            }

            fn description(&self) -> &'static str {
                "sleep tool"
            }

            fn parameters(&self) -> Value {
                json!({ "type": "object" })
            }

            async fn execute(
                &self,
                _tool_call_id: &str,
                _input: Value,
                _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
            ) -> Result<ToolOutput> {
                sleep(wall_now(), Duration::from_millis(200)).await;
                Ok(ToolOutput {
                    content: vec![],
                    details: None,
                    is_error: false,
                })
            }
        }

        #[test]
        fn wasm_host_env_requires_allowlist() {
            let dir = tempdir().expect("tempdir");
            let cwd = dir.path().to_path_buf();

            let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
            state
                .apply_registration(&registration_payload())
                .expect("apply registration");

            let allowed_call = HostCallPayload {
                call_id: "call-env-1".to_string(),
                capability: "env".to_string(),
                method: "env".to_string(),
                params: json!({ "name": "PI_TEST_ENV" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };

            let allowed_json = serde_json::to_string(&allowed_call).expect("serialize hostcall");
            let allowed_out = run_async(async {
                host::Host::call(&mut state, "env".to_string(), allowed_json).await
            })
            .expect("env hostcall ok");

            let out: Value = serde_json::from_str(&allowed_out).expect("parse env output");
            let values = out
                .get("values")
                .and_then(Value::as_object)
                .expect("values object");
            assert!(values.get("PI_TEST_ENV").is_some());

            let denied_call = HostCallPayload {
                call_id: "call-env-2".to_string(),
                capability: "env".to_string(),
                method: "env".to_string(),
                params: json!({ "name": "NOT_ALLOWED_ENV" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };

            let denied_json = serde_json::to_string(&denied_call).expect("serialize hostcall");
            let err_json = run_async(async {
                host::Host::call(&mut state, "env".to_string(), denied_json).await
            })
            .expect_err("env hostcall denied");
            let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
            assert_eq!(err.code, HostCallErrorCode::Denied);
        }

        #[test]
        fn wasm_host_env_denied_by_policy_even_when_allowlisted() {
            let dir = tempdir().expect("tempdir");
            let cwd = dir.path().to_path_buf();

            let mut state = HostState::new(ExtensionPolicy::default(), cwd).expect("host state");
            state
                .apply_registration(&registration_payload())
                .expect("apply registration");

            let call = HostCallPayload {
                call_id: "call-env-policy-deny".to_string(),
                capability: "env".to_string(),
                method: "env".to_string(),
                params: json!({ "name": "PI_TEST_ENV" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };

            let ((outcome, ()), events) = capture_tracing_events(|| {
                let json = serde_json::to_string(&call).expect("serialize hostcall");
                let outcome = run_async(async {
                    host::Host::call(&mut state, "env".to_string(), json).await
                });
                (outcome, ())
            });

            let err_json = outcome.expect_err("env hostcall denied by policy");
            let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
            assert_eq!(err.code, HostCallErrorCode::Denied);
            assert_policy_decision_logged(&events, &call.call_id, "env", "Deny");
        }

        #[test]
        fn wasm_host_fs_respects_manifest_scopes() {
            let dir = tempdir().expect("tempdir");
            let cwd = dir.path().to_path_buf();
            std::fs::write(dir.path().join("file.txt"), "hello").expect("write file");

            let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
            state
                .apply_registration(&registration_payload())
                .expect("apply registration");

            let read_call = HostCallPayload {
                call_id: "call-fs-read".to_string(),
                capability: "read".to_string(),
                method: "fs".to_string(),
                params: json!({ "op": "read", "path": "file.txt" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };

            let read_json = serde_json::to_string(&read_call).expect("serialize hostcall");
            let read_out = run_async(async {
                host::Host::call(&mut state, "fs".to_string(), read_json).await
            })
            .expect("fs read ok");
            let out: Value = serde_json::from_str(&read_out).expect("parse fs output");
            assert_eq!(out.get("text").and_then(Value::as_str), Some("hello"));

            let write_call = HostCallPayload {
                call_id: "call-fs-write".to_string(),
                capability: "write".to_string(),
                method: "fs".to_string(),
                params: json!({ "op": "write", "path": "out.txt", "encoding": "utf8", "data": "hi" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };

            let write_json = serde_json::to_string(&write_call).expect("serialize hostcall");
            let err_json = run_async(async {
                host::Host::call(&mut state, "fs".to_string(), write_json).await
            })
            .expect_err("fs write denied");
            let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
            assert_eq!(err.code, HostCallErrorCode::Denied);
        }

        #[test]
        fn wasm_host_fs_write_succeeds_with_write_scope_and_logs_policy() {
            let dir = tempdir().expect("tempdir");
            let cwd = dir.path().to_path_buf();

            let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
            state
                .apply_registration(&registration_payload_with_write_scope())
                .expect("apply registration");

            let call = HostCallPayload {
                call_id: "call-fs-write-ok".to_string(),
                capability: "write".to_string(),
                method: "fs".to_string(),
                params: json!({ "op": "write", "path": "out.txt", "encoding": "utf8", "data": "hi" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };

            let ((out, ()), events) = capture_tracing_events(|| {
                let json = serde_json::to_string(&call).expect("serialize hostcall");
                let out =
                    run_async(async { host::Host::call(&mut state, "fs".to_string(), json).await })
                        .expect("fs write ok");
                (out, ())
            });

            let out: Value = serde_json::from_str(&out).expect("parse fs output");
            assert_eq!(out.get("bytes_written").and_then(Value::as_u64), Some(2));
            assert_eq!(
                std::fs::read_to_string(dir.path().join("out.txt")).expect("read out.txt"),
                "hi"
            );
            assert_policy_decision_logged(&events, &call.call_id, "write", "Allow");
        }

        #[test]
        fn wasm_host_tool_call_times_out_and_returns_timeout_error() {
            let dir = tempdir().expect("tempdir");
            let cwd = dir.path().to_path_buf();

            let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
            state.tools = Arc::new(ToolRegistry::from_tools(vec![Box::new(SleepTool)]));
            state
                .apply_registration(&registration_payload())
                .expect("apply registration");

            let call = HostCallPayload {
                call_id: "call-tool-timeout".to_string(),
                capability: "tool".to_string(),
                method: "tool".to_string(),
                params: json!({ "name": "sleep", "input": {} }),
                timeout_ms: Some(50),
                cancel_token: None,
                context: None,
            };

            let ((outcome, ()), events) = capture_tracing_events(|| {
                let json = serde_json::to_string(&call).expect("serialize hostcall");
                let outcome = run_async(async {
                    host::Host::call(&mut state, "tool".to_string(), json).await
                });
                (outcome, ())
            });

            let err_json = outcome.expect_err("tool hostcall timeout");
            let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
            assert_eq!(err.code, HostCallErrorCode::Timeout);
            assert_policy_decision_logged(&events, &call.call_id, "tool", "Allow");
        }

        #[test]
        fn wasm_host_exec_denied_by_default_policy_and_logs_decision() {
            let dir = tempdir().expect("tempdir");
            let cwd = dir.path().to_path_buf();

            let mut state = HostState::new(ExtensionPolicy::default(), cwd).expect("host state");
            state
                .apply_registration(&registration_payload())
                .expect("apply registration");

            let call = HostCallPayload {
                call_id: "call-exec-deny".to_string(),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "command": "echo hi" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };

            let ((outcome, ()), events) = capture_tracing_events(|| {
                let json = serde_json::to_string(&call).expect("serialize hostcall");
                let outcome = run_async(async {
                    host::Host::call(&mut state, "exec".to_string(), json).await
                });
                (outcome, ())
            });

            let err_json = outcome.expect_err("exec denied");
            let err: HostCallError = serde_json::from_str(&err_json).expect("parse error json");
            assert_eq!(err.code, HostCallErrorCode::Denied);
            assert_policy_decision_logged(&events, &call.call_id, "exec", "Deny");
        }

        #[test]
        fn wasm_host_exec_succeeds_when_policy_allows() {
            let dir = tempdir().expect("tempdir");
            let cwd = dir.path().to_path_buf();

            let mut state = HostState::new(permissive_policy(), cwd).expect("host state");
            state
                .apply_registration(&registration_payload())
                .expect("apply registration");

            let call = HostCallPayload {
                call_id: "call-exec-ok".to_string(),
                capability: "exec".to_string(),
                method: "exec".to_string(),
                params: json!({ "command": "echo hello" }),
                timeout_ms: None,
                cancel_token: None,
                context: None,
            };

            let out_json = {
                let json = serde_json::to_string(&call).expect("serialize hostcall");
                run_async(async { host::Host::call(&mut state, "exec".to_string(), json).await })
                    .expect("exec ok")
            };

            let output: ToolOutput = serde_json::from_str(&out_json).expect("parse tool output");
            assert!(!output.is_error);
            let text = output
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("hello"));
        }

        #[test]
        fn wasm_host_http_get_succeeds_against_local_server_when_configured() {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            let addr = listener.local_addr().expect("local addr");

            let join = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                let _ = stream.write_all(response);
            });

            let dir = tempdir().expect("tempdir");
            let cwd = dir.path().to_path_buf();

            let mut state = HostState::new(strict_policy(&["http"], &[]), cwd).expect("host state");
            state
                .apply_registration(&registration_payload())
                .expect("apply registration");
            state.http = HttpConnector::new(HttpConnectorConfig {
                require_tls: false,
                allowlist: vec!["127.0.0.1".to_string()],
                ..Default::default()
            });

            let url = format!("http://127.0.0.1:{}/", addr.port());
            let call = HostCallPayload {
                call_id: "call-http-ok".to_string(),
                capability: "http".to_string(),
                method: "http".to_string(),
                params: json!({ "url": url, "method": "GET" }),
                timeout_ms: Some(2000),
                cancel_token: None,
                context: None,
            };

            let out_json = {
                let json = serde_json::to_string(&call).expect("serialize hostcall");
                run_async(async { host::Host::call(&mut state, "http".to_string(), json).await })
                    .expect("http ok")
            };

            let out: Value = serde_json::from_str(&out_json).expect("parse http output");
            assert_eq!(out.get("status").and_then(Value::as_u64), Some(200));
            assert_eq!(out.get("body").and_then(Value::as_str), Some("ok"));

            join.join().expect("server thread join");
        }
    }
}

#[cfg(feature = "wasm-host")]
pub struct WasmExtensionHost {
    policy: ExtensionPolicy,
    cwd: PathBuf,
    engine: wasmtime::Engine,
}

#[cfg(feature = "wasm-host")]
impl WasmExtensionHost {
    pub fn new(cwd: &Path, policy: ExtensionPolicy) -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.async_support(true);

        let engine = wasmtime::Engine::new(&config)
            .map_err(|err| Error::extension(format!("Failed to create WASM engine: {err}")))?;

        Ok(Self {
            policy,
            cwd: cwd.to_path_buf(),
            engine,
        })
    }

    pub const fn policy(&self) -> &ExtensionPolicy {
        &self.policy
    }

    pub fn load_from_path(&self, path: &Path) -> Result<WasmExtension> {
        if !path.exists() {
            return Err(Error::validation(format!(
                "Extension artifact not found: {}",
                path.display()
            )));
        }
        Ok(WasmExtension {
            path: path.to_path_buf(),
        })
    }

    pub async fn instantiate(&self, extension: &WasmExtension) -> Result<wasm_host::Instance> {
        wasm_host::Instance::instantiate(
            &self.engine,
            &extension.path,
            wasm_host::HostState::new(self.policy.clone(), self.cwd.clone())?,
        )
        .await
    }

    async fn instantiate_with(
        &self,
        extension: &WasmExtension,
        tools: Arc<ToolRegistry>,
        manager: Option<ExtensionManagerHandle>,
    ) -> Result<wasm_host::Instance> {
        wasm_host::Instance::instantiate(
            &self.engine,
            &extension.path,
            wasm_host::HostState::new_with_tools(
                self.policy.clone(),
                self.cwd.clone(),
                tools,
                manager,
            )?,
        )
        .await
    }
}

// ============================================================================
// Extension Event System
// ============================================================================

/// Timeout for extension events in milliseconds.
pub const EXTENSION_EVENT_TIMEOUT_MS: u64 = 5000;

/// Event names for the extension lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionEventName {
    /// Agent startup (once per session).
    Startup,
    /// Input from the user.
    Input,
    /// Before the agent starts processing.
    BeforeAgentStart,
    /// Agent started processing.
    AgentStart,
    /// Agent ended processing.
    AgentEnd,
    /// Turn lifecycle start.
    TurnStart,
    /// Turn lifecycle end.
    TurnEnd,
    /// Message lifecycle start.
    MessageStart,
    /// Message lifecycle update (assistant streaming).
    MessageUpdate,
    /// Message lifecycle end.
    MessageEnd,
    /// Tool execution start.
    ToolExecutionStart,
    /// Tool execution update.
    ToolExecutionUpdate,
    /// Tool execution end.
    ToolExecutionEnd,
    /// Tool call (pre-exec; can block).
    ToolCall,
    /// Tool result (post-exec; can modify).
    ToolResult,
    /// Session before switch.
    SessionBeforeSwitch,
    /// Session switched.
    SessionSwitch,
    /// Session before fork.
    SessionBeforeFork,
    /// Session forked.
    SessionFork,
    /// Session before compact.
    SessionBeforeCompact,
    /// Session compacted.
    SessionCompact,
}

impl std::fmt::Display for ExtensionEventName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Startup => "startup",
            Self::Input => "input",
            Self::BeforeAgentStart => "before_agent_start",
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::MessageStart => "message_start",
            Self::MessageUpdate => "message_update",
            Self::MessageEnd => "message_end",
            Self::ToolExecutionStart => "tool_execution_start",
            Self::ToolExecutionUpdate => "tool_execution_update",
            Self::ToolExecutionEnd => "tool_execution_end",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::SessionBeforeSwitch => "session_before_switch",
            Self::SessionSwitch => "session_switch",
            Self::SessionBeforeFork => "session_before_fork",
            Self::SessionFork => "session_fork",
            Self::SessionBeforeCompact => "session_before_compact",
            Self::SessionCompact => "session_compact",
        };
        write!(f, "{name}")
    }
}

// ============================================================================
// Extension Manifest + Load Specs
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExtensionRuntime {
    Js,
    Wasm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    pub schema: String,
    pub extension_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub api_version: String,
    pub runtime: ExtensionRuntime,
    pub entrypoint: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_manifest: Option<CapabilityManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ExtensionManifest {
    fn normalize(
        mut self,
        package_name: Option<String>,
        package_version: Option<String>,
    ) -> Result<Self> {
        if self.name.trim().is_empty() {
            if let Some(name) = package_name {
                self.name = name;
            }
        }

        if self.version.trim().is_empty() {
            if let Some(version) = package_version {
                self.version = version;
            }
        }

        if self.api_version.trim().is_empty() {
            self.api_version = PROTOCOL_VERSION.to_string();
        }

        validate_extension_manifest(&self)?;
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub struct ExtensionManifestSource {
    pub manifest: ExtensionManifest,
    pub manifest_json: String,
    pub root: PathBuf,
    pub manifest_path: PathBuf,
}

impl ExtensionManifestSource {
    pub fn entry_path(&self) -> PathBuf {
        self.root.join(self.manifest.entrypoint.trim())
    }
}

#[derive(Debug, Clone)]
pub enum ExtensionLoadSpec {
    Js(JsExtensionLoadSpec),
    #[cfg(feature = "wasm-host")]
    Wasm(WasmExtensionLoadSpec),
}

#[cfg(feature = "wasm-host")]
#[derive(Debug, Clone)]
pub struct WasmExtensionLoadSpec {
    pub manifest: ExtensionManifest,
    pub manifest_json: String,
    pub root: PathBuf,
    pub entry_path: PathBuf,
}

fn extension_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9._-]{0,63}$").expect("regex"))
}

fn validate_extension_manifest(manifest: &ExtensionManifest) -> Result<()> {
    if manifest.schema != "pi.ext.manifest.v1" {
        return Err(Error::validation(format!(
            "Unsupported extension manifest schema: {}",
            manifest.schema
        )));
    }

    let extension_id = manifest.extension_id.trim();
    if extension_id.is_empty() {
        return Err(Error::validation(
            "Extension manifest extension_id is empty",
        ));
    }
    if !extension_id_regex().is_match(extension_id) {
        return Err(Error::validation(format!(
            "Invalid extension_id '{extension_id}'"
        )));
    }

    if manifest.name.trim().is_empty() {
        return Err(Error::validation("Extension manifest name is empty"));
    }
    if manifest.version.trim().is_empty() {
        return Err(Error::validation("Extension manifest version is empty"));
    }
    if manifest.api_version.trim().is_empty() {
        return Err(Error::validation("Extension manifest api_version is empty"));
    }
    if manifest.entrypoint.trim().is_empty() {
        return Err(Error::validation("Extension manifest entrypoint is empty"));
    }
    let entry_path = Path::new(manifest.entrypoint.trim());
    if entry_path.is_absolute()
        || entry_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(Error::validation(format!(
            "Extension manifest entrypoint must be a relative path inside the extension root: {}",
            manifest.entrypoint
        )));
    }

    if let Some(capability_manifest) = &manifest.capability_manifest {
        if capability_manifest.schema != "pi.ext.cap.v1" {
            return Err(Error::validation(format!(
                "Unsupported capability manifest schema: {}",
                capability_manifest.schema
            )));
        }
    }

    Ok(())
}

fn read_package_json_meta(root: &Path) -> Option<(Option<String>, Option<String>, Option<Value>)> {
    let package_json = root.join("package.json");
    if !package_json.exists() {
        return None;
    }
    let raw = fs::read_to_string(package_json).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    let name = json.get("name").and_then(Value::as_str).map(str::to_string);
    let version = json
        .get("version")
        .and_then(Value::as_str)
        .map(str::to_string);
    let pi = json.get("pi").cloned();
    Some((name, version, pi))
}

fn parse_extension_manifest_value(
    value: Value,
    package_name: Option<String>,
    package_version: Option<String>,
) -> Result<ExtensionManifest> {
    let manifest: ExtensionManifest = serde_json::from_value(value)
        .map_err(|err| Error::validation(format!("Invalid extension manifest: {err}")))?;
    manifest.normalize(package_name, package_version)
}

pub fn load_extension_manifest(root: &Path) -> Result<Option<ExtensionManifestSource>> {
    let (package_name, package_version, package_pi) =
        read_package_json_meta(root).unwrap_or((None, None, None));

    let extension_json = root.join("extension.json");
    if extension_json.exists() {
        let raw = fs::read_to_string(&extension_json).map_err(|err| {
            Error::validation(format!(
                "Failed to read extension manifest {}: {err}",
                extension_json.display()
            ))
        })?;
        let value: Value = serde_json::from_str(&raw).map_err(|err| {
            Error::validation(format!(
                "Failed to parse extension manifest {}: {err}",
                extension_json.display()
            ))
        })?;
        let manifest = parse_extension_manifest_value(value, package_name, package_version)?;
        let manifest_json = serde_json::to_string(&manifest)
            .map_err(|err| Error::validation(format!("Serialize manifest: {err}")))?;
        return Ok(Some(ExtensionManifestSource {
            manifest,
            manifest_json,
            root: root.to_path_buf(),
            manifest_path: extension_json,
        }));
    }

    if let Some(pi) = package_pi {
        if pi.get("schema").and_then(Value::as_str) == Some("pi.ext.manifest.v1") {
            let manifest = parse_extension_manifest_value(pi, package_name, package_version)?;
            let manifest_json = serde_json::to_string(&manifest)
                .map_err(|err| Error::validation(format!("Serialize manifest: {err}")))?;
            let manifest_path = root.join("package.json");
            return Ok(Some(ExtensionManifestSource {
                manifest,
                manifest_json,
                root: root.to_path_buf(),
                manifest_path,
            }));
        }
    }

    Ok(None)
}

fn resolve_extension_index(root: &Path) -> Option<PathBuf> {
    let index_ts = root.join("index.ts");
    if index_ts.exists() {
        return Some(index_ts);
    }
    let index_js = root.join("index.js");
    if index_js.exists() {
        return Some(index_js);
    }
    None
}

impl ExtensionManifestSource {
    fn to_load_spec(&self) -> Result<ExtensionLoadSpec> {
        let entry_path = self.entry_path();
        if !entry_path.exists() {
            return Err(Error::validation(format!(
                "Extension entrypoint not found: {}",
                entry_path.display()
            )));
        }

        match self.manifest.runtime {
            ExtensionRuntime::Js => Ok(ExtensionLoadSpec::Js(JsExtensionLoadSpec::from_manifest(
                &self.manifest,
                &self.root,
            )?)),
            ExtensionRuntime::Wasm => {
                #[cfg(feature = "wasm-host")]
                {
                    Ok(ExtensionLoadSpec::Wasm(WasmExtensionLoadSpec {
                        manifest: self.manifest.clone(),
                        manifest_json: self.manifest_json.clone(),
                        root: self.root.clone(),
                        entry_path,
                    }))
                }
                #[cfg(not(feature = "wasm-host"))]
                {
                    Err(Error::validation(
                        "WASM extensions require the `wasm-host` feature".to_string(),
                    ))
                }
            }
        }
    }
}

pub fn resolve_extension_load_spec(entry: &Path) -> Result<ExtensionLoadSpec> {
    if entry.is_dir() {
        if let Some(source) = load_extension_manifest(entry)? {
            return source.to_load_spec();
        }
        if let Some(index) = resolve_extension_index(entry) {
            return Ok(ExtensionLoadSpec::Js(JsExtensionLoadSpec::from_entry_path(
                index,
            )?));
        }
        return Err(Error::validation(format!(
            "Extension directory has no manifest or entrypoint: {}",
            entry.display()
        )));
    }

    if entry.is_file() {
        if entry
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s == "extension.json")
        {
            let root = entry.parent().unwrap_or(entry);
            if let Some(source) = load_extension_manifest(root)? {
                return source.to_load_spec();
            }
        }

        if let Some(ext) = entry.extension().and_then(|s| s.to_str()) {
            match ext {
                "wasm" => {
                    #[cfg(feature = "wasm-host")]
                    {
                        if let Some(source) =
                            load_extension_manifest(entry.parent().unwrap_or(entry))?
                        {
                            let spec = source.to_load_spec()?;
                            if let ExtensionLoadSpec::Wasm(wasm_spec) = spec {
                                if wasm_spec.entry_path != entry {
                                    return Err(Error::validation(format!(
                                        "WASM entrypoint mismatch: manifest entrypoint is {}, but got {}",
                                        wasm_spec.entry_path.display(),
                                        entry.display()
                                    )));
                                }
                                return Ok(ExtensionLoadSpec::Wasm(wasm_spec));
                            }
                            return Err(Error::validation(format!(
                                "Extension manifest runtime is not wasm for {}",
                                entry.display()
                            )));
                        }
                        return Err(Error::validation(format!(
                            "WASM extension requires extension.json or package.json#pi manifest: {}",
                            entry.display()
                        )));
                    }
                    #[cfg(not(feature = "wasm-host"))]
                    {
                        return Err(Error::validation(
                            "WASM extensions require the `wasm-host` feature".to_string(),
                        ));
                    }
                }
                "js" | "ts" | "mjs" | "cjs" => {
                    return Ok(ExtensionLoadSpec::Js(JsExtensionLoadSpec::from_entry_path(
                        entry,
                    )?));
                }
                _ => {}
            }
        }
    }

    Err(Error::validation(format!(
        "Unsupported extension entry: {}",
        entry.display()
    )))
}

// ============================================================================
// JS Extension Runtime (QuickJS via PiJsRuntime)
// ============================================================================

#[derive(Debug, Clone)]
pub struct JsExtensionLoadSpec {
    pub extension_id: String,
    pub entry_path: PathBuf,
    pub name: String,
    pub version: String,
    pub api_version: String,
}

impl JsExtensionLoadSpec {
    pub fn from_entry_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(Error::validation(format!(
                "Extension entry does not exist: {}",
                path.display()
            )));
        }

        let entry_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        let file_stem = entry_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if file_stem.is_empty() {
            return Err(Error::validation(format!(
                "Extension entry has no filename: {}",
                entry_path.display()
            )));
        }

        let extension_id = if file_stem == "index" {
            entry_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .trim()
                .to_string()
        } else {
            file_stem
        };

        if extension_id.is_empty() {
            return Err(Error::validation(format!(
                "Could not derive extension id from entry path: {}",
                entry_path.display()
            )));
        }

        let mut name = extension_id.clone();
        let mut version = "0.0.0".to_string();

        if let Some(parent) = entry_path.parent() {
            let manifest_path = parent.join("package.json");
            if manifest_path.exists() {
                if let Ok(raw) = fs::read_to_string(&manifest_path) {
                    if let Ok(json) = serde_json::from_str::<Value>(&raw) {
                        if let Some(manifest_name) = json.get("name").and_then(Value::as_str) {
                            if !manifest_name.trim().is_empty() {
                                name = manifest_name.trim().to_string();
                            }
                        }
                        if let Some(manifest_version) = json.get("version").and_then(Value::as_str)
                        {
                            if !manifest_version.trim().is_empty() {
                                version = manifest_version.trim().to_string();
                            }
                        }
                    }
                }
            }
        }

        Ok(Self {
            extension_id,
            entry_path,
            name,
            version,
            api_version: PROTOCOL_VERSION.to_string(),
        })
    }

    pub fn from_manifest(manifest: &ExtensionManifest, root: &Path) -> Result<Self> {
        let entry_path = root.join(manifest.entrypoint.trim());
        if !entry_path.exists() {
            return Err(Error::validation(format!(
                "Extension entry does not exist: {}",
                entry_path.display()
            )));
        }

        let entry_path = entry_path
            .canonicalize()
            .unwrap_or_else(|_| entry_path.clone());

        if manifest.extension_id.trim().is_empty() {
            return Err(Error::validation(
                "Extension manifest extension_id is empty".to_string(),
            ));
        }

        Ok(Self {
            extension_id: manifest.extension_id.clone(),
            entry_path,
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            api_version: manifest.api_version.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct JsExtensionSnapshot {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    api_version: String,
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    slash_commands: Vec<Value>,
    #[serde(default)]
    providers: Vec<Value>,
    #[serde(default)]
    event_hooks: Vec<String>,
    #[serde(default)]
    active_tools: Option<Vec<String>>,
}

#[cfg(feature = "wasm-host")]
#[derive(Clone)]
pub struct WasmExtensionHandle {
    instance: Arc<AsyncMutex<wasm_host::Instance>>,
    registration: RegisterPayload,
    tool_defs: Vec<ExtensionToolDef>,
}

#[cfg(feature = "wasm-host")]
impl WasmExtensionHandle {
    fn new(instance: wasm_host::Instance, registration: RegisterPayload) -> Self {
        let tool_defs = parse_extension_tool_defs(&registration.tools);
        Self {
            instance: Arc::new(AsyncMutex::new(instance)),
            registration,
            tool_defs,
        }
    }

    pub fn tool_defs(&self) -> &[ExtensionToolDef] {
        &self.tool_defs
    }

    pub fn event_hooks(&self) -> &[String] {
        &self.registration.event_hooks
    }

    pub const fn registration(&self) -> &RegisterPayload {
        &self.registration
    }

    pub async fn handle_tool(&self, name: &str, input: &Value) -> Result<String> {
        let input_json = serde_json::to_string(input)
            .map_err(|err| Error::extension(format!("Serialize tool input: {err}")))?;
        let cx = Cx::for_request();
        let mut instance = self
            .instance
            .lock(&cx)
            .await
            .map_err(|err| Error::extension(format!("Lock wasm instance: {err}")))?;
        instance.handle_tool(name, &input_json).await
    }

    pub async fn handle_slash(
        &self,
        command: &str,
        args: &[String],
        input: &Value,
    ) -> Result<String> {
        let input_json = serde_json::to_string(input)
            .map_err(|err| Error::extension(format!("Serialize slash input: {err}")))?;
        let cx = Cx::for_request();
        let mut instance = self
            .instance
            .lock(&cx)
            .await
            .map_err(|err| Error::extension(format!("Lock wasm instance: {err}")))?;
        instance.handle_slash(command, args, &input_json).await
    }

    pub async fn handle_event_value(
        &self,
        event: &Value,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let event_json = serde_json::to_string(event)
            .map_err(|err| Error::extension(format!("Serialize event: {err}")))?;
        let cx = Cx::for_request();
        let fut = async {
            let mut instance = self
                .instance
                .lock(&cx)
                .await
                .map_err(|err| Error::extension(format!("Lock wasm instance: {err}")))?;
            instance.handle_event(&event_json).await
        };

        let response_json = if timeout_ms > 0 {
            match timeout(wall_now(), Duration::from_millis(timeout_ms), Box::pin(fut)).await {
                Ok(value) => value?,
                Err(_) => {
                    return Err(Error::extension(format!(
                        "WASM event timed out after {timeout_ms}ms"
                    )));
                }
            }
        } else {
            fut.await?
        };

        if response_json.trim().is_empty() {
            return Ok(None);
        }

        let value: Value = serde_json::from_str(&response_json)
            .map_err(|err| Error::extension(format!("Parse event response: {err}")))?;
        if value.is_null() {
            Ok(None)
        } else {
            Ok(Some(value))
        }
    }
}

fn parse_extension_tool_defs(tools: &[Value]) -> Vec<ExtensionToolDef> {
    let mut defs = Vec::new();
    for value in tools {
        match serde_json::from_value::<ExtensionToolDef>(value.clone()) {
            Ok(def) => defs.push(def),
            Err(err) => {
                tracing::warn!(error = %err, "Invalid extension tool definition; ignoring");
            }
        }
    }
    defs
}

#[derive(Clone)]
struct JsRuntimeHost {
    tools: Arc<ToolRegistry>,
    manager: ExtensionManager,
    http: Arc<HttpConnector>,
    policy: ExtensionPolicy,
}

#[derive(Debug)]
enum JsRuntimeCommand {
    LoadExtensions {
        specs: Vec<JsExtensionLoadSpec>,
        reply: oneshot::Sender<Result<Vec<JsExtensionSnapshot>>>,
    },
    GetRegisteredTools {
        reply: oneshot::Sender<Result<Vec<ExtensionToolDef>>>,
    },
    PumpOnce {
        reply: oneshot::Sender<Result<bool>>,
    },
    DispatchEvent {
        event_name: String,
        event_payload: Value,
        ctx_payload: Value,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<Value>>,
    },
    ExecuteTool {
        tool_name: String,
        tool_call_id: String,
        input: Value,
        ctx_payload: Value,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<Value>>,
    },
    ExecuteCommand {
        command_name: String,
        args: String,
        ctx_payload: Value,
        timeout_ms: u64,
        reply: oneshot::Sender<Result<Value>>,
    },
}

#[derive(Clone)]
pub struct JsExtensionRuntimeHandle {
    sender: mpsc::Sender<JsRuntimeCommand>,
}

impl JsExtensionRuntimeHandle {
    #[allow(clippy::too_many_lines)]
    pub async fn start(
        config: PiJsRuntimeConfig,
        tools: Arc<ToolRegistry>,
        manager: ExtensionManager,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel(32);
        let (init_tx, init_rx) = oneshot::channel();
        let host = JsRuntimeHost {
            tools,
            manager,
            http: Arc::new(HttpConnector::with_defaults()),
            policy: ExtensionPolicy::default(),
        };

        thread::spawn(move || {
            let runtime = RuntimeBuilder::current_thread()
                .build()
                .expect("extension runtime build");
            runtime.block_on(async move {
                let cx = Cx::for_request();
                let init =
                    PiJsRuntime::with_clock_and_config(crate::scheduler::WallClock, config).await;
                let js_runtime = match init {
                    Ok(runtime) => {
                        let _ = init_tx.send(&cx, Ok(()));
                        runtime
                    }
                    Err(err) => {
                        let _ = init_tx.send(&cx, Err(err));
                        return;
                    }
                };

                while let Ok(cmd) = rx.recv(&cx).await {
                    match cmd {
                        JsRuntimeCommand::LoadExtensions { specs, reply } => {
                            let result = load_all_extensions(&js_runtime, &host, &specs).await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::GetRegisteredTools { reply } => {
                            let result = js_runtime.get_registered_tools().await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::PumpOnce { reply } => {
                            let result = pump_js_runtime_once(&js_runtime, &host).await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::DispatchEvent {
                            event_name,
                            event_payload,
                            ctx_payload,
                            timeout_ms,
                            reply,
                        } => {
                            let result = dispatch_extension_event(
                                &js_runtime,
                                &host,
                                &event_name,
                                event_payload,
                                ctx_payload,
                                timeout_ms,
                            )
                            .await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::ExecuteTool {
                            tool_name,
                            tool_call_id,
                            input,
                            ctx_payload,
                            timeout_ms,
                            reply,
                        } => {
                            let result = execute_extension_tool(
                                &js_runtime,
                                &host,
                                &tool_name,
                                &tool_call_id,
                                input,
                                ctx_payload,
                                timeout_ms,
                            )
                            .await;
                            let _ = reply.send(&cx, result);
                        }
                        JsRuntimeCommand::ExecuteCommand {
                            command_name,
                            args,
                            ctx_payload,
                            timeout_ms,
                            reply,
                        } => {
                            let result = execute_extension_command(
                                &js_runtime,
                                &host,
                                &command_name,
                                &args,
                                ctx_payload,
                                timeout_ms,
                            )
                            .await;
                            let _ = reply.send(&cx, result);
                        }
                    }
                }
            });
        });

        let cx = Cx::for_request();
        init_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("JS extension runtime init cancelled"))??;

        Ok(Self { sender: tx })
    }

    async fn load_extensions_snapshots(
        &self,
        specs: Vec<JsExtensionLoadSpec>,
    ) -> Result<Vec<JsExtensionSnapshot>> {
        let cx = Cx::for_request();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(
                &cx,
                JsRuntimeCommand::LoadExtensions {
                    specs,
                    reply: reply_tx,
                },
            )
            .await
            .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
        reply_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
    }

    pub async fn get_registered_tools(&self) -> Result<Vec<ExtensionToolDef>> {
        let cx = Cx::for_request();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(
                &cx,
                JsRuntimeCommand::GetRegisteredTools { reply: reply_tx },
            )
            .await
            .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
        reply_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
    }

    pub async fn pump_once(&self) -> Result<bool> {
        let cx = Cx::for_request();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(&cx, JsRuntimeCommand::PumpOnce { reply: reply_tx })
            .await
            .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
        reply_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
    }

    pub async fn dispatch_event(
        &self,
        event_name: String,
        event_payload: Value,
        ctx_payload: Value,
        timeout_ms: u64,
    ) -> Result<Value> {
        let cx = Cx::for_request();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(
                &cx,
                JsRuntimeCommand::DispatchEvent {
                    event_name,
                    event_payload,
                    ctx_payload,
                    timeout_ms,
                    reply: reply_tx,
                },
            )
            .await
            .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
        reply_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
    }

    pub async fn execute_tool(
        &self,
        tool_name: String,
        tool_call_id: String,
        input: Value,
        ctx_payload: Value,
        timeout_ms: u64,
    ) -> Result<Value> {
        let cx = Cx::for_request();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(
                &cx,
                JsRuntimeCommand::ExecuteTool {
                    tool_name,
                    tool_call_id,
                    input,
                    ctx_payload,
                    timeout_ms,
                    reply: reply_tx,
                },
            )
            .await
            .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
        reply_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
    }

    pub async fn execute_command(
        &self,
        command_name: String,
        args: String,
        ctx_payload: Value,
        timeout_ms: u64,
    ) -> Result<Value> {
        let cx = Cx::for_request();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(
                &cx,
                JsRuntimeCommand::ExecuteCommand {
                    command_name,
                    args,
                    ctx_payload,
                    timeout_ms,
                    reply: reply_tx,
                },
            )
            .await
            .map_err(|_| Error::extension("JS extension runtime channel closed"))?;
        reply_rx
            .recv(&cx)
            .await
            .map_err(|_| Error::extension("JS extension runtime task cancelled"))?
    }
}

#[allow(clippy::future_not_send)]
async fn load_all_extensions(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    specs: &[JsExtensionLoadSpec],
) -> Result<Vec<JsExtensionSnapshot>> {
    for spec in specs {
        load_one_extension(runtime, host, spec).await?;
    }
    snapshot_extensions(runtime).await
}

#[allow(clippy::future_not_send)]
async fn load_one_extension(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    spec: &JsExtensionLoadSpec,
) -> Result<()> {
    let entry_specifier = spec.entry_path.display().to_string();
    let meta = json!({
        "name": spec.name,
        "version": spec.version,
        "apiVersion": spec.api_version,
    });
    let task_id = format!("task-load-{}", Uuid::new_v4());

    runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let load_fn: rquickjs::Function<'_> = global.get("__pi_load_extension")?;
            let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
            let meta_value = json_to_js(&ctx, &meta)?;
            let promise: rquickjs::Value<'_> =
                load_fn.call((spec.extension_id.clone(), entry_specifier, meta_value))?;
            let _task: String = task_start.call((task_id.clone(), promise))?;
            Ok(())
        })
        .await?;

    let _ = await_js_task(runtime, host, &task_id, Duration::from_secs(10)).await?;
    Ok(())
}

#[allow(clippy::future_not_send)]
async fn snapshot_extensions(runtime: &PiJsRuntime) -> Result<Vec<JsExtensionSnapshot>> {
    let json = runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let snapshot_fn: rquickjs::Function<'_> = global.get("__pi_snapshot_extensions")?;
            let value: rquickjs::Value<'_> = snapshot_fn.call(())?;
            js_to_json(&value)
        })
        .await?;

    let snapshots: Vec<JsExtensionSnapshot> =
        serde_json::from_value(json).map_err(|err| Error::extension(err.to_string()))?;
    Ok(snapshots)
}

#[allow(clippy::future_not_send)]
async fn dispatch_extension_event(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    event_name: &str,
    event_payload: Value,
    ctx_payload: Value,
    timeout_ms: u64,
) -> Result<Value> {
    let task_id = format!("task-event-{}", Uuid::new_v4());
    runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let dispatch_fn: rquickjs::Function<'_> =
                global.get("__pi_dispatch_extension_event")?;
            let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
            let event_js = json_to_js(&ctx, &event_payload)?;
            let ctx_js = json_to_js(&ctx, &ctx_payload)?;
            let promise: rquickjs::Value<'_> =
                dispatch_fn.call((event_name.to_string(), event_js, ctx_js))?;
            let _task: String = task_start.call((task_id.clone(), promise))?;
            Ok(())
        })
        .await?;

    await_js_task(runtime, host, &task_id, Duration::from_millis(timeout_ms)).await
}

#[allow(clippy::future_not_send)]
async fn execute_extension_tool(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    tool_name: &str,
    tool_call_id: &str,
    input: Value,
    ctx_payload: Value,
    timeout_ms: u64,
) -> Result<Value> {
    let task_id = format!("task-tool-{}", Uuid::new_v4());
    runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let exec_fn: rquickjs::Function<'_> = global.get("__pi_execute_tool")?;
            let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
            let input_js = json_to_js(&ctx, &input)?;
            let ctx_js = json_to_js(&ctx, &ctx_payload)?;
            let promise: rquickjs::Value<'_> = exec_fn.call((
                tool_name.to_string(),
                tool_call_id.to_string(),
                input_js,
                ctx_js,
            ))?;
            let _task: String = task_start.call((task_id.clone(), promise))?;
            Ok(())
        })
        .await?;

    await_js_task(runtime, host, &task_id, Duration::from_millis(timeout_ms)).await
}

#[allow(clippy::future_not_send)]
async fn execute_extension_command(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    command_name: &str,
    args: &str,
    ctx_payload: Value,
    timeout_ms: u64,
) -> Result<Value> {
    let task_id = format!("task-cmd-{}", Uuid::new_v4());
    runtime
        .with_ctx(|ctx| {
            let global = ctx.globals();
            let exec_fn: rquickjs::Function<'_> = global.get("__pi_execute_command")?;
            let task_start: rquickjs::Function<'_> = global.get("__pi_task_start")?;
            let ctx_js = json_to_js(&ctx, &ctx_payload)?;
            let promise: rquickjs::Value<'_> =
                exec_fn.call((command_name.to_string(), args.to_string(), ctx_js))?;
            let _task: String = task_start.call((task_id.clone(), promise))?;
            Ok(())
        })
        .await?;

    await_js_task(runtime, host, &task_id, Duration::from_millis(timeout_ms)).await
}

#[allow(clippy::future_not_send)]
async fn pump_js_runtime_once(runtime: &PiJsRuntime, host: &JsRuntimeHost) -> Result<bool> {
    let mut pending = runtime.drain_hostcall_requests();
    while let Some(req) = pending.pop_front() {
        let call_id = req.call_id.clone();
        let outcome = dispatch_hostcall(host, req).await;
        runtime.complete_hostcall(call_id, outcome);
    }

    let _ = runtime.tick().await?;
    let _ = runtime.drain_microtasks().await?;

    Ok(runtime.has_pending())
}

#[derive(Debug, Deserialize)]
struct JsTaskState {
    status: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    error: Option<JsTaskError>,
}

#[derive(Debug, Deserialize)]
struct JsTaskError {
    #[serde(default)]
    code: Option<String>,
    message: String,
    #[serde(default)]
    stack: Option<String>,
}

fn js_hostcall_timeout_ms(request: &HostcallRequest) -> Option<u64> {
    fn timeout_value(value: &Value) -> Option<u64> {
        value
            .get("timeout")
            .and_then(Value::as_u64)
            .or_else(|| value.get("timeoutMs").and_then(Value::as_u64))
            .or_else(|| value.get("timeout_ms").and_then(Value::as_u64))
            .filter(|ms| *ms > 0)
    }

    match request.kind {
        HostcallKind::Exec { .. } => request
            .payload
            .get("options")
            .and_then(timeout_value)
            .or_else(|| timeout_value(&request.payload)),
        HostcallKind::Http => timeout_value(&request.payload),
        _ => None,
    }
}

async fn prompt_capability_once(
    manager: &ExtensionManager,
    extension_id: &str,
    capability: &str,
) -> bool {
    let title = format!("Allow extension capability: {capability}");
    let message = format!("Extension {extension_id} requests capability '{capability}'. Allow?");
    let payload = json!({
        "title": title,
        "message": message,
        "extension_id": extension_id,
        "capability": capability,
    });
    let request = ExtensionUiRequest::new("", "confirm", payload);

    match manager.request_ui(request).await {
        Ok(Some(response)) => {
            response
                .value
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && !response.cancelled
        }
        Ok(None) | Err(_) => false,
    }
}

#[allow(clippy::future_not_send)]
async fn resolve_js_hostcall_policy_decision(
    host: &JsRuntimeHost,
    extension_id: Option<&str>,
    required: &str,
) -> (PolicyDecision, String, String) {
    const UNKNOWN_EXTENSION_ID: &str = "<unknown>";
    let PolicyCheck {
        mut decision,
        capability,
        mut reason,
    } = host.policy.evaluate(required);

    if decision != PolicyDecision::Prompt {
        return (decision, reason, capability);
    }

    if let Some(extension_id) = extension_id {
        if let Some(allow) = host
            .manager
            .cached_policy_prompt_decision(extension_id, &capability)
        {
            decision = if allow {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Deny
            };
            reason = if allow {
                "prompt_cache_allow".to_string()
            } else {
                "prompt_cache_deny".to_string()
            };
            return (decision, reason, capability);
        }
    }

    let prompt_extension_id = extension_id.unwrap_or(UNKNOWN_EXTENSION_ID);
    let allow = prompt_capability_once(&host.manager, prompt_extension_id, &capability).await;
    if let Some(extension_id) = extension_id {
        host.manager
            .cache_policy_prompt_decision(extension_id, &capability, allow);
    }
    decision = if allow {
        PolicyDecision::Allow
    } else {
        PolicyDecision::Deny
    };
    reason = if allow {
        "prompt_user_allow".to_string()
    } else {
        "prompt_user_deny".to_string()
    };
    (decision, reason, capability)
}

fn log_js_hostcall_start(
    call_id: &str,
    extension_id: Option<&str>,
    required: &str,
    method: &str,
    params_hash: &str,
    call_timeout_ms: Option<u64>,
) {
    tracing::info!(
        event = "host_call.start",
        runtime = "js",
        call_id = %call_id,
        extension_id = ?extension_id,
        capability = %required,
        method = %method,
        params_hash = %params_hash,
        timeout_ms = call_timeout_ms,
        "Hostcall start"
    );
}

fn log_js_policy_decision(
    call_id: &str,
    extension_id: Option<&str>,
    capability: &str,
    decision: &PolicyDecision,
    reason: &str,
    params_hash: &str,
) {
    if *decision == PolicyDecision::Allow {
        tracing::info!(
            event = "policy.decision",
            runtime = "js",
            call_id = %call_id,
            extension_id = ?extension_id,
            capability = %capability,
            decision = ?decision,
            reason = %reason,
            params_hash = %params_hash,
            "Hostcall allowed by policy"
        );
    } else {
        tracing::warn!(
            event = "policy.decision",
            runtime = "js",
            call_id = %call_id,
            extension_id = ?extension_id,
            capability = %capability,
            decision = ?decision,
            reason = %reason,
            params_hash = %params_hash,
            "Hostcall denied by policy"
        );
    }
}

fn log_js_hostcall_end(
    call_id: &str,
    extension_id: Option<&str>,
    required: &str,
    method: &str,
    params_hash: &str,
    duration_ms: u64,
    outcome: &HostcallOutcome,
) {
    let (is_error, error_code) = match outcome {
        HostcallOutcome::Success(_) => (false, None),
        HostcallOutcome::Error { code, .. } => (true, Some(code.as_str())),
    };

    if is_error {
        tracing::warn!(
            event = "host_call.end",
            runtime = "js",
            call_id = %call_id,
            extension_id = ?extension_id,
            capability = %required,
            method = %method,
            params_hash = %params_hash,
            duration_ms,
            error_code = error_code,
            "Hostcall end (error)"
        );
    } else {
        tracing::info!(
            event = "host_call.end",
            runtime = "js",
            call_id = %call_id,
            extension_id = ?extension_id,
            capability = %required,
            method = %method,
            params_hash = %params_hash,
            duration_ms,
            "Hostcall end (success)"
        );
    }
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_allowed(
    host: &JsRuntimeHost,
    request: HostcallRequest,
) -> HostcallOutcome {
    let HostcallRequest {
        call_id,
        kind,
        payload,
        ..
    } = request;

    match (kind, payload) {
        (HostcallKind::Tool { name }, payload) => {
            dispatch_hostcall_tool(&host.tools, &call_id, &name, payload).await
        }
        (HostcallKind::Exec { cmd }, payload) => {
            dispatch_hostcall_exec(&call_id, &cmd, payload).await
        }
        (HostcallKind::Http, payload) => {
            dispatch_hostcall_http(&call_id, &host.http, payload).await
        }
        (HostcallKind::Session { op }, payload) => {
            dispatch_hostcall_session(&call_id, &host.manager, &op, payload).await
        }
        (HostcallKind::Ui { op }, payload) => {
            dispatch_hostcall_ui(&call_id, &host.manager, &op, payload).await
        }
        (HostcallKind::Events { op }, payload) => {
            dispatch_hostcall_events(&call_id, &host.manager, &op, payload).await
        }
    }
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall(host: &JsRuntimeHost, request: HostcallRequest) -> HostcallOutcome {
    let call_id = request.call_id.clone();
    let extension_id = request.extension_id.clone();
    let method = request.method();
    let required = request.required_capability();
    let params_hash = request.params_hash();
    let call_timeout_ms = js_hostcall_timeout_ms(&request);
    let started_at = Instant::now();

    log_js_hostcall_start(
        &call_id,
        extension_id.as_deref(),
        &required,
        method,
        &params_hash,
        call_timeout_ms,
    );

    let (decision, reason, capability) =
        resolve_js_hostcall_policy_decision(host, extension_id.as_deref(), &required).await;
    log_js_policy_decision(
        &call_id,
        extension_id.as_deref(),
        &capability,
        &decision,
        &reason,
        &params_hash,
    );

    let outcome = if decision == PolicyDecision::Allow {
        dispatch_hostcall_allowed(host, request).await
    } else {
        HostcallOutcome::Error {
            code: "denied".to_string(),
            message: format!("Capability '{capability}' denied by policy ({reason})"),
        }
    };

    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    log_js_hostcall_end(
        &call_id,
        extension_id.as_deref(),
        &required,
        method,
        &params_hash,
        duration_ms,
        &outcome,
    );

    outcome
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_tool(
    tools: &ToolRegistry,
    call_id: &str,
    name: &str,
    payload: Value,
) -> HostcallOutcome {
    let Some(tool) = tools.get(name) else {
        return HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: format!("Unknown tool: {name}"),
        };
    };

    match tool.execute(call_id, payload, None).await {
        Ok(output) => match serde_json::to_value(output) {
            Ok(value) => HostcallOutcome::Success(value),
            Err(err) => HostcallOutcome::Error {
                code: "internal".to_string(),
                message: format!("Serialize tool output: {err}"),
            },
        },
        Err(err) => HostcallOutcome::Error {
            code: "tool_error".to_string(),
            message: err.to_string(),
        },
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_hostcall_exec(_call_id: &str, cmd: &str, payload: Value) -> HostcallOutcome {
    let args_value = payload.get("args").cloned().unwrap_or(Value::Null);
    let args_array = match args_value {
        Value::Null => Vec::new(),
        Value::Array(items) => items,
        _ => {
            return HostcallOutcome::Error {
                code: "invalid_request".to_string(),
                message: "exec args must be an array".to_string(),
            };
        }
    };

    let args = args_array
        .iter()
        .map(|v| {
            v.as_str()
                .map_or_else(|| v.to_string(), ToString::to_string)
        })
        .collect::<Vec<_>>();

    let options = payload.get("options").cloned().unwrap_or_else(|| json!({}));
    let cwd = options
        .get("cwd")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let timeout_ms = options
        .get("timeout")
        .and_then(Value::as_u64)
        .or_else(|| options.get("timeoutMs").and_then(Value::as_u64))
        .or_else(|| options.get("timeout_ms").and_then(Value::as_u64))
        .filter(|ms| *ms > 0);

    let cmd = cmd.to_string();
    let (tx, rx) = oneshot::channel();
    thread::spawn(move || {
        let result: std::result::Result<Value, String> = (|| {
            let mut command = Command::new(&cmd);
            command
                .args(&args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            if let Some(cwd) = cwd.as_ref() {
                command.current_dir(cwd);
            }

            let mut child = command.spawn().map_err(|err| err.to_string())?;
            let pid = child.id();

            let mut stdout = child.stdout.take().ok_or("Missing stdout pipe")?;
            let mut stderr = child.stderr.take().ok_or("Missing stderr pipe")?;

            let stdout_handle = thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = stdout.read_to_end(&mut buf);
                buf
            });
            let stderr_handle = thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = stderr.read_to_end(&mut buf);
                buf
            });

            let start = Instant::now();
            let mut killed = false;
            let status = loop {
                if let Some(status) = child.try_wait().map_err(|err| err.to_string())? {
                    break status;
                }

                if let Some(timeout_ms) = timeout_ms {
                    if start.elapsed() >= Duration::from_millis(timeout_ms) {
                        killed = true;
                        crate::tools::kill_process_tree(Some(pid));
                        let _ = child.kill();
                        break child.wait().map_err(|err| err.to_string())?;
                    }
                }

                thread::sleep(Duration::from_millis(10));
            };

            let stdout_bytes = stdout_handle.join().unwrap_or_else(|_| Vec::new());
            let stderr_bytes = stderr_handle.join().unwrap_or_else(|_| Vec::new());

            let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
            let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
            let code = status.code().unwrap_or(0);

            Ok(json!({
                "stdout": stdout,
                "stderr": stderr,
                "code": code,
                "killed": killed,
            }))
        })();

        let cx = Cx::for_request();
        let _ = tx.send(&cx, result);
    });

    let cx = Cx::for_request();
    match rx.recv(&cx).await {
        Ok(Ok(value)) => HostcallOutcome::Success(value),
        Ok(Err(err)) => HostcallOutcome::Error {
            code: "io".to_string(),
            message: err,
        },
        Err(_) => HostcallOutcome::Error {
            code: "internal".to_string(),
            message: "exec task cancelled".to_string(),
        },
    }
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_http(
    call_id: &str,
    connector: &HttpConnector,
    payload: Value,
) -> HostcallOutcome {
    let call = crate::connectors::HostCallPayload {
        call_id: call_id.to_string(),
        capability: "http".to_string(),
        method: "http".to_string(),
        params: payload,
        timeout_ms: None,
        cancel_token: None,
        context: None,
    };

    match connector.dispatch(&call).await {
        Ok(result) => {
            if result.is_error {
                let message = result.error.as_ref().map_or_else(
                    || "HTTP connector error".to_string(),
                    |err| err.message.clone(),
                );
                let code = result
                    .error
                    .as_ref()
                    .map_or("internal", |err| hostcall_code_to_str(err.code));
                HostcallOutcome::Error {
                    code: code.to_string(),
                    message,
                }
            } else {
                HostcallOutcome::Success(result.output)
            }
        }
        Err(err) => HostcallOutcome::Error {
            code: "internal".to_string(),
            message: err.to_string(),
        },
    }
}

const fn hostcall_code_to_str(code: crate::connectors::HostCallErrorCode) -> &'static str {
    match code {
        crate::connectors::HostCallErrorCode::Timeout => "timeout",
        crate::connectors::HostCallErrorCode::Denied => "denied",
        crate::connectors::HostCallErrorCode::Io => "io",
        crate::connectors::HostCallErrorCode::InvalidRequest => "invalid_request",
        crate::connectors::HostCallErrorCode::Internal => "internal",
    }
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_session(
    _call_id: &str,
    manager: &ExtensionManager,
    op: &str,
    payload: Value,
) -> HostcallOutcome {
    let Some(session) = manager.session_handle() else {
        return HostcallOutcome::Error {
            code: "denied".to_string(),
            message: "No session configured".to_string(),
        };
    };

    let op_norm = op.trim().to_ascii_lowercase();
    let result = match op_norm.as_str() {
        "get_state" | "getstate" => Ok(session.get_state().await),
        "get_messages" | "getmessages" => serde_json::to_value(session.get_messages().await)
            .map_err(|err| Error::extension(format!("Serialize messages: {err}"))),
        "get_entries" | "getentries" => serde_json::to_value(session.get_entries().await)
            .map_err(|err| Error::extension(format!("Serialize entries: {err}"))),
        "get_branch" | "getbranch" => serde_json::to_value(session.get_branch().await)
            .map_err(|err| Error::extension(format!("Serialize branch: {err}"))),
        "get_file" | "getfile" => {
            let state = session.get_state().await;
            let file = state
                .get("sessionFile")
                .or_else(|| state.get("session_file"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(file)
        }
        "get_name" | "getname" => {
            let state = session.get_state().await;
            let name = state
                .get("sessionName")
                .or_else(|| state.get("session_name"))
                .cloned()
                .unwrap_or(Value::Null);
            Ok(name)
        }
        "set_name" | "setname" => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            session.set_name(name).await.map(|()| Value::Null)
        }
        "append_message" | "appendmessage" => {
            let message_value = payload.get("message").cloned().unwrap_or(payload);
            match serde_json::from_value(message_value) {
                Ok(message) => session.append_message(message).await.map(|()| Value::Null),
                Err(err) => Err(Error::extension(format!("Parse message: {err}"))),
            }
        }
        "append_entry" | "appendentry" => {
            let custom_type = payload
                .get("customType")
                .and_then(Value::as_str)
                .or_else(|| payload.get("custom_type").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let data = payload.get("data").cloned();
            session
                .append_custom_entry(custom_type, data)
                .await
                .map(|()| Value::Null)
        }
        _ => Err(Error::extension(format!("Unknown session op: {op}"))),
    };

    match result {
        Ok(value) => HostcallOutcome::Success(value),
        Err(err) => HostcallOutcome::Error {
            code: "invalid_request".to_string(),
            message: err.to_string(),
        },
    }
}

#[allow(clippy::future_not_send)]
async fn dispatch_hostcall_ui(
    call_id: &str,
    manager: &ExtensionManager,
    op: &str,
    payload: Value,
) -> HostcallOutcome {
    let request = ExtensionUiRequest {
        id: call_id.to_string(),
        method: op.to_string(),
        payload,
        timeout_ms: None,
    };

    match manager.request_ui(request).await {
        Ok(Some(response)) => HostcallOutcome::Success(response.value.unwrap_or(Value::Null)),
        Ok(None) => HostcallOutcome::Success(Value::Null),
        Err(err) => HostcallOutcome::Error {
            code: "io".to_string(),
            message: err.to_string(),
        },
    }
}

#[allow(clippy::future_not_send, clippy::too_many_lines)]
async fn dispatch_hostcall_events(
    _call_id: &str,
    manager: &ExtensionManager,
    op: &str,
    payload: Value,
) -> HostcallOutcome {
    let op_norm = op.trim().to_ascii_lowercase();
    match op_norm.as_str() {
        "setactivetools" | "set_active_tools" => {
            let tools = payload
                .get("tools")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            manager.set_active_tools(tools);
            HostcallOutcome::Success(Value::Null)
        }
        "appendentry" | "append_entry" => {
            let Some(session) = manager.session_handle() else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "No session configured".to_string(),
                };
            };
            let custom_type = payload
                .get("customType")
                .and_then(Value::as_str)
                .or_else(|| payload.get("custom_type").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let data = payload.get("data").cloned();
            match session.append_custom_entry(custom_type, data).await {
                Ok(()) => HostcallOutcome::Success(Value::Null),
                Err(err) => HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err.to_string(),
                },
            }
        }
        "sendmessage" | "send_message" => {
            let Some(actions) = manager.host_actions() else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "No host actions configured".to_string(),
                };
            };

            let extension_id = payload
                .get("extensionId")
                .and_then(Value::as_str)
                .or_else(|| payload.get("extension_id").and_then(Value::as_str))
                .map(ToString::to_string);

            let message = payload.get("message").cloned().unwrap_or(Value::Null);
            let options = payload.get("options").cloned().unwrap_or(Value::Null);

            let custom_type = message
                .get("customType")
                .and_then(Value::as_str)
                .or_else(|| message.get("custom_type").and_then(Value::as_str))
                .unwrap_or_default()
                .trim()
                .to_string();
            if custom_type.is_empty() {
                return HostcallOutcome::Error {
                    code: "invalid_request".to_string(),
                    message: "sendMessage: message.customType is required".to_string(),
                };
            }

            let display = message
                .get("display")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let details = message.get("details").cloned();

            let content = match message.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => {
                    serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string())
                }
                None => String::new(),
            };

            let deliver_as = ExtensionDeliverAs::parse(
                options
                    .get("deliverAs")
                    .and_then(Value::as_str)
                    .or_else(|| options.get("deliver_as").and_then(Value::as_str)),
            );
            let trigger_turn = options
                .get("triggerTurn")
                .and_then(Value::as_bool)
                .or_else(|| options.get("trigger_turn").and_then(Value::as_bool))
                .unwrap_or(false);

            let msg = ExtensionSendMessage {
                extension_id,
                custom_type,
                content,
                display,
                details,
                deliver_as,
                trigger_turn,
            };

            match actions.send_message(msg).await {
                Ok(()) => HostcallOutcome::Success(Value::Null),
                Err(err) => HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err.to_string(),
                },
            }
        }
        "sendusermessage" | "send_user_message" => {
            let Some(actions) = manager.host_actions() else {
                return HostcallOutcome::Error {
                    code: "denied".to_string(),
                    message: "No host actions configured".to_string(),
                };
            };

            let extension_id = payload
                .get("extensionId")
                .and_then(Value::as_str)
                .or_else(|| payload.get("extension_id").and_then(Value::as_str))
                .map(ToString::to_string);

            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if text.is_empty() {
                return HostcallOutcome::Success(Value::Null);
            }

            let options = payload.get("options").cloned().unwrap_or(Value::Null);
            let deliver_as = ExtensionDeliverAs::parse(
                options
                    .get("deliverAs")
                    .and_then(Value::as_str)
                    .or_else(|| options.get("deliver_as").and_then(Value::as_str)),
            );

            let msg = ExtensionSendUserMessage {
                extension_id,
                text,
                deliver_as,
            };

            match actions.send_user_message(msg).await {
                Ok(()) => HostcallOutcome::Success(Value::Null),
                Err(err) => HostcallOutcome::Error {
                    code: "io".to_string(),
                    message: err.to_string(),
                },
            }
        }
        _ => HostcallOutcome::Success(Value::Null),
    }
}

#[allow(clippy::future_not_send)]
async fn await_js_task(
    runtime: &PiJsRuntime,
    host: &JsRuntimeHost,
    task_id: &str,
    timeout: Duration,
) -> Result<Value> {
    let start = Instant::now();

    loop {
        if start.elapsed() > timeout {
            return Err(Error::extension(format!(
                "JS task timed out after {}ms",
                timeout.as_millis()
            )));
        }

        let _has_pending = pump_js_runtime_once(runtime, host).await?;

        let state_json = runtime
            .with_ctx(|ctx| {
                let global = ctx.globals();
                let take_fn: rquickjs::Function<'_> = global.get("__pi_task_take")?;
                let value: rquickjs::Value<'_> = take_fn.call((task_id.to_string(),))?;
                js_to_json(&value)
            })
            .await?;

        if state_json.is_null() {
            return Err(Error::extension("JS task state missing".to_string()));
        }

        let state: JsTaskState =
            serde_json::from_value(state_json).map_err(|err| Error::extension(err.to_string()))?;

        match state.status.as_str() {
            "pending" => {
                if !runtime.has_pending() {
                    sleep(wall_now(), Duration::from_millis(1)).await;
                }
            }
            "resolved" => return Ok(state.value.unwrap_or(Value::Null)),
            "rejected" => {
                let err = state.error.unwrap_or_else(|| JsTaskError {
                    code: None,
                    message: "Unknown JS task error".to_string(),
                    stack: None,
                });
                let mut message = err.message;
                if let Some(code) = err.code {
                    message = format!("{code}: {message}");
                }
                if let Some(stack) = err.stack {
                    if !stack.is_empty() {
                        message.push('\n');
                        message.push_str(&stack);
                    }
                }
                return Err(Error::extension(message));
            }
            other => {
                return Err(Error::extension(format!(
                    "Unexpected JS task status: {other}"
                )));
            }
        }

        sleep(wall_now(), Duration::from_millis(0)).await;
    }
}

/// Extension manager for handling loaded extensions.
#[derive(Clone)]
pub struct ExtensionManager {
    inner: Arc<Mutex<ExtensionManagerInner>>,
}

#[cfg(feature = "wasm-host")]
#[derive(Clone, Default)]
struct ExtensionManagerHandle {
    inner: Weak<Mutex<ExtensionManagerInner>>,
}

#[cfg(feature = "wasm-host")]
impl ExtensionManagerHandle {
    fn new(manager: &ExtensionManager) -> Self {
        Self {
            inner: Arc::downgrade(&manager.inner),
        }
    }

    fn upgrade(&self) -> Option<ExtensionManager> {
        self.inner.upgrade().map(|inner| ExtensionManager { inner })
    }
}

#[derive(Default)]
struct ExtensionManagerInner {
    extensions: Vec<RegisterPayload>,
    js_runtime: Option<JsExtensionRuntimeHandle>,
    #[cfg(feature = "wasm-host")]
    wasm_extensions: Vec<WasmExtensionHandle>,
    ui_sender: Option<mpsc::Sender<ExtensionUiRequest>>,
    pending_ui: HashMap<String, oneshot::Sender<ExtensionUiResponse>>,
    session: Option<Arc<dyn ExtensionSession>>,
    active_tools: Option<Vec<String>>,
    providers: Vec<Value>,
    cwd: Option<String>,
    model_registry_values: HashMap<String, String>,
    host_actions: Option<Arc<dyn ExtensionHostActions>>,
    policy_prompt_cache: HashMap<String, HashMap<String, bool>>,
}

impl std::fmt::Debug for ExtensionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionManager").finish_non_exhaustive()
    }
}

impl Default for ExtensionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionManager {
    /// Create a new extension manager.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ExtensionManagerInner::default())),
        }
    }

    pub fn set_ui_sender(&self, sender: mpsc::Sender<ExtensionUiRequest>) {
        let mut guard = self.inner.lock().unwrap();
        guard.ui_sender = Some(sender);
    }

    pub fn clear_ui_sender(&self) {
        let mut guard = self.inner.lock().unwrap();
        guard.ui_sender = None;
    }

    pub fn set_js_runtime(&self, runtime: JsExtensionRuntimeHandle) {
        let mut guard = self.inner.lock().unwrap();
        guard.js_runtime = Some(runtime);
    }

    pub fn set_cwd(&self, cwd: String) {
        let mut guard = self.inner.lock().unwrap();
        guard.cwd = Some(cwd);
    }

    pub fn set_model_registry_values(&self, values: HashMap<String, String>) {
        let mut guard = self.inner.lock().unwrap();
        guard.model_registry_values = values;
    }

    #[cfg(feature = "wasm-host")]
    fn handle(&self) -> ExtensionManagerHandle {
        ExtensionManagerHandle::new(self)
    }

    pub fn set_host_actions(&self, actions: Arc<dyn ExtensionHostActions>) {
        let mut guard = self.inner.lock().unwrap();
        guard.host_actions = Some(actions);
    }

    pub fn js_runtime(&self) -> Option<JsExtensionRuntimeHandle> {
        let guard = self.inner.lock().unwrap();
        guard.js_runtime.clone()
    }

    fn host_actions(&self) -> Option<Arc<dyn ExtensionHostActions>> {
        let guard = self.inner.lock().unwrap();
        guard.host_actions.clone()
    }

    fn cached_policy_prompt_decision(&self, extension_id: &str, capability: &str) -> Option<bool> {
        let guard = self.inner.lock().unwrap();
        guard
            .policy_prompt_cache
            .get(extension_id)
            .and_then(|by_cap| by_cap.get(capability))
            .copied()
    }

    fn cache_policy_prompt_decision(&self, extension_id: &str, capability: &str, allow: bool) {
        let mut guard = self.inner.lock().unwrap();
        guard
            .policy_prompt_cache
            .entry(extension_id.to_string())
            .or_default()
            .insert(capability.to_string(), allow);
    }

    pub fn active_tools(&self) -> Option<Vec<String>> {
        let guard = self.inner.lock().unwrap();
        guard.active_tools.clone()
    }

    pub async fn load_js_extensions(&self, specs: Vec<JsExtensionLoadSpec>) -> Result<()> {
        let runtime = self
            .js_runtime()
            .ok_or_else(|| Error::extension("JS extension runtime not configured"))?;

        let snapshots = runtime.load_extensions_snapshots(specs).await?;

        let mut payloads = Vec::new();
        let mut active_tools: Option<Vec<String>> = None;
        for snapshot in snapshots {
            let JsExtensionSnapshot {
                id,
                name,
                version,
                api_version,
                tools,
                slash_commands,
                providers,
                event_hooks,
                active_tools: ext_active_tools,
            } = snapshot;
            let _ = providers;
            if let Some(list) = ext_active_tools {
                active_tools = Some(list);
            }
            payloads.push(RegisterPayload {
                name: if name.is_empty() { id } else { name },
                version,
                api_version: if api_version.is_empty() {
                    PROTOCOL_VERSION.to_string()
                } else {
                    api_version
                },
                capabilities: Vec::new(),
                capability_manifest: None,
                tools,
                slash_commands,
                event_hooks,
            });
        }

        {
            let mut guard = self.inner.lock().unwrap();
            guard.extensions = payloads;
            guard.active_tools = active_tools;
        }
        Ok(())
    }

    #[cfg(feature = "wasm-host")]
    pub async fn load_wasm_extensions(
        &self,
        host: &WasmExtensionHost,
        specs: Vec<WasmExtensionLoadSpec>,
        tools: Arc<ToolRegistry>,
    ) -> Result<()> {
        let mut wasm_handles = Vec::new();
        let mut registrations = Vec::new();

        for spec in specs {
            let extension = host.load_from_path(&spec.entry_path)?;
            let mut instance = host
                .instantiate_with(&extension, Arc::clone(&tools), Some(self.handle()))
                .await?;

            let registration_json = instance.init(&spec.manifest_json).await?;
            let mut registration: RegisterPayload = serde_json::from_str(&registration_json)
                .map_err(|err| {
                    Error::extension(format!(
                        "WASM init returned invalid registration payload: {err}"
                    ))
                })?;
            if registration.capability_manifest.is_none() {
                registration
                    .capability_manifest
                    .clone_from(&spec.manifest.capability_manifest);
            }
            validate_register(&registration)?;

            wasm_handles.push(WasmExtensionHandle::new(instance, registration.clone()));
            registrations.push(registration);
        }

        {
            let mut guard = self.inner.lock().unwrap();
            guard.extensions.extend(registrations);
            guard.wasm_extensions.extend(wasm_handles);
        }
        Ok(())
    }

    #[cfg(feature = "wasm-host")]
    pub fn wasm_extensions(&self) -> Vec<WasmExtensionHandle> {
        let guard = self.inner.lock().unwrap();
        guard.wasm_extensions.clone()
    }

    pub fn set_session(&self, session: Arc<dyn ExtensionSession>) {
        let mut guard = self.inner.lock().unwrap();
        guard.session = Some(session);
    }

    pub fn session_handle(&self) -> Option<Arc<dyn ExtensionSession>> {
        let guard = self.inner.lock().unwrap();
        guard.session.clone()
    }

    pub fn set_active_tools(&self, tools: Vec<String>) {
        let mut guard = self.inner.lock().unwrap();
        guard.active_tools = Some(tools);
    }

    pub fn register(&self, payload: RegisterPayload) {
        let mut guard = self.inner.lock().unwrap();
        guard.extensions.push(payload);
    }

    pub fn has_command(&self, name: &str) -> bool {
        let needle = normalize_command(name);
        let guard = self.inner.lock().unwrap();
        guard
            .extensions
            .iter()
            .flat_map(|ext| ext.slash_commands.iter())
            .filter_map(extract_slash_command_name)
            .any(|cmd| normalize_command(&cmd) == needle)
    }

    pub fn list_commands(&self) -> Vec<Value> {
        let guard = self.inner.lock().unwrap();
        let mut commands = Vec::new();

        for ext in &guard.extensions {
            for cmd in &ext.slash_commands {
                let Some(name) = extract_slash_command_name(cmd) else {
                    continue;
                };
                let description = cmd.get("description").and_then(Value::as_str);
                commands.push(json!({
                    "name": name,
                    "description": description,
                    "source": "extension",
                }));
            }
        }

        drop(guard);
        commands
    }

    pub async fn request_ui(
        &self,
        mut request: ExtensionUiRequest,
    ) -> Result<Option<ExtensionUiResponse>> {
        let cx = Cx::for_request();
        if request.id.trim().is_empty() {
            request.id = Uuid::new_v4().to_string();
        }

        let (ui_sender, expects_response) = {
            let guard = self.inner.lock().unwrap();
            (guard.ui_sender.clone(), request.expects_response())
        };

        let Some(ui_sender) = ui_sender else {
            return Err(Error::extension("Extension UI sender not configured"));
        };

        if !expects_response {
            ui_sender
                .send(&cx, request)
                .await
                .map_err(|_| Error::extension("Extension UI channel closed"))?;
            return Ok(None);
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.inner.lock().unwrap();
            guard.pending_ui.insert(request.id.clone(), tx);
        }

        if ui_sender.send(&cx, request.clone()).await.is_err() {
            self.inner.lock().unwrap().pending_ui.remove(&request.id);
            return Err(Error::extension("Extension UI channel closed"));
        }

        let response = if let Some(timeout_ms) = request.effective_timeout_ms() {
            match timeout(wall_now(), Duration::from_millis(timeout_ms), rx.recv(&cx)).await {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(_)) => Err(Error::extension("Extension UI response dropped")),
                Err(_) => Err(Error::extension("Extension UI request timed out")),
            }
        } else {
            rx.recv(&cx)
                .await
                .map_err(|_| Error::extension("Extension UI response dropped"))
        };

        match response {
            Ok(resp) => Ok(Some(resp)),
            Err(err) => {
                self.inner.lock().unwrap().pending_ui.remove(&request.id);
                Err(err)
            }
        }
    }

    pub fn respond_ui(&self, response: ExtensionUiResponse) -> bool {
        let cx = Cx::for_request();
        let tx = {
            let mut guard = self.inner.lock().unwrap();
            guard.pending_ui.remove(&response.id)
        };
        tx.is_some_and(|sender| sender.send(&cx, response).is_ok())
    }

    #[allow(clippy::too_many_lines)]
    #[allow(clippy::too_many_lines)]
    async fn dispatch_event_value(
        &self,
        event: ExtensionEventName,
        data: Option<Value>,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let event_name = event.to_string();
        let (runtime, has_ui, session, cwd_override, model_registry_values, has_hook) = {
            let guard = self.inner.lock().unwrap();
            let has_hook = guard
                .extensions
                .iter()
                .any(|ext| ext.event_hooks.iter().any(|hook| hook == &event_name));
            (
                guard.js_runtime.clone(),
                guard.ui_sender.is_some(),
                guard.session.clone(),
                guard.cwd.clone(),
                guard.model_registry_values.clone(),
                has_hook,
            )
        };

        #[cfg(feature = "wasm-host")]
        let (wasm_extensions, has_hook_wasm) = {
            let guard = self.inner.lock().unwrap();
            let has_hook_wasm = guard
                .wasm_extensions
                .iter()
                .any(|ext| ext.event_hooks().iter().any(|hook| hook == &event_name));
            (guard.wasm_extensions.clone(), has_hook_wasm)
        };

        let has_any_hook = {
            #[cfg(feature = "wasm-host")]
            {
                has_hook || has_hook_wasm
            }
            #[cfg(not(feature = "wasm-host"))]
            {
                has_hook
            }
        };

        if !has_any_hook {
            return Ok(None);
        }

        let mut ctx = serde_json::Map::new();
        ctx.insert("hasUI".to_string(), Value::Bool(has_ui));
        if let Some(cwd) = cwd_override.or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        }) {
            ctx.insert("cwd".to_string(), Value::String(cwd));
        }

        if !model_registry_values.is_empty() {
            let mut map = serde_json::Map::new();
            for (key, value) in model_registry_values {
                map.insert(key, Value::String(value));
            }
            ctx.insert("modelRegistry".to_string(), Value::Object(map));
        }

        if let Some(session) = session {
            let state = session.get_state().await;
            let entries = session.get_entries().await;
            let branch = session.get_branch().await;
            let leaf_entry = entries.last().cloned().unwrap_or(Value::Null);
            ctx.insert("sessionState".to_string(), state);
            ctx.insert("sessionEntries".to_string(), Value::Array(entries));
            ctx.insert("sessionBranch".to_string(), Value::Array(branch));
            ctx.insert("sessionLeafEntry".to_string(), leaf_entry);
        }

        let event_payload = match data {
            None => json!({ "type": event_name }),
            Some(Value::Object(mut map)) => {
                map.insert("type".to_string(), Value::String(event_name.clone()));
                Value::Object(map)
            }
            Some(other) => json!({ "type": event_name, "data": other }),
        };

        let ctx_payload = Value::Object(ctx);

        let mut response = None;
        if let Some(runtime) = runtime {
            if has_hook {
                let js_response = runtime
                    .dispatch_event(
                        event_name.clone(),
                        event_payload.clone(),
                        ctx_payload.clone(),
                        timeout_ms,
                    )
                    .await?;
                response = Some(js_response);
            }
        }

        #[cfg(feature = "wasm-host")]
        if has_hook_wasm {
            let mut wasm_payload = event_payload;
            if let Value::Object(map) = &mut wasm_payload {
                map.insert("ctx".to_string(), ctx_payload);
            }
            if let Some(value) = Self::dispatch_wasm_event_value(
                &wasm_extensions,
                &event_name,
                &wasm_payload,
                timeout_ms,
            )
            .await?
            {
                response = Some(value);
            }
        }

        Ok(response)
    }

    #[cfg(feature = "wasm-host")]
    async fn dispatch_wasm_event_value(
        extensions: &[WasmExtensionHandle],
        event_name: &str,
        event_payload: &Value,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        let mut response = None;
        for ext in extensions {
            if !ext.event_hooks().iter().any(|hook| hook == event_name) {
                continue;
            }
            if let Some(value) = ext.handle_event_value(event_payload, timeout_ms).await? {
                response = Some(value);
            }
        }
        Ok(response)
    }

    /// Dispatch an event to all registered extensions.
    pub async fn dispatch_event(
        &self,
        event: ExtensionEventName,
        data: Option<Value>,
    ) -> Result<()> {
        let _ = self
            .dispatch_event_value(event, data, EXTENSION_EVENT_TIMEOUT_MS)
            .await?;
        Ok(())
    }

    /// Dispatch an event to all registered extensions and return the raw response (if any).
    pub async fn dispatch_event_with_response(
        &self,
        event: ExtensionEventName,
        data: Option<Value>,
        timeout_ms: u64,
    ) -> Result<Option<Value>> {
        self.dispatch_event_value(event, data, timeout_ms).await
    }

    /// Dispatch a cancellable event to all registered extensions.
    pub async fn dispatch_cancellable_event(
        &self,
        event: ExtensionEventName,
        data: Option<Value>,
        timeout_ms: u64,
    ) -> Result<bool> {
        let Some(response) = self.dispatch_event_value(event, data, timeout_ms).await? else {
            return Ok(false);
        };

        Ok(response.as_bool() == Some(false)
            || response
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || response
                .get("cancel")
                .and_then(Value::as_bool)
                .unwrap_or(false))
    }

    /// Dispatch a `tool_call` event to registered extensions and return the first
    /// blocking response (if any).
    #[allow(clippy::too_many_lines)]
    pub async fn dispatch_tool_call(
        &self,
        tool_call: &crate::model::ToolCall,
        timeout_ms: u64,
    ) -> Result<Option<ToolCallEventResult>> {
        let event_name = "tool_call".to_string();
        let (runtime, has_ui, session, cwd_override, model_registry_values, has_hook_js) = {
            let guard = self.inner.lock().unwrap();
            let has_hook = guard
                .extensions
                .iter()
                .any(|ext| ext.event_hooks.iter().any(|hook| hook == &event_name));
            (
                guard.js_runtime.clone(),
                guard.ui_sender.is_some(),
                guard.session.clone(),
                guard.cwd.clone(),
                guard.model_registry_values.clone(),
                has_hook,
            )
        };

        #[cfg(feature = "wasm-host")]
        let (wasm_extensions, has_hook_wasm) = {
            let guard = self.inner.lock().unwrap();
            let has_hook_wasm = guard
                .wasm_extensions
                .iter()
                .any(|ext| ext.event_hooks().iter().any(|hook| hook == &event_name));
            (guard.wasm_extensions.clone(), has_hook_wasm)
        };

        let has_any_hook = {
            #[cfg(feature = "wasm-host")]
            {
                has_hook_js || has_hook_wasm
            }
            #[cfg(not(feature = "wasm-host"))]
            {
                has_hook_js
            }
        };

        if !has_any_hook {
            return Ok(None);
        }

        let mut ctx = serde_json::Map::new();
        ctx.insert("hasUI".to_string(), Value::Bool(has_ui));
        if let Some(cwd) = cwd_override.or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        }) {
            ctx.insert("cwd".to_string(), Value::String(cwd));
        }

        if !model_registry_values.is_empty() {
            let mut map = serde_json::Map::new();
            for (key, value) in model_registry_values {
                map.insert(key, Value::String(value));
            }
            ctx.insert("modelRegistry".to_string(), Value::Object(map));
        }

        if let Some(session) = session {
            let state = session.get_state().await;
            let entries = session.get_entries().await;
            let branch = session.get_branch().await;
            let leaf_entry = entries.last().cloned().unwrap_or(Value::Null);
            ctx.insert("sessionState".to_string(), state);
            ctx.insert("sessionEntries".to_string(), Value::Array(entries));
            ctx.insert("sessionBranch".to_string(), Value::Array(branch));
            ctx.insert("sessionLeafEntry".to_string(), leaf_entry);
        }

        let ctx_payload = Value::Object(ctx);
        let event_payload = json!({
            "type": "tool_call",
            "toolName": tool_call.name.clone(),
            "toolCallId": tool_call.id.clone(),
            "input": tool_call.arguments.clone()
        });

        let mut response: Option<ToolCallEventResult> = None;

        if let Some(runtime) = runtime {
            if has_hook_js {
                let js_response = runtime
                    .dispatch_event(
                        event_name.clone(),
                        event_payload.clone(),
                        ctx_payload.clone(),
                        timeout_ms,
                    )
                    .await?;
                if !js_response.is_null() {
                    let parsed: ToolCallEventResult = serde_json::from_value(js_response)
                        .map_err(|err| Error::extension(err.to_string()))?;
                    if parsed.block {
                        return Ok(Some(parsed));
                    }
                    response = Some(parsed);
                }
            }
        }

        #[cfg(feature = "wasm-host")]
        if has_hook_wasm {
            let mut wasm_payload = event_payload;
            if let Value::Object(map) = &mut wasm_payload {
                map.insert("ctx".to_string(), ctx_payload);
            }
            if let Some(value) = Self::dispatch_wasm_event_value(
                &wasm_extensions,
                &event_name,
                &wasm_payload,
                timeout_ms,
            )
            .await?
            {
                let parsed: ToolCallEventResult = serde_json::from_value(value)
                    .map_err(|err| Error::extension(err.to_string()))?;
                if parsed.block {
                    return Ok(Some(parsed));
                }
                response = response.or(Some(parsed));
            }
        }

        Ok(response)
    }

    /// Dispatch a `tool_result` event to registered extensions and return the
    /// last handler response (if any).
    #[allow(clippy::too_many_lines)]
    pub async fn dispatch_tool_result(
        &self,
        tool_call: &crate::model::ToolCall,
        output: &crate::tools::ToolOutput,
        is_error: bool,
        timeout_ms: u64,
    ) -> Result<Option<ToolResultEventResult>> {
        let event_name = "tool_result".to_string();
        let (runtime, has_ui, session, cwd_override, model_registry_values, has_hook_js) = {
            let guard = self.inner.lock().unwrap();
            let has_hook = guard
                .extensions
                .iter()
                .any(|ext| ext.event_hooks.iter().any(|hook| hook == &event_name));
            (
                guard.js_runtime.clone(),
                guard.ui_sender.is_some(),
                guard.session.clone(),
                guard.cwd.clone(),
                guard.model_registry_values.clone(),
                has_hook,
            )
        };

        #[cfg(feature = "wasm-host")]
        let (wasm_extensions, has_hook_wasm) = {
            let guard = self.inner.lock().unwrap();
            let has_hook_wasm = guard
                .wasm_extensions
                .iter()
                .any(|ext| ext.event_hooks().iter().any(|hook| hook == &event_name));
            (guard.wasm_extensions.clone(), has_hook_wasm)
        };

        let has_any_hook = {
            #[cfg(feature = "wasm-host")]
            {
                has_hook_js || has_hook_wasm
            }
            #[cfg(not(feature = "wasm-host"))]
            {
                has_hook_js
            }
        };

        if !has_any_hook {
            return Ok(None);
        }

        let mut ctx = serde_json::Map::new();
        ctx.insert("hasUI".to_string(), Value::Bool(has_ui));
        if let Some(cwd) = cwd_override.or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        }) {
            ctx.insert("cwd".to_string(), Value::String(cwd));
        }

        if !model_registry_values.is_empty() {
            let mut map = serde_json::Map::new();
            for (key, value) in model_registry_values {
                map.insert(key, Value::String(value));
            }
            ctx.insert("modelRegistry".to_string(), Value::Object(map));
        }

        if let Some(session) = session {
            let state = session.get_state().await;
            let entries = session.get_entries().await;
            let branch = session.get_branch().await;
            let leaf_entry = entries.last().cloned().unwrap_or(Value::Null);
            ctx.insert("sessionState".to_string(), state);
            ctx.insert("sessionEntries".to_string(), Value::Array(entries));
            ctx.insert("sessionBranch".to_string(), Value::Array(branch));
            ctx.insert("sessionLeafEntry".to_string(), leaf_entry);
        }

        let ctx_payload = Value::Object(ctx);
        let event_payload = json!({
            "type": "tool_result",
            "toolName": tool_call.name.clone(),
            "toolCallId": tool_call.id.clone(),
            "input": tool_call.arguments.clone(),
            "content": output.content.clone(),
            "details": output.details.clone(),
            "isError": is_error
        });

        let mut response: Option<ToolResultEventResult> = None;

        if let Some(runtime) = runtime {
            if has_hook_js {
                let js_response = runtime
                    .dispatch_event(
                        event_name.clone(),
                        event_payload.clone(),
                        ctx_payload.clone(),
                        timeout_ms,
                    )
                    .await?;
                if !js_response.is_null() {
                    response = Some(
                        serde_json::from_value(js_response)
                            .map_err(|err| Error::extension(err.to_string()))?,
                    );
                }
            }
        }

        #[cfg(feature = "wasm-host")]
        if has_hook_wasm {
            let mut wasm_payload = event_payload;
            if let Value::Object(map) = &mut wasm_payload {
                map.insert("ctx".to_string(), ctx_payload);
            }
            if let Some(value) = Self::dispatch_wasm_event_value(
                &wasm_extensions,
                &event_name,
                &wasm_payload,
                timeout_ms,
            )
            .await?
            {
                response = Some(
                    serde_json::from_value(value)
                        .map_err(|err| Error::extension(err.to_string()))?,
                );
            }
        }

        Ok(response)
    }
}

/// Extract extension event information from an agent event.
pub fn extension_event_from_agent(
    event: &AgentEvent,
) -> Option<(ExtensionEventName, Option<Value>)> {
    let name = match event {
        AgentEvent::AgentStart { .. } => ExtensionEventName::AgentStart,
        AgentEvent::AgentEnd { .. } => ExtensionEventName::AgentEnd,
        AgentEvent::TurnStart { .. } => ExtensionEventName::TurnStart,
        AgentEvent::TurnEnd { .. } => ExtensionEventName::TurnEnd,
        AgentEvent::MessageStart { .. } => ExtensionEventName::MessageStart,
        AgentEvent::MessageUpdate { .. } => ExtensionEventName::MessageUpdate,
        AgentEvent::MessageEnd { .. } => ExtensionEventName::MessageEnd,
        AgentEvent::ToolExecutionStart { .. } => ExtensionEventName::ToolExecutionStart,
        AgentEvent::ToolExecutionUpdate { .. } => ExtensionEventName::ToolExecutionUpdate,
        AgentEvent::ToolExecutionEnd { .. } => ExtensionEventName::ToolExecutionEnd,
    };

    let payload = serde_json::to_value(event).ok();
    Some((name, payload))
}

fn extract_slash_command_name(value: &Value) -> Option<String> {
    value
        .get("name")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn normalize_command(name: &str) -> String {
    name.trim_start_matches('/').trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonschema::Validator;
    use tempfile::tempdir;

    fn compiled_extension_protocol_schema() -> Validator {
        let schema_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("docs/schema/extension_protocol.json");
        let raw = std::fs::read_to_string(&schema_path)
            .map_err(|err| {
                format!(
                    "Failed to read extension protocol schema {}: {err}",
                    schema_path.display()
                )
            })
            .unwrap();
        let schema: Value = serde_json::from_str(&raw)
            .map_err(|err| {
                format!(
                    "Failed to parse extension protocol schema {}: {err}",
                    schema_path.display()
                )
            })
            .unwrap();

        jsonschema::draft202012::options()
            .should_validate_formats(true)
            .build(&schema)
            .map_err(|err| {
                format!(
                    "Failed to compile JSON schema {}: {err}",
                    schema_path.display()
                )
            })
            .unwrap()
    }

    #[allow(clippy::too_many_lines)]
    fn sample_protocol_messages() -> Vec<(&'static str, ExtensionMessage)> {
        vec![
            (
                "register",
                ExtensionMessage {
                    id: "msg-register".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::Register(RegisterPayload {
                        name: "demo".to_string(),
                        version: "0.1.0".to_string(),
                        api_version: "1.0".to_string(),
                        capabilities: vec!["read".to_string()],
                        capability_manifest: None,
                        tools: Vec::new(),
                        slash_commands: Vec::new(),
                        event_hooks: Vec::new(),
                    }),
                },
            ),
            (
                "tool_call",
                ExtensionMessage {
                    id: "msg-tool-call".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::ToolCall(ToolCallPayload {
                        call_id: "call-1".to_string(),
                        name: "read".to_string(),
                        input: json!({ "path": "README.md" }),
                        context: None,
                    }),
                },
            ),
            (
                "tool_result",
                ExtensionMessage {
                    id: "msg-tool-result".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::ToolResult(ToolResultPayload {
                        call_id: "call-1".to_string(),
                        output: json!({ "ok": true }),
                        is_error: false,
                    }),
                },
            ),
            (
                "slash_command",
                ExtensionMessage {
                    id: "msg-slash-command".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::SlashCommand(SlashCommandPayload {
                        name: "/hello".to_string(),
                        args: vec!["world".to_string()],
                        input: None,
                    }),
                },
            ),
            (
                "slash_result",
                ExtensionMessage {
                    id: "msg-slash-result".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::SlashResult(SlashResultPayload {
                        output: json!({ "text": "ok" }),
                        is_error: false,
                    }),
                },
            ),
            (
                "event_hook",
                ExtensionMessage {
                    id: "msg-event-hook".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::EventHook(EventHookPayload {
                        event: "agent_start".to_string(),
                        data: Some(json!({ "note": "hello" })),
                    }),
                },
            ),
            (
                "host_call",
                ExtensionMessage {
                    id: "msg-host-call".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::HostCall(HostCallPayload {
                        call_id: "host-1".to_string(),
                        capability: "read".to_string(),
                        method: "tool".to_string(),
                        params: json!({ "name": "read", "input": { "path": "README.md" } }),
                        timeout_ms: Some(2500),
                        cancel_token: None,
                        context: None,
                    }),
                },
            ),
            (
                "host_call_cancel",
                ExtensionMessage {
                    id: "msg-host-call-cancel".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::HostCall(HostCallPayload {
                        call_id: "host-2".to_string(),
                        capability: "http".to_string(),
                        method: "http".to_string(),
                        params: json!({ "url": "https://example.com", "method": "GET" }),
                        timeout_ms: Some(1500),
                        cancel_token: Some("cancel-1".to_string()),
                        context: Some(json!({ "trace_id": "trace-1" })),
                    }),
                },
            ),
            (
                "host_result",
                ExtensionMessage {
                    id: "msg-host-result".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::HostResult(HostResultPayload {
                        call_id: "host-1".to_string(),
                        output: json!({ "content": [] }),
                        is_error: false,
                        error: None,
                        chunk: None,
                    }),
                },
            ),
            (
                "host_result_timeout",
                ExtensionMessage {
                    id: "msg-host-result-timeout".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::HostResult(HostResultPayload {
                        call_id: "host-2".to_string(),
                        output: json!({}),
                        is_error: true,
                        error: Some(HostCallError {
                            code: HostCallErrorCode::Timeout,
                            message: "Timed out".to_string(),
                            details: None,
                            retryable: Some(true),
                        }),
                        chunk: None,
                    }),
                },
            ),
            (
                "host_result_denied",
                ExtensionMessage {
                    id: "msg-host-result-denied".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::HostResult(HostResultPayload {
                        call_id: "host-3".to_string(),
                        output: json!({}),
                        is_error: true,
                        error: Some(HostCallError {
                            code: HostCallErrorCode::Denied,
                            message: "Denied".to_string(),
                            details: Some(json!({ "capability": "exec" })),
                            retryable: None,
                        }),
                        chunk: None,
                    }),
                },
            ),
            (
                "log",
                ExtensionMessage {
                    id: "msg-log".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::Log(Box::new(LogPayload {
                        schema: LOG_SCHEMA_VERSION.to_string(),
                        ts: "2026-02-03T03:01:02.123Z".to_string(),
                        level: LogLevel::Info,
                        event: "tool_call.start".to_string(),
                        message: "tool call dispatched".to_string(),
                        correlation: LogCorrelation {
                            extension_id: "ext.demo".to_string(),
                            scenario_id: "scn-001".to_string(),
                            session_id: None,
                            run_id: None,
                            artifact_id: None,
                            tool_call_id: None,
                            slash_command_id: None,
                            event_id: None,
                            host_call_id: None,
                            rpc_id: None,
                            trace_id: None,
                            span_id: None,
                        },
                        source: None,
                        data: None,
                    })),
                },
            ),
            (
                "error",
                ExtensionMessage {
                    id: "msg-error".to_string(),
                    version: PROTOCOL_VERSION.to_string(),
                    body: ExtensionBody::Error(ErrorPayload {
                        code: "E_DEMO".to_string(),
                        message: "Something went wrong".to_string(),
                        details: Some(json!({ "hint": "check config" })),
                    }),
                },
            ),
        ]
    }

    #[test]
    fn parse_register_message() {
        let json = r#"
        {
          "id": "msg-1",
          "version": "1.0",
          "type": "register",
          "payload": {
            "name": "demo",
            "version": "0.1.0",
            "api_version": "1.0",
            "capabilities": ["read"]
          }
        }
        "#;
        let msg = ExtensionMessage::parse_and_validate(json).unwrap();
        assert!(matches!(msg.body, ExtensionBody::Register(_)));
    }

    #[test]
    fn reject_invalid_version() {
        let json = r#"
        {
          "id": "msg-2",
          "version": "2.0",
          "type": "log",
          "payload": {
            "schema": "pi.ext.log.v1",
            "ts": "2026-02-03T03:01:02.123Z",
            "level": "info",
            "event": "tool_call.start",
            "message": "hi",
            "correlation": {
              "extension_id": "ext.demo",
              "scenario_id": "scn-001"
            }
          }
        }
        "#;
        let err = ExtensionMessage::parse_and_validate(json).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Unsupported extension protocol version"));
    }

    #[test]
    fn parse_host_call_message() {
        let json = r#"
        {
          "id": "msg-3",
          "version": "1.0",
          "type": "host_call",
          "payload": {
            "call_id": "call-1",
            "capability": "read",
            "method": "tool",
            "params": { "name": "read", "input": { "path": "README.md" } },
            "timeout_ms": 1000
          }
        }
        "#;
        let msg = ExtensionMessage::parse_and_validate(json).unwrap();
        assert!(matches!(msg.body, ExtensionBody::HostCall(_)));
    }

    #[test]
    fn parse_log_message() {
        let json = r#"
        {
          "id": "msg-4",
          "version": "1.0",
          "type": "log",
          "payload": {
            "schema": "pi.ext.log.v1",
            "ts": "2026-02-03T03:01:02.123Z",
            "level": "info",
            "event": "tool_call.start",
            "message": "tool call dispatched",
            "correlation": {
              "extension_id": "ext.demo",
              "scenario_id": "scn-001"
            }
          }
        }
        "#;
        let msg = ExtensionMessage::parse_and_validate(json).unwrap();
        assert!(matches!(msg.body, ExtensionBody::Log(_)));
    }

    #[test]
    fn extension_ui_rpc_event_format() {
        let request = ExtensionUiRequest::new(
            "req-1",
            "notify",
            json!({ "title": "Hello", "message": "World" }),
        );
        let event = request.to_rpc_event();
        assert_eq!(event["type"], "extension_ui_request");
        assert_eq!(event["id"], "req-1");
        assert_eq!(event["method"], "notify");
        assert_eq!(event["title"], "Hello");
        assert_eq!(event["message"], "World");
    }

    #[test]
    fn extension_ui_request_roundtrip() {
        let manager = ExtensionManager::new();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            let (ui_tx, ui_rx) = mpsc::channel(16);
            manager.set_ui_sender(ui_tx);

            let responder = manager.clone();
            handle.spawn(async move {
                let cx = Cx::for_request();
                if let Ok(req) = ui_rx.recv(&cx).await {
                    responder.respond_ui(ExtensionUiResponse {
                        id: req.id,
                        value: Some(json!(true)),
                        cancelled: false,
                    });
                }
            });

            let request = ExtensionUiRequest::new("", "confirm", json!({ "title": "Confirm" }));
            let response = manager.request_ui(request).await.unwrap();
            assert_eq!(response.unwrap().value, Some(json!(true)));
        });
    }

    #[test]
    fn js_hostcall_prompt_mode_asks_once_per_capability() {
        let manager = ExtensionManager::new();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");
        let handle = runtime.handle();

        runtime.block_on(async move {
            use std::sync::atomic::{AtomicUsize, Ordering};

            let (ui_tx, ui_rx) = mpsc::channel(16);
            manager.set_ui_sender(ui_tx);

            let prompt_count = Arc::new(AtomicUsize::new(0));
            let prompt_count_clone = Arc::clone(&prompt_count);

            let responder = manager.clone();
            handle.spawn(async move {
                let cx = Cx::for_request();
                while let Ok(req) = ui_rx.recv(&cx).await {
                    prompt_count_clone.fetch_add(1, Ordering::SeqCst);
                    responder.respond_ui(ExtensionUiResponse {
                        id: req.id,
                        value: Some(json!(true)),
                        cancelled: false,
                    });
                }
            });

            let dir = tempdir().expect("tempdir");
            let host = JsRuntimeHost {
                tools: Arc::new(ToolRegistry::new(&[], dir.path(), None)),
                manager: manager.clone(),
                http: Arc::new(HttpConnector::with_defaults()),
                policy: ExtensionPolicy {
                    mode: ExtensionPolicyMode::Prompt,
                    max_memory_mb: 256,
                    default_caps: Vec::new(),
                    deny_caps: Vec::new(),
                },
            };

            let request = HostcallRequest {
                call_id: "call-1".to_string(),
                kind: HostcallKind::Tool {
                    name: "nonexistent".to_string(),
                },
                payload: json!({}),
                trace_id: 1,
                extension_id: Some("ext.test".to_string()),
            };

            let _ = dispatch_hostcall(&host, request).await;

            let request = HostcallRequest {
                call_id: "call-2".to_string(),
                kind: HostcallKind::Tool {
                    name: "nonexistent".to_string(),
                },
                payload: json!({}),
                trace_id: 2,
                extension_id: Some("ext.test".to_string()),
            };

            let _ = dispatch_hostcall(&host, request).await;

            assert_eq!(prompt_count.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn js_runtime_pump_once_advances_timers_and_hostcalls() {
        let manager = ExtensionManager::new();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            let dir = tempdir().expect("tempdir");
            let entry_path = dir.path().join("ext.mjs");
            std::fs::write(
                &entry_path,
                r#"
                export default function init(pi) {
                  pi.on("agent_start", () => {
                    setTimeout(() => {
                      pi.tool("write", { path: "out.txt", content: "hi" });
                    }, 0);
                  });
                }
                "#,
            )
            .expect("write extension entry");

            let tools = Arc::new(ToolRegistry::new(&["write"], dir.path(), None));
            let js_runtime = JsExtensionRuntimeHandle::start(
                PiJsRuntimeConfig {
                    cwd: dir.path().display().to_string(),
                    ..Default::default()
                },
                Arc::clone(&tools),
                manager.clone(),
            )
            .await
            .expect("start js runtime");
            manager.set_js_runtime(js_runtime.clone());

            let spec = JsExtensionLoadSpec::from_entry_path(&entry_path).expect("load spec");
            manager
                .load_js_extensions(vec![spec])
                .await
                .expect("load extension");

            manager
                .dispatch_event(ExtensionEventName::AgentStart, None)
                .await
                .expect("dispatch agent_start");

            let out_path = dir.path().join("out.txt");
            let mut wrote = false;
            for _ in 0..20 {
                let _ = js_runtime.pump_once().await.expect("pump_once");
                if out_path.exists() {
                    wrote = true;
                    break;
                }
                sleep(wall_now(), Duration::from_millis(1)).await;
            }

            assert!(wrote, "expected out.txt to be created after pumping");
            let contents = std::fs::read_to_string(&out_path).expect("read out.txt");
            assert_eq!(contents, "hi");
        });
    }

    #[test]
    fn extension_protocol_schema_accepts_all_variants() {
        let schema = compiled_extension_protocol_schema();
        for (label, message) in sample_protocol_messages() {
            let instance = serde_json::to_value(&message)
                .map_err(|err| format!("{label}: {err}"))
                .unwrap();

            let errors = schema
                .iter_errors(&instance)
                .map(|err| err.to_string())
                .collect::<Vec<_>>();
            assert!(
                errors.is_empty(),
                "{label}: schema validation failed:\n{}",
                errors.join("\n")
            );

            let json = serde_json::to_string(&message)
                .map_err(|err| format!("{label}: {err}"))
                .unwrap();
            let parsed = ExtensionMessage::parse_and_validate(&json)
                .map_err(|err| format!("{label}: parse_and_validate failed: {err}"))
                .unwrap();
            let parsed_json = serde_json::to_value(&parsed)
                .map_err(|err| format!("{label}: {err}"))
                .unwrap();
            assert_eq!(
                instance, parsed_json,
                "{label}: JSON changed after roundtrip"
            );
        }
    }

    #[test]
    fn extension_protocol_schema_rejects_missing_required_fields() {
        let schema = compiled_extension_protocol_schema();

        let (_, message) = sample_protocol_messages()
            .into_iter()
            .find(|(label, _)| *label == "register")
            .expect("register sample");
        let mut instance = serde_json::to_value(&message).expect("serialize");

        // Missing "id"
        instance
            .as_object_mut()
            .expect("object")
            .remove("id")
            .expect("id present");
        assert!(
            schema.validate(&instance).is_err(),
            "schema should reject missing id"
        );
    }

    #[test]
    fn parse_and_validate_rejects_unknown_type() {
        let json = r#"
        {
          "id": "msg-unknown",
          "version": "1.0",
          "type": "not_a_real_type",
          "payload": { "x": 1 }
        }
        "#;
        assert!(ExtensionMessage::parse_and_validate(json).is_err());
    }

    #[test]
    fn parse_fs_host_call_message() {
        let json = r#"
        {
          "id": "msg-fs",
          "version": "1.0",
          "type": "host_call",
          "payload": {
            "call_id": "call-1",
            "capability": "read",
            "method": "fs",
            "params": { "op": "read", "path": "README.md" }
          }
        }
        "#;
        let msg = ExtensionMessage::parse_and_validate(json).unwrap();
        assert!(matches!(msg.body, ExtensionBody::HostCall(_)));
    }

    #[test]
    fn required_capability_for_host_call_maps_tools_and_fs_ops() {
        let tool_read = HostCallPayload {
            call_id: "call-tool-read".to_string(),
            capability: "read".to_string(),
            method: "tool".to_string(),
            params: json!({ "name": "read", "input": { "path": "README.md" } }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        assert_eq!(
            required_capability_for_host_call(&tool_read).as_deref(),
            Some("read")
        );

        let tool_bash = HostCallPayload {
            call_id: "call-tool-bash".to_string(),
            capability: "exec".to_string(),
            method: "tool".to_string(),
            params: json!({ "name": "bash", "input": { "command": "echo hi" } }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        assert_eq!(
            required_capability_for_host_call(&tool_bash).as_deref(),
            Some("exec")
        );

        let fs_delete = HostCallPayload {
            call_id: "call-fs-delete".to_string(),
            capability: "write".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "delete", "path": "tmp.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        assert_eq!(
            required_capability_for_host_call(&fs_delete).as_deref(),
            Some("write")
        );

        let env_get = HostCallPayload {
            call_id: "call-env-get".to_string(),
            capability: "env".to_string(),
            method: "env".to_string(),
            params: json!({ "name": "HOME" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        assert_eq!(
            required_capability_for_host_call(&env_get).as_deref(),
            Some("env")
        );

        let unknown = HostCallPayload {
            call_id: "call-unknown".to_string(),
            capability: "read".to_string(),
            method: "nope".to_string(),
            params: json!({}),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        assert!(required_capability_for_host_call(&unknown).is_none());
    }

    #[test]
    fn fs_connector_denies_path_traversal_outside_cwd() {
        let dir = tempdir().expect("tempdir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create project dir");

        let inside = project.join("inside.txt");
        std::fs::write(&inside, "hello").expect("write inside");

        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret").expect("write outside");

        let policy = ExtensionPolicy::default();
        let scopes = FsScopes::for_cwd(&project).expect("scopes");
        let connector = FsConnector::new(project, policy, scopes).expect("connector");

        let ok_call = HostCallPayload {
            call_id: "call-ok".to_string(),
            capability: "read".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "read", "path": "inside.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let ok_result = connector.handle_host_call(&ok_call);
        assert!(!ok_result.is_error);

        let denied_call = HostCallPayload {
            call_id: "call-deny".to_string(),
            capability: "read".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "read", "path": "../outside.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let denied = connector.handle_host_call(&denied_call);
        assert!(denied.is_error);
        assert_eq!(
            denied.error.as_ref().expect("error").code,
            HostCallErrorCode::Denied
        );
    }

    #[cfg(unix)]
    #[test]
    fn fs_connector_denies_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().expect("tempdir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create project dir");

        let outside = dir.path().join("secret.txt");
        std::fs::write(&outside, "secret").expect("write outside");

        let link = project.join("link.txt");
        symlink(&outside, &link).expect("symlink");

        let policy = ExtensionPolicy::default();
        let scopes = FsScopes::for_cwd(&project).expect("scopes");
        let connector = FsConnector::new(project, policy, scopes).expect("connector");

        let call = HostCallPayload {
            call_id: "call-link".to_string(),
            capability: "read".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "read", "path": "link.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };
        let result = connector.handle_host_call(&call);
        assert!(result.is_error);
        assert_eq!(
            result.error.as_ref().expect("error").code,
            HostCallErrorCode::Denied
        );
    }

    #[test]
    fn fs_connector_denies_when_policy_denies_capability() {
        let dir = tempdir().expect("tempdir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create project dir");

        let inside = project.join("inside.txt");
        std::fs::write(&inside, "hello").expect("write inside");

        let mut policy = ExtensionPolicy::default();
        policy.deny_caps.push("read".to_string());

        let scopes = FsScopes::for_cwd(&project).expect("scopes");
        let connector = FsConnector::new(&project, policy, scopes).expect("connector");

        let call = HostCallPayload {
            call_id: "call-policy-deny".to_string(),
            capability: "read".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "read", "path": "inside.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let result = connector.handle_host_call(&call);
        assert!(result.is_error);
        assert_eq!(
            result.error.as_ref().expect("error").code,
            HostCallErrorCode::Denied
        );
    }

    #[test]
    fn fs_connector_denies_write_when_manifest_does_not_declare_write_scope() {
        let dir = tempdir().expect("tempdir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("create project dir");

        let inside = project.join("inside.txt");
        std::fs::write(&inside, "hello").expect("write inside");

        let manifest = CapabilityManifest {
            schema: "pi.ext.cap.v1".to_string(),
            capabilities: vec![CapabilityRequirement {
                capability: "read".to_string(),
                methods: vec!["fs".to_string()],
                scope: Some(CapabilityScope {
                    paths: Some(vec![".".to_string()]),
                    hosts: None,
                    env: None,
                }),
            }],
        };
        let scopes = FsScopes::from_manifest(Some(&manifest), &project).expect("scopes");
        let connector =
            FsConnector::new(&project, ExtensionPolicy::default(), scopes).expect("connector");

        let call = HostCallPayload {
            call_id: "call-scope-deny".to_string(),
            capability: "write".to_string(),
            method: "fs".to_string(),
            params: json!({ "op": "write", "path": "inside.txt" }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let result = connector.handle_host_call(&call);
        assert!(result.is_error);
        assert_eq!(
            result.error.as_ref().expect("error").code,
            HostCallErrorCode::Denied
        );
    }

    fn canonicalize_json(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys = map.keys().cloned().collect::<Vec<_>>();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    if let Some(value) = map.get(&key) {
                        out.insert(key, canonicalize_json(value));
                    }
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(canonicalize_json).collect()),
            other => other.clone(),
        }
    }

    fn sha256_hex(input: &str) -> String {
        use std::fmt::Write as _;

        let mut hasher = sha2::Sha256::new();
        hasher.update(input.as_bytes());
        let digest = hasher.finalize();

        let mut out = String::with_capacity(digest.len() * 2);
        for byte in digest {
            write!(&mut out, "{byte:02x}").expect("write hex");
        }
        out
    }

    fn hostcall_params_hash(method: &str, params: &Value) -> String {
        let canonical = canonicalize_json(&json!({ "method": method, "params": params }));
        let json = serde_json::to_string(&canonical).expect("serialize canonical hostcall");
        sha256_hex(&json)
    }

    fn hostcall_ledger_start_data(call: &HostCallPayload) -> Value {
        let mut data = serde_json::Map::new();
        data.insert(
            "capability".to_string(),
            Value::String(call.capability.clone()),
        );
        data.insert("method".to_string(), Value::String(call.method.clone()));
        data.insert(
            "params_hash".to_string(),
            Value::String(hostcall_params_hash(&call.method, &call.params)),
        );
        if let Some(timeout_ms) = call.timeout_ms {
            data.insert("timeout_ms".to_string(), json!(timeout_ms));
        }
        Value::Object(data)
    }

    fn hostcall_ledger_end_data(
        call: &HostCallPayload,
        duration_ms: u64,
        result: &HostResultPayload,
    ) -> Value {
        let mut data = serde_json::Map::new();
        data.insert(
            "capability".to_string(),
            Value::String(call.capability.clone()),
        );
        data.insert("method".to_string(), Value::String(call.method.clone()));
        data.insert(
            "params_hash".to_string(),
            Value::String(hostcall_params_hash(&call.method, &call.params)),
        );
        if let Some(timeout_ms) = call.timeout_ms {
            data.insert("timeout_ms".to_string(), json!(timeout_ms));
        }
        data.insert("duration_ms".to_string(), json!(duration_ms));
        data.insert("is_error".to_string(), Value::Bool(result.is_error));
        if result.is_error {
            if let Some(error) = result.error.as_ref() {
                data.insert("error".to_string(), json!({ "code": error.code }));
            }
        }
        Value::Object(data)
    }

    #[test]
    fn hostcall_params_hash_is_stable_for_key_ordering() {
        let mut first = serde_json::Map::new();
        first.insert("b".to_string(), json!(2));
        first.insert("a".to_string(), json!(1));
        let first = Value::Object(first);

        let mut second = serde_json::Map::new();
        second.insert("a".to_string(), json!(1));
        second.insert("b".to_string(), json!(2));
        let second = Value::Object(second);

        assert_eq!(
            hostcall_params_hash("http", &first),
            hostcall_params_hash("http", &second)
        );
        assert_ne!(
            hostcall_params_hash("http", &first),
            hostcall_params_hash("tool", &first)
        );
    }

    #[test]
    fn hostcall_ledger_start_redacts_params_and_includes_hash() {
        let call = HostCallPayload {
            call_id: "host-ledger-1".to_string(),
            capability: "env".to_string(),
            method: "env".to_string(),
            params: json!({ "name": "ANTHROPIC_API_KEY", "value": "sk-ant-SECRET" }),
            timeout_ms: Some(1234),
            cancel_token: None,
            context: None,
        };

        let data = hostcall_ledger_start_data(&call);
        let obj = data.as_object().expect("object");
        assert!(obj.get("params_hash").is_some());
        assert!(obj.get("params").is_none());

        let encoded = serde_json::to_string(&data).expect("serialize data");
        assert!(!encoded.contains("sk-ant-SECRET"));
        assert!(!encoded.contains("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn hostcall_ledger_end_includes_error_code_when_is_error() {
        let call = HostCallPayload {
            call_id: "host-ledger-2".to_string(),
            capability: "exec".to_string(),
            method: "exec".to_string(),
            params: json!({ "cmd": "ls", "args": ["-la"] }),
            timeout_ms: None,
            cancel_token: None,
            context: None,
        };

        let result = HostResultPayload {
            call_id: call.call_id.clone(),
            output: json!({}),
            is_error: true,
            error: Some(HostCallError {
                code: HostCallErrorCode::Denied,
                message: "Denied".to_string(),
                details: None,
                retryable: None,
            }),
            chunk: None,
        };

        let data = hostcall_ledger_end_data(&call, 10, &result);
        let obj = data.as_object().expect("object");
        assert_eq!(obj.get("is_error").and_then(Value::as_bool), Some(true));

        let error = obj
            .get("error")
            .and_then(Value::as_object)
            .expect("error object");
        assert_eq!(error.get("code").and_then(Value::as_str), Some("denied"));
    }

    #[derive(Debug, Clone)]
    struct CapturedEvent {
        level: tracing::Level,
        fields: std::collections::BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct CaptureLayer {
        events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
    }

    impl CaptureLayer {
        fn snapshot(&self) -> Vec<CapturedEvent> {
            self.events.lock().expect("events mutex").clone()
        }
    }

    struct FieldVisitor<'a> {
        fields: &'a mut std::collections::BTreeMap<String, String>,
    }

    impl tracing::field::Visit for FieldVisitor<'_> {
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    impl<S> tracing_subscriber::Layer<S> for CaptureLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut fields = std::collections::BTreeMap::new();
            let mut visitor = FieldVisitor {
                fields: &mut fields,
            };
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("events mutex")
                .push(CapturedEvent {
                    level: *event.metadata().level(),
                    fields,
                });
        }
    }

    fn capture_tracing_events<T>(f: impl FnOnce() -> T) -> (T, Vec<CapturedEvent>) {
        use tracing_subscriber::layer::SubscriberExt as _;

        let capture = CaptureLayer::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let result = tracing::subscriber::with_default(subscriber, f);
        (result, capture.snapshot())
    }

    fn run_async<T, Fut>(future: Fut) -> T
    where
        Fut: std::future::Future<Output = T>,
    {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .expect("build asupersync runtime");
        runtime.block_on(future)
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn js_hostcall_prompt_policy_caches_user_allow_and_never_logs_raw_params() {
        use std::sync::Arc;

        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let manager = ExtensionManager::new();
        let (ui_tx, ui_rx) = asupersync::channel::mpsc::channel(8);
        manager.set_ui_sender(ui_tx);

        let manager_for_ui = manager.clone();
        let ui_join = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("build asupersync runtime");
            runtime.block_on(async move {
                let cx = asupersync::Cx::for_request();
                let request = ui_rx.recv(&cx).await.expect("ui request");
                assert_eq!(request.method, "confirm");

                assert!(
                    manager_for_ui.respond_ui(ExtensionUiResponse {
                        id: request.id,
                        value: Some(serde_json::Value::Bool(true)),
                        cancelled: false,
                    }),
                    "respond_ui"
                );
            });
        });

        let host = JsRuntimeHost {
            tools: Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None)),
            manager,
            http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Prompt,
                max_memory_mb: 256,
                default_caps: Vec::new(),
                deny_caps: Vec::new(),
            },
        };

        let request = crate::extensions_js::HostcallRequest {
            call_id: "hostcall-1".to_string(),
            kind: crate::extensions_js::HostcallKind::Tool {
                name: "custom_tool".to_string(),
            },
            payload: serde_json::json!({
                "token": "supersecret",
                "nested": { "apiKey": "sk-ant-SECRET" }
            }),
            trace_id: 0,
            extension_id: Some("ext-1".to_string()),
        };

        let request_cached = crate::extensions_js::HostcallRequest {
            call_id: "hostcall-2".to_string(),
            kind: crate::extensions_js::HostcallKind::Tool {
                name: "custom_tool".to_string(),
            },
            payload: serde_json::json!({ "token": "supersecret" }),
            trace_id: 0,
            extension_id: Some("ext-1".to_string()),
        };

        let ((first, second), events) = capture_tracing_events(|| {
            run_async(async {
                let first = super::dispatch_hostcall(&host, request).await;
                let second = super::dispatch_hostcall(&host, request_cached).await;
                (first, second)
            })
        });

        ui_join.join().expect("ui thread join");

        assert!(matches!(first, HostcallOutcome::Error { code, .. } if code == "invalid_request"));
        assert!(matches!(second, HostcallOutcome::Error { code, .. } if code == "invalid_request"));

        let decision_events = events
            .iter()
            .filter(|event| {
                event
                    .fields
                    .get("event")
                    .is_some_and(|value| value.contains("policy.decision"))
            })
            .collect::<Vec<_>>();
        assert_eq!(decision_events.len(), 2);
        assert!(
            decision_events[0]
                .fields
                .get("reason")
                .is_some_and(|value| value.contains("prompt_user_allow")),
            "expected prompt_user_allow reason, got {:?}",
            decision_events[0].fields
        );
        assert!(
            decision_events[1]
                .fields
                .get("reason")
                .is_some_and(|value| value.contains("prompt_cache_allow")),
            "expected prompt_cache_allow reason, got {:?}",
            decision_events[1].fields
        );

        for event in &events {
            for value in event.fields.values() {
                assert!(
                    !value.contains("supersecret"),
                    "secret leaked into logs: {value}"
                );
                assert!(
                    !value.contains("sk-ant-SECRET"),
                    "api key leaked into logs: {value}"
                );
            }
        }

        let params_hash = decision_events[0]
            .fields
            .get("params_hash")
            .expect("params_hash");
        let params_hash = params_hash.trim_matches('"');
        assert_eq!(params_hash.len(), 64);
        assert!(params_hash.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn js_hostcall_strict_policy_denies_and_logs_reason() {
        use std::sync::Arc;

        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let host = JsRuntimeHost {
            tools: Arc::new(crate::tools::ToolRegistry::new(&[], &cwd, None)),
            manager: ExtensionManager::new(),
            http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Strict,
                max_memory_mb: 256,
                default_caps: vec!["read".to_string()],
                deny_caps: Vec::new(),
            },
        };

        let request = crate::extensions_js::HostcallRequest {
            call_id: "hostcall-strict-1".to_string(),
            kind: crate::extensions_js::HostcallKind::Exec {
                cmd: "does-not-run".to_string(),
            },
            payload: serde_json::json!({}),
            trace_id: 0,
            extension_id: Some("ext-1".to_string()),
        };

        let (outcome, events) = capture_tracing_events(|| {
            run_async(async { super::dispatch_hostcall(&host, request).await })
        });

        assert!(matches!(outcome, HostcallOutcome::Error { code, .. } if code == "denied"));

        let decision = events.iter().find(|event| {
            event
                .fields
                .get("event")
                .is_some_and(|value| value.contains("policy.decision"))
        });
        let decision = decision.expect("policy.decision event");
        assert_eq!(decision.level, tracing::Level::WARN);
        assert!(
            decision
                .fields
                .get("reason")
                .is_some_and(|value| value.contains("not_in_default_caps"))
        );
        assert!(
            decision
                .fields
                .get("call_id")
                .is_some_and(|value| value.contains("hostcall-strict-1"))
        );
    }

    #[test]
    fn js_hostcall_routes_write_and_read_tools_when_allowed() {
        use std::sync::Arc;

        let dir = tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();

        let host = JsRuntimeHost {
            tools: Arc::new(crate::tools::ToolRegistry::new(
                &["read", "write"],
                &cwd,
                None,
            )),
            manager: ExtensionManager::new(),
            http: Arc::new(crate::connectors::http::HttpConnector::with_defaults()),
            policy: ExtensionPolicy {
                mode: ExtensionPolicyMode::Strict,
                max_memory_mb: 256,
                default_caps: vec!["read".to_string(), "write".to_string()],
                deny_caps: Vec::new(),
            },
        };

        let write_request = crate::extensions_js::HostcallRequest {
            call_id: "hostcall-write".to_string(),
            kind: crate::extensions_js::HostcallKind::Tool {
                name: "write".to_string(),
            },
            payload: serde_json::json!({
                "path": "note.txt",
                "content": "hello"
            }),
            trace_id: 0,
            extension_id: Some("ext-1".to_string()),
        };

        let read_request = crate::extensions_js::HostcallRequest {
            call_id: "hostcall-read".to_string(),
            kind: crate::extensions_js::HostcallKind::Tool {
                name: "read".to_string(),
            },
            payload: serde_json::json!({ "path": "note.txt" }),
            trace_id: 0,
            extension_id: Some("ext-1".to_string()),
        };

        let ((write_outcome, read_outcome), events) = capture_tracing_events(|| {
            run_async(async {
                let write_outcome = super::dispatch_hostcall(&host, write_request).await;
                let read_outcome = super::dispatch_hostcall(&host, read_request).await;
                (write_outcome, read_outcome)
            })
        });

        assert!(matches!(write_outcome, HostcallOutcome::Success(_)));
        assert_eq!(
            std::fs::read_to_string(cwd.join("note.txt")).expect("read note.txt"),
            "hello"
        );

        let value = match read_outcome {
            HostcallOutcome::Success(value) => value,
            HostcallOutcome::Error { code, message } => {
                assert!(
                    code == "__expected_success__",
                    "expected read success, got error {code}: {message}"
                );
                return;
            }
        };

        let encoded = serde_json::to_string(&value).expect("serialize read output");
        assert!(encoded.contains("hello"));

        let decisions = events
            .iter()
            .filter(|event| {
                event
                    .fields
                    .get("event")
                    .is_some_and(|value| value.contains("policy.decision"))
            })
            .collect::<Vec<_>>();
        assert_eq!(decisions.len(), 2);
        for decision in decisions {
            assert_eq!(decision.level, tracing::Level::INFO);
            assert!(
                decision
                    .fields
                    .get("reason")
                    .is_some_and(|value| value.contains("default_caps"))
            );
        }
    }
}
