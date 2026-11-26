//! README tool-inventory drift guard (bd-dwh6g).
//!
//! The README "Built-in Tools" section is hand-written. Until the 2026-09-01
//! reality check it had drifted to "28 tools" while the registry could build
//! 35, and it omitted five settings-gated tools entirely. This test binds the
//! prose to the code so either side failing to move with the other fails
//! the gate:
//!
//! - the heading count equals the number of distinct tool names the README
//!   lists (table rows plus the memory-bank list);
//! - every name the registry's `enabled`-name match arms accept appears in
//!   the README, and every README name is accepted by the registry, is a
//!   memory-bank tool, or is one of the session-host-coupled tools that join
//!   through `extend_tools`;
//! - the Essential bullet lists exactly the README names whose default tier
//!   is `LoadMode::Essential`;
//! - the "default `--tools` list names N tools" sentence matches
//!   `xdev::default_enabled_tools().len()`.
//!
//! Parsing is deliberately simple (backtick scanning), so keep the README
//! section's shape: a `### N Built-in Tools` heading, tier bullets, a table
//! whose first cell holds backticked tool names, and an `All tools include`
//! paragraph that ends the section.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn readme() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display())) // ubs:ignore test harness
}

/// The README section from the `### N Built-in Tools` heading to the
/// `All tools include` paragraph, plus the parsed heading count.
fn tools_section(readme: &str) -> (usize, String) {
    let start = readme
        .find("Built-in Tools\n")
        .expect("README has a 'Built-in Tools' heading"); // ubs:ignore test assertion
    let heading_start = readme[..start]
        .rfind("\n### ")
        .expect("heading line before 'Built-in Tools'"); // ubs:ignore test assertion
    let heading_line_end = readme[heading_start + 1..]
        .find('\n')
        .map_or(readme.len(), |offset| heading_start + 1 + offset);
    let heading = readme[heading_start + 1..heading_line_end].trim();
    let count: usize = heading
        .trim_start_matches("### ")
        .split_whitespace()
        .next()
        .and_then(|token| token.parse().ok())
        .unwrap_or_else(|| panic!("heading must start with a count: {heading:?}")); // ubs:ignore test assertion
    let body_start = heading_line_end;
    let end_rel = readme[body_start..]
        .find("\nAll tools include")
        .expect("section ends with the 'All tools include' paragraph"); // ubs:ignore test assertion
    (count, readme[body_start..body_start + end_rel].to_string())
}

/// Backticked tokens that look like tool names (`[a-z_]+`).
fn backticked_identifiers(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut parts = text.split('`');
    // Even-indexed parts are outside backticks; odd-indexed are inside.
    parts.next();
    while let (Some(inside), Some(_)) = (parts.next(), parts.next()) {
        if !inside.is_empty()
            && inside
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        {
            names.push(inside.to_string());
        }
    }
    names
}

/// Tool names the README table lists (first cell of each row).
fn table_names(section: &str) -> BTreeSet<String> {
    section
        .lines()
        .filter(|line| line.starts_with("| `"))
        .flat_map(|line| {
            let first_cell = line.trim_start_matches('|').split('|').next().unwrap_or("");
            backticked_identifiers(first_cell)
        })
        .collect()
}

/// Names listed after "memory-bank tools (" in the Discoverable bullet.
fn memory_bank_names(section: &str) -> BTreeSet<String> {
    let start = section
        .find("memory-bank tools (")
        .expect("Discoverable bullet names the memory-bank tools"); // ubs:ignore test assertion
    let rest = &section[start + "memory-bank tools (".len()..];
    let end = rest.find(')').expect("memory-bank list closes"); // ubs:ignore test assertion
    backticked_identifiers(&rest[..end]).into_iter().collect()
}

/// One tier bullet's text, joined across wrapped lines.
fn bullet(section: &str, marker: &str) -> String {
    let start = section
        .find(marker)
        .unwrap_or_else(|| panic!("README tools section has a bullet starting {marker:?}")); // ubs:ignore test assertion
    let rest = &section[start..];
    let end = rest[1..]
        .find("\n- ")
        .map_or(rest.len(), |offset| offset + 1);
    rest[..end].to_string()
}

/// Names the registry's `for name in enabled { match *name { "..." => ... } }`
/// accepts, scanned from `src/tools.rs`.
fn registry_arm_names() -> BTreeSet<String> {
    let source = include_str!("../src/tools.rs");
    let loop_start = source
        .find("for name in enabled {")
        .expect("ToolRegistry enabled-name loop exists"); // ubs:ignore test assertion
    let body = &source[loop_start..];
    let loop_end = body
        .find("_ => {}")
        .expect("enabled-name match has a wildcard arm"); // ubs:ignore test assertion
    body[..loop_end]
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let rest = trimmed.strip_prefix('"')?;
            let (name, tail) = rest.split_once('"')?;
            tail.trim_start()
                .starts_with("=>")
                .then(|| name.to_string())
        })
        .collect()
}

