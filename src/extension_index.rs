//! Extension discovery index (offline-first).
//!
//! This module provides a local, searchable index of available extensions. The index is:
//! - **Offline-first**: Pi ships a bundled seed index embedded at compile time.
//! - **Fail-open**: cache load/refresh failures should never break discovery.
//! - **Host-agnostic**: the index is primarily a data structure; CLI commands live elsewhere.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::extension_inclusion::{
    ExtensionCategory, InclusionEntry, VersionPin, classify_registrations,
};
use crate::http::client::Client;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tempfile::NamedTempFile;

pub const EXTENSION_INDEX_SCHEMA: &str = "pi.ext.index.v1";
pub const EXTENSION_INDEX_VERSION: u32 = 1;
pub const EXTENSION_SAFETY_PROVENANCE_SCHEMA: &str = "pi.ext.safety_provenance.v1";
pub const DEFAULT_INDEX_MAX_AGE: Duration = Duration::new(86_400, 0);
const DEFAULT_NPM_QUERY: &str = "keywords:pi-extension";
const DEFAULT_GITHUB_QUERY: &str = "topic:pi-extension";
const DEFAULT_REMOTE_LIMIT: usize = 100;
const REMOTE_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CAPABILITY_SIGNALS: usize = 12;
const REDACTED_CAPABILITY_SIGNAL: &str = "redacted-capability";

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
            .map_or(true, |age| age >= max_age)
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
            if let Some(ExtensionIndexSource::Npm { package, .. }) = &entry.source
                && package.to_ascii_lowercase() == q_lc
            {
                sources.insert(install.clone());
                continue;
            }

            if let Some(rest) = entry.id.strip_prefix("npm/")
                && rest.eq_ignore_ascii_case(q)
            {
                sources.insert(install.clone());
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

/// Redaction-safe, offline-only safety metadata shown before extension use.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionSafetyProvenance {
    pub schema: &'static str,
    pub source_type: String,
    pub license_status: String,
    pub registration_categories: Vec<String>,
    pub requested_capabilities: Vec<String>,
    pub risk_profile: String,
    pub freshness: String,
    pub source_confidence: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<String>,
}

impl ExtensionSafetyProvenance {
    #[must_use]
    pub fn from_index_entry(
        entry: &ExtensionIndexEntry,
        index: &ExtensionIndex,
        max_age: Duration,
    ) -> Self {
        Self::from_index_entry_at(entry, index, Utc::now(), max_age)
    }

    #[must_use]
    pub fn from_index_entry_at(
        entry: &ExtensionIndexEntry,
        index: &ExtensionIndex,
        now: DateTime<Utc>,
        max_age: Duration,
    ) -> Self {
        let mut degraded_reasons = validate_index_entry_shape(entry);
        let source_type = source_type_from_index_entry(entry);
        if source_type == "unknown" {
            degraded_reasons.push("missing_source".to_string());
        }

        let (freshness, mut freshness_reasons) = index_freshness_signal(index, now, max_age);
        degraded_reasons.append(&mut freshness_reasons);

        let license_status = license_status(entry.license.as_deref());
        let registration_categories = registration_categories_from_tags(&entry.tags);
        let requested_capabilities = Vec::new();
        let risk_profile = risk_profile(
            &registration_categories,
            &requested_capabilities,
            None,
            &mut degraded_reasons,
        );
        let source_confidence =
            source_confidence(&source_type, &license_status, &freshness, &degraded_reasons);

        Self {
            schema: EXTENSION_SAFETY_PROVENANCE_SCHEMA,
            source_type,
            license_status,
            registration_categories,
            requested_capabilities,
            risk_profile,
            freshness,
            source_confidence,
            degraded_reasons: dedupe_sorted(degraded_reasons),
        }
    }

    #[must_use]
    pub fn from_install_source(source: &str) -> Self {
        let source = source.trim();
        let mut degraded_reasons = Vec::new();
        if source.is_empty() {
            degraded_reasons.push("blank_source".to_string());
        }

        let source_type = source_type_from_install_source(source);
        if source_type == "unknown" {
            degraded_reasons.push("unknown_source_type".to_string());
        }

        let license_status = "unknown".to_string();
        let registration_categories = vec!["unknown".to_string()];
        let requested_capabilities = Vec::new();
        let freshness = "unknown".to_string();
        let risk_profile = "unknown".to_string();
        let source_confidence =
            source_confidence(&source_type, &license_status, &freshness, &degraded_reasons);

        Self {
            schema: EXTENSION_SAFETY_PROVENANCE_SCHEMA,
            source_type,
            license_status,
            registration_categories,
            requested_capabilities,
            risk_profile,
            freshness,
            source_confidence,
            degraded_reasons: dedupe_sorted(degraded_reasons),
        }
    }

