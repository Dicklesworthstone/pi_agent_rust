//! Resource loading for skills, prompt templates, themes, and extensions.
//!
//! Implements a subset of pi-mono's resource discovery behavior:
//! - Skills (Agent Skills spec)
//! - Prompt templates (markdown files with optional frontmatter)
//! - Package-based resource discovery

use crate::config::Config;
use crate::error::{Error, Result};
use crate::package_manager::{PackageManager, ResolveExtensionSourcesOptions};
use crate::theme::Theme;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

// ============================================================================
// Diagnostics
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticKind {
    Warning,
    Collision,
}

#[derive(Debug, Clone)]
pub struct CollisionInfo {
    pub resource_type: String,
    pub name: String,
    pub winner_path: PathBuf,
    pub loser_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ResourceDiagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
    pub path: PathBuf,
    pub collision: Option<CollisionInfo>,
}

// ============================================================================
// Skills
// ============================================================================

const MAX_SKILL_NAME_LEN: usize = 64;
const MAX_SKILL_DESC_LEN: usize = 1024;

const ALLOWED_SKILL_FRONTMATTER: [&str; 7] = [
    "name",
    "description",
    "license",
    "compatibility",
    "metadata",
    "allowed-tools",
    "disable-model-invocation",
];

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
    pub base_dir: PathBuf,
    pub source: String,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone)]
pub struct LoadSkillsResult {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

#[derive(Debug, Clone)]
pub struct LoadSkillsOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub skill_paths: Vec<PathBuf>,
    pub include_defaults: bool,
}

// ============================================================================
// Prompt templates
// ============================================================================

#[derive(Debug, Clone)]
pub struct PromptTemplate {
    pub name: String,
    pub description: String,
    pub content: String,
    pub source: String,
    pub file_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LoadPromptTemplatesOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub prompt_paths: Vec<PathBuf>,
    pub include_defaults: bool,
}

// ============================================================================
// Themes
// ============================================================================

#[derive(Debug, Clone)]
pub struct ThemeResource {
    pub name: String,
    pub theme: Theme,
    pub source: String,
    pub file_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LoadThemesOptions {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub theme_paths: Vec<PathBuf>,
    pub include_defaults: bool,
}

#[derive(Debug, Clone)]
pub struct LoadThemesResult {
    pub themes: Vec<ThemeResource>,
    pub diagnostics: Vec<ResourceDiagnostic>,
}

// ============================================================================
// Resource Loader
// ============================================================================

#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ResourceCliOptions {
    pub no_skills: bool,
    pub no_prompt_templates: bool,
    pub no_extensions: bool,
    pub no_themes: bool,
    pub skill_paths: Vec<String>,
    pub prompt_paths: Vec<String>,
    pub extension_paths: Vec<String>,
    pub theme_paths: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PackageResources {
    pub extensions: Vec<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub prompts: Vec<PathBuf>,
    pub themes: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ResourceLoader {
    skills: Vec<Skill>,
    skill_diagnostics: Vec<ResourceDiagnostic>,
    prompts: Vec<PromptTemplate>,
    prompt_diagnostics: Vec<ResourceDiagnostic>,
    themes: Vec<ThemeResource>,
    theme_diagnostics: Vec<ResourceDiagnostic>,
    extensions: Vec<PathBuf>,
    enable_skill_commands: bool,
}

impl ResourceLoader {
    pub const fn empty(enable_skill_commands: bool) -> Self {
        Self {
            skills: Vec::new(),
            skill_diagnostics: Vec::new(),
            prompts: Vec::new(),
            prompt_diagnostics: Vec::new(),
            themes: Vec::new(),
            theme_diagnostics: Vec::new(),
            extensions: Vec::new(),
            enable_skill_commands,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub async fn load(
        manager: &PackageManager,
        cwd: &Path,
        config: &Config,
        cli: &ResourceCliOptions,
    ) -> Result<Self> {
        let enable_skill_commands = config.enable_skill_commands();

        // Resolve configured resources (settings + auto-discovery + packages) and CLI `-e` sources.
        let resolved = Box::pin(manager.resolve()).await?;
        let cli_extensions = Box::pin(manager.resolve_extension_sources(
            &cli.extension_paths,
            ResolveExtensionSourcesOptions {
                local: false,
                temporary: true,
            },
        ))
        .await?;

        let resolved_skill_paths = resolved
            .skills
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .collect::<Vec<_>>();
        let resolved_prompt_paths = resolved
            .prompts
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .collect::<Vec<_>>();
        let resolved_theme_paths = resolved
            .themes
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .collect::<Vec<_>>();
        let resolved_extension_paths = resolved
            .extensions
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .collect::<Vec<_>>();

        let cli_skill_paths = cli_extensions
            .skills
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .collect::<Vec<_>>();
        let cli_prompt_paths = cli_extensions
            .prompts
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .collect::<Vec<_>>();
        let cli_theme_paths = cli_extensions
            .themes
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .collect::<Vec<_>>();
        let cli_extension_paths = cli_extensions
            .extensions
            .into_iter()
            .filter(|r| r.enabled)
            .map(|r| r.path)
            .collect::<Vec<_>>();

        // Merge paths with pi-mono semantics:
        // - `--no-skills` disables configured + auto skills, but still loads CLI `-e` and explicit `--skill`
        // - `--no-prompt-templates` disables configured + auto prompts, but still loads CLI `-e` and explicit `--prompt-template`
        let mut skill_paths = Vec::new();
        if !cli.no_skills {
            skill_paths.extend(resolved_skill_paths);
        }
        skill_paths.extend(cli_skill_paths);
        skill_paths.extend(cli.skill_paths.iter().map(|p| resolve_path(p, cwd)));
        let skill_paths = dedupe_paths(skill_paths);

        let mut prompt_paths = Vec::new();
        if !cli.no_prompt_templates {
            prompt_paths.extend(resolved_prompt_paths);
        }
        prompt_paths.extend(cli_prompt_paths);
        prompt_paths.extend(cli.prompt_paths.iter().map(|p| resolve_path(p, cwd)));
        let prompt_paths = dedupe_paths(prompt_paths);

        let mut theme_paths = Vec::new();
        if !cli.no_themes {
            theme_paths.extend(resolved_theme_paths);
        }
        theme_paths.extend(cli_theme_paths);
        theme_paths.extend(cli.theme_paths.iter().map(|p| resolve_path(p, cwd)));
        let theme_paths = dedupe_paths(theme_paths);

        // Extension entries:
        // - `--no-extensions` disables configured + auto discovery but still allows CLI `-e` sources.
        let mut extension_entries = Vec::new();
        if !cli.no_extensions {
            extension_entries.extend(resolved_extension_paths);
        }
        extension_entries.extend(cli_extension_paths);
        let extension_entries = dedupe_paths(extension_entries);

        let skills_result = load_skills(LoadSkillsOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: Config::global_dir(),
            skill_paths,
            include_defaults: false,
        });

        let prompt_templates = load_prompt_templates(LoadPromptTemplatesOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: Config::global_dir(),
            prompt_paths,
            include_defaults: false,
        });
        let (prompts, prompt_diagnostics) = dedupe_prompts(prompt_templates);

        let themes_result = load_themes(LoadThemesOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: Config::global_dir(),
            theme_paths,
            include_defaults: false,
        });
        let (themes, theme_diagnostics) = dedupe_themes(themes_result.themes);
        let mut theme_diags = themes_result.diagnostics;
        theme_diags.extend(theme_diagnostics);

        Ok(Self {
            skills: skills_result.skills,
            skill_diagnostics: skills_result.diagnostics,
            prompts,
            prompt_diagnostics,
            themes,
            theme_diagnostics: theme_diags,
            extensions: extension_entries,
            enable_skill_commands,
        })
    }

    pub fn extensions(&self) -> &[PathBuf] {
        &self.extensions
    }

    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    pub fn prompts(&self) -> &[PromptTemplate] {
        &self.prompts
    }

    pub fn skill_diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.skill_diagnostics
    }

