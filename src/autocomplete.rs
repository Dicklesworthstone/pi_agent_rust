//! Autocomplete provider for interactive editor input.
//!
//! This module is intentionally rendering-agnostic: it takes editor text + cursor
//! position and returns structured suggestions plus the range that should be
//! replaced when applying a selection.
//!
//! Current suggestion sources (legacy parity targets):
//! - Built-in slash commands (e.g., `/help`, `/model`)
//! - Prompt templates (`/<template>`) from the resource loader
//! - Skills (`/skill:<name>`) when skill commands are enabled
//! - File references (`@path`) with a cached project file index
//! - Path completions when the cursor is in a path-like token

use std::cmp::Ordering;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::resources::ResourceLoader;
use ignore::WalkBuilder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutocompleteItemKind {
    SlashCommand,
    ExtensionCommand,
    PromptTemplate,
    Skill,
    File,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    pub kind: AutocompleteItemKind,
    pub label: String,
    pub insert: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteResponse {
    pub replace: Range<usize>,
    pub items: Vec<AutocompleteItem>,
}

#[derive(Debug, Clone, Default)]
pub struct AutocompleteCatalog {
    pub prompt_templates: Vec<NamedEntry>,
    pub skills: Vec<NamedEntry>,
    pub extension_commands: Vec<NamedEntry>,
    pub enable_skill_commands: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedEntry {
    pub name: String,
    pub description: Option<String>,
}

impl AutocompleteCatalog {
    #[must_use]
    pub fn from_resources(resources: &ResourceLoader) -> Self {
        let mut prompt_templates = resources
            .prompts()
            .iter()
            .map(|template| NamedEntry {
                name: template.name.clone(),
                description: Some(template.description.clone()).filter(|d| !d.trim().is_empty()),
            })
            .collect::<Vec<_>>();

        prompt_templates.sort_by(|a, b| a.name.cmp(&b.name));

        let mut skills = resources
            .skills()
            .iter()
            .map(|skill| NamedEntry {
                name: skill.name.clone(),
                description: Some(skill.description.clone()).filter(|d| !d.trim().is_empty()),
            })
            .collect::<Vec<_>>();

        skills.sort_by(|a, b| a.name.cmp(&b.name));

        Self {
            prompt_templates,
            skills,
            extension_commands: Vec::new(),
            enable_skill_commands: resources.enable_skill_commands(),
        }
    }
}

#[derive(Debug)]
pub struct AutocompleteProvider {
    cwd: PathBuf,
    home_dir_override: Option<PathBuf>,
    catalog: AutocompleteCatalog,
    file_cache: FileCache,
    max_items: usize,
}

impl AutocompleteProvider {
    #[must_use]
    pub const fn new(cwd: PathBuf, catalog: AutocompleteCatalog) -> Self {
        Self {
            cwd,
            home_dir_override: None,
            catalog,
            file_cache: FileCache::new(),
            max_items: 50,
        }
    }

    pub fn set_catalog(&mut self, catalog: AutocompleteCatalog) {
        self.catalog = catalog;
    }

    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
        self.file_cache.invalidate();
    }

    pub const fn max_items(&self) -> usize {
        self.max_items
    }

    pub fn set_max_items(&mut self, max_items: usize) {
        self.max_items = max_items.max(1);
    }

    /// Return suggestions for the given editor state.
    ///
    /// `cursor` is interpreted as a byte offset into `text`. If it is out of
    /// bounds or not on a UTF-8 boundary, it is clamped to the nearest safe
    /// boundary.
    #[must_use]
    pub fn suggest(&mut self, text: &str, cursor: usize) -> AutocompleteResponse {
        let cursor = clamp_cursor(text, cursor);
        let segment = token_at_cursor(text, cursor);

        if segment.text.starts_with('/') {
            return self.suggest_slash(&segment);
        }

        if segment.text.starts_with('@') {
            return self.suggest_file_ref(&segment);
        }

        if is_path_like(segment.text) {
            return self.suggest_path(&segment);
        }

        AutocompleteResponse {
            replace: cursor..cursor,
            items: Vec::new(),
        }
    }

    pub(crate) fn resolve_file_ref(&mut self, candidate: &str) -> Option<String> {
        let normalized = normalize_file_ref_candidate(candidate);
        if normalized.is_empty() {
            return None;
        }

        if is_absolute_like(&normalized) {
            return Some(normalized);
        }

        self.file_cache.refresh_if_needed(&self.cwd);
        let stripped = normalized.strip_prefix("./").unwrap_or(&normalized);
        if self.file_cache.files.iter().any(|path| path == stripped) {
            return Some(stripped.to_string());
        }

        None
    }

    #[allow(clippy::too_many_lines)]
    fn suggest_slash(&self, token: &TokenAtCursor<'_>) -> AutocompleteResponse {
        let query = token.text.trim_start_matches('/');

        // `/skill:<name>` is special-cased.
        if let Some(skill_query) = query.strip_prefix("skill:") {
            if !self.catalog.enable_skill_commands {
                return AutocompleteResponse {
                    replace: token.range.clone(),
                    items: Vec::new(),
                };
            }

            let mut items = self
                .catalog
                .skills
                .iter()
                .filter_map(|skill| {
                    let (is_prefix, score) = fuzzy_match_score(&skill.name, skill_query)?;
                    Some(ScoredItem {
                        is_prefix,
                        score,
                        kind_rank: kind_rank(AutocompleteItemKind::Skill),
                        label: format!("/skill:{}", skill.name),
                        item: AutocompleteItem {
                            kind: AutocompleteItemKind::Skill,
                            label: format!("/skill:{}", skill.name),
                            insert: format!("/skill:{}", skill.name),
                            description: skill.description.clone(),
                        },
                    })
                })
                .collect::<Vec<_>>();

            sort_scored_items(&mut items);
            let items = items
                .into_iter()
                .take(self.max_items)
                .map(|s| s.item)
                .collect();

            return AutocompleteResponse {
                replace: token.range.clone(),
                items,
            };
        }

        let mut items = Vec::new();

        // Built-in slash commands.
        for cmd in builtin_slash_commands() {
            if let Some((is_prefix, score)) = fuzzy_match_score(cmd.name, query) {
                let label = format!("/{}", cmd.name);
                items.push(ScoredItem {
                    is_prefix,
                    score,
                    kind_rank: kind_rank(AutocompleteItemKind::SlashCommand),
                    label: label.clone(),
                    item: AutocompleteItem {
                        kind: AutocompleteItemKind::SlashCommand,
                        label: label.clone(),
                        insert: label,
                        description: Some(cmd.description.to_string()),
                    },
                });
            }
        }

        // Extension commands.
        for cmd in &self.catalog.extension_commands {
            if let Some((is_prefix, score)) = fuzzy_match_score(&cmd.name, query) {
                let label = format!("/{}", cmd.name);
                items.push(ScoredItem {
                    is_prefix,
                    score,
                    kind_rank: kind_rank(AutocompleteItemKind::ExtensionCommand),
                    label: label.clone(),
                    item: AutocompleteItem {
                        kind: AutocompleteItemKind::ExtensionCommand,
                        label: label.clone(),
                        insert: label,
                        description: cmd.description.clone(),
                    },
                });
            }
        }

        // Prompt templates.
        for template in &self.catalog.prompt_templates {
            if let Some((is_prefix, score)) = fuzzy_match_score(&template.name, query) {
                let label = format!("/{}", template.name);
                items.push(ScoredItem {
                    is_prefix,
                    score,
                    kind_rank: kind_rank(AutocompleteItemKind::PromptTemplate),
                    label: label.clone(),
                    item: AutocompleteItem {
                        kind: AutocompleteItemKind::PromptTemplate,
                        label: label.clone(),
                        insert: label,
                        description: template.description.clone(),
                    },
                });
            }
        }

        sort_scored_items(&mut items);
        let items = items
            .into_iter()
            .take(self.max_items)
            .map(|s| s.item)
            .collect();

        AutocompleteResponse {
            replace: token.range.clone(),
            items,
        }
    }

    fn suggest_file_ref(&mut self, token: &TokenAtCursor<'_>) -> AutocompleteResponse {
        let query = token.text.trim_start_matches('@');
        self.file_cache.refresh_if_needed(&self.cwd);

        let mut items = self
            .file_cache
            .files
            .iter()
            .filter_map(|path| {
                let (is_prefix, score) = fuzzy_match_score(path, query)?;
                let label = format!("@{path}");
                Some(ScoredItem {
                    is_prefix,
                    score,
                    kind_rank: kind_rank(AutocompleteItemKind::File),
                    label: label.clone(),
                    item: AutocompleteItem {
                        kind: AutocompleteItemKind::File,
                        label: label.clone(),
                        insert: label,
                        description: None,
                    },
                })
            })
            .collect::<Vec<_>>();

        sort_scored_items(&mut items);
        let items = items
            .into_iter()
            .take(self.max_items)
            .map(|s| s.item)
            .collect();

        AutocompleteResponse {
            replace: token.range.clone(),
            items,
        }
    }

    fn suggest_path(&self, token: &TokenAtCursor<'_>) -> AutocompleteResponse {
        let raw = token.text.trim();
        let (dir_part_raw, base_part) = split_path_prefix(raw);

        let Some(dir_path) =
            resolve_dir_path(&self.cwd, &dir_part_raw, self.home_dir_override.as_deref())
        else {
            return AutocompleteResponse {
                replace: token.range.clone(),
                items: Vec::new(),
            };
        };

        let mut items = Vec::new();
        for entry in WalkBuilder::new(&dir_path)
            .require_git(false)
            .max_depth(Some(1))
            .build()
            .filter_map(Result::ok)
        {
            if entry.depth() != 1 {
                continue;
            }

            let Some(file_name) = entry.file_name().to_str() else {
                continue;
            };

            if !base_part.is_empty() && !file_name.starts_with(base_part.as_str()) {
                continue;
            }

            let mut insert = if dir_part_raw == "." {
                if raw.starts_with("./") {
                    format!("./{file_name}")
                } else {
                    file_name.to_string()
                }
            } else if dir_part_raw.ends_with(std::path::MAIN_SEPARATOR)
                || dir_part_raw.ends_with('/')
            {
                format!("{dir_part_raw}{file_name}")
            } else {
                format!("{dir_part_raw}/{file_name}")
            };

            let is_dir = entry.file_type().is_some_and(|ty| ty.is_dir());
            if is_dir {
                insert.push('/');
            }

            let label = insert.clone();
            items.push(ScoredItem {
                is_prefix: true,
                score: 0,
                kind_rank: kind_rank(AutocompleteItemKind::Path),
                label: label.clone(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::Path,
                    label,
                    insert,
                    description: None,
                },
            });
        }

        sort_scored_items(&mut items);
        let items = items
            .into_iter()
            .take(self.max_items)
            .map(|s| s.item)
            .collect();

        AutocompleteResponse {
            replace: token.range.clone(),
            items,
        }
    }
}

#[derive(Debug)]
struct FileCache {
    files: Vec<String>,
    refreshed_at: Option<Instant>,
}

impl FileCache {
    const TTL: Duration = Duration::from_secs(2);

    const fn new() -> Self {
        Self {
            files: Vec::new(),
            refreshed_at: None,
        }
    }

    fn invalidate(&mut self) {
        self.files.clear();
        self.refreshed_at = None;
    }

    fn refresh_if_needed(&mut self, cwd: &Path) {
        let now = Instant::now();
        let is_fresh = self
            .refreshed_at
            .is_some_and(|t| now.duration_since(t) <= Self::TTL);
        if is_fresh && !self.files.is_empty() {
            return;
        }

        self.files = collect_project_files(cwd);
        self.refreshed_at = Some(now);
    }
}

fn collect_project_files(cwd: &Path) -> Vec<String> {
    // Prefer a fast external enumerator when present.
    if let Some(bin) = find_fd_binary() {
        if let Some(files) = run_fd_list_files(bin, cwd) {
            return files;
        }
    }

    walk_project_files(cwd)
}

fn normalize_file_ref_candidate(candidate: &str) -> String {
    candidate.trim().replace('\\', "/")
}

fn is_absolute_like(candidate: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    if candidate.starts_with('~') {
        return true;
    }
    if candidate.starts_with("//") {
        return true;
    }
    if Path::new(candidate).is_absolute() {
        return true;
    }
    candidate.as_bytes().get(1) == Some(&b':')
}

/// Cached result of fd binary detection.
/// Uses OnceLock to avoid spawning processes on every file cache refresh.
static FD_BINARY_CACHE: OnceLock<Option<&'static str>> = OnceLock::new();

fn find_fd_binary() -> Option<&'static str> {
    *FD_BINARY_CACHE.get_or_init(|| {
        ["fd", "fdfind"].into_iter().find(|&candidate| {
            std::process::Command::new(candidate)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok()
        })
    })
}