    #[must_use]
    pub fn from_inclusion_entry(entry: &InclusionEntry) -> Self {
        let mut degraded_reasons = Vec::new();
        if entry.id.trim().is_empty() {
            degraded_reasons.push("blank_id".to_string());
        }

        let source_type = source_type_from_inclusion_entry(entry);
        if source_type == "unknown" {
            degraded_reasons.push("missing_source".to_string());
        }

        let license_status = license_status(entry.license.as_deref());
        let registration_categories = registration_categories_from_inclusion_entry(entry);
        let requested_capabilities = sanitize_capabilities(
            entry.capabilities.as_deref().unwrap_or_default(),
            &mut degraded_reasons,
        );
        let risk_profile = risk_profile(
            &registration_categories,
            &requested_capabilities,
            entry.risk_level.as_deref(),
            &mut degraded_reasons,
        );
        let freshness = "offline".to_string();
        let source_confidence =
            source_confidence(&source_type, &license_status, &freshness, &degraded_reasons);

        Self {
            schema: EXTENSION_SAFETY_PROVENANCE_SCHEMA,
            source_type,
            license_status,
            registration_categories,
            requested_capabilities,
            risk_profile,
            freshness,
            source_confidence,
            degraded_reasons: dedupe_sorted(degraded_reasons),
        }
    }

    #[must_use]
    pub fn compact_label(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            self.source_type,
            self.license_status,
            self.risk_profile,
            self.freshness,
            self.source_confidence
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExtensionIndexRefreshStats {
    pub npm_entries: usize,
    pub github_entries: usize,
    pub merged_entries: usize,
    pub refreshed: bool,
}

fn validate_index_entry_shape(entry: &ExtensionIndexEntry) -> Vec<String> {
    let mut reasons = Vec::new();
    if entry.id.trim().is_empty() {
        reasons.push("blank_id".to_string());
    }
    if entry.name.trim().is_empty() {
        reasons.push("blank_name".to_string());
    }
    reasons
}

fn source_type_from_index_entry(entry: &ExtensionIndexEntry) -> String {
    let id = entry.id.to_ascii_lowercase();
    if id.starts_with("community/") || has_tag(&entry.tags, "community") {
        return "community".to_string();
    }

    match &entry.source {
        Some(ExtensionIndexSource::Npm { .. }) => "npm".to_string(),
        Some(ExtensionIndexSource::Git { repo, path, .. }) => {
            let repo_lc = repo.to_ascii_lowercase();
            let path_lc = path.as_deref().unwrap_or_default().to_ascii_lowercase();
            if repo_lc.contains("github.com/badlogic/pi-mono")
                && path_lc.contains("packages/coding-agent/examples/extensions/")
            {
                "official".to_string()
            } else {
                "git".to_string()
            }
        }
        Some(ExtensionIndexSource::Url { .. }) => "url".to_string(),
        None => "unknown".to_string(),
    }
}

fn source_type_from_install_source(source: &str) -> String {
    let source_lc = source.to_ascii_lowercase();
    if source_lc.starts_with("npm:") {
        "npm".to_string()
    } else if source_lc.starts_with("git:") {
        "git".to_string()
    } else if source_lc.starts_with("http://") || source_lc.starts_with("https://") {
        "url".to_string()
    } else if source.is_empty() {
        "unknown".to_string()
    } else {
        "local".to_string()
    }
}

fn source_type_from_inclusion_entry(entry: &InclusionEntry) -> String {
    if entry.id.to_ascii_lowercase().starts_with("community/") {
        return "community".to_string();
    }

    if let Some(source_tier) = entry.source_tier.as_deref() {
        match source_tier {
            "official-pi-mono" => return "official".to_string(),
            "community" | "agents-mikeastock" => return "community".to_string(),
            "npm-registry" | "npm-registry-pi" => return "npm".to_string(),
            _ => {}
        }
    }

    match &entry.version_pin {
        Some(VersionPin::Npm { .. }) => "npm".to_string(),
        Some(VersionPin::Git { repo, path, .. }) => {
            let repo_lc = repo.to_ascii_lowercase();
            let path_lc = path.as_deref().unwrap_or_default().to_ascii_lowercase();
            if repo_lc.contains("github.com/badlogic/pi-mono")
                && path_lc.contains("packages/coding-agent/examples/extensions/")
            {
                "official".to_string()
            } else {
                "git".to_string()
            }
        }
        Some(VersionPin::Url { .. }) => "url".to_string(),
        Some(VersionPin::Checksum) | None => "unknown".to_string(),
    }
}

fn has_tag(tags: &[String], needle: &str) -> bool {
    tags.iter().any(|tag| tag.eq_ignore_ascii_case(needle))
}

fn license_status(license: Option<&str>) -> String {
    match license.map(str::trim) {
        Some(value) if !value.is_empty() && !value.eq_ignore_ascii_case("unknown") => {
            "present".to_string()
        }
        Some(_) | None => "missing".to_string(),
    }
}

fn index_freshness_signal(
    index: &ExtensionIndex,
    now: DateTime<Utc>,
    max_age: Duration,
) -> (String, Vec<String>) {
    if let Some(ts) = index.last_refreshed_at.as_deref() {
        if DateTime::parse_from_rfc3339(ts).is_err() {
            return (
                "unknown".to_string(),
                vec!["malformed_last_refreshed_at".to_string()],
            );
        }
        if index.is_stale(now, max_age) {
            ("stale".to_string(), Vec::new())
        } else {
            ("fresh".to_string(), Vec::new())
        }
    } else if index
        .generated_at
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        ("seed".to_string(), Vec::new())
    } else {
        (
            "unknown".to_string(),
            vec!["missing_index_timestamp".to_string()],
        )
    }
}

fn registration_categories_from_tags(tags: &[String]) -> Vec<String> {
    let mut categories = BTreeSet::new();
    for tag in tags {
        let tag = tag.to_ascii_lowercase();
        if tag.contains("provider") {
            categories.insert("provider".to_string());
        }
        if tag.contains("event") || tag.contains("hook") {
            categories.insert("event_hook".to_string());
        }
        if tag.contains("tool") {
            categories.insert("tool".to_string());
        }
        if tag.contains("command") || tag.contains("slash") {
            categories.insert("command".to_string());
        }
        if tag.contains("ui") || tag.contains("renderer") {
            categories.insert("ui_component".to_string());
        }
        if tag.contains("flag") || tag.contains("shortcut") || tag.contains("config") {
            categories.insert("configuration".to_string());
        }
    }

    if categories.is_empty() {
        vec!["general".to_string()]
    } else {
        categories.into_iter().collect()
    }
}

fn registration_categories_from_inclusion_entry(entry: &InclusionEntry) -> Vec<String> {
    if entry.registrations.is_empty() {
        return vec![category_label(&entry.category).to_string()];
    }

    let mut categories = BTreeSet::new();
    for registration in &entry.registrations {
        match registration.as_str() {
            "registerTool" => {
                categories.insert("tool".to_string());
            }
            "registerCommand" | "registerSlashCommand" => {
                categories.insert("command".to_string());
            }
            "registerProvider" => {
                categories.insert("provider".to_string());
            }
            "registerEvent" | "registerEventHook" => {
                categories.insert("event_hook".to_string());
            }
            "registerMessageRenderer" => {
                categories.insert("ui_component".to_string());
            }
            "registerFlag" | "registerShortcut" => {
                categories.insert("configuration".to_string());
            }
            _ => {}
        }
    }

    let classified = classify_registrations(&entry.registrations);
    if classified == ExtensionCategory::Multi {
        categories.insert("multi".to_string());
    }
    if categories.is_empty() {
        categories.insert(category_label(&classified).to_string());
    }
    categories.into_iter().collect()
}

const fn category_label(category: &ExtensionCategory) -> &'static str {
    match category {
        ExtensionCategory::Tool => "tool",
        ExtensionCategory::Command => "command",
        ExtensionCategory::Provider => "provider",
        ExtensionCategory::EventHook => "event_hook",
        ExtensionCategory::UiComponent => "ui_component",
        ExtensionCategory::Configuration => "configuration",
        ExtensionCategory::Multi => "multi",
        ExtensionCategory::General => "general",
    }
}