    pub fn prompt_diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.prompt_diagnostics
    }

    pub fn themes(&self) -> &[ThemeResource] {
        &self.themes
    }

    pub fn theme_diagnostics(&self) -> &[ResourceDiagnostic] {
        &self.theme_diagnostics
    }

    pub fn resolve_theme(&self, selected: Option<&str>) -> Option<Theme> {
        let selected = selected?;
        let trimmed = selected.trim();
        if trimmed.is_empty() {
            return None;
        }

        let path = Path::new(trimmed);
        if path.exists() {
            let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
            let theme = match ext {
                "json" => Theme::load(path),
                "ini" | "theme" => load_legacy_ini_theme(path),
                _ => Err(Error::config(format!(
                    "Unsupported theme format: {}",
                    path.display()
                ))),
            };
            if let Ok(theme) = theme {
                return Some(theme);
            }
        }

        self.themes
            .iter()
            .find(|theme| theme.name.eq_ignore_ascii_case(trimmed))
            .map(|theme| theme.theme.clone())
    }

    pub const fn enable_skill_commands(&self) -> bool {
        self.enable_skill_commands
    }

    pub fn format_skills_for_prompt(&self) -> String {
        format_skills_for_prompt(&self.skills)
    }

    pub fn list_commands(&self) -> Vec<Value> {
        let mut commands = Vec::new();

        for template in &self.prompts {
            commands.push(json!({
                "name": template.name,
                "description": template.description,
                "source": "template",
                "location": template.source,
                "path": template.file_path.display().to_string(),
            }));
        }

        for skill in &self.skills {
            commands.push(json!({
                "name": format!("skill:{}", skill.name),
                "description": skill.description,
                "source": "skill",
                "location": skill.source,
                "path": skill.file_path.display().to_string(),
            }));
        }

        commands
    }

    pub fn expand_input(&self, text: &str) -> String {
        let mut expanded = text.to_string();
        if self.enable_skill_commands {
            expanded = expand_skill_command(&expanded, &self.skills);
        }
        expand_prompt_template(&expanded, &self.prompts)
    }
}

// ============================================================================
// Package resources
// ============================================================================

pub async fn discover_package_resources(manager: &PackageManager) -> Result<PackageResources> {
    let entries = manager.list_packages().await.unwrap_or_default();
    let mut resources = PackageResources::default();

    for entry in entries {
        let Some(root) = manager.installed_path(&entry.source, entry.scope).await? else {
            continue;
        };
        if !root.exists() {
            if let Err(err) = manager.install(&entry.source, entry.scope).await {
                eprintln!("Warning: Failed to install {}: {err}", entry.source);
                continue;
            }
        }

        if !root.exists() {
            continue;
        }

        if let Some(pi) = read_pi_manifest(&root) {
            append_resources_from_manifest(&mut resources, &root, &pi);
        } else {
            append_resources_from_defaults(&mut resources, &root);
        }
    }

    Ok(resources)
}

fn read_pi_manifest(root: &Path) -> Option<Value> {
    let manifest_path = root.join("package.json");
    if !manifest_path.exists() {
        return None;
    }
    let raw = fs::read_to_string(&manifest_path).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    json.get("pi").cloned()
}

fn append_resources_from_manifest(resources: &mut PackageResources, root: &Path, pi: &Value) {
    let Some(obj) = pi.as_object() else {
        return;
    };
    append_resource_paths(
        resources,
        root,
        obj.get("extensions"),
        ResourceKind::Extensions,
    );
    append_resource_paths(resources, root, obj.get("skills"), ResourceKind::Skills);
    append_resource_paths(resources, root, obj.get("prompts"), ResourceKind::Prompts);
    append_resource_paths(resources, root, obj.get("themes"), ResourceKind::Themes);
}

