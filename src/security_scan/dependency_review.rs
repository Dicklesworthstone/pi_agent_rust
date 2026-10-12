//! Offline review of native dependency-audit artifacts. Absence in an
//! unfinished lookup is uncertainty, never a resolved vulnerability.

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

mod disposition;
mod snapshot;
pub(super) use disposition::execute as apply_disposition;
use snapshot::{Match, Snapshot};

const MAX_REPORT_BYTES: usize = 16 * 1024 * 1024;
const COMPARISON_SCHEMA: &str = "pi.security-dependency-comparison/v1";
const STATES: [&str; 6] = [
    "persistent", "newly_observed", "new_package", "no_longer_observed",
    "package_not_in_current_inventory", "unknown",
];

fn error(message: &str) -> Error {
    Error::tool("security_scan", format!("[DEPENDENCY_REVIEW] {message}"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompareInput {
    #[serde(rename = "op")]
    _op: String,
    baseline: String,
    current: String,
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
}

fn checkpoint(owner: &AgentCx) -> Result<()> {
    owner.checkpoint().map_err(|_| error("dependency review cancelled"))
}

pub(super) async fn compare(cwd: &Path, input: Value) -> Result<ToolOutput> {
    let input: CompareInput = serde_json::from_value(input)
        .map_err(|_| error("compare_dependencies requires baseline/current SARIF filenames and optional offset/limit"))?;
    let limit = input.limit.unwrap_or(20);
    if !(1..=50).contains(&limit) {
        return Err(error("comparison limit must be in 1..=50"));
    }
    validate_name(&input.baseline)?;
    validate_name(&input.current)?;
    let owner = AgentCx::for_current_or_request();
    if !owner.capabilities().io {
        return Err(error("dependency review requires I/O capability"));
    }
    checkpoint(&owner)?;
    let workspace = cwd.to_path_buf();
    let worker = owner.clone();
    let output = asupersync::runtime::spawn_blocking(move || {
        checkpoint(&worker)?;
        // Pin one directory for both reads, not one pathname per artifact.
        let root = Root::open(&workspace)?;
        let before = Snapshot::parse(&root.read(&input.baseline)?)?;
        checkpoint(&worker)?;
        let after = Snapshot::parse(&root.read(&input.current)?)?;
        checkpoint(&worker)?;
        compare_snapshots(&before, &after, input.offset, limit)
    }).await?;
    checkpoint(&owner)?;
    Ok(output)
}

fn observation(finding: Option<&Match>) -> Value {
    finding.map_or(Value::Null, |finding| json!({
        "modified":finding.modified,"withdrawn":finding.withdrawn,
        "metadataAvailable":finding.metadata_available
    }))
}

fn classify<'a>(
    before: &Snapshot,
    after: &Snapshot,
    old: Option<&'a Match>,
    new: Option<&'a Match>,
) -> (&'a Match, &'static str, &'static str) {
    match (old, new) {
        (Some(old), Some(_)) => (old, "persistent", "matched_in_both_reports"),
        (None, Some(new)) => match before.packages.get(&new.key()) {
            Some(true) => (new, "newly_observed", "baseline_query_completed_without_this_match"),
            Some(false) => (new, "unknown", "baseline_query_incomplete"),
            None => (new, "new_package", "exact_version_not_in_baseline_inventory"),
        },
        (Some(old), None) => match after.packages.get(&old.key()) {
            Some(true) => (old, "no_longer_observed", "current_query_completed_without_this_match"),
            Some(false) => (old, "unknown", "current_query_incomplete"),
            None => (old, "package_not_in_current_inventory", "removed_upgraded_or_excluded_not_proven_fixed"),
        },
        (None, None) => unreachable!("union of observed fingerprints"),
    }
}

fn compare_snapshots(before: &Snapshot, after: &Snapshot, offset: usize, limit: usize) -> Result<ToolOutput> {
    before.check_pair(after)?;
    let fingerprints: BTreeSet<_> = before.findings.keys().chain(after.findings.keys()).collect();
    let mut entries: Vec<_> = fingerprints.into_iter().map(|fingerprint| {
        let old = before.findings.get(fingerprint);
        let new = after.findings.get(fingerprint);
        let (finding, state, reason) = classify(before, after, old, new);
        (finding, state, reason, old, new)
    }).collect();
    entries.sort_by(|a, b| {
        (&a.0.ecosystem, &a.0.name, &a.0.version, &a.0.advisory_id)
            .cmp(&(&b.0.ecosystem, &b.0.name, &b.0.version, &b.0.advisory_id))
    });
    let mut counts: BTreeMap<&str, usize> = STATES.into_iter().map(|state| (state, 0)).collect();
    for (_, state, _, _, _) in &entries {
        *counts.get_mut(state).expect("closed comparison state") += 1;
    }
    let page: Vec<_> = entries.iter().skip(offset).take(limit).map(|(finding, state, reason, old, new)| {
        json!({"ecosystem":finding.ecosystem,"name":finding.name,"version":finding.version,
            "advisoryId":finding.advisory_id,"fingerprint":finding.fingerprint,
            "state":state,"reason":reason,"baseline":observation(*old),"current":observation(*new),
            "baselineDecision":before.reviews.get(&finding.fingerprint).map(|record|record.decision),
            "currentDecision":after.reviews.get(&finding.fingerprint).map(|record|record.decision),
            "locationsChanged":old.is_some() && new.is_some() && before.locations_changed(after, &finding.key()),
            "advisoryChanged":old.zip(*new).is_some_and(|(a,b)| a.modified != b.modified
                || a.withdrawn != b.withdrawn || a.metadata_available != b.metadata_available)})
    }).collect();
    // Offset can be usize::MAX: no unchecked addition or allocation follows it.
    let next = offset.checked_add(page.len()).filter(|next| *next < entries.len());
    let complete = before.query_complete && after.query_complete;
    let details = json!({"schema":COMPARISON_SCHEMA,"baseline":before.summary(),"current":after.summary(),
        "queryComplete":complete,"counts":counts,"total":entries.len(),"offset":offset,
        "limit":limit,"nextOffset":next,"findings":page,
        "lockfilesChanged":before.lockfiles_changed(after),"exclusionsChanged":before.exclusions_changed(after),
        "interpretation":"Exact ecosystem/name/version/advisory identities; versions are not merged. Missing matches are observations, not proof of remediation. Package removal, upgrades, exclusions, and advisory-database changes can alter results. Suppressed and withdrawn matches are retained."});
    let text = format!(
        "Dependency comparison: {} persistent, {} newly observed, {} on new package versions, {} no longer observed, {} no longer in the eligible inventory, {} unknown. {} of {} entries in this page. No longer observed does not mean fixed; incomplete queries never establish absence. No network request or file mutation was performed.",
        counts["persistent"], counts["newly_observed"], counts["new_package"], counts["no_longer_observed"],
        counts["package_not_in_current_inventory"], counts["unknown"],
        details["findings"].as_array().map_or(0, Vec::len), entries.len()
    );
    let output = ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(text))],
        details: Some(details), is_error: !complete,
    };
    if serde_json::to_vec(&output)?.len() > crate::tools::DEFAULT_MAX_BYTES {
        return Err(error("comparison page exceeds the output budget; request a smaller limit"));
    }
    Ok(output)
}