fn sanitize_capabilities(raw: &[String], degraded_reasons: &mut Vec<String>) -> Vec<String> {
    let mut values = BTreeSet::new();
    let mut redacted = false;
    for item in raw {
        match sanitize_capability(item) {
            CapabilitySignal::Valid(value) => {
                values.insert(value);
            }
            CapabilitySignal::Redacted => {
                redacted = true;
                degraded_reasons.push("redacted_capability_signal".to_string());
            }
            CapabilitySignal::Empty => {}
        }
    }

    if redacted {
        values.insert(REDACTED_CAPABILITY_SIGNAL.to_string());
    }

    let mut out = values
        .into_iter()
        .take(MAX_CAPABILITY_SIGNALS)
        .collect::<Vec<_>>();
    if raw.len() > MAX_CAPABILITY_SIGNALS {
        out.push("truncated".to_string());
    }
    out
}

enum CapabilitySignal {
    Valid(String),
    Redacted,
    Empty,
}

fn sanitize_capability(value: &str) -> CapabilitySignal {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return CapabilitySignal::Empty;
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("sk-")
        || trimmed.len() > 64
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '/'))
    {
        return CapabilitySignal::Redacted;
    }

    CapabilitySignal::Valid(lower)
}

fn risk_profile(
    registration_categories: &[String],
    requested_capabilities: &[String],
    declared_risk: Option<&str>,
    degraded_reasons: &mut Vec<String>,
) -> String {
    let mut risk = if registration_categories.iter().any(|category| {
        matches!(
            category.as_str(),
            "provider" | "event_hook" | "multi" | "unknown"
        )
    }) {
        "elevated"
    } else {
        "low"
    };

    if requested_capabilities
        .iter()
        .any(|capability| high_risk_capability(capability))
    {
        risk = "high";
    } else if requested_capabilities
        .iter()
        .any(|capability| elevated_capability(capability))
        && risk != "high"
    {
        risk = "elevated";
    }

    if let Some(declared) = declared_risk
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        match declared.to_ascii_lowercase().as_str() {
            "critical" | "high" => risk = "high",
            "medium" | "elevated" if risk != "high" => risk = "elevated",
            "low" if risk == "low" => {}
            "unknown" => risk = "unknown",
            _ => {
                degraded_reasons.push("unsupported_risk_level".to_string());
                risk = "unknown";
            }
        }
    }

    if requested_capabilities
        .iter()
        .any(|capability| capability == REDACTED_CAPABILITY_SIGNAL)
        && risk != "high"
    {
        risk = "unknown";
    }

    risk.to_string()
}

