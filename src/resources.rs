//! Resource loading for skills, prompt templates, themes, and extensions.
//!
//! Implements a subset of pi-mono's resource discovery behavior:
//! - Skills (Agent Skills spec)
//! - Prompt templates (markdown files with optional frontmatter)
//! - Package-based resource discovery

use crate::config::Config;
use crate::error::{Error, Result};
use crate::package_manager::{
    PackageManager, PackageScope, ResolveExtensionSourcesOptions, ResolvedResource, ResourceOrigin,
};
use crate::theme::Theme;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use tracing::warn;

fn panic_payload_message(payload: Box<dyn std::any::Any + Send + 'static>) -> String {
    payload.downcast::<String>().map_or_else(
        |payload| {
            payload.downcast::<&'static str>().map_or_else(
                |_| "unknown panic payload".to_string(),
                |message| (*message).to_string(),
            )
        },
        |message| *message,
    )
}

fn read_dir_sorted_paths(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

fn canonical_identity_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolved_path_kind(path: &Path) -> (bool, bool) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::metadata(path).map_or((false, false), |meta| (meta.is_dir(), meta.is_file()))
        }
        Ok(meta) => (meta.is_dir(), meta.is_file()),
        Err(_) => (false, false),
    }
}

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

impl ResourceCliOptions {
    #[must_use]
    pub const fn has_explicit_paths(&self) -> bool {
        !self.skill_paths.is_empty()
            || !self.prompt_paths.is_empty()
            || !self.extension_paths.is_empty()
            || !self.theme_paths.is_empty()
    }

    /// Returns `true` when every resource category is disabled via `--no-*` flags
    /// and there are no explicit CLI `-e` extension sources that would require
    /// package resolution.
    #[must_use]
    pub const fn all_configured_resources_disabled(&self) -> bool {
        self.no_skills && self.no_prompt_templates && self.no_extensions && self.no_themes
    }
}

#[derive(Debug, Clone, Default)]
pub struct PackageResources {
    pub extensions: Vec<PathBuf>,
    pub skills: Vec<PathBuf>,
    pub prompts: Vec<PathBuf>,
    pub themes: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub struct ExtensionResourcePaths {
    pub skill_paths: Vec<PathBuf>,
    pub prompt_paths: Vec<PathBuf>,
    pub theme_paths: Vec<PathBuf>,
}

impl ExtensionResourcePaths {
    pub const fn is_empty(&self) -> bool {
        self.skill_paths.is_empty() && self.prompt_paths.is_empty() && self.theme_paths.is_empty()
    }
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

        // Skip the expensive package/settings resolution when every resource
        // category is disabled (--no-extensions --no-skills etc.) and there are
        // no explicit CLI `-e` sources.  This avoids reading settings.json,
        // running npm lookups, and hitting the network on startup when the user
        // explicitly opted out of all configured resources (Issue #38).
        let skip_configured_resolution =
            cli.all_configured_resources_disabled() && cli.extension_paths.is_empty();

        let resolved = if skip_configured_resolution {
            crate::package_manager::ResolvedPaths::default()
        } else {
            Box::pin(manager.resolve()).await?
        };

        let cli_extensions = if cli.extension_paths.is_empty() {
            crate::package_manager::ResolvedPaths::default()
        } else {
            validate_non_empty_cli_inputs(&cli.extension_paths, "extension source")?;
            Box::pin(manager.resolve_extension_sources(
                &cli.extension_paths,
                ResolveExtensionSourcesOptions {
                    local: false,
                    temporary: true,
                },
            ))
            .await?
        };

        validate_non_empty_cli_inputs(&cli.skill_paths, "skill path")?;
        let explicit_skill_paths = dedupe_paths(
            cli.skill_paths
                .iter()
                .map(|path| resolve_path(path, cwd))
                .collect(),
        );
        validate_explicit_resource_paths(&explicit_skill_paths, ExplicitResourceKind::Skill)?;

        validate_non_empty_cli_inputs(&cli.prompt_paths, "prompt template path")?;
        let explicit_prompt_paths = dedupe_paths(
            cli.prompt_paths
                .iter()
                .map(|path| resolve_path(path, cwd))
                .collect(),
        );
        validate_explicit_resource_paths(&explicit_prompt_paths, ExplicitResourceKind::Prompt)?;

        validate_non_empty_cli_inputs(&cli.theme_paths, "theme path")?;
        let explicit_theme_paths = dedupe_paths(
            cli.theme_paths
                .iter()
                .map(|path| resolve_path(path, cwd))
                .collect(),
        );
        validate_explicit_resource_paths(&explicit_theme_paths, ExplicitResourceKind::Theme)?;

        // Merge paths with documented precedence semantics:
        // - explicit CLI resources win over everything else
        // - CLI `-e` resources outrank configured/project/global/package resources
        // - project directories outrank global directories, which outrank installed packages
        // - `--no-skills` / `--no-prompt-templates` / `--no-themes` only disable configured
        //   resources; explicit CLI paths and CLI `-e` resources still participate
        let skill_paths = merge_resource_paths(
            &explicit_skill_paths,
            cli_extensions.skills,
            resolved.skills,
            !cli.no_skills,
        );

        let prompt_paths = merge_resource_paths(
            &explicit_prompt_paths,
            cli_extensions.prompts,
            resolved.prompts,
            !cli.no_prompt_templates,
        );

        let theme_paths = merge_resource_paths(
            &explicit_theme_paths,
            cli_extensions.themes,
            resolved.themes,
            !cli.no_themes,
        );

        // Extension entries:
        // - `--no-extensions` disables configured + auto discovery but still allows CLI `-e` sources.
        // - Deduplicate by canonical extension ID so that transpiled cache copies
        //   in `~/.pi/agent/cache/modules/` don't cause command collisions with
        //   the original source `.ts` extensions (Issue #37).
        let extension_entries = dedupe_extension_entries_by_id(merge_resource_paths(
            &[],
            cli_extensions.extensions,
            resolved.extensions,
            !cli.no_extensions,
        ));

        // Load skills, prompt templates, and themes in parallel — they are independent
        // filesystem walks that benefit from overlapped I/O on multi-core machines.
        let agent_dir = Config::global_dir();
        let cwd_buf = cwd.to_path_buf();
        let (skills_join, prompts_join, themes_join) = std::thread::scope(|s| {
            let cwd_s = &cwd_buf;
            let agent_s = &agent_dir;
            let skills_handle = s.spawn(move || {
                load_skills(LoadSkillsOptions {
                    cwd: cwd_s.clone(),
                    agent_dir: agent_s.clone(),
                    skill_paths,
                    include_defaults: false,
                })
            });
            let prompts_handle = s.spawn(move || {
                load_prompt_templates(LoadPromptTemplatesOptions {
                    cwd: cwd_s.clone(),
                    agent_dir: agent_s.clone(),
                    prompt_paths,
                    include_defaults: false,
                })
            });
            let themes_handle = s.spawn(move || {
                load_themes(LoadThemesOptions {
                    cwd: cwd_s.clone(),
                    agent_dir: agent_s.clone(),
                    theme_paths,
                    include_defaults: false,
                })
            });
            (
                skills_handle.join(),
                prompts_handle.join(),
                themes_handle.join(),
            )
        });
        let skills_result = skills_join.map_err(|payload| {
            Error::config(format!(
                "Skills loader thread panicked: {}",
                panic_payload_message(payload)
            ))
        })?;
        let prompt_templates = prompts_join.map_err(|payload| {
            Error::config(format!(
                "Prompt loader thread panicked: {}",
                panic_payload_message(payload)
            ))
        })?;
        let themes_result = themes_join.map_err(|payload| {
            Error::config(format!(
                "Theme loader thread panicked: {}",
                panic_payload_message(payload)
            ))
        })?;
        let (prompts, prompt_diagnostics) = dedupe_prompts(prompt_templates);
        let (themes, theme_diagnostics) = dedupe_themes(themes_result.themes);
        let mut theme_diags = themes_result.diagnostics;
        theme_diags.extend(theme_diagnostics);
        ensure_explicit_file_paths_loaded(
            &explicit_skill_paths,
            skills_result
                .skills
                .iter()
                .map(|skill| skill.file_path.clone())
                .collect(),
            &skills_result.diagnostics,
            ExplicitResourceKind::Skill,
        )?;
        ensure_explicit_file_paths_loaded(
            &explicit_prompt_paths,
            prompts
                .iter()
                .map(|prompt| prompt.file_path.clone())
                .collect(),
            &prompt_diagnostics,
            ExplicitResourceKind::Prompt,
        )?;
        ensure_explicit_file_paths_loaded(
            &explicit_theme_paths,
            themes.iter().map(|theme| theme.file_path.clone()).collect(),
            &theme_diags,
            ExplicitResourceKind::Theme,
        )?;

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

