//! Agent-facing structural code tools: `ast_grep` and `ast_edit`.
//!
//! These tools expose tree-sitter structural search and staged structural
//! rewrite to the agent, built on the same `ast-grep` grammars already used
//! for exec mediation (`src/extensions/exec_mediation.rs`). This module owns
//! the agent-facing tool surface, which is deliberately distinct from exec
//! mediation: mediation classifies heredoc scripts for policy, while these
//! tools let the agent query and rewrite source code structurally.
//!
//! # Grammar coverage
//!
//! Per-file parsing by extension: rust, python, javascript, typescript, tsx,
//! bash, go, ruby (the exec-mediation set plus rust/tsx/go).
//!
//! # `ast_edit` staging lifecycle
//!
//! `ast_edit` never writes on the first call. `action: "stage"` computes all
//! replacements and returns a proposal id, a replacement count, and per-file
//! unified diff previews. `action: "resolve"` with the proposal id and a
//! one-line reason applies the proposal atomically: every file is re-hashed
//! at apply time (any drift rejects the whole proposal, naming the file),
//! then each file is written via temp-file + rename; a mid-apply failure
//! rolls back already-written files from the staged originals.
//! `action: "reject"` discards the proposal with zero writes.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use ast_grep_core::{AstGrep, Pattern};
use ast_grep_language::SupportLang;
use async_trait::async_trait;
use serde::Deserialize;
use sha2::Digest as _;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// ============================================================================
// Limits and caps
// ============================================================================

/// Default maximum matches returned by `ast_grep`.
const DEFAULT_MATCH_LIMIT: usize = 100;
/// Hard cap on matches returned by `ast_grep`.
const HARD_MATCH_LIMIT: usize = 1000;
/// Per-match matched-text cap in characters.
const MATCH_TEXT_CAP: usize = 500;
/// Hard cap on files scanned per invocation (prevents OOM/hangs on huge trees).
const MAX_SCAN_FILES: usize = 20_000;
/// Files larger than this are skipped (prevents OOM on generated artifacts).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Default maximum replacements an `ast_edit` proposal may contain.
const DEFAULT_MAX_REPLACEMENTS: usize = 500;
/// Hard cap on replacements per `ast_edit` proposal.
const HARD_MAX_REPLACEMENTS: usize = 5000;
/// Maximum staged proposals retained per tool instance (oldest evicted).
const MAX_STAGED_PROPOSALS: usize = 32;
/// Cap on the per-file diff preview retained in a proposal (bytes).
const DIFF_PREVIEW_MAX_BYTES: usize = 64 * 1024;

// ============================================================================
// Language registry
// ============================================================================

/// Languages supported by the structural tools.
///
/// Coverage is the exec-mediation grammar set (bash, python, javascript,
/// typescript, ruby) plus rust, tsx, and go. Each file is parsed in its own
/// language; tree-sitter grammars never match comments or string contents as
/// code structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AstLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Bash,
    Go,
    Ruby,
}

impl AstLanguage {
    /// Canonical language name used in output and errors.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Bash => "bash",
            Self::Go => "go",
            Self::Ruby => "ruby",
        }
    }

    #[must_use]
    const fn support_lang(self) -> SupportLang {
        match self {
            Self::Rust => SupportLang::Rust,
            Self::Python => SupportLang::Python,
            Self::JavaScript => SupportLang::JavaScript,
            Self::TypeScript => SupportLang::TypeScript,
            Self::Tsx => SupportLang::Tsx,
            Self::Bash => SupportLang::Bash,
            Self::Go => SupportLang::Go,
            Self::Ruby => SupportLang::Ruby,
        }
    }

    /// All supported languages, in canonical order.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Rust,
            Self::Python,
            Self::JavaScript,
            Self::TypeScript,
            Self::Tsx,
            Self::Bash,
            Self::Go,
            Self::Ruby,
        ]
    }

    /// Detect a language from a file extension (without the dot).
    #[must_use]
    fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" | "jsx" => Some(Self::Tsx),
            "sh" | "bash" => Some(Self::Bash),
            "go" => Some(Self::Go),
            "rb" => Some(Self::Ruby),
            _ => None,
        }
    }

    /// Resolve an explicit `lang` override (case-insensitive, common aliases).
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "rust" | "rs" => Some(Self::Rust),
            "python" | "py" => Some(Self::Python),
            "javascript" | "js" => Some(Self::JavaScript),
            "typescript" | "ts" => Some(Self::TypeScript),
            "tsx" | "jsx" => Some(Self::Tsx),
            "bash" | "sh" | "shell" => Some(Self::Bash),
            "go" | "golang" => Some(Self::Go),
            "ruby" | "rb" => Some(Self::Ruby),
            _ => None,
        }
    }

    /// Detect the language for a path from its extension.
    #[must_use]
    pub fn for_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::from_extension)
    }
}

