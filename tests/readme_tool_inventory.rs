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
