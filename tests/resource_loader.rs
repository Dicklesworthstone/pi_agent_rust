#![allow(clippy::too_many_lines)]

mod common;

use common::TestHarness;
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
    assert_eq!(alpha.file_path, user_alpha);

    let collision = result
        .diagnostics
        .iter()
        .find(|d| d.kind == DiagnosticKind::Collision)
        .expect("collision diagnostic present");
    assert!(collision.message.contains("alpha"));

    let info = collision.collision.as_ref().expect("collision info");
    assert_eq!(info.resource_type, "skill");
    assert_eq!(info.name, "alpha");
    assert_eq!(info.winner_path, user_alpha);
    assert_eq!(info.loser_path, project_alpha);

    // Sanity: project beta should be detected under defaults.
    assert!(project_beta.exists());
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
