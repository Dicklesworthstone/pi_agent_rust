//! Extension discovery index (offline-first).
//!
//! This module provides a local, searchable index of available extensions. The index is:
//! - **Offline-first**: Pi ships a bundled seed index embedded at compile time.
//! - **Fail-open**: cache load/refresh failures should never break discovery.
//! - **Host-agnostic**: the index is primarily a data structure; CLI commands live elsewhere.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::client::Client;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::NamedTempFile;

pub const EXTENSION_INDEX_SCHEMA: &str = "pi.ext.index.v1";
pub const EXTENSION_INDEX_VERSION: u32 = 1;
pub const DEFAULT_INDEX_MAX_AGE: Duration = Duration::from_secs(60 * 60 * 24);
const DEFAULT_NPM_QUERY: &str = "keywords:pi-extension";
const DEFAULT_GITHUB_QUERY: &str = "topic:pi-extension";
const DEFAULT_REMOTE_LIMIT: usize = 100;
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionIndex {
    pub schema: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refreshed_at: Option<String>,
    #[serde(default)]
    pub entries: Vec<ExtensionIndexEntry>,
}

impl ExtensionIndex {
    #[must_use]
    pub fn new_empty() -> Self {
        Self {
            schema: EXTENSION_INDEX_SCHEMA.to_string(),
            version: EXTENSION_INDEX_VERSION,
            generated_at: Some(Utc::now().to_rfc3339()),
            last_refreshed_at: None,
            entries: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != EXTENSION_INDEX_SCHEMA {
            return Err(Error::validation(format!(
                "Unsupported extension index schema: {}",
                self.schema
            )));
        }
        if self.version != EXTENSION_INDEX_VERSION {
            return Err(Error::validation(format!(
                "Unsupported extension index version: {}",
                self.version
            )));
        }
        Ok(())
    }

    #[must_use]
    pub fn is_stale(&self, now: DateTime<Utc>, max_age: Duration) -> bool {
        let Some(ts) = &self.last_refreshed_at else {
            return true;
        };
        let Ok(parsed) = DateTime::parse_from_rfc3339(ts) else {
            return true;
        };
        let parsed = parsed.with_timezone(&Utc);
        now.signed_duration_since(parsed)
            .to_std()
            .map_or(true, |age| age > max_age)
    }

    /// Resolve a unique `installSource` for an id/name, if present.
    ///
    /// This is used to support ergonomic forms like `pi install checkpoint-pi` without requiring
    /// users to spell out `npm:` / `git:` prefixes. If resolution is ambiguous, returns `None`.
    #[must_use]
    pub fn resolve_install_source(&self, query: &str) -> Option<String> {
        let q = query.trim();
        if q.is_empty() {
            return None;
        }
        let q_lc = q.to_ascii_lowercase();

        let mut sources: BTreeSet<String> = BTreeSet::new();
        for entry in &self.entries {
            let Some(install) = &entry.install_source else {
                continue;
            };

            if entry.name.eq_ignore_ascii_case(q) || entry.id.eq_ignore_ascii_case(q) {
                sources.insert(install.clone());
                continue;
            }

            // Convenience: `npm/<name>` or `<name>` for npm entries.
            if let Some(ExtensionIndexSource::Npm { package, .. }) = &entry.source {
                if package.to_ascii_lowercase() == q_lc {
                    sources.insert(install.clone());
                    continue;
                }
            }

            if let Some(rest) = entry.id.strip_prefix("npm/") {
                if rest.eq_ignore_ascii_case(q) {
                    sources.insert(install.clone());
                }
            }
        }

        if sources.len() == 1 {
            sources.into_iter().next()
        } else {
            None
        }
    }

    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<ExtensionSearchHit> {
        let q = query.trim();
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }

        let tokens = q
            .split_whitespace()
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>();
        if tokens.is_empty() {
            return Vec::new();
        }

        let mut hits = self
            .entries
            .iter()
            .filter_map(|entry| {
                let score = score_entry(entry, &tokens);
                if score <= 0 {
                    None
                } else {
                    Some(ExtensionSearchHit {
                        entry: entry.clone(),
                        score,
                    })
                }
            })
            .collect::<Vec<_>>();

        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| {
                    b.entry
                        .install_source
                        .is_some()
                        .cmp(&a.entry.install_source.is_some())
                })
                .then_with(|| {
                    a.entry
                        .name
                        .to_ascii_lowercase()
                        .cmp(&b.entry.name.to_ascii_lowercase())
                })
                .then_with(|| {
                    a.entry
                        .id
                        .to_ascii_lowercase()
                        .cmp(&b.entry.id.to_ascii_lowercase())
                })
        });

        hits.truncate(limit);
        hits
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionIndexEntry {
    /// Globally unique id within the index (stable key).
    pub id: String,
    /// Primary display name (often npm package name or repo name).
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ExtensionIndexSource>,
    /// Optional source string compatible with Pi's package manager (e.g. `npm:pkg@ver`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ExtensionIndexSource {
    Npm {
        package: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    Git {
        repo: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#ref: Option<String>,
    },
    Url {
        url: String,
    },
}

#[derive(Debug, Clone)]
pub struct ExtensionSearchHit {
    pub entry: ExtensionIndexEntry,
    pub score: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ExtensionIndexRefreshStats {
    pub npm_entries: usize,
    pub github_entries: usize,
    pub merged_entries: usize,
    pub refreshed: bool,
}

fn score_entry(entry: &ExtensionIndexEntry, tokens: &[String]) -> i64 {
    let name = entry.name.to_ascii_lowercase();
    let id = entry.id.to_ascii_lowercase();
    let description = entry
        .description
        .as_ref()
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    let tags = entry
        .tags
        .iter()
        .map(|t| t.to_ascii_lowercase())
        .collect::<Vec<_>>();

    let mut score: i64 = 0;
    for token in tokens {
        if name.contains(token) {
            score += 300;
        }
        if id.contains(token) {
            score += 120;
        }
        if description.contains(token) {
            score += 60;
        }
        if tags.iter().any(|t| t.contains(token)) {
            score += 180;
        }
    }

    score
}

#[derive(Debug, Clone)]
pub struct ExtensionIndexStore {
    path: PathBuf,
}

impl ExtensionIndexStore {
    #[must_use]
    pub const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn default_path() -> PathBuf {
        Config::extension_index_path()
    }

    #[must_use]
    pub fn default_store() -> Self {
        Self::new(Self::default_path())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<ExtensionIndex>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&self.path)?;
        let index: ExtensionIndex = serde_json::from_str(&content)?;
        index.validate()?;
        Ok(Some(index))
    }

    pub fn load_or_seed(&self) -> Result<ExtensionIndex> {
        match self.load() {
            Ok(Some(index)) => Ok(index),
            Ok(None) => seed_index(),
            Err(err) => {
                tracing::warn!(
                    "failed to load extension index cache (falling back to seed): {err}"
                );
                seed_index()
            }
        }
    }

    pub fn save(&self, index: &ExtensionIndex) -> Result<()> {
        index.validate()?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
            let mut tmp = NamedTempFile::new_in(parent)?;
            let encoded = serde_json::to_string_pretty(index)?;
            tmp.write_all(encoded.as_bytes())?;
            tmp.flush()?;
            tmp.persist(&self.path)
                .map(|_| ())
                .map_err(|e| Error::from(Box::new(e.error)))
        } else {
            Err(Error::config(format!(
                "Invalid extension index path: {}",
                self.path.display()
            )))
        }
    }

    pub fn resolve_install_source(&self, query: &str) -> Result<Option<String>> {
        let index = self.load_or_seed()?;
        Ok(index.resolve_install_source(query))
    }

    pub async fn load_or_refresh_best_effort(
        &self,
        client: &Client,
        max_age: Duration,
    ) -> Result<ExtensionIndex> {
        let current = self.load_or_seed()?;
        if current.is_stale(Utc::now(), max_age) {
            let (refreshed, _) = self.refresh_best_effort(client).await?;
            return Ok(refreshed);
        }
        Ok(current)
    }

    pub async fn refresh_best_effort(
        &self,
        client: &Client,
    ) -> Result<(ExtensionIndex, ExtensionIndexRefreshStats)> {
        let mut current = self.load_or_seed()?;

        let npm_entries = match fetch_npm_entries(client, DEFAULT_REMOTE_LIMIT).await {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!("npm extension index refresh failed: {err}");
                Vec::new()
            }
        };
        let github_entries = match fetch_github_entries(client, DEFAULT_REMOTE_LIMIT).await {
            Ok(entries) => entries,
            Err(err) => {
                tracing::warn!("github extension index refresh failed: {err}");
                Vec::new()
            }
        };

        let npm_count = npm_entries.len();
        let github_count = github_entries.len();
        if npm_count == 0 && github_count == 0 {
            return Ok((
                current,
                ExtensionIndexRefreshStats {
                    npm_entries: 0,
                    github_entries: 0,
                    merged_entries: 0,
                    refreshed: false,
                },
            ));
        }

        current.entries = merge_entries(current.entries, npm_entries, github_entries);
        current.last_refreshed_at = Some(Utc::now().to_rfc3339());
        if let Err(err) = self.save(&current) {
            tracing::warn!("failed to persist refreshed extension index cache: {err}");
        }

        Ok((
            current.clone(),
            ExtensionIndexRefreshStats {
                npm_entries: npm_count,
                github_entries: github_count,
                merged_entries: current.entries.len(),
                refreshed: true,
            },
        ))
    }
}

