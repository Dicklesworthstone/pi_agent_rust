#![allow(clippy::too_many_lines)]

mod common;

use common::{TestHarness, run_async};
use pi::package_manager::{
    PackageManager, PackageScope, ResolveRoots, ResolvedResource, ResourceOrigin,
};
use pi::resources::{
    DiagnosticKind, LoadPromptTemplatesOptions, LoadSkillsOptions, LoadThemesOptions,
    dedupe_prompts, dedupe_themes, load_prompt_templates, load_skills, load_themes,
};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn write_skill(
    harness: &TestHarness,
    skill_dir: &Path,
    name: &str,
    description: &str,
    extra_frontmatter: &str,
) -> PathBuf {
    let mut frontmatter = String::new();
    frontmatter.push_str("---\n");
    let _ = writeln!(frontmatter, "name: {name}");
    if !description.is_empty() {
        let _ = writeln!(frontmatter, "description: {description}");
    }
    if !extra_frontmatter.trim().is_empty() {
        frontmatter.push_str(extra_frontmatter.trim());
        frontmatter.push('\n');
    }
    frontmatter.push_str("---\n\nSkill body.\n");

    let skill_path = skill_dir.join("SKILL.md");
    let relative = skill_path
        .strip_prefix(harness.temp_dir())
        .expect("skill dir under temp dir")
        .to_path_buf();
    harness.create_file(relative, frontmatter.as_bytes())
}

fn write_prompt(harness: &TestHarness, path: &Path, raw: &str) -> PathBuf {
    let relative = path
        .strip_prefix(harness.temp_dir())
        .expect("prompt under temp dir");
    harness.create_file(relative, raw.as_bytes())
}

fn write_theme_ini(harness: &TestHarness, path: &Path, styles: &str) -> PathBuf {
    let content = format!("[styles]\n{styles}\n");
    let relative = path
        .strip_prefix(harness.temp_dir())
        .expect("theme under temp dir");
    harness.create_file(relative, content.as_bytes())
}

#[test]
fn load_skills_defaults_and_collision_diagnostics() {
    let harness = TestHarness::new("load_skills_defaults_and_collision_diagnostics");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let user_alpha_dir = agent_dir.join("skills").join("alpha");
    let project_alpha_dir = cwd.join(".pi").join("skills").join("alpha");
    let project_beta_dir = cwd.join(".pi").join("skills").join("beta");

    let user_alpha = write_skill(&harness, &user_alpha_dir, "alpha", "User alpha", "");
    let project_alpha = write_skill(&harness, &project_alpha_dir, "alpha", "Project alpha", "");
    let project_beta = write_skill(&harness, &project_beta_dir, "beta", "Project beta", "");

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: None,
        skill_paths: Vec::new(),
        include_defaults: true,
    });

    harness.log().info_ctx("skills", "loaded skills", |ctx| {
        ctx.push(("count".into(), result.skills.len().to_string()));
        ctx.push((
            "names".into(),
            result
                .skills
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(", "),
        ));
    });
    harness
        .log()
        .info_ctx("diagnostics", "load_skills diagnostics", |ctx| {
            ctx.push(("count".into(), result.diagnostics.len().to_string()));
            for (idx, diag) in result.diagnostics.iter().enumerate() {
                ctx.push((format!("diag_{idx}"), diag.message.clone()));
            }
        });

    assert_eq!(result.skills.len(), 2, "collision should de-dupe alpha");
    assert!(result.skills.iter().any(|s| s.name == "alpha"));
    assert!(result.skills.iter().any(|s| s.name == "beta"));

    let alpha = result
        .skills
        .iter()
        .find(|s| s.name == "alpha")
        .expect("alpha skill present");
    assert_eq!(alpha.file_path, project_alpha);

    let collision = result
        .diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::Collision)
        .expect("collision diagnostic present");
    assert!(collision.message.contains("alpha"));

    let info = collision.collision.as_ref().expect("collision info");
    assert_eq!(info.resource_type, "skill");
    assert_eq!(info.name, "alpha");
    assert_eq!(info.winner_path, project_alpha);
    assert_eq!(info.loser_path, user_alpha);

    // Sanity: project beta should be detected under defaults.
    assert!(project_beta.exists());
}