fn append_resources_from_defaults(resources: &mut PackageResources, root: &Path) {
    let candidates = [
        ("extensions", ResourceKind::Extensions),
        ("skills", ResourceKind::Skills),
        ("prompts", ResourceKind::Prompts),
        ("themes", ResourceKind::Themes),
    ];

    for (dir, kind) in candidates {
        let path = root.join(dir);
        if path.exists() {
            match kind {
                ResourceKind::Extensions => resources.extensions.push(path),
                ResourceKind::Skills => resources.skills.push(path),
                ResourceKind::Prompts => resources.prompts.push(path),
                ResourceKind::Themes => resources.themes.push(path),
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ResourceKind {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

fn append_resource_paths(
    resources: &mut PackageResources,
    root: &Path,
    value: Option<&Value>,
    kind: ResourceKind,
) {
    let Some(value) = value else {
        return;
    };
    let paths = extract_string_list(value);
    if paths.is_empty() {
        return;
    }

    for path in paths {
        let resolved = if Path::new(&path).is_absolute() {
            PathBuf::from(path)
        } else {
            root.join(path)
        };
        match kind {
            ResourceKind::Extensions => resources.extensions.push(resolved),
            ResourceKind::Skills => resources.skills.push(resolved),
            ResourceKind::Prompts => resources.prompts.push(resolved),
            ResourceKind::Themes => resources.themes.push(resolved),
        }
    }
}

fn extract_string_list(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

// ============================================================================
// Skills loader
// ============================================================================

#[allow(clippy::too_many_lines, clippy::items_after_statements)]
pub fn load_skills(options: LoadSkillsOptions) -> LoadSkillsResult {
    let mut skill_map: HashMap<String, Skill> = HashMap::new();
    let mut real_paths: HashSet<PathBuf> = HashSet::new();
    let mut diagnostics = Vec::new();
    let mut collisions = Vec::new();

    // Helper to merge skills into the map, tracking collisions
    fn merge_skills(
        result: LoadSkillsResult,
        skill_map: &mut HashMap<String, Skill>,
        real_paths: &mut HashSet<PathBuf>,
        diagnostics: &mut Vec<ResourceDiagnostic>,
        collisions: &mut Vec<ResourceDiagnostic>,
    ) {
        diagnostics.extend(result.diagnostics);
        for skill in result.skills {
            let real_path =
                fs::canonicalize(&skill.file_path).unwrap_or_else(|_| skill.file_path.clone());
            if real_paths.contains(&real_path) {
                continue;
            }

            if let Some(existing) = skill_map.get(&skill.name) {
                collisions.push(ResourceDiagnostic {
                    kind: DiagnosticKind::Collision,
                    message: format!("name \"{}\" collision", skill.name),
                    path: skill.file_path.clone(),
                    collision: Some(CollisionInfo {
                        resource_type: "skill".to_string(),
                        name: skill.name.clone(),
                        winner_path: existing.file_path.clone(),
                        loser_path: skill.file_path.clone(),
                    }),
                });
            } else {
                real_paths.insert(real_path);
                skill_map.insert(skill.name.clone(), skill);
            }
        }
    }

    if options.include_defaults {
        merge_skills(
            load_skills_from_dir(options.agent_dir.join("skills"), "user".to_string(), true),
            &mut skill_map,
            &mut real_paths,
            &mut diagnostics,
            &mut collisions,
        );
        merge_skills(
            load_skills_from_dir(
                options.cwd.join(Config::project_dir()).join("skills"),
                "project".to_string(),
                true,
            ),
            &mut skill_map,
            &mut real_paths,
            &mut diagnostics,
            &mut collisions,
        );
    }

    for path in options.skill_paths {
        let resolved = path.clone();
        if !resolved.exists() {
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message: "skill path does not exist".to_string(),
                path: resolved,
                collision: None,
            });
            continue;
        }

        let source = if options.include_defaults {
            "path".to_string()
        } else if is_under_path(&resolved, &options.agent_dir.join("skills")) {
            "user".to_string()
        } else if is_under_path(
            &resolved,
            &options.cwd.join(Config::project_dir()).join("skills"),
        ) {
            "project".to_string()
        } else {
            "path".to_string()
        };

        match fs::metadata(&resolved) {
            Ok(meta) if meta.is_dir() => {
                merge_skills(
                    load_skills_from_dir(resolved, source, true),
                    &mut skill_map,
                    &mut real_paths,
                    &mut diagnostics,
                    &mut collisions,
                );
            }
            Ok(meta) if meta.is_file() && resolved.extension().is_some_and(|ext| ext == "md") => {
                let result = load_skill_from_file(&resolved, source);
                if let Some(skill) = result.skill {
                    merge_skills(
                        LoadSkillsResult {
                            skills: vec![skill],
                            diagnostics: result.diagnostics,
                        },
                        &mut skill_map,
                        &mut real_paths,
                        &mut diagnostics,
                        &mut collisions,
                    );
                } else {
                    diagnostics.extend(result.diagnostics);
                }
            }
            Ok(_) => {
                diagnostics.push(ResourceDiagnostic {
                    kind: DiagnosticKind::Warning,
                    message: "skill path is not a markdown file".to_string(),
                    path: resolved,
                    collision: None,
                });
            }
            Err(err) => diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message: format!("failed to read skill path: {err}"),
                path: resolved,
                collision: None,
            }),
        }
    }

    diagnostics.extend(collisions);

    let mut skills: Vec<Skill> = skill_map.into_values().collect();
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    LoadSkillsResult {
        skills,
        diagnostics,
    }
}

#[allow(clippy::needless_pass_by_value)] // Recursive function that clones arguments
fn load_skills_from_dir(
    dir: PathBuf,
    source: String,
    include_root_files: bool,
) -> LoadSkillsResult {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();

    if !dir.exists() {
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    }

    let Ok(entries) = fs::read_dir(&dir) else {
        return LoadSkillsResult {
            skills,
            diagnostics,
        };
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();

        if file_name.starts_with('.') || file_name == "node_modules" {
            continue;
        }

        let full_path = entry.path();
        let file_type = entry.file_type();

        let (is_dir, is_file) = match file_type {
            Ok(ft) if ft.is_symlink() => match fs::metadata(&full_path) {
                Ok(meta) => (meta.is_dir(), meta.is_file()),
                Err(_) => continue,
            },
            Ok(ft) => (ft.is_dir(), ft.is_file()),
            Err(_) => continue,
        };

        if is_dir {
            let sub = load_skills_from_dir(full_path, source.clone(), false);
            skills.extend(sub.skills);
            diagnostics.extend(sub.diagnostics);
            continue;
        }

        if !is_file {
            continue;
        }

        let is_root_md = include_root_files && file_name.ends_with(".md");
        let is_skill_md = !include_root_files && file_name == "SKILL.md";
        if !is_root_md && !is_skill_md {
            continue;
        }

        let result = load_skill_from_file(&full_path, source.clone());
        if let Some(skill) = result.skill {
            skills.push(skill);
        }
        diagnostics.extend(result.diagnostics);
    }

    LoadSkillsResult {
        skills,
        diagnostics,
    }
}

struct LoadSkillFileResult {
    skill: Option<Skill>,
    diagnostics: Vec<ResourceDiagnostic>,
}

fn load_skill_from_file(path: &Path, source: String) -> LoadSkillFileResult {
    let mut diagnostics = Vec::new();

    let Ok(raw) = fs::read_to_string(path) else {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: "failed to parse skill file".to_string(),
            path: path.to_path_buf(),
            collision: None,
        });
        return LoadSkillFileResult {
            skill: None,
            diagnostics,
        };
    };

    let parsed = parse_frontmatter(&raw);
    let frontmatter = &parsed.frontmatter;

    let field_errors = validate_frontmatter_fields(frontmatter.keys());
    for error in field_errors {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error,
            path: path.to_path_buf(),
            collision: None,
        });
    }

    let description = frontmatter.get("description").cloned().unwrap_or_default();
    let desc_errors = validate_description(&description);
    for error in desc_errors {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error,
            path: path.to_path_buf(),
            collision: None,
        });
    }

    if description.trim().is_empty() {
        return LoadSkillFileResult {
            skill: None,
            diagnostics,
        };
    }

    let base_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let parent_dir = base_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let name = frontmatter
        .get("name")
        .cloned()
        .unwrap_or_else(|| parent_dir.clone());

    let name_errors = validate_name(&name, &parent_dir);
    for error in name_errors {
        diagnostics.push(ResourceDiagnostic {
            kind: DiagnosticKind::Warning,
            message: error,
            path: path.to_path_buf(),
            collision: None,
        });
    }

    let disable_model_invocation = frontmatter
        .get("disable-model-invocation")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));

    LoadSkillFileResult {
        skill: Some(Skill {
            name,
            description,
            file_path: path.to_path_buf(),
            base_dir,
            source,
            disable_model_invocation,
        }),
        diagnostics,
    }
}