/// Tools the registry adds outside the enabled-name arms: the host-coupled
/// tools joined via `extend_tools` (`ask`, `todo`, `submit_plan`), the
/// always-present `manage_skill`, the `xdev` dispatcher, and the memory bank.
fn non_arm_names() -> BTreeSet<String> {
    [
        "ask",
        "todo",
        "submit_plan",
        "manage_skill",
        "xdev",
        "retain",
        "recall",
        "reflect",
        "memory_edit",
        "learn",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

#[test]
fn readme_tool_count_matches_listed_names() {
    let readme = readme();
    let (count, section) = tools_section(&readme);
    let mut listed = table_names(&section);
    listed.extend(memory_bank_names(&section));
    assert_eq!(
        count,
        listed.len(),
        "README heading says {count} tools but lists {}: {listed:?}",
        listed.len()
    );
    let comparison_row = readme
        .lines()
        .find(|line| line.starts_with("| **Tools** |"))
        .expect("comparison table has a Tools row"); // ubs:ignore test assertion
    assert!(
        comparison_row.contains(&format!("{count} built-in")),
        "comparison table must repeat the heading count: {comparison_row}"
    );
}

#[test]
fn readme_names_and_registry_arms_agree() {
    let readme = readme();
    let (_, section) = tools_section(&readme);
    let mut listed = table_names(&section);
    listed.extend(memory_bank_names(&section));
    let arms = registry_arm_names();
    assert!(
        arms.len() >= 20,
        "registry arm scan looks broken ({} names): {arms:?}",
        arms.len()
    );
    let known: BTreeSet<String> = arms.union(&non_arm_names()).cloned().collect();

    let undocumented: Vec<_> = known.difference(&listed).collect();
    assert!(
        undocumented.is_empty(),
        "tools the registry can build but README does not list: {undocumented:?}"
    );
    let unknown: Vec<_> = listed.difference(&known).collect();
    assert!(
        unknown.is_empty(),
        "README lists tools the registry cannot build: {unknown:?}"
    );
}

#[test]
fn readme_essential_bullet_matches_default_tiers() {
    let readme = readme();
    let (_, section) = tools_section(&readme);
    let essential_bullet: BTreeSet<String> =
        backticked_identifiers(&bullet(&section, "- **Essential**"))
            .into_iter()
            .collect();
    let mut listed = table_names(&section);
    listed.extend(memory_bank_names(&section));
    let essential_by_code: BTreeSet<String> = listed
        .iter()
        .filter(|name| pi::xdev::default_tier(name) == pi::xdev::LoadMode::Essential)
        .cloned()
        .collect();
    assert_eq!(
        essential_bullet, essential_by_code,
        "README Essential bullet must equal the names whose default tier is Essential"
    );
}

#[test]
fn readme_default_list_count_matches_code() {
    let readme = readme();
    let (_, section) = tools_section(&readme);
    let default_bullet = bullet(&section, "- **Default-enabled**");
    let marker = "list names ";
    let start = default_bullet
        .find(marker)
        .expect("Default-enabled bullet states how many tools the default list names"); // ubs:ignore test assertion
    let stated: usize = default_bullet[start + marker.len()..]
        .split_whitespace()
        .next()
        .and_then(|token| token.parse().ok())
        .expect("default-list count parses"); // ubs:ignore test assertion
    assert_eq!(
        stated,
        pi::xdev::default_enabled_tools().len(),
        "README default `--tools` count must match xdev::default_enabled_tools()"
    );
}

/// Primary default `provider/model` slugs for the first `limit` distinct
/// providers listed in `pi::app::PROVIDER_DEFAULT_MODELS`.
fn top_provider_defaults(limit: usize) -> Vec<String> {
    let mut seen_providers = std::collections::HashSet::new();
    let mut top = Vec::with_capacity(limit);
    for &(provider, model_id) in pi::app::PROVIDER_DEFAULT_MODELS {
        if seen_providers.insert(provider) {
            top.push(format!("{provider}/{model_id}"));
            if top.len() == limit {
                break;
            }
        }
    }
    top
}

fn verify_readme_model_defaults_line(
    readme_text: &str,
    expected_top: &[String],
) -> Result<(), String> {
    let row = readme_text
        .lines()
        .find(|line| line.starts_with("| `--model <MODEL>` |"))
        .ok_or_else(|| {
            "README.md is missing the `| `--model <MODEL>` |` CLI option row".to_string()
        })?;
    let expected_prose = expected_top
        .iter()
        .enumerate()
        .map(|(idx, slug)| {
            if idx == 0 {
                format!("`{slug}`")
            } else {
                format!("then `{slug}`")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let expected_fragment = format!("auto-select fallback: {expected_prose}");
    if !row.contains(&expected_fragment) {
        return Err(format!(
            "README `--model` row drifted from PROVIDER_DEFAULT_MODELS: expected {expected_fragment:?} in {row:?}"
        ));
    }
    Ok(())
}

#[test]
fn readme_model_auto_select_matches_provider_default_models() {
    let readme = readme();
    let expected_top = top_provider_defaults(3);
    assert_eq!(
        expected_top.len(),
        3,
        "PROVIDER_DEFAULT_MODELS must expose at least 3 distinct primary providers"
    );
    let parity = verify_readme_model_defaults_line(&readme, &expected_top);
    assert!(parity.is_ok(), "{}", parity.err().unwrap_or_default());
}

#[test]
fn readme_model_auto_select_parity_rejects_drift() {
    let expected_top = top_provider_defaults(3);
    let stale_readme = "| `--model <MODEL>` | Model to use (auto-select fallback: `anthropic/claude-sonnet-4-6`, then `anthropic/claude-opus-4-7`, then `openai/gpt-5.1-codex`) |\n";
    let err = verify_readme_model_defaults_line(stale_readme, &expected_top)
        .expect_err("stale README auto-select prose must fail closed");
    assert!(
        err.contains("drifted from PROVIDER_DEFAULT_MODELS"),
        "unexpected error message: {err}"
    );
}

fn read_repo_file(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).expect("read repository doc/config file") // ubs:ignore test harness
}

fn verify_default_features_doc_parity(readme_text: &str, cargo_toml: &str) -> Result<(), String> {
    let default_line = cargo_toml
        .lines()
        .find(|line| line.trim_start().starts_with("default = ["))
        .ok_or_else(|| "Cargo.toml missing [features] default = [...] line".to_string())?;
    let inner = default_line
        .split('[')
        .nth(1)
        .and_then(|rest| rest.split(']').next())
        .ok_or_else(|| format!("malformed Cargo.toml default features line: {default_line}"))?;
    let default_features: Vec<&str> = inner
        .split(',')
        .filter_map(|token| {
            let trimmed = token.trim().trim_matches('"');
            (!trimmed.is_empty()).then_some(trimmed)
        })
        .collect();
    if default_features.is_empty() {
        return Err("parsed zero default features from Cargo.toml".to_string());
    }
    let section_start = readme_text
        .find("### Feature defaults")
        .ok_or_else(|| "README.md missing `### Feature defaults` section".to_string())?;
    let section_body = &readme_text[section_start..];
    let section_end = section_body[1..]
        .find("\n### ")
        .map_or(section_body.len(), |idx| idx + 1);
    let section = &section_body[..section_end];
    if section.contains("sqlite-sessions` only") {
        return Err(
            "README Feature defaults still falsely claims `sqlite-sessions` only".to_string(),
        );
    }
    for feature in default_features {
        if !section.contains(&format!("`{feature}`")) {
            return Err(format!(
                "README Feature defaults omits Cargo.toml default feature `{feature}`"
            ));
        }
    }
    Ok(())
}

fn verify_dsr_recipe_check_count_doc(recipe_doc: &str, repos_yaml: &str) -> Result<(), String> {
    let checks_block = repos_yaml
        .split("\n    checks:\n")
        .nth(1)
        .ok_or_else(|| ".dsr/repos.yaml missing `checks:` list".to_string())?;
    let actual_checks = checks_block
        .lines()
        .take_while(|line| line.starts_with("      ") || line.trim().is_empty())
        .filter(|line| line.starts_with("      - "))
        .count();
    let heading_line = recipe_doc
        .lines()
        .find(|line| line.starts_with("## 2. The DSR quality recipe ("))
        .ok_or_else(|| {
            "docs/perf-budgets-recipe.md missing section 2 DSR check-count heading".to_string()
        })?;
    let expected_fragment = format!("({actual_checks} checks,");
    if !heading_line.contains(&expected_fragment) {
        return Err(format!(
            "docs/perf-budgets-recipe.md check count drifted from .dsr/repos.yaml: expected {expected_fragment:?} in {heading_line:?}"
        ));
    }
    Ok(())
}

fn verify_changelog_unreleased_completeness(changelog: &str) -> Result<(), String> {
    let start = changelog
        .find("## [Unreleased]")
        .ok_or_else(|| "CHANGELOG.md missing `## [Unreleased]` heading".to_string())?;
    let rest = &changelog[start..];
    let end = rest[1..]
        .find("\n## [v0.8.0]")
        .map_or(rest.len(), |idx| idx + 1);
    let post_v080 = &rest[..end];
    for required in [
        "/goal",
        "pi web",
        "--wait-async",
        "--max-cost",
        "/memory sync",
        "session/fork",
        "/rules",
        "/fast ultrafast",
        "models.fetched.json",
        "better-sqlite3",
        "axios",
        "SnapCompact",
        "pi::sdk",
    ] {
        if !post_v080.contains(required) {
            return Err(format!(
                "CHANGELOG.md post-v0.8.0 section omits flagship item `{required}`"
            ));
        }
    }
    Ok(())
}

#[test]
fn docs_and_changelog_parity_checks_pass_and_reject_planted_drift() {
    let readme_text = readme();
    let cargo_toml = read_repo_file("Cargo.toml");
    let recipe_doc = read_repo_file("docs/perf-budgets-recipe.md");
    let repos_yaml = read_repo_file(".dsr/repos.yaml");
    let sdk_doc = read_repo_file("docs/sdk.md");
    let changelog = read_repo_file("CHANGELOG.md");

    let feat_check = verify_default_features_doc_parity(&readme_text, &cargo_toml);
    assert!(
        feat_check.is_ok(),
        "{}",
        feat_check.err().unwrap_or_default()
    );

    let dsr_doc_check = verify_dsr_recipe_check_count_doc(&recipe_doc, &repos_yaml);
    assert!(
        dsr_doc_check.is_ok(),
        "{}",
        dsr_doc_check.err().unwrap_or_default()
    );

    assert!(
        !sdk_doc.contains("governed by the active\ncertification contract")
            && !sdk_doc.contains("governed by the active certification contract"),
        "docs/sdk.md must not claim retired drop-in certification as an active release gate"
    );
    assert!(
        !readme_text.contains("Fresh `v0.3.0` measurement pending")
            && !readme_text.contains("**v0.3.0 performance-claim status"),
        "README.md must not carry stale v0.3.0 pending-measurement release labels"
    );

    let cl_check = verify_changelog_unreleased_completeness(&changelog);
    assert!(cl_check.is_ok(), "{}", cl_check.err().unwrap_or_default());

    // Planted-red negatives: injected drift must fail closed.
    let stale_feat = "### Feature defaults\n\nDefault DSR builds enable `sqlite-sessions` only.\n";
    assert!(verify_default_features_doc_parity(stale_feat, &cargo_toml).is_err());

    let stale_recipe = "## 2. The DSR quality recipe (8 checks, registered)\n";
    assert!(verify_dsr_recipe_check_count_doc(stale_recipe, &repos_yaml).is_err());

    let truncated_cl = "## [Unreleased]\n\n### Added\n- Only one item.\n\n## [v0.8.0]\n";
    assert!(verify_changelog_unreleased_completeness(truncated_cl).is_err());

    // Truthfulness guards from the 2026-11-25 reality check:
    assert!(
        !readme_text.contains(r#"{ "apiKey": "sk-ant-..." }"#),
        "README troubleshooting must not recommend a nonexistent settings.json `apiKey` field"
    );
    assert!(
        !readme_text
            .contains("DSR provisions the coverage toolchain and records summary, LCOV, and HTML"),
        "README Coverage section must not claim DSR runs LCOV/HTML coverage until registered in .dsr/repos.yaml"
    );
    assert!(
        !readme_text.contains("not part of the code quality recipe"),
        "README must reflect that check_readme_evidence_freshness.py is Check 10 in .dsr/repos.yaml"
    );
    assert!(
        !cargo_toml.contains("22 MiB"),
        "Cargo.toml must not retain retired 22 MiB release size-budget comments"
    );
}

fn subcommands_section(readme_text: &str) -> &str {
    let start = readme_text
        .find("### Subcommands\n")
        .expect("README has a `### Subcommands` section"); // ubs:ignore test assertion
    let rest = &readme_text[start..];
    let end = rest[1..]
        .find("\n## ")
        .map_or(rest.len(), |offset| offset + 1);
    &rest[..end]
}

#[test]
fn readme_subcommands_cover_all_visible_cli_commands() {
    use clap::CommandFactory;

    let readme_text = readme();
    let section = subcommands_section(&readme_text);
    let cli_cmd = pi::cli::Cli::command();

    let mut missing = Vec::new();
    for sub in cli_cmd.get_subcommands() {
        if sub.is_hide_set() || sub.get_name() == "help" {
            continue;
        }
        let name = sub.get_name();
        let mentioned =
            section.contains(&format!("pi {name}")) || section.contains(&format!("`{name}`"));
        if !mentioned {
            missing.push(name.to_string());
        }
        if name == "usage" {
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            assert!(
                !about.to_ascii_lowercase().contains("migrate"),
                "`pi usage` help text must not carry a misplaced Migrate doc comment: {about:?}"
            );
        }
    }
    assert!(
        missing.is_empty(),
        "visible CLI subcommands omitted from README `### Subcommands`: {missing:?}"
    );
}