#[test]
fn load_skills_universal_defaults_preserve_scope_and_native_precedence() {
    let harness =
        TestHarness::new("load_skills_universal_defaults_preserve_scope_and_native_precedence");
    let cwd = harness.temp_path("project");
    let agent_dir = harness.temp_path("custom-agent");
    let home_dir = harness.temp_path("home");
    let explicit_dir = harness.temp_path("explicit");
    let tiers = [
        (cwd.join(".pi/skills"), "project"),
        (cwd.join(".agents/skills"), "project"),
        (agent_dir.join("skills"), "user"),
        (home_dir.join(".agents/skills"), "user"),
        (explicit_dir.clone(), "path"),
        (agent_dir.join("skills.managed"), "managed"),
    ];
    let names = [
        "all-scopes",
        "project-universal",
        "user-native",
        "user-universal",
        "explicit-only",
        "managed-only",
    ];

    // Every winner competes with all lower tiers, including a custom Pi user
    // directory unrelated to the home directory used for universal skills.
    for (first_tier, name) in names.iter().enumerate() {
        for (dir, _) in &tiers[first_tier..] {
            write_skill(&harness, &dir.join(name), name, "Skill description", "");
        }
    }

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: Some(home_dir),
        skill_paths: vec![explicit_dir],
        include_defaults: true,
    });

    assert_eq!(
        result
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        [
            "all-scopes",
            "explicit-only",
            "managed-only",
            "project-universal",
            "user-native",
            "user-universal",
        ],
    );
    assert_eq!(result.diagnostics.len(), 15);
    for (winner_tier, name) in names.iter().enumerate() {
        let expected_path = tiers[winner_tier].0.join(name).join("SKILL.md");
        let skill = result
            .skills
            .iter()
            .find(|skill| skill.name == *name)
            .expect("skill from winning tier");
        assert_eq!(skill.file_path, expected_path);
        assert_eq!(skill.source, tiers[winner_tier].1);
        for (loser_dir, _) in &tiers[winner_tier + 1..] {
            let loser_path = loser_dir.join(name).join("SKILL.md");
            assert!(result.diagnostics.iter().any(|diagnostic| {
                diagnostic.kind == DiagnosticKind::Collision
                    && diagnostic.collision.as_ref().is_some_and(|collision| {
                        collision.name == *name
                            && collision.winner_path == expected_path
                            && collision.loser_path == loser_path
                    })
            }));
        }
    }
}

#[test]
fn load_skills_reports_unknown_frontmatter_fields() {
    let harness = TestHarness::new("load_skills_reports_unknown_frontmatter_fields");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let gamma_dir = agent_dir.join("skills").join("gamma");
    let gamma = write_skill(&harness, &gamma_dir, "gamma", "Gamma skill", "bogus: yep\n");

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: None,
        skill_paths: vec![gamma_dir],
        include_defaults: false,
    });

    harness.log().info_ctx("skills", "loaded skills", |ctx| {
        ctx.push(("count".into(), result.skills.len().to_string()));
        for skill in &result.skills {
            ctx.push((
                format!("skill_{}", skill.name),
                skill.file_path.display().to_string(),
            ));
        }
    });
    harness
        .log()
        .info_ctx("diagnostics", "load_skills diagnostics", |ctx| {
            ctx.push(("count".into(), result.diagnostics.len().to_string()));
            for (idx, diag) in result.diagnostics.iter().enumerate() {
                ctx.push((format!("diag_{idx}"), diag.message.clone()));
            }
        });

    assert_eq!(result.skills.len(), 1);
    let skill = &result.skills[0];
    assert_eq!(skill.name, "gamma");
    assert_eq!(skill.file_path, gamma);
    assert_eq!(skill.source, "user");

    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message == "unknown frontmatter field \"bogus\""),
        "expected unknown-field warning"
    );
}

#[test]
fn load_skills_requires_description() {
    let harness = TestHarness::new("load_skills_requires_description");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let delta_dir = agent_dir.join("skills").join("delta");
    let _delta = write_skill(&harness, &delta_dir, "delta", "", "");

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: None,
        skill_paths: vec![delta_dir],
        include_defaults: false,
    });

    harness
        .log()
        .info_ctx("diagnostics", "load_skills diagnostics", |ctx| {
            ctx.push(("count".into(), result.diagnostics.len().to_string()));
            for (idx, diag) in result.diagnostics.iter().enumerate() {
                ctx.push((format!("diag_{idx}"), diag.message.clone()));
            }
        });

    assert!(result.skills.is_empty());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message == "description is required"),
        "expected missing-description warning"
    );
}