fn run_fd_list_files(bin: &str, cwd: &Path) -> Option<Vec<String>> {
    let output = std::process::Command::new(bin)
        .current_dir(cwd)
        .arg("--type")
        .arg("f")
        .arg("--strip-cwd-prefix")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut files = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| line.replace('\\', "/"))
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    Some(files)
}

fn walk_project_files(cwd: &Path) -> Vec<String> {
    let mut files = Vec::new();

    let walker = ignore::WalkBuilder::new(cwd)
        .hidden(false)
        .follow_links(false)
        .standard_filters(true)
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        if !entry.file_type().is_some_and(|ty| ty.is_file()) {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(cwd) {
            let rel = rel.display().to_string().replace('\\', "/");
            if !rel.is_empty() && !rel.starts_with("..") {
                files.push(rel);
            }
        }
    }

    files.sort();
    files.dedup();
    files
}

#[derive(Debug, Clone, Copy)]
struct BuiltinSlashCommand {
    name: &'static str,
    description: &'static str,
}

const fn builtin_slash_commands() -> &'static [BuiltinSlashCommand] {
    &[
        BuiltinSlashCommand {
            name: "help",
            description: "Show help for interactive commands",
        },
        BuiltinSlashCommand {
            name: "login",
            description: "OAuth login (provider-specific)",
        },
        BuiltinSlashCommand {
            name: "logout",
            description: "Remove stored OAuth credentials",
        },
        BuiltinSlashCommand {
            name: "clear",
            description: "Clear conversation history",
        },
        BuiltinSlashCommand {
            name: "model",
            description: "Show or change the current model",
        },
        BuiltinSlashCommand {
            name: "thinking",
            description: "Set thinking level (off/minimal/low/medium/high/xhigh)",
        },
        BuiltinSlashCommand {
            name: "scoped-models",
            description: "Show or set model scope patterns",
        },
        BuiltinSlashCommand {
            name: "exit",
            description: "Exit Pi",
        },
        BuiltinSlashCommand {
            name: "history",
            description: "Show input history",
        },
        BuiltinSlashCommand {
            name: "export",
            description: "Export conversation to HTML",
        },
        BuiltinSlashCommand {
            name: "session",
            description: "Show session info",
        },
        BuiltinSlashCommand {
            name: "settings",
            description: "Show current settings summary",
        },
        BuiltinSlashCommand {
            name: "theme",
            description: "List or switch themes",
        },
        BuiltinSlashCommand {
            name: "resume",
            description: "Pick and resume a previous session",
        },
        BuiltinSlashCommand {
            name: "new",
            description: "Start a new session",
        },
        BuiltinSlashCommand {
            name: "copy",
            description: "Copy last assistant message to clipboard",
        },
        BuiltinSlashCommand {
            name: "name",
            description: "Set session display name",
        },
        BuiltinSlashCommand {
            name: "hotkeys",
            description: "Show keyboard shortcuts",
        },
        BuiltinSlashCommand {
            name: "changelog",
            description: "Show changelog entries",
        },
        BuiltinSlashCommand {
            name: "tree",
            description: "Show session branch tree summary",
        },
        BuiltinSlashCommand {
            name: "fork",
            description: "Branch from a previous user message",
        },
        BuiltinSlashCommand {
            name: "compact",
            description: "Compact older context",
        },
        BuiltinSlashCommand {
            name: "reload",
            description: "Reload resources from disk",
        },
        BuiltinSlashCommand {
            name: "share",
            description: "Export to a temp HTML file and show path",
        },
    ]
}

