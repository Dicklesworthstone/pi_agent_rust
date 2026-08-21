//! Managed skills (bd-cv653.4.2).
//!
//! The agent can author skills at runtime into the isolated managed dir
//! (`<global_dir>/skills.managed/<name>/SKILL.md`). Managed skills load
//! dead-last in precedence (resources.rs adds the tier after packages), so
//! user and project skills always win collisions — `manage_skill` can never
//! shadow or mutate user-authored work, and every mutation requires the
//! `managed: true` frontmatter marker plus lands in an audit ledger.
//!
//! `learn` captures a lesson (memory kind=lesson, bd-cv653.4.1) and can
//! promote it into a managed skill; drafts that fail the lint gate (same
//! validators as the skills loader) are kept as lessons only, with the
//! warning surfaced.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{Error, Result};

/// Tool-result schema tag for managed-skill operations.
pub const SKILL_SCHEMA: &str = "pi.managed_skill.v1";

/// Directory the managed tier loads from (dead-last precedence).
#[must_use]
pub fn managed_skills_dir() -> PathBuf {
    crate::config::Config::global_dir().join("skills.managed")
}

/// One managed skill with its provenance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedSkillInfo {
    pub schema: String,
    pub name: String,
    pub description: String,
    pub path: String,
    pub managed: bool,
}

/// Lint a skill draft with the same validators the skills loader applies.
/// Returns the list of violations (empty = valid).
#[must_use]
pub fn lint_skill_draft(name: &str, description: &str, fields: &[&str]) -> Vec<String> {
    let mut errors = crate::resources::validate_name(name, name);
    errors.extend(crate::resources::validate_description(description));
    let owned: Vec<String> = fields.iter().map(|field| (*field).to_string()).collect();
    errors.extend(crate::resources::validate_frontmatter_fields(owned.iter()));
    errors
}

fn skill_dir(name: &str) -> PathBuf {
    managed_skills_dir().join(name)
}

fn skill_file(name: &str) -> PathBuf {
    skill_dir(name).join("SKILL.md")
}

/// Render a SKILL.md with the managed marker.
fn render_skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\nmanaged: true\n---\n\n{body}\n")
}

/// Read the frontmatter map of an existing SKILL.md (light parse: the
/// `key: value` lines inside the first `---` fence).
fn frontmatter_of(path: &Path) -> Option<std::collections::HashMap<String, String>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let mut fields = std::collections::HashMap::new();
    let mut inside = false;
    for line in raw.lines() {
        if line.trim() == "---" {
            if inside {
                break;
            }
            inside = true;
            continue;
        }
        if inside && let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Some(fields)
}

fn is_managed(path: &Path) -> bool {
    frontmatter_of(path)
        .and_then(|fields| fields.get("managed").cloned())
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
}

fn audit(op: &str, name: &str, rationale: Option<&str>, session_id: Option<&str>) {
    let dir = managed_skills_dir();
    let _ = std::fs::create_dir_all(&dir);
    let entry = serde_json::json!({
        "op": op,
        "name": name,
        "rationale": rationale,
        "sessionId": session_id,
        "atMs": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("audit.jsonl"))
    {
        use std::io::Write as _;
        let _ = writeln!(file, "{entry}");
    }
}

/// Create a managed skill. The draft is linted first; invalid drafts are
/// refused with the violations listed (the caller keeps the lesson).
///
/// # Errors
/// Named `PI_SKILL_EXISTS` when a live skill already has the name;
/// `PI_SKILL_INVALID` with the lint violations.
pub fn create(name: &str, description: &str, body: &str) -> Result<ManagedSkillInfo> {
    let violations = lint_skill_draft(name, description, &["name", "description", "managed"]);
    if !violations.is_empty() {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "PI_SKILL_INVALID: skill draft failed the lint gate: {}",
                violations.join("; ")
            ),
        ));
    }
    let dir = skill_dir(name);
    let file = skill_file(name);
    if file.exists() {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "PI_SKILL_EXISTS: a skill named '{name}' already exists at {}",
                file.display()
            ),
        ));
    }
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to create skill dir: {e}")))?;
    std::fs::write(&file, render_skill_md(name, description, body))
        .map_err(|e| Error::tool("manage_skill", format!("Failed to write skill: {e}")))?;
    audit("create", name, None, None);
    Ok(ManagedSkillInfo {
        schema: SKILL_SCHEMA.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        path: file.display().to_string(),
        managed: true,
    })
}

