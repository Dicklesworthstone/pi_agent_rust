//! Extended tests for resource loading edge cases: empty directories,
//! missing skill files, no-default loading, empty dedup inputs, and
//! multiple collision chains.
//!
//! Run:
//! ```bash
//! dsr quality --tool pi_agent_rust
//! ```

#![allow(clippy::too_many_lines)]
#![allow(clippy::similar_names)]

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

// ─── Skills ──────────────────────────────────────────────────────────────────

#[test]
fn load_skills_empty_directories_returns_empty() {
    let harness = TestHarness::new("load_skills_empty_directories_returns_empty");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: Some(harness.temp_path("missing-home")),
        skill_paths: Vec::new(),
        include_defaults: true,
    });

    assert!(result.skills.is_empty(), "Expected no skills");
    assert!(result.diagnostics.is_empty(), "Expected no diagnostics");
}

#[test]
fn load_skills_nonexistent_skill_path_ignored() {
    let harness = TestHarness::new("load_skills_nonexistent_skill_path_ignored");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: None,
        skill_paths: vec![harness.temp_path("nonexistent/skill_dir")],
        include_defaults: false,
    });

    assert!(
        result.skills.is_empty(),
        "Expected no skills for missing path"
    );
}

#[test]
fn load_skills_no_defaults_only_loads_explicit_paths() {
    let harness = TestHarness::new("load_skills_no_defaults_only_loads_explicit_paths");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");
    let home_dir = harness.temp_path("home");

    for default_dir in [
        cwd.join(".pi/skills"),
        cwd.join(".agents/skills"),
        agent_dir.join("skills"),
        home_dir.join(".agents/skills"),
        agent_dir.join("skills.managed"),
    ] {
        write_skill(
            &harness,
            &default_dir.join("unselected"),
            "unselected",
            "Must not be discovered",
            "",
        );
    }

    // Create skill in explicit path
    let explicit_dir = harness.temp_path("explicit_skills/myskill");
    let explicit = write_skill(&harness, &explicit_dir, "myskill", "Explicit skill", "");
    let selected_user = write_skill(
        &harness,
        &home_dir.join(".agents/skills/selected"),
        "selected",
        "Explicit user skill wins by caller order",
        "",
    );
    let selected_project = write_skill(
        &harness,
        &cwd.join(".agents/skills/selected"),
        "selected",
        "Explicit project skill is second",
        "",
    );

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: Some(home_dir),
        skill_paths: vec![
            explicit_dir,
            selected_user.clone(),
            selected_project.clone(),
        ],
        include_defaults: false,
    });

    assert_eq!(result.skills.len(), 2);
    assert_eq!(result.skills[0].name, "myskill");
    assert_eq!(result.skills[0].file_path, explicit);
    assert_eq!(result.skills[1].name, "selected");
    assert_eq!(result.skills[1].file_path, selected_user);
    assert_eq!(result.skills[1].source, "user");
    assert_eq!(result.diagnostics.len(), 1);
    let collision = result.diagnostics[0]
        .collision
        .as_ref()
        .expect("explicit duplicate reports a collision");
    assert_eq!(collision.winner_path, selected_user);
    assert_eq!(collision.loser_path, selected_project);
}

#[test]
fn load_skills_multiple_explicit_paths() {
    let harness = TestHarness::new("load_skills_multiple_explicit_paths");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let skill_a_dir = harness.temp_path("skills/alpha");
    let skill_b_dir = harness.temp_path("skills/beta");
    write_skill(&harness, &skill_a_dir, "alpha", "Alpha skill", "");
    write_skill(&harness, &skill_b_dir, "beta", "Beta skill", "");

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: None,
        skill_paths: vec![skill_a_dir, skill_b_dir],
        include_defaults: false,
    });

    assert_eq!(result.skills.len(), 2);
    assert!(result.skills.iter().any(|s| s.name == "alpha"));
    assert!(result.skills.iter().any(|s| s.name == "beta"));
}

#[test]
fn load_skills_with_disable_model_invocation_flag() {
    let harness = TestHarness::new("load_skills_with_disable_model_invocation_flag");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let skill_dir = harness.temp_path("skills/no_model");
    write_skill(
        &harness,
        &skill_dir,
        "no_model",
        "Skill with model invocation disabled",
        "disable-model-invocation: true\n",
    );

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: None,
        skill_paths: vec![skill_dir],
        include_defaults: false,
    });

    assert_eq!(result.skills.len(), 1);
    assert!(result.skills[0].disable_model_invocation);
}