    pub fn extend_with_paths(&mut self, cwd: &Path, paths: &ExtensionResourcePaths) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }

        let agent_dir = Config::global_dir();
        let cwd_buf = cwd.to_path_buf();

        if !paths.skill_paths.is_empty() {
            let skill_paths = dedupe_paths(paths.skill_paths.clone());
            if !skill_paths.is_empty() {
                let result = load_skills(LoadSkillsOptions {
                    cwd: cwd_buf.clone(),
                    agent_dir: agent_dir.clone(),
                    skill_paths,
                    include_defaults: false,
                });

                let mut existing_names: HashMap<String, PathBuf> = HashMap::new();
                let mut existing_paths: HashSet<PathBuf> = HashSet::new();
                for skill in &self.skills {
                    existing_names.insert(skill.name.clone(), skill.file_path.clone());
                    existing_paths.insert(canonical_identity_path(&skill.file_path));
                }

                let mut collisions = Vec::new();
                for skill in result.skills {
                    let real_path = canonical_identity_path(&skill.file_path);
                    if existing_paths.contains(&real_path) {
                        continue;
                    }
                    if let Some(winner_path) = existing_names.get(&skill.name) {
                        collisions.push(ResourceDiagnostic {
                            kind: DiagnosticKind::Collision,
                            message: format!("name \"{}\" collision", skill.name),
                            path: skill.file_path.clone(),
                            collision: Some(CollisionInfo {
                                resource_type: "skill".to_string(),
                                name: skill.name.clone(),
                                winner_path: winner_path.clone(),
                                loser_path: skill.file_path.clone(),
                            }),
                        });
                    } else {
                        existing_names.insert(skill.name.clone(), skill.file_path.clone());
                        existing_paths.insert(real_path);
                        self.skills.push(skill);
                    }
                }

                self.skill_diagnostics.extend(result.diagnostics);
                self.skill_diagnostics.extend(collisions);
            }
        }

        if !paths.prompt_paths.is_empty() {
            let prompt_paths = dedupe_paths(paths.prompt_paths.clone());
            if !prompt_paths.is_empty() {
                let new_prompts = load_prompt_templates(LoadPromptTemplatesOptions {
                    cwd: cwd_buf.clone(),
                    agent_dir: agent_dir.clone(),
                    prompt_paths,
                    include_defaults: false,
                });
                if !new_prompts.is_empty() {
                    let mut merged = self.prompts.clone();
                    merged.extend(new_prompts);
                    let (deduped, diagnostics) = dedupe_prompts(merged);
                    self.prompts = deduped;
                    self.prompt_diagnostics.extend(diagnostics);
                }
            }
        }

        if !paths.theme_paths.is_empty() {
            let theme_paths = dedupe_paths(paths.theme_paths.clone());
            if !theme_paths.is_empty() {
                let themes_result = load_themes(LoadThemesOptions {
                    cwd: cwd_buf,
                    agent_dir,
                    theme_paths,
                    include_defaults: false,
                });
                let mut merged = self.themes.clone();
                merged.extend(themes_result.themes);
                let (deduped, diagnostics) = dedupe_themes(merged);
                self.themes = deduped;
                self.theme_diagnostics.extend(themes_result.diagnostics);
                self.theme_diagnostics.extend(diagnostics);
            }
        }

        Ok(())
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
        if !root.exists()
            && let Err(err) = manager.install(&entry.source, entry.scope).await
        {
            warn!("Failed to install {}: {err}", entry.source);
            continue;
        }

        if !root.exists() {
            continue;
        }

        if let Some(pi) = read_pi_manifest(&root)? {
            append_resources_from_manifest(&mut resources, &root, &pi)?;
        } else {
            append_resources_from_defaults(&mut resources, &root);
        }
    }

    Ok(resources)
}

fn read_pi_manifest(root: &Path) -> Result<Option<Value>> {
    let manifest_path = root.join("package.json");
    if !manifest_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&manifest_path).map_err(|err| {
        Error::config(format!(
            "Failed to read package manifest {}: {err}",
            manifest_path.display()
        ))
    })?;
    let json: Value = serde_json::from_str(&raw).map_err(|err| {
        Error::config(format!(
            "Failed to parse package manifest {}: {err}",
            manifest_path.display()
        ))
    })?;
    match json.get("pi") {
        Some(pi) if pi.is_object() => Ok(Some(pi.clone())),
        Some(_) => Err(Error::config(format!(
            "Invalid package manifest {}: `pi` must be an object",
            manifest_path.display()
        ))),
        None => Ok(None),
    }
}

fn append_resources_from_manifest(
    resources: &mut PackageResources,
    root: &Path,
    pi: &Value,
) -> Result<()> {
    let Some(obj) = pi.as_object() else {
        return Ok(());
    };
    append_resource_paths(
        resources,
        root,
        obj.get("extensions"),
        ResourceKind::Extensions,
        "extensions",
    )?;
    append_resource_paths(
        resources,
        root,
        obj.get("skills"),
        ResourceKind::Skills,
        "skills",
    )?;
    append_resource_paths(
        resources,
        root,
        obj.get("prompts"),
        ResourceKind::Prompts,
        "prompts",
    )?;
    append_resource_paths(
        resources,
        root,
        obj.get("themes"),
        ResourceKind::Themes,
        "themes",
    )?;
    Ok(())
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
    field_name: &str,
) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let manifest_path = root.join("package.json");
    let paths = extract_manifest_string_list(&manifest_path, field_name, value)?;
    if paths.is_empty() {
        return Ok(());
    }

    for path in paths {
        let resolved = resolve_manifest_resource_path(root, &manifest_path, field_name, &path)?;
        match kind {
            ResourceKind::Extensions => resources.extensions.push(resolved),
            ResourceKind::Skills => resources.skills.push(resolved),
            ResourceKind::Prompts => resources.prompts.push(resolved),
            ResourceKind::Themes => resources.themes.push(resolved),
        }
    }
    Ok(())
}

fn extract_manifest_string_list(
    manifest_path: &Path,
    field_name: &str,
    value: &Value,
) -> Result<Vec<String>> {
    match value {
        Value::String(s) => Ok(vec![validate_manifest_resource_string(
            manifest_path,
            field_name,
            s,
        )?]),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                item.as_str().ok_or_else(|| {
                    Error::config(format!(
                        "Invalid package manifest {}: `pi.{field_name}` must be a string or array of strings",
                        manifest_path.display()
                    ))
                }).and_then(|path| validate_manifest_resource_string(manifest_path, field_name, path))
            })
            .collect(),
        _ => Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{field_name}` must be a string or array of strings",
            manifest_path.display()
        ))),
    }
}

fn validate_manifest_resource_string(
    manifest_path: &Path,
    field_name: &str,
    value: &str,
) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{field_name}` entries must be non-empty paths",
            manifest_path.display()
        )));
    }
    Ok(trimmed.to_string())
}

