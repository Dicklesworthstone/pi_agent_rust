//! Extension discovery index (offline-first).
//!
//! This module provides a local, searchable index of available extensions. The index is:
//! - **Offline-first**: Pi ships a bundled seed index embedded at compile time.
//! - **Fail-open**: cache load/refresh failures should never break discovery.
//! - **Host-agnostic**: the index is primarily a data structure; CLI commands live elsewhere.

use crate::config::Config;
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::NamedTempFile;

pub const EXTENSION_INDEX_SCHEMA: &str = "pi.ext.index.v1";
pub const EXTENSION_INDEX_VERSION: u32 = 1;
pub const DEFAULT_INDEX_MAX_AGE: Duration = Duration::from_secs(60 * 60 * 24);

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
    use super::{ExtensionIndex, ExtensionIndexEntry, ExtensionIndexStore, seed_index};

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
}
