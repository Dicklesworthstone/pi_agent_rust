//! Internal URL routing for document resources and native agent results.
//!
//! `agent://` lists this parent's children. `agent://<id>` reads an accepted
//! output (or explicit lifecycle status), and `agent://<id>/findings/0/path`
//! projects JSON without reparsing prose or trusting provisional transcripts.
//! `skill://<name>/<path>` reads supporting files or directory inventories;
//! bare skill names continue to read their instructions. Document, Git,
//! conflict and SSH schemes retain their existing contracts.

pub(crate) mod agent;
mod conflicts;
mod documents;
mod skills;

pub use documents::{
    ConflictRegion, ResolveOptions, ResolvedDoc, SSH_HOSTKEY_REMEDIATION, SshFailureKind,
    SshTarget, TransferEndpoint, URL_ROUTER_SCHEMA, classify_ssh_failure, has_scheme,
    parse_ssh_target, parse_transfer_endpoint, remote_atomic_write_script, resume_offset,
    ssh_command_flags, ssh_fetch_document, ssh_host_allowed, ssh_replace_document, ssh_transfer,
    ssh_write_allowed_hosts, ssh_write_document, write_local,
};

pub(crate) use conflicts::prepare_conflict_write;
pub use conflicts::{
    ConflictChoice, ConflictResolution, apply_conflict_choices, conflict_regions,
    write_conflict_resolution,
};

use crate::error::{Error, Result};
use std::path::Path;

fn agent_reference(path: &str) -> Result<Option<&str>> {
    let Some((scheme, reference)) = path.split_once("://") else {
        return Ok(None);
    };
    match scheme {
        "agent" => Ok(Some(reference)),
        "skill" | "prompt" | "local" | "conflict" | "pr" | "issue" | "ssh" => Ok(None),
        _ if has_scheme(path) => Err(Error::tool(
            "read",
            format!(
                "PI_URL_UNKNOWN_SCHEME: unknown scheme '{scheme}://'. Registered schemes: \
                 agent://, skill://, prompt://, local://, conflict://, pr://, issue://, ssh://"
            ),
        )),
        _ => Ok(None),
    }
}

fn resolve_agent(reference: &str) -> Result<ResolvedDoc> {
    if crate::agent_hub::ipc::inherited_channel_present() {
        // A partial, expired or invalid parent channel never falls back to
        // a different local hub with potentially colliding child display IDs.
        crate::agent_hub::ipc::results::resolve_from_env(reference)
    } else {
        agent::resolve(reference)
    }
}

/// Parent-side result dispatch must not forward into a grandparent channel.
/// The caller has already authenticated its live child; ordinary acceptance,
/// JSON projection, retention and redaction remain owned by the local resolver.
pub(crate) fn resolve_local_agent(reference: &str) -> Result<ResolvedDoc> {
    agent::resolve(reference)
}

/// Resolve a scheme into a paginatable virtual document.
///
/// # Errors
/// Unknown schemes and unavailable resources return named errors. Agent
/// projections distinguish pending, rejected, unavailable and missing data.
/// Resolution never silently falls back to the filesystem.
pub fn resolve(path: &str, cwd: &Path) -> Result<ResolvedDoc> {
    if let Some(reference) = path.strip_prefix("conflict://") {
        return conflicts::resolve(reference, cwd);
    }
    if let Some(reference) = path.strip_prefix("skill://") {
        return skills::resolve(reference, cwd);
    }
    match agent_reference(path)? {
        Some(reference) => resolve_agent(reference),
        None => documents::resolve(path, cwd),
    }
}

/// Resolve with explicit document backend overrides.
///
/// # Errors
/// See [`resolve`]. Overrides do not grant access to another parent's agents.
pub fn resolve_with(path: &str, cwd: &Path, options: &ResolveOptions) -> Result<ResolvedDoc> {
    if let Some(reference) = path.strip_prefix("conflict://") {
        return conflicts::resolve(reference, cwd);
    }
    if let Some(reference) = path.strip_prefix("skill://") {
        return skills::resolve(reference, cwd);
    }
    match agent_reference(path)? {
        Some(reference) => resolve_agent(reference),
        None => documents::resolve_with(path, cwd, options),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_schemes_advertise_agent_results_without_falling_back() {
        let error = resolve("missing://result", Path::new("."))
            .unwrap_err()
            .to_string();
        assert!(error.contains("PI_URL_UNKNOWN_SCHEME"));
        assert!(error.contains("agent://"));
        assert!(error.contains("ssh://"));
    }

    #[test]
    fn default_and_overridden_routes_both_reach_the_agent_handler() {
        let path = format!("agent://missing-{}", uuid::Uuid::new_v4());
        for error in [
            resolve(&path, Path::new(".")).unwrap_err(),
            resolve_with(&path, Path::new("."), &ResolveOptions::default()).unwrap_err(),
        ] {
            assert!(error.to_string().contains("PI_URL_AGENT_NOT_FOUND"));
        }
    }
}