const fn kind_rank(kind: AutocompleteItemKind) -> u8 {
    match kind {
        AutocompleteItemKind::SlashCommand => 0,
        AutocompleteItemKind::ExtensionCommand => 1,
        AutocompleteItemKind::PromptTemplate => 2,
        AutocompleteItemKind::Skill => 3,
        AutocompleteItemKind::File => 4,
        AutocompleteItemKind::Path => 5,
    }
}

#[derive(Debug)]
struct ScoredItem {
    is_prefix: bool,
    score: i32,
    kind_rank: u8,
    label: String,
    item: AutocompleteItem,
}

fn sort_scored_items(items: &mut [ScoredItem]) {
    items.sort_by(|a, b| {
        let prefix_cmp = b.is_prefix.cmp(&a.is_prefix);
        if prefix_cmp != Ordering::Equal {
            return prefix_cmp;
        }
        let score_cmp = b.score.cmp(&a.score);
        if score_cmp != Ordering::Equal {
            return score_cmp;
        }
        let kind_cmp = a.kind_rank.cmp(&b.kind_rank);
        if kind_cmp != Ordering::Equal {
            return kind_cmp;
        }
        a.label.cmp(&b.label)
    });
}

fn clamp_usize_to_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn fuzzy_match_score(candidate: &str, query: &str) -> Option<(bool, i32)> {
    let query = query.trim();
    if query.is_empty() {
        return Some((true, 0));
    }

    let cand = candidate.to_ascii_lowercase();
    let query = query.to_ascii_lowercase();

    if cand.starts_with(&query) {
        // Prefer shorter completions for prefix matches.
        let penalty =
            clamp_usize_to_i32(cand.len()).saturating_sub(clamp_usize_to_i32(query.len()));
        return Some((true, 1_000 - penalty));
    }

    if let Some(idx) = cand.find(&query) {
        return Some((false, 700 - clamp_usize_to_i32(idx)));
    }

    // Subsequence match with a gap penalty.
    let mut score = 500i32;
    let mut search_from = 0usize;
    for q in query.chars() {
        let pos = cand[search_from..].find(q)?;
        let abs = search_from + pos;
        let gap = clamp_usize_to_i32(abs.saturating_sub(search_from));
        score -= gap;
        search_from = abs + q.len_utf8();
    }

    // Prefer shorter candidates if the match score ties.
    score -= clamp_usize_to_i32(cand.len()) / 10;
    Some((false, score))
}