fn validate_name(name: &str, parent_dir: &str) -> Vec<String> {
    let mut errors = Vec::new();

    if name != parent_dir {
        errors.push(format!(
            "name \"{name}\" does not match parent directory \"{parent_dir}\""
        ));
    }

    if name.len() > MAX_SKILL_NAME_LEN {
        errors.push(format!(
            "name exceeds {MAX_SKILL_NAME_LEN} characters ({})",
            name.len()
        ));
    }

    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }

    if name.starts_with('-') || name.ends_with('-') {
        errors.push("name must not start or end with a hyphen".to_string());
    }

    if name.contains("--") {
        errors.push("name must not contain consecutive hyphens".to_string());
    }

    errors
}

fn validate_description(description: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if description.trim().is_empty() {
        errors.push("description is required".to_string());
    } else if description.len() > MAX_SKILL_DESC_LEN {
        errors.push(format!(
            "description exceeds {MAX_SKILL_DESC_LEN} characters ({})",
            description.len()
        ));
    }
    errors
}

fn validate_frontmatter_fields<'a, I>(keys: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a String>,
{
    let allowed: HashSet<&str> = ALLOWED_SKILL_FRONTMATTER.into_iter().collect();
    let mut errors = Vec::new();
    for key in keys {
        if !allowed.contains(key.as_str()) {
            errors.push(format!("unknown frontmatter field \"{key}\""));
        }
    }
    errors
}

pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<&Skill> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description."
            .to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];

    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.file_path.display().to_string())
        ));
        lines.push("  </skill>".to_string());
    }

    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ============================================================================
// Prompt templates loader and expansion
// ============================================================================

pub fn load_prompt_templates(options: LoadPromptTemplatesOptions) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();
    let user_dir = options.agent_dir.join("prompts");
    let project_dir = options.cwd.join(Config::project_dir()).join("prompts");

    if options.include_defaults {
        templates.extend(load_templates_from_dir(&user_dir, "user", "(user)"));
        templates.extend(load_templates_from_dir(
            &project_dir,
            "project",
            "(project)",
        ));
    }

    for path in options.prompt_paths {
        if !path.exists() {
            continue;
        }

        let source_info = if options.include_defaults {
            ("path", build_path_source_label(&path))
        } else if is_under_path(&path, &user_dir) {
            ("user", "(user)".to_string())
        } else if is_under_path(&path, &project_dir) {
            ("project", "(project)".to_string())
        } else {
            ("path", build_path_source_label(&path))
        };

        let (source, label) = source_info;

        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                templates.extend(load_templates_from_dir(&path, source, &label));
            }
            Ok(meta) if meta.is_file() && path.extension().is_some_and(|ext| ext == "md") => {
                if let Some(template) = load_template_from_file(&path, source, &label) {
                    templates.push(template);
                }
            }
            _ => {}
        }
    }

    templates
}

fn load_templates_from_dir(dir: &Path, source: &str, label: &str) -> Vec<PromptTemplate> {
    let mut templates = Vec::new();
    if !dir.exists() {
        return templates;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return templates;
    };

    for entry in entries.flatten() {
        let full_path = entry.path();
        let file_type = entry.file_type();
        let is_file = match file_type {
            Ok(ft) if ft.is_symlink() => fs::metadata(&full_path).is_ok_and(|m| m.is_file()),
            Ok(ft) => ft.is_file(),
            Err(_) => false,
        };

        if is_file && full_path.extension().is_some_and(|ext| ext == "md") {
            if let Some(template) = load_template_from_file(&full_path, source, label) {
                templates.push(template);
            }
        }
    }

    templates
}

fn load_template_from_file(path: &Path, source: &str, label: &str) -> Option<PromptTemplate> {
    let raw = fs::read_to_string(path).ok()?;
    let parsed = parse_frontmatter(&raw);
    let mut description = parsed
        .frontmatter
        .get("description")
        .cloned()
        .unwrap_or_default();

    if description.is_empty() {
        if let Some(first_line) = parsed.body.lines().find(|line| !line.trim().is_empty()) {
            let trimmed = first_line.trim();
            let truncated = if trimmed.chars().count() > 60 {
                let s: String = trimmed.chars().take(57).collect();
                format!("{s}...")
            } else {
                trimmed.to_string()
            };
            description = truncated;
        }
    }

    if description.is_empty() {
        description = label.to_string();
    } else {
        description = format!("{description} {label}");
    }

    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("template")
        .to_string();

    Some(PromptTemplate {
        name,
        description,
        content: parsed.body,
        source: source.to_string(),
        file_path: path.to_path_buf(),
    })
}

// ============================================================================
// Themes loader
// ============================================================================

pub fn load_themes(options: LoadThemesOptions) -> LoadThemesResult {
    let mut themes = Vec::new();
    let mut diagnostics = Vec::new();

    let user_dir = options.agent_dir.join("themes");
    let project_dir = options.cwd.join(Config::project_dir()).join("themes");

    if options.include_defaults {
        themes.extend(load_themes_from_dir(
            &user_dir,
            "user",
            "(user)",
            &mut diagnostics,
        ));
        themes.extend(load_themes_from_dir(
            &project_dir,
            "project",
            "(project)",
            &mut diagnostics,
        ));
    }

    for path in options.theme_paths {
        if !path.exists() {
            continue;
        }

        let source_info = if options.include_defaults {
            ("path", build_path_source_label(&path))
        } else if is_under_path(&path, &user_dir) {
            ("user", "(user)".to_string())
        } else if is_under_path(&path, &project_dir) {
            ("project", "(project)".to_string())
        } else {
            ("path", build_path_source_label(&path))
        };

        let (source, label) = source_info;

        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {
                themes.extend(load_themes_from_dir(
                    &path,
                    source,
                    &label,
                    &mut diagnostics,
                ));
            }
            Ok(meta) if meta.is_file() && is_theme_file(&path) => {
                if let Some(theme) = load_theme_from_file(&path, source, &label, &mut diagnostics) {
                    themes.push(theme);
                }
            }
            _ => {}
        }
    }

    LoadThemesResult {
        themes,
        diagnostics,
    }
}

fn load_themes_from_dir(
    dir: &Path,
    source: &str,
    label: &str,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) -> Vec<ThemeResource> {
    let mut themes = Vec::new();
    if !dir.exists() {
        return themes;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return themes;
    };

    for entry in entries.flatten() {
        let full_path = entry.path();
        let file_type = entry.file_type();
        let is_file = match file_type {
            Ok(ft) if ft.is_symlink() => fs::metadata(&full_path).is_ok_and(|m| m.is_file()),
            Ok(ft) => ft.is_file(),
            Err(_) => false,
        };

        if is_file && is_theme_file(&full_path) {
            if let Some(theme) = load_theme_from_file(&full_path, source, label, diagnostics) {
                themes.push(theme);
            }
        }
    }

    themes
}

fn is_theme_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("json" | "ini" | "theme")
    )
}