#[cfg(unix)]
#[test]
fn prompt_templates_ignore_alias_symlink_to_same_tree() {
    use std::os::unix::fs::symlink;

    let harness = TestHarness::new("prompt_templates_ignore_alias_symlink_to_same_tree");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let prompts_root = harness.temp_path("prompt-roots");
    let real_root = prompts_root.join("real");
    std::fs::create_dir_all(&real_root).expect("create real prompt root");

    let real_prompt = write_prompt(
        &harness,
        &real_root.join("review.md"),
        "---\ndescription: Review prompt\n---\nReview body.\n",
    );
    let alias_root = prompts_root.join("alias");
    symlink(&real_root, &alias_root).expect("create alias symlink");

    let templates = load_prompt_templates(LoadPromptTemplatesOptions {
        cwd,
        agent_dir,
        prompt_paths: vec![real_root, alias_root],
        include_defaults: false,
    });
    let (deduped, diagnostics) = dedupe_prompts(templates);

    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].name, "review");
    assert_eq!(deduped[0].file_path, real_prompt);
    assert!(
        diagnostics.is_empty(),
        "alias symlink to same prompt tree should not report a collision"
    );
}

#[test]
fn prompt_template_description_and_collision_diagnostics() {
    let harness = TestHarness::new("prompt_template_description_and_collision_diagnostics");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let user_plan = write_prompt(
        &harness,
        &agent_dir.join("prompts").join("plan.md"),
        "This is the user plan template.\n\nMore body.\n",
    );
    let project_plan = write_prompt(
        &harness,
        &cwd.join(".pi").join("prompts").join("plan.md"),
        "---\ndescription: Project plan\n---\nProject body.\n",
    );

    let templates = load_prompt_templates(LoadPromptTemplatesOptions {
        cwd,
        agent_dir,
        prompt_paths: vec![user_plan.clone(), project_plan.clone()],
        include_defaults: false,
    });

    let (deduped, diagnostics) = dedupe_prompts(templates);

    harness.log().info_ctx("prompts", "deduped prompts", |ctx| {
        ctx.push(("count".into(), deduped.len().to_string()));
        for prompt in &deduped {
            ctx.push((
                format!("prompt_{}", prompt.name),
                prompt.file_path.display().to_string(),
            ));
            ctx.push((
                format!("prompt_desc_{}", prompt.name),
                prompt.description.clone(),
            ));
        }
    });
    harness
        .log()
        .info_ctx("diagnostics", "dedupe_prompts diagnostics", |ctx| {
            ctx.push(("count".into(), diagnostics.len().to_string()));
            for (idx, diag) in diagnostics.iter().enumerate() {
                ctx.push((format!("diag_{idx}"), diag.message.clone()));
            }
        });

    assert_eq!(deduped.len(), 1);
    let plan = deduped
        .iter()
        .find(|p| p.name == "plan")
        .expect("plan template present");
    assert_eq!(plan.file_path, user_plan);
    assert_eq!(plan.source, "user");
    assert!(
        plan.description.contains("(user)"),
        "expected user label suffix"
    );

    let collision = diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::Collision)
        .expect("prompt collision diagnostic present");
    let info = collision.collision.as_ref().expect("collision info");
    assert_eq!(info.resource_type, "prompt");
    assert_eq!(info.name, "plan");
    assert_eq!(info.winner_path, user_plan);
    assert_eq!(info.loser_path, project_plan);
}

#[cfg(unix)]
#[test]
fn themes_ignore_alias_symlink_to_same_tree() {
    use std::os::unix::fs::symlink;

    let harness = TestHarness::new("themes_ignore_alias_symlink_to_same_tree");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let themes_root = harness.temp_path("theme-roots");
    let real_root = themes_root.join("real");
    std::fs::create_dir_all(&real_root).expect("create real theme root");

    let real_theme = write_theme_ini(
        &harness,
        &real_root.join("dark.ini"),
        "brand.accent = bold #38bdf8",
    );
    let alias_root = themes_root.join("alias");
    symlink(&real_root, &alias_root).expect("create alias symlink");

    let result = load_themes(LoadThemesOptions {
        cwd,
        agent_dir,
        theme_paths: vec![real_root, alias_root],
        include_defaults: false,
    });
    let (deduped, diagnostics) = dedupe_themes(result.themes);

    assert_eq!(deduped.len(), 1);
    assert_eq!(deduped[0].name, "dark");
    assert_eq!(deduped[0].file_path, real_theme);
    assert!(
        diagnostics.is_empty(),
        "alias symlink to same theme tree should not report a collision"
    );
}