fn is_path_like(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    if text.starts_with('~') {
        return true;
    }
    text.starts_with("./")
        || text.starts_with("../")
        || text.starts_with("~/")
        || text.starts_with('/')
        || text.contains('/')
}

fn expand_tilde(text: &str) -> String {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).display().to_string();
        }
    }
    text.to_string()
}

fn resolve_dir_path(cwd: &Path, dir_part: &str, home_override: Option<&Path>) -> Option<PathBuf> {
    let dir_part = dir_part.trim();
    let home_dir = || home_override.map(Path::to_path_buf).or_else(dirs::home_dir);

    if dir_part == "~" {
        return home_dir();
    }
    if let Some(rest) = dir_part.strip_prefix("~/") {
        return home_dir().map(|home| home.join(rest));
    }
    if Path::new(dir_part).is_absolute() {
        return Some(PathBuf::from(dir_part));
    }

    Some(cwd.join(dir_part))
}

fn split_path_prefix(path: &str) -> (String, String) {
    let path = path.trim();
    if path == "~" {
        return ("~".to_string(), String::new());
    }
    if path.ends_with('/') {
        return (path.to_string(), String::new());
    }
    let Some((dir, base)) = path.rsplit_once('/') else {
        return (".".to_string(), path.to_string());
    };
    let dir = if dir.is_empty() {
        "/".to_string()
    } else {
        dir.to_string()
    };
    (dir, base.to_string())
}

