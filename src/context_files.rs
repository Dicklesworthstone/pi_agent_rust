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
//! | Windsurf | `.windsurfrules` file or `.windsurf/rules/*` dir | always |
//! | Gemini | `GEMINI.md` | always |
//!
//! Codex `AGENTS.md` and Claude `CLAUDE.md` are pi's native conventions and
//! stay owned by [`crate::app`]'s project-context loader; this module skips
//! them (native wins) and also drops any foreign rule whose content is
//! byte-identical to another already-collected rule (first occurrence wins,
//! discovery order below).
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

    /// Rules injected unconditionally (always-apply and unscoped).
    pub fn always_rules(&self) -> impl Iterator<Item = &ForeignRule> {
        self.rules.iter().filter(|rule| !rule.is_scoped())
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
                match globset::Glob::new(&anchored) {
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
        let relative = path.strip_prefix(workspace_root).unwrap_or(path);
        self.entries
            .iter()
            .filter(|(_, set)| set.is_match(relative))
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
    // Windsurf: single file or rules directory.
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
            rules.push(plain_rule(
                path,
                workspace_root,
                content,
                ForeignRuleFormat::Windsurf,
            ));
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
    // context file (AGENTS.md / CLAUDE.md at the workspace root) or to an
    // earlier foreign rule.
    let mut seen: HashSet<String> = HashSet::new();
    for native in ["AGENTS.md", "CLAUDE.md"] {
        if let Some(content) = read_rule_file(&workspace_root.join(native)) {
            seen.insert(normalized_body(&content));
        }
    }
    let mut deduped = Vec::new();
    for rule in rules {
        if rule.content.trim().is_empty() {
            continue;
        }
        if seen.insert(normalized_body(&rule.content)) {
            deduped.push(rule);
        }
    }

    // Budget: keep discovery-order prefix; drop the first overflowing rule
    // and everything after it (deterministic).
    let mut kept = Vec::new();
    let mut spent = 0usize;
    let mut truncated = 0usize;
    for rule in deduped {
        let cost = rule.content.len();
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
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .display()
        .to_string()
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
    let mut fields = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            if !key.is_empty() {
                fields.push((key.to_string(), value.trim().to_string()));
            }
        } else if let Some((last_key, last_value)) = fields.last_mut() {
            // YAML list items (`- "*.ts"`) under the previous key.
            let item = line.trim();
            if let Some(item) = item.strip_prefix('-') {
                let _ = last_key;
                if !last_value.is_empty() {
                    last_value.push(',');
                }
                last_value.push_str(item.trim());
            }
        }
    }
    if !closed {
        return (Vec::new(), content.to_string());
    }
    (fields, lines.collect::<Vec<_>>().join("\n"))
}

/// Parse a comma-separated (or YAML-list-flattened) glob field value.
fn parse_glob_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .map(|glob| glob.trim_matches('"').trim_matches('\'').trim())
        .filter(|glob| !glob.is_empty() && *glob != "[]")
        .map(str::to_string)
        .collect()
}

/// Cursor `.mdc`: frontmatter `description` / `globs` / `alwaysApply`.
fn parse_cursor_mdc(path: &Path, workspace_root: &Path, content: &str) -> ForeignRule {
    let (fields, body) = split_simple_frontmatter(content);
    let mut globs = Vec::new();
    let mut always_apply = false;
    for (key, value) in &fields {
        match key.as_str() {
            "globs" => globs = parse_glob_list(value),
            "alwaysApply" => always_apply = value.trim() == "true",
            _ => {}
        }
    }
    // MDC semantics: no globs and no alwaysApply means "agent-requested" /
    // description-gated; pi treats content-bearing rules without scoping as
    // always-apply so they are not silently dropped.
    if globs.is_empty() {
        always_apply = true;
    }
    ForeignRule {
        content: body,
        globs,
        always_apply,
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
        source: relative_display(path, workspace_root),
        format: ForeignRuleFormat::CopilotScoped,
    }
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
}
