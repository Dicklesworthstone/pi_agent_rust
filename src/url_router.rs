//! Internal URL scheme router (bd-cv653.6.3).
//!
//! FS-shaped tools resolve internal schemes into virtual documents with
//! the same shape as local reads: `read skill://name` returns the skill
//! file; `read conflict://1 @theirs` resolves a merge conflict by writing
//! one selector; `read ssh://host/path` cats a remote path.
//!
//! v1 schemes: `skill://`, `prompt://`, `local://`, `conflict://`,
//! `pr://` / `issue://` (+ `pr://.../diff/N`), `ssh://`. Blocked on
//! dependencies and documented in module docs only: `agent://` (needs the
//! bd-cv653.5.3 child registry), `vault://` (needs the bd-cv653.7.9
//! placeholder vault). Unknown schemes error with the registered list —
//! resolution NEVER silently falls back to the filesystem.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;

use crate::error::{Error, Result};

/// Tool-result schema tag for scheme resolutions.
pub const URL_ROUTER_SCHEMA: &str = "pi.url_router.v1";

/// ssh cat cap (a remote read should never be unbounded).
const SSH_MAX_BYTES: usize = 1024 * 1024;

/// A resolved virtual document.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedDoc {
    pub schema: String,
    pub scheme: String,
    pub reference: String,
    pub content: String,
    pub content_type: String,
    pub metadata: serde_json::Value,
    /// True when line selectors (:50-200, offset/limit) apply.
    pub line_addressable: bool,
}

/// Registered v1 schemes (for error messages).
const SCHEMES: &[&str] = &["skill", "prompt", "local", "conflict", "pr", "issue", "ssh"];

/// Parse `scheme://rest` into (scheme, rest).
fn split_scheme(path: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = path.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some((scheme, rest))
}

/// True when a path is scheme-shaped (`x://...`).
///
/// Routes through the router (which errors on unknown schemes with the
/// registered list — never a silent filesystem fallback). `file://` stays
/// on the filesystem.
#[must_use]
pub fn has_scheme(path: &str) -> bool {
    split_scheme(path).is_some_and(|(scheme, _)| scheme != "file")
}

/// Resolution-time overrides (tests stub the gh backend through these).
#[derive(Debug, Clone, Default)]
pub struct ResolveOptions {
    /// `gh` binary override (the github stub path in tests).
    pub gh_binary: Option<String>,
}

/// Resolve a scheme path into a virtual document with default options.
///
/// # Errors
/// Unknown scheme → error listing registered schemes; unresolvable ref →
/// named error with a hint. Never falls back to the filesystem.
pub fn resolve(path: &str, cwd: &Path) -> Result<ResolvedDoc> {
    resolve_with(path, cwd, &ResolveOptions::default())
}