#[test]
fn themes_load_ini_and_dedupe_collisions() {
    let harness = TestHarness::new("themes_load_ini_and_dedupe_collisions");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let user_dark = write_theme_ini(
        &harness,
        &agent_dir.join("themes").join("dark.ini"),
        "brand.accent = bold #38bdf8",
    );
    let project_dark = write_theme_ini(
        &harness,
        &cwd.join(".pi").join("themes").join("dark.ini"),
        "brand.accent = bold #38bdf8",
    );

    let result = load_themes(LoadThemesOptions {
        cwd,
        agent_dir,
        theme_paths: vec![user_dark.clone(), project_dark.clone()],
        include_defaults: false,
    });

    harness.log().info_ctx("themes", "loaded themes", |ctx| {
        ctx.push(("count".into(), result.themes.len().to_string()));
        for theme in &result.themes {
            ctx.push((
                format!("theme_{}", theme.name),
                theme.file_path.display().to_string(),
            ));
            ctx.push((format!("theme_src_{}", theme.name), theme.source.clone()));
        }
    });

    assert!(
        result.diagnostics.is_empty(),
        "expected no diagnostics for valid themes"
    );
    assert_eq!(result.themes.len(), 2);

    let (deduped, diagnostics) = dedupe_themes(result.themes);
    assert_eq!(deduped.len(), 1);
    let dark = deduped
        .iter()
        .find(|t| t.name == "dark")
        .expect("dark theme present");
    assert_eq!(dark.file_path, user_dark);

    let collision = diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::Collision)
        .expect("theme collision diagnostic present");
    let info = collision.collision.as_ref().expect("collision info");
    assert_eq!(info.resource_type, "theme");
    assert_eq!(info.name, "dark");
    assert_eq!(info.winner_path, user_dark);
    assert_eq!(info.loser_path, project_dark);
}

#[test]
fn themes_invalid_ini_emits_warning() {
    let harness = TestHarness::new("themes_invalid_ini_emits_warning");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let broken = write_theme_ini(
        &harness,
        &agent_dir.join("themes").join("broken.ini"),
        "brand.accent = #zzzzzz",
    );

    let result = load_themes(LoadThemesOptions {
        cwd,
        agent_dir,
        theme_paths: vec![broken],
        include_defaults: false,
    });

    harness
        .log()
        .info_ctx("diagnostics", "load_themes diagnostics", |ctx| {
            ctx.push(("count".into(), result.diagnostics.len().to_string()));
            for (idx, diag) in result.diagnostics.iter().enumerate() {
                ctx.push((format!("diag_{idx}"), diag.message.clone()));
            }
        });

    assert!(result.themes.is_empty());
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::Warning),
        "expected warning diagnostic"
    );
}

fn fixture_source_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("resource_loader")
        .join(name)
}

fn copy_fixture_dir(src: &Path, dest: &Path) {
    std::fs::create_dir_all(dest).expect("create fixture dest");
    let entries = std::fs::read_dir(src).expect("read fixture dir");
    for entry in entries {
        let entry = entry.expect("fixture entry");
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        let meta = entry.metadata().expect("fixture metadata");
        if meta.is_dir() {
            copy_fixture_dir(&src_path, &dest_path);
        } else if meta.is_file() {
            std::fs::copy(&src_path, &dest_path).expect("copy fixture file");
        }
    }
}

fn precedence_sorted_paths(resources: &[ResolvedResource]) -> Vec<PathBuf> {
    let mut enabled = resources
        .iter()
        .filter(|resource| resource.enabled)
        .cloned()
        .collect::<Vec<_>>();
    enabled.sort_by(|left, right| {
        resource_precedence(left)
            .cmp(&resource_precedence(right))
            .then_with(|| {
                left.path
                    .to_string_lossy()
                    .cmp(&right.path.to_string_lossy())
            })
    });
    enabled.into_iter().map(|resource| resource.path).collect()
}

const fn resource_precedence(resource: &ResolvedResource) -> u8 {
    match (resource.metadata.scope, resource.metadata.origin) {
        (PackageScope::Temporary, _) => 0,
        (PackageScope::Project, ResourceOrigin::TopLevel) => 1,
        (PackageScope::Project, ResourceOrigin::Universal) => 2,
        (PackageScope::User, ResourceOrigin::TopLevel) => 3,
        (PackageScope::User, ResourceOrigin::Universal) => 4,
        (PackageScope::Project, ResourceOrigin::Package) => 5,
        (PackageScope::User, ResourceOrigin::Package) => 6,
        (_, ResourceOrigin::Managed) => 7,
    }
}