/// Update a managed skill's body (description kept unless provided).
///
/// # Errors
/// `PI_SKILL_UNKNOWN` when absent; `PI_SKILL_NOT_MANAGED` when the marker
/// is missing (user-authored content — never touched).
pub fn update(name: &str, description: Option<&str>, body: &str) -> Result<ManagedSkillInfo> {
    let file = skill_file(name);
    if !file.exists() {
        return Err(Error::tool(
            "manage_skill",
            format!("PI_SKILL_UNKNOWN: no managed skill named '{name}'"),
        ));
    }
    if !is_managed(&file) {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "PI_SKILL_NOT_MANAGED: '{}' lacks the managed marker — refusing to mutate \
                 user-authored content",
                file.display()
            ),
        ));
    }
    let existing = frontmatter_of(&file).unwrap_or_default();
    let description = description
        .map(str::to_string)
        .or_else(|| existing.get("description").cloned())
        .unwrap_or_default();
    let violations = lint_skill_draft(name, &description, &["name", "description", "managed"]);
    if !violations.is_empty() {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "PI_SKILL_INVALID: updated draft failed the lint gate: {}",
                violations.join("; ")
            ),
        ));
    }
    std::fs::write(&file, render_skill_md(name, &description, body))
        .map_err(|e| Error::tool("manage_skill", format!("Failed to write skill: {e}")))?;
    audit("update", name, None, None);
    Ok(ManagedSkillInfo {
        schema: SKILL_SCHEMA.to_string(),
        name: name.to_string(),
        description,
        path: file.display().to_string(),
        managed: true,
    })
}

/// Delete a managed skill directory. Refuses anything lacking the managed
/// marker (user-authored content is untouchable).
///
/// # Errors
/// `PI_SKILL_UNKNOWN` / `PI_SKILL_NOT_MANAGED`.
pub fn delete(name: &str) -> Result<()> {
    let dir = skill_dir(name);
    let file = skill_file(name);
    if !file.exists() {
        return Err(Error::tool(
            "manage_skill",
            format!("PI_SKILL_UNKNOWN: no managed skill named '{name}'"),
        ));
    }
    if !is_managed(&file) {
        return Err(Error::tool(
            "manage_skill",
            format!(
                "PI_SKILL_NOT_MANAGED: '{}' lacks the managed marker — refusing to delete \
                 user-authored content",
                file.display()
            ),
        ));
    }
    std::fs::remove_dir_all(&dir)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to delete skill: {e}")))?;
    audit("delete", name, None, None);
    Ok(())
}

/// List managed skills with provenance.
///
/// # Errors
/// IO errors reading the managed dir.
pub fn list() -> Result<Vec<ManagedSkillInfo>> {
    let dir = managed_skills_dir();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| Error::tool("manage_skill", format!("Failed to read managed dir: {e}")))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let file = path.join("SKILL.md");
        if !file.exists() {
            continue;
        }
        let fields = frontmatter_of(&file).unwrap_or_default();
        let name = fields
            .get("name")
            .cloned()
            .or_else(|| entry.file_name().to_str().map(str::to_string))
            .unwrap_or_default();
        out.push(ManagedSkillInfo {
            schema: SKILL_SCHEMA.to_string(),
            name,
            description: fields.get("description").cloned().unwrap_or_default(),
            path: file.display().to_string(),
            managed: is_managed(&file),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_name(tag: &str) -> String {
        format!("pi-test-{tag}-{}", std::process::id())
    }

    #[test]
    fn lint_gate_rejects_bad_names_and_descriptions() {
        assert!(!lint_skill_draft("Bad Name", "ok", &["name", "description"]).is_empty());
        assert!(!lint_skill_draft("ok-name", "", &["name", "description"]).is_empty());
        assert!(!lint_skill_draft(&"x".repeat(65), "ok", &["name", "description"]).is_empty());
        assert!(
            lint_skill_draft("good-name", "a real description", &["name", "description"])
                .is_empty()
        );
    }

    #[test]
    fn create_list_update_delete_cycle() {
        let name = unique_name("cycle");
        let info = create(&name, "cycle skill", "body one").expect("create");
        assert!(info.managed);
        assert!(std::path::Path::new(&info.path).exists());

        let listed = list().expect("list");
        assert!(listed.iter().any(|skill| skill.name == name));

        let updated = update(&name, None, "body two").expect("update");
        assert_eq!(updated.description, "cycle skill");
        let raw = std::fs::read_to_string(&info.path).expect("read");
        assert!(raw.contains("body two"));
        assert!(raw.contains("managed: true"));

        delete(&name).expect("delete");
        assert!(!std::path::Path::new(&info.path).exists());
    }

    #[test]
    fn delete_refuses_unmanaged_content() {
        let name = unique_name("unmanaged");
        let dir = skill_dir(&name);
        std::fs::create_dir_all(&dir).expect("dir");
        // User-authored-looking SKILL.md: no managed marker.
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: user skill\n---\n\nbody\n"),
        )
        .expect("write");
        let err = delete(&name).unwrap_err();
        assert!(
            err.to_string().contains("PI_SKILL_NOT_MANAGED"),
            "expected refusal: {err}"
        );
        let err = update(&name, None, "hijack").unwrap_err();
        assert!(
            err.to_string().contains("PI_SKILL_NOT_MANAGED"),
            "expected refusal: {err}"
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn create_refuses_invalid_drafts() {
        let err = create("Bad Name", "desc", "body").unwrap_err();
        assert!(
            err.to_string().contains("PI_SKILL_INVALID"),
            "expected lint refusal: {err}"
        );
    }
}