fn merge_entries(
    existing: Vec<ExtensionIndexEntry>,
    npm_entries: Vec<ExtensionIndexEntry>,
    github_entries: Vec<ExtensionIndexEntry>,
) -> Vec<ExtensionIndexEntry> {
    let mut by_id = BTreeMap::<String, ExtensionIndexEntry>::new();
    for entry in existing {
        by_id.insert(entry.id.to_ascii_lowercase(), entry);
    }

    for incoming in npm_entries.into_iter().chain(github_entries) {
        let key = incoming.id.to_ascii_lowercase();
        if let Some(entry) = by_id.get_mut(&key) {
            merge_entry(entry, incoming);
        } else {
            by_id.insert(key, incoming);
        }
    }

    let mut entries = by_id.into_values().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.id.to_ascii_lowercase());
    entries
}

fn merge_entry(existing: &mut ExtensionIndexEntry, incoming: ExtensionIndexEntry) {
    if !incoming.name.trim().is_empty() {
        existing.name = incoming.name;
    }
    if incoming.description.is_some() {
        existing.description = incoming.description;
    }
    if incoming.license.is_some() {
        existing.license = incoming.license;
    }
    if incoming.source.is_some() {
        existing.source = incoming.source;
    }
    if incoming.install_source.is_some() {
        existing.install_source = incoming.install_source;
    }
    existing.tags = merge_tags(existing.tags.iter().cloned(), incoming.tags);
}