// ============================================================================
// Shared helpers
// ============================================================================

/// Resolve a user-supplied path against the tool working directory.
fn resolve_tool_path(path: &str, cwd: &Path) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    }
}

/// Display path relative to the working directory when possible.
fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .map_or_else(|_| path.display().to_string(), |rel| rel.display().to_string())
}

/// SHA-256 hex digest of file content (stale-anchor hash).
fn content_hash(content: &str) -> String {
    let digest = sha2::Sha256::digest(content.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Build a gitignore-style glob override filter anchored at `cwd`.
fn build_glob_override(
    cwd: &Path,
    glob: Option<&str>,
    tool: &str,
) -> Result<Option<ignore::overrides::Override>> {
    let Some(glob) = glob else {
        return Ok(None);
    };
    let mut builder = ignore::overrides::OverrideBuilder::new(cwd);
    builder
        .add(glob)
        .map_err(|error| Error::validation(format!("invalid glob '{glob}': {error}")))?;
    builder.build().map(Some).map_err(|error| {
        Error::tool(
            tool,
            format!("failed to build glob override '{glob}': {error}"),
        )
    })
}

/// Whether a path passes the glob override (matched relative to `cwd`).
fn glob_allows(path: &Path, cwd: &Path, glob_override: Option<&ignore::overrides::Override>) -> bool {
    let Some(glob_override) = glob_override else {
        return true;
    };
    let logical = path.strip_prefix(cwd).unwrap_or(path);
    !glob_override.matched(logical, false).is_ignore()
}

/// Outcome of a scoped file scan.
struct ScanOutcome {
    /// Candidate files (language-detectable unless a `lang` override is set),
    /// sorted for deterministic ordering.
    files: Vec<PathBuf>,
    /// Whether the scan hit `MAX_SCAN_FILES` and skipped the remainder.
    truncated: bool,
}

/// Collect candidate files under `root` (file or directory), honoring
/// gitignore rules, the optional glob override, and language detection.
fn collect_files(
    root: &Path,
    cwd: &Path,
    glob: Option<&str>,
    lang_override: Option<AstLanguage>,
    tool: &str,
) -> Result<ScanOutcome> {
    let glob_override = build_glob_override(cwd, glob, tool)?;
    let mut files = Vec::new();
    let mut truncated = false;

    if root.is_file() {
        if glob_allows(root, cwd, glob_override.as_ref())
            && (lang_override.is_some() || AstLanguage::for_path(root).is_some())
        {
            files.push(root.to_path_buf());
        }
        return Ok(ScanOutcome {
            files,
            truncated: false,
        });
    }

    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .current_dir(cwd)
        .hidden(false)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false);

    for entry in builder.build().flatten() {
        if files.len() >= MAX_SCAN_FILES {
            truncated = true;
            break;
        }
        let path = entry.path();
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if lang_override.is_none() && AstLanguage::for_path(path).is_none() {
            continue;
        }
        if !glob_allows(path, cwd, glob_override.as_ref()) {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files.sort();
    Ok(ScanOutcome { files, truncated })
}

/// Compile a search pattern for a language, producing a named parse error.
fn compile_pattern(pattern: &str, lang: AstLanguage) -> std::result::Result<Pattern, String> {
    Pattern::try_new(pattern, lang.support_lang()).map_err(|error| {
        format!(
            "pattern `{pattern}` failed to parse as {}: {error}",
            lang.name()
        )
    })
}

/// Read a file as UTF-8, skipping oversize or unreadable files.
fn read_source_file(path: &Path) -> std::result::Result<Option<String>, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot stat '{}': {error}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Ok(None);
    }
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => Ok(None),
        Err(error) => Err(format!("cannot read '{}': {error}", path.display())),
    }
}

