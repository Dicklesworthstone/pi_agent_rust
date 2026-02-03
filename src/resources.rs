//! Resource loading for skills, prompt templates, themes, and extensions.
//!
//! Implements a subset of pi-mono's resource discovery behavior:
//! - Skills (Agent Skills spec)
//! - Prompt templates (markdown files with optional frontmatter)
//! - Package-based resource discovery

use crate::config::Config;
use crate::error::Result;
use crate::package_manager::{PackageManager, ResolveExtensionSourcesOptions};
use rich_rust::Theme;
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
        let resolved = manager.resolve().await?;
        let cli_extensions = manager
            .resolve_extension_sources(
                &cli.extension_paths,
                ResolveExtensionSourcesOptions {
                    local: false,
                    temporary: true,
                },
            )
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
            enable_skill_commands,
        })
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
            if let Ok(theme) = Theme::read(path, true) {
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

pub fn discover_package_resources(manager: &PackageManager) -> Result<PackageResources> {
    let entries = manager.list_packages().unwrap_or_default();
    let mut resources = PackageResources::default();

    for entry in entries {
        let Some(root) = manager.installed_path(&entry.source, entry.scope)? else {
            continue;
        };
        if !root.exists() {
            if let Err(err) = manager.install(&entry.source, entry.scope) {
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
    LoadSkillsResult {
        skills: skill_map.into_values().collect(),
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
            let mut truncated = first_line.trim().to_string();
            if truncated.len() > 60 {
                truncated.truncate(60);
                truncated.push_str("...");
            }
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
        Some("ini" | "theme")
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

    match Theme::read(path, true) {
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

fn build_path_source_label(path: &Path) -> String {
    let base = path.file_stem().and_then(|s| s.to_str()).unwrap_or("path");
    format!("(path:{base})")
}

fn dedupe_prompts(prompts: Vec<PromptTemplate>) -> (Vec<PromptTemplate>, Vec<ResourceDiagnostic>) {
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

    (seen.into_values().collect(), diagnostics)
}

fn dedupe_themes(themes: Vec<ThemeResource>) -> (Vec<ThemeResource>, Vec<ResourceDiagnostic>) {
    let mut seen: HashMap<String, ThemeResource> = HashMap::new();
    let mut diagnostics = Vec::new();

    for theme in themes {
        if let Some(existing) = seen.get(&theme.name) {
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
        seen.insert(theme.name.clone(), theme);
    }

    (seen.into_values().collect(), diagnostics)
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
            |len| args.get(start_idx..start_idx + len).unwrap_or(&[]).to_vec(),
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
}