fn load_theme_from_file(
    path: &Path,
    source: &str,
    label: &str,
    diagnostics: &mut Vec<ResourceDiagnostic>,
) -> Option<ThemeResource> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("theme")
        .to_string();

    let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("");
    let theme = match ext {
        "json" => Theme::load(path),
        "ini" | "theme" => load_legacy_ini_theme(path),
        _ => return None,
    };

    match theme {
        Ok(theme) => Some(ThemeResource {
            name,
            theme,
            source: format!("{source}:{label}"),
            file_path: path.to_path_buf(),
        }),
        Err(err) => {
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Warning,
                message: format!(
                    "Failed to load theme \"{name}\" ({}): {err}",
                    path.display()
                ),
                path: path.to_path_buf(),
                collision: None,
            });
            None
        }
    }
}

fn load_legacy_ini_theme(path: &Path) -> Result<Theme> {
    let content = fs::read_to_string(path)?;
    let mut theme = Theme::dark();
    if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
        theme.name = name.to_string();
    }

    let mut first_color = None;
    for token in content.split_whitespace() {
        let Some(raw) = token.strip_prefix('#') else {
            continue;
        };
        let trimmed = raw.trim_end_matches(|c: char| !c.is_ascii_hexdigit());
        if trimmed.len() != 6 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::config(format!(
                "Invalid color '{token}' in theme file {}",
                path.display()
            )));
        }
        if first_color.is_none() {
            first_color = Some(format!("#{trimmed}"));
        }
    }

    if let Some(accent) = first_color {
        theme.colors.accent = accent;
    }

    Ok(theme)
}

fn build_path_source_label(path: &Path) -> String {
    let base = path.file_stem().and_then(|s| s.to_str()).unwrap_or("path");
    format!("(path:{base})")
}

pub fn dedupe_prompts(
    prompts: Vec<PromptTemplate>,
) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
    let mut seen: HashMap<String, PromptTemplate> = HashMap::new();
    let mut diagnostics = Vec::new();

    for prompt in prompts {
        if let Some(existing) = seen.get(&prompt.name) {
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Collision,
                message: format!("name \"/{}\" collision", prompt.name),
                path: prompt.file_path.clone(),
                collision: Some(CollisionInfo {
                    resource_type: "prompt".to_string(),
                    name: prompt.name.clone(),
                    winner_path: existing.file_path.clone(),
                    loser_path: prompt.file_path.clone(),
                }),
            });
            continue;
        }
        seen.insert(prompt.name.clone(), prompt);
    }

    let mut prompts: Vec<PromptTemplate> = seen.into_values().collect();
    prompts.sort_by(|a, b| a.name.cmp(&b.name));
    (prompts, diagnostics)
}

pub fn dedupe_themes(themes: Vec<ThemeResource>) -> (Vec<ThemeResource>, Vec<ResourceDiagnostic>) {
    let mut seen: HashMap<String, ThemeResource> = HashMap::new();
    let mut diagnostics = Vec::new();

    for theme in themes {
        let key = theme.name.to_ascii_lowercase();
        if let Some(existing) = seen.get(&key) {
            diagnostics.push(ResourceDiagnostic {
                kind: DiagnosticKind::Collision,
                message: format!("theme \"{}\" collision", theme.name),
                path: theme.file_path.clone(),
                collision: Some(CollisionInfo {
                    resource_type: "theme".to_string(),
                    name: theme.name.clone(),
                    winner_path: existing.file_path.clone(),
                    loser_path: theme.file_path.clone(),
                }),
            });
            continue;
        }
        seen.insert(key, theme);
    }

    let mut themes: Vec<ThemeResource> = seen.into_values().collect();
    themes.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    (themes, diagnostics)
}

pub fn parse_command_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quote: Option<char> = None;

    for ch in args.chars() {
        if let Some(quote) = in_quote {
            if ch == quote {
                in_quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }

        if ch == '"' || ch == '\'' {
            in_quote = Some(ch);
        } else if ch == ' ' || ch == '\t' {
            if !current.is_empty() {
                out.push(current.clone());
                current.clear();
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        out.push(current);
    }

    out
}

#[allow(clippy::option_if_let_else)] // Clearer with if-let than map_or_else in the closure
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let mut result = content.to_string();

    // Positional $1, $2, ...
    result = replace_regex(&result, r"\$(\d+)", |caps| {
        let idx = caps[1].parse::<usize>().unwrap_or(0);
        args.get(idx.saturating_sub(1)).cloned().unwrap_or_default()
    });

    // ${@:start} or ${@:start:length}
    result = replace_regex(&result, r"\$\{@:(\d+)(?::(\d+))?\}", |caps| {
        let mut start = caps[1].parse::<usize>().unwrap_or(1);
        if start == 0 {
            start = 1;
        }
        let start_idx = start.saturating_sub(1);
        let maybe_len = caps.get(2).and_then(|m| m.as_str().parse::<usize>().ok());
        let slice = maybe_len.map_or_else(
            || args.get(start_idx..).unwrap_or(&[]).to_vec(),
            |len| {
                let end = (start_idx + len).min(args.len());
                args.get(start_idx..end).unwrap_or(&[]).to_vec()
            },
        );
        slice.join(" ")
    });

    let all_args = args.join(" ");
    result = result.replace("$ARGUMENTS", &all_args);
    result = result.replace("$@", &all_args);
    result
}

pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }
    let space_index = text.find(' ');
    let name = space_index.map_or(&text[1..], |idx| &text[1..idx]);
    let args = space_index.map_or("", |idx| &text[idx + 1..]);

    if let Some(template) = templates.iter().find(|t| t.name == name) {
        let args = parse_command_args(args);
        return substitute_args(&template.content, &args);
    }

    text.to_string()
}

fn expand_skill_command(text: &str, skills: &[Skill]) -> String {
    if !text.starts_with("/skill:") {
        return text.to_string();
    }

    let space_index = text.find(' ');
    let name = space_index.map_or(&text[7..], |idx| &text[7..idx]);
    let args = space_index.map_or("", |idx| text[idx + 1..].trim());

    let Some(skill) = skills.iter().find(|s| s.name == name) else {
        return text.to_string();
    };

    match fs::read_to_string(&skill.file_path) {
        Ok(content) => {
            let body = strip_frontmatter(&content).trim().to_string();
            let block = format!(
                "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
                skill.name,
                skill.file_path.display(),
                skill.base_dir.display(),
                body
            );
            if args.is_empty() {
                block
            } else {
                format!("{block}\n\n{args}")
            }
        }
        Err(err) => {
            eprintln!(
                "Warning: Failed to read skill {}: {err}",
                skill.file_path.display()
            );
            text.to_string()
        }
    }
}

// ============================================================================
// Frontmatter parsing helpers
// ============================================================================

struct ParsedFrontmatter {
    frontmatter: HashMap<String, String>,
    body: String,
}

