//! e2e: foreign-format workspace rules import (bd-cv653.6.2).
//!
//! A mixed-format fixture workspace (Cursor MDC + legacy, Cline, Copilot,
//! Windsurf, Gemini) is assembled into a system prompt; scoped rules stay
//! out of the prompt until a tool call touches a matching path; and the
//! import is proven read-only via mtime assertions on every foreign file.

mod common;

use clap::Parser as _;
use common::TestHarness;
use pi::context_files::{ScopedRuleMatcher, discover_foreign_rules};
use std::path::{Path, PathBuf};

fn write_fixture_workspace(harness: &TestHarness) -> Vec<PathBuf> {
    let files = [
        (
            ".cursor/rules/typescript.mdc",
            "---\ndescription: ts style\nglobs: \"*.ts\"\nalwaysApply: false\n---\nUse strict TypeScript.",
        ),
        (".cursorrules", "Cursor legacy: prefer named exports."),
        (".clinerules/general.md", "Cline: comment sparingly."),
        (
            ".github/copilot-instructions.md",
            "Copilot: run the linter before committing.",
        ),
        (
            ".github/instructions/api.instructions.md",
            "---\napplyTo: \"server/api/**\"\n---\nAPI handlers validate inputs.",
        ),
        (".windsurfrules", "Windsurf: keep functions short."),
        ("GEMINI.md", "Gemini: describe changes in commit messages."),
    ];
    files
        .iter()
        .map(|(path, content)| harness.create_file(path, content))
        .collect()
}

fn mtimes(paths: &[PathBuf]) -> Vec<std::time::SystemTime> {
    paths
        .iter()
        .map(|path| {
            std::fs::metadata(path)
                .expect("stat fixture rule file")
                .modified()
                .expect("fixture rule mtime")
        })
        .collect()
}

/// Acceptance 1 + 4: mixed-format assembly with provenance, read-only.
#[test]
fn mixed_format_workspace_assembles_with_provenance_and_zero_writes() {
    let harness = TestHarness::new("foreign_rules_mixed_format_assembly");
    let fixture_paths = write_fixture_workspace(&harness);
    let before = mtimes(&fixture_paths);

    let rules = discover_foreign_rules(harness.temp_dir());
    harness
        .log()
        .info_ctx("foreign-rules", "discovered", |ctx| {
            ctx.push(("rules".into(), rules.rules.len().to_string()));
            ctx.push(("truncated".into(), rules.truncated_rules.to_string()));
        });

    let sources: Vec<(&str, &str, bool)> = rules
        .rules
        .iter()
        .map(|rule| (rule.source.as_str(), rule.format.label(), rule.is_scoped()))
        .collect();
    assert_eq!(
        sources,
        vec![
            (".cursor/rules/typescript.mdc", "cursor-mdc", true),
            (".cursorrules", "cursor-legacy", false),
            (".clinerules/general.md", "cline", false),
            (".github/copilot-instructions.md", "copilot", false),
            (
                ".github/instructions/api.instructions.md",
                "copilot-scoped",
                true
            ),
            (".windsurfrules", "windsurf", false),
            ("GEMINI.md", "gemini", false),
        ]
    );

    let block = rules.system_prompt_block().expect("imported rules block");
    assert!(block.contains("# Imported Rules"));
    assert!(block.contains(".cursorrules (cursor-legacy)"));
    assert!(block.contains("prefer named exports"));
    assert!(
        !block.contains("Use strict TypeScript"),
        "scoped rule content must not appear before activation"
    );
    assert!(block.contains("2 additional path-scoped rule(s)"));

    // Acceptance 4: zero writes — every foreign file's mtime is unchanged.
    let after = mtimes(&fixture_paths);
    assert_eq!(before, after, "foreign rule files must never be written");
}

/// Acceptance 2 (e2e shape): scoped rules activate for matching tool-call
/// paths only, absolute or relative, and each activates exactly once.
#[test]
fn scoped_rules_activate_on_matching_paths() {
    let harness = TestHarness::new("foreign_rules_scoped_activation");
    write_fixture_workspace(&harness);
    let root = harness.temp_dir();

    let rules = discover_foreign_rules(root);
    let matcher = ScopedRuleMatcher::new(&rules.rules);
    assert!(!matcher.is_empty());

    let sources_for = |path: &str| -> Vec<String> {
        matcher
            .matching_rules(Path::new(path), root)
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

    assert_eq!(
        sources_for("src/main.ts"),
        vec![".cursor/rules/typescript.mdc".to_string()]
    );
    assert_eq!(
        sources_for("server/api/users.rs"),
        vec![".github/instructions/api.instructions.md".to_string()]
    );
    assert!(sources_for("README.md").is_empty());
    let absolute = root.join("app/lib.ts");
    assert_eq!(
        matcher
            .matching_rules(&absolute, root)
            .into_iter()
            .map(|index| {
                rules
                    .rules
                    .get(index)
                    .expect("matcher indexes stay in bounds")
                    .source
                    .clone()
            })
            .collect::<Vec<_>>(),
        vec![".cursor/rules/typescript.mdc".to_string()]
    );

    harness
        .log()
        .info_ctx("foreign-rules", "activation matrix verified", |ctx| {
            ctx.push(("scoped_rules".into(), "2".into()));
        });
}

/// The system prompt assembly path carries the imported block end-to-end.
#[test]
fn build_system_prompt_includes_imported_rules_block() {
    let harness = TestHarness::new("foreign_rules_system_prompt_assembly");
    write_fixture_workspace(&harness);
    let root = harness.temp_dir();

    let rules = discover_foreign_rules(root);
    let cli = pi::cli::Cli::parse_from(["pi", "--system-prompt", "BASE PROMPT"]);
    let global_dir = harness.create_dir("hermetic-global");
    let package_dir = harness.create_dir("hermetic-package");
    let prompt = pi::app::build_system_prompt(
        &cli,
        root,
        &["read"],
        None,
        &global_dir,
        &package_dir,
        true, // test_mode keeps ambient ancestor context out; rules are explicit
        true,
        Some(&rules),
        &pi::config::Config::default(),
    )
    .expect("build system prompt with imported rules");

    assert!(prompt.contains("BASE PROMPT"));
    assert!(prompt.contains("# Imported Rules"));
    assert!(prompt.contains("Windsurf: keep functions short."));
    assert!(!prompt.contains("Use strict TypeScript"));

    let normalized = harness.temp_path("assembled-prompt.txt");
    std::fs::write(&normalized, &prompt).expect("write assembled prompt artifact");
    harness.record_artifact("foreign-rules:assembled-prompt", &normalized);
}
