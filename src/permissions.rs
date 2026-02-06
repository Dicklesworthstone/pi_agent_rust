//! Persistent storage for extension capability decisions.
//!
//! When a user chooses "Allow Always" or "Deny Always" for an extension
//! capability prompt, the decision is recorded here so it survives across
//! sessions.  Decisions are keyed by `(extension_id, capability)` and
//! optionally scoped to a version range.

use crate::config::Config;
use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

/// On-disk schema version.
const CURRENT_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A persisted capability decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedDecision {
    /// The capability that was prompted (e.g. `exec`, `http`).
    pub capability: String,

    /// `true` = allowed, `false` = denied.
    pub allow: bool,

    /// ISO-8601 timestamp when the decision was made.
    pub decided_at: String,

    /// Optional ISO-8601 expiry.  `None` means the decision never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,

    /// Optional semver range string (e.g. `>=1.0.0`).
    /// If the extension's version no longer satisfies this range the decision
    /// is treated as absent (user gets re-prompted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_range: Option<String>,
}

/// Root structure serialized to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PermissionsFile {
    version: u32,
    /// `extension_id` → list of decisions.
    decisions: HashMap<String, Vec<PersistedDecision>>,
}

impl Default for PermissionsFile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            decisions: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/// In-memory mirror of the on-disk permissions file with load/save helpers.
#[derive(Debug, Clone)]
pub struct PermissionStore {
    path: PathBuf,
    /// `extension_id` → `capability` → decision.
    decisions: HashMap<String, HashMap<String, PersistedDecision>>,
}

impl PermissionStore {
    /// Open (or create) the permissions store at the default global path.
    pub fn open_default() -> Result<Self> {
        Self::open(&Config::permissions_path())
    }

    /// Open (or create) the permissions store at a specific path.
    pub fn open(path: &Path) -> Result<Self> {
        let decisions = if path.exists() {
            let raw = std::fs::read_to_string(path).map_err(|e| {
                Error::config(format!(
                    "Failed to read permissions file {}: {e}",
                    path.display()
                ))
            })?;
            let file: PermissionsFile = serde_json::from_str(&raw).map_err(|e| {
                Error::config(format!(
                    "Failed to parse permissions file {}: {e}",
                    path.display()
                ))
            })?;
            // Convert Vec<PersistedDecision> → HashMap keyed by capability.
            file.decisions
                .into_iter()
                .map(|(ext_id, decs)| {
                    let by_cap: HashMap<String, PersistedDecision> = decs
                        .into_iter()
                        .map(|d| (d.capability.clone(), d))
                        .collect();
                    (ext_id, by_cap)
                })
                .collect()
        } else {
            HashMap::new()
        };

        Ok(Self {
            path: path.to_path_buf(),
            decisions,
        })
    }

    /// Look up a persisted decision for `(extension_id, capability)`.
    ///
    /// Returns `Some(true)` for allow, `Some(false)` for deny, `None` if no
    /// decision is stored (or the stored decision has expired).
    pub fn lookup(&self, extension_id: &str, capability: &str) -> Option<bool> {
        let by_cap = self.decisions.get(extension_id)?;
        let dec = by_cap.get(capability)?;

        // Check expiry.
        if let Some(ref exp) = dec.expires_at {
            let now = now_iso8601();
            if now > *exp {
                return None;
            }
        }

        Some(dec.allow)
    }

    /// Record a decision and persist to disk.
    pub fn record(&mut self, extension_id: &str, capability: &str, allow: bool) -> Result<()> {
        let decision = PersistedDecision {
            capability: capability.to_string(),
            allow,
            decided_at: now_iso8601(),
            expires_at: None,
            version_range: None,
        };

        self.decisions
            .entry(extension_id.to_string())
            .or_default()
            .insert(capability.to_string(), decision);

        self.save()
    }

    /// Record a decision with a version range constraint.
    pub fn record_with_version(
        &mut self,
        extension_id: &str,
        capability: &str,
        allow: bool,
        version_range: &str,
    ) -> Result<()> {
        let decision = PersistedDecision {
            capability: capability.to_string(),
            allow,
            decided_at: now_iso8601(),
            expires_at: None,
            version_range: Some(version_range.to_string()),
        };

        self.decisions
            .entry(extension_id.to_string())
            .or_default()
            .insert(capability.to_string(), decision);

        self.save()
    }

    /// Remove all decisions for a specific extension.
    pub fn revoke_extension(&mut self, extension_id: &str) -> Result<()> {
        self.decisions.remove(extension_id);
        self.save()
    }

    /// Remove all persisted decisions.
    pub fn reset(&mut self) -> Result<()> {
        self.decisions.clear();
        self.save()
    }

    /// List all persisted decisions grouped by extension.
    pub fn list(&self) -> &HashMap<String, HashMap<String, PersistedDecision>> {
        &self.decisions
    }

    /// Seed the in-memory cache of an [`ExtensionManager`]-style
    /// `HashMap<String, HashMap<String, bool>>` from persisted decisions.
    ///
    /// Only non-expired entries are included.
    pub fn to_cache_map(&self) -> HashMap<String, HashMap<String, bool>> {
        let now = now_iso8601();
        self.decisions
            .iter()
            .map(|(ext_id, by_cap)| {
                let filtered: HashMap<String, bool> = by_cap
                    .iter()
                    .filter(|(_, dec)| dec.expires_at.as_ref().map_or(true, |exp| now <= *exp))
                    .map(|(cap, dec)| (cap.clone(), dec.allow))
                    .collect();
                (ext_id.clone(), filtered)
            })
            .filter(|(_, m)| !m.is_empty())
            .collect()
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Atomic write to disk following the same pattern as `config.rs`.
    fn save(&self) -> Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }

        // Convert internal HashMap → Vec for stable serialization.
        let file = PermissionsFile {
            version: CURRENT_VERSION,
            decisions: self
                .decisions
                .iter()
                .map(|(ext_id, by_cap)| {
                    let decs: Vec<PersistedDecision> = by_cap.values().cloned().collect();
                    (ext_id.clone(), decs)
                })
                .collect(),
        };

        let mut contents = serde_json::to_string_pretty(&file)?;
        contents.push('\n');

        let mut tmp = NamedTempFile::new_in(parent)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let perms = std::fs::Permissions::from_mode(0o600);
            tmp.as_file().set_permissions(perms)?;
        }

        tmp.write_all(contents.as_bytes())?;
        tmp.as_file().sync_all()?;

        tmp.persist(&self.path).map_err(|err| {
            Error::config(format!(
                "Failed to persist permissions file to {}: {}",
                self.path.display(),
                err.error
            ))
        })?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_iso8601() -> String {
    // Use wall-clock time.  We don't need sub-second precision for expiry
    // comparisons, but include it for diagnostics.
    let now = std::time::SystemTime::now();
    let duration = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    // Simple ISO-8601 without pulling in chrono: YYYY-MM-DDThh:mm:ssZ
    // (good enough for lexicographic comparison).
    let days = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    // Convert days since epoch to date using a basic algorithm.
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's `chrono`-compatible date library.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let store = PermissionStore::open(&path).unwrap();
        assert!(store.list().is_empty());

        // File should not exist until a record is made.
        assert!(!path.exists());
    }

    #[test]
    fn record_and_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("my-ext", "exec", true).unwrap();
        store.record("my-ext", "env", false).unwrap();
        store.record("other-ext", "http", true).unwrap();

        assert_eq!(store.lookup("my-ext", "exec"), Some(true));
        assert_eq!(store.lookup("my-ext", "env"), Some(false));
        assert_eq!(store.lookup("other-ext", "http"), Some(true));
        assert_eq!(store.lookup("unknown", "exec"), None);
        assert_eq!(store.lookup("my-ext", "unknown"), None);

        // Reload from disk.
        let store2 = PermissionStore::open(&path).unwrap();
        assert_eq!(store2.lookup("my-ext", "exec"), Some(true));
        assert_eq!(store2.lookup("my-ext", "env"), Some(false));
        assert_eq!(store2.lookup("other-ext", "http"), Some(true));
    }

    #[test]
    fn revoke_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("my-ext", "exec", true).unwrap();
        store.record("my-ext", "env", false).unwrap();
        store.record("other-ext", "http", true).unwrap();

        store.revoke_extension("my-ext").unwrap();

        assert_eq!(store.lookup("my-ext", "exec"), None);
        assert_eq!(store.lookup("my-ext", "env"), None);
        assert_eq!(store.lookup("other-ext", "http"), Some(true));

        // Persists to disk.
        let store2 = PermissionStore::open(&path).unwrap();
        assert_eq!(store2.lookup("my-ext", "exec"), None);
    }

    #[test]
    fn reset_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("a", "exec", true).unwrap();
        store.record("b", "http", false).unwrap();
        store.reset().unwrap();

        assert!(store.list().is_empty());

        let store2 = PermissionStore::open(&path).unwrap();
        assert!(store2.list().is_empty());
    }

    #[test]
    fn to_cache_map_filters_expired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();

        // Insert a non-expired decision directly.
        store
            .decisions
            .entry("ext1".to_string())
            .or_default()
            .insert(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: Some("2099-12-31T23:59:59Z".to_string()),
                    version_range: None,
                },
            );

        // Insert an expired decision.
        store
            .decisions
            .entry("ext1".to_string())
            .or_default()
            .insert(
                "env".to_string(),
                PersistedDecision {
                    capability: "env".to_string(),
                    allow: false,
                    decided_at: "2020-01-01T00:00:00Z".to_string(),
                    expires_at: Some("2020-06-01T00:00:00Z".to_string()),
                    version_range: None,
                },
            );

        let cache = store.to_cache_map();
        assert_eq!(cache.get("ext1").and_then(|m| m.get("exec")), Some(&true));
        // Expired entry should be absent.
        assert_eq!(cache.get("ext1").and_then(|m| m.get("env")), None);
    }

    #[test]
    fn overwrite_decision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();
        assert_eq!(store.lookup("ext", "exec"), Some(true));

        // Overwrite with deny.
        store.record("ext", "exec", false).unwrap();
        assert_eq!(store.lookup("ext", "exec"), Some(false));

        // Persists.
        let store2 = PermissionStore::open(&path).unwrap();
        assert_eq!(store2.lookup("ext", "exec"), Some(false));
    }

    #[test]
    fn version_range_stored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store
            .record_with_version("ext", "exec", true, ">=1.0.0")
            .unwrap();

        let store2 = PermissionStore::open(&path).unwrap();
        let dec = store2
            .decisions
            .get("ext")
            .and_then(|m| m.get("exec"))
            .unwrap();
        assert_eq!(dec.version_range.as_deref(), Some(">=1.0.0"));
        assert!(dec.allow);
    }

    #[test]
    fn now_iso8601_format() {
        let ts = now_iso8601();
        // Basic format check: YYYY-MM-DDThh:mm:ssZ
        assert_eq!(ts.len(), 20);
        assert!(ts.ends_with('Z'));
        assert_eq!(ts.as_bytes()[4], b'-');
        assert_eq!(ts.as_bytes()[7], b'-');
        assert_eq!(ts.as_bytes()[10], b'T');
        assert_eq!(ts.as_bytes()[13], b':');
        assert_eq!(ts.as_bytes()[16], b':');
    }
}
