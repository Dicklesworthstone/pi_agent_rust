//! Validate native dependency-audit SARIF before using it as a baseline.
//! Known fields are deserialized directly, so duplicate identities/completion
//! fields cannot be collapsed by an intermediate JSON Value. Unused SARIF text
//! is never copied into the comparison's model-facing output.

use super::{Result, error};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

const AUDIT_SCHEMA: &str = "pi.security-dependency-audit/v1";
const SCOPE: &str = "exact public-registry versions in the selected lockfiles; no installed-code or reachability claim";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Key {
    pub ecosystem: String,
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Location {
    lockfile: String,
    package_path: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Package {
    ecosystem: String,
    name: String,
    version: String,
    locations: Vec<Location>,
}

impl Package {
    fn key(&self) -> Key {
        Key {
            ecosystem: self.ecosystem.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
struct Excluded {
    location: Location,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lockfile {
    path: String,
    format: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    schema: String,
    scope: String,
    lockfiles: Vec<Lockfile>,
    packages: Vec<Package>,
    excluded: Vec<Excluded>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Match {
    pub package_index: usize,
    pub ecosystem: String,
    pub name: String,
    pub version: String,
    pub advisory_id: String,
    pub modified: String,
    pub fingerprint: String,
    pub withdrawn: Option<bool>,
    pub metadata_available: bool,
}

impl Match {
    pub fn key(&self) -> Key {
        Key {
            ecosystem: self.ecosystem.clone(),
            name: self.name.clone(),
            version: self.version.clone(),
        }
    }
}

#[derive(Deserialize)]
struct Fingerprints {
    #[serde(rename = "pi/dependency/v1")]
    dependency: String,
}

#[derive(Deserialize)]
struct FindingProperties {
    dependency: Match,
    #[serde(rename = "piDependencyReview")]
    review: Option<super::disposition::Record>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Finding {
    rule_id: String,
    fingerprints: Fingerprints,
    properties: FindingProperties,
    #[serde(default)]
    suppressions: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Properties {
    schema: String,
    observed_at: String,
    status: String,
    query_complete: bool,
    metadata_complete: bool,
    completed_queries: usize,
    inventory: Inventory,
    query_completion: Vec<bool>,
}

#[derive(Deserialize)]
struct Driver {
    name: String,
}
#[derive(Deserialize)]
struct Tool {
    driver: Driver,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Invocation {
    execution_successful: bool,
}
#[derive(Deserialize)]
struct Run {
    tool: Tool,
    invocations: Vec<Invocation>,
    properties: Properties,
    results: Vec<Finding>,
}
#[derive(Deserialize)]
struct Document {
    version: String,
    runs: Vec<Run>,
}

pub(super) struct Snapshot {
    pub digest: String,
    pub observed_at: String,
    pub status: String,
    pub query_complete: bool,
    pub metadata_complete: bool,
    pub packages: BTreeMap<Key, bool>,
    pub findings: BTreeMap<String, Match>,
    pub reviews: BTreeMap<String, super::disposition::Record>,
    lockfiles: BTreeMap<String, (String, String)>,
    locations: BTreeMap<Key, BTreeSet<Location>>,
    excluded: Vec<Excluded>,
}

pub(super) fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

fn fingerprint(key: &Key, advisory: &str) -> String {
    let mut bytes = Vec::new();
    for part in ["pi/dependency/v1", &key.ecosystem, &key.name, &key.version, advisory] {
        bytes.extend_from_slice(part.as_bytes());
        bytes.push(0);
    }
    digest(&bytes)
}

fn hex_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn date(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>> {
    if value.len() > 80 {
        return Err(error("invalid report timestamp"));
    }
    chrono::DateTime::parse_from_rfc3339(value).map_err(|_| error("invalid report timestamp"))
}

fn lockfile_format(path: &str) -> Result<&'static str> {
    if path.is_empty() || path.len() > 4096 || path.contains(['\\', ':'])
        || path.chars().any(char::is_control)
        || Path::new(path).components().any(|part| !matches!(part, Component::Normal(_)))
        || path.split('/').any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(error("invalid lockfile identity in report"));
    }
    match Path::new(path).file_name().and_then(|name| name.to_str()) {
        Some("Cargo.lock") => Ok("cargo"),
        Some("package-lock.json" | "npm-shrinkwrap.json") => Ok("npm"),
        _ => Err(error("unsupported lockfile identity in report")),
    }
}

fn location_valid(location: &Location, lockfiles: &BTreeMap<String, (String, String)>) -> bool {
    lockfiles.contains_key(&location.lockfile)
        && location.package_path.as_ref().is_none_or(|path| {
            !path.is_empty() && path.len() <= 4096 && !path.chars().any(char::is_control)
        })
}

impl Snapshot {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > super::MAX_REPORT_BYTES {
            return Err(error("dependency report exceeds 16 MiB"));
        }
        let document: Document = serde_json::from_slice(bytes)
            .map_err(|_| error("malformed native dependency SARIF; no parser contents are exposed"))?;
        if document.version != crate::security_scan::SARIF_VERSION || document.runs.len() != 1 {
            return Err(error("expected exactly one native dependency-audit SARIF run"));
        }
        let run = document.runs.into_iter().next().expect("one validated run");
        Self::from_run(run, digest(bytes))
    }

    fn from_run(run: Run, digest: String) -> Result<Self> {
        let props = run.properties;
        if run.tool.driver.name != "pi dependency audit" || props.schema != AUDIT_SCHEMA
            || props.inventory.schema != crate::security_scan::dependencies::INVENTORY_SCHEMA
            || props.inventory.scope != SCOPE || run.invocations.len() != 1
        {
            return Err(error("not a supported native dependency audit"));
        }
        date(&props.observed_at)?;
        let count = props.inventory.packages.len();
        let complete = count > 0 && props.query_completion.iter().all(|complete| *complete);
        let status = if count == 0 { "not_checked" } else if !complete { "incomplete" }
            else if !props.metadata_complete { "complete_with_metadata_gaps" } else { "complete" };
        if count > 10_000 || props.query_completion.len() != count
            || props.query_complete != complete || props.status != status
            || props.completed_queries != props.query_completion.iter().filter(|complete| **complete).count()
            || run.invocations[0].execution_successful != complete
            || (props.metadata_complete && !complete) || run.results.len() > 2000
        {
            return Err(error("contradictory or oversized dependency query-completion record"));
        }
        let mut snapshot = Self {
            digest, observed_at: props.observed_at, status: props.status,
            query_complete: complete, metadata_complete: props.metadata_complete,
            packages: BTreeMap::new(), findings: BTreeMap::new(),
            reviews: BTreeMap::new(),
            lockfiles: BTreeMap::new(), locations: BTreeMap::new(), excluded: props.inventory.excluded,
        };
        snapshot.admit_inventory(props.inventory.lockfiles, &props.inventory.packages, &props.query_completion)?;
        for result in run.results {
            let finding = result.properties.dependency;
            let key = finding.key();
            if !snapshot.packages.contains_key(&key)
                || props.inventory.packages.get(finding.package_index).map(Package::key).as_ref() != Some(&key)
                || finding.advisory_id.is_empty() || finding.advisory_id.len() > 128
                || matches!(finding.advisory_id.as_str(), "." | "..")
                || !finding.advisory_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                || result.rule_id != format!("osv/{}", finding.advisory_id)
                || result.fingerprints.dependency != finding.fingerprint
                || finding.fingerprint != fingerprint(&key, &finding.advisory_id)
                || finding.metadata_available != finding.withdrawn.is_some()
                || (snapshot.metadata_complete && !finding.metadata_available)
            {
                return Err(error("dependency finding identity or metadata disagrees with its inventory"));
            }
            date(&finding.modified)?;
            super::disposition::validate_record(result.properties.review.as_ref(), &finding,
                result.suppressions.as_deref().unwrap_or(&[]), &snapshot.observed_at)?;
            if let Some(record) = result.properties.review {
                snapshot.reviews.insert(finding.fingerprint.clone(), record);
            }
            if snapshot.findings.insert(finding.fingerprint.clone(), finding).is_some() {
                return Err(error("duplicate dependency finding identity"));
            }
        }
        Ok(snapshot)
    }

    fn admit_inventory(&mut self, files: Vec<Lockfile>, packages: &[Package], completion: &[bool]) -> Result<()> {
        if files.is_empty() || files.len() > 32 || self.excluded.len() > 20_000 {
            return Err(error("invalid dependency inventory size"));
        }
        for file in files {
            if file.format != lockfile_format(&file.path)? || !hex_digest(&file.sha256)
                || self.lockfiles.insert(file.path, (file.format, file.sha256)).is_some()
            {
                return Err(error("duplicate or invalid lockfile identity"));
            }
        }
        let mut locations = self.excluded.len();
        for (package, complete) in packages.iter().zip(completion) {
            let key = package.key();
            locations = locations.checked_add(package.locations.len()).ok_or_else(|| error("inventory limit"))?;
            if !matches!(key.ecosystem.as_str(), "crates.io" | "npm")
                || !crate::security_scan::dependencies::valid_name(&key.ecosystem, &key.name)
                || key.version.len() > 256 || semver::Version::parse(&key.version).is_err()
                || locations > 20_000 || package.locations.is_empty()
                || self.packages.insert(key.clone(), *complete).is_some()
            {
                return Err(error("invalid or duplicate public package identity"));
            }
            let mut unique = BTreeSet::new();
            for location in &package.locations {
                if !location_valid(location, &self.lockfiles)
                    || !unique.insert(location.clone())
                    || self.lockfiles[&location.lockfile].0 != if key.ecosystem == "npm" { "npm" } else { "cargo" }
                    || (key.ecosystem == "crates.io" && location.package_path.is_some())
                    || (key.ecosystem == "npm" && location.package_path.is_none())
                {
                    return Err(error("invalid or duplicate package location"));
                }
            }
            self.locations.insert(key, unique);
        }
        for excluded in &self.excluded {
            if !location_valid(&excluded.location, &self.lockfiles) || excluded.reason.is_empty()
                || excluded.reason.len() > 128
                || !excluded.reason.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            {
                return Err(error("invalid dependency exclusion"));
            }
        }
        self.excluded.sort();
        Ok(())
    }

    pub fn check_pair(&self, current: &Self) -> Result<()> {
        if self.lockfiles.iter().map(|(path, (format, _))| (path, format)).ne(
            current.lockfiles.iter().map(|(path, (format, _))| (path, format)))
        {
            return Err(error("selected lockfile scopes differ; audit the same paths before comparing"));
        }
        if date(&self.observed_at)? > date(&current.observed_at)? {
            return Err(error("current audit predates baseline; swap the reports"));
        }
        Ok(())
    }

    pub fn locations_changed(&self, current: &Self, key: &Key) -> bool {
        self.locations.get(key) != current.locations.get(key)
    }

    pub fn exclusions_changed(&self, current: &Self) -> bool {
        self.excluded != current.excluded
    }

    pub fn lockfiles_changed(&self, current: &Self) -> bool {
        self.lockfiles != current.lockfiles
    }

    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({"sha256":self.digest,"observedAt":self.observed_at,"status":self.status,
            "queryComplete":self.query_complete,"metadataComplete":self.metadata_complete,
            "lockfiles":self.lockfiles.len(),"eligiblePackages":self.packages.len(),
            "completedQueries":self.packages.values().filter(|complete| **complete).count(),
            "advisoryMatches":self.findings.len(),"excludedEntries":self.excluded.len(),
            "reviewedFindings":self.reviews.len()})
    }
}