// Match the audit exporter's root-level naming contract. No source or model
// selected report may cause arbitrary filesystem reads or symlink traversal.
#[allow(clippy::case_sensitive_file_extension_comparisons)] // Same lowercase contract as audit export.
fn validate_name(name: &str) -> Result<()> {
    if name.len() > 240 || name.len() <= ".sarif".len() || !name.ends_with(".sarif")
        || name.starts_with('.') || name.contains(['/', '\\', ':']) || name.chars().any(char::is_control)
    {
        return Err(error("report paths must be root-level .sarif filenames of at most 240 bytes"));
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
struct Root(rustix::fd::OwnedFd);

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
impl Root {
    fn open(cwd: &Path) -> Result<Self> {
        use rustix::fs::{Mode, OFlags};
        rustix::fs::open(cwd, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map(Self).map_err(|_| error("cannot pin dependency report directory"))
    }

    fn read(&self, name: &str) -> Result<Vec<u8>> {
        use rustix::fs::{Mode, OFlags};
        use std::io::Read as _;
        validate_name(name)?;
        let fd = rustix::fs::openat(&self.0, name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK, Mode::empty())
            .map_err(|_| error("cannot open dependency report without following links"))?;
        let file = std::fs::File::from(fd);
        let metadata = file.metadata().map_err(|_| error("cannot inspect dependency report"))?;
        if !metadata.is_file() || metadata.len() > MAX_REPORT_BYTES as u64 {
            return Err(error("dependency report must be a regular file of at most 16 MiB"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_REPORT_BYTES as u64 + 1).read_to_end(&mut bytes)
            .map_err(|_| error("dependency report read failed"))?;
        if bytes.len() > MAX_REPORT_BYTES {
            return Err(error("dependency report grew beyond 16 MiB"));
        }
        Ok(bytes)
    }
}

#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
struct Root;
#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
impl Root {
    fn open(_cwd: &Path) -> Result<Self> {
        Err(error("confined dependency review requires supported Unix descriptor APIs"))
    }
    #[allow(clippy::unused_self)]
    fn read(&self, _name: &str) -> Result<Vec<u8>> {
        Err(error("unsupported descriptor backend"))
    }
}

#[cfg(test)]
mod tests;