fn resolve_manifest_resource_path(
    root: &Path,
    manifest_path: &Path,
    field_name: &str,
    raw_path: &str,
) -> Result<PathBuf> {
    let relative = Path::new(raw_path);
    if relative.is_absolute() {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{field_name}` paths must stay within the package root",
            manifest_path.display()
        )));
    }

    let mut depth = 0usize;
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(_) => depth = depth.saturating_add(1),
            Component::ParentDir => {
                if depth == 0 {
                    return Err(Error::config(format!(
                        "Invalid package manifest {}: `pi.{field_name}` paths must stay within the package root",
                        manifest_path.display()
                    )));
                }
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(Error::config(format!(
                    "Invalid package manifest {}: `pi.{field_name}` paths must stay within the package root",
                    manifest_path.display()
                )));
            }
        }
    }

    let resolved = root.join(relative);
    if resolved.exists() && !is_under_path(&resolved, root) {
        return Err(Error::config(format!(
            "Invalid package manifest {}: `pi.{field_name}` paths must stay within the package root",
            manifest_path.display()
        )));
    }
    Ok(resolved)
}

// ============================================================================
// Skills loader
// ============================================================================

#[allow(clippy::too_many_lines, clippy::items_after_statements)]
pub fn load_skills(options: LoadSkillsOptions) -> LoadSkillsResult {
    let mut skill_map: HashMap<String, Skill> = HashMap::new();
    let mut real_paths: HashSet<PathBuf> = HashSet::new();
    let mut visited_dirs: HashSet<PathBuf> = HashSet::new();
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
            let real_path = canonical_identity_path(&skill.file_path);
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
            load_skills_from_dir_with_visited(
                options.cwd.join(Config::project_dir()).join("skills"),
                "project".to_string(),
                true,
                &mut visited_dirs,
            ),
            &mut skill_map,
            &mut real_paths,
            &mut diagnostics,
            &mut collisions,
        );
        merge_skills(
            load_skills_from_dir_with_visited(
                options.agent_dir.join("skills"),
                "user".to_string(),
                true,
                &mut visited_dirs,
            ),
            &mut skill_map,
            &mut real_paths,
            &mut diagnostics,
            &mut collisions,
        );
    }

    for resolved in options.skill_paths {
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
                    load_skills_from_dir_with_visited(resolved, source, true, &mut visited_dirs),
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

#[cfg(test)]
fn load_skills_from_dir(
    dir: PathBuf,
    source: String,
    include_root_files: bool,
) -> LoadSkillsResult {
    let mut visited_dirs = HashSet::new();
    load_skills_from_dir_with_visited(dir, source, include_root_files, &mut visited_dirs)
}

fn load_skills_from_dir_with_visited(
    dir: PathBuf,
    source: String,
    include_root_files: bool,
    visited_dirs: &mut HashSet<PathBuf>,
) -> LoadSkillsResult {
    let mut skills = Vec::new();
    let mut diagnostics = Vec::new();
    let mut stack = vec![(dir, source, include_root_files)];

    while let Some((current_dir, current_source, current_include_root)) = stack.pop() {
        if !current_dir.exists() {
            continue;
        }

        // Prevent unbounded recursion for symlink cycles.
        let canonical_dir = fs::canonicalize(&current_dir).unwrap_or_else(|_| current_dir.clone());
        if !visited_dirs.insert(canonical_dir) {
            continue;
        }

        let mut child_dirs = Vec::new();

        for full_path in read_dir_sorted_paths(&current_dir) {
            let file_name = full_path.file_name().unwrap_or_default().to_string_lossy();

            if file_name.starts_with('.') || file_name == "node_modules" {
                continue;
            }

            let (is_dir, is_file) = resolved_path_kind(&full_path);

            if is_dir {
                child_dirs.push(full_path);
                continue;
            }

            if !is_file {
                continue;
            }

            let is_root_md = current_include_root && file_name.ends_with(".md");
            let is_skill_md = !current_include_root && file_name == "SKILL.md";
            if !is_root_md && !is_skill_md {
                continue;
            }

            let result = load_skill_from_file(&full_path, current_source.clone());
            if let Some(skill) = result.skill {
                skills.push(skill);
            }
            diagnostics.extend(result.diagnostics);
        }

        for child_dir in child_dirs.into_iter().rev() {
            stack.push((child_dir, current_source.clone(), false));
        }
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
        templates.extend(load_templates_from_dir(
            &project_dir,
            "project",
            "(project)",
        ));
        templates.extend(load_templates_from_dir(&user_dir, "user", "(user)"));
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

    for full_path in read_dir_sorted_paths(dir) {
        let (_, is_file) = resolved_path_kind(&full_path);

        if is_file
            && full_path.extension().is_some_and(|ext| ext == "md")
            && let Some(template) = load_template_from_file(&full_path, source, label)
        {
            templates.push(template);
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

    if description.is_empty()
        && let Some(first_line) = parsed.body.lines().find(|line| !line.trim().is_empty())
    {
        let trimmed = first_line.trim();
        let truncated = if trimmed.chars().count() > 60 {
            let s: String = trimmed.chars().take(57).collect();
            format!("{s}...")
        } else {
            trimmed.to_string()
        };
        description = truncated;
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
            &project_dir,
            "project",
            "(project)",
            &mut diagnostics,
        ));
        themes.extend(load_themes_from_dir(
            &user_dir,
            "user",
            "(user)",
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

    for full_path in read_dir_sorted_paths(dir) {
        let (_, is_file) = resolved_path_kind(&full_path);

        if is_file
            && is_theme_file(&full_path)
            && let Some(theme) = load_theme_from_file(&full_path, source, label, diagnostics)
        {
            themes.push(theme);
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
        let real_path = canonical_identity_path(&prompt.file_path);
        if let Some(existing) = seen.get(&prompt.name) {
            if canonical_identity_path(&existing.file_path) == real_path {
                continue;
            }
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
        let real_path = canonical_identity_path(&theme.file_path);
        if let Some(existing) = seen.get(&key) {
            if canonical_identity_path(&existing.file_path) == real_path {
                continue;
            }
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
    let mut just_closed_quote = false;

    for ch in args.chars() {
        if let Some(quote) = in_quote {
            if ch == quote {
                in_quote = None;
                just_closed_quote = true;
            } else {
                current.push(ch);
            }
            continue;
        }

        // Treat quotes as delimiters only at the start of a token so embedded
        // apostrophes in natural-language input stay literal.
        if (ch == '"' || ch == '\'') && current.is_empty() {
            in_quote = Some(ch);
        } else if ch.is_whitespace() {
            if !current.is_empty() || just_closed_quote {
                out.push(current.clone());
                current.clear();
            }
            just_closed_quote = false;
        } else {
            current.push(ch);
            just_closed_quote = false;
        }
    }

    if !current.is_empty() || just_closed_quote {
        out.push(current);
    }

    out
}

fn split_command_name_and_args(text: &str, prefix_len: usize) -> (&str, &str) {
    let body = &text[prefix_len..];
    let Some((idx, _)) = body.char_indices().find(|(_, ch)| ch.is_whitespace()) else {
        return (body, "");
    };

    let args_start = prefix_len + idx;
    let name = &text[prefix_len..args_start];
    let args = text[args_start..].trim_start_matches(char::is_whitespace);
    (name, args)
}

/// Cached regex for positional `$1`, `$2`, … substitution.
fn positional_arg_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\$(\d+)").expect("positional arg regex"))
}

/// Cached regex for `${@:start}` or `${@:start:length}` substitution.
fn slice_arg_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\$\{@:(\d+)(?::(\d+))?\}").expect("slice arg regex"))
}

#[allow(clippy::option_if_let_else)] // Clearer with if-let than map_or_else in the closure
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let mut result = content.to_string();

    // Positional $1, $2, ...
    result = replace_regex(&result, positional_arg_regex(), |caps| {
        let idx = caps[1].parse::<usize>().unwrap_or(0);
        if idx == 0 {
            String::new()
        } else {
            args.get(idx.saturating_sub(1)).cloned().unwrap_or_default()
        }
    });

    // ${@:start} or ${@:start:length}
    result = replace_regex(&result, slice_arg_regex(), |caps| {
        let mut start = caps[1].parse::<usize>().unwrap_or(1);
        if start == 0 {
            start = 1;
        }
        let start_idx = start.saturating_sub(1);
        let maybe_len = caps.get(2).and_then(|m| m.as_str().parse::<usize>().ok());
        let slice = maybe_len.map_or_else(
            || args.get(start_idx..).unwrap_or(&[]).to_vec(),
            |len| {
                let end = start_idx.saturating_add(len).min(args.len());
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
    let (name, args) = split_command_name_and_args(text, 1);

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

    let (name, args) = split_command_name_and_args(text, 7);

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

fn validate_non_empty_cli_inputs(inputs: &[String], label: &str) -> Result<()> {
    for input in inputs {
        if input.trim().is_empty() {
            return Err(Error::config(format!("Explicit {label} must be non-empty")));
        }
    }
    Ok(())
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
        let key = canonical_identity_path(&path).to_string_lossy().to_string();
        if seen.insert(key) {
            out.push(path);
        }
    }
    out
}

/// Resolve the module disk cache directory used by the JS extension runtime.
///
/// This mirrors the logic in `extensions_js::runtime_disk_cache_dir()` so that
/// we can detect cache/modules entries during discovery without depending on the
/// runtime module.
fn module_cache_dir() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os("PIJS_MODULE_CACHE_DIR") {
        return if raw.is_empty() {
            None
        } else {
            Some(PathBuf::from(raw))
        };
    }
    dirs::home_dir().map(|home| home.join(".pi").join("agent").join("cache").join("modules"))
}

fn is_cache_module_path_with_cache_dir(path: &Path, cache_dir: Option<&Path>) -> bool {
    let Some(cache_dir) = cache_dir else {
        return false;
    };
    let canonical = canonical_identity_path(path);
    let canonical_cache = canonical_identity_path(cache_dir);
    canonical.starts_with(&canonical_cache)
}

/// Derive the canonical extension ID from a filesystem path.
///
/// Uses the same heuristic as `JsExtensionLoadSpec::from_entry_path`: if the
/// file stem is `index`, the parent directory name is the ID; otherwise the
/// file stem itself is the ID.
fn extension_id_from_path(path: &Path) -> Option<String> {
    let canonical = canonical_identity_path(path);
    let stem = canonical.file_stem().and_then(|s| s.to_str())?.trim();
    if stem.is_empty() {
        return None;
    }
    if stem.eq_ignore_ascii_case("index") {
        canonical
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        Some(stem.to_string())
    }
}

fn extension_dedupe_key_from_path(path: &Path) -> Option<String> {
    extension_id_from_path(path).map(|id| id.to_ascii_lowercase())
}

/// Deduplicate extension entries by canonical extension ID, preferring source
/// entries over transpiled cache copies (Issue #37).
///
/// When both a source `.ts` extension and its transpiled cache copy in
/// `~/.pi/agent/cache/modules/` are discovered, the cache entry is dropped to
/// prevent command collisions at load time.
fn dedupe_extension_entries_by_id(entries: Vec<PathBuf>) -> Vec<PathBuf> {
    let cache_dir = module_cache_dir();
    dedupe_extension_entries_by_id_with_cache_dir(entries, cache_dir.as_deref())
}

fn dedupe_extension_entries_by_id_with_cache_dir(
    entries: Vec<PathBuf>,
    cache_dir: Option<&Path>,
) -> Vec<PathBuf> {
    // First pass: collect extension IDs and identify cache vs source entries.
    let mut id_to_source_idx: HashMap<String, usize> = HashMap::new();
    let mut is_cache = Vec::with_capacity(entries.len());

    for (idx, path) in entries.iter().enumerate() {
        let cache = is_cache_module_path_with_cache_dir(path, cache_dir);
        is_cache.push(cache);

        if let Some(id) = extension_dedupe_key_from_path(path)
            && !cache
        {
            // Source entry wins; record its index.
            id_to_source_idx.entry(id).or_insert(idx);
        }
    }

    // Second pass: keep entries unless they are cache entries whose ID already
    // has a source entry.
    let mut out = Vec::with_capacity(entries.len());
    for (idx, path) in entries.into_iter().enumerate() {
        if is_cache[idx]
            && let Some(id) = extension_dedupe_key_from_path(&path)
            && id_to_source_idx.contains_key(&id)
        {
            // Skip cache entry — source entry is preferred.
            continue;
        }
        out.push(path);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ResourcePathPrecedence {
    CliExtension,
    ProjectDirectory,
    GlobalDirectory,
    ProjectPackage,
    GlobalPackage,
}

fn precedence_sorted_enabled_paths(resources: Vec<ResolvedResource>) -> Vec<PathBuf> {
    let mut enabled = resources
        .into_iter()
        .filter(|resource| resource.enabled)
        .collect::<Vec<_>>();
    // Preserve source order within a precedence tier so CLI-specified
    // extension/resource ordering remains behaviorally significant.
    enabled.sort_by_key(resource_path_precedence);
    enabled.into_iter().map(|resource| resource.path).collect()
}

fn merge_resource_paths(
    explicit_paths: &[PathBuf],
    cli_resources: Vec<ResolvedResource>,
    resolved_resources: Vec<ResolvedResource>,
    include_resolved: bool,
) -> Vec<PathBuf> {
    let mut merged = explicit_paths.to_vec();
    merged.extend(precedence_sorted_enabled_paths(cli_resources));
    if include_resolved {
        merged.extend(precedence_sorted_enabled_paths(resolved_resources));
    }
    dedupe_paths(merged)
}

const fn resource_path_precedence(resource: &ResolvedResource) -> ResourcePathPrecedence {
    match (resource.metadata.scope, resource.metadata.origin) {
        (PackageScope::Temporary, _) => ResourcePathPrecedence::CliExtension,
        (PackageScope::Project, ResourceOrigin::TopLevel) => {
            ResourcePathPrecedence::ProjectDirectory
        }
        (PackageScope::User, ResourceOrigin::TopLevel) => ResourcePathPrecedence::GlobalDirectory,
        (PackageScope::Project, ResourceOrigin::Package) => ResourcePathPrecedence::ProjectPackage,
        (PackageScope::User, ResourceOrigin::Package) => ResourcePathPrecedence::GlobalPackage,
    }
}

#[derive(Clone, Copy)]
enum ExplicitResourceKind {
    Skill,
    Prompt,
    Theme,
}

impl ExplicitResourceKind {
    const fn label(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Prompt => "prompt template",
            Self::Theme => "theme",
        }
    }

    fn file_supported(self, path: &Path) -> bool {
        match self {
            Self::Skill | Self::Prompt => path.extension().is_some_and(|ext| ext == "md"),
            Self::Theme => is_theme_file(path),
        }
    }

    const fn unsupported_file_message(self) -> &'static str {
        match self {
            Self::Skill | Self::Prompt => "is not a markdown file",
            Self::Theme => "is not a supported theme file (.json, .ini, or .theme)",
        }
    }
}

fn validate_explicit_resource_paths(
    paths: &[PathBuf],
    resource_kind: ExplicitResourceKind,
) -> Result<()> {
    for path in paths {
        if !path.exists() {
            return Err(Error::config(format!(
                "Explicit {} path '{}' does not exist",
                resource_kind.label(),
                path.display()
            )));
        }

        let metadata = fs::metadata(path).map_err(|err| {
            Error::config(format!(
                "Failed to inspect explicit {} path '{}': {err}",
                resource_kind.label(),
                path.display()
            ))
        })?;

        if metadata.is_dir() {
            continue;
        }

        if metadata.is_file() {
            if resource_kind.file_supported(path) {
                continue;
            }

            return Err(Error::config(format!(
                "Explicit {} path '{}' {}",
                resource_kind.label(),
                path.display(),
                resource_kind.unsupported_file_message()
            )));
        }

        return Err(Error::config(format!(
            "Explicit {} path '{}' is neither a file nor a directory",
            resource_kind.label(),
            path.display()
        )));
    }

    Ok(())
}

fn ensure_explicit_file_paths_loaded(
    explicit_paths: &[PathBuf],
    loaded_paths: Vec<PathBuf>,
    diagnostics: &[ResourceDiagnostic],
    resource_kind: ExplicitResourceKind,
) -> Result<()> {
    let loaded_paths = loaded_paths
        .into_iter()
        .map(|path| canonical_identity_path(&path))
        .collect::<HashSet<_>>();

    for path in explicit_paths {
        let metadata = fs::metadata(path).map_err(|err| {
            Error::config(format!(
                "Failed to inspect explicit {} path '{}': {err}",
                resource_kind.label(),
                path.display()
            ))
        })?;
        if !metadata.is_file() {
            continue;
        }

        let key = canonical_identity_path(path);
        if loaded_paths.contains(&key) {
            continue;
        }

        let detail = diagnostics
            .iter()
            .find_map(|diagnostic| {
                if canonical_identity_path(&diagnostic.path) == key {
                    return Some(diagnostic.message.clone());
                }
                diagnostic.collision.as_ref().and_then(|collision| {
                    if canonical_identity_path(&collision.winner_path) == key
                        || canonical_identity_path(&collision.loser_path) == key
                    {
                        Some(diagnostic.message.clone())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| "file could not be loaded".to_string());

        return Err(Error::config(format!(
            "Explicit {} path '{}' could not be loaded: {detail}",
            resource_kind.label(),
            path.display()
        )));
    }

    Ok(())
}

fn replace_regex<F>(input: &str, regex: &regex::Regex, mut replacer: F) -> String
where
    F: FnMut(&regex::Captures<'_>) -> String,
{
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
        assert_eq!(
            parse_command_args("foo\tbar\n\"baz qux\"\r\n''"),
            vec!["foo", "bar", "baz qux", ""]
        );
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
        let out = expand_prompt_template("/review foo", std::slice::from_ref(&template));
        assert_eq!(out, "Review foo");
        let tab_out = expand_prompt_template("/review\tfoo", std::slice::from_ref(&template));
        assert_eq!(tab_out, "Review foo");
        let newline_out = expand_prompt_template("/review\nfoo", std::slice::from_ref(&template));
        assert_eq!(newline_out, "Review foo");
    }

    #[test]
    fn test_expand_skill_command_accepts_non_space_whitespace_separator() {
        let dir = tempfile::tempdir().expect("tempdir");
        let skill_dir = dir.path().join("review");
        fs::create_dir_all(&skill_dir).expect("create skill dir");
        let skill_file = skill_dir.join("SKILL.md");
        fs::write(
            &skill_file,
            "---\nname: review\ndescription: Review code\n---\nSkill body.\n",
        )
        .expect("write skill");

        let skill = Skill {
            name: "review".to_string(),
            description: "Review code".to_string(),
            file_path: skill_file,
            base_dir: skill_dir,
            source: "user".to_string(),
            disable_model_invocation: false,
        };

        let tab_out = expand_skill_command(
            "/skill:review\tfocus this file",
            std::slice::from_ref(&skill),
        );
        assert!(tab_out.contains("Skill body."));
        assert!(tab_out.ends_with("focus this file"));

        let newline_out = expand_skill_command("/skill:review\nfocus this file", &[skill]);
        assert!(newline_out.contains("Skill body."));
        assert!(newline_out.ends_with("focus this file"));
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
    fn test_resource_cli_options_detect_explicit_paths() {
        let empty = ResourceCliOptions {
            no_skills: false,
            no_prompt_templates: false,
            no_extensions: false,
            no_themes: false,
            skill_paths: Vec::new(),
            prompt_paths: Vec::new(),
            extension_paths: Vec::new(),
            theme_paths: Vec::new(),
        };
        assert!(!empty.has_explicit_paths());

        let with_extension = ResourceCliOptions {
            extension_paths: vec!["./ext.native.json".to_string()],
            ..empty
        };
        assert!(with_extension.has_explicit_paths());
    }

    #[test]
    fn test_cli_extensions_load_when_no_extensions_flag_set() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let extension_path = temp_dir.path().join("ext.native.json");
            fs::write(&extension_path, "{}").expect("write extension");

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
    fn test_resource_loader_rejects_missing_cli_extension_path() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let missing_path = temp_dir.path().join("missing.native.json");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: true,
                no_prompt_templates: true,
                no_extensions: false,
                no_themes: true,
                skill_paths: Vec::new(),
                prompt_paths: Vec::new(),
                extension_paths: vec![missing_path.to_string_lossy().to_string()],
                theme_paths: Vec::new(),
            };

            let err = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect_err("missing explicit CLI extension path should fail");
            assert!(
                err.to_string().contains("does not exist"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_resource_loader_rejects_missing_cli_skill_path() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let missing_path = temp_dir.path().join("missing-skill.md");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: false,
                no_prompt_templates: true,
                no_extensions: true,
                no_themes: true,
                skill_paths: vec![missing_path.to_string_lossy().to_string()],
                prompt_paths: Vec::new(),
                extension_paths: Vec::new(),
                theme_paths: Vec::new(),
            };

            let err = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect_err("missing explicit CLI skill path should fail");
            assert!(
                err.to_string().contains("does not exist"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_resource_loader_rejects_blank_cli_skill_path() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: false,
                no_prompt_templates: true,
                no_extensions: true,
                no_themes: true,
                skill_paths: vec!["   ".to_string()],
                prompt_paths: Vec::new(),
                extension_paths: Vec::new(),
                theme_paths: Vec::new(),
            };

            let err = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect_err("blank explicit CLI skill path should fail");
            assert!(
                err.to_string()
                    .contains("Explicit skill path must be non-empty"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_resource_loader_rejects_blank_cli_extension_source() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: true,
                no_prompt_templates: true,
                no_extensions: false,
                no_themes: true,
                skill_paths: Vec::new(),
                prompt_paths: Vec::new(),
                extension_paths: vec![" \t ".to_string()],
                theme_paths: Vec::new(),
            };

            let err = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect_err("blank explicit CLI extension source should fail");
            assert!(
                err.to_string()
                    .contains("Explicit extension source must be non-empty"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_resource_loader_rejects_missing_cli_prompt_path() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let missing_path = temp_dir.path().join("missing-prompt.md");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: true,
                no_prompt_templates: false,
                no_extensions: true,
                no_themes: true,
                skill_paths: Vec::new(),
                prompt_paths: vec![missing_path.to_string_lossy().to_string()],
                extension_paths: Vec::new(),
                theme_paths: Vec::new(),
            };

            let err = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect_err("missing explicit CLI prompt path should fail");
            assert!(
                err.to_string().contains("does not exist"),
                "unexpected error: {err}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_resource_loader_accepts_explicit_cli_prompt_alias_path() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let prompt_dir = temp_dir.path().join("prompts");
            fs::create_dir_all(&prompt_dir).expect("create prompt dir");
            let prompt_path = prompt_dir.join("review.md");
            fs::write(
                &prompt_path,
                "---\ndescription: Review prompt\n---\nReview body\n",
            )
            .expect("write prompt");
            let alias_path = temp_dir.path().join("review-alias.md");
            std::os::unix::fs::symlink(&prompt_path, &alias_path).expect("create prompt alias");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: true,
                no_prompt_templates: false,
                no_extensions: true,
                no_themes: true,
                skill_paths: Vec::new(),
                prompt_paths: vec![
                    prompt_path.to_string_lossy().to_string(),
                    alias_path.to_string_lossy().to_string(),
                ],
                extension_paths: Vec::new(),
                theme_paths: Vec::new(),
            };

            let loader = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect("load explicit prompt alias");
            assert_eq!(loader.prompts().len(), 1);
            assert_eq!(loader.prompts()[0].file_path, prompt_path);
            assert!(loader.prompt_diagnostics().is_empty());
        });
    }

    #[test]
    fn test_resource_loader_rejects_invalid_cli_skill_file() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let skill_dir = temp_dir.path().join("bad-skill");
            fs::create_dir_all(&skill_dir).expect("create skill dir");
            let skill_path = skill_dir.join("SKILL.md");
            fs::write(&skill_path, "# Missing frontmatter\n").expect("write skill");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: false,
                no_prompt_templates: true,
                no_extensions: true,
                no_themes: true,
                skill_paths: vec![skill_path.to_string_lossy().to_string()],
                prompt_paths: Vec::new(),
                extension_paths: Vec::new(),
                theme_paths: Vec::new(),
            };

            let err = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect_err("invalid explicit CLI skill file should fail");
            assert!(
                err.to_string().contains("description is required"),
                "unexpected error: {err}"
            );
        });
    }

    #[test]
    fn test_resource_loader_rejects_invalid_cli_theme_file() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let theme_path = temp_dir.path().join("broken.json");
            fs::write(&theme_path, "{not-json").expect("write theme");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: true,
                no_prompt_templates: true,
                no_extensions: true,
                no_themes: false,
                skill_paths: Vec::new(),
                prompt_paths: Vec::new(),
                extension_paths: Vec::new(),
                theme_paths: vec![theme_path.to_string_lossy().to_string()],
            };

            let err = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect_err("invalid explicit CLI theme file should fail");
            assert!(
                err.to_string().contains("could not be loaded"),
                "unexpected error: {err}"
            );
            assert!(
                err.to_string().contains("Failed to load theme"),
                "unexpected error: {err}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn test_resource_loader_accepts_explicit_cli_theme_alias_path() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let theme_dir = temp_dir.path().join("themes");
            fs::create_dir_all(&theme_dir).expect("create theme dir");
            let theme_path = theme_dir.join("dark.ini");
            fs::write(&theme_path, "[styles]\nbrand.accent = bold #38bdf8\n").expect("write theme");
            let alias_path = temp_dir.path().join("dark-alias.ini");
            std::os::unix::fs::symlink(&theme_path, &alias_path).expect("create theme alias");

            let manager = PackageManager::new(temp_dir.path().to_path_buf());
            let config = Config::default();
            let cli = ResourceCliOptions {
                no_skills: true,
                no_prompt_templates: true,
                no_extensions: true,
                no_themes: false,
                skill_paths: Vec::new(),
                prompt_paths: Vec::new(),
                extension_paths: Vec::new(),
                theme_paths: vec![
                    theme_path.to_string_lossy().to_string(),
                    alias_path.to_string_lossy().to_string(),
                ],
            };

            let loader = ResourceLoader::load(&manager, temp_dir.path(), &config, &cli)
                .await
                .expect("load explicit theme alias");
            assert_eq!(loader.themes().len(), 1);
            assert_eq!(loader.themes()[0].file_path, theme_path);
            assert!(loader.theme_diagnostics().is_empty());
        });
    }

    #[test]
    fn test_extension_paths_deduped_between_settings_and_cli() {
        run_async(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let extension_path = temp_dir.path().join("ext.native.json");
            fs::write(&extension_path, "{}").expect("write extension");

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
    fn test_dedupe_extension_entries_by_id_casefolds_cache_source_pairs() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let source_dir = temp_dir.path().join("source");
        let cache_dir = temp_dir.path().join("cache").join("modules");
        fs::create_dir_all(&source_dir).expect("create source dir");
        fs::create_dir_all(&cache_dir).expect("create cache dir");

        let source_entry = source_dir.join("Foo.ts");
        let cache_entry = cache_dir.join("foo.js");
        fs::write(&source_entry, "export default function init() {}\n")
            .expect("write source entry");
        fs::write(&cache_entry, "export default function init() {}\n").expect("write cache entry");

        let deduped = dedupe_extension_entries_by_id_with_cache_dir(
            vec![cache_entry, source_entry.clone()],
            Some(&cache_dir),
        );

        assert_eq!(
            deduped,
            vec![source_entry],
            "case-variant cache copy should be dropped in favor of the source entry"
        );
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
    fn test_extract_manifest_string_list_variants() {
        let temp = tempfile::tempdir().expect("tempdir");
        let manifest_path = temp.path().join("package.json");
        assert_eq!(
            extract_manifest_string_list(
                &manifest_path,
                "extensions",
                &Value::String("one".to_string())
            )
            .expect("single string should parse"),
            vec!["one".to_string()]
        );
        assert_eq!(
            extract_manifest_string_list(&manifest_path, "extensions", &json!(["one", "three"]))
                .expect("string arrays should parse"),
            vec!["one".to_string(), "three".to_string()]
        );
        let err = extract_manifest_string_list(&manifest_path, "extensions", &json!({"a": 1}))
            .expect_err("objects should be rejected");
        assert!(
            err.to_string()
                .contains("`pi.extensions` must be a string or array of strings")
        );
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

    #[test]
    fn test_read_dir_sorted_paths_returns_lexicographic_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("z.md"), "z").expect("write z");
        fs::write(temp.path().join("a.md"), "a").expect("write a");

        let names: Vec<String> = read_dir_sorted_paths(temp.path())
            .into_iter()
            .map(|path| {
                path.file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["a.md", "z.md"]);
    }

    #[test]
    fn test_precedence_sorted_enabled_paths_orders_by_documented_resource_priority() {
        let resources = vec![
            ResolvedResource {
                path: PathBuf::from("/global/package/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "pkg:user".to_string(),
                    scope: PackageScope::User,
                    origin: ResourceOrigin::Package,
                    base_dir: None,
                },
            },
            ResolvedResource {
                path: PathBuf::from("/project/.pi/prompts/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "local:project".to_string(),
                    scope: PackageScope::Project,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: None,
                },
            },
            ResolvedResource {
                path: PathBuf::from("/global/.pi/prompts/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "local:user".to_string(),
                    scope: PackageScope::User,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: None,
                },
            },
            ResolvedResource {
                path: PathBuf::from("/project/package/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "pkg:project".to_string(),
                    scope: PackageScope::Project,
                    origin: ResourceOrigin::Package,
                    base_dir: None,
                },
            },
            ResolvedResource {
                path: PathBuf::from("/tmp/cli-ext/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "cli-extension".to_string(),
                    scope: PackageScope::Temporary,
                    origin: ResourceOrigin::Package,
                    base_dir: None,
                },
            },
            ResolvedResource {
                path: PathBuf::from("/disabled/ignored.md"),
                enabled: false,
                metadata: crate::package_manager::PathMetadata {
                    source: "ignored".to_string(),
                    scope: PackageScope::Project,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: None,
                },
            },
        ];

        let sorted = precedence_sorted_enabled_paths(resources);
        assert_eq!(
            sorted,
            vec![
                PathBuf::from("/tmp/cli-ext/review.md"),
                PathBuf::from("/project/.pi/prompts/review.md"),
                PathBuf::from("/global/.pi/prompts/review.md"),
                PathBuf::from("/project/package/review.md"),
                PathBuf::from("/global/package/review.md"),
            ]
        );
    }

    #[test]
    fn test_precedence_sorted_enabled_paths_preserves_source_order_within_same_precedence() {
        let resources = vec![
            ResolvedResource {
                path: PathBuf::from("/tmp/cli-ext/zeta/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "cli-extension:zeta".to_string(),
                    scope: PackageScope::Temporary,
                    origin: ResourceOrigin::Package,
                    base_dir: None,
                },
            },
            ResolvedResource {
                path: PathBuf::from("/tmp/cli-ext/alpha/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "cli-extension:alpha".to_string(),
                    scope: PackageScope::Temporary,
                    origin: ResourceOrigin::Package,
                    base_dir: None,
                },
            },
            ResolvedResource {
                path: PathBuf::from("/project/.pi/prompts/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "local:project".to_string(),
                    scope: PackageScope::Project,
                    origin: ResourceOrigin::TopLevel,
                    base_dir: None,
                },
            },
        ];

        let sorted = precedence_sorted_enabled_paths(resources);
        assert_eq!(
            sorted,
            vec![
                PathBuf::from("/tmp/cli-ext/zeta/review.md"),
                PathBuf::from("/tmp/cli-ext/alpha/review.md"),
                PathBuf::from("/project/.pi/prompts/review.md"),
            ],
            "same-tier resources should keep their original source order"
        );
    }

    #[test]
    fn test_merge_resource_paths_keeps_explicit_cli_paths_first() {
        let explicit_path = PathBuf::from("/cli/direct/review.md");
        let merged = merge_resource_paths(
            std::slice::from_ref(&explicit_path),
            vec![ResolvedResource {
                path: PathBuf::from("/tmp/cli-ext/review.md"),
                enabled: true,
                metadata: crate::package_manager::PathMetadata {
                    source: "cli-extension".to_string(),
                    scope: PackageScope::Temporary,
                    origin: ResourceOrigin::Package,
                    base_dir: None,
                },
            }],
            vec![
                ResolvedResource {
                    path: PathBuf::from("/project/.pi/prompts/review.md"),
                    enabled: true,
                    metadata: crate::package_manager::PathMetadata {
                        source: "local:project".to_string(),
                        scope: PackageScope::Project,
                        origin: ResourceOrigin::TopLevel,
                        base_dir: None,
                    },
                },
                ResolvedResource {
                    path: PathBuf::from("/global/.pi/prompts/review.md"),
                    enabled: true,
                    metadata: crate::package_manager::PathMetadata {
                        source: "local:user".to_string(),
                        scope: PackageScope::User,
                        origin: ResourceOrigin::TopLevel,
                        base_dir: None,
                    },
                },
            ],
            true,
        );

        assert_eq!(
            merged,
            vec![
                explicit_path,
                PathBuf::from("/tmp/cli-ext/review.md"),
                PathBuf::from("/project/.pi/prompts/review.md"),
                PathBuf::from("/global/.pi/prompts/review.md"),
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

    #[test]
    fn test_parse_command_args_preserves_empty_quoted_args() {
        assert_eq!(parse_command_args("\"\""), vec![""]);
        assert_eq!(parse_command_args("''"), vec![""]);
        assert_eq!(
            parse_command_args("foo \"\" bar ''"),
            vec!["foo", "", "bar", ""]
        );
    }

    #[test]
    fn test_parse_command_args_preserves_apostrophes_inside_words() {
        assert_eq!(parse_command_args("it's fine"), vec!["it's", "fine"]);
        assert_eq!(
            parse_command_args("review o'brien's draft"),
            vec!["review", "o'brien's", "draft"]
        );
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
        let result = substitute_args("$0", &args);
        assert_eq!(result, "");
    }

    #[test]
    fn test_substitute_args_empty_args() {
        let result = substitute_args("$1 $@ $ARGUMENTS", &[]);
        assert_eq!(result, "  ");
    }

    #[test]
    fn panic_payload_message_handles_known_payload_types() {
        let string_payload: Box<dyn std::any::Any + Send + 'static> =
            Box::new("loader panic".to_string());
        assert_eq!(
            panic_payload_message(string_payload),
            "loader panic".to_string()
        );

        let str_payload: Box<dyn std::any::Any + Send + 'static> = Box::new("panic str");
        assert_eq!(panic_payload_message(str_payload), "panic str".to_string());
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

    #[test]
    fn test_expand_prompt_template_preserves_empty_positional_arguments() {
        let template = PromptTemplate {
            name: "review".to_string(),
            description: "review prompt".to_string(),
            content: "first=[$1] second=[$2] rest=[${@:2}]".to_string(),
            source: "test".to_string(),
            file_path: PathBuf::from("/review.md"),
        };

        let result = expand_prompt_template("/review \"\" foo", &[template]);
        assert_eq!(result, "first=[] second=[foo] rest=[foo]");
    }

    #[test]
    fn test_expand_prompt_template_preserves_trailing_empty_positional_arguments() {
        let template = PromptTemplate {
            name: "review".to_string(),
            description: "review prompt".to_string(),
            content: "first=[$1] second=[$2] third=[$3]".to_string(),
            source: "test".to_string(),
            file_path: PathBuf::from("/review.md"),
        };

        let result = expand_prompt_template("/review foo \"\"", &[template]);
        assert_eq!(result, "first=[foo] second=[] third=[]");
    }

    #[test]
    fn test_expand_prompt_template_preserves_repeated_empty_quoted_arguments() {
        let template = PromptTemplate {
            name: "review".to_string(),
            description: "review prompt".to_string(),
            content: "first=[$1] second=[$2] third=[$3] fourth=[$4]".to_string(),
            source: "test".to_string(),
            file_path: PathBuf::from("/review.md"),
        };

        let result = expand_prompt_template("/review foo \"\" \"\" bar", &[template]);
        assert_eq!(result, "first=[foo] second=[] third=[] fourth=[bar]");
    }

    #[test]
    fn test_expand_prompt_template_preserves_apostrophes_in_arguments() {
        let template = PromptTemplate {
            name: "review".to_string(),
            description: "review prompt".to_string(),
            content: "first=[$1] second=[$2]".to_string(),
            source: "test".to_string(),
            file_path: PathBuf::from("/review.md"),
        };

        let result = expand_prompt_template("/review it's fine", &[template]);
        assert_eq!(result, "first=[it's] second=[fine]");
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
        let re = regex::Regex::new(r"\d+").unwrap();
        let result = replace_regex("hello world", &re, |_| "num".to_string());
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_replace_regex_replaces_all_matches() {
        let re = regex::Regex::new(r"\d").unwrap();
        let result = replace_regex("a1b2c3", &re, |caps| format!("[{}]", &caps[0]));
        assert_eq!(result, "a[1]b[2]c[3]");
    }

    // ── package manifest parsing ──────────────────────────────────────

    #[test]
    fn test_read_pi_manifest_returns_none_when_package_json_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let pi = read_pi_manifest(tmp.path()).expect("missing package.json should not error");
        assert!(pi.is_none());
    }

    #[test]
    fn test_read_pi_manifest_errors_on_malformed_package_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest_path = tmp.path().join("package.json");
        fs::write(&manifest_path, "{ not valid json").expect("write malformed package.json");

        let err = read_pi_manifest(tmp.path()).expect_err("malformed package.json must error");
        let message = err.to_string();
        assert!(message.contains("Failed to parse package manifest"));
        assert!(message.contains(&manifest_path.display().to_string()));
    }

    #[test]
    fn test_read_pi_manifest_errors_when_pi_field_is_not_object() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest_path = tmp.path().join("package.json");
        fs::write(&manifest_path, r#"{"name":"pkg","pi":"not-an-object"}"#)
            .expect("write invalid pi manifest");

        let err = read_pi_manifest(tmp.path()).expect_err("non-object `pi` field must error");
        let message = err.to_string();
        assert!(message.contains("Invalid package manifest"));
        assert!(message.contains("`pi` must be an object"));
        assert!(message.contains(&manifest_path.display().to_string()));
    }

    #[test]
    fn test_read_pi_manifest_allows_default_fallback_when_pi_key_is_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let manifest_path = tmp.path().join("package.json");
        fs::write(&manifest_path, r#"{"name":"pkg","version":"1.0.0"}"#)
            .expect("write package.json");

        let pi = read_pi_manifest(tmp.path()).expect("missing `pi` key should not error");
        assert!(pi.is_none());
    }

    #[test]
    fn test_append_resources_from_manifest_errors_on_invalid_resource_entry_type() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pi = json!({
            "extensions": ["ok", 7]
        });

        let mut resources = PackageResources::default();
        let err = append_resources_from_manifest(&mut resources, tmp.path(), &pi)
            .expect_err("non-string manifest entries must error");
        assert!(
            err.to_string()
                .contains("`pi.extensions` must be a string or array of strings")
        );
    }

    #[test]
    fn test_append_resources_from_manifest_errors_on_outside_root_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pi = json!({
            "skills": "../outside/skills"
        });

        let mut resources = PackageResources::default();
        let err = append_resources_from_manifest(&mut resources, tmp.path(), &pi)
            .expect_err("outside-root manifest paths must error");
        assert!(
            err.to_string()
                .contains("`pi.skills` paths must stay within the package root")
        );
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

    #[cfg(unix)]
    #[test]
    fn test_load_skills_from_dir_ignores_symlink_cycles() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills_root = tmp.path().join("skills");
        let skill_dir = skills_root.join("my-skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Cyclic symlink guard test\n---\nBody",
        )
        .expect("write skill");

        let loop_link = skill_dir.join("loop");
        std::os::unix::fs::symlink(&skill_dir, &loop_link).expect("create symlink loop");

        let result = load_skills_from_dir(skills_root, "test".to_string(), true);
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "my-skill");
    }

    #[cfg(unix)]
    #[test]
    fn test_load_skills_ignores_alias_symlink_to_same_skill_tree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skills_root = tmp.path().join("skills");
        let real_root = skills_root.join("real");
        let skill_dir = real_root.join("my-skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Symlink alias guard test\n---\nBody",
        )
        .expect("write skill");

        std::os::unix::fs::symlink(&real_root, skills_root.join("alias"))
            .expect("create alias symlink");

        let result = load_skills(LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: tmp.path().join("agent"),
            skill_paths: vec![skills_root],
            include_defaults: false,
        });

        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "my-skill");
        assert!(result.diagnostics.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_load_skills_dedupes_diagnostics_across_alias_roots() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real_root = tmp.path().join("skills-real");
        let alias_root = tmp.path().join("skills-alias");
        let skill_dir = real_root.join("my-skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Alias diagnostic guard test\ninvalid-field: nope\n---\nBody",
        )
        .expect("write skill");

        std::os::unix::fs::symlink(&real_root, &alias_root).expect("create alias root");

        let result = load_skills(LoadSkillsOptions {
            cwd: tmp.path().to_path_buf(),
            agent_dir: tmp.path().join("agent"),
            skill_paths: vec![real_root, alias_root],
            include_defaults: false,
        });

        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "my-skill");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].path, skill_dir.join("SKILL.md"));
        assert!(
            result.diagnostics[0]
                .message
                .contains("unknown frontmatter field")
        );
    }

    #[test]
    fn test_load_skills_prefers_lexicographically_first_duplicate_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("skills");
        let z_skill = root.join("z").join("dup-skill");
        let a_skill = root.join("a").join("dup-skill");
        fs::create_dir_all(&z_skill).expect("create z skill dir");
        fs::create_dir_all(&a_skill).expect("create a skill dir");
        fs::write(
            z_skill.join("SKILL.md"),
            "---\nname: dup-skill\ndescription: z duplicate\n---\nZ body",
        )
        .expect("write z skill");
        fs::write(
            a_skill.join("SKILL.md"),
            "---\nname: dup-skill\ndescription: a duplicate\n---\nA body",
        )
        .expect("write a skill");

        let result = load_skills(LoadSkillsOptions {
            cwd: temp.path().to_path_buf(),
            agent_dir: temp.path().join("agent"),
            skill_paths: vec![root],
            include_defaults: false,
        });

        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].file_path, a_skill.join("SKILL.md"));
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0]
                .collision
                .as_ref()
                .expect("collision")
                .winner_path,
            a_skill.join("SKILL.md")
        );
    }

    #[test]
    fn test_load_themes_prefers_lexicographically_first_duplicate_stem() {
        let temp = tempfile::tempdir().expect("tempdir");
        let themes_dir = temp.path().join("themes");
        let dark_theme = themes_dir.join("dark.theme");
        let dark_ini = themes_dir.join("dark.ini");
        fs::create_dir_all(&themes_dir).expect("create themes dir");
        fs::write(&dark_theme, "#445566").expect("write theme");
        fs::write(&dark_ini, "#112233").expect("write ini");

        let loaded = load_themes(LoadThemesOptions {
            cwd: temp.path().to_path_buf(),
            agent_dir: temp.path().join("agent"),
            theme_paths: vec![themes_dir],
            include_defaults: false,
        });
        let (themes, diagnostics) = dedupe_themes(loaded.themes);

        assert_eq!(themes.len(), 1);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(themes[0].file_path, dark_ini);
        assert_eq!(
            diagnostics[0]
                .collision
                .as_ref()
                .expect("collision")
                .winner_path,
            dark_ini
        );
    }

    // ── Property tests ──────────────────────────────────────────────────

    mod proptest_resources {
        use super::*;
        use proptest::prelude::*;

        fn arb_valid_name() -> impl Strategy<Value = String> {
            "[a-z0-9]([a-z0-9]|(-[a-z0-9])){0,20}"
                .prop_filter("no consecutive hyphens", |s| !s.contains("--"))
        }

        proptest! {
            #[test]
            fn validate_name_accepts_valid_names(name in arb_valid_name()) {
                let errors = validate_name(&name, &name);
                assert!(
                    errors.is_empty(),
                    "valid name '{name}' should have no errors, got: {errors:?}"
                );
            }

            #[test]
            fn validate_name_rejects_uppercase(
                prefix in "[a-z]{1,5}",
                upper in "[A-Z]{1,3}",
                suffix in "[a-z]{1,5}",
            ) {
                let name = format!("{prefix}{upper}{suffix}");
                let errors = validate_name(&name, &name);
                assert!(
                    errors.iter().any(|e| e.contains("invalid characters")),
                    "uppercase in '{name}' should be rejected, got: {errors:?}"
                );
            }

            #[test]
            fn validate_name_rejects_leading_or_trailing_hyphen(
                core in "[a-z]{1,10}",
                leading in proptest::bool::ANY,
            ) {
                let name = if leading {
                    format!("-{core}")
                } else {
                    format!("{core}-")
                };
                let errors = validate_name(&name, &name);
                assert!(
                    errors.iter().any(|e| e.contains("must not start or end with a hyphen")),
                    "name '{name}' should fail hyphen check, got: {errors:?}"
                );
            }

            #[test]
            fn validate_name_rejects_consecutive_hyphens(
                left in "[a-z]{1,8}",
                right in "[a-z]{1,8}",
            ) {
                let name = format!("{left}--{right}");
                let errors = validate_name(&name, &name);
                assert!(
                    errors.iter().any(|e| e.contains("consecutive hyphens")),
                    "name '{name}' should fail consecutive-hyphen check, got: {errors:?}"
                );
            }

            #[test]
            fn validate_name_length_limit_enforced(extra_len in 1..100usize) {
                let name: String = "a".repeat(MAX_SKILL_NAME_LEN + extra_len);
                let errors = validate_name(&name, &name);
                assert!(
                    errors.iter().any(|e| e.contains("exceeds")),
                    "name of length {} should exceed limit, got: {errors:?}",
                    name.len()
                );
            }

            #[test]
            fn validate_description_accepts_within_limit(
                desc in "[a-zA-Z]{1,5}[a-zA-Z ]{0,95}",
            ) {
                let errors = validate_description(&desc);
                assert!(
                    errors.is_empty(),
                    "short description should be valid, got: {errors:?}"
                );
            }

            #[test]
            fn validate_description_rejects_over_limit(extra in 1..200usize) {
                let desc = "x".repeat(MAX_SKILL_DESC_LEN + extra);
                let errors = validate_description(&desc);
                assert!(
                    errors.iter().any(|e| e.contains("exceeds")),
                    "description of length {} should exceed limit",
                    desc.len()
                );
            }

            #[test]
            fn escape_xml_idempotent_on_safe_strings(s in "[a-zA-Z0-9 ]{0,50}") {
                assert_eq!(
                    escape_xml(&s), s,
                    "safe string should pass through unchanged"
                );
            }

            #[test]
            fn escape_xml_output_never_contains_raw_special_chars(s in ".*") {
                let escaped = escape_xml(&s);
                // After escaping, no raw `<`, `>`, `&` (except in escape sequences),
                // `"`, or `'` should remain unescaped.
                // We check that re-escaping is idempotent on the escaped output.
                // A simpler check: the escaped output, when re-escaped, should only
                // double-encode the `&` in existing entities.
                let double_escaped = escape_xml(&escaped);
                // If no raw specials in escaped, then double-escape only affects `&`
                // in entities like `&amp;` → `&amp;amp;`.
                // We just check the output doesn't contain bare `<` or `>`.
                assert!(
                    !escaped.contains('<') && !escaped.contains('>'),
                    "escaped output should not contain raw < or >: {escaped}"
                );
                let _ = double_escaped; // suppress unused warning
            }

            #[test]
            fn parse_command_args_round_trip_simple_tokens(
                tokens in prop::collection::vec("[a-zA-Z0-9]{1,10}", 0..8),
            ) {
                let input = tokens.join(" ");
                let parsed = parse_command_args(&input);
                assert_eq!(
                    parsed, tokens,
                    "simple space-separated tokens should round-trip"
                );
            }

            #[test]
            fn parse_command_args_quoted_preserves_spaces(
                before in "[a-z]{1,5}",
                inner in "[a-z ]{1,10}",
                after in "[a-z]{1,5}",
            ) {
                let input = format!("{before} \"{inner}\" {after}");
                let parsed = parse_command_args(&input);
                assert!(
                    parsed.contains(&inner),
                    "quoted token '{inner}' should appear in parsed output: {parsed:?}"
                );
            }

            #[test]
            fn substitute_args_positional_in_range(
                idx in 1..10usize,
                values in prop::collection::vec("[a-z]{1,5}", 1..10),
            ) {
                let template = format!("${idx}");
                let result = substitute_args(&template, &values);
                let expected = values.get(idx.saturating_sub(1)).cloned().unwrap_or_default();
                assert_eq!(
                    result, expected,
                    "positional ${idx} should resolve correctly"
                );
            }

            #[test]
            fn substitute_args_dollar_at_is_all_joined(
                values in prop::collection::vec("[a-z]{1,5}", 0..8),
            ) {
                let result = substitute_args("$@", &values);
                let expected = values.join(" ");
                assert_eq!(result, expected, "$@ should join all args");
            }

            #[test]
            fn substitute_args_arguments_equals_dollar_at(
                values in prop::collection::vec("[a-z]{1,5}", 0..8),
            ) {
                let r1 = substitute_args("$@", &values);
                let r2 = substitute_args("$ARGUMENTS", &values);
                assert_eq!(r1, r2, "$@ and $ARGUMENTS should be equivalent");
            }

            #[test]
            fn parse_frontmatter_no_dashes_returns_raw_body(
                body in "[a-zA-Z0-9 \n]{0,100}",
            ) {
                let parsed = parse_frontmatter(&body);
                assert!(
                    parsed.frontmatter.is_empty(),
                    "no --- means no frontmatter"
                );
                assert_eq!(parsed.body, body);
            }

            #[test]
            fn parse_frontmatter_unclosed_returns_raw(
                key in "[a-z]{1,8}",
                val in "[a-z]{1,8}",
            ) {
                let raw = format!("---\n{key}: {val}\nmore stuff");
                let parsed = parse_frontmatter(&raw);
                assert!(
                    parsed.frontmatter.is_empty(),
                    "unclosed frontmatter should return empty map"
                );
                assert_eq!(parsed.body, raw);
            }

            #[test]
            fn parse_frontmatter_closed_extracts_key_value(
                key in "[a-z]{1,8}",
                val in "[a-z]{1,8}",
                body in "[a-z ]{0,30}",
            ) {
                let raw = format!("---\n{key}: {val}\n---\n{body}");
                let parsed = parse_frontmatter(&raw);
                assert_eq!(
                    parsed.frontmatter.get(&key),
                    Some(&val),
                    "closed frontmatter should extract {key}: {val}"
                );
                assert_eq!(parsed.body, body);
            }

            #[test]
            fn resolve_path_absolute_is_identity(
                suffix in "[a-z]{1,10}(/[a-z]{1,10}){0,3}",
            ) {
                let abs = format!("/{suffix}");
                let cwd = Path::new("/some/cwd");
                let resolved = resolve_path(&abs, cwd);
                assert_eq!(
                    resolved,
                    PathBuf::from(&abs),
                    "absolute path should pass through unchanged"
                );
            }

            #[test]
            fn resolve_path_relative_is_under_cwd(
                rel in "[a-z]{1,10}(/[a-z]{1,10}){0,2}",
            ) {
                let cwd = Path::new("/work/dir");
                let resolved = resolve_path(&rel, cwd);
                assert!(
                    resolved.starts_with(cwd),
                    "relative path should resolve under cwd: {resolved:?}"
                );
            }

            #[test]
            fn dedupe_paths_preserves_first_and_removes_dups(
                paths in prop::collection::vec("[a-z]{1,5}", 1..20),
            ) {
                let path_bufs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
                let deduped = dedupe_paths(path_bufs.clone());

                // All elements in deduped should be unique
                let unique: HashSet<String> = deduped.iter()
                    .map(|p| p.to_string_lossy().to_string())
                    .collect();
                assert_eq!(
                    deduped.len(), unique.len(),
                    "deduped output must contain no duplicates"
                );

                // First occurrence order preserved
                let mut seen = HashSet::new();
                let expected: Vec<&PathBuf> = path_bufs.iter()
                    .filter(|p| seen.insert(p.to_string_lossy().to_string()))
                    .collect();
                assert_eq!(
                    deduped.iter().collect::<Vec<_>>(), expected,
                    "deduped must preserve first-occurrence order"
                );
            }
        }
    }
}