fn high_risk_capability(capability: &str) -> bool {
    matches!(
        capability,
        "exec" | "env" | "shell" | "bash" | "process" | "child_process" | "spawn"
    )
}

fn elevated_capability(capability: &str) -> bool {
    matches!(
        capability,
        "write" | "http" | "https" | "network" | "net" | "fetch" | "filesystem:write"
    )
}

fn source_confidence(
    source_type: &str,
    license_status: &str,
    freshness: &str,
    degraded_reasons: &[String],
) -> String {
    if !degraded_reasons.is_empty() || source_type == "unknown" {
        return "degraded".to_string();
    }
    if freshness == "stale" || license_status == "missing" || license_status == "unknown" {
        return "low".to_string();
    }
    if source_type == "official" {
        "high".to_string()
    } else {
        "medium".to_string()
    }
}

fn dedupe_sorted(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
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
            persist_tempfile_for_cache(tmp, &self.path).map_err(|err| {
                Error::config(format!(
                    "Failed to persist extension index to {}: {err}",
                    self.path.display()
                ))
            })
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

fn persist_tempfile_for_cache(tmp: NamedTempFile, path: &Path) -> std::io::Result<()> {
    let _persist_guard = extension_index_persist_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match tmp.persist(path) {
        Ok(_) => Ok(()),
        Err(err) => persist_tempfile_for_cache_after_conflict(err, path),
    }
}

fn extension_index_persist_lock() -> &'static Mutex<()> {
    static PERSIST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    PERSIST_LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(windows)]
fn persist_tempfile_for_cache_after_conflict(
    err: tempfile::PersistError,
    path: &Path,
) -> std::io::Result<()> {
    if err.error.kind() != std::io::ErrorKind::AlreadyExists {
        return Err(err.error);
    }

    // Extension index writes are documented as fail-open cache refreshes.
    // On Windows, `persist()` may reject replacing an existing file, so retry
    // with a best-effort remove+persist fallback instead of surfacing a
    // permanent refresh failure.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(remove_err) if remove_err.kind() == std::io::ErrorKind::NotFound => {}
        Err(remove_err) => return Err(remove_err),
    }

    err.file
        .persist(path)
        .map(|_| ())
        .map_err(|persist_err| persist_err.error)
}