#[test]
fn universal_skills_reject_invalid_files_without_hiding_valid_skills() {
    let harness =
        TestHarness::new("universal_skills_reject_invalid_files_without_hiding_valid_skills");
    let cwd = harness.temp_path("project");
    let home_dir = harness.temp_path("home");
    let healthy = write_skill(
        &harness,
        &cwd.join(".agents/skills/healthy"),
        "healthy",
        "Valid universal skill",
        "",
    );
    let malformed = harness.create_file(
        "project/.agents/skills/malformed/SKILL.md",
        b"---\nname: malformed\n---\nMissing required description.\n",
    );
    let non_utf8 = harness.create_file("home/.agents/skills/non-utf8/SKILL.md", [0xff, 0xfe, 0x00]);
    let oversized = harness.create_file(
        "home/.agents/skills/oversized/SKILL.md",
        vec![b'x'; 1024 * 1024 + 1],
    );

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir: harness.temp_path("agent"),
        home_dir: Some(home_dir),
        skill_paths: Vec::new(),
        include_defaults: true,
    });

    assert_eq!(result.skills.len(), 1);
    assert_eq!(result.skills[0].file_path, healthy);
    assert_eq!(result.diagnostics.len(), 3);
    for (path, message) in [
        (malformed, "description is required"),
        (non_utf8, "is not valid UTF-8"),
        (oversized, "exceeds the 1048576-byte resource limit"),
    ] {
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.kind == DiagnosticKind::Warning
                && diagnostic.path == path
                && diagnostic.message.contains(message)
        }));
    }
}

#[test]
fn universal_skill_discovery_stays_within_selected_roots() {
    let harness = TestHarness::new("universal_skill_discovery_stays_within_selected_roots");
    let cwd = harness.temp_path("workspace/project");
    let home_dir = harness.temp_path("home");
    let visible = write_skill(
        &harness,
        &cwd.join(".agents/skills/group/visible"),
        "visible",
        "Nested project skill",
        "",
    );
    let personal = write_skill(
        &harness,
        &home_dir.join(".agents/skills/personal"),
        "personal",
        "User skill",
        "",
    );
    for ignored_dir in [
        harness.temp_path("workspace/.agents/skills/ignored"),
        cwd.join("nested/.agents/skills/ignored"),
        cwd.join(".agents/unrelated/ignored"),
        cwd.join(".agents/skills/.hidden/ignored"),
        cwd.join(".agents/skills/node_modules/ignored"),
        home_dir.join("unrelated/.agents/skills/ignored"),
    ] {
        write_skill(&harness, &ignored_dir, "ignored", "Must not be scanned", "");
    }
    let options = LoadSkillsOptions {
        cwd,
        agent_dir: harness.temp_path("custom-agent"),
        home_dir: Some(home_dir),
        skill_paths: Vec::new(),
        include_defaults: true,
    };

    let result = load_skills(options.clone());
    assert_eq!(result.skills.len(), 2);
    assert_eq!(result.skills[0].file_path, personal);
    assert_eq!(result.skills[1].file_path, visible);
    assert!(result.diagnostics.is_empty());

    // An embedding caller can deliberately disable universal user discovery
    // without changing process-wide HOME or losing project skills.
    let project_only = load_skills(LoadSkillsOptions {
        home_dir: None,
        ..options
    });
    assert_eq!(project_only.skills.len(), 1);
    assert_eq!(project_only.skills[0].file_path, visible);
    assert!(project_only.diagnostics.is_empty());
}

#[test]
fn universal_skill_name_collisions_have_a_deterministic_winner() {
    let harness = TestHarness::new("universal_skill_name_collisions_have_a_deterministic_winner");
    let cwd = harness.temp_path("project");
    let root = cwd.join(".agents/skills");
    // Create the later path first so creation order cannot decide the winner.
    let later = write_skill(
        &harness,
        &root.join("zeta/shared"),
        "shared",
        "Later path",
        "",
    );
    let first = write_skill(
        &harness,
        &root.join("alpha/shared"),
        "shared",
        "First path",
        "",
    );
    let options = LoadSkillsOptions {
        cwd,
        agent_dir: harness.temp_path("agent"),
        home_dir: None,
        skill_paths: Vec::new(),
        include_defaults: true,
    };
    for result in [load_skills(options.clone()), load_skills(options)] {
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].file_path, first);
        assert_eq!(result.skills[0].source, "project");
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].kind, DiagnosticKind::Collision);
        let collision = result.diagnostics[0]
            .collision
            .as_ref()
            .expect("collision details");
        assert_eq!(collision.name, "shared");
        assert_eq!(collision.winner_path, first);
        assert_eq!(collision.loser_path, later);
    }
}

