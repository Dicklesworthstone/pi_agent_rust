//! Foreign context-file (rules) import (bd-cv653.6.2).
//!
//! Every other agent ships its own on-disk rules convention. Rather than
//! demanding a migration, pi reads the formats already present in a
//! workspace in their native shape and folds them into the assembled
//! context:
//!
//! | Format | Location | Scoping |
//! |--------|----------|---------|
//! | Cursor MDC | `.cursor/rules/*.mdc` | frontmatter `globs` / `alwaysApply` |
//! | Cursor legacy | `.cursorrules` | always |
//! | Cline | `.clinerules` file or `.clinerules/*.md` dir | always |
//! | Copilot | `.github/copilot-instructions.md` | always |
//! | Copilot scoped | `.github/instructions/*.instructions.md` | frontmatter `applyTo` globs |
//! | Windsurf legacy | `.windsurfrules` | always |
//! | Windsurf rules | `.windsurf/rules/*` | frontmatter `trigger` / `globs` / `description` |
//! | Gemini | `GEMINI.md` | always |
//!
//! Codex `AGENTS.md` and Claude `CLAUDE.md` are pi's native conventions and
//! stay owned by [`crate::app`]'s project-context loader; this module skips
//! them (native wins) and also drops foreign rules with identical content
//! and activation scopes (first occurrence wins, discovery order below).
//! Identical bodies with different scopes must not erase one another.
//!
//! Import is strictly read-only: parsers take bytes already read from disk
//! and never write foreign files.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

/// Total byte budget for injected foreign-rule content.
///
/// Overflow is deterministic: rules are kept in discovery order until the
/// budget is hit, the first overflowing rule is dropped along with everything
/// after it, and a truncation notice records how many rules were omitted.
pub const FOREIGN_RULES_BUDGET_BYTES: usize = 24 * 1024;

/// Where a rule came from, for provenance display (`/context`) and logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForeignRuleFormat {
    CursorMdc,
    CursorLegacy,
    Cline,
    Copilot,
    CopilotScoped,
    Windsurf,
    Gemini,
}

impl ForeignRuleFormat {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CursorMdc => "cursor-mdc",
            Self::CursorLegacy => "cursor-legacy",
            Self::Cline => "cline",
            Self::Copilot => "copilot",
            Self::CopilotScoped => "copilot-scoped",
            Self::Windsurf => "windsurf",
            Self::Gemini => "gemini",
        }
    }
}

/// One normalized imported rule.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForeignRule {
    /// Rule body with any recognized frontmatter stripped.
    pub content: String,
    /// Scoping globs (workspace-relative). Empty means the rule is scoped
    /// only by `always_apply`.
    pub globs: Vec<String>,
    /// Inject unconditionally into the system context block.
    pub always_apply: bool,
    /// Selection hint for an on-demand rule. Only this description, not the
    /// rule body, is exposed until the agent reads the file when relevant.
    /// `None` on an unscoped, non-always rule means explicit requests only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Workspace-relative source path.
    pub source: String,
    /// Which convention the rule was parsed from.
    pub format: ForeignRuleFormat,
}

impl ForeignRule {
    /// Whether this rule applies only when specific paths are touched.
    #[must_use]
    pub const fn is_scoped(&self) -> bool {
        !self.always_apply && !self.globs.is_empty()
    }
}

/// Result of a workspace scan: rules within budget plus truncation evidence.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ForeignRules {
    pub rules: Vec<ForeignRule>,
    /// Rules dropped by the injection budget (count, not content).
    pub truncated_rules: usize,
}

impl ForeignRules {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.truncated_rules == 0
    }

    /// Rules injected unconditionally. An explicitly inactive rule without
    /// globs is on-demand, not an unconditional rule.
    pub fn always_rules(&self) -> impl Iterator<Item = &ForeignRule> {
        self.rules.iter().filter(|rule| rule.always_apply)
    }

    /// Glob-scoped rules awaiting path activation.
    pub fn scoped_rules(&self) -> impl Iterator<Item = &ForeignRule> {
        self.rules
            .iter()
            .filter(|rule| ForeignRule::is_scoped(rule))
    }

    /// Render the always-apply rules as a system-prompt block with
    /// provenance headers, or `None` when nothing needs injecting.
    #[must_use]
    pub fn system_prompt_block(&self) -> Option<String> {
        let mut block = String::new();
        for rule in self.always_rules() {
            let _ = write!(
                block,
                "## {} ({})\n\n{}\n\n",
                rule.source,
                rule.format.label(),
                rule.content.trim()
            );
        }
        let scoped = self.scoped_rules().count();
        if scoped > 0 {
            let _ = writeln!(
                block,
                "{scoped} additional path-scoped rule(s) will be provided when matching files are touched."
            );
        }
        for rule in self
            .rules
            .iter()
            .filter(|rule| !rule.always_apply && !rule.is_scoped())
        {
            if let Some(description) = &rule.description {
                let _ = writeln!(
                    block,
                    "On-demand rule at `{}`: {description}\nRead this rule file only when that description is relevant to the current task.",
                    rule.source
                );
            } else {
                let _ = writeln!(
                    block,
                    "On-demand rule at `{}`: read only when explicitly requested; not automatically applied.",
                    rule.source
                );
            }
        }
        if self.truncated_rules > 0 {
            let _ = writeln!(
                block,
                "[{} imported rule(s) omitted: {} byte budget reached]",
                self.truncated_rules, FOREIGN_RULES_BUDGET_BYTES
            );
        }
        if block.is_empty() {
            None
        } else {
            Some(format!(
                "# Imported Rules\n\nRules imported read-only from other tools' native config files in this workspace:\n\n{block}"
            ))
        }
    }
}