#[derive(Debug, Clone)]
struct TokenAtCursor<'a> {
    text: &'a str,
    range: Range<usize>,
}

fn token_at_cursor(text: &str, cursor: usize) -> TokenAtCursor<'_> {
    let cursor = clamp_cursor(text, cursor);

    let start = text[..cursor].rfind(char::is_whitespace).map_or(0, |idx| {
        idx + text[idx..].chars().next().unwrap_or(' ').len_utf8()
    });
    let end = text[cursor..]
        .find(char::is_whitespace)
        .map_or(text.len(), |idx| cursor + idx);

    let start = clamp_to_char_boundary(text, start.min(end));
    let end = clamp_to_char_boundary(text, end.max(start));

    TokenAtCursor {
        text: &text[start..end],
        range: start..end,
    }
}

fn clamp_cursor(text: &str, cursor: usize) -> usize {
    clamp_to_char_boundary(text, cursor.min(text.len()))
}

fn clamp_to_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_suggests_builtins() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let resp = provider.suggest("/he", 3);
        assert_eq!(resp.replace, 0..3);
        assert!(
            resp.items
                .iter()
                .any(|item| item.insert == "/help"
                    && item.kind == AutocompleteItemKind::SlashCommand)
        );
    }

    #[test]
    fn slash_suggests_templates() {
        let catalog = AutocompleteCatalog {
            prompt_templates: vec![NamedEntry {
                name: "review".to_string(),
                description: Some("Code review".to_string()),
            }],
            skills: Vec::new(),
            extension_commands: Vec::new(),
            enable_skill_commands: false,
        };
        let mut provider = AutocompleteProvider::new(PathBuf::from("."), catalog);
        let resp = provider.suggest("/rev", 4);
        assert!(
            resp.items.iter().any(|item| item.insert == "/review"
                && item.kind == AutocompleteItemKind::PromptTemplate)
        );
    }

    #[test]
    fn skill_suggests_only_when_enabled() {
        let catalog = AutocompleteCatalog {
            prompt_templates: Vec::new(),
            skills: vec![NamedEntry {
                name: "rustfmt".to_string(),
                description: None,
            }],
            extension_commands: Vec::new(),
            enable_skill_commands: true,
        };
        let mut provider = AutocompleteProvider::new(PathBuf::from("."), catalog);
        let resp = provider.suggest("/skill:ru", "/skill:ru".len());
        assert!(resp.items.iter().any(
            |item| item.insert == "/skill:rustfmt" && item.kind == AutocompleteItemKind::Skill
        ));

        provider.set_catalog(AutocompleteCatalog {
            prompt_templates: Vec::new(),
            skills: vec![NamedEntry {
                name: "rustfmt".to_string(),
                description: None,
            }],
            extension_commands: Vec::new(),
            enable_skill_commands: false,
        });
        let resp = provider.suggest("/skill:ru", "/skill:ru".len());
        assert!(resp.items.is_empty());
    }

    #[test]
    fn set_catalog_updates_prompt_templates() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());

        let query = "/zzz_reload_test_template";
        let resp = provider.suggest(query, query.len());
        assert!(
            !resp
                .items
                .iter()
                .any(|item| item.insert == query
                    && item.kind == AutocompleteItemKind::PromptTemplate)
        );

        provider.set_catalog(AutocompleteCatalog {
            prompt_templates: vec![NamedEntry {
                name: "zzz_reload_test_template".to_string(),
                description: None,
            }],
            skills: Vec::new(),
            extension_commands: Vec::new(),
            enable_skill_commands: false,
        });
        let resp = provider.suggest(query, query.len());
        assert!(
            resp.items
                .iter()
                .any(|item| item.insert == query
                    && item.kind == AutocompleteItemKind::PromptTemplate)
        );

        provider.set_catalog(AutocompleteCatalog::default());
        let resp = provider.suggest(query, query.len());
        assert!(
            !resp
                .items
                .iter()
                .any(|item| item.insert == query
                    && item.kind == AutocompleteItemKind::PromptTemplate)
        );
    }

    #[test]
    fn file_ref_uses_cached_project_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("hello.txt"), "hi").expect("write");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());
        // Trigger a refresh and query.
        let resp = provider.suggest("@ma", 3);
        assert!(resp.items.iter().any(|item| item.insert == "@src/main.rs"));
    }

    #[test]
    fn path_suggests_children_for_prefix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}").expect("write");
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn lib() {}").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());
        let resp = provider.suggest("src/ma", "src/ma".len());
        assert_eq!(resp.replace, 0..6);
        assert!(
            resp.items.iter().any(|item| item.insert == "src/main.rs"
                && item.kind == AutocompleteItemKind::Path)
        );
        assert!(!resp.items.iter().any(|item| item.insert == "src/lib.rs"));
    }

    #[test]
    fn path_suggest_respects_gitignore_and_preserves_dot_slash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(".gitignore"), "target/\n").expect("write");
        std::fs::create_dir_all(tmp.path().join("target")).expect("mkdir");
        std::fs::create_dir_all(tmp.path().join("tags")).expect("mkdir");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());
        let resp = provider.suggest("./ta", "./ta".len());
        assert!(
            resp.items
                .iter()
                .any(|item| item.insert == "./tags/" && item.kind == AutocompleteItemKind::Path)
        );
        assert!(!resp.items.iter().any(|item| item.insert == "./target/"));
    }

    #[test]
    fn path_like_accepts_tilde() {
        assert!(is_path_like("~"));
        assert!(is_path_like("~/"));
    }

    #[test]
    fn split_path_prefix_handles_tilde() {
        assert_eq!(split_path_prefix("~"), ("~".to_string(), String::new()));
        assert_eq!(
            split_path_prefix("~/notes.txt"),
            ("~".to_string(), "notes.txt".to_string())
        );
    }

    #[test]
    fn fuzzy_match_prefers_prefix_and_shorter() {
        let (prefix_short, score_short) = fuzzy_match_score("help", "he").expect("match help");
        let (prefix_long, score_long) = fuzzy_match_score("hello", "he").expect("match hello");
        assert!(prefix_short && prefix_long);
        assert!(score_short > score_long);
    }

    #[test]
    fn fuzzy_match_accepts_subsequence() {
        let (is_prefix, score) = fuzzy_match_score("autocomplete", "acmp").expect("subsequence");
        assert!(!is_prefix);
        assert!(score > 0);
    }

    #[test]
    fn suggest_replaces_only_current_token() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let resp = provider.suggest("foo /he bar", "foo /he".len());
        assert_eq!(resp.replace, 4..7);
    }
}