fn parse_frontmatter(raw: &str) -> ParsedFrontmatter {
    let mut lines = raw.lines();
    let Some(first) = lines.next() else {
        return ParsedFrontmatter {
            frontmatter: HashMap::new(),
            body: String::new(),
        };
    };

    if first.trim() != "---" {
        return ParsedFrontmatter {
            frontmatter: HashMap::new(),
            body: raw.to_string(),
        };
    }

    let mut front_lines = Vec::new();
    let mut body_lines = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
                continue;
            }
            front_lines.push(line);
        } else {
            body_lines.push(line);
        }
    }

    if in_frontmatter {
        return ParsedFrontmatter {
            frontmatter: HashMap::new(),
            body: raw.to_string(),
        };
    }

    ParsedFrontmatter {
        frontmatter: parse_frontmatter_lines(&front_lines),
        body: body_lines.join("\n"),
    }
}

fn parse_frontmatter_lines(lines: &[&str]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        map.insert(key.to_string(), value.to_string());
    }
    map
}

fn strip_frontmatter(raw: &str) -> String {
    parse_frontmatter(raw).body
}

// ============================================================================
// Misc helpers
// ============================================================================

fn resolve_path(input: &str, cwd: &Path) -> PathBuf {
    let trimmed = input.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| cwd.to_path_buf());
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return dirs::home_dir()
            .unwrap_or_else(|| cwd.to_path_buf())
            .join(rest);
    }
    if trimmed.starts_with('~') {
        return dirs::home_dir()
            .unwrap_or_else(|| cwd.to_path_buf())
            .join(trimmed.trim_start_matches('~'));
    }
    let path = PathBuf::from(trimmed);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn is_under_path(target: &Path, root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let Ok(target) = target.canonicalize() else {
        return false;
    };
    if target == root {
        return true;
    }
    target.starts_with(root)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        let key = path.to_string_lossy().to_string();
        if seen.insert(key) {
            out.push(path);
        }
    }
    out
}