/// Match glob-scoped rules against a path touched by a tool call.
///
/// Globs are matched workspace-relative with `globset` semantics; a bare
/// pattern like `*.ts` also matches in subdirectories (fd/Cursor
/// convention), so it is compiled as `**/*.ts`.
#[derive(Debug)]
pub struct ScopedRuleMatcher {
    entries: Vec<(usize, globset::GlobSet)>,
}

impl ScopedRuleMatcher {
    /// Build a matcher over `rules`; indices returned by
    /// [`Self::matching_rules`] index into that same slice. Rules whose
    /// globs all fail to compile match nothing (fail-open skip, logged).
    #[must_use]
    pub fn new(rules: &[ForeignRule]) -> Self {
        let mut entries = Vec::new();
        for (index, rule) in rules.iter().enumerate() {
            if !rule.is_scoped() {
                continue;
            }
            let mut builder = globset::GlobSetBuilder::new();
            let mut added = 0usize;
            for glob in &rule.globs {
                let anchored = if glob.contains('/') {
                    glob.trim_start_matches("./").to_string()
                } else {
                    format!("**/{glob}")
                };
                match globset::GlobBuilder::new(&anchored)
                    .literal_separator(true)
                    .build()
                {
                    Ok(compiled) => {
                        builder.add(compiled);
                        added += 1;
                    }
                    Err(error) => {
                        tracing::debug!(
                            "skipping unparseable rule glob {glob:?} from {}: {error}",
                            rule.source
                        );
                    }
                }
            }
            if added == 0 {
                continue;
            }
            if let Ok(set) = builder.build() {
                entries.push((index, set));
            }
        }
        Self { entries }
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Indices of rules whose globs match `path` (workspace-relative or
    /// absolute; an absolute path is matched by its `workspace_root`-relative
    /// suffix when it lives inside the workspace).
    #[must_use]
    pub fn matching_rules(&self, path: &Path, workspace_root: &Path) -> Vec<usize> {
        let relative = if path.is_absolute() {
            let Ok(relative) = path.strip_prefix(workspace_root) else {
                return Vec::new();
            };
            relative
        } else {
            path.strip_prefix(workspace_root).unwrap_or(path)
        };
        let normalized = relative.to_string_lossy().replace('\\', "/");
        let mut components = Vec::new();
        for component in normalized.split('/') {
            match component {
                "" | "." => {}
                ".." => {
                    if components.pop().is_none() {
                        return Vec::new();
                    }
                }
                _ => components.push(component),
            }
        }
        if components.is_empty() {
            return Vec::new();
        }
        let normalized = components.join("/");
        self.entries
            .iter()
            .filter(|(_, set)| set.is_match(&normalized))
            .map(|(index, _)| *index)
            .collect()
    }
}

/// Scan `workspace_root` for foreign rule files, in fixed discovery order.
///
/// Read-only: only `read_to_string` on regular files, never a write.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn discover_foreign_rules(workspace_root: &Path) -> ForeignRules {
    let mut rules = Vec::new();

    // Cursor MDC directory.
    collect_sorted_dir(
        &workspace_root.join(".cursor").join("rules"),
        Some("mdc"),
        &mut |path, content| rules.push(parse_cursor_mdc(path, workspace_root, &content)),
    );
    // Cursor legacy single file.
    if let Some(content) = read_rule_file(&workspace_root.join(".cursorrules")) {
        rules.push(plain_rule(
            &workspace_root.join(".cursorrules"),
            workspace_root,
            content,
            ForeignRuleFormat::CursorLegacy,
        ));
    }
    // Cline: single file or directory of markdown files.
    let clinerules = workspace_root.join(".clinerules");
    if clinerules.is_dir() {
        collect_sorted_dir(&clinerules, Some("md"), &mut |path, content| {
            rules.push(plain_rule(
                path,
                workspace_root,
                content,
                ForeignRuleFormat::Cline,
            ));
        });
    } else if let Some(content) = read_rule_file(&clinerules) {
        rules.push(plain_rule(
            &clinerules,
            workspace_root,
            content,
            ForeignRuleFormat::Cline,
        ));
    }
    // Copilot: repo-wide instructions + scoped *.instructions.md.
    let copilot = workspace_root
        .join(".github")
        .join("copilot-instructions.md");
    if let Some(content) = read_rule_file(&copilot) {
        rules.push(plain_rule(
            &copilot,
            workspace_root,
            content,
            ForeignRuleFormat::Copilot,
        ));
    }
    collect_sorted_dir(
        &workspace_root.join(".github").join("instructions"),
        Some("md"),
        &mut |path, content| {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".instructions.md"))
            {
                rules.push(parse_copilot_scoped(path, workspace_root, &content));
            }
        },
    );
    // Windsurf: legacy files are unconditional; directory rules declare
    // their own trigger. Treating every trigger as plain text activates
    // manual and path-specific instructions on unrelated tasks.
    if let Some(content) = read_rule_file(&workspace_root.join(".windsurfrules")) {
        rules.push(plain_rule(
            &workspace_root.join(".windsurfrules"),
            workspace_root,
            content,
            ForeignRuleFormat::Windsurf,
        ));
    }
    collect_sorted_dir(
        &workspace_root.join(".windsurf").join("rules"),
        None,
        &mut |path, content| {
            rules.push(parse_windsurf_rule(path, workspace_root, &content));
        },
    );
    // Gemini.
    if let Some(content) = read_rule_file(&workspace_root.join("GEMINI.md")) {
        rules.push(plain_rule(
            &workspace_root.join("GEMINI.md"),
            workspace_root,
            content,
            ForeignRuleFormat::Gemini,
        ));
    }

    // Native precedence + dedupe: drop any foreign rule identical to a native
    // context file (AGENTS.md / CLAUDE.md at the workspace root). Foreign
    // duplicates must also have equivalent activation scopes: dropping a
    // Rust rule because a TypeScript rule has the same body loses coverage.
    let mut native_bodies: HashSet<String> = HashSet::new();
    for native in ["AGENTS.md", "CLAUDE.md"] {
        if let Some(content) = read_rule_file(&workspace_root.join(native)) {
            native_bodies.insert(normalized_body(&content));
        }
    }
    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for rule in rules {
        let body = normalized_body(&rule.content);
        if body.is_empty() || native_bodies.contains(&body) {
            continue;
        }
        let mut scope = if rule.always_apply {
            Vec::new()
        } else {
            rule.globs.clone()
        };
        scope.sort();
        scope.dedup();
        if seen.insert((body, rule.always_apply, scope, rule.description.clone())) {
            deduped.push(rule);
        }
    }

    // Budget: keep discovery-order prefix; drop the first overflowing rule
    // and everything after it (deterministic).
    let mut kept = Vec::new();
    let mut spent = 0usize;
    let mut truncated = 0usize;
    for rule in deduped {
        let cost = rule
            .content
            .len()
            .saturating_add(rule.description.as_ref().map_or(0, String::len));
        if truncated > 0 || spent.saturating_add(cost) > FOREIGN_RULES_BUDGET_BYTES {
            truncated += 1;
            continue;
        }
        spent += cost;
        kept.push(rule);
    }

    ForeignRules {
        rules: kept,
        truncated_rules: truncated,
    }
}