fn merge_tags(
    left: impl IntoIterator<Item = String>,
    right: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let mut tags = BTreeSet::new();
    for tag in left.into_iter().chain(right) {
        let trimmed = tag.trim();
        if !trimmed.is_empty() {
            tags.insert(trimmed.to_string());
        }
    }
    tags.into_iter().collect()
}

async fn fetch_npm_entries(client: &Client, limit: usize) -> Result<Vec<ExtensionIndexEntry>> {
    let query =
        url::form_urlencoded::byte_serialize(DEFAULT_NPM_QUERY.as_bytes()).collect::<String>();
    let size = limit.clamp(1, DEFAULT_REMOTE_LIMIT);
    let url = format!("https://registry.npmjs.org/-/v1/search?text={query}&size={size}");
    let response = client
        .get(&url)
        .timeout(REMOTE_REQUEST_TIMEOUT)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if status != 200 {
        return Err(Error::api(format!(
            "npm extension search failed with status {status}"
        )));
    }

    parse_npm_search_entries(&body)
}

async fn fetch_github_entries(client: &Client, limit: usize) -> Result<Vec<ExtensionIndexEntry>> {
    let query =
        url::form_urlencoded::byte_serialize(DEFAULT_GITHUB_QUERY.as_bytes()).collect::<String>();
    let per_page = limit.clamp(1, DEFAULT_REMOTE_LIMIT);
    let url = format!(
        "https://api.github.com/search/repositories?q={query}&sort=updated&order=desc&per_page={per_page}"
    );
    let response = client
        .get(&url)
        .timeout(REMOTE_REQUEST_TIMEOUT)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if status != 200 {
        return Err(Error::api(format!(
            "GitHub extension search failed with status {status}"
        )));
    }

    parse_github_search_entries(&body)
}