#[cfg(not(windows))]
fn persist_tempfile_for_cache_after_conflict(
    err: tempfile::PersistError,
    _path: &Path,
) -> std::io::Result<()> {
    Err(err.error)
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
    let provenance: ArtifactProvenance =
        serde_json::from_str(&crate::embedded_assets::extension_artifact_provenance_json())?;
    let generated_at = provenance.generated;

    let mut entries = Vec::with_capacity(provenance.items.len());
    for item in provenance.items {
        let license = item
            .license
            .clone()
            .filter(|value| !value.trim().is_empty() && !value.eq_ignore_ascii_case("unknown"));

        let (source, install_source, tags) = match &item.source {
            ArtifactProvenanceSource::Npm {
                package,
                version,
                url,
            } => {
                let spec = version
                    .as_ref()
                    .map_or_else(|| package.clone(), |v| format!("{}@{}", package, v.trim()));
                (
                    Some(ExtensionIndexSource::Npm {
                        package: package.clone(),
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
        EXTENSION_INDEX_SCHEMA, EXTENSION_INDEX_VERSION, EXTENSION_SAFETY_PROVENANCE_SCHEMA,
        ExtensionIndex, ExtensionIndexEntry, ExtensionIndexSource, ExtensionIndexStore,
        ExtensionSafetyProvenance, merge_entries, merge_tags, non_empty, normalize_license,
        parse_github_search_entries, parse_npm_search_entries, score_entry, seed_index,
    };
    use crate::extension_inclusion::{ExtensionCategory, InclusionEntry, VersionPin};
    use chrono::{Duration as ChronoDuration, Utc};
    use std::time::Duration;

    #[test]
    fn seed_index_parses_and_has_entries() {
        let index = seed_index().expect("seed index");
        assert!(index.entries.len() > 10);
    }

    #[test]
    fn seed_index_uses_npm_package_for_install_source() {
        let index = seed_index().expect("seed index");
        let entry = index
            .entries
            .iter()
            .find(|entry| {
                matches!(
                    &entry.source,
                    Some(ExtensionIndexSource::Npm { package, .. }) if package != &entry.name
                )
            })
            .expect("seed should include an npm package whose display name differs from package");

        let Some(ExtensionIndexSource::Npm {
            package, version, ..
        }) = &entry.source
        else {
            unreachable!("entry source should be npm");
        };

        let expected_install = version.as_ref().map_or_else(
            || format!("npm:{package}"),
            |version| format!("npm:{package}@{version}"),
        );
        assert_eq!(
            entry.install_source.as_deref(),
            Some(expected_install.as_str())
        );
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

    #[test]
    fn is_stale_true_at_exact_max_age_boundary() {
        let now = Utc::now();
        let mut index = ExtensionIndex::new_empty();
        index.last_refreshed_at = Some((now - ChronoDuration::hours(1)).to_rfc3339());
        assert!(index.is_stale(now, Duration::from_secs(3600)));
    }

    fn safety_test_index() -> ExtensionIndex {
        ExtensionIndex {
            schema: EXTENSION_INDEX_SCHEMA.to_string(),
            version: EXTENSION_INDEX_VERSION,
            generated_at: Some("2026-02-05".to_string()),
            last_refreshed_at: None,
            entries: Vec::new(),
        }
    }

    fn safety_entry(
        id: &str,
        source: Option<ExtensionIndexSource>,
        license: Option<&str>,
    ) -> ExtensionIndexEntry {
        ExtensionIndexEntry {
            id: id.to_string(),
            name: id.rsplit('/').next().unwrap_or(id).to_string(),
            description: None,
            tags: Vec::new(),
            license: license.map(str::to_string),
            source,
            install_source: Some(format!("npm:{id}")),
        }
    }

    #[test]
    fn safety_provenance_classifies_index_source_types() {
        let index = safety_test_index();
        let cases = [
            (
                safety_entry(
                    "bookmark",
                    Some(ExtensionIndexSource::Git {
                        repo: "https://github.com/badlogic/pi-mono".to_string(),
                        path: Some(
                            "packages/coding-agent/examples/extensions/bookmark.ts".to_string(),
                        ),
                        r#ref: None,
                    }),
                    Some("MIT"),
                ),
                "official",
            ),
            (
                safety_entry(
                    "community/notify",
                    Some(ExtensionIndexSource::Git {
                        repo: "https://github.com/badlogic/pi-mono".to_string(),
                        path: Some("packages/coding-agent/community/notify.ts".to_string()),
                        r#ref: None,
                    }),
                    Some("MIT"),
                ),
                "community",
            ),
            (
                safety_entry(
                    "npm/pi-tool",
                    Some(ExtensionIndexSource::Npm {
                        package: "pi-tool".to_string(),
                        version: Some("1.2.3".to_string()),
                        url: None,
                    }),
                    Some("MIT"),
                ),
                "npm",
            ),
            (
                safety_entry(
                    "git/org/ext",
                    Some(ExtensionIndexSource::Git {
                        repo: "https://github.com/org/ext".to_string(),
                        path: None,
                        r#ref: None,
                    }),
                    Some("Apache-2.0"),
                ),
                "git",
            ),
            (
                safety_entry(
                    "url/ext",
                    Some(ExtensionIndexSource::Url {
                        url: "https://example.com/ext.js".to_string(),
                    }),
                    Some("MIT"),
                ),
                "url",
            ),
            (safety_entry("unknown/ext", None, Some("MIT")), "unknown"),
        ];

        for (entry, expected_source_type) in cases {
            let safety = ExtensionSafetyProvenance::from_index_entry_at(
                &entry,
                &index,
                Utc::now(),
                Duration::from_secs(3600),
            );
            assert_eq!(safety.source_type, expected_source_type);
        }
    }

    #[test]
    fn safety_provenance_degrades_malformed_index_entries() {
        let mut index = safety_test_index();
        index.last_refreshed_at = Some("not-a-timestamp".to_string());
        let entry = ExtensionIndexEntry {
            id: String::new(),
            name: String::new(),
            description: None,
            tags: Vec::new(),
            license: None,
            source: None,
            install_source: None,
        };

        let safety = ExtensionSafetyProvenance::from_index_entry_at(
            &entry,
            &index,
            Utc::now(),
            Duration::from_secs(3600),
        );

        assert_eq!(safety.source_type, "unknown");
        assert_eq!(safety.license_status, "missing");
        assert_eq!(safety.source_confidence, "degraded");
        assert!(safety.degraded_reasons.contains(&"blank_id".to_string()));
        assert!(safety.degraded_reasons.contains(&"blank_name".to_string()));
        assert!(
            safety
                .degraded_reasons
                .contains(&"malformed_last_refreshed_at".to_string())
        );
    }

    #[test]
    fn safety_provenance_marks_missing_license_low_confidence() {
        let index = safety_test_index();
        let entry = safety_entry(
            "npm/pi-tool",
            Some(ExtensionIndexSource::Npm {
                package: "pi-tool".to_string(),
                version: None,
                url: None,
            }),
            None,
        );

        let safety = ExtensionSafetyProvenance::from_index_entry_at(
            &entry,
            &index,
            Utc::now(),
            Duration::from_secs(3600),
        );

        assert_eq!(safety.license_status, "missing");
        assert_eq!(safety.source_confidence, "low");
    }

    #[test]
    fn safety_provenance_classifies_install_source_fallbacks_offline() {
        let cases = [
            ("npm:pi-tool@1.2.3", "npm", "low"),
            ("git:org/pi-tool", "git", "low"),
            ("https://example.com/pi-tool.js", "url", "low"),
            ("./extensions/local-tool.js", "local", "low"),
            ("", "unknown", "degraded"),
        ];

        for (source, expected_source_type, expected_confidence) in cases {
            let safety = ExtensionSafetyProvenance::from_install_source(source);

            assert_eq!(safety.source_type, expected_source_type);
            assert_eq!(safety.source_confidence, expected_confidence);
            assert_eq!(safety.license_status, "unknown");
            assert_eq!(safety.freshness, "unknown");
        }
    }

    #[test]
    fn safety_provenance_reports_provider_event_heavy_inclusion_risk() {
        let entry = InclusionEntry {
            id: "community/heavy".to_string(),
            name: Some("heavy".to_string()),
            tier: Some("tier-1".to_string()),
            score: Some(80.0),
            category: ExtensionCategory::Multi,
            registrations: vec![
                "registerProvider".to_string(),
                "registerEventHook".to_string(),
            ],
            version_pin: Some(VersionPin::Git {
                repo: "https://github.com/example/heavy".to_string(),
                path: None,
                commit: Some("abc123".to_string()),
            }),
            sha256: Some("abc".to_string()),
            artifact_path: None,
            license: Some("MIT".to_string()),
            source_tier: Some("community".to_string()),
            rationale: None,
            directory: None,
            provenance: None,
            capabilities: Some(vec!["read".to_string()]),
            risk_level: None,
            inclusion_rationale: None,
        };

        let safety = ExtensionSafetyProvenance::from_inclusion_entry(&entry);

        assert_eq!(safety.source_type, "community");
        assert!(
            safety
                .registration_categories
                .contains(&"provider".to_string())
        );
        assert!(
            safety
                .registration_categories
                .contains(&"event_hook".to_string())
        );
        assert_eq!(safety.risk_profile, "elevated");
    }

    #[test]
    fn safety_provenance_redacts_invalid_capabilities_without_leaking_secrets() {
        let entry = InclusionEntry {
            id: "npm/secret-tool".to_string(),
            name: Some("secret-tool".to_string()),
            tier: Some("tier-1".to_string()),
            score: Some(91.0),
            category: ExtensionCategory::Tool,
            registrations: vec!["registerTool".to_string()],
            version_pin: Some(VersionPin::Npm {
                package: "secret-tool".to_string(),
                version: "1.0.0".to_string(),
                registry_url: "https://registry.npmjs.org".to_string(),
            }),
            sha256: Some("abc".to_string()),
            artifact_path: None,
            license: Some("MIT".to_string()),
            source_tier: Some("npm-registry".to_string()),
            rationale: Some("prompt leak marker sk-should-not-appear".to_string()),
            directory: None,
            provenance: Some(serde_json::json!({
                "prompt": "OPENAI_API_KEY=sk-should-not-appear",
                "url": "https://example.com/?token=sk-should-not-appear"
            })),
            capabilities: Some(vec![
                "exec".to_string(),
                "OPENAI_API_KEY=sk-should-not-appear".to_string(),
            ]),
            risk_level: None,
            inclusion_rationale: None,
        };

        let safety = ExtensionSafetyProvenance::from_inclusion_entry(&entry);
        let serialized = serde_json::to_string(&safety).expect("serialize safety provenance");

        assert_eq!(safety.schema, EXTENSION_SAFETY_PROVENANCE_SCHEMA);
        assert_eq!(safety.risk_profile, "high");
        assert!(
            safety
                .requested_capabilities
                .contains(&"redacted-capability".to_string())
        );
        assert!(!serialized.contains("sk-should-not-appear"));
        assert!(!serialized.contains("OPENAI_API_KEY"));
        assert!(!serialized.contains("token="));
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
    fn store_save_overwrites_existing_file() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let path = temp_dir.path().join("index.json");
        let store = ExtensionIndexStore::new(path);

        let mut first = ExtensionIndex::new_empty();
        first.entries.push(test_entry(
            "npm/first",
            "first",
            Some("first version"),
            &["test"],
        ));
        store.save(&first).expect("save first");

        let mut second = ExtensionIndex::new_empty();
        second.generated_at = Some("2026-03-09T00:00:00Z".to_string());
        second.last_refreshed_at = Some("2026-03-09T01:00:00Z".to_string());
        second.entries.push(test_entry(
            "npm/second",
            "second",
            Some("second version"),
            &["fresh"],
        ));
        store.save(&second).expect("overwrite existing cache");

        let loaded = store.load().expect("load").expect("some");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].name, "second");
        assert_eq!(
            loaded.entries[0].description.as_deref(),
            Some("second version")
        );
        assert_eq!(
            loaded.last_refreshed_at.as_deref(),
            Some("2026-03-09T01:00:00Z")
        );
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

    mod proptest_extension_index {
        use super::*;
        use proptest::prelude::*;

        fn make_entry(id: &str, name: &str) -> ExtensionIndexEntry {
            ExtensionIndexEntry {
                id: id.to_string(),
                name: name.to_string(),
                description: None,
                tags: Vec::new(),
                license: None,
                source: None,
                install_source: None,
            }
        }

        proptest! {
            /// `non_empty` returns None for whitespace-only strings.
            #[test]
            fn non_empty_whitespace(ws in "[ \\t\\n]{0,10}") {
                assert!(non_empty(&ws).is_none());
            }

            /// `non_empty` returns trimmed value for non-empty strings.
            #[test]
            fn non_empty_trims(s in "[a-z]{1,10}", ws in "[ \\t]{0,3}") {
                let padded = format!("{ws}{s}{ws}");
                let result = non_empty(&padded).unwrap();
                assert_eq!(result, s);
            }

            /// `normalize_license` filters "unknown" (case-insensitive).
            #[test]
            fn normalize_license_filters_unknown(
                case_idx in 0..3usize
            ) {
                let variants = ["unknown", "UNKNOWN", "Unknown"];
                assert!(normalize_license(Some(variants[case_idx])).is_none());
            }

            /// `normalize_license(None)` returns None.
            #[test]
            fn normalize_license_none(_dummy in 0..1u8) {
                assert!(normalize_license(None).is_none());
            }

            /// `normalize_license` passes through valid licenses.
            #[test]
            fn normalize_license_passthrough(s in "[A-Z]{3,10}") {
                if !s.eq_ignore_ascii_case("unknown") {
                    assert!(normalize_license(Some(&s)).is_some());
                }
            }

            /// `score_entry` is zero for empty token list.
            #[test]
            fn score_empty_tokens(name in "[a-z]{1,10}") {
                let entry = make_entry("id", &name);
                assert_eq!(score_entry(&entry, &[]), 0);
            }

            /// `score_entry` is non-negative.
            #[test]
            fn score_non_negative(
                name in "[a-z]{1,10}",
                token in "[a-z]{1,5}"
            ) {
                let entry = make_entry("id", &name);
                assert!(score_entry(&entry, &[token]) >= 0);
            }

            /// `score_entry` is case-insensitive.
            #[test]
            fn score_case_insensitive(name in "[a-z]{1,10}") {
                // score_entry expects pre-lowered tokens (search() lowercases them).
                // The case-insensitivity is on the *entry* fields, not tokens.
                let lower_entry = make_entry("id", &name);
                let upper_entry = make_entry("id", &name.to_uppercase());
                let search_token = vec![name];
                assert_eq!(score_entry(&lower_entry, &search_token), score_entry(&upper_entry, &search_token));
            }

            /// Name match gives 300 points per token.
            #[test]
            fn score_name_match(name in "[a-z]{3,8}") {
                let entry = make_entry("different-id", &name);
                let score = score_entry(&entry, &[name]);
                // At minimum 300 for name match (might also match id/description/tags)
                assert!(score >= 300);
            }

            /// `merge_tags` deduplicates.
            #[test]
            fn merge_tags_dedup(tag in "[a-z]{1,10}") {
                let result = merge_tags(
                    vec![tag.clone(), tag.clone()],
                    vec![tag.clone()],
                );
                assert_eq!(result.len(), 1);
                assert_eq!(result[0], tag);
            }

            /// `merge_tags` filters empty/whitespace.
            #[test]
            fn merge_tags_filters_empty(tag in "[a-z]{1,10}") {
                let result = merge_tags(
                    vec![tag, String::new(), "  ".to_string()],
                    vec![],
                );
                assert_eq!(result.len(), 1);
            }

            /// `merge_tags` result is sorted (BTreeSet).
            #[test]
            fn merge_tags_sorted(
                a in "[a-z]{1,5}",
                b in "[a-z]{1,5}",
                c in "[a-z]{1,5}"
            ) {
                let result = merge_tags(vec![c, a], vec![b]);
                for w in result.windows(2) {
                    assert!(w[0] <= w[1]);
                }
            }

            /// `merge_tags` preserves all unique tags from both sides.
            #[test]
            fn merge_tags_preserves(
                left in prop::collection::vec("[a-z]{1,5}", 0..5),
                right in prop::collection::vec("[a-z]{1,5}", 0..5)
            ) {
                let result = merge_tags(left.clone(), right.clone());
                // Every non-empty tag from either side should be in result
                for tag in left.iter().chain(right.iter()) {
                    let trimmed = tag.trim();
                    if !trimmed.is_empty() {
                        assert!(
                            result.contains(&trimmed.to_string()),
                            "missing tag: {trimmed}"
                        );
                    }
                }
            }

            /// `merge_entries` keeps casefolded ids unique and sorted.
            #[test]
            fn merge_entries_unique_sorted_casefold_ids(
                existing in prop::collection::vec(("[A-Za-z]{1,8}", "[a-z]{1,8}"), 0..10),
                npm in prop::collection::vec(("[A-Za-z]{1,8}", "[a-z]{1,8}"), 0..10),
                git in prop::collection::vec(("[A-Za-z]{1,8}", "[a-z]{1,8}"), 0..10)
            ) {
                let to_entries = |rows: Vec<(String, String)>, prefix: &str| {
                    rows.into_iter()
                        .map(|(id, name)| make_entry(&format!("{prefix}/{id}"), &name))
                        .collect::<Vec<_>>()
                };
                let merged = merge_entries(
                    to_entries(existing, "npm"),
                    to_entries(npm, "npm"),
                    to_entries(git, "git"),
                );

                let lower_ids = merged
                    .iter()
                    .map(|entry| entry.id.to_ascii_lowercase())
                    .collect::<Vec<_>>();
                let mut sorted = lower_ids.clone();
                sorted.sort();
                assert_eq!(lower_ids, sorted);

                let unique = lower_ids.iter().cloned().collect::<std::collections::BTreeSet<_>>();
                assert_eq!(unique.len(), lower_ids.len());
            }

            /// `search` output is bounded by limit and sorted by non-increasing score.
            #[test]
            fn search_bounded_and_score_sorted(
                rows in prop::collection::vec(("[a-z]{1,8}", "[a-z]{1,8}", prop::option::of("[a-z ]{1,20}")), 0..16),
                query in "[a-z]{1,6}",
                limit in 0usize..16usize
            ) {
                let entries = rows
                    .into_iter()
                    .map(|(id, name, description)| ExtensionIndexEntry {
                        id: format!("npm/{id}"),
                        name,
                        description: description.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
                        tags: vec!["tag".to_string()],
                        license: None,
                        source: None,
                        install_source: Some(format!("npm:{id}")),
                    })
                    .collect::<Vec<_>>();
                let index = ExtensionIndex {
                    schema: EXTENSION_INDEX_SCHEMA.to_string(),
                    version: EXTENSION_INDEX_VERSION,
                    generated_at: None,
                    last_refreshed_at: None,
                    entries,
                };

                let hits = index.search(&query, limit);
                assert!(hits.len() <= limit);
                assert!(hits.windows(2).all(|pair| pair[0].score >= pair[1].score));
                assert!(hits.iter().all(|hit| hit.score > 0));
            }

            /// Name ambiguity must fail-open to `None`; exact id remains resolvable.
            #[test]
            fn resolve_install_source_ambiguous_name_none_exact_id_some(
                name in "[a-z]{1,10}",
                left in "[a-z]{1,8}",
                right in "[a-z]{1,8}"
            ) {
                prop_assume!(!left.eq_ignore_ascii_case(&right));

                let left_id = format!("npm/{left}");
                let right_id = format!("npm/{right}");
                let left_install = format!("npm:{left}@1.0.0");
                let right_install = format!("npm:{right}@2.0.0");

                let index = ExtensionIndex {
                    schema: EXTENSION_INDEX_SCHEMA.to_string(),
                    version: EXTENSION_INDEX_VERSION,
                    generated_at: None,
                    last_refreshed_at: None,
                    entries: vec![
                        ExtensionIndexEntry {
                            id: left_id.clone(),
                            name: name.clone(),
                            description: None,
                            tags: Vec::new(),
                            license: None,
                            source: Some(ExtensionIndexSource::Npm {
                                package: left,
                                version: Some("1.0.0".to_string()),
                                url: None,
                            }),
                            install_source: Some(left_install.clone()),
                        },
                        ExtensionIndexEntry {
                            id: right_id.clone(),
                            name: name.clone(),
                            description: None,
                            tags: Vec::new(),
                            license: None,
                            source: Some(ExtensionIndexSource::Npm {
                                package: right,
                                version: Some("2.0.0".to_string()),
                                url: None,
                            }),
                            install_source: Some(right_install.clone()),
                        },
                    ],
                };

                assert_eq!(index.resolve_install_source(&name), None);
                assert_eq!(index.resolve_install_source(&left_id), Some(left_install));
                assert_eq!(index.resolve_install_source(&right_id), Some(right_install));
            }

            /// `ExtensionIndexSource` serde roundtrip for Npm variant.
            #[test]
            fn source_npm_serde(pkg in "[a-z]{1,10}", ver in "[0-9]\\.[0-9]\\.[0-9]") {
                let source = ExtensionIndexSource::Npm {
                    package: pkg,
                    version: Some(ver),
                    url: None,
                };
                let json = serde_json::to_string(&source).unwrap();
                let _: ExtensionIndexSource = serde_json::from_str(&json).unwrap();
            }

            /// `ExtensionIndexSource` serde roundtrip for Git variant.
            #[test]
            fn source_git_serde(repo in "[a-z]{1,10}/[a-z]{1,10}") {
                let source = ExtensionIndexSource::Git {
                    repo,
                    path: None,
                    r#ref: None,
                };
                let json = serde_json::to_string(&source).unwrap();
                let _: ExtensionIndexSource = serde_json::from_str(&json).unwrap();
            }

            /// `ExtensionIndexEntry` serde roundtrip.
            #[test]
            fn entry_serde_roundtrip(
                id in "[a-z]{1,10}",
                name in "[a-z]{1,10}"
            ) {
                let entry = make_entry(&id, &name);
                let json = serde_json::to_string(&entry).unwrap();
                let back: ExtensionIndexEntry = serde_json::from_str(&json).unwrap();
                assert_eq!(back.id, id);
                assert_eq!(back.name, name);
            }
        }
    }
}