fn normalized_body(content: &str) -> String {
    content.trim().to_string()
}

fn relative_display(path: &Path, workspace_root: &Path) -> String {
    let rel = path.strip_prefix(workspace_root).unwrap_or(path);
    rel.to_string_lossy().replace('\\', "/")
}

fn read_rule_file(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(content) => Some(content),
        Err(error) => {
            tracing::debug!("could not read rule file {}: {error}", path.display());
            None
        }
    }
}

/// Visit regular files in `dir` (sorted by name) with an optional extension
/// filter, feeding readable contents to `visit`.
fn collect_sorted_dir(dir: &Path, extension: Option<&str>, visit: &mut dyn FnMut(&Path, String)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<_> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .filter(|path| {
            extension.is_none_or(|extension| {
                path.extension()
                    .is_some_and(|found| found.eq_ignore_ascii_case(extension))
            })
        })
        .collect();
    paths.sort();
    for path in paths {
        if let Some(content) = read_rule_file(&path) {
            visit(&path, content);
        }
    }
}

fn plain_rule(
    path: &Path,
    workspace_root: &Path,
    content: String,
    format: ForeignRuleFormat,
) -> ForeignRule {
    ForeignRule {
        content,
        globs: Vec::new(),
        always_apply: true,
        description: None,
        source: relative_display(path, workspace_root),
        format,
    }
}

