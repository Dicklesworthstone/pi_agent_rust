//! Private implementation seams for extension compatibility scanning.
//!
//! Public compatibility contracts remain defined in `extensions` so their
//! Rust type identity stays stable while implementation details move here.

use super::{
    COMPAT_LEDGER_SCHEMA_VERSION, CompatCapabilityEvidence, CompatEvidence, CompatIssueEvidence,
    CompatLedger, CompatRewriteEvidence, CompatibilityScanner,
};
use crate::error::{Error, Result};
use regex::Regex;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MARKER_IMPORT: u16 = 1 << 0;
const MARKER_REQUIRE: u16 = 1 << 1;
const MARKER_PI: u16 = 1 << 2;
const MARKER_PROCESS_ENV: u16 = 1 << 3;
const MARKER_PROCESS: u16 = 1 << 4;
const MARKER_FUNCTION: u16 = 1 << 5;
const MARKER_EVAL: u16 = 1 << 6;
const MARKER_BINDING: u16 = 1 << 7;
const MARKER_DLOPEN: u16 = 1 << 8;

struct ScannerState {
    in_block_comment: bool,
    in_template: bool,
    last_significant_char: Option<char>,
}

impl CompatibilityScanner {
    #[must_use]
    pub const fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn scan_path(&self, path: &Path) -> Result<CompatLedger> {
        let files = collect_js_like_files(path)?;
        self.scan_files(&files)
    }

    pub fn scan_root(&self) -> Result<CompatLedger> {
        self.scan_path(&self.root)
    }