/// Render a unified diff preview between two file contents.
fn unified_diff_preview(display: &str, old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut text = diff
        .unified_diff()
        .context_radius(3)
        .header(format!("a/{display}"), format!("b/{display}"))
        .to_string();
    if text.len() > DIFF_PREVIEW_MAX_BYTES {
        let mut end = DIFF_PREVIEW_MAX_BYTES;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n... [diff preview truncated]\n");
    }
    text
}

/// Write file content atomically via temp-file + rename in the same directory.
fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp = path.with_file_name(format!(
        ".{file_name}.pi-ast-edit-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let result = std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn text_output(text: String, details: serde_json::Value) -> ToolOutput {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details),
        is_error: false,
    }
}

// ============================================================================
// ast_grep tool
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AstGrepInput {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    lang: Option<String>,
    limit: Option<usize>,
}

/// A single structural match (owned, JSON-serializable).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AstMatch {
    file: String,
    language: &'static str,
    start_line: usize,
    start_column: usize,
    end_line: usize,
    end_column: usize,
    matched: String,
    matched_truncated: bool,
}

/// Structural code search tool (`ast_grep`).
///
/// Patterns are ast-grep patterns: they match AST structure, not text.
/// `$NAME` captures exactly one node, `$$$NAME` captures zero or more nodes,
/// and reusing the same `$NAME` twice in one pattern requires identical code
/// at both sites.
pub struct AstGrepTool {
    cwd: PathBuf,
}

impl AstGrepTool {
    #[must_use]
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    fn run(&self, input: AstGrepInput) -> Result<ToolOutput> {
        if input.pattern.trim().is_empty() {
            return Err(Error::validation("`pattern` must not be empty"));
        }
        if input.pattern.len() > 4096 {
            return Err(Error::validation("`pattern` exceeds 4096 characters"));
        }
        let limit = input.limit.unwrap_or(DEFAULT_MATCH_LIMIT);
        if limit == 0 || limit > HARD_MATCH_LIMIT {
            return Err(Error::validation(format!(
                "`limit` must be between 1 and {HARD_MATCH_LIMIT}"
            )));
        }
        let lang_override = input
            .lang
            .as_deref()
            .map(|name| {
                AstLanguage::from_name(name).ok_or_else(|| {
                    Error::validation(format!(
                        "unknown lang '{name}'; supported: rust, python, javascript, typescript, tsx, bash, go, ruby"
                    ))
                })
            })
            .transpose()?;

        let root = resolve_tool_path(input.path.as_deref().unwrap_or("."), &self.cwd);
        if !root.exists() {
            return Err(Error::validation(format!(
                "path not found: {}",
                input.path.as_deref().unwrap_or(".")
            )));
        }
        let scan = collect_files(&root, &self.cwd, input.glob.as_deref(), lang_override, "ast_grep")?;

        // Compile the pattern once per in-scope language.
        let mut compiled: HashMap<AstLanguage, std::result::Result<Pattern, String>> =
            HashMap::new();
        for file in &scan.files {
            let lang = lang_override.unwrap_or_else(|| {
                AstLanguage::for_path(file).expect("scan only collects language-detectable files")
            });
            compiled
                .entry(lang)
                .or_insert_with(|| compile_pattern(&input.pattern, lang));
        }
        let pattern_errors: Vec<String> = compiled
            .values()
            .filter_map(|result| result.as_ref().err().cloned())
            .collect();
        if !compiled.is_empty() && pattern_errors.len() == compiled.len() {
            return Err(Error::validation(format!(
                "[AST_PATTERN_PARSE] {}",
                pattern_errors.join("; ")
            )));
        }

        let mut matches: Vec<AstMatch> = Vec::new();
        let mut truncated = scan.truncated;
        let mut skipped_files = 0_usize;
        'files: for file in &scan.files {
            let lang = lang_override.unwrap_or_else(|| {
                AstLanguage::for_path(file).expect("scan only collects language-detectable files")
            });
            let Ok(pattern) = &compiled[&lang] else {
                skipped_files += 1;
                continue;
            };
            let content = match read_source_file(file) {
                Ok(Some(content)) => content,
                Ok(None) => {
                    skipped_files += 1;
                    continue;
                }
                Err(message) => return Err(Error::tool("ast_grep", message)),
            };
            let grep = AstGrep::new(content.as_str(), lang.support_lang());
            for node in grep.root().find_all(pattern) {
                let start = node.start_pos().byte_point();
                let end = node.end_pos().byte_point();
                let matched_text = node.text();
                let mut matched = String::with_capacity(matched_text.len().min(MATCH_TEXT_CAP));
                let mut matched_truncated = false;
                for (count, ch) in matched_text.chars().enumerate() {
                    if count >= MATCH_TEXT_CAP {
                        matched_truncated = true;
                        break;
                    }
                    matched.push(ch);
                }
                if matched_truncated {
                    matched.push_str("... [truncated]");
                }
                matches.push(AstMatch {
                    file: display_path(file, &self.cwd),
                    language: lang.name(),
                    start_line: start.0 + 1,
                    start_column: start.1,
                    end_line: end.0 + 1,
                    end_column: end.1,
                    matched,
                    matched_truncated,
                });
                if matches.len() >= limit {
                    truncated = true;
                    break 'files;
                }
            }
        }