/// Split a `---` frontmatter block off `content`, returning
/// `(fields, body)`. Fields are simple `key: value` lines; unknown keys are
/// ignored; a malformed or unclosed block yields no fields and the whole
/// content as body (tolerant per the bead spec).
fn split_simple_frontmatter(content: &str) -> (Vec<(String, String)>, String) {
    let mut lines = content.lines();
    if !matches!(lines.next(), Some(first) if first.trim() == "---") {
        return (Vec::new(), content.to_string());
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut closed = false;
    let mut block_scalar = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        // Fold indented YAML block scalars into a selection hint. A colon
        // within the description is content, not another frontmatter key.
        if block_scalar
            && (line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty())
        {
            if let Some((_, value)) = fields.last_mut() {
                if !value.is_empty() && !line.trim().is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        block_scalar = false;
        // Recognize sequence entries before key/value pairs: glob paths
        // may contain a colon, which must not start a bogus field.
        if let Some(item) = line.trim().strip_prefix("- ") {
            if let Some((_, value)) = fields.last_mut() {
                if !value.is_empty() {
                    value.push(',');
                }
                value.push_str(item.trim());
            }
        } else if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            if !key.is_empty() {
                let value = value.trim();
                block_scalar = matches!(value, ">" | ">-" | ">+" | "|" | "|-" | "|+");
                fields.push((
                    key.to_string(),
                    if block_scalar { "" } else { value }.to_string(),
                ));
            }
        }
    }
    if !closed {
        return (Vec::new(), content.to_string());
    }
    (fields, lines.collect::<Vec<_>>().join("\n"))
}

/// Remove quotes only when they enclose the entire scalar, not a sequence
/// such as `"*.rs", "*.ts"`. Preserve glob escapes for `globset` to interpret.
fn quoted_scalar(value: &str) -> Option<&str> {
    let quote = value.chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    let mut escaped = false;
    for (index, character) in value.char_indices().skip(1) {
        if escaped {
            escaped = false;
        } else if character == '\\' && quote == '"' {
            escaped = true;
        } else if character == quote {
            return (index + 1 == value.len()).then_some(&value[1..index]);
        }
    }
    None
}

/// Parse comma-separated scalars, YAML flow lists, or flattened block lists.
/// Commas inside brace alternatives, character classes, quotes, and escaped
/// literals belong to the glob; splitting them silently disables valid rules.
fn parse_glob_list(value: &str) -> Vec<String> {
    let value = value.trim();
    let value = quoted_scalar(value).unwrap_or_else(|| {
        value
            .strip_prefix('[')
            .and_then(|value| value.strip_suffix(']'))
            .unwrap_or(value)
    });
    let mut globs = Vec::new();
    let mut start = 0usize;
    let mut braces = 0usize;
    let mut in_class = false;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            }
            continue;
        }
        if in_class {
            if character == ']' {
                in_class = false;
            }
            continue;
        }
        match character {
            '\'' | '"' if value[start..index].trim().is_empty() => quote = Some(character),
            '[' => in_class = true,
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            ',' if braces == 0 => {
                push_glob(&mut globs, &value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    push_glob(&mut globs, &value[start..]);
    globs
}

fn push_glob(globs: &mut Vec<String>, value: &str) {
    let value = value.trim();
    let value = quoted_scalar(value).unwrap_or(value).trim();
    if !value.is_empty() {
        globs.push(value.to_string());
    }
}

fn rule_description(fields: &[(String, String)]) -> Option<String> {
    fields
        .iter()
        .rev()
        .find(|(key, _)| key == "description")
        .map(|(_, value)| quoted_scalar(value).unwrap_or(value).trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Cursor `.mdc`: frontmatter `description` / `globs` / `alwaysApply`.
fn parse_cursor_mdc(path: &Path, workspace_root: &Path, content: &str) -> ForeignRule {
    let (fields, body) = split_simple_frontmatter(content);
    let mut globs = Vec::new();
    let mut always_apply = None;
    for (key, value) in &fields {
        match key.as_str() {
            "globs" => globs = parse_glob_list(value),
            "alwaysApply" => always_apply = Some(value.trim() == "true"),
            _ => {}
        }
    }
    // Preserve the plain/unscoped fallback, but never override an explicit
    // alwaysApply: false. Such a rule without globs is on-demand.
    let always_apply = always_apply.unwrap_or(globs.is_empty());
    let description = if !always_apply && globs.is_empty() {
        rule_description(&fields)
    } else {
        None
    };
    ForeignRule {
        content: body,
        globs,
        always_apply,
        description,
        source: relative_display(path, workspace_root),
        format: ForeignRuleFormat::CursorMdc,
    }
}

/// Copilot `*.instructions.md`: frontmatter `applyTo` glob list.
fn parse_copilot_scoped(path: &Path, workspace_root: &Path, content: &str) -> ForeignRule {
    let (fields, body) = split_simple_frontmatter(content);
    let globs = fields
        .iter()
        .find(|(key, _)| key == "applyTo")
        .map(|(_, value)| parse_glob_list(value))
        .unwrap_or_default();
    let always_apply = globs.is_empty() || globs.iter().any(|glob| glob == "**");
    ForeignRule {
        content: body,
        globs: if always_apply { Vec::new() } else { globs },
        always_apply,
        description: None,
        source: relative_display(path, workspace_root),
        format: ForeignRuleFormat::CopilotScoped,
    }
}

/// Windsurf workspace rules declare one of four activation modes. Unknown
/// or incomplete triggers remain on-demand, never silently become global.
fn parse_windsurf_rule(path: &Path, workspace_root: &Path, content: &str) -> ForeignRule {
    let (fields, body) = split_simple_frontmatter(content);
    let trigger = fields
        .iter()
        .rev()
        .find(|(key, _)| key == "trigger")
        .map(|(_, value)| quoted_scalar(value).unwrap_or(value).trim());
    let mut rule = plain_rule(path, workspace_root, body, ForeignRuleFormat::Windsurf);
    match trigger {
        None | Some("always_on") => {}
        Some("glob") => {
            rule.always_apply = false;
            rule.globs = fields
                .iter()
                .rev()
                .find(|(key, _)| key == "globs")
                .map(|(_, value)| parse_glob_list(value))
                .unwrap_or_default();
        }
        Some("model_decision") => {
            rule.always_apply = false;
            rule.description = rule_description(&fields);
        }
        Some("manual") => rule.always_apply = false,
        Some(trigger) => {
            tracing::debug!(
                "unknown rule trigger {trigger:?} from {}; keeping rule on-demand",
                rule.source
            );
            rule.always_apply = false;
        }
    }
    rule
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdirs");
        std::fs::write(path, content).expect("write rule fixture");
    }

    /// Acceptance 1: every supported format in one workspace surfaces with
    /// correct provenance, format tags, and scoping.
    #[test]
    fn discovers_every_format_with_provenance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            root,
            ".cursor/rules/style.mdc",
            "---\ndescription: style\nglobs: \"*.ts,src/**/*.tsx\"\nalwaysApply: false\n---\nUse tabs.",
        );
        write(root, ".cursorrules", "Legacy cursor rule.");
        write(root, ".clinerules/01-general.md", "Cline general rule.");
        write(
            root,
            ".github/copilot-instructions.md",
            "Copilot repo rule.",
        );
        write(
            root,
            ".github/instructions/backend.instructions.md",
            "---\napplyTo: \"server/**\"\n---\nBackend only.",
        );
        write(root, ".windsurfrules", "Windsurf rule.");
        write(root, ".windsurf/rules/one.md", "Windsurf dir rule.");
        write(root, "GEMINI.md", "Gemini rule.");

        let rules = discover_foreign_rules(root);
        assert_eq!(rules.truncated_rules, 0);
        let summary: Vec<(String, &'static str, bool)> = rules
            .rules
            .iter()
            .map(|rule| (rule.source.clone(), rule.format.label(), rule.is_scoped()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (".cursor/rules/style.mdc".to_string(), "cursor-mdc", true),
                (".cursorrules".to_string(), "cursor-legacy", false),
                (".clinerules/01-general.md".to_string(), "cline", false),
                (
                    ".github/copilot-instructions.md".to_string(),
                    "copilot",
                    false
                ),
                (
                    ".github/instructions/backend.instructions.md".to_string(),
                    "copilot-scoped",
                    true
                ),
                (".windsurfrules".to_string(), "windsurf", false),
                (".windsurf/rules/one.md".to_string(), "windsurf", false),
                ("GEMINI.md".to_string(), "gemini", false),
            ]
        );

        let block = rules.system_prompt_block().expect("block");
        assert!(block.contains("# Imported Rules"));
        assert!(block.contains(".cursorrules (cursor-legacy)"));
        assert!(block.contains("2 additional path-scoped rule(s)"));
        assert!(!block.contains("Use tabs."), "scoped rule must not inject");
    }

    /// Acceptance 2 (unit matrix): glob-scoped rules activate only for
    /// matching paths; bare-filename globs match at any depth.
    #[test]
    fn scoped_rule_matcher_matrix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            root,
            ".cursor/rules/ts.mdc",
            "---\nglobs: \"*.ts\"\n---\nTS rule.",
        );
        write(
            root,
            ".cursor/rules/api.mdc",
            "---\nglobs: src/api/**\n---\nAPI rule.",
        );
        let rules = discover_foreign_rules(root);
        let matcher = ScopedRuleMatcher::new(&rules.rules);
        assert!(!matcher.is_empty());

        let matches = |path: &str| -> Vec<String> {
            matcher
                .matching_rules(&PathBuf::from(path), root)
                .into_iter()
                .map(|index| {
                    rules
                        .rules
                        .get(index)
                        .expect("matcher indexes stay in bounds")
                        .source
                        .clone()
                })
                .collect()
        };

        assert_eq!(matches("main.ts"), vec![".cursor/rules/ts.mdc"]);
        assert_eq!(matches("deep/nested/mod.ts"), vec![".cursor/rules/ts.mdc"]);
        assert_eq!(matches("src/api/users.rs"), vec![".cursor/rules/api.mdc"]);
        // Discovery order is sorted by filename: api.mdc precedes ts.mdc.
        assert_eq!(
            matches("src/api/users.ts"),
            vec![".cursor/rules/api.mdc", ".cursor/rules/ts.mdc"]
        );
        assert!(matches("README.md").is_empty());
        // Absolute path inside the workspace resolves via its relative form.
        let absolute = root.join("lib.ts");
        assert_eq!(
            matches(absolute.to_string_lossy().as_ref()),
            vec![".cursor/rules/ts.mdc"]
        );
    }

    /// Acceptance 3: budget overflow drops deterministically with a notice.
    #[test]
    fn budget_overflow_truncates_deterministically() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let big = "x".repeat(FOREIGN_RULES_BUDGET_BYTES - 10);
        write(root, ".cursorrules", &big);
        write(root, ".windsurfrules", "small but over budget");
        write(root, "GEMINI.md", "also dropped");

        let rules = discover_foreign_rules(root);
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(
            rules.rules.first().expect("kept rule").source,
            ".cursorrules"
        );
        assert_eq!(rules.truncated_rules, 2);
        let block = rules.system_prompt_block().expect("block");
        assert!(block.contains("2 imported rule(s) omitted"));
    }

    /// Native precedence + dedupe: content identical to AGENTS.md/CLAUDE.md
    /// or an earlier foreign rule is dropped.
    #[test]
    fn native_wins_and_duplicates_collapse() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(root, "AGENTS.md", "Shared canonical rules.\n");
        write(root, "GEMINI.md", "Shared canonical rules.\n");
        write(root, ".cursorrules", "Cursor-specific extras.");
        write(root, ".windsurfrules", "Cursor-specific extras.");

        let rules = discover_foreign_rules(root);
        let sources: Vec<&str> = rules
            .rules
            .iter()
            .map(|rule| rule.source.as_str())
            .collect();
        assert_eq!(sources, vec![".cursorrules"]);
    }

    /// Malformed frontmatter is tolerated: unclosed blocks become plain
    /// always-apply content; unparseable globs are skipped without
    /// dropping the rule; MDC with no scoping falls back to always-apply.
    #[test]
    fn malformed_frontmatter_tolerance() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            root,
            ".cursor/rules/unclosed.mdc",
            "---\nglobs: *.rs\nno closing fence\nbody text",
        );
        write(
            root,
            ".cursor/rules/badglob.mdc",
            "---\nglobs: \"[unclosed\"\n---\nBad glob body.",
        );
        write(
            root,
            ".cursor/rules/unscoped.mdc",
            "---\ndescription: only a description\n---\nUnscoped body.",
        );

        let rules = discover_foreign_rules(root);
        assert_eq!(rules.rules.len(), 3);
        let unclosed = rules.rules.get(1).expect("unclosed rule present");
        assert_eq!(unclosed.source, ".cursor/rules/unclosed.mdc");
        assert!(
            unclosed.always_apply,
            "unclosed frontmatter is plain content"
        );
        assert!(unclosed.content.contains("no closing fence"));

        let badglob = rules.rules.first().expect("badglob rule present");
        assert_eq!(badglob.source, ".cursor/rules/badglob.mdc");
        assert!(badglob.is_scoped());
        let matcher = ScopedRuleMatcher::new(&rules.rules);
        assert!(
            matcher
                .matching_rules(&PathBuf::from("anything.rs"), root)
                .is_empty(),
            "unparseable glob matches nothing"
        );

        let unscoped = rules.rules.get(2).expect("unscoped rule present");
        assert!(unscoped.always_apply);
        assert_eq!(unscoped.content.trim(), "Unscoped body.");
    }

    /// YAML-list globs flatten into the glob list.
    #[test]
    fn yaml_list_globs_parse() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            root,
            ".cursor/rules/list.mdc",
            "---\nglobs:\n  - \"*.py\"\n  - \"scripts/**\"\n---\nList body.",
        );
        let rules = discover_foreign_rules(root);
        assert_eq!(
            rules.rules.first().expect("list rule").globs,
            vec!["*.py", "scripts/**"]
        );
    }

    /// Copilot `applyTo: \"**\"` means repo-wide (always-apply).
    #[test]
    fn copilot_apply_to_star_star_is_always() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            root,
            ".github/instructions/all.instructions.md",
            "---\napplyTo: \"**\"\n---\nEverywhere.",
        );
        let rules = discover_foreign_rules(root);
        let rule = rules.rules.first().expect("copilot rule present");
        assert!(rule.always_apply);
        assert!(rule.globs.is_empty());
    }

    #[test]
    fn glob_lists_preserve_alternatives_classes_and_escapes() {
        for (input, expected) in [
            (
                "**/*.{ts,tsx},server/**/*.rs",
                vec!["**/*.{ts,tsx}", "server/**/*.rs"],
            ),
            (
                r#""*.ts,src/**/*.{js,jsx}""#,
                vec!["*.ts", "src/**/*.{js,jsx}"],
            ),
            (
                r#"["*.rs", "src/**/*.{js,jsx}"]"#,
                vec!["*.rs", "src/**/*.{js,jsx}"],
            ),
            ("['*.py', 'scripts/**']", vec!["*.py", "scripts/**"]),
            ("[a,b]*.rs,*.py", vec!["[a,b]*.rs", "*.py"]),
            (r"name\,part.rs,*.py", vec![r"name\,part.rs", "*.py"]),
            ("src/{a,{b,c}}/*.rs,*.py", vec!["src/{a,{b,c}}/*.rs", "*.py"]),
            ("日本語/*.{rs,ts},*.py", vec!["日本語/*.{rs,ts}", "*.py"]),
            ("{*.rs,*.ts", vec!["{*.rs,*.ts"]),
            ("[]", vec![]),
        ] {
            assert_eq!(parse_glob_list(input), expected, "input: {input}");
        }
    }

    #[test]
    fn imported_brace_globs_activate_for_each_alternative() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            root,
            ".cursor/rules/frontend.mdc",
            "---\nglobs: [\"src/**/*.{ts,tsx}\", \"*.vue\"]\nalwaysApply: false\n---\nFrontend standards.",
        );
        write(
            root,
            ".github/instructions/backend.instructions.md",
            "---\napplyTo: 'server/**/*.{rs,go}'\n---\nBackend standards.",
        );
        let rules = discover_foreign_rules(root);
        let matcher = ScopedRuleMatcher::new(&rules.rules);
        for path in ["src/main.ts", "src/ui/view.tsx", "ui/view.vue"] {
            assert_eq!(matcher.matching_rules(Path::new(path), root), vec![0]);
        }
        for path in ["server/main.rs", "server/api/main.go"] {
            assert_eq!(matcher.matching_rules(Path::new(path), root), vec![1]);
        }
        assert!(
            matcher
                .matching_rules(Path::new("src/main.py"), root)
                .is_empty()
        );
        let block = rules.system_prompt_block().expect("scoped notice");
        assert!(!block.contains("Frontend standards."));
        assert!(!block.contains("Backend standards."));
    }

    #[test]
    fn scoped_matches_stay_inside_workspace_and_respect_directory_depth() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("workspace");
        write(&root, ".cursor/rules/ts.mdc", "---\nglobs: src/*.ts\n---\nTS.");
        let rules = discover_foreign_rules(&root);
        let matcher = ScopedRuleMatcher::new(&rules.rules);
        for path in [
            "src/main.ts",
            "./src/main.ts",
            "src/sub/../main.ts",
            r"src\main.ts",
        ] {
            assert_eq!(matcher.matching_rules(Path::new(path), &root), vec![0]);
        }
        for path in [
            "src/sub/main.ts",
            "../src/main.ts",
            "src/../../src/main.ts",
        ] {
            assert!(matcher.matching_rules(Path::new(path), &root).is_empty());
        }
        assert!(
            matcher
                .matching_rules(&tmp.path().join("src/main.ts"), &root)
                .is_empty()
        );
        assert!(
            matcher
                .matching_rules(&root.join("../src/main.ts"), &root)
                .is_empty()
        );
    }

    #[test]
    fn deduplication_keeps_distinct_scopes_and_collapses_reordered_scopes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for (name, globs) in [
            ("a", "*.ts,*.tsx"),
            ("b", "*.rs"),
            ("c", "*.tsx,*.ts,*.ts"),
        ] {
            write(
                root,
                &format!(".cursor/rules/{name}.mdc"),
                &format!("---\nglobs: {globs}\n---\nShared standards."),
            );
        }
        let rules = discover_foreign_rules(root);
        assert_eq!(rules.rules.len(), 2);
        let matcher = ScopedRuleMatcher::new(&rules.rules);
        assert_eq!(matcher.matching_rules(Path::new("lib.tsx"), root), vec![0]);
        assert_eq!(matcher.matching_rules(Path::new("lib.rs"), root), vec![1]);
    }

    #[test]
    fn explicit_inactive_rule_is_not_promoted_to_global_context() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            root,
            ".cursor/rules/manual.mdc",
            "---\nalwaysApply: false\nglobs: []\n---\nOnly for a requested migration.",
        );
        let rules = discover_foreign_rules(root);
        assert_eq!(rules.rules.len(), 1);
        assert_eq!(rules.always_rules().count(), 0);
        assert!(ScopedRuleMatcher::new(&rules.rules).is_empty());
        let block = rules.system_prompt_block().expect("on-demand notice");
        assert!(block.contains(".cursor/rules/manual.mdc"));
        assert!(!block.contains("Only for a requested migration."));
    }

    #[test]
    fn windsurf_triggers_control_prompt_injection_and_path_activation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for (name, frontmatter, body) in [
            ("always", "trigger: always_on", "Global coding standards."),
            (
                "frontend",
                "trigger: 'glob'\nglobs: [\"src/**/*.{ts,tsx}\"]",
                "Frontend implementation details.",
            ),
            (
                "migration",
                "trigger: model_decision\ndescription: >-\n  Database tasks: migrations\n  and schema changes.\nglobs: **",
                "Detailed database migration procedure.",
            ),
            (
                "manual",
                "trigger: manual\ndescription: Never select this automatically.\nglobs: **",
                "Explicit maintenance procedure.",
            ),
        ] {
            write(
                root,
                &format!(".windsurf/rules/{name}.md"),
                &format!("---\n{frontmatter}\n---\n{body}"),
            );
        }
        let rules = discover_foreign_rules(root);
        assert_eq!(rules.rules.len(), 4);
        assert_eq!(rules.always_rules().count(), 1);
        assert_eq!(rules.scoped_rules().count(), 1);
        let block = rules.system_prompt_block().expect("rules block");
        assert!(block.contains("Global coding standards."));
        assert!(block.contains("Database tasks: migrations and schema changes."));
        assert!(block.contains(".windsurf/rules/migration.md"));
        assert!(block.contains(".windsurf/rules/manual.md"));
        for hidden in [
            "trigger:",
            "Frontend implementation details.",
            "Detailed database migration procedure.",
            "Explicit maintenance procedure.",
            "Never select this automatically.",
        ] {
            assert!(!block.contains(hidden), "must not inject {hidden}");
        }
        let matcher = ScopedRuleMatcher::new(&rules.rules);
        for path in ["src/main.ts", "src/ui/view.tsx"] {
            let matches = matcher.matching_rules(Path::new(path), root);
            assert_eq!(matches.len(), 1);
            assert_eq!(rules.rules[matches[0]].source, ".windsurf/rules/frontend.md");
        }
        assert!(matcher.matching_rules(Path::new("src/main.rs"), root).is_empty());
    }

    #[test]
    fn incomplete_and_unknown_windsurf_triggers_never_become_global() {
        let root = Path::new("workspace");
        for frontmatter in [
            "trigger: glob",
            "trigger: glob\nglobs: []",
            "trigger: model_decision",
            "trigger: model_decision\ndescription: ''",
            "trigger: future_mode\nglobs: **\ndescription: Not authorized.",
            "trigger: ''",
        ] {
            let rule = parse_windsurf_rule(
                &root.join(".windsurf/rules/guarded.md"),
                root,
                &format!("---\n{frontmatter}\n---\nGuarded instructions."),
            );
            assert!(!rule.always_apply, "frontmatter: {frontmatter}");
            assert!(rule.globs.is_empty());
            assert!(rule.description.is_none());
            assert_eq!(rule.content, "Guarded instructions.");
        }
    }

    #[test]
    fn cursor_description_selects_on_demand_without_injecting_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        write(
            root,
            ".cursor/rules/review.mdc",
            "---\nalwaysApply: false\ndescription: \"Security review of authentication changes\"\n---\nDetailed security review checklist.",
        );
        let rules = discover_foreign_rules(root);
        assert_eq!(rules.always_rules().count(), 0);
        assert_eq!(rules.scoped_rules().count(), 0);
        let block = rules.system_prompt_block().expect("description hint");
        assert!(block.contains("Security review of authentication changes"));
        assert!(block.contains(".cursor/rules/review.mdc"));
        assert!(!block.contains("Detailed security review checklist."));
        assert!(!block.contains("read only when explicitly requested"));
    }

    #[test]
    fn selection_descriptions_participate_in_deduplication_and_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        for (name, description) in [("a", "Database changes"), ("b", "API changes")] {
            write(
                root,
                &format!(".windsurf/rules/{name}.md"),
                &format!(
                    "---\ntrigger: model_decision\ndescription: {description}\n---\nShared checklist."
                ),
            );
        }
        let rules = discover_foreign_rules(root);
        assert_eq!(rules.rules.len(), 2, "distinct selection hints must survive");
        let block = rules.system_prompt_block().expect("selection hints");
        assert!(block.contains("Database changes"));
        assert!(block.contains("API changes"));
        assert!(!block.contains("Shared checklist."));

        let large = tempfile::tempdir().expect("tempdir");
        write(
            large.path(),
            ".windsurf/rules/huge.md",
            &format!(
                "---\ntrigger: model_decision\ndescription: {}\n---\nBody.",
                "d".repeat(FOREIGN_RULES_BUDGET_BYTES)
            ),
        );
        let rules = discover_foreign_rules(large.path());
        assert!(rules.rules.is_empty());
        assert_eq!(rules.truncated_rules, 1);
    }

    #[test]
    fn frontmatter_sequence_colons_and_description_block_scalars_are_content() {
        let (fields, body) = split_simple_frontmatter(
            "---\nglobs:\n  - \"src/namespace:*.rs\"\n  - \"*.ts\"\ndescription: |\n  Use for: API changes\n  involving authentication.\nalwaysApply: false\n---\nRule body.",
        );
        assert_eq!(fields.len(), 3);
        assert_eq!(
            parse_glob_list(&fields[0].1),
            vec!["src/namespace:*.rs", "*.ts"]
        );
        assert_eq!(
            rule_description(&fields).as_deref(),
            Some("Use for: API changes involving authentication.")
        );
        assert_eq!(fields[2], ("alwaysApply".to_string(), "false".to_string()));
        assert_eq!(body, "Rule body.");
    }
}