#[test]
fn resolved_paths_feed_skill_and_prompt_loaders_with_collision_diagnostics() {
    let harness =
        TestHarness::new("resolved_paths_feed_skill_and_prompt_loaders_with_collision_diagnostics");

    let src_root = fixture_source_dir("resolve_basic");
    let dest_root = harness.temp_path("fixture");
    copy_fixture_dir(&src_root, &dest_root);

    let cwd = dest_root.join("project");
    let global_base_dir = dest_root.join("global");
    let project_base_dir = cwd.join(".pi");

    let manager = PackageManager::new(cwd.clone());
    let roots = ResolveRoots {
        project_settings_enabled: true,
        home_dir: None,
        global_settings_path: global_base_dir.join("settings.json"),
        project_settings_path: project_base_dir.join("settings.json"),
        global_base_dir: global_base_dir.clone(),
        project_base_dir: project_base_dir.clone(),
    };

    let resolved = run_async(async move { manager.resolve_with_roots(&roots).await })
        .expect("resolve_with_roots");

    let skill_paths = precedence_sorted_paths(&resolved.skills);
    let skills_result = load_skills(LoadSkillsOptions {
        cwd: cwd.clone(),
        agent_dir: global_base_dir.clone(),
        home_dir: None,
        skill_paths,
        include_defaults: false,
    });

    harness.log().info_ctx("skills", "loaded skills", |ctx| {
        ctx.push(("count".into(), skills_result.skills.len().to_string()));
        for skill in &skills_result.skills {
            ctx.push((skill.name.clone(), skill.file_path.display().to_string()));
        }
    });
    harness
        .log()
        .info_ctx("skills", "load_skills diagnostics", |ctx| {
            ctx.push(("count".into(), skills_result.diagnostics.len().to_string()));
            for (idx, diag) in skills_result.diagnostics.iter().enumerate() {
                ctx.push((format!("diag_{idx}"), diag.message.clone()));
            }
        });

    let alpha = skills_result
        .skills
        .iter()
        .find(|s| s.name == "alpha")
        .expect("alpha skill present");
    assert!(
        alpha
            .file_path
            .ends_with(Path::new("project/.pi/skills/alpha/SKILL.md")),
        "expected project alpha skill to win collision"
    );
    assert!(
        skills_result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::Collision && d.message.contains("alpha")),
        "expected alpha collision diagnostic"
    );

    let prompt_paths = precedence_sorted_paths(&resolved.prompts);
    let templates = load_prompt_templates(LoadPromptTemplatesOptions {
        cwd,
        agent_dir: global_base_dir,
        prompt_paths,
        include_defaults: false,
    });
    let (prompts, diagnostics) = dedupe_prompts(templates);

    harness.log().info_ctx("prompts", "deduped prompts", |ctx| {
        ctx.push(("count".into(), prompts.len().to_string()));
        for prompt in &prompts {
            ctx.push((prompt.name.clone(), prompt.file_path.display().to_string()));
        }
    });
    harness
        .log()
        .info_ctx("prompts", "dedupe_prompts diagnostics", |ctx| {
            ctx.push(("count".into(), diagnostics.len().to_string()));
            for (idx, diag) in diagnostics.iter().enumerate() {
                ctx.push((format!("diag_{idx}"), diag.message.clone()));
            }
        });

    let collision = prompts
        .iter()
        .find(|p| p.name == "collision")
        .expect("collision prompt present");
    assert!(
        collision
            .file_path
            .ends_with(Path::new("project/.pi/prompts/collision.md")),
        "expected project collision prompt to win"
    );

    let collision_diag = diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::Collision)
        .expect("collision diagnostic present");
    let info = collision_diag.collision.as_ref().expect("collision info");
    assert_eq!(info.resource_type, "prompt");
    assert_eq!(info.name, "collision");
    assert!(
        info.winner_path
            .ends_with(Path::new("project/.pi/prompts/collision.md")),
        "expected winner path to be project collision prompt"
    );
    assert!(
        info.loser_path
            .ends_with(Path::new("global/prompts/collision.md")),
        "expected loser path to be global collision prompt"
    );
}