        let payload = serde_json::json!({
            "pattern": input.pattern,
            "matches": matches,
            "matchCount": matches.len(),
            "truncated": truncated,
            "filesScanned": scan.files.len(),
            "skippedFiles": skipped_files,
            "patternErrors": pattern_errors,
        });
        let text = serde_json::to_string_pretty(&payload)
            .unwrap_or_else(|_| "{\"error\":\"serialization failed\"}".to_string());
        Ok(text_output(text, payload))
    }
}

#[async_trait]
impl Tool for AstGrepTool {
    fn name(&self) -> &str {
        "ast_grep"
    }

    fn label(&self) -> &str {
        "ast_grep"
    }

    fn description(&self) -> &str {
        "Structural code search using tree-sitter AST patterns (ast-grep syntax). Patterns match AST structure, NOT text: `$EXPR.unwrap()` matches real `.unwrap()` calls and ignores comments and strings that merely contain `unwrap()`. Metavariables: `$NAME` captures exactly one AST node; `$$$NAME` captures zero or more nodes (use `$$$NAME`, NOT `$$NAME`). Reusing the same `$NAME` twice in one pattern requires identical code at both sites. Returns JSON matches with file, 1-based line, byte column, and matched text (capped at 500 chars each, `limit` matches total). Languages: rust, python, javascript, typescript, tsx, bash, go, ruby — each file is parsed in its own language by extension; override with `lang`."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "ast-grep structural pattern, e.g. `$EXPR.unwrap()` or `foo($$$ARGS)`. Matches AST structure, not text."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search (default: current directory). Respects .gitignore."
                },
                "glob": {
                    "type": "string",
                    "description": "Filter files by glob, e.g. '**/*.rs' or 'src/**/*.ts'"
                },
                "lang": {
                    "type": "string",
                    "description": "Force one language for all in-scope files: rust|python|javascript|typescript|tsx|bash|go|ruby"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum matches to return (default 100, hard cap 1000)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: AstGrepInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        self.run(input)
    }
}

// ============================================================================
// ast_edit tool
// ============================================================================

/// One rewrite operation: replace matches of `pat` with `out`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AstEditOp {
    /// Structural pattern to match (single AST node).
    pub pat: String,
    /// Replacement template (`$NAME`/`$$$NAME` substituted from the match).
    /// An empty string deletes the matched node.
    pub out: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AstEditInput {
    action: Option<String>,
    ops: Option<Vec<AstEditOp>>,
    path: Option<String>,
    glob: Option<String>,
    lang: Option<String>,
    max_replacements: Option<usize>,
    proposal_id: Option<String>,
    reason: Option<String>,
}

/// A rewrite pattern compiled for one language.
struct CompiledOp {
    pattern: Pattern,
    out: String,
}

/// One concrete text edit against a file's current content.
#[derive(Debug)]
struct TextEdit {
    position: usize,
    deleted_length: usize,
    inserted_text: String,
}

/// A staged file rewrite: original content retained for hash verification and
/// rollback, new content plus diff preview for review.
#[derive(Debug)]
struct StagedFile {
    path: PathBuf,
    display: String,
    original_hash: String,
    original_content: String,
    new_content: String,
    replacements: usize,
    diff: String,
}