    fn scan_files(&self, files: &[PathBuf]) -> Result<CompatLedger> {
        let mut caps: BTreeMap<(String, String, String), Vec<CompatEvidence>> = BTreeMap::new();
        let mut rewrites: BTreeMap<(String, String), Vec<CompatEvidence>> = BTreeMap::new();
        let mut forbidden: BTreeMap<(String, String, String), Vec<CompatEvidence>> =
            BTreeMap::new();
        let mut flagged: BTreeMap<(String, String, String), Vec<CompatEvidence>> = BTreeMap::new();

        for path in files {
            self.scan_file(path, &mut caps, &mut rewrites, &mut forbidden, &mut flagged)?;
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

        Ok(CompatLedger {
            schema: COMPAT_LEDGER_SCHEMA_VERSION.to_string(),
            capabilities,
            rewrites,
            forbidden,
            flagged,
        })
    }

    fn scan_file(
        &self,
        path: &Path,
        caps: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
        rewrites: &mut BTreeMap<(String, String), Vec<CompatEvidence>>,
        forbidden: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
        flagged: &mut BTreeMap<(String, String, String), Vec<CompatEvidence>>,
    ) -> Result<()> {
        let content = fs::read_to_string(path).map_err(|err| {
            Error::extension(format!(
                "Failed to read extension source file {}: {err}",
                path.display()
            ))
        })?;

        let rel = relative_posix(&self.root, path);
        let mut state = ScannerState {
            in_block_comment: false,
            in_template: false,
            last_significant_char: None,
        };

        for (idx, raw_line) in content.lines().enumerate() {
            let line_no = idx + 1;
            let maybe_scan_needed = state.in_block_comment
                || state.in_template
                || (raw_line.as_bytes().contains(&b'/')
                    && (raw_line.contains("//") || raw_line.contains("/*")))
                || raw_line.contains('`');

            let stripped = if maybe_scan_needed {
                Cow::Owned(strip_js_comments(raw_line, &mut state))
            } else {
                Cow::Borrowed(raw_line)
            };
            let scan_text = stripped.trim_end();

            if scan_text.is_empty() {
                continue;
            }

            let markers = Self::detect_scan_markers(scan_text);
            if markers & (MARKER_IMPORT | MARKER_REQUIRE) != 0 {
                Self::scan_imports_in_line(
                    &rel, line_no, scan_text, caps, rewrites, forbidden, flagged,
                );
            }

            if markers & (MARKER_PI | MARKER_PROCESS_ENV) != 0 {
                Self::scan_pi_apis_in_line(&rel, line_no, scan_text, caps);
            }

            if markers & (MARKER_FUNCTION | MARKER_EVAL) != 0 {
                Self::scan_flagged_apis_in_line(&rel, line_no, scan_text, flagged);
            }

            if (markers & MARKER_PROCESS) != 0 && (markers & (MARKER_BINDING | MARKER_DLOPEN) != 0)
            {
                Self::scan_forbidden_patterns_in_line(&rel, line_no, scan_text, forbidden);
            }
        }

        Ok(())
    }

    #[must_use]
    fn detect_scan_markers(text: &str) -> u16 {
        let bytes = text.as_bytes();
        let mut markers = 0_u16;
        let mut idx = 0;

        while idx < bytes.len() {
            match bytes[idx] {
                b'i' if bytes[idx..].starts_with(b"import") => markers |= MARKER_IMPORT,
                b'r' if bytes[idx..].starts_with(b"require") => markers |= MARKER_REQUIRE,
                b'p' => {
                    if bytes[idx..].starts_with(b"pi") {
                        markers |= MARKER_PI;
                    }
                    if bytes[idx..].starts_with(b"process") {
                        markers |= MARKER_PROCESS;
                        if bytes[idx..].starts_with(b"process.env") {
                            markers |= MARKER_PROCESS_ENV;
                        }
                    }
                }
                b'F' if bytes[idx..].starts_with(b"Function") => markers |= MARKER_FUNCTION,
                b'e' if bytes[idx..].starts_with(b"eval") => markers |= MARKER_EVAL,
                b'b' if bytes[idx..].starts_with(b"binding") => markers |= MARKER_BINDING,
                b'd' if bytes[idx..].starts_with(b"dlopen") => markers |= MARKER_DLOPEN,
                _ => {}
            }

            if (markers & (MARKER_IMPORT | MARKER_REQUIRE) != 0)
                && (markers & (MARKER_PI | MARKER_PROCESS_ENV) != 0)
                && (markers & (MARKER_FUNCTION | MARKER_EVAL) != 0)
                && (markers & MARKER_PROCESS != 0)
                && (markers & (MARKER_BINDING | MARKER_DLOPEN) != 0)
            {
                break;
            }
            idx += 1;
        }

        markers
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
        if text.contains("Function")
            && let Some(column) = find_regex_column(text, new_function_regex())
        {
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

        if text.contains("eval")
            && let Some(column) = find_regex_column(text, eval_regex())
        {
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
        if text.contains("process") {
            if text.contains("binding")
                && let Some(column) = find_regex_column(text, binding_regex())
            {
                let evidence =
                    CompatEvidence::new(file.to_string(), line, column, text.to_string());
                forbidden
                    .entry((
                        "forbidden_api".to_string(),
                        "process.binding(...)".to_string(),
                        "Native module access is forbidden; remove this usage.".to_string(),
                    ))
                    .or_default()
                    .push(evidence);
            }

            if text.contains("dlopen")
                && let Some(column) = find_regex_column(text, dlopen_regex())
            {
                let evidence =
                    CompatEvidence::new(file.to_string(), line, column, text.to_string());
                forbidden
                    .entry((
                        "forbidden_api".to_string(),
                        "process.dlopen(...)".to_string(),
                        "Native addon loading is forbidden; remove this usage.".to_string(),
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
    // Paths are all rooted under `path`, so sorting by the full `PathBuf`
    // yields the same deterministic order as sorting by relative string keys
    // without per-entry key allocation.
    out.sort_unstable();
    Ok(out)
}

fn collect_js_like_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current_dir) = stack.pop() {
        let entries = fs::read_dir(&current_dir).map_err(|err| {
            Error::extension(format!(
                "Failed to read extension source directory {}: {err}",
                current_dir.display()
            ))
        })?;

        for entry in entries {
            let entry = entry.map_err(|err| {
                Error::extension(format!(
                    "Failed to enumerate extension source entry in {}: {err}",
                    current_dir.display()
                ))
            })?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(|err| {
                Error::extension(format!(
                    "Failed to inspect extension source entry {}: {err}",
                    path.display()
                ))
            })?;

            if file_type.is_dir() {
                if should_ignore_dir(&path) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() && is_js_like(&path) {
                out.push(path);
            }
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
    matches!(
        ext,
        "ts" | "js" | "tsx" | "jsx" | "mjs" | "cjs" | "mts" | "cts"
    )
}

fn relative_posix(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    if rel.as_os_str().is_empty() {
        return path.file_name().and_then(|name| name.to_str()).map_or_else(
            || path.to_string_lossy().replace('\\', "/"),
            ToString::to_string,
        );
    }
    let mut out = String::new();
    for component in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
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
    RE.get_or_init(|| {
        Regex::new(r#"\bimport\s*\(\s*["'`]((?:[^"'`]+))["'`]\s*\)"#).expect("import()")
    })
}

fn require_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\brequire\s*\(\s*["'`]((?:[^"'`]+))["'`]\s*\)"#).expect("require")
    })
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
    RE.get_or_init(|| Regex::new(r#"\bpi\.tool\s*\(\s*["'`]((?:[^"'`]+))["'`]"#).expect("pi.tool"))
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

fn binding_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"process\s*\.\s*binding\s*\(").expect("binding regex"))
}

fn dlopen_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"process\s*\.\s*dlopen\s*\(").expect("dlopen regex"))
}

const fn is_js_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn parse_top_level_import_specifier(line: &str) -> Option<(String, usize)> {
    let trimmed = line.trim_start();
    let leading_ws = line.len().saturating_sub(trimmed.len());
    let bytes = trimmed.as_bytes();

    if !trimmed.starts_with("import") {
        return None;
    }

    let mut idx = "import".len();
    if bytes.get(idx).is_some_and(|b| is_js_ident_continue(*b)) {
        return None;
    }

    while idx < bytes.len() && bytes[idx].is_ascii_whitespace() {
        idx += 1;
    }

    if idx >= bytes.len() {
        return None;
    }

    // Optional `import type ...`.
    if trimmed[idx..].starts_with("type") {
        let after_type = idx + "type".len();
        if bytes
            .get(after_type)
            .is_some_and(|b| is_js_ident_continue(*b))
        {
            // Not a standalone `type` keyword.
        } else {
            let mut k = after_type;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k > after_type {
                idx = k;
            }
        }
    }

    if idx >= bytes.len() {
        return None;
    }

    // Side-effect import: `import "pkg"`.
    if matches!(bytes[idx], b'"' | b'\'') {
        let quote = bytes[idx];
        let start = idx + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end] != quote {
            end += 1;
        }
        if end < bytes.len() {
            let spec = trimmed[start..end].to_string();
            return Some((spec, leading_ws + start + 1));
        }
        return None;
    }

    // Standard import: `import ... from "pkg"`.
    let mut search_from = idx;
    while let Some(rel) = trimmed[search_from..].find("from") {
        let from_idx = search_from + rel;
        let after_from = from_idx + "from".len();
        let before_ok = from_idx == 0 || !is_js_ident_continue(bytes[from_idx - 1]);
        let after_ok = after_from >= bytes.len() || !is_js_ident_continue(bytes[after_from]);
        if before_ok && after_ok {
            let mut k = after_from;
            while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                k += 1;
            }
            if k < bytes.len() && matches!(bytes[k], b'"' | b'\'') {
                let quote = bytes[k];
                let start = k + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != quote {
                    end += 1;
                }
                if end < bytes.len() {
                    let spec = trimmed[start..end].to_string();
                    return Some((spec, leading_ws + start + 1));
                }
                return None;
            }
        }
        search_from = after_from;
    }

    None
}

fn extract_import_specifiers(line: &str) -> Vec<(String, usize)> {
    if !line.contains("import") {
        return Vec::new();
    }

    let mut out = Vec::new();

    let top_level = parse_top_level_import_specifier(line);
    if let Some((specifier, column)) = &top_level {
        out.push((specifier.clone(), *column));
    } else {
        if let Some(caps) = import_from_regex().captures(line)
            && let Some(m) = caps.get(1)
        {
            out.push((m.as_str().to_string(), m.start() + 1));
        }

        if let Some(caps) = import_side_effect_regex().captures(line)
            && let Some(m) = caps.get(1)
        {
            out.push((m.as_str().to_string(), m.start() + 1));
        }
    }

    if line.contains('(') {
        for caps in import_dynamic_regex().captures_iter(line) {
            if let Some(m) = caps.get(1) {
                let candidate = (m.as_str().to_string(), m.start() + 1);
                if !out.contains(&candidate) {
                    out.push(candidate);
                }
            }
        }
    }

    out
}

fn extract_require_specifiers(line: &str) -> Vec<(String, usize)> {
    if !line.contains("require") {
        return Vec::new();
    }

    if !line.contains('(') {
        return Vec::new();
    }

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

    if !line.contains("pi") {
        return out;
    }

    if line.contains("pi.tool") {
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
    }

    if line.contains("pi.exec")
        && let Some(column) = find_regex_column(line, pi_exec_regex())
    {
        out.push(("exec".to_string(), "pi.exec".to_string(), column));
    }

    if line.contains("pi.http")
        && let Some(column) = find_regex_column(line, pi_http_regex())
    {
        out.push(("http".to_string(), "pi.http".to_string(), column));
    }

    if line.contains("pi.log")
        && let Some(column) = find_regex_column(line, pi_log_regex())
    {
        out.push(("log".to_string(), "pi.log".to_string(), column));
    }

    if line.contains("pi.session")
        && let Some(column) = find_regex_column(line, pi_session_regex())
    {
        out.push(("session".to_string(), "pi.session.*".to_string(), column));
    }

    if line.contains("pi.ui")
        && let Some(column) = find_regex_column(line, pi_ui_regex())
    {
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

/// Strip single-line (`//`) and block (`/* ... */`) JS comments from a line,
/// respecting string literals (double/single/backtick) and regex literals.
///
/// `state` carries block-comment and template-literal state across lines.
#[allow(clippy::too_many_lines)]
fn strip_js_comments(line: &str, state: &mut ScannerState) -> String {
    let mut result = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_regex = false;
    let mut in_regex_class = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if state.in_block_comment {
            if ch == '*' && matches!(chars.peek(), Some('/')) {
                chars.next();
                state.in_block_comment = false;
            }
            continue;
        }

        if state.in_template {
            if escaped {
                result.push(ch);
                escaped = false;
                if !ch.is_whitespace() {
                    state.last_significant_char = Some(ch);
                }
                continue;
            }
            if ch == '\\' {
                result.push(ch);
                escaped = true;
                continue;
            }
            if ch == '`' {
                state.in_template = false;
                state.last_significant_char = Some(ch);
            } else if !ch.is_whitespace() {
                state.last_significant_char = Some(ch);
            }
            result.push(ch);
            continue;
        }

        if escaped {
            result.push(ch);
            escaped = false;
            if !ch.is_whitespace() {
                state.last_significant_char = Some(ch);
            }
            continue;
        }

        if ch == '\\' && (in_single_quote || in_double_quote || in_regex) {
            result.push(ch);
            escaped = true;
            continue;
        }

        if in_single_quote {
            if ch == '\'' {
                in_single_quote = false;
            }
            result.push(ch);
            state.last_significant_char = Some(ch);
            continue;
        }

        if in_double_quote {
            if ch == '"' {
                in_double_quote = false;
            }
            result.push(ch);
            state.last_significant_char = Some(ch);
            continue;
        }

        if in_regex {
            if in_regex_class {
                if ch == ']' {
                    in_regex_class = false;
                }
            } else if ch == '[' {
                in_regex_class = true;
            } else if ch == '/' {
                in_regex = false;
            }
            result.push(ch);
            state.last_significant_char = Some(ch);
            continue;
        }

        match ch {
            '/' => {
                if matches!(chars.peek(), Some(&'/')) {
                    break;
                }
                if matches!(chars.peek(), Some(&'*')) {
                    chars.next();
                    state.in_block_comment = true;
                    continue;
                }

                // Disambiguate regex start vs division.
                let is_regex_start = state.last_significant_char.is_none_or(|c| {
                    matches!(
                        c,
                        '=' | '('
                            | ')'
                            | ','
                            | ':'
                            | ';'
                            | '!'
                            | '&'
                            | '|'
                            | '?'
                            | '['
                            | ']'
                            | '{'
                            | '}'
                            | '^'
                            | '~'
                            | '*'
                            | '+'
                            | '-'
                            | '<'
                            | '>'
                    )
                });

                if is_regex_start {
                    in_regex = true;
                }
                result.push(ch);
                state.last_significant_char = Some(ch);
            }
            '\'' => {
                in_single_quote = true;
                result.push(ch);
                state.last_significant_char = Some(ch);
            }
            '"' => {
                in_double_quote = true;
                result.push(ch);
                state.last_significant_char = Some(ch);
            }
            '`' => {
                state.in_template = true;
                result.push(ch);
                state.last_significant_char = Some(ch);
            }
            c if c.is_whitespace() => {
                result.push(c);
            }
            c => {
                result.push(c);
                state.last_significant_char = Some(c);
            }
        }
    }

    result
}

#[cfg(test)]
mod compatibility_scanner_comment_tests {
    use super::{CompatibilityScanner, ScannerState, strip_js_comments};
    use std::fs;

    #[test]
    fn strip_js_comments_keeps_comment_markers_inside_strings() {
        let mut state = ScannerState {
            in_block_comment: false,
            in_template: false,
            last_significant_char: None,
        };
        let line = r#"const code = "import('fs') // not a comment"; // real comment"#;
        let stripped = strip_js_comments(line, &mut state);
        assert_eq!(
            stripped.trim(),
            r#"const code = "import('fs') // not a comment";"#
        );
        assert!(!state.in_block_comment);
    }

    #[test]
    fn strip_js_comments_keeps_comment_markers_inside_regex() {
        let mut state = ScannerState {
            in_block_comment: false,
            in_template: false,
            last_significant_char: None,
        };
        // Regex matching `//` inside a class: /[//]/
        // Followed by code: ; import 'fs';
        let line = r"const r = /[//]/; import 'fs'; // real comment";
        let stripped = strip_js_comments(line, &mut state);
        assert_eq!(stripped.trim(), r"const r = /[//]/; import 'fs';");
        assert!(!state.in_block_comment);

        // Regex matching `/*` inside a class: /[\/*]/
        let mut state2 = ScannerState {
            in_block_comment: false,
            in_template: false,
            last_significant_char: None,
        };
        let line2 = r"const r2 = /[\/*]/; import 'path'; /* real comment */";
        let stripped2 = strip_js_comments(line2, &mut state2);
        assert_eq!(stripped2.trim(), r"const r2 = /[\/*]/; import 'path';");
        assert!(!state2.in_block_comment);
    }

    #[test]
    fn strip_js_comments_handles_multiline_templates() {
        let mut state = ScannerState {
            in_block_comment: false,
            in_template: false,
            last_significant_char: None,
        };

        // Line 1: open template
        let line1 = "const s = `";
        let stripped1 = strip_js_comments(line1, &mut state);
        assert_eq!(stripped1, "const s = `");
        assert!(state.in_template);

        // Line 2: content with pseudo-comment
        let line2 = "/* not a comment */";
        let stripped2 = strip_js_comments(line2, &mut state);
        assert_eq!(stripped2, "/* not a comment */");
        assert!(state.in_template);
        assert!(!state.in_block_comment);

        // Line 3: close template
        let line3 = "`; // real comment";
        let stripped3 = strip_js_comments(line3, &mut state);
        assert_eq!(stripped3, "`; ");
        assert!(!state.in_template);
    }

    #[test]
    fn compatibility_scanner_ignores_commented_patterns() {
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("commented.js");
        fs::write(
            &entry,
            r#"
// import fs from "fs";
// pi.exec("echo should-not-count");
/* process.binding("fs");
   eval("bad");
*/
"#,
        )
        .expect("write test file");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let ledger = scanner.scan_path(&entry).expect("scan");

        assert!(ledger.capabilities.is_empty());
        assert!(ledger.rewrites.is_empty());
        assert!(ledger.forbidden.is_empty());
        assert!(ledger.flagged.is_empty());
    }

    #[test]
    fn compatibility_scanner_ignores_comment_markers_in_templates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("template.js");
        // This test case ensures that `/*` inside a template literal doesn't start
        // a block comment that hides subsequent code.
        fs::write(
            &entry,
            r#"
const s = `
/* not a comment
`;
import fs from "fs";
"#,
        )
        .expect("write test file");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let ledger = scanner.scan_path(&entry).expect("scan");

        assert!(
            ledger.capabilities.iter().any(|c| c.capability == "read"),
            "import fs should be detected even if preceded by pseudo-comment in template"
        );
    }

    #[test]
    fn compatibility_scanner_still_reports_live_code_with_nearby_comments() {
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("mixed.js");
        fs::write(
            &entry,
            r#"
/* import child_process from "child_process"; */
import fs from "fs"; // real import
pi.exec("echo hello");
"#,
        )
        .expect("write test file");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let ledger = scanner.scan_path(&entry).expect("scan");

        assert_eq!(
            ledger.rewrites.len(),
            1,
            "live fs import should be rewritten"
        );
        assert!(
            ledger
                .rewrites
                .iter()
                .any(|rewrite| rewrite.from == "fs" && rewrite.to == "pi:node/fs")
        );
        assert!(
            ledger
                .capabilities
                .iter()
                .any(|cap| cap.capability == "read")
        );
        assert!(
            ledger
                .capabilities
                .iter()
                .any(|cap| cap.capability == "write")
        );
        assert!(
            ledger
                .capabilities
                .iter()
                .any(|cap| cap.capability == "exec")
        );
        assert!(ledger.forbidden.is_empty());
        assert!(ledger.flagged.is_empty());
    }

    #[test]
    fn compatibility_scanner_scan_path_fails_on_missing_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing_dir = temp.path().join("missing");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let err = scanner
            .scan_path(&missing_dir)
            .expect_err("scan should fail closed");

        let err_text = err.to_string();
        assert!(err_text.contains("Failed to read extension source directory"));
        assert!(err_text.contains(&missing_dir.display().to_string()));
    }

    #[test]
    fn compatibility_scanner_scan_path_fails_on_non_utf8_source_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let clean = temp.path().join("clean.js");
        fs::write(&clean, "export default {};\n").expect("write clean file");

        let blocked = temp.path().join("blocked.js");
        fs::write(&blocked, [0xff, 0xfe, 0xfd]).expect("write non-UTF-8 source file");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let err = scanner
            .scan_path(temp.path())
            .expect_err("scan should fail closed");

        let err_text = err.to_string();
        assert!(err_text.contains("Failed to read extension source file"));
        assert!(err_text.contains(&blocked.display().to_string()));
    }

    #[test]
    fn compatibility_scanner_keeps_late_requires_in_minified_lines() {
        let mut sample_content = String::from(
            r#"const endpoint="https://example.invalid/assets";const matcher=/https?:\/\//;"#,
        );
        sample_content.push_str(&"const bundledValue=0;".repeat(512));
        sample_content.push_str(r#"const cp=require("child_process");cp.spawnSync("true");"#);
        assert!(
            sample_content.len() > 4096,
            "sample must exercise scanning beyond the former long-line fallback threshold"
        );

        let mut state = ScannerState {
            in_block_comment: false,
            in_template: false,
            last_significant_char: None,
        };
        let stripped = strip_js_comments(&sample_content, &mut state);
        assert_eq!(
            stripped, sample_content,
            "quoted URLs and escaped regex slashes must not truncate live minified code"
        );

        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("bundle.js");
        fs::write(&entry, sample_content).expect("write bundle sample");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let ledger = scanner.scan_path(&entry).expect("scan");

        assert!(
            ledger
                .capabilities
                .iter()
                .any(|cap| cap.capability == "exec" && cap.reason == "import:child_process"),
            "minified bundle should still infer exec capability from child_process require"
        );
    }

    #[test]
    fn compatibility_scanner_ignores_commented_patterns_on_long_minified_lines() {
        let mut sample_content = "const bundledValue=0;".repeat(512);
        sample_content.push_str(
            r#"// require("child_process");pi.exec("false");eval("bad");process.binding("fs");"#,
        );
        assert!(sample_content.len() > 4096, "sample must be a long line");

        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("commented-bundle.js");
        fs::write(&entry, sample_content).expect("write commented bundle sample");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let ledger = scanner.scan_path(&entry).expect("scan");

        assert!(ledger.capabilities.is_empty());
        assert!(ledger.rewrites.is_empty());
        assert!(ledger.forbidden.is_empty());
        assert!(ledger.flagged.is_empty());
    }

    #[test]
    fn compatibility_scanner_keeps_live_api_after_long_inline_block_comment() {
        let mut sample_content = "const bundledValue=0;".repeat(512);
        sample_content.push_str(
            r#"/* require("fs");eval("bad");process.binding("fs"); */const cp=require("child_process");cp.spawnSync("true");"#,
        );
        assert!(sample_content.len() > 4096, "sample must be a long line");

        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("mixed-bundle.js");
        fs::write(&entry, sample_content).expect("write mixed bundle sample");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let ledger = scanner.scan_path(&entry).expect("scan");

        assert!(
            ledger
                .capabilities
                .iter()
                .any(|cap| cap.capability == "exec" && cap.reason == "import:child_process"),
            "live child_process require after a long comment must remain visible"
        );
        assert!(
            !ledger
                .capabilities
                .iter()
                .any(|cap| cap.reason == "import:fs"),
            "commented fs require must stay ignored"
        );
        assert!(
            ledger
                .rewrites
                .iter()
                .any(|rewrite| rewrite.from == "child_process"),
            "live child_process require must retain its rewrite evidence"
        );
        assert!(
            !ledger.rewrites.iter().any(|rewrite| rewrite.from == "fs"),
            "commented fs require must not create rewrite evidence"
        );
        assert!(ledger.forbidden.is_empty());
        assert!(ledger.flagged.is_empty());
    }

    #[test]
    fn compatibility_scanner_single_file_preserves_filename_in_evidence() {
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("single-file.js");
        fs::write(&entry, "import fs from 'fs';\n").expect("write single-file extension");

        let scanner = CompatibilityScanner::new(entry.clone());
        let ledger = scanner.scan_path(&entry).expect("scan");

        let rewrite = ledger
            .rewrites
            .iter()
            .find(|rewrite| rewrite.from == "fs" && rewrite.to == "pi:node/fs")
            .expect("fs rewrite");
        assert_eq!(
            rewrite.evidence[0].file, "single-file.js",
            "single-file scans should keep the filename instead of an empty relative path"
        );
    }

    #[test]
    fn compatibility_scanner_detects_backtick_tool_calls() {
        let temp = tempfile::tempdir().expect("tempdir");
        let entry = temp.path().join("backtick.js");
        fs::write(
            &entry,
            r#"
pi.tool(`read`, { path: "file.txt" });
"#,
        )
        .expect("write test file");

        let scanner = CompatibilityScanner::new(temp.path().to_path_buf());
        let ledger = scanner.scan_path(&entry).expect("scan");

        assert!(
            ledger
                .capabilities
                .iter()
                .any(|cap| cap.capability == "read"),
            "pi.tool(`read`) should be detected"
        );
    }
}