#[cfg(unix)]
#[test]
fn universal_skills_deduplicate_alias_roots_and_stop_symlink_cycles() {
    use std::os::unix::fs::symlink;

    let harness =
        TestHarness::new("universal_skills_deduplicate_alias_roots_and_stop_symlink_cycles");
    let cwd = harness.temp_path("project");
    let home_dir = harness.temp_path("home");
    let universal_root = cwd.join(".agents/skills");
    let original = write_skill(
        &harness,
        &universal_root.join("shared"),
        "shared",
        "Shared definition",
        "",
    );
    write_skill(
        &harness,
        &universal_root.join("malformed"),
        "malformed",
        "",
        "",
    );
    std::fs::create_dir_all(cwd.join(".pi")).expect("create Pi root");
    std::fs::create_dir_all(home_dir.join(".agents")).expect("create user root");
    symlink(&universal_root, cwd.join(".pi/skills")).expect("native alias");
    symlink(&universal_root, home_dir.join(".agents/skills")).expect("user alias");
    symlink(&universal_root, universal_root.join("cycle")).expect("directory cycle");
    symlink(cwd.join("missing-target"), universal_root.join("dangling")).expect("dangling symlink");

    let result = load_skills(LoadSkillsOptions {
        cwd: cwd.clone(),
        agent_dir: harness.temp_path("agent"),
        home_dir: Some(home_dir),
        skill_paths: vec![universal_root, original],
        include_defaults: true,
    });

    assert_eq!(result.skills.len(), 1);
    assert_eq!(
        result.skills[0].file_path,
        cwd.join(".pi/skills/shared/SKILL.md")
    );
    assert_eq!(result.skills[0].source, "project");
    assert_eq!(result.diagnostics.len(), 1);
    assert_eq!(result.diagnostics[0].kind, DiagnosticKind::Warning);
    assert_eq!(result.diagnostics[0].message, "description is required");
}

// ─── Prompt Templates ────────────────────────────────────────────────────────

#[test]
fn load_prompt_templates_empty_dirs_returns_empty() {
    let harness = TestHarness::new("load_prompt_templates_empty_dirs_returns_empty");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let templates = load_prompt_templates(LoadPromptTemplatesOptions {
        cwd,
        agent_dir,
        prompt_paths: Vec::new(),
        include_defaults: false,
    });

    assert!(templates.is_empty(), "Expected no templates");
}

#[test]
fn load_prompt_templates_simple_markdown_file() {
    let harness = TestHarness::new("load_prompt_templates_simple_markdown_file");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let prompt_path = agent_dir.join("prompts").join("review.md");
    write_prompt(
        &harness,
        &prompt_path,
        "---\ndescription: Code review\n---\nReview this code.\n",
    );

    let templates = load_prompt_templates(LoadPromptTemplatesOptions {
        cwd,
        agent_dir,
        prompt_paths: vec![prompt_path],
        include_defaults: false,
    });

    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "review");
    assert!(templates[0].description.contains("Code review"));
}

#[test]
fn dedupe_prompts_no_duplicates_returns_all() {
    let harness = TestHarness::new("dedupe_prompts_no_duplicates_returns_all");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let path_a = agent_dir.join("prompts").join("alpha.md");
    let path_b = agent_dir.join("prompts").join("beta.md");
    write_prompt(&harness, &path_a, "Alpha prompt body.\n");
    write_prompt(&harness, &path_b, "Beta prompt body.\n");

    let templates = load_prompt_templates(LoadPromptTemplatesOptions {
        cwd,
        agent_dir,
        prompt_paths: vec![path_a, path_b],
        include_defaults: false,
    });

    let (deduped, diagnostics) = dedupe_prompts(templates);
    assert_eq!(deduped.len(), 2);
    assert!(diagnostics.is_empty());
}

#[test]
fn dedupe_prompts_empty_input_returns_empty() {
    let (deduped, diagnostics) = dedupe_prompts(Vec::new());
    assert!(deduped.is_empty());
    assert!(diagnostics.is_empty());
}