/// A staged rewrite proposal awaiting resolve/reject.
#[derive(Debug)]
struct StagedProposal {
    files: Vec<StagedFile>,
    total_replacements: usize,
    sequence: u64,
}

/// Bounded proposal store with oldest-first eviction.
#[derive(Debug, Default)]
struct ProposalStore {
    proposals: HashMap<String, StagedProposal>,
    order: VecDeque<String>,
    next_sequence: u64,
}

impl ProposalStore {
    fn insert(&mut self, id: String, mut proposal: StagedProposal) {
        proposal.sequence = self.next_sequence;
        self.next_sequence += 1;
        self.order.push_back(id.clone());
        self.proposals.insert(id, proposal);
        while self.order.len() > MAX_STAGED_PROPOSALS {
            if let Some(oldest) = self.order.pop_front() {
                self.proposals.remove(&oldest);
            }
        }
    }

    fn take(&mut self, id: &str) -> Option<StagedProposal> {
        let proposal = self.proposals.remove(id);
        if proposal.is_some() {
            self.order.retain(|entry| entry != id);
        }
        proposal
    }
}

/// Staged structural rewrite tool (`ast_edit`).
///
/// `action: "stage"` (default) computes the rewrite and returns a proposal id
/// with per-file diff previews. `action: "resolve"` applies a staged proposal
/// atomically after re-hashing every file. `action: "reject"` discards it.
pub struct AstEditTool {
    cwd: PathBuf,
    proposals: Mutex<ProposalStore>,
}