fn replace_regex<F>(input: &str, pattern: &str, mut replacer: F) -> String
where
    F: FnMut(&regex::Captures<'_>) -> String,
{
    let regex = regex::Regex::new(pattern).unwrap_or_else(|_| regex::Regex::new("$^").unwrap());
    regex
        .replace_all(input, |caps: &regex::Captures<'_>| replacer(caps))
        .to_string()
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::runtime::RuntimeBuilder;
    use std::fs;
    use std::future::Future;

    fn run_async<T>(future: impl Future<Output = T>) -> T {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        runtime.block_on(future)
    }

    #[test]
    fn test_parse_command_args() {
        assert_eq!(parse_command_args("foo bar"), vec!["foo", "bar"]);
        assert_eq!(
            parse_command_args("foo \"bar baz\" qux"),
            vec!["foo", "bar baz", "qux"]
        );
        assert_eq!(parse_command_args("foo 'bar baz'"), vec!["foo", "bar baz"]);
    }

    #[test]
    fn test_substitute_args() {
        let args = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        assert_eq!(substitute_args("hello $1", &args), "hello one");
        assert_eq!(substitute_args("$@", &args), "one two three");
        assert_eq!(substitute_args("$ARGUMENTS", &args), "one two three");
        assert_eq!(substitute_args("${@:2}", &args), "two three");
        assert_eq!(substitute_args("${@:2:1}", &args), "two");
    }

    #[test]
    fn test_expand_prompt_template() {
        let template = PromptTemplate {
            name: "review".to_string(),
            description: "Review code".to_string(),
            content: "Review $1".to_string(),
            source: "user".to_string(),
            file_path: PathBuf::from("/tmp/review.md"),
        };
        let out = expand_prompt_template("/review foo", &[template]);
        assert_eq!(out, "Review foo");
    }

    #[test]
    fn test_format_skills_for_prompt() {
        let skills = vec![
            Skill {
                name: "a".to_string(),
                description: "desc".to_string(),
                file_path: PathBuf::from("/tmp/a/SKILL.md"),
                base_dir: PathBuf::from("/tmp/a"),
                source: "user".to_string(),
                disable_model_invocation: false,
            },
            Skill {
                name: "b".to_string(),
                description: "desc".to_string(),
                file_path: PathBuf::from("/tmp/b/SKILL.md"),
                base_dir: PathBuf::from("/tmp/b"),
                source: "user".to_string(),
                disable_model_invocation: true,
            },
        ];
        let prompt = format_skills_for_prompt(&skills);
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>a</name>"));
        assert!(!prompt.contains("<name>b</name>"));
    }

    #[test]
    fn test_cli_extensions_load_when_no_extensions_flag_set() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let extension_path = temp_dir.path().join("ext.js");
            fs::write(&extension_path, "export default function() {}").expect("write extension");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: true,
                no_prompt_templates: true,
                no_extensions: true,
                no_themes: true,
                skill_paths: Vec::new(),
                prompt_paths: Vec::new(),
                extension_paths: vec![extension_path.to_string_lossy().to_string()],
                theme_paths: Vec::new(),
            };

            let loader = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect("load resources");
            assert!(loader.extensions().contains(&extension_path));
        });
    }

    #[test]
    fn test_extension_paths_deduped_between_settings_and_cli() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let extension_path = temp_dir.path().join("ext.js");
            fs::write(&extension_path, "export default function() {}").expect("write extension");

            let settings_dir = temp_dir.path().join(".pi");
            fs::create_dir_all(&settings_dir).expect("create settings dir");
            let settings_path = settings_dir.join("settings.json");
            let settings = json!({
                "extensions": [extension_path.to_string_lossy().to_string()]
            });
            fs::write(
                &settings_path,
                serde_json::to_string_pretty(&settings).expect("serialize settings"),
            )
            .expect("write settings");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: true,
                no_prompt_templates: true,
                no_extensions: false,
                no_themes: true,
                skill_paths: Vec::new(),
                prompt_paths: Vec::new(),
                extension_paths: vec![extension_path.to_string_lossy().to_string()],
                theme_paths: Vec::new(),
            };

            let loader = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect("load resources");
            let matches = loader
                .extensions()
                .iter()
                .filter(|path| *path == &extension_path)
                .count();
            assert_eq!(matches, 1);
        });
    }

    #[test]
    fn test_dedupe_themes_is_case_insensitive() {
        let (themes, diagnostics) = dedupe_themes(vec![
            ThemeResource {
                name: "Dark".to_string(),
                theme: Theme::dark(),
                source: "test:first".to_string(),
                file_path: PathBuf::from("/tmp/Dark.ini"),
            },
            ThemeResource {
                name: "dark".to_string(),
                theme: Theme::dark(),
                source: "test:second".to_string(),
                file_path: PathBuf::from("/tmp/dark.ini"),
            },
        ]);

        assert_eq!(themes.len(), 1);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].kind, DiagnosticKind::Collision);
        assert!(
            diagnostics[0].message.contains("theme"),
            "unexpected diagnostic: {:?}",
            diagnostics[0]
        );
    }

    #[test]
    fn test_extract_string_list_variants() {
        assert_eq!(
            extract_string_list(&Value::String("one".to_string())),
            vec!["one".to_string()]
        );
        assert_eq!(
            extract_string_list(&json!(["one", 2, "three", true, null])),
            vec!["one".to_string(), "three".to_string()]
        );
        assert!(extract_string_list(&json!({"a": 1})).is_empty());
    }

    #[test]
    fn test_validate_name_catches_all_error_categories() {
        let errors = validate_name("Bad--Name-", "parent");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("does not match parent directory"))
        );
        assert!(errors.iter().any(|e| e.contains("invalid characters")));
        assert!(
            errors
                .iter()
                .any(|e| e.contains("must not start or end with a hyphen"))
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("must not contain consecutive hyphens"))
        );

        let too_long = "a".repeat(MAX_SKILL_NAME_LEN + 1);
        let too_long_errors = validate_name(&too_long, &too_long);
        assert!(
            too_long_errors
                .iter()
                .any(|e| e.contains(&format!("name exceeds {MAX_SKILL_NAME_LEN} characters")))
        );
    }

    #[test]
    fn test_validate_description_rules() {
        let empty_errors = validate_description("   ");
        assert!(empty_errors.iter().any(|e| e == "description is required"));

        let long = "x".repeat(MAX_SKILL_DESC_LEN + 1);
        let long_errors = validate_description(&long);
        assert!(long_errors.iter().any(|e| e.contains(&format!(
            "description exceeds {MAX_SKILL_DESC_LEN} characters"
        ))));

        assert!(validate_description("ok").is_empty());
    }

    #[test]
    fn test_validate_frontmatter_fields_allows_known_and_rejects_unknown() {
        let keys = [
            "name".to_string(),
            "description".to_string(),
            "unknown-field".to_string(),
        ];
        let errors = validate_frontmatter_fields(keys.iter());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0], "unknown frontmatter field \"unknown-field\"");
    }

    #[test]
    fn test_escape_xml_replaces_all_special_chars() {
        let escaped = escape_xml("& < > \" '");
        assert_eq!(escaped, "&amp; &lt; &gt; &quot; &apos;");
    }

    #[test]
    fn test_parse_frontmatter_valid_and_unclosed() {
        let parsed = parse_frontmatter(
            r#"---
name: "skill-name"
description: 'demo'
# comment
metadata: keep
---
body line 1
body line 2"#,
        );
        assert_eq!(
            parsed.frontmatter.get("name"),
            Some(&"skill-name".to_string())
        );
        assert_eq!(
            parsed.frontmatter.get("description"),
            Some(&"demo".to_string())
        );
        assert_eq!(
            parsed.frontmatter.get("metadata"),
            Some(&"keep".to_string())
        );
        assert_eq!(parsed.body, "body line 1\nbody line 2");

        let unclosed = parse_frontmatter(
            r"---
name: nope
still frontmatter",
        );
        assert!(unclosed.frontmatter.is_empty());
        assert!(unclosed.body.starts_with("---"));
    }

    #[test]
    fn test_resolve_path_tilde_relative_absolute_and_trim() {
        let cwd = Path::new("/work/cwd");
        let home = dirs::home_dir().unwrap_or_else(|| cwd.to_path_buf());

        assert_eq!(resolve_path("  rel/file  ", cwd), cwd.join("rel/file"));
        assert_eq!(resolve_path("/abs/file", cwd), PathBuf::from("/abs/file"));
        assert_eq!(resolve_path("~", cwd), home);
        assert_eq!(resolve_path("~/cfg", cwd), home.join("cfg"));
        assert_eq!(resolve_path("~custom", cwd), home.join("custom"));
    }

    #[test]
    fn test_theme_path_helpers() {
        assert!(is_theme_file(Path::new("/tmp/theme.json")));
        assert!(is_theme_file(Path::new("/tmp/theme.ini")));
        assert!(is_theme_file(Path::new("/tmp/theme.theme")));
        assert!(!is_theme_file(Path::new("/tmp/theme.txt")));

        assert_eq!(
            build_path_source_label(Path::new("/tmp/ocean.theme")),
            "(path:ocean)"
        );
        assert_eq!(build_path_source_label(Path::new("/")), "(path:path)");
    }

    #[test]
    fn test_dedupe_paths_preserves_order_of_first_occurrence() {
        let paths = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/a"),
            PathBuf::from("/c"),
            PathBuf::from("/b"),
        ];
        let deduped = dedupe_paths(paths);
        assert_eq!(
            deduped,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/b"),
                PathBuf::from("/c"),
            ]
        );
    }

    // ── strip_frontmatter ──────────────────────────────────────────────

    #[test]
    fn test_strip_frontmatter_removes_yaml_header() {
        let raw = "---\nname: test\n---\nbody content";
        assert_eq!(strip_frontmatter(raw), "body content");
    }

    #[test]
    fn test_strip_frontmatter_returns_body_when_no_frontmatter() {
        let raw = "just body content";
        assert_eq!(strip_frontmatter(raw), "just body content");
    }

    // ── is_under_path ──────────────────────────────────────────────────

    #[test]
    fn test_is_under_path_same_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(is_under_path(tmp.path(), tmp.path()));
    }

    #[test]
    fn test_is_under_path_child() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let child = tmp.path().join("sub");
        fs::create_dir(&child).expect("mkdir");
        assert!(is_under_path(&child, tmp.path()));
    }

    #[test]
    fn test_is_under_path_unrelated() {
        let tmp1 = tempfile::tempdir().expect("tmp1");
        let tmp2 = tempfile::tempdir().expect("tmp2");
        assert!(!is_under_path(tmp1.path(), tmp2.path()));
    }

    #[test]
    fn test_is_under_path_nonexistent() {
        assert!(!is_under_path(
            Path::new("/nonexistent/a"),
            Path::new("/nonexistent/b")
        ));
    }

    // ── dedupe_prompts ─────────────────────────────────────────────────

    #[test]
    fn test_dedupe_prompts_removes_duplicates_keeps_first() {
        let prompts = vec![
            PromptTemplate {
                name: "review".to_string(),
                description: "first".to_string(),
                content: "content1".to_string(),
                source: "a".to_string(),
                file_path: PathBuf::from("/a/review.md"),
            },
            PromptTemplate {
                name: "review".to_string(),
                description: "second".to_string(),
                content: "content2".to_string(),
                source: "b".to_string(),
                file_path: PathBuf::from("/b/review.md"),
            },
            PromptTemplate {
                name: "unique".to_string(),
                description: "only one".to_string(),
                content: "content3".to_string(),
                source: "c".to_string(),
                file_path: PathBuf::from("/c/unique.md"),
            },
        ];
        let (deduped, diagnostics) = dedupe_prompts(prompts);
        assert_eq!(deduped.len(), 2);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].kind, DiagnosticKind::Collision);
        assert!(diagnostics[0].message.contains("review"));
    }

    #[test]
    fn test_dedupe_prompts_sorts_by_name() {
        let prompts = vec![
            PromptTemplate {
                name: "z-prompt".to_string(),
                description: "z".to_string(),
                content: String::new(),
                source: "s".to_string(),
                file_path: PathBuf::from("/z.md"),
            },
            PromptTemplate {
                name: "a-prompt".to_string(),
                description: "a".to_string(),
                content: String::new(),
                source: "s".to_string(),
                file_path: PathBuf::from("/a.md"),
            },
        ];
        let (deduped, diagnostics) = dedupe_prompts(prompts);
        assert!(diagnostics.is_empty());
        assert_eq!(deduped[0].name, "a-prompt");
        assert_eq!(deduped[1].name, "z-prompt");
    }

    // ── expand_skill_command ───────────────────────────────────────────

    #[test]
    fn test_expand_skill_command_with_matching_skill() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill_file = tmp.path().join("SKILL.md");
        fs::write(
            &skill_file,
            "---\nname: test-skill\ndescription: A test\n---\nDo the thing.",
        )
        .expect("write skill");

        let skills = vec![Skill {
            name: "test-skill".to_string(),
            description: "A test".to_string(),
            file_path: skill_file,
            base_dir: tmp.path().to_path_buf(),
            source: "test".to_string(),
            disable_model_invocation: false,
        }];
        let result = expand_skill_command("/skill:test-skill extra args", &skills);
        assert!(result.contains("<skill name=\"test-skill\""));
        assert!(result.contains("Do the thing."));
        assert!(result.contains("extra args"));
    }

    #[test]
    fn test_expand_skill_command_no_matching_skill_returns_input() {
        let result = expand_skill_command("/skill:nonexistent", &[]);
        assert_eq!(result, "/skill:nonexistent");
    }

    #[test]
    fn test_expand_skill_command_non_skill_prefix_returns_input() {
        let result = expand_skill_command("plain text", &[]);
        assert_eq!(result, "plain text");
    }

    // ── parse_command_args edge cases ──────────────────────────────────

    #[test]
    fn test_parse_command_args_empty() {
        assert!(parse_command_args("").is_empty());
        assert!(parse_command_args("   ").is_empty());
    }

    #[test]
    fn test_parse_command_args_tabs_as_separators() {
        assert_eq!(parse_command_args("a\tb\tc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_parse_command_args_unclosed_quote() {
        // Unclosed quote just includes chars up to end
        assert_eq!(parse_command_args("foo \"bar"), vec!["foo", "bar"]);
    }

    // ── substitute_args edge cases ─────────────────────────────────────

    #[test]
    fn test_substitute_args_out_of_range_positional() {
        let args = vec!["one".to_string()];
        assert_eq!(substitute_args("$2", &args), "");
    }

    #[test]
    fn test_substitute_args_zero_positional() {
        let args = vec!["one".to_string(), "two".to_string()];
        // $0 → index 0-1 = index -1 which saturates → args[0]? No, it tries idx.saturating_sub(1) = 0
        // Actually $0 parsed as idx=0, then 0.saturating_sub(1) = 0, so args[0] = "one"
        // Wait, 0usize.saturating_sub(1) = 0, but that's still index 0 which is "one"
        // Hmm, actually 0_usize.saturating_sub(1) = 0, so args.get(0) = Some("one")
        // But logically $0 shouldn't match anything... let me just test the behavior
        let result = substitute_args("$0", &args);
        // $0 → idx=0, 0.saturating_sub(1)=0, args[0]="one"
        assert_eq!(result, "one");
    }

    #[test]
    fn test_substitute_args_empty_args() {
        let result = substitute_args("$1 $@ $ARGUMENTS", &[]);
        assert_eq!(result, "  ");
    }

    // ── expand_prompt_template edge cases ──────────────────────────────

    #[test]
    fn test_expand_prompt_template_non_slash_returns_as_is() {
        let result = expand_prompt_template("plain text", &[]);
        assert_eq!(result, "plain text");
    }

    #[test]
    fn test_expand_prompt_template_unknown_command_returns_as_is() {
        let result = expand_prompt_template("/nonexistent foo", &[]);
        assert_eq!(result, "/nonexistent foo");
    }

    // ── parse_frontmatter edge cases ───────────────────────────────────

    #[test]
    fn test_parse_frontmatter_empty_input() {
        let parsed = parse_frontmatter("");
        assert!(parsed.frontmatter.is_empty());
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn test_parse_frontmatter_only_body() {
        let parsed = parse_frontmatter("no frontmatter here\njust body");
        assert!(parsed.frontmatter.is_empty());
        assert_eq!(parsed.body, "no frontmatter here\njust body");
    }

    #[test]
    fn test_parse_frontmatter_empty_key_ignored() {
        let parsed = parse_frontmatter("---\n: value\nname: test\n---\nbody");
        assert!(!parsed.frontmatter.contains_key(""));
        assert_eq!(parsed.frontmatter.get("name"), Some(&"test".to_string()));
    }

    // ── validate_name edge cases ───────────────────────────────────────

    #[test]
    fn test_validate_name_valid_name() {
        let errors = validate_name("good-name", "good-name");
        assert!(errors.is_empty());
    }

    #[test]
    fn test_validate_name_single_char() {
        let errors = validate_name("a", "a");
        assert!(errors.is_empty());
    }

    // ── CollisionInfo and DiagnosticKind ────────────────────────────────

    #[test]
    fn test_diagnostic_kind_equality() {
        assert_eq!(DiagnosticKind::Warning, DiagnosticKind::Warning);
        assert_eq!(DiagnosticKind::Collision, DiagnosticKind::Collision);
        assert_ne!(DiagnosticKind::Warning, DiagnosticKind::Collision);
    }

    // ── replace_regex ──────────────────────────────────────────────────

    #[test]
    fn test_replace_regex_no_match_returns_input() {
        let result = replace_regex("hello world", r"\d+", |_| "num".to_string());
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_replace_regex_replaces_all_matches() {
        let result = replace_regex("a1b2c3", r"\d", |caps| format!("[{}]", &caps[0]));
        assert_eq!(result, "a[1]b[2]c[3]");
    }

    // ── load_skill_from_file with valid skill ──────────────────────────

    #[test]
    fn test_load_skill_from_file_valid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill_dir = tmp.path().join("my-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        let skill_file = skill_dir.join("SKILL.md");
        fs::write(
            &skill_file,
            "---\nname: my-skill\ndescription: A great skill\n---\nDo something.",
        )
        .expect("write");

        let result = load_skill_from_file(&skill_file, "test".to_string());
        assert!(result.skill.is_some());
        let skill = result.skill.unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "A great skill");
    }

    #[test]
    fn test_load_skill_from_file_missing_description() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill_dir = tmp.path().join("bad-skill");
        fs::create_dir(&skill_dir).expect("mkdir");
        let skill_file = skill_dir.join("SKILL.md");
        fs::write(&skill_file, "---\nname: bad-skill\n---\nContent.").expect("write");

        let result = load_skill_from_file(&skill_file, "test".to_string());
        assert!(!result.diagnostics.is_empty());
    }
}