fn parse_npm_search_entries(body: &str) -> Result<Vec<ExtensionIndexEntry>> {
    #[derive(Debug, Deserialize)]
    struct NpmSearchResponse {
        #[serde(default)]
        objects: Vec<NpmSearchObject>,
    }

    #[derive(Debug, Deserialize)]
    struct NpmSearchObject {
        package: NpmPackage,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct NpmPackage {
        name: String,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        keywords: Vec<String>,
        #[serde(default)]
        license: Option<String>,
        #[serde(default)]
        links: NpmLinks,
    }

    #[derive(Debug, Default, Deserialize)]
    struct NpmLinks {
        #[serde(default)]
        npm: Option<String>,
    }

    let parsed: NpmSearchResponse = serde_json::from_str(body)
        .map_err(|err| Error::api(format!("npm search response parse error: {err}")))?;

    let mut entries = Vec::with_capacity(parsed.objects.len());
    for object in parsed.objects {
        let package = object.package;
        let version = package.version.as_deref().and_then(non_empty);
        let install_spec = version.as_ref().map_or_else(
            || package.name.clone(),
            |ver| format!("{}@{ver}", package.name),
        );
        let license = normalize_license(package.license.as_deref());
        let description = package.description.as_deref().and_then(non_empty);
        let tags = merge_tags(
            vec!["npm".to_string(), "extension".to_string()],
            package
                .keywords
                .into_iter()
                .map(|keyword| keyword.to_ascii_lowercase()),
        );

        entries.push(ExtensionIndexEntry {
            id: format!("npm/{}", package.name),
            name: package.name.clone(),
            description,
            tags,
            license,
            source: Some(ExtensionIndexSource::Npm {
                package: package.name.clone(),
                version,
                url: package.links.npm.clone(),
            }),
            install_source: Some(format!("npm:{install_spec}")),
        });
    }

    Ok(entries)
}

fn parse_github_search_entries(body: &str) -> Result<Vec<ExtensionIndexEntry>> {
    #[derive(Debug, Deserialize)]
    struct GitHubSearchResponse {
        #[serde(default)]
        items: Vec<GitHubRepo>,
    }

    #[derive(Debug, Deserialize)]
    struct GitHubRepo {
        full_name: String,
        name: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        topics: Vec<String>,
        #[serde(default)]
        license: Option<GitHubLicense>,
    }

    #[derive(Debug, Deserialize)]
    struct GitHubLicense {
        #[serde(default)]
        spdx_id: Option<String>,
    }

    let parsed: GitHubSearchResponse = serde_json::from_str(body)
        .map_err(|err| Error::api(format!("GitHub search response parse error: {err}")))?;

    let mut entries = Vec::with_capacity(parsed.items.len());
    for item in parsed.items {
        let spdx_id = item.license.and_then(|value| value.spdx_id);
        let license = spdx_id
            .as_deref()
            .and_then(non_empty)
            .filter(|value| !value.eq_ignore_ascii_case("NOASSERTION"));
        let tags = merge_tags(
            vec!["git".to_string(), "extension".to_string()],
            item.topics
                .into_iter()
                .map(|topic| topic.to_ascii_lowercase()),
        );

        entries.push(ExtensionIndexEntry {
            id: format!("git/{}", item.full_name),
            name: item.name,
            description: item.description.as_deref().and_then(non_empty),
            tags,
            license,
            source: Some(ExtensionIndexSource::Git {
                repo: item.full_name.clone(),
                path: None,
                r#ref: None,
            }),
            install_source: Some(format!("git:{}", item.full_name)),
        });
    }

    Ok(entries)
}

fn normalize_license(value: Option<&str>) -> Option<String> {
    value
        .and_then(non_empty)
        .filter(|license| !license.eq_ignore_ascii_case("unknown"))
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

// ============================================================================
// Seed Index (Bundled)
// ============================================================================

const SEED_ARTIFACT_PROVENANCE_JSON: &str =
    include_str!("../docs/extension-artifact-provenance.json");

#[derive(Debug, Deserialize)]
struct ArtifactProvenance {
    #[serde(rename = "$schema")]
    _schema: Option<String>,
    #[serde(default)]
    generated: Option<String>,
    #[serde(default)]
    items: Vec<ArtifactProvenanceItem>,
}

#[derive(Debug, Deserialize)]
struct ArtifactProvenanceItem {
    id: String,
    name: String,
    #[serde(default)]
    license: Option<String>,
    source: ArtifactProvenanceSource,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ArtifactProvenanceSource {
    Git {
        repo: String,
        #[serde(default)]
        path: Option<String>,
    },
    Npm {
        package: String,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        url: Option<String>,
    },
    Url {
        url: String,
    },
}

pub fn seed_index() -> Result<ExtensionIndex> {
    let provenance: ArtifactProvenance = serde_json::from_str(SEED_ARTIFACT_PROVENANCE_JSON)?;
    let generated_at = provenance.generated;

    let mut entries = Vec::with_capacity(provenance.items.len());
    for item in provenance.items {
        let license = item
            .license
            .clone()
            .filter(|value| !value.trim().is_empty() && !value.eq_ignore_ascii_case("unknown"));

        let (source, install_source, tags) = match &item.source {
            ArtifactProvenanceSource::Npm { version, url, .. } => {
                let spec = version.as_ref().map_or_else(
                    || item.name.clone(),
                    |v| format!("{}@{}", item.name, v.trim()),
                );
                (
                    Some(ExtensionIndexSource::Npm {
                        package: item.name.clone(),
                        version: version.clone(),
                        url: url.clone(),
                    }),
                    Some(format!("npm:{spec}")),
                    vec!["npm".to_string(), "extension".to_string()],
                )
            }
            ArtifactProvenanceSource::Git { repo, path } => {
                let install_source = path.as_ref().map_or_else(
                    || Some(format!("git:{repo}")),
                    |_| None, // deep path entries typically require a package filter
                );
                (
                    Some(ExtensionIndexSource::Git {
                        repo: repo.clone(),
                        path: path.clone(),
                        r#ref: None,
                    }),
                    install_source,
                    vec!["git".to_string(), "extension".to_string()],
                )
            }
            ArtifactProvenanceSource::Url { url } => (
                Some(ExtensionIndexSource::Url { url: url.clone() }),
                None,
                vec!["url".to_string(), "extension".to_string()],
            ),
        };

        entries.push(ExtensionIndexEntry {
            id: item.id,
            name: item.name,
            description: None,
            tags,
            license,
            source,
            install_source,
        });
    }

    entries.sort_by_key(|entry| entry.id.to_ascii_lowercase());

    Ok(ExtensionIndex {
        schema: EXTENSION_INDEX_SCHEMA.to_string(),
        version: EXTENSION_INDEX_VERSION,
        generated_at,
        last_refreshed_at: None,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        EXTENSION_INDEX_SCHEMA, EXTENSION_INDEX_VERSION, ExtensionIndex, ExtensionIndexEntry,
        ExtensionIndexSource, ExtensionIndexStore, merge_entries, merge_tags, non_empty,
        normalize_license, parse_github_search_entries, parse_npm_search_entries, score_entry,
        seed_index,
    };
    use chrono::{Duration as ChronoDuration, Utc};
    use std::time::Duration;

    #[test]
    fn seed_index_parses_and_has_entries() {
        let index = seed_index().expect("seed index");
        assert!(index.entries.len() > 10);
    }

    #[test]
    fn resolve_install_source_requires_unique_match() {
        let index = ExtensionIndex {
            schema: super::EXTENSION_INDEX_SCHEMA.to_string(),
            version: super::EXTENSION_INDEX_VERSION,
            generated_at: None,
            last_refreshed_at: None,
            entries: vec![
                ExtensionIndexEntry {
                    id: "npm/foo".to_string(),
                    name: "foo".to_string(),
                    description: None,
                    tags: Vec::new(),
                    license: None,
                    source: None,
                    install_source: Some("npm:foo@1.0.0".to_string()),
                },
                ExtensionIndexEntry {
                    id: "npm/foo-alt".to_string(),
                    name: "foo".to_string(),
                    description: None,
                    tags: Vec::new(),
                    license: None,
                    source: None,
                    install_source: Some("npm:foo@2.0.0".to_string()),
                },
            ],
        };

        assert_eq!(index.resolve_install_source("foo"), None);
        assert_eq!(
            index.resolve_install_source("npm/foo"),
            Some("npm:foo@1.0.0".to_string())
        );
    }

    #[test]
    fn store_resolve_install_source_falls_back_to_seed() {
        let store = ExtensionIndexStore::new(std::path::PathBuf::from("this-file-does-not-exist"));
        let resolved = store.resolve_install_source("checkpoint-pi");
        // The exact seed contents can change; the important part is "no error".
        assert!(resolved.is_ok());
    }

    #[test]
    fn parse_npm_search_entries_maps_install_sources() {
        let body = r#"{
          "objects": [
            {
              "package": {
                "name": "checkpoint-pi",
                "version": "1.2.3",
                "description": "checkpoint helper",
                "keywords": ["pi-extension", "checkpoint"],
                "license": "MIT",
                "links": { "npm": "https://www.npmjs.com/package/checkpoint-pi" }
              }
            }
          ]
        }"#;

        let entries = parse_npm_search_entries(body).expect("parse npm search");
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.id, "npm/checkpoint-pi");
        assert_eq!(
            entry.install_source.as_deref(),
            Some("npm:checkpoint-pi@1.2.3")
        );
        assert!(entry.tags.iter().any(|tag| tag == "checkpoint"));
    }

    #[test]
    fn parse_github_search_entries_maps_git_install_sources() {
        let body = r#"{
          "items": [
            {
              "full_name": "org/pi-cool-ext",
              "name": "pi-cool-ext",
              "description": "cool extension",
              "topics": ["pi-extension", "automation"],
              "license": { "spdx_id": "Apache-2.0" }
            }
          ]
        }"#;

        let entries = parse_github_search_entries(body).expect("parse github search");
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        assert_eq!(entry.id, "git/org/pi-cool-ext");
        assert_eq!(entry.install_source.as_deref(), Some("git:org/pi-cool-ext"));
        assert!(entry.tags.iter().any(|tag| tag == "automation"));
        assert!(matches!(
            entry.source,
            Some(ExtensionIndexSource::Git { .. })
        ));
    }

    #[test]
    fn merge_entries_preserves_existing_fields_when_incoming_missing() {
        let existing = vec![ExtensionIndexEntry {
            id: "npm/checkpoint-pi".to_string(),
            name: "checkpoint-pi".to_string(),
            description: Some("existing description".to_string()),
            tags: vec!["npm".to_string()],
            license: Some("MIT".to_string()),
            source: Some(ExtensionIndexSource::Npm {
                package: "checkpoint-pi".to_string(),
                version: Some("1.0.0".to_string()),
                url: None,
            }),
            install_source: Some("npm:checkpoint-pi@1.0.0".to_string()),
        }];
        let incoming = vec![ExtensionIndexEntry {
            id: "npm/checkpoint-pi".to_string(),
            name: "checkpoint-pi".to_string(),
            description: None,
            tags: vec!["extension".to_string()],
            license: None,
            source: None,
            install_source: None,
        }];

        let merged = merge_entries(existing, incoming, Vec::new());
        assert_eq!(merged.len(), 1);
        let entry = &merged[0];
        assert_eq!(entry.description.as_deref(), Some("existing description"));
        assert_eq!(
            entry.install_source.as_deref(),
            Some("npm:checkpoint-pi@1.0.0")
        );
        assert!(entry.tags.iter().any(|tag| tag == "npm"));
        assert!(entry.tags.iter().any(|tag| tag == "extension"));
    }

    // ── new_empty ──────────────────────────────────────────────────────

    #[test]
    fn new_empty_has_correct_schema_and_version() {
        let index = ExtensionIndex::new_empty();
        assert_eq!(index.schema, EXTENSION_INDEX_SCHEMA);
        assert_eq!(index.version, EXTENSION_INDEX_VERSION);
        assert!(index.generated_at.is_some());
        assert!(index.last_refreshed_at.is_none());
        assert!(index.entries.is_empty());
    }

    // ── validate ───────────────────────────────────────────────────────

    #[test]
    fn validate_accepts_correct_schema_and_version() {
        let index = ExtensionIndex::new_empty();
        assert!(index.validate().is_ok());
    }

    #[test]
    fn validate_rejects_wrong_schema() {
        let mut index = ExtensionIndex::new_empty();
        index.schema = "wrong.schema".to_string();
        let err = index.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("Unsupported extension index schema")
        );
    }

    #[test]
    fn validate_rejects_wrong_version() {
        let mut index = ExtensionIndex::new_empty();
        index.version = 999;
        let err = index.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("Unsupported extension index version")
        );
    }

    // ── is_stale ───────────────────────────────────────────────────────

    #[test]
    fn is_stale_true_when_no_timestamp() {
        let index = ExtensionIndex::new_empty();
        assert!(index.is_stale(Utc::now(), Duration::from_secs(3600)));
    }

    #[test]
    fn is_stale_true_when_invalid_timestamp() {
        let mut index = ExtensionIndex::new_empty();
        index.last_refreshed_at = Some("not-a-date".to_string());
        assert!(index.is_stale(Utc::now(), Duration::from_secs(3600)));
    }

    #[test]
    fn is_stale_false_when_fresh() {
        let mut index = ExtensionIndex::new_empty();
        index.last_refreshed_at = Some(Utc::now().to_rfc3339());
        assert!(!index.is_stale(Utc::now(), Duration::from_secs(3600)));
    }

    #[test]
    fn is_stale_true_when_expired() {
        let mut index = ExtensionIndex::new_empty();
        let old = Utc::now() - ChronoDuration::hours(2);
        index.last_refreshed_at = Some(old.to_rfc3339());
        assert!(index.is_stale(Utc::now(), Duration::from_secs(3600)));
    }

    // ── search ─────────────────────────────────────────────────────────

    fn test_entry(id: &str, name: &str, desc: Option<&str>, tags: &[&str]) -> ExtensionIndexEntry {
        ExtensionIndexEntry {
            id: id.to_string(),
            name: name.to_string(),
            description: desc.map(std::string::ToString::to_string),
            tags: tags.iter().map(std::string::ToString::to_string).collect(),
            license: None,
            source: None,
            install_source: Some(format!("npm:{name}")),
        }
    }

    fn test_index(entries: Vec<ExtensionIndexEntry>) -> ExtensionIndex {
        ExtensionIndex {
            schema: EXTENSION_INDEX_SCHEMA.to_string(),
            version: EXTENSION_INDEX_VERSION,
            generated_at: None,
            last_refreshed_at: None,
            entries,
        }
    }

    #[test]
    fn search_empty_query_returns_nothing() {
        let index = test_index(vec![test_entry("npm/foo", "foo", None, &[])]);
        assert!(index.search("", 10).is_empty());
        assert!(index.search("   ", 10).is_empty());
    }

    #[test]
    fn search_zero_limit_returns_nothing() {
        let index = test_index(vec![test_entry("npm/foo", "foo", None, &[])]);
        assert!(index.search("foo", 0).is_empty());
    }

    #[test]
    fn search_matches_by_name() {
        let index = test_index(vec![
            test_entry("npm/alpha", "alpha", None, &[]),
            test_entry("npm/beta", "beta", None, &[]),
        ]);
        let hits = index.search("alpha", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.name, "alpha");
    }

    #[test]
    fn search_matches_by_description() {
        let index = test_index(vec![test_entry(
            "npm/foo",
            "foo",
            Some("checkpoint helper"),
            &[],
        )]);
        let hits = index.search("checkpoint", 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_matches_by_tag() {
        let index = test_index(vec![test_entry("npm/foo", "foo", None, &["automation"])]);
        let hits = index.search("automation", 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn search_respects_limit() {
        let index = test_index(vec![
            test_entry("npm/foo-a", "foo-a", None, &[]),
            test_entry("npm/foo-b", "foo-b", None, &[]),
            test_entry("npm/foo-c", "foo-c", None, &[]),
        ]);
        let hits = index.search("foo", 2);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn search_ranks_name_higher_than_description() {
        let index = test_index(vec![
            test_entry("npm/other", "other", Some("checkpoint tool"), &[]),
            test_entry("npm/checkpoint", "checkpoint", None, &[]),
        ]);
        let hits = index.search("checkpoint", 10);
        assert_eq!(hits.len(), 2);
        // Name match (300) beats description match (60)
        assert_eq!(hits[0].entry.name, "checkpoint");
    }

    // ── score_entry ────────────────────────────────────────────────────

    #[test]
    fn score_entry_name_match_highest() {
        let entry = test_entry("npm/foo", "foo", Some("bar"), &["baz"]);
        assert_eq!(score_entry(&entry, &["foo".to_string()]), 300 + 120);
        // name(300) + id contains "foo" too (120)
    }

    #[test]
    fn score_entry_no_match_returns_zero() {
        let entry = test_entry("npm/foo", "foo", None, &[]);
        assert_eq!(score_entry(&entry, &["zzz".to_string()]), 0);
    }

    #[test]
    fn score_entry_tag_match() {
        let entry = test_entry("npm/bar", "bar", None, &["automation"]);
        let score = score_entry(&entry, &["automation".to_string()]);
        assert_eq!(score, 180);
    }

    #[test]
    fn score_entry_multiple_tokens_accumulate() {
        let entry = test_entry("npm/foo", "foo", Some("great tool"), &["utility"]);
        let score = score_entry(&entry, &["foo".to_string(), "great".to_string()]);
        // "foo": name(300) + id(120) = 420
        // "great": description(60) = 60
        assert_eq!(score, 480);
    }

    // ── merge_tags ─────────────────────────────────────────────────────

    #[test]
    fn merge_tags_deduplicates() {
        let result = merge_tags(
            vec!["a".to_string(), "b".to_string()],
            vec!["b".to_string(), "c".to_string()],
        );
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_tags_trims_and_skips_empty() {
        let result = merge_tags(
            vec!["  a  ".to_string(), String::new()],
            vec!["  ".to_string(), "b".to_string()],
        );
        assert_eq!(result, vec!["a", "b"]);
    }

    // ── normalize_license ──────────────────────────────────────────────

    #[test]
    fn normalize_license_returns_none_for_none() {
        assert_eq!(normalize_license(None), None);
    }

    #[test]
    fn normalize_license_returns_none_for_empty() {
        assert_eq!(normalize_license(Some("")), None);
        assert_eq!(normalize_license(Some("  ")), None);
    }

    #[test]
    fn normalize_license_returns_none_for_unknown() {
        assert_eq!(normalize_license(Some("unknown")), None);
        assert_eq!(normalize_license(Some("UNKNOWN")), None);
    }

    #[test]
    fn normalize_license_returns_value_for_valid() {
        assert_eq!(normalize_license(Some("MIT")), Some("MIT".to_string()));
        assert_eq!(
            normalize_license(Some("Apache-2.0")),
            Some("Apache-2.0".to_string())
        );
    }

    // ── non_empty ──────────────────────────────────────────────────────

    #[test]
    fn non_empty_returns_none_for_empty_and_whitespace() {
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("   "), None);
    }

    #[test]
    fn non_empty_trims_and_returns() {
        assert_eq!(non_empty("  hello  "), Some("hello".to_string()));
    }

    // ── resolve_install_source edge cases ──────────────────────────────

    #[test]
    fn resolve_install_source_empty_query_returns_none() {
        let index = test_index(vec![test_entry("npm/foo", "foo", None, &[])]);
        assert_eq!(index.resolve_install_source(""), None);
        assert_eq!(index.resolve_install_source("   "), None);
    }

    #[test]
    fn resolve_install_source_case_insensitive() {
        let index = test_index(vec![ExtensionIndexEntry {
            id: "npm/Foo".to_string(),
            name: "Foo".to_string(),
            description: None,
            tags: Vec::new(),
            license: None,
            source: None,
            install_source: Some("npm:Foo".to_string()),
        }]);
        assert_eq!(
            index.resolve_install_source("foo"),
            Some("npm:Foo".to_string())
        );
    }

    #[test]
    fn resolve_install_source_npm_package_name() {
        let index = test_index(vec![ExtensionIndexEntry {
            id: "npm/my-ext".to_string(),
            name: "my-ext".to_string(),
            description: None,
            tags: Vec::new(),
            license: None,
            source: Some(ExtensionIndexSource::Npm {
                package: "my-ext".to_string(),
                version: Some("1.0.0".to_string()),
                url: None,
            }),
            install_source: Some("npm:my-ext@1.0.0".to_string()),
        }]);
        assert_eq!(
            index.resolve_install_source("my-ext"),
            Some("npm:my-ext@1.0.0".to_string())
        );
    }

    #[test]
    fn resolve_install_source_no_install_source_returns_none() {
        let index = test_index(vec![ExtensionIndexEntry {
            id: "npm/foo".to_string(),
            name: "foo".to_string(),
            description: None,
            tags: Vec::new(),
            license: None,
            source: None,
            install_source: None,
        }]);
        assert_eq!(index.resolve_install_source("foo"), None);
    }

    // ── ExtensionIndexStore save/load roundtrip ────────────────────────

    #[test]
    fn store_save_load_roundtrip() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("index.json");
        let store = ExtensionIndexStore::new(path);

        let mut index = ExtensionIndex::new_empty();
        index
            .entries
            .push(test_entry("npm/rt", "rt", Some("roundtrip"), &["test"]));
        store.save(&index).expect("save");

        let loaded = store.load().expect("load").expect("some");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].name, "rt");
        assert_eq!(loaded.entries[0].description.as_deref(), Some("roundtrip"));
    }

    #[test]
    fn store_load_nonexistent_returns_none() {
        let store = ExtensionIndexStore::new(std::path::PathBuf::from("/nonexistent/path.json"));
        assert!(store.load().expect("load").is_none());
    }

    #[test]
    fn store_load_or_seed_falls_back_on_missing() {
        let store = ExtensionIndexStore::new(std::path::PathBuf::from("/nonexistent/path.json"));
        let index = store.load_or_seed().expect("load_or_seed");
        assert!(!index.entries.is_empty());
    }

    // ── parse edge cases ───────────────────────────────────────────────

    #[test]
    fn parse_npm_no_version_omits_at_in_install_source() {
        let body = r#"{
          "objects": [{
            "package": {
              "name": "bare-ext",
              "keywords": [],
              "links": {}
            }
          }]
        }"#;
        let entries = parse_npm_search_entries(body).expect("parse");
        assert_eq!(entries[0].install_source.as_deref(), Some("npm:bare-ext"));
    }

    #[test]
    fn parse_npm_empty_objects_returns_empty() {
        let body = r#"{ "objects": [] }"#;
        let entries = parse_npm_search_entries(body).expect("parse");
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_github_noassertion_license_filtered_out() {
        let body = r#"{
          "items": [{
            "full_name": "org/ext",
            "name": "ext",
            "topics": [],
            "license": { "spdx_id": "NOASSERTION" }
          }]
        }"#;
        let entries = parse_github_search_entries(body).expect("parse");
        assert!(entries[0].license.is_none());
    }

    #[test]
    fn parse_github_null_license_ok() {
        let body = r#"{
          "items": [{
            "full_name": "org/ext2",
            "name": "ext2",
            "topics": []
          }]
        }"#;
        let entries = parse_github_search_entries(body).expect("parse");
        assert!(entries[0].license.is_none());
    }

    // ── merge_entries adds new entries ──────────────────────────────────

    #[test]
    fn merge_entries_adds_new_and_deduplicates() {
        let existing = vec![test_entry("npm/a", "a", None, &[])];
        let npm = vec![test_entry("npm/b", "b", None, &[])];
        let git = vec![test_entry("git/c", "c", None, &[])];
        let merged = merge_entries(existing, npm, git);
        assert_eq!(merged.len(), 3);
        // Sorted by id
        assert_eq!(merged[0].id, "git/c");
        assert_eq!(merged[1].id, "npm/a");
        assert_eq!(merged[2].id, "npm/b");
    }

    #[test]
    fn merge_entries_case_insensitive_dedup() {
        let existing = vec![test_entry("npm/Foo", "Foo", Some("old"), &[])];
        let npm = vec![test_entry("npm/foo", "foo", Some("new"), &[])];
        let merged = merge_entries(existing, npm, Vec::new());
        assert_eq!(merged.len(), 1);
        // Incoming overwrites description
        assert_eq!(merged[0].description.as_deref(), Some("new"));
    }

    // ── serde roundtrip ────────────────────────────────────────────────

    #[test]
    fn extension_index_serde_roundtrip() {
        let index = test_index(vec![test_entry("npm/x", "x", Some("desc"), &["tag1"])]);
        let json = serde_json::to_string(&index).expect("serialize");
        let deserialized: ExtensionIndex = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.entries.len(), 1);
        assert_eq!(deserialized.entries[0].name, "x");
    }

    #[test]
    fn extension_index_entry_source_variants_serialize() {
        let npm = ExtensionIndexSource::Npm {
            package: "p".to_string(),
            version: Some("1.0".to_string()),
            url: None,
        };
        let git = ExtensionIndexSource::Git {
            repo: "org/r".to_string(),
            path: None,
            r#ref: None,
        };
        let url = ExtensionIndexSource::Url {
            url: "https://example.com".to_string(),
        };

        for source in [npm, git, url] {
            let json = serde_json::to_string(&source).expect("serialize");
            let _: ExtensionIndexSource = serde_json::from_str(&json).expect("deserialize");
        }
    }

    // ── ExtensionIndexRefreshStats default ─────────────────────────────

    #[test]
    fn refresh_stats_default_all_zero() {
        let stats = super::ExtensionIndexRefreshStats::default();
        assert_eq!(stats.npm_entries, 0);
        assert_eq!(stats.github_entries, 0);
        assert_eq!(stats.merged_entries, 0);
        assert!(!stats.refreshed);
    }

    // ── store path accessor ────────────────────────────────────────────

    #[test]
    fn store_path_returns_configured_path() {
        let store = ExtensionIndexStore::new(std::path::PathBuf::from("/custom/path.json"));
        assert_eq!(store.path().to_str().unwrap(), "/custom/path.json");
    }
}