/// Resolve with explicit overrides.
///
/// # Errors
/// See [`resolve`].
pub fn resolve_with(path: &str, cwd: &Path, options: &ResolveOptions) -> Result<ResolvedDoc> {
    let Some((scheme, rest)) = split_scheme(path) else {
        return Err(Error::tool(
            "read",
            format!("PI_URL_NO_SCHEME: '{path}' is not a scheme URL"),
        ));
    };
    match scheme {
        "skill" => resolve_skill(rest),
        "prompt" => resolve_prompt(rest),
        "local" => resolve_local(rest),
        "conflict" => resolve_conflict(rest, cwd),
        "pr" | "issue" => resolve_github(scheme, rest, cwd, options),
        "ssh" => resolve_ssh(rest),
        other => Err(Error::tool(
            "read",
            format!(
                "PI_URL_UNKNOWN_SCHEME: unknown scheme '{other}://'. Registered schemes: {}",
                SCHEMES
                    .iter()
                    .map(|s| format!("{s}://"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

fn doc(
    scheme: &str,
    reference: &str,
    content: String,
    content_type: &str,
    metadata: serde_json::Value,
) -> ResolvedDoc {
    ResolvedDoc {
        schema: URL_ROUTER_SCHEMA.to_string(),
        scheme: scheme.to_string(),
        reference: reference.to_string(),
        content,
        content_type: content_type.to_string(),
        metadata,
        line_addressable: true,
    }
}

// ---------------------------------------------------------------------------
// skill:// and prompt:// via the resources loader
// ---------------------------------------------------------------------------

fn resolve_skill(name: &str) -> Result<ResolvedDoc> {
    let skills = crate::resources::load_skills(crate::resources::LoadSkillsOptions {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        agent_dir: crate::config::Config::global_dir(),
        skill_paths: Vec::new(),
        include_defaults: true,
    });
    let Some(skill) = skills.skills.iter().find(|skill| skill.name == name) else {
        let known: Vec<&str> = skills.skills.iter().map(|s| s.name.as_str()).collect();
        return Err(Error::tool(
            "read",
            format!(
                "PI_URL_UNRESOLVABLE: no skill named '{name}'. Available: {}",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            ),
        ));
    };
    let content = std::fs::read_to_string(&skill.file_path)
        .map_err(|e| Error::tool("read", format!("Failed to read skill '{name}': {e}")))?;
    Ok(doc(
        "skill",
        name,
        content,
        "text/markdown",
        serde_json::json!({
            "path": skill.file_path.display().to_string(),
            "source": skill.source,
            "description": skill.description,
        }),
    ))
}

fn resolve_prompt(name: &str) -> Result<ResolvedDoc> {
    let templates =
        crate::resources::load_prompt_templates(crate::resources::LoadPromptTemplatesOptions {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            agent_dir: crate::config::Config::global_dir(),
            prompt_paths: Vec::new(),
            include_defaults: true,
        });
    let Some(template) = templates.iter().find(|t| t.name == name) else {
        let known: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        return Err(Error::tool(
            "read",
            format!(
                "PI_URL_UNRESOLVABLE: no prompt template named '{name}'. Available: {}",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            ),
        ));
    };
    Ok(doc(
        "prompt",
        name,
        template.content.clone(),
        "text/markdown",
        serde_json::json!({
            "path": template.file_path.display().to_string(),
            "description": template.description,
        }),
    ))
}

// ---------------------------------------------------------------------------
// local:// session scratch documents
// ---------------------------------------------------------------------------

fn scratch_store() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static STORE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    &STORE
}

#[allow(clippy::significant_drop_tightening)]
fn resolve_local(name: &str) -> Result<ResolvedDoc> {
    let store = scratch_store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(content) = store.get(name) else {
        let known: Vec<&String> = store.keys().collect();
        return Err(Error::tool(
            "read",
            format!(
                "PI_URL_UNRESOLVABLE: no local scratch document '{name}'. Present: {}",
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        ));
    };
    Ok(doc(
        "local",
        name,
        content.clone(),
        "text/plain",
        serde_json::json!({ "scratch": true }),
    ))
}

/// Write a local:// scratch document (session-scoped).
///
/// # Errors
/// Named validation error for empty names.
pub fn write_local(name: &str, content: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(Error::validation(
            "local:// scratch name must be non-empty".to_string(),
        ));
    }
    scratch_store()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.to_string(), content.to_string());
    Ok(())
}

// ---------------------------------------------------------------------------
// conflict:// merge-conflict regions
// ---------------------------------------------------------------------------

/// One parsed conflict region.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictRegion {
    pub index: usize,
    pub file: String,
    pub ours_label: String,
    pub theirs_label: String,
    pub ours: String,
    pub theirs: String,
    pub base: String,
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| Error::tool("read", format!("Failed to run git: {e}")))?;
    if !output.status.success() {
        return Err(Error::tool(
            "read",
            format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse all <<<<<<< regions in one file (2-way markers; base from
/// diff3-style ||||||| section when present).
fn parse_conflicts(file: &Path, relative: &str, start_index: usize) -> Vec<ConflictRegion> {
    let Ok(content) = std::fs::read_to_string(file) else {
        return Vec::new();
    };
    let mut regions = Vec::new();
    let mut ours_label = String::new();
    let mut ours = String::new();
    let mut theirs = String::new();
    let mut base = String::new();
    let mut section: Option<&str> = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("<<<<<<<") {
            ours_label = rest.trim().to_string();
            section = Some("ours");
            ours.clear();
            theirs.clear();
            base.clear();
            continue;
        }
        if line.starts_with("|||||||") && section == Some("ours") {
            section = Some("base");
            continue;
        }
        if line.starts_with("=======") && matches!(section, Some("ours" | "base")) {
            section = Some("theirs");
            continue;
        }
        if let Some(rest) = line.strip_prefix(">>>>>>>")
            && section == Some("theirs")
        {
            regions.push(ConflictRegion {
                index: start_index + regions.len(),
                file: relative.to_string(),
                ours_label: std::mem::take(&mut ours_label),
                theirs_label: rest.trim().to_string(),
                ours: std::mem::take(&mut ours),
                theirs: std::mem::take(&mut theirs),
                base: std::mem::take(&mut base),
            });
            section = None;
            continue;
        }
        match section {
            Some("ours") => {
                ours.push_str(line);
                ours.push('\n');
            }
            Some("base") => {
                base.push_str(line);
                base.push('\n');
            }
            Some("theirs") => {
                theirs.push_str(line);
                theirs.push('\n');
            }
            _ => {}
        }
    }
    regions
}

/// List all conflict regions across the repo's unmerged files.
pub fn conflict_regions(cwd: &Path) -> Result<Vec<ConflictRegion>> {
    let listing = git_output(cwd, &["diff", "--name-only", "--diff-filter=U"])?;
    let mut regions = Vec::new();
    for relative in listing.lines().filter(|line| !line.is_empty()) {
        let file = cwd.join(relative);
        let parsed = parse_conflicts(&file, relative, regions.len());
        regions.extend(parsed);
    }
    Ok(regions)
}

fn resolve_conflict(rest: &str, cwd: &Path) -> Result<ResolvedDoc> {
    let regions = conflict_regions(cwd)?;
    if regions.is_empty() {
        return Err(Error::tool(
            "read",
            "PI_URL_UNRESOLVABLE: no merge conflicts in this repo".to_string(),
        ));
    }
    let (index_part, selector) = rest.split_once(' ').map_or((rest, "full"), |(idx, sel)| {
        (idx, sel.trim_start_matches('@'))
    });
    if index_part == "*" {
        let rendered = regions
            .iter()
            .map(|region| {
                format!(
                    "### conflict {} in {} (ours: {}, theirs: {})\n--- ours ---\n{}--- theirs ---\n{}",
                    region.index, region.file, region.ours_label, region.theirs_label, region.ours, region.theirs
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Ok(doc(
            "conflict",
            "*",
            rendered,
            "text/plain",
            serde_json::json!({ "count": regions.len() }),
        ));
    }
    let index: usize = index_part.parse().map_err(|_| {
        Error::validation(format!(
            "conflict:// index must be a number or '*', got '{index_part}'"
        ))
    })?;
    let Some(region) = regions.iter().find(|region| region.index == index) else {
        return Err(Error::tool(
            "read",
            format!(
                "PI_URL_UNRESOLVABLE: no conflict region {index} (have {})",
                regions.len()
            ),
        ));
    };
    let (content, which) = match selector {
        "ours" => (region.ours.clone(), "ours"),
        "theirs" => (region.theirs.clone(), "theirs"),
        "base" => (region.base.clone(), "base"),
        _ => (
            format!(
                "### conflict {} in {} (ours: {}, theirs: {})\n--- ours ---\n{}--- base ---\n{}--- theirs ---\n{}",
                region.index,
                region.file,
                region.ours_label,
                region.theirs_label,
                region.ours,
                region.base,
                region.theirs
            ),
            "full",
        ),
    };
    Ok(doc(
        "conflict",
        rest,
        content,
        "text/plain",
        serde_json::to_value(region)?,
    ))
    .map(|mut doc| {
        doc.metadata["selector"] = serde_json::Value::String(which.to_string()); // ubs:ignore Value index assignment never panics
        doc
    })
}

/// Resolve a conflict by writing one side into the file.
///
/// # Errors
/// `PI_URL_UNRESOLVABLE` for unknown regions; IO failures.
pub fn write_conflict_resolution(cwd: &Path, index: usize, side: &str) -> Result<ConflictRegion> {
    let regions = conflict_regions(cwd)?;
    let Some(region) = regions.iter().find(|region| region.index == index) else {
        return Err(Error::tool(
            "write",
            format!(
                "PI_URL_UNRESOLVABLE: no conflict region {index} (have {})",
                regions.len()
            ),
        ));
    };
    let chosen = match side {
        "ours" => &region.ours,
        "theirs" => &region.theirs,
        "base" => &region.base,
        other => {
            return Err(Error::validation(format!(
                "conflict resolution side must be ours, theirs, or base; got '{other}'"
            )));
        }
    };
    let file = cwd.join(&region.file);
    let content = std::fs::read_to_string(&file)
        .map_err(|e| Error::tool("write", format!("Failed to read {}: {e}", region.file)))?;
    let mut out = String::with_capacity(content.len());
    let mut skipping = false;
    let mut in_target = false;
    let mut occurrence = regions
        .iter()
        .filter(|r| r.file == region.file && r.index <= index)
        .count()
        .saturating_sub(1);
    for line in content.lines() {
        if line.starts_with("<<<<<<<") {
            skipping = true;
            in_target = occurrence == 0;
            if in_target {
                out.push_str(chosen);
            }
            occurrence = occurrence.saturating_sub(1);
            continue;
        }
        if line.starts_with(">>>>>>>") && skipping {
            skipping = false;
            in_target = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        } else if in_target {
            continue;
        }
        let _ = in_target;
    }
    std::fs::write(&file, out)
        .map_err(|e| Error::tool("write", format!("Failed to write {}: {e}", region.file)))?;
    Ok(region.clone())
}

// ---------------------------------------------------------------------------
// pr:// / issue:// via the gh CLI (same backend as the github tool)
// ---------------------------------------------------------------------------

fn parse_repo_number(rest: &str) -> Result<(Option<String>, String, Option<String>)> {
    // Forms: N | owner/repo/N | owner/repo/N/diff | N/diff
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return Err(Error::validation(format!(
            "pr/issue reference must be <n> or <owner/repo/n>, got '{rest}'"
        )));
    }
    let (number_index, repo) = if parts.len() >= 3 {
        (2, Some(format!("{}/{}", parts[0], parts[1])))
    } else {
        (0, None)
    };
    let number = parts
        .get(number_index)
        .or_else(|| parts.first())
        .ok_or_else(|| Error::validation(format!("missing issue/PR number in '{rest}'")))?
        .to_string();
    if number.parse::<u64>().is_err() {
        return Err(Error::validation(format!(
            "issue/PR number must be numeric, got '{number}'"
        )));
    }
    let sub = parts.get(number_index + 1).map(|s| (*s).to_string());
    Ok((repo, number, sub))
}

fn resolve_github(
    scheme: &str,
    rest: &str,
    cwd: &Path,
    options: &ResolveOptions,
) -> Result<ResolvedDoc> {
    let (repo, number, sub) = parse_repo_number(rest)?;
    let mut args: Vec<String> = if sub.as_deref() == Some("diff") && scheme == "pr" {
        vec!["pr".to_string(), "diff".to_string(), number.clone()]
    } else {
        vec![scheme.to_string(), "view".to_string(), number.clone()]
    };
    if let Some(repo) = &repo {
        args.push("--repo".to_string());
        args.push(repo.clone());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let gh = options.gh_binary.as_deref().unwrap_or("gh");
    let output = std::process::Command::new(gh)
        .args(&arg_refs)
        .current_dir(cwd)
        .output()
        .map_err(|e| {
            Error::tool(
                "read",
                format!("PI_URL_BACKEND: failed to run gh (install gh CLI): {e}"),
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::tool(
            "read",
            format!(
                "PI_URL_UNRESOLVABLE: gh {} failed: {}",
                arg_refs.join(" "),
                stderr.trim()
            ),
        ));
    }
    let content = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(doc(
        scheme,
        rest,
        content,
        "text/plain",
        serde_json::json!({
            "repo": repo,
            "number": number,
            "sub": sub,
            "backend": "gh",
        }),
    ))
}

// ---------------------------------------------------------------------------
// ssh://host/path (read-only cat, size-capped)
// ---------------------------------------------------------------------------

fn resolve_ssh(rest: &str) -> Result<ResolvedDoc> {
    let (host, remote_path) = rest.split_once('/').ok_or_else(|| {
        Error::validation(format!("ssh:// reference must be host/path, got '{rest}'"))
    })?;
    if host.is_empty() || remote_path.is_empty() {
        return Err(Error::validation(format!(
            "ssh:// reference must be host/path, got '{rest}'"
        )));
    }
    let output = std::process::Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            host,
            &format!("head -c {SSH_MAX_BYTES} -- '{remote_path}'"),
        ])
        .output()
        .map_err(|e| Error::tool("read", format!("PI_URL_BACKEND: failed to run ssh: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::tool(
            "read",
            format!(
                "PI_URL_UNRESOLVABLE: ssh {host} cat '{remote_path}' failed: {}",
                stderr.trim()
            ),
        ));
    }
    let content = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(doc(
        "ssh",
        rest,
        content,
        "text/plain",
        serde_json::json!({
            "host": host,
            "path": remote_path,
            "cappedAt": SSH_MAX_BYTES,
        }),
    ))
}

// ---------------------------------------------------------------------------
// ssh://host/path writes (bd-cv653.6.5): confined hosts, batch-mode auth,
// accept-new-then-strict host keys, atomic remote staging.
// ---------------------------------------------------------------------------

/// A parsed `ssh://host/path` write target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub path: String,
}

/// Parse and validate an `ssh://host/path` URL for a write-side tool.
///
/// Rejects non-ssh schemes, empty components, and any `..` path segment so
/// remote staging can never escape the intended directory.
///
/// # Errors
/// Named `PI_SSH_*` validation errors.
pub fn parse_ssh_target(url: &str) -> Result<SshTarget> {
    let Some(("ssh", rest)) = split_scheme(url) else {
        return Err(Error::validation(format!(
            "PI_SSH_TARGET: '{url}' is not an ssh://host/path URL"
        )));
    };
    let (host, remote_path) = rest.split_once('/').ok_or_else(|| {
        Error::validation(format!(
            "PI_SSH_TARGET: ssh:// reference must be host/path, got '{rest}'"
        ))
    })?;
    if host.is_empty() || remote_path.is_empty() {
        return Err(Error::validation(format!(
            "PI_SSH_TARGET: ssh:// reference must be host/path, got '{rest}'"
        )));
    }
    if remote_path.split('/').any(|segment| segment == "..") {
        return Err(Error::validation(format!(
            "PI_SSH_TRAVERSAL: ssh:// paths must not contain '..' segments: '{rest}'"
        )));
    }
    Ok(SshTarget {
        host: host.to_string(),
        path: remote_path.to_string(),
    })
}

/// Literal host tokens from ssh_config text. Wildcard (`*`, `?`) and
/// negation (`!host`) patterns never authorize a specific host.
fn ssh_config_literal_hosts(config_text: &str) -> Vec<String> {
    config_text
        .lines()
        .filter_map(|line| {
            let mut tokens = line.trim().split_whitespace();
            if !tokens.next()?.eq_ignore_ascii_case("host") {
                return None;
            }
            Some(
                tokens
                    .filter(|pattern| {
                        !pattern.is_empty()
                            && !pattern.contains(['*', '?'])
                            && !pattern.starts_with('!')
                    })
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

/// Confinement decision given config text and an explicit allowlist string
/// (`PI_SSH_ALLOWED_HOSTS`, comma-separated). Pure so tests stay offline.
fn ssh_host_allowed_with(
    host: &str,
    config_text: Option<&str>,
    env_allowlist: Option<&str>,
) -> bool {
    if let Some(env) = env_allowlist
        && env
            .split(',')
            .any(|candidate| candidate.trim().eq_ignore_ascii_case(host))
    {
        return true;
    }
    config_text.is_some_and(|text| {
        ssh_config_literal_hosts(text)
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(host))
    })
}

/// Reads are open (any reachable host); **writes** require the host to be
/// listed literally in `~/.ssh/config` or in `PI_SSH_ALLOWED_HOSTS`.
pub fn ssh_host_allowed(host: &str) -> bool {
    let config_path = std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".ssh").join("config"),
        |home| PathBuf::from(home).join(".ssh").join("config"),
    );
    let config = std::fs::read_to_string(config_path).ok();
    let env = std::env::var("PI_SSH_ALLOWED_HOSTS").ok();
    ssh_host_allowed_with(host, config.as_deref(), env.as_deref())
}

/// Shared ssh invocation flags: no interactive auth possible (BatchMode),
/// bounded connect, and accept-new-then-strict host keys — a *changed* key
/// still hard-fails and is classified by [`classify_ssh_failure`].
#[must_use]
pub fn ssh_command_flags() -> [&'static str; 6] {
    [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]
}

/// Remediation surfaced when a cached host key no longer matches.
pub const SSH_HOSTKEY_REMEDIATION: &str = "The remote host key differs from the cached entry in ~/.ssh/known_hosts. Verify the change out-of-band; if legitimate, remove the stale entry with `ssh-keygen -R <host>` and retry. Pi refuses to proceed automatically.";

/// Failure taxonomy for ssh stderr text (host-key FSM per bd-cv653.6.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshFailureKind {
    /// Cached key mismatch — always a hard failure with remediation.
    HostKeyChanged,
    /// Batch-mode auth rejected (no interactive retry exists).
    AuthFailed,
    /// ConnectTimeout elapsed.
    ConnectTimeout,
    Other,
}

#[must_use]
pub fn classify_ssh_failure(stderr: &str) -> SshFailureKind {
    let lowered = stderr.to_ascii_lowercase();
    if lowered.contains("remote host identification has changed")
        || lowered.contains("host key verification failed")
        || lowered.contains("offending ecdsa")
        || lowered.contains("offending ed25519")
        || lowered.contains("offending rsa")
    {
        SshFailureKind::HostKeyChanged
    } else if lowered.contains("permission denied") {
        SshFailureKind::AuthFailed
    } else if lowered.contains("connection timed out") || lowered.contains("timed out") {
        SshFailureKind::ConnectTimeout
    } else {
        SshFailureKind::Other
    }
}

/// Quote a single argument for consumption by the *remote* POSIX shell.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// POSIX sh snippet writing stdin into `remote_path` atomically: mktemp in
/// the target directory keeps rename(2) on one filesystem, existing
/// permissions are preserved best-effort, and the EXIT trap removes the
/// staging file if anything aborts before the rename.
#[must_use]
pub fn remote_atomic_write_script(remote_path: &str) -> String {
    let quoted = sh_quote(remote_path);
    format!(
        "set -eu; d=$(dirname -- {quoted}); t=$(mktemp \"$d/.pi-ssh-write.XXXXXX\"); \
         trap 'rm -f \"$t\"' EXIT; cat > \"$t\"; \
         if [ -e {quoted} ]; then chmod --reference={quoted} \"$t\"; fi; \
         mv -f -- \"$t\" {quoted}"
    )
}

/// Atomically write `content` to `ssh://host/path`.
///
/// Pipeline: parse/validate target → host confinement → spawn ssh with
/// batch-mode + accept-new flags → stream content over stdin into a remote
/// mktemp staging file → rename over the target.
///
/// # Errors
/// `PI_SSH_HOST_NOT_ALLOWED` (confinement), `PI_SSH_HOSTKEY_CHANGED`
/// (with [`SSH_HOSTKEY_REMEDIATION`]), `PI_SSH_AUTH_FAILED`,
/// `PI_SSH_TIMEOUT`, `PI_SSH_WRITE_FAILED`.
pub fn ssh_write_document(url: &str, content: &str) -> Result<serde_json::Value> {
    let target = parse_ssh_target(url)?;
    if !ssh_host_allowed(&target.host) {
        return Err(Error::tool(
            "write",
            format!(
                "PI_SSH_HOST_NOT_ALLOWED: host '{}' is not authorized for writes. \
                 Add it literally to ~/.ssh/config (Host block) or PI_SSH_ALLOWED_HOSTS.",
                target.host
            ),
        ));
    }
    let script = remote_atomic_write_script(&target.path);
    let mut child = std::process::Command::new("ssh")
        .args(ssh_command_flags())
        .arg(&target.host)
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::tool("write", format!("PI_SSH_BACKEND: failed to run ssh: {e}")))?;
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::tool("write", "PI_SSH_BACKEND: missing ssh stdin"))?;
        stdin
            .write_all(content.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| Error::tool("write", format!("PI_SSH_BACKEND: streaming payload: {e}")))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| Error::tool("write", format!("PI_SSH_BACKEND: waiting on ssh: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(match classify_ssh_failure(&stderr) {
            SshFailureKind::HostKeyChanged => Error::tool(
                "write",
                format!(
                    "PI_SSH_HOSTKEY_CHANGED: {SSH_HOSTKEY_REMEDIATION} (ssh stderr: {})",
                    stderr.trim()
                ),
            ),
            SshFailureKind::AuthFailed => Error::tool(
                "write",
                format!(
                    "PI_SSH_AUTH_FAILED: batch-mode authentication rejected for '{}' (no interactive prompts over ssh://). ssh stderr: {}",
                    target.host,
                    stderr.trim()
                ),
            ),
            SshFailureKind::ConnectTimeout => Error::tool(
                "write",
                format!(
                    "PI_SSH_TIMEOUT: connection to '{}' timed out. ssh stderr: {}",
                    target.host,
                    stderr.trim()
                ),
            ),
            SshFailureKind::Other => Error::tool(
                "write",
                format!(
                    "PI_SSH_WRITE_FAILED: ssh {host} write '{path}' failed: {stderr}",
                    host = target.host,
                    path = target.path,
                    stderr = stderr.trim()
                ),
            ),
        });
    }
    Ok(serde_json::json!({
        "schema": URL_ROUTER_SCHEMA,
        "scheme": "ssh",
        "host": target.host,
        "path": target.path,
        "bytes": content.len(),
        "atomic": true,
    }))
}

/// Fetch the FULL remote file content for edit flows (bd-cv653.6.5).
///
/// Unlike [`resolve_ssh`], there is no `head -c` truncation: an editor
/// operating on a truncated view would overwrite real data on write-back.
/// `max_bytes` is enforced after transfer with a named error instead.
///
/// # Errors
/// `PI_SSH_TARGET`/`PI_SSH_TRAVERSAL` (parse), `PI_SSH_HOSTKEY_CHANGED`,
/// `PI_SSH_AUTH_FAILED`, `PI_SSH_TIMEOUT`, `PI_SSH_READ_FAILED`,
/// `PI_SSH_TOO_LARGE`.
pub fn ssh_fetch_document(url: &str, max_bytes: u64) -> Result<Vec<u8>> {
    let target = parse_ssh_target(url)?;
    let output = std::process::Command::new("ssh")
        .args(ssh_command_flags())
        .arg(&target.host)
        .arg(format!("cat -- {}", sh_quote(&target.path)))
        .output()
        .map_err(|e| Error::tool("edit", format!("PI_SSH_BACKEND: failed to run ssh: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(match classify_ssh_failure(&stderr) {
            SshFailureKind::HostKeyChanged => Error::tool(
                "edit",
                format!("PI_SSH_HOSTKEY_CHANGED: {SSH_HOSTKEY_REMEDIATION} (ssh stderr: {})", stderr.trim()),
            ),
            SshFailureKind::AuthFailed => Error::tool(
                "edit",
                format!(
                    "PI_SSH_AUTH_FAILED: batch-mode authentication rejected for '{}'. ssh stderr: {}",
                    target.host,
                    stderr.trim()
                ),
            ),
            SshFailureKind::ConnectTimeout => Error::tool(
                "edit",
                format!("PI_SSH_TIMEOUT: connection to '{}' timed out. ssh stderr: {}", target.host, stderr.trim()),
            ),
            SshFailureKind::Other => Error::tool(
                "edit",
                format!(
                    "PI_SSH_READ_FAILED: ssh {host} cat '{path}' failed: {stderr}",
                    host = target.host,
                    path = target.path,
                    stderr = stderr.trim()
                ),
            ),
        });
    }
    if output.stdout.len() as u64 > max_bytes {
        return Err(Error::tool(
            "edit",
            format!(
                "PI_SSH_TOO_LARGE: remote file exceeds the {}-byte edit limit.",
                max_bytes
            ),
        ));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo_with_conflict(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-url-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {} failed", args.join(" "));
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "T"]);
        std::fs::write(dir.join("f.txt"), "line\n").expect("write"); // ubs:ignore test fixture
        git(&["add", "."]);
        git(&["commit", "-m", "init"]);
        // Branch A edits, branch B edits the same line → conflict on merge.
        git(&["checkout", "-b", "side"]);
        std::fs::write(dir.join("f.txt"), "side\n").expect("side"); // ubs:ignore test fixture
        git(&["commit", "-am", "side"]);
        git(&["checkout", "main"]);
        std::fs::write(dir.join("f.txt"), "main\n").expect("main"); // ubs:ignore test fixture
        git(&["commit", "-am", "main"]);
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["merge", "side"])
            .output()
            .expect("merge");
        assert!(!out.status.success(), "merge must conflict");
        dir
    }

    #[test]
    fn unknown_scheme_lists_registered() {
        let err = resolve("foo://bar", Path::new(".")).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("PI_URL_UNKNOWN_SCHEME"), "{text}");
        assert!(text.contains("skill://"), "{text}");
    }

    #[test]
    fn conflict_regions_parse_and_select() {
        let repo = init_repo_with_conflict("parse");
        let regions = conflict_regions(&repo).expect("regions");
        assert_eq!(regions.len(), 1);
        let region = &regions[0]; // ubs:ignore length asserted above
        assert_eq!(region.file, "f.txt");
        assert!(region.ours.contains("main"));
        assert!(region.theirs.contains("side"));

        let doc = resolve("conflict://0", &repo).expect("full doc");
        assert!(doc.content.contains("--- ours ---"));
        let doc = resolve("conflict://0 @theirs", &repo).expect("theirs doc");
        assert_eq!(doc.content.trim(), "side");
        let doc = resolve("conflict://*", &repo).expect("bulk");
        assert!(doc.content.contains("conflict 0"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn write_conflict_resolution_picks_a_side() {
        let repo = init_repo_with_conflict("resolve");
        let region = write_conflict_resolution(&repo, 0, "theirs").expect("resolve");
        assert_eq!(region.file, "f.txt");
        let content = std::fs::read_to_string(repo.join("f.txt")).expect("read");
        assert_eq!(content.trim(), "side");
        assert!(!content.contains("<<<<<<<"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn local_scratch_roundtrip() {
        write_local("note", "scratch payload").expect("write");
        let doc = resolve("local://note", Path::new(".")).expect("read");
        assert_eq!(doc.content, "scratch payload");
        let err = resolve("local://missing", Path::new(".")).unwrap_err();
        assert!(err.to_string().contains("PI_URL_UNRESOLVABLE"));
    }

    #[test]
    fn github_reference_parsing() {
        let (repo, number, sub) = parse_repo_number("1428").expect("bare");
        assert_eq!(repo, None);
        assert_eq!(number, "1428");
        assert_eq!(sub, None);
        let (repo, number, sub) = parse_repo_number("owner/repo/1428/diff").expect("full");
        assert_eq!(repo.as_deref(), Some("owner/repo"));
        assert_eq!(number, "1428");
        assert_eq!(sub.as_deref(), Some("diff"));
        assert!(parse_repo_number("notanumber").is_err());
    }
    #[test]
    fn parse_ssh_target_validates() {
        let ok = parse_ssh_target("ssh://yto/var/www/app.js").expect("ok");
        assert_eq!(ok.host, "yto");
        assert_eq!(ok.path, "var/www/app.js");
        assert!(parse_ssh_target("file:///etc/hosts").is_err());
        assert!(parse_ssh_target("ssh://hostonly").is_err());
        assert!(parse_ssh_target("ssh://h/").is_err());
        assert!(parse_ssh_target("ssh://h/a/../b").is_err());
    }

    #[test]
    fn ssh_config_literal_hosts_skips_patterns() {
        let hosts = ssh_config_literal_hosts(
            "Host yto fmd\n  HostName 1.2.3.4\nhost css\nHost *\nHost !blocked\n",
        );
        assert_eq!(hosts, ["yto", "fmd", "css"]); // ubs:ignore length asserted
    }

    #[test]
    fn ssh_host_confinement_sources() {
        let config = Some("Host yto\n");
        assert!(ssh_host_allowed_with("yto", config, None));
        assert!(ssh_host_allowed_with("YTO", config, None)); // case-insensitive
        assert!(!ssh_host_allowed_with("evil", config, None));
        assert!(ssh_host_allowed_with("csd", None, Some("a, csd ,b")));
        assert!(!ssh_host_allowed_with("x", Some("Host *\n"), None));
    }

    #[test]
    fn ssh_flags_force_batch_and_accept_new() {
        let flags = ssh_command_flags();
        assert!(flags.contains(&"BatchMode=yes"));
        assert!(flags.contains(&"StrictHostKeyChecking=accept-new"));
    }

    #[test]
    fn remote_write_script_stages_atomically() {
        let script = remote_atomic_write_script("/var/www/app.conf");
        assert!(script.contains("mktemp \"$d/.pi-ssh-write.XXXXXX\""));
        assert!(script.contains("mv -f -- \"$t\" '/var/www/app.conf'"));
        assert!(script.contains("chmod --reference='/var/www/app.conf'"));
        assert!(script.contains("trap 'rm -f \"$t\"' EXIT"));
        // Quote escaping survives embedded single quotes.
        let tricky = remote_atomic_write_script("/tmp/it's");
        assert!(tricky.contains("'\\''"), "{tricky}");
    }

    #[test]
    fn ssh_failures_classify() {
        assert_eq!(
            classify_ssh_failure("@@@@@@\r\nRemote host identification has changed."),
            SshFailureKind::HostKeyChanged
        );
        assert_eq!(
            classify_ssh_failure("Permission denied (publickey)."),
            SshFailureKind::AuthFailed
        );
        assert_eq!(
            classify_ssh_failure("Connection timed out"),
            SshFailureKind::ConnectTimeout
        );
        assert_eq!(
            classify_ssh_failure("bash: x: command not found"),
            SshFailureKind::Other
        );
    }

    #[test]
    fn ssh_fetch_rejects_bad_targets_before_spawning() {
        // Parse/validation happens before any process spawn, so these are
        // offline-safe assertions of the guard rails.
        assert!(ssh_fetch_document("file:///etc/hosts", 1024).is_err());
        assert!(ssh_fetch_document("ssh://h/a/../b", 1024).is_err());
    }
}
