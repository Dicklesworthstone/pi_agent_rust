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
    Model,
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

    pub(crate) fn refresh_background(&mut self) {
        self.file_cache.refresh_if_needed(&self.cwd);
    }

    /// Return suggestions for the given editor state.
    ///
    /// `cursor` is interpreted as a byte offset into `text`. If it is out of
    /// bounds or not on a UTF-8 boundary, it is clamped to the nearest safe
    /// boundary.
    #[must_use]
    pub fn suggest(&mut self, text: &str, cursor: usize) -> AutocompleteResponse {
        let cursor = clamp_cursor(text, cursor);
        if let Some(token) = auth_provider_argument_token(text, cursor) {
            return self.suggest_auth_provider_argument(&token);
        }
        if let Some(token) = model_argument_token(text, cursor) {
            return self.suggest_model_argument(&token);
        }
        let segment = token_at_cursor(text, cursor);

        if segment.text.starts_with('/') {
            let path_response = self.suggest_path(&segment);
            if should_prefer_absolute_path_completion(segment.text, &path_response) {
                return path_response;
            }
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
        let query = token.text.strip_prefix('@').unwrap_or(token.text);
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
        let separator = preferred_path_separator(raw, &dir_part_raw);

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
                if raw.starts_with("./") || raw.starts_with(".\\") {
                    format!(".{separator}{file_name}")
                } else {
                    file_name.to_string()
                }
            } else if dir_part_raw.ends_with('/') || dir_part_raw.ends_with('\\') {
                format!("{dir_part_raw}{file_name}")
            } else {
                format!("{dir_part_raw}{separator}{file_name}")
            };

            let is_dir = entry.file_type().is_some_and(|ty| ty.is_dir());
            if is_dir {
                insert.push(separator);
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

    fn suggest_model_argument(&self, token: &TokenAtCursor<'_>) -> AutocompleteResponse {
        let query = token.text.trim();
        let mut items = crate::models::model_autocomplete_candidates()
            .iter()
            .filter_map(|candidate| {
                let (is_prefix, score) = fuzzy_match_score(&candidate.slug, query)?;
                Some(ScoredItem {
                    is_prefix,
                    score,
                    kind_rank: kind_rank(AutocompleteItemKind::Model),
                    label: candidate.slug.clone(),
                    item: AutocompleteItem {
                        kind: AutocompleteItemKind::Model,
                        label: candidate.slug.clone(),
                        insert: candidate.slug.clone(),
                        description: candidate.description.clone(),
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

    fn suggest_auth_provider_argument(&self, token: &TokenAtCursor<'_>) -> AutocompleteResponse {
        let query = token.text.trim();
        let mut items = Vec::new();

        for meta in crate::provider_metadata::PROVIDER_METADATA {
            if let Some((is_prefix, score)) = fuzzy_match_score(meta.canonical_id, query) {
                items.push(ScoredItem {
                    is_prefix,
                    score,
                    kind_rank: kind_rank(AutocompleteItemKind::SlashCommand),
                    label: meta.canonical_id.to_string(),
                    item: AutocompleteItem {
                        kind: AutocompleteItemKind::SlashCommand,
                        label: meta.canonical_id.to_string(),
                        insert: meta.canonical_id.to_string(),
                        description: meta
                            .display_name
                            .map(|name| format!("Provider: {name}"))
                            .or_else(|| Some("Provider".to_string())),
                    },
                });
            }

            for alias in meta.aliases {
                if let Some((is_prefix, score)) = fuzzy_match_score(alias, query) {
                    items.push(ScoredItem {
                        is_prefix,
                        score,
                        kind_rank: kind_rank(AutocompleteItemKind::SlashCommand),
                        label: alias.to_string(),
                        item: AutocompleteItem {
                            kind: AutocompleteItemKind::SlashCommand,
                            label: alias.to_string(),
                            insert: alias.to_string(),
                            description: Some(format!("Alias for {}", meta.canonical_id)),
                        },
                    });
                }
            }
        }

        sort_scored_items(&mut items);
        let mut dedup = std::collections::HashSet::new();
        let items = items
            .into_iter()
            .filter(|entry| dedup.insert(entry.item.insert.clone()))
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
    last_update_request: Option<Instant>,
    update_rx: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    updating: bool,
}

impl FileCache {
    const TTL: Duration = Duration::from_secs(30);

    const fn new() -> Self {
        Self {
            files: Vec::new(),
            last_update_request: None,
            update_rx: None,
            updating: false,
        }
    }

    fn invalidate(&mut self) {
        self.files.clear();
        self.last_update_request = None;
        // Drop stale in-flight updates so old cwd results cannot repopulate cache.
        self.update_rx = None;
        self.updating = false;
    }

    fn refresh_if_needed(&mut self, cwd: &Path) {
        // Poll for completed updates
        if let Some(rx) = &self.update_rx {
            match rx.try_recv() {
                Ok(files) => {
                    self.files = files;
                    self.updating = false;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.updating = false;
                    self.update_rx = None;
                }
            }
        }

        let now = Instant::now();
        let is_fresh = self
            .last_update_request
            .is_some_and(|t| now.duration_since(t) <= Self::TTL);

        if !is_fresh && !self.updating {
            self.updating = true;
            self.last_update_request = Some(now);
            let cwd_buf = cwd.to_path_buf();
            let (tx, rx) = std::sync::mpsc::channel();
            self.update_rx = Some(rx);

            std::thread::spawn(move || {
                let files = collect_project_files(&cwd_buf);
                let _ = tx.send(files);
            });
        }
    }
}

const MAX_FILE_CACHE_ENTRIES: usize = 5000;

fn collect_project_files(cwd: &Path) -> Vec<String> {
    let mut files = find_fd_binary().map_or_else(
        || walk_project_files(cwd),
        |bin| run_fd_list_files(bin, cwd).unwrap_or_else(|| walk_project_files(cwd)),
    );

    if files.len() > MAX_FILE_CACHE_ENTRIES {
        files.truncate(MAX_FILE_CACHE_ENTRIES);
    }
    files
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
                .stdin(std::process::Stdio::null())
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

// A flat static data table of built-in slash commands; not splittable into a
// cohesive helper, so allow the line count rather than fragment the table.
#[allow(clippy::too_many_lines)]
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
            description: "Set thinking level (off/minimal/low/medium/high/xhigh/max)",
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
        BuiltinSlashCommand {
            name: "mcp",
            description: "Show MCP server status (Model Context Protocol)",
        },
    ]
}

const fn kind_rank(kind: AutocompleteItemKind) -> u8 {
    match kind {
        AutocompleteItemKind::SlashCommand => 0,
        AutocompleteItemKind::ExtensionCommand => 1,
        AutocompleteItemKind::PromptTemplate => 2,
        AutocompleteItemKind::Skill => 3,
        AutocompleteItemKind::Model => 4,
        AutocompleteItemKind::File => 5,
        AutocompleteItemKind::Path => 6,
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
        || text.starts_with(".\\")
        || text.starts_with("../")
        || text.starts_with("..\\")
        || text.starts_with("~/")
        || text.starts_with("~\\")
        || text.starts_with('/')
        || text.starts_with('\\')
        || text.contains('/')
        || text.contains('\\')
}

fn expand_tilde(text: &str) -> String {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest).display().to_string();
    }
    text.to_string()
}

fn resolve_dir_path(cwd: &Path, dir_part: &str, home_override: Option<&Path>) -> Option<PathBuf> {
    let dir_part = dir_part.trim();
    let home_dir = || home_override.map(Path::to_path_buf).or_else(dirs::home_dir);

    if dir_part == "~" {
        return home_dir();
    }
    if let Some(rest) = dir_part
        .strip_prefix("~/")
        .or_else(|| dir_part.strip_prefix("~\\"))
    {
        return home_dir().map(|home| home.join(rest));
    }
    if is_absolute_dir_path_like(dir_part) {
        return Some(PathBuf::from(dir_part));
    }

    Some(cwd.join(dir_part))
}

fn split_path_prefix(path: &str) -> (String, String) {
    let path = path.trim();
    if path == "~" {
        return ("~".to_string(), String::new());
    }
    if path.ends_with('/') || path.ends_with('\\') {
        return (path.to_string(), String::new());
    }
    let Some(separator_index) = path.rfind(['/', '\\']) else {
        return (".".to_string(), path.to_string());
    };

    let separator = path[separator_index..].chars().next().unwrap_or('/');
    let base_start = separator_index + separator.len_utf8();
    let base = path[base_start..].to_string();

    let dir = if separator_index == 0 {
        separator.to_string()
    } else if separator_index == 2 && path.as_bytes().get(1) == Some(&b':') {
        path[..base_start].to_string()
    } else {
        path[..separator_index].to_string()
    };

    (dir, base)
}

fn is_absolute_dir_path_like(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }

    if Path::new(path).is_absolute() {
        return true;
    }

    path.starts_with('/')
        || path.starts_with('\\')
        || (path.as_bytes().get(1) == Some(&b':')
            && matches!(path.as_bytes().get(2), Some(&b'/' | &b'\\')))
}

fn preferred_path_separator(raw: &str, dir_part_raw: &str) -> char {
    if raw.contains('\\') || dir_part_raw.contains('\\') {
        '\\'
    } else {
        '/'
    }
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

fn slash_first_argument_token<'a>(
    text: &'a str,
    cursor: usize,
    commands: &[&str],
) -> Option<TokenAtCursor<'a>> {
    let cursor = clamp_cursor(text, cursor);
    let line_start = text[..cursor].rfind('\n').map_or(0, |idx| idx + 1);
    let prefix = &text[line_start..cursor];
    let trimmed = prefix.trim_start();
    let leading_ws = prefix.len().saturating_sub(trimmed.len());

    let command = commands.iter().copied().find(|command| {
        trimmed.starts_with(command)
            && text
                .get(line_start + leading_ws + command.len()..)
                .and_then(|tail| tail.chars().next())
                .is_none_or(char::is_whitespace)
    })?;

    let command_end = line_start + leading_ws + command.len();
    if cursor <= command_end {
        return None;
    }

    let segment = token_at_cursor(text, cursor);
    if text[command_end..segment.range.start]
        .chars()
        .any(|ch| !ch.is_whitespace())
    {
        return None;
    }

    Some(segment)
}

fn model_argument_token(text: &str, cursor: usize) -> Option<TokenAtCursor<'_>> {
    slash_first_argument_token(text, cursor, &["/model", "/m"])
}

fn auth_provider_argument_token(text: &str, cursor: usize) -> Option<TokenAtCursor<'_>> {
    slash_first_argument_token(text, cursor, &["/login", "/logout"])
}

fn should_prefer_absolute_path_completion(
    token_text: &str,
    path_response: &AutocompleteResponse,
) -> bool {
    let token_text = token_text.trim();
    if !token_text.starts_with('/') {
        return false;
    }

    if token_text == "/" {
        return false;
    }

    if token_text.starts_with("/.") || token_text[1..].contains('/') {
        return true;
    }

    path_response
        .items
        .iter()
        .any(|item| item.insert.starts_with(token_text))
}

fn clamp_cursor(text: &str, cursor: usize) -> usize {
    clamp_to_char_boundary(text, cursor.min(text.len()))
}

const fn clamp_to_char_boundary(text: &str, mut idx: usize) -> usize {
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
        // Pre-populate the file cache (refresh_if_needed is async).
        provider.file_cache.files = walk_project_files(tmp.path());
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

    #[test]
    fn slash_suggests_extension_commands() {
        let catalog = AutocompleteCatalog {
            prompt_templates: Vec::new(),
            skills: Vec::new(),
            extension_commands: vec![NamedEntry {
                name: "deploy".to_string(),
                description: Some("Deploy to production".to_string()),
            }],
            enable_skill_commands: false,
        };
        let mut provider = AutocompleteProvider::new(PathBuf::from("."), catalog);
        let resp = provider.suggest("/dep", 4);
        assert!(resp.items.iter().any(|item| item.insert == "/deploy"
            && item.kind == AutocompleteItemKind::ExtensionCommand
            && item.description == Some("Deploy to production".to_string())));

        // Verify extension commands don't appear with empty catalog
        let empty_catalog = AutocompleteCatalog::default();
        provider.set_catalog(empty_catalog);
        let resp = provider.suggest("/dep", 4);
        assert!(
            !resp
                .items
                .iter()
                .any(|item| item.kind == AutocompleteItemKind::ExtensionCommand)
        );
    }

    #[test]
    fn model_command_suggests_model_catalog_candidates() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let input = "/model gpt-5.2-cod";
        let resp = provider.suggest(input, input.len());
        assert!(
            resp.items
                .iter()
                .any(|item| item.kind == AutocompleteItemKind::Model
                    && item.insert == "openai/gpt-5.2-codex")
        );
    }

    #[test]
    fn model_shorthand_command_suggests_model_catalog_candidates() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let input = "/m claude-sonnet-4";
        let resp = provider.suggest(input, input.len());
        assert!(
            resp.items
                .iter()
                .any(|item| item.kind == AutocompleteItemKind::Model)
        );
    }

    #[test]
    fn login_command_suggests_provider_argument_candidates() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let input = "/login openai-cod";
        let resp = provider.suggest(input, input.len());
        assert!(resp.items.iter().any(|item| item.insert == "openai-codex"));
    }

    #[test]
    fn logout_command_suggests_provider_alias_candidates() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let input = "/logout cop";
        let resp = provider.suggest(input, input.len());
        assert!(resp.items.iter().any(|item| item.insert == "copilot"));
    }

    #[test]
    fn model_autocomplete_does_not_trigger_for_later_arguments() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let input = "/model openai/gpt-5 extra";
        let resp = provider.suggest(input, input.len());
        assert!(
            !resp
                .items
                .iter()
                .any(|item| item.kind == AutocompleteItemKind::Model)
        );
    }

    #[test]
    fn auth_provider_autocomplete_does_not_trigger_for_later_arguments() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let input = "/login openai extra";
        let resp = provider.suggest(input, input.len());
        assert!(!resp.items.iter().any(|item| {
            item.insert == "openai"
                || item.insert == "openai-codex"
                || item.insert == "openai-responses"
        }));
    }

    #[test]
    fn login_without_argument_keeps_slash_completion_behavior() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let input = "/log";
        let resp = provider.suggest(input, input.len());
        assert!(resp.items.iter().any(|item| item.insert == "/login"));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_path_token_prefers_path_completion_over_slash_commands() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let input = "/tmp";
        let resp = provider.suggest(input, input.len());
        assert!(!resp.items.is_empty(), "expected /tmp path suggestions");
        assert!(
            resp.items
                .iter()
                .all(|item| item.kind == AutocompleteItemKind::Path)
        );
        assert!(
            resp.items
                .iter()
                .any(|item| item.insert.starts_with("/tmp"))
        );
    }

    // ── clamp_cursor / clamp_to_char_boundary ────────────────────────

    #[test]
    fn clamp_cursor_stays_within_bounds() {
        assert_eq!(clamp_cursor("hello", 0), 0);
        assert_eq!(clamp_cursor("hello", 5), 5);
        assert_eq!(clamp_cursor("hello", 100), 5);
    }

    #[test]
    fn clamp_cursor_avoids_mid_char_boundary() {
        let text = "café"; // é is 2 bytes
        // Try clamping to byte 4, which is the middle of é (bytes 3,4)
        let clamped = clamp_cursor(text, 4);
        assert!(text.is_char_boundary(clamped));
    }

    #[test]
    fn clamp_cursor_empty_string() {
        assert_eq!(clamp_cursor("", 0), 0);
        assert_eq!(clamp_cursor("", 10), 0);
    }

    #[test]
    fn clamp_to_char_boundary_retreats_to_valid_position() {
        let text = "a🎉b"; // 🎉 is 4 bytes, starts at byte 1
        // Byte 2 is mid-emoji, should retreat to byte 1
        let clamped = clamp_to_char_boundary(text, 2);
        assert_eq!(clamped, 1);
        assert!(text.is_char_boundary(clamped));
    }

    // ── token_at_cursor ─────────────────────────────────────────────

    #[test]
    fn token_at_cursor_single_word() {
        let tok = token_at_cursor("hello", 3);
        assert_eq!(tok.text, "hello");
        assert_eq!(tok.range, 0..5);
    }

    #[test]
    fn token_at_cursor_multiple_words() {
        let tok = token_at_cursor("foo bar baz", 5);
        assert_eq!(tok.text, "bar");
        assert_eq!(tok.range, 4..7);
    }

    #[test]
    fn token_at_cursor_at_boundary() {
        // Cursor at start of "bar"
        let tok = token_at_cursor("foo bar", 4);
        assert_eq!(tok.text, "bar");
        assert_eq!(tok.range, 4..7);
    }

    #[test]
    fn token_at_cursor_at_end() {
        let tok = token_at_cursor("foo bar", 7);
        assert_eq!(tok.text, "bar");
        assert_eq!(tok.range, 4..7);
    }

    #[test]
    fn token_at_cursor_empty_string() {
        let tok = token_at_cursor("", 0);
        assert_eq!(tok.text, "");
        assert_eq!(tok.range, 0..0);
    }

    #[test]
    fn token_at_cursor_cursor_at_start() {
        let tok = token_at_cursor("hello world", 0);
        assert_eq!(tok.text, "hello");
        assert_eq!(tok.range, 0..5);
    }

    // ── fuzzy_match_score ────────────────────────────────────────────

    #[test]
    fn fuzzy_match_empty_query_returns_prefix_zero() {
        let result = fuzzy_match_score("anything", "");
        assert_eq!(result, Some((true, 0)));
    }

    #[test]
    fn fuzzy_match_whitespace_query_returns_prefix_zero() {
        let result = fuzzy_match_score("anything", "   ");
        assert_eq!(result, Some((true, 0)));
    }

    #[test]
    fn fuzzy_match_exact_prefix() {
        let (is_prefix, score) = fuzzy_match_score("help", "help").unwrap();
        assert!(is_prefix);
        assert_eq!(score, 1000); // exact match → 0 penalty
    }

    #[test]
    fn fuzzy_match_case_insensitive() {
        let (is_prefix, _) = fuzzy_match_score("Help", "he").unwrap();
        assert!(is_prefix);
    }

    #[test]
    fn fuzzy_match_substring_not_prefix() {
        let (is_prefix, score) = fuzzy_match_score("xhelp", "help").unwrap();
        assert!(!is_prefix);
        // substring found at index 1 → 700 - 1 = 699
        assert_eq!(score, 699);
    }

    #[test]
    fn fuzzy_match_no_match() {
        let result = fuzzy_match_score("help", "xyz");
        assert!(result.is_none());
    }

    #[test]
    fn fuzzy_match_subsequence_with_gaps() {
        let (is_prefix, score) = fuzzy_match_score("model", "mdl").unwrap();
        assert!(!is_prefix);
        assert!(score > 0, "Subsequence match should have positive score");
    }

    // ── is_path_like ─────────────────────────────────────────────────

    #[test]
    fn is_path_like_empty_returns_false() {
        assert!(!is_path_like(""));
        assert!(!is_path_like("   "));
    }

    #[test]
    fn is_path_like_dot_slash() {
        assert!(is_path_like("./foo"));
        assert!(is_path_like("../bar"));
    }

    #[test]
    fn is_path_like_absolute() {
        assert!(is_path_like("/usr/bin"));
    }

    #[test]
    fn is_path_like_contains_slash() {
        assert!(is_path_like("src/main.rs"));
    }

    #[test]
    fn is_path_like_windows_separators() {
        assert!(is_path_like(".\\foo"));
        assert!(is_path_like("src\\main.rs"));
        assert!(is_path_like("~\\notes.txt"));
        assert!(is_path_like("\\\\server\\share"));
    }

    #[test]
    fn is_path_like_plain_word_not_path() {
        assert!(!is_path_like("hello"));
        assert!(!is_path_like("foo.bar"));
    }

    // ── expand_tilde ─────────────────────────────────────────────────

    #[test]
    fn expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/foo/bar"), "/foo/bar");
        assert_eq!(expand_tilde("hello"), "hello");
    }

    #[test]
    fn expand_tilde_with_home() {
        let expanded = expand_tilde("~/notes.txt");
        // If there is a home dir, the path should not start with ~/
        if dirs::home_dir().is_some() {
            assert!(!expanded.starts_with("~/"));
            assert!(expanded.ends_with("notes.txt"));
        }
    }

    // ── resolve_dir_path ─────────────────────────────────────────────

    #[test]
    fn resolve_dir_path_absolute() {
        let result = resolve_dir_path(Path::new("/tmp"), "/usr/bin", None);
        assert_eq!(result, Some(PathBuf::from("/usr/bin")));
    }

    #[test]
    fn resolve_dir_path_relative() {
        let result = resolve_dir_path(Path::new("/home/user"), "src", None);
        assert_eq!(result, Some(PathBuf::from("/home/user/src")));
    }

    #[test]
    fn resolve_dir_path_tilde_with_override() {
        let result = resolve_dir_path(Path::new("/cwd"), "~/docs", Some(Path::new("/mock_home")));
        assert_eq!(result, Some(PathBuf::from("/mock_home/docs")));
    }

    #[test]
    fn resolve_dir_path_tilde_backslash_with_override() {
        let result = resolve_dir_path(Path::new("/cwd"), "~\\docs", Some(Path::new("/mock_home")));
        assert_eq!(result, Some(PathBuf::from("/mock_home/docs")));
    }

    #[test]
    fn resolve_dir_path_tilde_alone() {
        let result = resolve_dir_path(Path::new("/cwd"), "~", Some(Path::new("/mock_home")));
        assert_eq!(result, Some(PathBuf::from("/mock_home")));
    }

    #[test]
    fn resolve_dir_path_windows_drive_root_is_absolute_like() {
        let result = resolve_dir_path(Path::new("/cwd"), "C:\\Users", None);
        assert_eq!(result, Some(PathBuf::from("C:\\Users")));
    }

    #[test]
    fn resolve_dir_path_windows_rooted_backslash_is_absolute_like() {
        let result = resolve_dir_path(Path::new("/cwd"), "\\Users", None);
        assert_eq!(result, Some(PathBuf::from("\\Users")));
    }

    // ── split_path_prefix ────────────────────────────────────────────

    #[test]
    fn split_path_prefix_simple_file() {
        assert_eq!(
            split_path_prefix("hello.txt"),
            (".".to_string(), "hello.txt".to_string())
        );
    }

    #[test]
    fn split_path_prefix_trailing_slash() {
        assert_eq!(
            split_path_prefix("src/"),
            ("src/".to_string(), String::new())
        );
    }

    #[test]
    fn split_path_prefix_nested_path() {
        assert_eq!(
            split_path_prefix("src/main.rs"),
            ("src".to_string(), "main.rs".to_string())
        );
    }

    #[test]
    fn split_path_prefix_windows_relative_path() {
        assert_eq!(
            split_path_prefix("src\\main.rs"),
            ("src".to_string(), "main.rs".to_string())
        );
    }

    #[test]
    fn split_path_prefix_windows_drive_root_path() {
        assert_eq!(
            split_path_prefix("C:\\notes.txt"),
            ("C:\\".to_string(), "notes.txt".to_string())
        );
    }

    #[test]
    fn split_path_prefix_root_path() {
        assert_eq!(
            split_path_prefix("/main.rs"),
            ("/".to_string(), "main.rs".to_string())
        );
    }

    // ── normalize_file_ref_candidate ─────────────────────────────────

    #[test]
    fn normalize_file_ref_trims_whitespace() {
        assert_eq!(normalize_file_ref_candidate("  hello  "), "hello");
    }

    #[test]
    fn normalize_file_ref_replaces_backslashes() {
        assert_eq!(normalize_file_ref_candidate("src\\main.rs"), "src/main.rs");
    }

    #[test]
    fn normalize_file_ref_empty() {
        assert_eq!(normalize_file_ref_candidate(""), "");
        assert_eq!(normalize_file_ref_candidate("   "), "");
    }

    // ── is_absolute_like ─────────────────────────────────────────────

    #[test]
    fn is_absolute_like_empty() {
        assert!(!is_absolute_like(""));
    }

    #[test]
    fn is_absolute_like_tilde() {
        assert!(is_absolute_like("~/foo"));
        assert!(is_absolute_like("~"));
    }

    #[test]
    fn is_absolute_like_double_slash() {
        assert!(is_absolute_like("//network/share"));
    }

    #[test]
    #[cfg(unix)]
    fn is_absolute_like_absolute_path() {
        assert!(is_absolute_like("/usr/bin"));
    }

    #[test]
    fn is_absolute_like_relative_path() {
        assert!(!is_absolute_like("src/main.rs"));
        assert!(!is_absolute_like("./foo"));
    }

    // ── kind_rank ordering ──────────────────────────────────────────

    #[test]
    fn kind_rank_ordering() {
        assert!(
            kind_rank(AutocompleteItemKind::SlashCommand)
                < kind_rank(AutocompleteItemKind::ExtensionCommand)
        );
        assert!(
            kind_rank(AutocompleteItemKind::ExtensionCommand)
                < kind_rank(AutocompleteItemKind::PromptTemplate)
        );
        assert!(
            kind_rank(AutocompleteItemKind::PromptTemplate)
                < kind_rank(AutocompleteItemKind::Skill)
        );
        assert!(kind_rank(AutocompleteItemKind::Skill) < kind_rank(AutocompleteItemKind::Model));
        assert!(kind_rank(AutocompleteItemKind::Model) < kind_rank(AutocompleteItemKind::File));
        assert!(kind_rank(AutocompleteItemKind::File) < kind_rank(AutocompleteItemKind::Path));
    }

    // ── sort_scored_items ───────────────────────────────────────────

    #[test]
    fn sort_scored_items_prefix_first() {
        let mut items = vec![
            ScoredItem {
                is_prefix: false,
                score: 900,
                kind_rank: 0,
                label: "b".to_string(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::SlashCommand,
                    label: "b".to_string(),
                    insert: "b".to_string(),
                    description: None,
                },
            },
            ScoredItem {
                is_prefix: true,
                score: 100,
                kind_rank: 0,
                label: "a".to_string(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::SlashCommand,
                    label: "a".to_string(),
                    insert: "a".to_string(),
                    description: None,
                },
            },
        ];
        sort_scored_items(&mut items);
        // Prefix match comes first despite lower score
        assert_eq!(items[0].label, "a");
    }

    #[test]
    fn sort_scored_items_higher_score_first() {
        let mut items = vec![
            ScoredItem {
                is_prefix: true,
                score: 100,
                kind_rank: 0,
                label: "low".to_string(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::SlashCommand,
                    label: "low".to_string(),
                    insert: "low".to_string(),
                    description: None,
                },
            },
            ScoredItem {
                is_prefix: true,
                score: 900,
                kind_rank: 0,
                label: "high".to_string(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::SlashCommand,
                    label: "high".to_string(),
                    insert: "high".to_string(),
                    description: None,
                },
            },
        ];
        sort_scored_items(&mut items);
        assert_eq!(items[0].label, "high");
    }

    #[test]
    fn sort_scored_items_kind_rank_tiebreaker() {
        let mut items = vec![
            ScoredItem {
                is_prefix: true,
                score: 500,
                kind_rank: kind_rank(AutocompleteItemKind::PromptTemplate),
                label: "template".to_string(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::PromptTemplate,
                    label: "template".to_string(),
                    insert: "template".to_string(),
                    description: None,
                },
            },
            ScoredItem {
                is_prefix: true,
                score: 500,
                kind_rank: kind_rank(AutocompleteItemKind::SlashCommand),
                label: "command".to_string(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::SlashCommand,
                    label: "command".to_string(),
                    insert: "command".to_string(),
                    description: None,
                },
            },
        ];
        sort_scored_items(&mut items);
        // SlashCommand has lower kind_rank, so it comes first
        assert_eq!(items[0].label, "command");
    }

    #[test]
    fn sort_scored_items_label_tiebreaker() {
        let mut items = vec![
            ScoredItem {
                is_prefix: true,
                score: 500,
                kind_rank: 0,
                label: "zebra".to_string(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::SlashCommand,
                    label: "zebra".to_string(),
                    insert: "zebra".to_string(),
                    description: None,
                },
            },
            ScoredItem {
                is_prefix: true,
                score: 500,
                kind_rank: 0,
                label: "apple".to_string(),
                item: AutocompleteItem {
                    kind: AutocompleteItemKind::SlashCommand,
                    label: "apple".to_string(),
                    insert: "apple".to_string(),
                    description: None,
                },
            },
        ];
        sort_scored_items(&mut items);
        assert_eq!(items[0].label, "apple");
    }

    // ── clamp_usize_to_i32 ──────────────────────────────────────────

    #[test]
    fn clamp_usize_to_i32_within_range() {
        assert_eq!(clamp_usize_to_i32(0), 0);
        assert_eq!(clamp_usize_to_i32(42), 42);
        assert_eq!(clamp_usize_to_i32(i32::MAX as usize), i32::MAX);
    }

    #[test]
    fn clamp_usize_to_i32_overflow() {
        assert_eq!(clamp_usize_to_i32(usize::MAX), i32::MAX);
        assert_eq!(clamp_usize_to_i32(i32::MAX as usize + 1), i32::MAX);
    }

    // ── builtin_slash_commands ───────────────────────────────────────

    #[test]
    fn builtin_slash_commands_not_empty() {
        let cmds = builtin_slash_commands();
        assert!(!cmds.is_empty());
    }

    #[test]
    fn builtin_slash_commands_contains_help() {
        let cmds = builtin_slash_commands();
        assert!(cmds.iter().any(|c| c.name == "help"));
    }

    #[test]
    fn builtin_slash_commands_contains_exit() {
        let cmds = builtin_slash_commands();
        assert!(cmds.iter().any(|c| c.name == "exit"));
    }

    #[test]
    fn builtin_slash_commands_all_unique_names() {
        let cmds = builtin_slash_commands();
        let mut names: Vec<_> = cmds.iter().map(|c| c.name).collect();
        let orig_len = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), orig_len, "Duplicate slash command names found");
    }

    // ── set_max_items ────────────────────────────────────────────────

    #[test]
    fn set_max_items_clamps_to_one() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        provider.set_max_items(0);
        assert_eq!(provider.max_items(), 1);
    }

    #[test]
    fn set_max_items_accepts_large_value() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        provider.set_max_items(1000);
        assert_eq!(provider.max_items(), 1000);
    }

    // ── max_items limits output ──────────────────────────────────────

    #[test]
    fn suggest_respects_max_items() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        provider.set_max_items(3);
        // "/" with empty query returns all builtins, should be capped at 3
        let resp = provider.suggest("/", 1);
        assert!(resp.items.len() <= 3);
    }

    // ── suggest with non-matching input ──────────────────────────────

    #[test]
    fn suggest_plain_text_returns_empty() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let resp = provider.suggest("hello world", 5);
        assert!(resp.items.is_empty());
    }

    // ── suggest_slash with empty query ───────────────────────────────

    #[test]
    fn suggest_slash_alone_returns_all_builtins() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        let resp = provider.suggest("/", 1);
        let builtin_count = builtin_slash_commands().len();
        println!("Items: {:?}", resp.items);
        assert_eq!(resp.items.len(), builtin_count);
    }

    // ── set_cwd invalidates cache ────────────────────────────────────

    #[test]
    fn set_cwd_invalidates_file_cache() {
        let tmp1 = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp1.path().join("one.txt"), "1").expect("write");

        let tmp2 = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp2.path().join("two.txt"), "2").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp1.path().to_path_buf(), AutocompleteCatalog::default());
        // Pre-populate cache (refresh_if_needed is async).
        provider.file_cache.files = walk_project_files(tmp1.path());
        let resp = provider.suggest("@on", 3);
        assert!(resp.items.iter().any(|i| i.insert == "@one.txt"));

        provider.set_cwd(tmp2.path().to_path_buf());
        // Re-populate after cwd change (invalidate clears the cache).
        provider.file_cache.files = walk_project_files(tmp2.path());
        let resp = provider.suggest("@tw", 3);
        assert!(resp.items.iter().any(|i| i.insert == "@two.txt"));
        // Old file should no longer appear
        let resp = provider.suggest("@on", 3);
        assert!(!resp.items.iter().any(|i| i.insert == "@one.txt"));
    }

    // ── walk_project_files ──────────────────────────────────────────

    #[test]
    fn walk_project_files_returns_sorted_deduped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("sub")).expect("mkdir");
        std::fs::write(tmp.path().join("b.txt"), "b").expect("write");
        std::fs::write(tmp.path().join("a.txt"), "a").expect("write");
        std::fs::write(tmp.path().join("sub/c.txt"), "c").expect("write");

        let files = walk_project_files(tmp.path());
        assert!(files.contains(&"a.txt".to_string()));
        assert!(files.contains(&"b.txt".to_string()));
        assert!(files.contains(&"sub/c.txt".to_string()));
        // Verify sorted
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
    }

    #[test]
    fn walk_project_files_empty_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let files = walk_project_files(tmp.path());
        assert!(files.is_empty());
    }

    // ── resolve_file_ref ─────────────────────────────────────────────

    #[test]
    #[cfg(unix)]
    fn resolve_file_ref_absolute_returns_normalized() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("/tmp"), AutocompleteCatalog::default());
        let result = provider.resolve_file_ref("/some/absolute/path.txt");
        assert_eq!(result, Some("/some/absolute/path.txt".to_string()));
    }

    #[test]
    fn resolve_file_ref_tilde_returns_normalized() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("/tmp"), AutocompleteCatalog::default());
        let result = provider.resolve_file_ref("~/notes.txt");
        assert_eq!(result, Some("~/notes.txt".to_string()));
    }

    #[test]
    fn resolve_file_ref_empty_returns_none() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("/tmp"), AutocompleteCatalog::default());
        assert!(provider.resolve_file_ref("").is_none());
        assert!(provider.resolve_file_ref("   ").is_none());
    }

    #[test]
    fn resolve_file_ref_matches_project_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("README.md"), "hi").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());
        // Pre-populate cache (refresh_if_needed is async).
        provider.file_cache.files = walk_project_files(tmp.path());
        let result = provider.resolve_file_ref("README.md");
        assert_eq!(result, Some("README.md".to_string()));
    }

    #[test]
    fn resolve_file_ref_nonexistent_file_returns_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());
        assert!(provider.resolve_file_ref("nonexistent.txt").is_none());
    }

    #[test]
    fn resolve_file_ref_strips_dot_slash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("foo.txt"), "hi").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());
        // Pre-populate cache (refresh_if_needed is async).
        provider.file_cache.files = walk_project_files(tmp.path());
        let result = provider.resolve_file_ref("./foo.txt");
        assert_eq!(result, Some("foo.txt".to_string()));
    }

    // ── AutocompleteCatalog ──────────────────────────────────────────

    #[test]
    fn autocomplete_catalog_default_is_empty() {
        let catalog = AutocompleteCatalog::default();
        assert!(catalog.prompt_templates.is_empty());
        assert!(catalog.skills.is_empty());
        assert!(catalog.extension_commands.is_empty());
        assert!(!catalog.enable_skill_commands);
    }

    // ── AutocompleteResponse empty ──────────────────────────────────

    #[test]
    fn suggest_cursor_past_end_clamps() {
        let mut provider =
            AutocompleteProvider::new(PathBuf::from("."), AutocompleteCatalog::default());
        // Cursor way past end of text
        let resp = provider.suggest("/he", 1000);
        // Should still find /help since cursor clamps to end
        assert!(resp.items.iter().any(|i| i.insert == "/help"));
    }

    // ── Mixed catalog with slash query ───────────────────────────────

    #[test]
    fn slash_suggests_mixed_sources_sorted_by_kind() {
        let catalog = AutocompleteCatalog {
            prompt_templates: vec![NamedEntry {
                name: "test-prompt".to_string(),
                description: Some("A test".to_string()),
            }],
            skills: Vec::new(),
            extension_commands: vec![NamedEntry {
                name: "test-ext".to_string(),
                description: Some("An extension".to_string()),
            }],
            enable_skill_commands: false,
        };
        let mut provider = AutocompleteProvider::new(PathBuf::from("."), catalog);
        let resp = provider.suggest("/test", 5);

        assert!(
            resp.items
                .iter()
                .any(|i| i.kind == AutocompleteItemKind::ExtensionCommand)
        );
        assert!(
            resp.items
                .iter()
                .any(|i| i.kind == AutocompleteItemKind::PromptTemplate)
        );
    }

    // ── skill commands disabled returns empty ────────────────────────

    #[test]
    fn skill_query_disabled_returns_empty() {
        let catalog = AutocompleteCatalog {
            prompt_templates: Vec::new(),
            skills: vec![NamedEntry {
                name: "deploy".to_string(),
                description: None,
            }],
            extension_commands: Vec::new(),
            enable_skill_commands: false,
        };
        let mut provider = AutocompleteProvider::new(PathBuf::from("."), catalog);
        let resp = provider.suggest("/skill:de", "/skill:de".len());
        assert!(resp.items.is_empty());
    }

    // ── file ref suggest with @ ──────────────────────────────────────

    #[test]
    fn file_ref_suggest_empty_query_returns_all_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.txt"), "a").expect("write");
        std::fs::write(tmp.path().join("b.txt"), "b").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());
        // Pre-populate the file cache (refresh_if_needed is async).
        provider.file_cache.files = walk_project_files(tmp.path());
        // Just "@" with no query
        let resp = provider.suggest("@", 1);
        assert!(resp.items.len() >= 2);
    }

    // ── path completion with tilde override ──────────────────────────

    #[test]
    fn path_completion_with_home_override() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mock_home = tmp.path().join("home");
        std::fs::create_dir_all(&mock_home).expect("mkdir");
        std::fs::write(mock_home.join("notes.txt"), "hi").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());
        provider.home_dir_override = Some(mock_home);

        let resp = provider.suggest("~/no", 4);
        assert!(resp.items.iter().any(|i| i.insert.contains("notes.txt")));
    }

    #[test]
    fn path_completion_windows_relative_input_preserves_backslashes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src").join("main.rs"), "hi").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());

        let resp = provider.suggest("src\\ma", "src\\ma".len());
        assert!(resp.items.iter().any(|i| i.insert == "src\\main.rs"));
    }

    #[test]
    fn path_completion_windows_dot_prefix_preserves_backslashes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("main.rs"), "hi").expect("write");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());

        let resp = provider.suggest(".\\ma", ".\\ma".len());
        assert!(resp.items.iter().any(|i| i.insert == ".\\main.rs"));
    }

    #[test]
    fn path_completion_windows_directory_input_preserves_backslashes_for_directories() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("src").join("nested")).expect("mkdir");

        let mut provider =
            AutocompleteProvider::new(tmp.path().to_path_buf(), AutocompleteCatalog::default());

        let resp = provider.suggest("src\\n", "src\\n".len());
        assert!(resp.items.iter().any(|i| i.insert == "src\\nested\\"));
        assert!(!resp.items.iter().any(|i| i.insert == "src\\nested/"));
    }

    // ── FileCache invalidation ───────────────────────────────────────

    #[test]
    fn file_cache_invalidate_clears_files() {
        let mut cache = FileCache::new();
        cache.files = vec!["a.txt".to_string()];
        cache.last_update_request = Some(Instant::now());
        let (_tx, rx) = std::sync::mpsc::channel();
        cache.update_rx = Some(rx);
        cache.updating = true;

        cache.invalidate();
        assert!(cache.files.is_empty());
        assert!(cache.last_update_request.is_none());
        assert!(cache.update_rx.is_none());
        assert!(!cache.updating);
    }

    // ── NamedEntry equality ──────────────────────────────────────────

    #[test]
    fn named_entry_equality() {
        let a = NamedEntry {
            name: "test".to_string(),
            description: Some("desc".to_string()),
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    mod proptest_autocomplete {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `clamp_usize_to_i32` saturates at `i32::MAX`.
            #[test]
            fn clamp_usize_saturates(val in 0..usize::MAX) {
                let result = clamp_usize_to_i32(val);
                let expected = i32::try_from(val).unwrap_or(i32::MAX);
                assert_eq!(result, expected);
            }

            /// `clamp_to_char_boundary` always returns a valid char boundary.
            #[test]
            fn clamp_to_char_boundary_valid(s in "\\PC{1,30}", idx in 0..60usize) {
                let clamped = clamp_to_char_boundary(&s, idx);
                assert!(s.is_char_boundary(clamped));
                assert!(clamped <= s.len());
            }

            /// `clamp_cursor` always returns a valid char boundary <= len.
            #[test]
            fn clamp_cursor_valid(s in "\\PC{1,30}", cursor in 0..100usize) {
                let clamped = clamp_cursor(&s, cursor);
                assert!(s.is_char_boundary(clamped));
                assert!(clamped <= s.len());
            }

            /// `fuzzy_match_score` with empty query always returns `Some((true, 0))`.
            #[test]
            fn fuzzy_empty_query_matches_all(cand in "[a-z]{1,20}") {
                assert_eq!(fuzzy_match_score(&cand, ""), Some((true, 0)));
                assert_eq!(fuzzy_match_score(&cand, "  "), Some((true, 0)));
            }

            /// Prefix matches report `is_prefix=true` and score >= 900.
            #[test]
            fn fuzzy_prefix_match(base in "[a-z]{2,10}", suffix in "[a-z]{0,5}") {
                let candidate = format!("{base}{suffix}");
                let result = fuzzy_match_score(&candidate, &base);
                assert!(result.is_some());
                let (is_prefix, score) = result.unwrap();
                assert!(is_prefix, "prefix match should be flagged");
                assert!(score >= 900, "prefix score should be high, got {score}");
            }

            /// `fuzzy_match_score` is case-insensitive.
            #[test]
            fn fuzzy_case_insensitive(cand in "[a-z]{2,10}", query in "[a-z]{1,5}") {
                let lower = fuzzy_match_score(&cand, &query);
                let upper = fuzzy_match_score(&cand, &query.to_uppercase());
                assert_eq!(lower.is_some(), upper.is_some());
                if let (Some((lp, ls)), Some((up, us))) = (lower, upper) {
                    assert_eq!(lp, up);
                    assert_eq!(ls, us);
                }
            }

            /// `is_path_like` recognizes paths starting with ./ ../ ~/ or /
            #[test]
            fn is_path_like_common_prefixes(name in "[a-z]{1,10}") {
                assert!(is_path_like(&format!("./{name}")));
                assert!(is_path_like(&format!("../{name}")));
                assert!(is_path_like(&format!("~/{name}")));
                assert!(is_path_like(&format!("/{name}")));
            }

            /// `is_path_like` returns false for simple words without slashes.
            #[test]
            fn is_path_like_false_for_words(word in "[a-z]{1,10}") {
                assert!(!is_path_like(word.trim()));
            }

            /// `is_path_like` empty or whitespace returns false.
            #[test]
            fn is_path_like_empty_false(ws in "[ \\t]{0,5}") {
                assert!(!is_path_like(&ws));
            }

            /// `split_path_prefix` reconstructs the original path.
            #[test]
            fn split_path_prefix_reconstructs(dir in "[a-z]{1,5}", file in "[a-z]{1,5}") {
                let path = format!("{dir}/{file}");
                let (d, f) = split_path_prefix(&path);
                assert_eq!(d, dir);
                assert_eq!(f, file);
            }

            /// `split_path_prefix("~")` returns `("~", "")`.
            #[test]
            fn split_path_prefix_tilde(_dummy in 0..1u8) {
                let (d, f) = split_path_prefix("~");
                assert_eq!(d, "~");
                assert!(f.is_empty());
            }

            /// `split_path_prefix` with trailing slash returns dir=path, file="".
            #[test]
            fn split_path_prefix_trailing_slash(dir in "[a-z]{1,10}") {
                let path = format!("{dir}/");
                let (d, f) = split_path_prefix(&path);
                assert_eq!(d, path);
                assert!(f.is_empty());
            }

            /// `split_path_prefix` with no slash returns dir=".", file=path.
            #[test]
            fn split_path_prefix_no_slash(word in "[a-z]{1,10}") {
                let (d, f) = split_path_prefix(&word);
                assert_eq!(d, ".");
                assert_eq!(f, word);
            }

            /// `token_at_cursor` result range is within text bounds.
            #[test]
            fn token_at_cursor_bounds(text in "[a-z ]{1,30}", cursor in 0..40usize) {
                let tok = token_at_cursor(&text, cursor);
                assert!(tok.range.start <= tok.range.end);
                assert!(tok.range.end <= text.len());
                assert_eq!(&text[tok.range.clone()], tok.text);
            }

            /// `token_at_cursor` result text contains no whitespace.
            #[test]
            fn token_at_cursor_no_whitespace(text in "[a-z ]{1,20}", cursor in 0..30usize) {
                let tok = token_at_cursor(&text, cursor);
                assert!(!tok.text.contains(char::is_whitespace) || tok.text.is_empty());
            }

            /// `kind_rank` covers all variants with distinct ranks 0..=6.
            #[test]
            fn kind_rank_distinct(idx in 0..7usize) {
                let kinds = [
                    AutocompleteItemKind::SlashCommand,
                    AutocompleteItemKind::ExtensionCommand,
                    AutocompleteItemKind::PromptTemplate,
                    AutocompleteItemKind::Skill,
                    AutocompleteItemKind::Model,
                    AutocompleteItemKind::File,
                    AutocompleteItemKind::Path,
                ];
                let expected = [0_u8, 1, 2, 3, 4, 5, 6][idx];
                assert_eq!(kind_rank(kinds[idx]), expected);
            }

            /// `resolve_dir_path` with absolute path returns it unchanged.
            #[test]
            fn resolve_dir_absolute(dir in "[a-z]{1,10}") {
                let abs = format!("/{dir}");
                let result = resolve_dir_path(Path::new("/cwd"), &abs, None);
                assert_eq!(result, Some(PathBuf::from(&abs)));
            }

            /// `resolve_dir_path` with relative path joins to cwd.
            #[test]
            fn resolve_dir_relative(dir in "[a-z]{1,10}") {
                let result = resolve_dir_path(Path::new("/cwd"), &dir, None);
                assert_eq!(result, Some(PathBuf::from(format!("/cwd/{dir}"))));
            }
        }
    }
}