// ─── Themes ──────────────────────────────────────────────────────────────────

#[test]
fn load_themes_empty_dirs_returns_empty() {
    let harness = TestHarness::new("load_themes_empty_dirs_returns_empty");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let result = load_themes(LoadThemesOptions {
        cwd,
        agent_dir,
        theme_paths: Vec::new(),
        include_defaults: false,
    });

    assert!(result.themes.is_empty(), "Expected no themes");
    assert!(result.diagnostics.is_empty());
}

#[test]
fn load_themes_single_valid_theme() {
    let harness = TestHarness::new("load_themes_single_valid_theme");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let theme_path = agent_dir.join("themes").join("ocean.ini");
    write_theme_ini(&harness, &theme_path, "brand.accent = bold #0ea5e9");

    let result = load_themes(LoadThemesOptions {
        cwd,
        agent_dir,
        theme_paths: vec![theme_path],
        include_defaults: false,
    });

    assert_eq!(result.themes.len(), 1);
    assert_eq!(result.themes[0].name, "ocean");
    assert!(result.diagnostics.is_empty());
}

#[test]
fn dedupe_themes_empty_input_returns_empty() {
    let (deduped, diagnostics) = dedupe_themes(Vec::new());
    assert!(deduped.is_empty());
    assert!(diagnostics.is_empty());
}

#[test]
fn dedupe_themes_case_insensitive_collision() {
    let harness = TestHarness::new("dedupe_themes_case_insensitive_collision");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    // Create two themes with same name different case
    let path_lower = agent_dir.join("themes").join("dark.ini");
    let path_upper = cwd.join(".pi").join("themes").join("Dark.ini");
    write_theme_ini(&harness, &path_lower, "brand.accent = bold #38bdf8");
    write_theme_ini(&harness, &path_upper, "brand.accent = bold #facc15");

    let result = load_themes(LoadThemesOptions {
        cwd,
        agent_dir,
        theme_paths: vec![path_lower, path_upper],
        include_defaults: false,
    });

    let (deduped, diagnostics) = dedupe_themes(result.themes);
    // Should deduplicate to 1
    assert_eq!(deduped.len(), 1);
    // Should produce a collision diagnostic
    assert!(
        diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::Collision),
        "Expected collision diagnostic for case-insensitive match"
    );
}

// ─── Skills with defaults enabled ────────────────────────────────────────────

#[test]
fn load_skills_with_defaults_includes_user_and_project_dirs() {
    let harness = TestHarness::new("load_skills_with_defaults_includes_user_and_project_dirs");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    // User-level skill
    let user_skill_dir = agent_dir.join("skills").join("user_skill");
    write_skill(&harness, &user_skill_dir, "user_skill", "User skill", "");

    // Project-level skill
    let project_skill_dir = cwd.join(".pi").join("skills").join("project_skill");
    write_skill(
        &harness,
        &project_skill_dir,
        "project_skill",
        "Project skill",
        "",
    );

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: None,
        skill_paths: Vec::new(),
        include_defaults: true,
    });

    assert!(
        result.skills.iter().any(|s| s.name == "user_skill"),
        "Expected user_skill from agent dir"
    );
    assert!(
        result.skills.iter().any(|s| s.name == "project_skill"),
        "Expected project_skill from project dir"
    );
}

// ─── Multiple unknown frontmatter fields ─────────────────────────────────────

#[test]
fn load_skills_reports_all_unknown_frontmatter_fields() {
    let harness = TestHarness::new("load_skills_reports_all_unknown_frontmatter_fields");

    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let agent_dir = harness.temp_path("agent");
    std::fs::create_dir_all(&agent_dir).expect("create agent dir");

    let skill_dir = agent_dir.join("skills").join("multi_unknown");
    write_skill(
        &harness,
        &skill_dir,
        "multi_unknown",
        "A skill with unknowns",
        "field_a: 1\nfield_b: 2\n",
    );

    let result = load_skills(LoadSkillsOptions {
        cwd,
        agent_dir,
        home_dir: None,
        skill_paths: vec![skill_dir],
        include_defaults: false,
    });

    assert_eq!(result.skills.len(), 1);
    // Should have diagnostics for unknown fields
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("field_a")),
        "Expected warning for field_a"
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message.contains("field_b")),
        "Expected warning for field_b"
    );
}