impl AstEditTool {
    #[must_use]
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            proposals: Mutex::new(ProposalStore::default()),
        }
    }

    /// Compile every op for `lang`, enforcing the single-node rule on both
    /// `pat` and non-empty `out` with named parse errors.
    fn compile_ops(
        ops: &[AstEditOp],
        lang: AstLanguage,
    ) -> std::result::Result<Vec<CompiledOp>, String> {
        let mut compiled = Vec::with_capacity(ops.len());
        for (index, op) in ops.iter().enumerate() {
            if op.pat.trim().is_empty() {
                return Err(format!("op {index}: `pat` must not be empty"));
            }
            let pattern = Pattern::try_new(op.pat.as_str(), lang.support_lang()).map_err(|error| {
                format!(
                    "op {index}: pattern `{}` failed to parse as {}: {error}",
                    op.pat,
                    lang.name()
                )
            })?;
            if !op.out.is_empty() {
                Pattern::try_new(op.out.as_str(), lang.support_lang()).map_err(|error| {
                    format!(
                        "op {index}: replacement `{}` must parse as a single {} AST node: {error}",
                        op.out,
                        lang.name()
                    )
                })?;
            }
            compiled.push(CompiledOp {
                pattern,
                out: op.out.clone(),
            });
        }
        Ok(compiled)
    }

    /// Apply all ops sequentially to `original`, returning the rewritten
    /// content and the total replacement count.
    ///
    /// Overlapping (nested) matches are resolved outermost-first: an inner
    /// match inside an already-rewritten range is skipped, keeping the
    /// rewrite well-defined.
    fn compute_rewrite(
        lang: AstLanguage,
        original: &str,
        ops: &[CompiledOp],
    ) -> std::result::Result<(String, usize), String> {
        let mut current = original.to_string();
        let mut replacements = 0_usize;
        for op in ops {
            let mut edits: Vec<TextEdit> = Vec::new();
            {
                let grep = AstGrep::new(current.as_str(), lang.support_lang());
                for node in grep.root().find_all(&op.pattern) {
                    let edit = node.make_edit(&op.pattern, &op.out.as_str());
                    let inserted_text = String::from_utf8(edit.inserted_text).map_err(|error| {
                        format!("replacement produced invalid UTF-8: {error}")
                    })?;
                    edits.push(TextEdit {
                        position: edit.position,
                        deleted_length: edit.deleted_length,
                        inserted_text,
                    });
                }
            }
            if edits.is_empty() {
                continue;
            }
            // Outermost-first, left-to-right; skip nested overlaps.
            edits.sort_by(|a, b| {
                a.position
                    .cmp(&b.position)
                    .then(b.deleted_length.cmp(&a.deleted_length))
            });
            let mut last_end = 0_usize;
            let mut accepted: Vec<TextEdit> = Vec::with_capacity(edits.len());
            for edit in edits {
                if edit.position < last_end {
                    continue;
                }
                last_end = edit.position + edit.deleted_length;
                accepted.push(edit);
            }
            replacements += accepted.len();
            // Apply right-to-left so earlier offsets stay valid.
            for edit in accepted.into_iter().rev() {
                current.replace_range(
                    edit.position..edit.position + edit.deleted_length,
                    &edit.inserted_text,
                );
            }
        }
        Ok((current, replacements))
    }

    fn stage(&self, input: &AstEditInput) -> Result<ToolOutput> {
        let ops = input.ops.as_deref().ok_or_else(|| {
            Error::validation("`ops` is required for action=stage")
        })?;
        if ops.is_empty() {
            return Err(Error::validation("`ops` must contain at least one op"));
        }
        let max_replacements = input.max_replacements.unwrap_or(DEFAULT_MAX_REPLACEMENTS);
        if max_replacements == 0 || max_replacements > HARD_MAX_REPLACEMENTS {
            return Err(Error::validation(format!(
                "`maxReplacements` must be between 1 and {HARD_MAX_REPLACEMENTS}"
            )));
        }
        let lang_override = input
            .lang
            .as_deref()
            .map(|name| {
                AstLanguage::from_name(name).ok_or_else(|| {
                    Error::validation(format!(
                        "unknown lang '{name}'; supported: rust, python, javascript, typescript, tsx, bash, go, ruby"
                    ))
                })
            })
            .transpose()?;

        let root = resolve_tool_path(input.path.as_deref().unwrap_or("."), &self.cwd);
        if !root.exists() {
            return Err(Error::validation(format!(
                "path not found: {}",
                input.path.as_deref().unwrap_or(".")
            )));
        }
        let scan = collect_files(&root, &self.cwd, input.glob.as_deref(), lang_override, "ast_edit")?;

        // Compile ops per in-scope language; any parse failure is a hard,
        // named error — never a partial apply.
        let mut compiled: BTreeMap<AstLanguage, Vec<CompiledOp>> = BTreeMap::new();
        for file in &scan.files {
            let lang = lang_override.unwrap_or_else(|| {
                AstLanguage::for_path(file).expect("scan only collects language-detectable files")
            });
            if let std::collections::btree_map::Entry::Vacant(entry) = compiled.entry(lang) {
                let ops_for_lang = Self::compile_ops(ops, lang).map_err(|message| {
                    Error::validation(format!("[AST_PATTERN_PARSE] {message}"))
                })?;
                entry.insert(ops_for_lang);
            }
        }

        let mut staged_files: Vec<StagedFile> = Vec::new();
        let mut total_replacements = 0_usize;
        let mut skipped_files = 0_usize;
        for file in &scan.files {
            let lang = lang_override.unwrap_or_else(|| {
                AstLanguage::for_path(file).expect("scan only collects language-detectable files")
            });
            let original = match read_source_file(file) {
                Ok(Some(content)) => content,
                Ok(None) => {
                    skipped_files += 1;
                    continue;
                }
                Err(message) => return Err(Error::tool("ast_edit", message)),
            };
            let (new_content, count) = Self::compute_rewrite(lang, &original, &compiled[&lang])
                .map_err(|message| Error::tool("ast_edit", message))?;
            if count == 0 {
                continue;
            }
            total_replacements += count;
            if total_replacements > max_replacements {
                return Err(Error::validation(format!(
                    "[AST_LIMIT_EXCEEDED] rewrite would perform {total_replacements} replacements, exceeding maxReplacements={max_replacements}; narrow the scope or raise the cap"
                )));
            }
            let display = display_path(file, &self.cwd);
            let diff = unified_diff_preview(&display, &original, &new_content);
            staged_files.push(StagedFile {
                path: file.clone(),
                display,
                original_hash: content_hash(&original),
                original_content: original,
                new_content,
                replacements: count,
                diff,
            });
        }

        if staged_files.is_empty() {
            let payload = serde_json::json!({
                "staged": false,
                "replacements": 0,
                "filesScanned": scan.files.len(),
                "skippedFiles": skipped_files,
                "message": "no structural matches; nothing staged",
            });
            let text = serde_json::to_string_pretty(&payload)
                .unwrap_or_else(|_| "{\"error\":\"serialization failed\"}".to_string());
            return Ok(text_output(text, payload));
        }

        let proposal_id = format!("ast-{}", uuid::Uuid::new_v4());
        let proposal = StagedProposal {
            total_replacements,
            files: staged_files,
            sequence: 0,
        };
        let files_json: Vec<serde_json::Value> = proposal
            .files
            .iter()
            .map(|file| {
                serde_json::json!({
                    "path": file.display,
                    "replacements": file.replacements,
                    "diff": file.diff,
                })
            })
            .collect();
        self.proposals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(proposal_id.clone(), proposal);

        let payload = serde_json::json!({
            "staged": true,
            "proposalId": proposal_id,
            "replacements": total_replacements,
            "files": files_json,
            "filesScanned": scan.files.len(),
            "skippedFiles": skipped_files,
            "scanTruncated": scan.truncated,
            "message": "proposal staged; NOTHING was written. Review the diffs, then call ast_edit with action=resolve, this proposalId, and a one-line reason to apply atomically (or action=reject to discard).",
        });
        let text = serde_json::to_string_pretty(&payload)
            .unwrap_or_else(|_| "{\"error\":\"serialization failed\"}".to_string());
        Ok(text_output(text, payload))
    }

    fn resolve(&self, input: &AstEditInput) -> Result<ToolOutput> {
        let proposal_id = input.proposal_id.as_deref().ok_or_else(|| {
            Error::validation("`proposalId` is required for action=resolve")
        })?;
        let reason = input.reason.as_deref().map(str::trim).ok_or_else(|| {
            Error::validation("[AST_REASON_REQUIRED] `reason` is required for action=resolve")
        })?;
        if reason.is_empty() || reason.contains('\n') {
            return Err(Error::validation(
                "[AST_REASON_REQUIRED] `reason` must be a non-empty single line",
            ));
        }
        let proposal = self
            .proposals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(proposal_id)
            .ok_or_else(|| {
                Error::validation(format!(
                    "[AST_PROPOSAL_UNKNOWN] unknown proposal id '{proposal_id}' (already resolved, rejected, or evicted)"
                ))
            })?;

        // Pre-verify every file before writing anything: stale-anchor
        // rejection (content re-hash) applies to the whole proposal.
        for file in &proposal.files {
            let current = std::fs::read_to_string(&file.path).map_err(|error| {
                Error::tool(
                    "ast_edit",
                    format!(
                        "[AST_PROPOSAL_STALE] cannot re-read '{}': {error}; whole proposal '{proposal_id}' rejected",
                        file.display
                    ),
                )
            })?;
            if content_hash(&current) != file.original_hash {
                return Err(Error::tool(
                    "ast_edit",
                    format!(
                        "[AST_PROPOSAL_STALE] file '{}' changed since staging; whole proposal '{proposal_id}' rejected (re-stage to proceed)",
                        file.display
                    ),
                ));
            }
        }

        // Apply: temp-file + rename per file; roll back on mid-failure.
        let mut written = 0_usize;
        for file in &proposal.files {
            if let Err(error) = write_file_atomic(&file.path, &file.new_content) {
                let mut rollback_errors = Vec::new();
                for previous in &proposal.files[..written] {
                    if let Err(rollback_error) =
                        write_file_atomic(&previous.path, &previous.original_content)
                    {
                        rollback_errors
                            .push(format!("{}: {rollback_error}", previous.display));
                    }
                }
                let mut message = format!(
                    "[AST_APPLY_FAILED] failed to write '{}': {error}; rolled back {written} previously-written file(s)",
                    file.display
                );
                if rollback_errors.is_empty() {
                    message.push_str("; all prior writes restored");
                } else {
                    let _ = write!(
                        message,
                        "; ROLLBACK ERRORS (manual repair needed): {}",
                        rollback_errors.join("; ")
                    );
                }
                return Err(Error::tool("ast_edit", message));
            }
            written += 1;
        }

        let files_json: Vec<serde_json::Value> = proposal
            .files
            .iter()
            .map(|file| {
                serde_json::json!({
                    "path": file.display,
                    "replacements": file.replacements,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "applied": true,
            "proposalId": proposal_id,
            "reason": reason,
            "replacements": proposal.total_replacements,
            "filesWritten": written,
            "files": files_json,
        });
        let text = serde_json::to_string_pretty(&payload)
            .unwrap_or_else(|_| "{\"error\":\"serialization failed\"}".to_string());
        Ok(text_output(text, payload))
    }

    fn reject(&self, input: &AstEditInput) -> Result<ToolOutput> {
        let proposal_id = input.proposal_id.as_deref().ok_or_else(|| {
            Error::validation("`proposalId` is required for action=reject")
        })?;
        let removed = self
            .proposals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take(proposal_id);
        if removed.is_none() {
            return Err(Error::validation(format!(
                "[AST_PROPOSAL_UNKNOWN] unknown proposal id '{proposal_id}' (already resolved, rejected, or evicted)"
            )));
        }
        let payload = serde_json::json!({
            "rejected": true,
            "proposalId": proposal_id,
            "message": "proposal discarded; zero files were written",
        });
        let text = serde_json::to_string_pretty(&payload)
            .unwrap_or_else(|_| "{\"error\":\"serialization failed\"}".to_string());
        Ok(text_output(text, payload))
    }
}

#[async_trait]
impl Tool for AstEditTool {
    fn name(&self) -> &str {
        "ast_edit"
    }

    fn label(&self) -> &str {
        "ast_edit"
    }

    fn description(&self) -> &str {
        "Staged structural code rewrite using tree-sitter AST patterns (ast-grep syntax). Patterns match AST structure, NOT text, and never match inside comments or strings. ops are applied in order; each op is {pat, out}: `$NAME` captures exactly one AST node, `$$$NAME` captures zero or more (use `$$$NAME`, NOT `$$NAME`); reusing the same `$NAME` twice in `pat` requires identical code at both sites; `pat` and `out` must each parse as a single AST node; an empty `out` deletes the matched node; substitution is 1:1 (each match replaced independently). action=stage (default) returns a proposalId, replacement count, and per-file diff previews — NOTHING is written. action=resolve with proposalId and a one-line `reason` applies atomically: every file is re-hashed at apply time and any change since staging rejects the WHOLE proposal naming the file; writes are temp-file+rename per file with rollback on mid-failure. action=reject discards a proposal with zero writes."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["stage", "resolve", "reject"],
                    "description": "stage (default): compute rewrite and return a proposal; resolve: apply a staged proposal atomically; reject: discard it"
                },
                "ops": {
                    "type": "array",
                    "description": "Rewrite ops for action=stage, applied in order",
                    "items": {
                        "type": "object",
                        "properties": {
                            "pat": {
                                "type": "string",
                                "description": "Structural pattern (single AST node), e.g. '$EXPR.unwrap()'"
                            },
                            "out": {
                                "type": "string",
                                "description": "Replacement template (single AST node) with $NAME/$$$NAME substitution; empty string deletes the match"
                            }
                        },
                        "required": ["pat", "out"]
                    }
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to rewrite (default: current directory). Respects .gitignore."
                },
                "glob": {
                    "type": "string",
                    "description": "Filter files by glob, e.g. '**/*.rs'"
                },
                "lang": {
                    "type": "string",
                    "description": "Force one language for all in-scope files: rust|python|javascript|typescript|tsx|bash|go|ruby"
                },
                "maxReplacements": {
                    "type": "integer",
                    "description": "Maximum replacements allowed in one proposal (default 500, hard cap 5000); exceeding it fails staging with no proposal"
                },
                "proposalId": {
                    "type": "string",
                    "description": "Proposal id for action=resolve or action=reject"
                },
                "reason": {
                    "type": "string",
                    "description": "One-line reason for action=resolve (required; recorded with the apply)"
                }
            },
            "required": []
        })
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::write()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: AstEditInput =
            serde_json::from_value(input).map_err(|e| Error::validation(e.to_string()))?;
        match input.action.as_deref().unwrap_or("stage") {
            "stage" => self.stage(&input),
            "resolve" => self.resolve(&input),
            "reject" => self.reject(&input),
            other => Err(Error::validation(format!(
                "unknown action '{other}'; expected stage|resolve|reject"
            ))),
        }
    }
}
