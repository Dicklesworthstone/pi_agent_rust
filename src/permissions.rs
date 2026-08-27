//! Persistent storage for extension capability decisions.
//!
//! When a user chooses "Allow Always" or "Deny Always" for an extension
//! capability prompt, the decision is recorded here so it survives across
//! sessions.  Decisions are keyed by `(extension_id, capability)` and
//! optionally scoped to a version range.

use crate::config::Config;
use crate::error::{Error, Result};
use chrono::{DateTime, Utc};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
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
    decisions: BTreeMap<String, Vec<PersistedDecision>>,
}

impl Default for PermissionsFile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            decisions: BTreeMap::new(),
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

    pub(crate) fn empty_at(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            decisions: HashMap::new(),
        }
    }

    /// Open (or create) the permissions store at a specific path.
    pub fn open(path: &Path) -> Result<Self> {
        let decisions = load_permissions_decisions(path)?;

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

        if !decision_is_active(dec, Utc::now()) {
            return None;
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

        self.update_persisted_decisions(|decisions| {
            decisions
                .entry(extension_id.to_string())
                .or_default()
                .insert(capability.to_string(), decision);
        })
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

        self.update_persisted_decisions(|decisions| {
            decisions
                .entry(extension_id.to_string())
                .or_default()
                .insert(capability.to_string(), decision);
        })
    }

    /// Remove all decisions for a specific extension.
    pub fn revoke_extension(&mut self, extension_id: &str) -> Result<()> {
        self.update_persisted_decisions(|decisions| {
            decisions.remove(extension_id);
        })
    }

    /// Remove all persisted decisions.
    pub fn reset(&mut self) -> Result<()> {
        self.update_persisted_decisions(HashMap::clear)
    }

    /// List all persisted decisions grouped by extension.
    pub const fn list(&self) -> &HashMap<String, HashMap<String, PersistedDecision>> {
        &self.decisions
    }

    /// Seed the in-memory cache of an `ExtensionManager`-style
    /// `HashMap<String, HashMap<String, bool>>` from persisted decisions.
    ///
    /// Only non-expired entries are included.
    pub fn to_cache_map(&self) -> HashMap<String, HashMap<String, bool>> {
        let now = Utc::now();
        self.decisions
            .iter()
            .map(|(ext_id, by_cap)| {
                let filtered: HashMap<String, bool> = by_cap
                    .iter()
                    .filter(|(_, dec)| decision_is_active(dec, now))
                    .map(|(cap, dec)| (cap.clone(), dec.allow))
                    .collect();
                (ext_id.clone(), filtered)
            })
            .filter(|(_, m)| !m.is_empty())
            .collect()
    }

    /// Retrieve the full decision cache (including version ranges) for
    /// runtime enforcement.
    pub fn to_decision_cache(&self) -> HashMap<String, HashMap<String, PersistedDecision>> {
        let now = Utc::now();
        self.decisions
            .iter()
            .map(|(ext_id, by_cap)| {
                let filtered: HashMap<String, PersistedDecision> = by_cap
                    .iter()
                    .filter(|(_, dec)| decision_is_active(dec, now))
                    .map(|(cap, dec)| (cap.clone(), dec.clone()))
                    .collect();
                (ext_id.clone(), filtered)
            })
            .filter(|(_, m)| !m.is_empty())
            .collect()
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    fn update_persisted_decisions(
        &mut self,
        update: impl FnOnce(&mut HashMap<String, HashMap<String, PersistedDecision>>),
    ) -> Result<()> {
        let _process_guard = permissions_persist_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lock_handle = open_permissions_lock_file(&self.path)?;
        let _file_guard = lock_permissions_file(lock_handle, Duration::from_secs(30))?;

        match load_permissions_decisions(&self.path) {
            Ok(decisions) => self.decisions = decisions,
            Err(err) => {
                tracing::warn!(
                    "Failed to reload extension permissions before update; repairing from in-memory state: {err}"
                );
            }
        }
        update(&mut self.decisions);
        self.save_unlocked()
    }

    /// Atomic write to disk following the same pattern as `config.rs`.
    fn save_unlocked(&self) -> Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }

        // Convert internal HashMap → ordered on-disk structure for stable serialization.
        let file = PermissionsFile {
            version: CURRENT_VERSION,
            decisions: {
                let mut extension_ids = self.decisions.keys().cloned().collect::<Vec<_>>();
                extension_ids.sort();
                extension_ids
                    .into_iter()
                    .map(|extension_id| {
                        let by_cap = self
                            .decisions
                            .get(&extension_id)
                            .expect("extension id collected from decision map");
                        let mut decisions = by_cap.values().cloned().collect::<Vec<_>>();
                        decisions.sort_by(|left, right| left.capability.cmp(&right.capability));
                        (extension_id, decisions)
                    })
                    .collect()
            },
        };

        let mut contents = serde_json::to_string_pretty(&file)?;
        contents.push('\n');

        for attempt in 0..2 {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }

            let mut tmp = NamedTempFile::new_in(parent)?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let perms = std::fs::Permissions::from_mode(0o600);
                tmp.as_file().set_permissions(perms)?;
            }

            tmp.write_all(contents.as_bytes())?;
            tmp.as_file().sync_all()?;

            match tmp.persist(&self.path) {
                Ok(_) => break,
                Err(err) if err.error.kind() == std::io::ErrorKind::NotFound && attempt == 0 => {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)?;
                    }
                }
                Err(err) => {
                    return Err(Error::config(format!(
                        "Failed to persist permissions file to {}: {}",
                        self.path.display(),
                        err.error
                    )));
                }
            }
        }
        sync_permissions_parent_dir(&self.path)?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_permissions_decisions(
    path: &Path,
) -> Result<HashMap<String, HashMap<String, PersistedDecision>>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }

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
    if file.version != CURRENT_VERSION {
        return Err(Error::config(format!(
            "Unsupported permissions file schema version {} in {} (expected {})",
            file.version,
            path.display(),
            CURRENT_VERSION
        )));
    }

    Ok(file
        .decisions
        .into_iter()
        .map(|(ext_id, decs)| {
            let by_cap: HashMap<String, PersistedDecision> = decs
                .into_iter()
                .map(|d| (d.capability.clone(), d))
                .collect();
            (ext_id, by_cap)
        })
        .collect())
}

fn permissions_persist_lock() -> &'static Mutex<()> {
    static PERSIST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    PERSIST_LOCK.get_or_init(|| Mutex::new(()))
}

fn permissions_lock_path(path: &Path) -> PathBuf {
    let mut lock_path = path.to_path_buf();
    let mut file_name = path.file_name().map_or_else(
        || std::ffi::OsString::from("permissions"),
        std::ffi::OsString::from,
    );
    file_name.push(".lock");
    lock_path.set_file_name(file_name);
    lock_path
}

fn open_permissions_lock_file(path: &Path) -> Result<File> {
    let lock_path = permissions_lock_path(path);
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let mut options = File::options();
    options.read(true).write(true).create(true).truncate(false);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }

    options.open(&lock_path).map_err(|err| {
        Error::config(format!(
            "Failed to open permissions lock file {}: {err}",
            lock_path.display()
        ))
    })
}

fn lock_permissions_file(file: File, timeout: Duration) -> Result<PermissionsLockGuard> {
    let start = Instant::now();
    loop {
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => return Ok(PermissionsLockGuard { file }),
            Ok(false) => {}
            Err(err) => {
                return Err(Error::config(format!(
                    "Failed to lock permissions file: {err}"
                )));
            }
        }

        if start.elapsed() >= timeout {
            return Err(Error::config("Timed out waiting for permissions lock"));
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

struct PermissionsLockGuard {
    file: File,
}

impl Drop for PermissionsLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(unix)]
fn sync_permissions_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_permissions_parent_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn now_iso8601() -> String {
    // Use wall-clock time.  We don't need sub-second precision for expiry
    // comparisons, but include it for diagnostics.
    let now = std::time::SystemTime::now();
    let duration = now.duration_since(std::time::UNIX_EPOCH).unwrap_or({
        // Clock is before UNIX_EPOCH - use epoch as fallback to prevent
        // timestamp manipulation attacks on permission expiry
        std::time::Duration::ZERO
    });
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

fn parse_expiry_timestamp(expires_at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(expires_at)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn decision_is_active(decision: &PersistedDecision, now: DateTime<Utc>) -> bool {
    decision.expires_at.as_deref().is_none_or(|expires_at| {
        parse_expiry_timestamp(expires_at).is_some_and(|expiry| now <= expiry)
    })
}

/// Convert days since Unix epoch to (year, month, day).
/// Uses saturating arithmetic to prevent overflow attacks.
const fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's `chrono`-compatible date library.
    // Use saturating arithmetic to prevent overflow with malicious inputs.
    let z = days.saturating_add(719_468);
    let era = z / 146_097;
    let era_days = era.saturating_mul(146_097);
    let doe = z.saturating_sub(era_days);
    let doe_div_1460 = doe / 1460;
    let doe_div_36524 = doe / 36524;
    let doe_div_146096 = doe / 146_096;
    let yoe = doe
        .saturating_sub(doe_div_1460)
        .saturating_add(doe_div_36524)
        .saturating_sub(doe_div_146096)
        / 365;
    let era_years = era.saturating_mul(400);
    let y = yoe.saturating_add(era_years);
    let yoe_times_365 = yoe.saturating_mul(365);
    let yoe_div_4 = yoe / 4;
    let yoe_div_100 = yoe / 100;
    let days_in_year = yoe_times_365
        .saturating_add(yoe_div_4)
        .saturating_sub(yoe_div_100);
    let doy = doe.saturating_sub(days_in_year);
    let doy_times_5 = doy.saturating_mul(5);
    let mp = doy_times_5.saturating_add(2) / 153;
    let mp_times_153 = mp.saturating_mul(153);
    let d = doy
        .saturating_sub((mp_times_153.saturating_add(2)) / 5)
        .saturating_add(1);
    let m = if mp < 10 {
        mp.saturating_add(3)
    } else {
        mp.saturating_sub(9)
    };
    let y = if m <= 2 { y.saturating_add(1) } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

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

    // -----------------------------------------------------------------------
    // days_to_ymd tests
    // -----------------------------------------------------------------------

    #[test]
    fn days_to_ymd_epoch() {
        // Day 0 = 1970-01-01
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
    }

    #[test]
    fn days_to_ymd_known_dates() {
        // 2000-01-01 = day 10957
        assert_eq!(days_to_ymd(10957), (2000, 1, 1));
        // 2000-02-29 (leap year) = day 10957 + 31 (Jan) + 28 (Feb 1..28) = 11016
        assert_eq!(days_to_ymd(11016), (2000, 2, 29));
        // 2000-03-01 = day 11017
        assert_eq!(days_to_ymd(11017), (2000, 3, 1));
        // 2024-01-01 = day 19723
        assert_eq!(days_to_ymd(19723), (2024, 1, 1));
    }

    #[test]
    fn days_to_ymd_dec_31() {
        // 1970-12-31 = day 364
        assert_eq!(days_to_ymd(364), (1970, 12, 31));
        // 1971-01-01 = day 365
        assert_eq!(days_to_ymd(365), (1971, 1, 1));
    }

    #[test]
    fn days_to_ymd_leap_year_boundary() {
        // 1972 is a leap year: Feb 29 = day 789 (730 days for 1970-1971 + 31 + 28)
        // 1970: 365, 1971: 365 = 730
        // Jan 1972: 31 → 761, Feb 1-28: 28 → 789 = Feb 29
        assert_eq!(days_to_ymd(789), (1972, 2, 29));
        assert_eq!(days_to_ymd(790), (1972, 3, 1));
    }

    #[test]
    fn days_to_ymd_far_future() {
        // 2099-12-31 = day 47481, 2100-01-01 = day 47482
        assert_eq!(days_to_ymd(47_481), (2099, 12, 31));
        assert_eq!(days_to_ymd(47_482), (2100, 1, 1));
    }

    // -----------------------------------------------------------------------
    // now_iso8601 additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn now_iso8601_lexicographic_order() {
        let ts1 = now_iso8601();
        let ts2 = now_iso8601();
        // Second call should be >= first (same second or later)
        assert!(ts2 >= ts1);
    }

    #[test]
    fn now_iso8601_year_plausible() {
        let ts = now_iso8601();
        let year: u32 = ts[0..4].parse().unwrap();
        assert!(year >= 2024);
        assert!(year <= 2100);
    }

    // -----------------------------------------------------------------------
    // PersistedDecision serde tests
    // -----------------------------------------------------------------------

    #[test]
    fn persisted_decision_serde_minimal() {
        // Optional fields should be omitted when None
        let dec = PersistedDecision {
            capability: "exec".to_string(),
            allow: true,
            decided_at: "2026-01-15T10:30:00Z".to_string(),
            expires_at: None,
            version_range: None,
        };
        let json = serde_json::to_string(&dec).unwrap();
        assert!(!json.contains("expires_at"));
        assert!(!json.contains("version_range"));

        let roundtrip: PersistedDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, dec);
    }

    #[test]
    fn persisted_decision_serde_full() {
        let dec = PersistedDecision {
            capability: "http".to_string(),
            allow: false,
            decided_at: "2026-01-15T10:30:00Z".to_string(),
            expires_at: Some("2026-06-15T10:30:00Z".to_string()),
            version_range: Some(">=2.0.0".to_string()),
        };
        let json = serde_json::to_string(&dec).unwrap();
        assert!(json.contains("expires_at"));
        assert!(json.contains("version_range"));

        let roundtrip: PersistedDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, dec);
    }

    #[test]
    fn persisted_decision_deserialize_missing_optionals() {
        // JSON without optional fields should deserialize fine
        let json = r#"{"capability":"exec","allow":true,"decided_at":"2026-01-01T00:00:00Z"}"#;
        let dec: PersistedDecision = serde_json::from_str(json).unwrap();
        assert_eq!(dec.capability, "exec");
        assert!(dec.allow);
        assert!(dec.expires_at.is_none());
        assert!(dec.version_range.is_none());
    }

    // -----------------------------------------------------------------------
    // PermissionsFile serde tests
    // -----------------------------------------------------------------------

    #[test]
    fn permissions_file_default_version() {
        let file = PermissionsFile::default();
        assert_eq!(file.version, CURRENT_VERSION);
        assert!(file.decisions.is_empty());
    }

    #[test]
    fn permissions_file_serde_roundtrip() {
        let mut decisions = BTreeMap::new();
        decisions.insert(
            "ext-a".to_string(),
            vec![PersistedDecision {
                capability: "exec".to_string(),
                allow: true,
                decided_at: "2026-01-01T00:00:00Z".to_string(),
                expires_at: None,
                version_range: None,
            }],
        );
        let file = PermissionsFile {
            version: CURRENT_VERSION,
            decisions,
        };
        let json = serde_json::to_string_pretty(&file).unwrap();
        let roundtrip: PermissionsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip.version, CURRENT_VERSION);
        assert_eq!(roundtrip.decisions.len(), 1);
        assert_eq!(roundtrip.decisions["ext-a"].len(), 1);
        assert_eq!(roundtrip.decisions["ext-a"][0].capability, "exec");
    }

    // -----------------------------------------------------------------------
    // PermissionStore edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn open_corrupt_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        std::fs::write(&path, "not valid json!!!").unwrap();

        let result = PermissionStore::open(&path);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("parse"));
    }

    #[test]
    fn open_empty_json_object_returns_error() {
        // An empty object {} is missing required fields
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        std::fs::write(&path, "{}").unwrap();

        let result = PermissionStore::open(&path);
        assert!(result.is_err());
    }

    #[test]
    fn open_valid_empty_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        std::fs::write(&path, r#"{"version":1,"decisions":{}}"#).unwrap();

        let store = PermissionStore::open(&path).unwrap();
        assert!(store.list().is_empty());
    }

    #[test]
    fn open_unsupported_schema_version_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        std::fs::write(&path, r#"{"version":999,"decisions":{}}"#).unwrap();

        let result = PermissionStore::open(&path);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("Unsupported permissions file schema version"));
    }

    #[test]
    fn lookup_expired_decision_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        // Insert a decision that expired in the past
        store
            .decisions
            .entry("ext".to_string())
            .or_default()
            .insert(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2020-01-01T00:00:00Z".to_string(),
                    expires_at: Some("2020-06-01T00:00:00Z".to_string()),
                    version_range: None,
                },
            );

        assert_eq!(store.lookup("ext", "exec"), None);
    }

    #[test]
    fn lookup_future_expiry_returns_decision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store
            .decisions
            .entry("ext".to_string())
            .or_default()
            .insert(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: false,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: Some("2099-12-31T23:59:59Z".to_string()),
                    version_range: None,
                },
            );

        assert_eq!(store.lookup("ext", "exec"), Some(false));
    }

    #[test]
    fn lookup_expiry_with_timezone_offset_uses_actual_timestamp() {
        let decision = PersistedDecision {
            capability: "exec".to_string(),
            allow: true,
            decided_at: "2026-01-01T00:00:00Z".to_string(),
            expires_at: Some("2026-01-01T00:30:00+01:00".to_string()),
            version_range: None,
        };
        let now = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        assert!(
            !decision_is_active(&decision, now),
            "offset expiry should be normalized before comparison"
        );
    }

    #[test]
    fn lookup_invalid_expiry_treated_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store
            .decisions
            .entry("ext".to_string())
            .or_default()
            .insert(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: Some("not-a-timestamp".to_string()),
                    version_range: None,
                },
            );

        assert_eq!(store.lookup("ext", "exec"), None);
        let cache = store.to_cache_map();
        assert_eq!(cache.get("ext").and_then(|caps| caps.get("exec")), None);
    }

    #[test]
    fn lookup_no_expiry_returns_decision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store
            .decisions
            .entry("ext".to_string())
            .or_default()
            .insert(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: None,
                    version_range: None,
                },
            );

        assert_eq!(store.lookup("ext", "exec"), Some(true));
    }

    #[test]
    fn record_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir
            .path()
            .join("deep")
            .join("nested")
            .join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();

        assert!(path.exists());
        let store2 = PermissionStore::open(&path).unwrap();
        assert_eq!(store2.lookup("ext", "exec"), Some(true));
    }

    #[test]
    fn multiple_capabilities_per_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store.record("ext", "exec", true).unwrap();
        store.record("ext", "http", false).unwrap();
        store.record("ext", "env", true).unwrap();
        store.record("ext", "fs", false).unwrap();

        assert_eq!(store.lookup("ext", "exec"), Some(true));
        assert_eq!(store.lookup("ext", "http"), Some(false));
        assert_eq!(store.lookup("ext", "env"), Some(true));
        assert_eq!(store.lookup("ext", "fs"), Some(false));

        // All persist
        let store2 = PermissionStore::open(&path).unwrap();
        assert_eq!(store2.lookup("ext", "exec"), Some(true));
        assert_eq!(store2.lookup("ext", "http"), Some(false));
        assert_eq!(store2.lookup("ext", "env"), Some(true));
        assert_eq!(store2.lookup("ext", "fs"), Some(false));
    }

    #[test]
    fn record_with_version_stores_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store
            .record_with_version("ext", "exec", true, "^1.0.0")
            .unwrap();
        store
            .record_with_version("ext", "http", false, ">=2.0.0 <3.0.0")
            .unwrap();

        let dec_exec = store.decisions["ext"].get("exec").unwrap();
        assert_eq!(dec_exec.version_range.as_deref(), Some("^1.0.0"));
        assert!(dec_exec.allow);

        let dec_http = store.decisions["ext"].get("http").unwrap();
        assert_eq!(dec_http.version_range.as_deref(), Some(">=2.0.0 <3.0.0"));
        assert!(!dec_http.allow);
    }

    #[test]
    fn record_with_version_overwrites_previous() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store
            .record_with_version("ext", "exec", true, "^1.0.0")
            .unwrap();
        store
            .record_with_version("ext", "exec", false, "^2.0.0")
            .unwrap();

        let dec = store.decisions["ext"].get("exec").unwrap();
        assert_eq!(dec.version_range.as_deref(), Some("^2.0.0"));
        assert!(!dec.allow);
    }

    #[test]
    fn list_returns_all_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store.record("ext-a", "exec", true).unwrap();
        store.record("ext-a", "http", false).unwrap();
        store.record("ext-b", "env", true).unwrap();

        let all = store.list();
        assert_eq!(all.len(), 2);
        assert_eq!(all["ext-a"].len(), 2);
        assert_eq!(all["ext-b"].len(), 1);
    }

    #[test]
    fn revoke_nonexistent_extension_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store.record("ext", "exec", true).unwrap();
        // Revoking a non-existent extension should not fail
        store.revoke_extension("nonexistent").unwrap();

        assert_eq!(store.lookup("ext", "exec"), Some(true));
    }

    // -----------------------------------------------------------------------
    // to_cache_map additional scenarios
    // -----------------------------------------------------------------------

    #[test]
    fn to_cache_map_all_expired_removes_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        // All decisions for this extension are expired
        store
            .decisions
            .entry("ext".to_string())
            .or_default()
            .insert(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2020-01-01T00:00:00Z".to_string(),
                    expires_at: Some("2020-06-01T00:00:00Z".to_string()),
                    version_range: None,
                },
            );
        store
            .decisions
            .entry("ext".to_string())
            .or_default()
            .insert(
                "http".to_string(),
                PersistedDecision {
                    capability: "http".to_string(),
                    allow: false,
                    decided_at: "2020-01-01T00:00:00Z".to_string(),
                    expires_at: Some("2020-06-01T00:00:00Z".to_string()),
                    version_range: None,
                },
            );

        let cache = store.to_cache_map();
        // Extension should not appear at all since all entries are expired
        assert!(!cache.contains_key("ext"));
    }

    #[test]
    fn to_cache_map_no_expiry_always_included() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store
            .decisions
            .entry("ext".to_string())
            .or_default()
            .insert(
                "exec".to_string(),
                PersistedDecision {
                    capability: "exec".to_string(),
                    allow: true,
                    decided_at: "2026-01-01T00:00:00Z".to_string(),
                    expires_at: None,
                    version_range: None,
                },
            );

        let cache = store.to_cache_map();
        assert_eq!(cache.get("ext").and_then(|m| m.get("exec")), Some(&true));
    }

    #[test]
    fn to_cache_map_multiple_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store.record("ext-a", "exec", true).unwrap();
        store.record("ext-a", "http", false).unwrap();
        store.record("ext-b", "env", true).unwrap();

        let cache = store.to_cache_map();
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("ext-a").and_then(|m| m.get("exec")), Some(&true));
        assert_eq!(cache.get("ext-a").and_then(|m| m.get("http")), Some(&false));
        assert_eq!(cache.get("ext-b").and_then(|m| m.get("env")), Some(&true));
    }

    // -----------------------------------------------------------------------
    // File permissions test (Unix)
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn save_sets_file_permissions_0o600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();

        let metadata = std::fs::metadata(&path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // -----------------------------------------------------------------------
    // Disk persistence roundtrip tests
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_saves_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        // First session
        {
            let mut store = PermissionStore::open(&path).unwrap();
            store.record("ext-a", "exec", true).unwrap();
            store.record("ext-b", "http", false).unwrap();
        }

        // Second session: modify and add
        {
            let mut store = PermissionStore::open(&path).unwrap();
            store.record("ext-a", "exec", false).unwrap(); // overwrite
            store.record("ext-c", "env", true).unwrap(); // new
        }

        // Third session: verify all state
        {
            let store = PermissionStore::open(&path).unwrap();
            assert_eq!(store.lookup("ext-a", "exec"), Some(false)); // overwritten
            assert_eq!(store.lookup("ext-b", "http"), Some(false)); // unchanged
            assert_eq!(store.lookup("ext-c", "env"), Some(true)); // new
        }
    }

    #[test]
    fn save_serializes_extensions_and_capabilities_stably() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let mut store = PermissionStore::open(&path).unwrap();

        store.record("ext-b", "env", true).unwrap();
        store.record("ext-a", "http", false).unwrap();
        store.record("ext-a", "exec", true).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let ext_a = raw.find("\"ext-a\"").unwrap();
        let ext_b = raw.find("\"ext-b\"").unwrap();
        let exec = raw.find("\"capability\": \"exec\"").unwrap();
        let http = raw.find("\"capability\": \"http\"").unwrap();
        let env = raw.find("\"capability\": \"env\"").unwrap();

        assert!(
            ext_a < ext_b,
            "extension ids should serialize in sorted order"
        );
        assert!(exec < http, "capabilities should serialize in sorted order");
        assert!(
            http < env,
            "later extensions should appear after earlier ones"
        );
    }

    #[test]
    fn reset_then_record_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();
        store.reset().unwrap();
        store.record("ext", "http", false).unwrap();

        assert_eq!(store.lookup("ext", "exec"), None);
        assert_eq!(store.lookup("ext", "http"), Some(false));

        let store2 = PermissionStore::open(&path).unwrap();
        assert_eq!(store2.lookup("ext", "exec"), None);
        assert_eq!(store2.lookup("ext", "http"), Some(false));
    }

    #[test]
    fn decided_at_is_recent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");

        let mut store = PermissionStore::open(&path).unwrap();
        store.record("ext", "exec", true).unwrap();

        let dec = &store.decisions["ext"]["exec"];
        // decided_at should be a recent timestamp (year >= 2024)
        let year: u32 = dec.decided_at[0..4].parse().unwrap();
        assert!(year >= 2024);
    }

    #[test]
    fn concurrent_records_preserve_all_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permissions.json");
        let workers = 12;
        let barrier = Arc::new(Barrier::new(workers));
        let mut handles = Vec::new();

        for idx in 0..workers {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let mut store = PermissionStore::open(&path).unwrap();
                barrier.wait();
                store
                    .record(&format!("ext-{idx}"), "exec", idx % 2 == 0)
                    .unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let store = PermissionStore::open(&path).unwrap();
        for idx in 0..workers {
            assert_eq!(
                store.lookup(&format!("ext-{idx}"), "exec"),
                Some(idx % 2 == 0)
            );
        }
    }

    mod proptest_permissions {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// `days_to_ymd` produces valid month/day ranges.
            #[test]
            fn days_to_ymd_valid_ranges(days in 0..100_000u64) {
                let (y, m, d) = days_to_ymd(days);
                assert!(y >= 1970, "year {y} too small for days={days}");
                assert!((1..=12).contains(&m), "month {m} out of range");
                assert!((1..=31).contains(&d), "day {d} out of range");
            }

            /// `days_to_ymd(0)` is 1970-01-01.
            #[test]
            fn days_to_ymd_epoch(_dummy in 0..1u8) {
                let (y, m, d) = days_to_ymd(0);
                assert_eq!((y, m, d), (1970, 1, 1));
            }

            /// Consecutive days increment the day or roll the month/year.
            #[test]
            fn days_to_ymd_consecutive(days in 0..99_999u64) {
                let (y1, m1, d1) = days_to_ymd(days);
                let (y2, m2, d2) = days_to_ymd(days + 1);
                // Either same date with day+1, or month/year rollover
                if d2 == d1 + 1 && m2 == m1 && y2 == y1 {
                    // Normal day increment
                } else if d2 == 1 {
                    // Day rolled over to 1 — month or year changed
                    assert!(m2 != m1 || y2 != y1);
                } else {
                    assert!(false, "unexpected day sequence: {y1}-{m1}-{d1} -> {y2}-{m2}-{d2}");
                }
            }

            /// `now_iso8601` produces valid ISO-8601 format.
            #[test]
            fn now_iso8601_format(_dummy in 0..1u8) {
                let ts = now_iso8601();
                assert_eq!(ts.len(), 20, "expected YYYY-MM-DDThh:mm:ssZ, got {ts}");
                assert!(ts.ends_with('Z'));
                assert_eq!(&ts[4..5], "-");
                assert_eq!(&ts[7..8], "-");
                assert_eq!(&ts[10..11], "T");
                assert_eq!(&ts[13..14], ":");
                assert_eq!(&ts[16..17], ":");
            }

            /// `PersistedDecision` serde roundtrip preserves all fields.
            #[test]
            fn decision_serde_roundtrip(
                cap in "[a-z]{1,10}",
                allow in proptest::bool::ANY,
                has_expiry in proptest::bool::ANY,
                has_range in proptest::bool::ANY
            ) {
                let dec = PersistedDecision {
                    capability: cap,
                    allow,
                    decided_at: "2025-01-01T00:00:00Z".to_string(),
                    expires_at: if has_expiry { Some("2030-01-01T00:00:00Z".to_string()) } else { None },
                    version_range: if has_range { Some(">=1.0.0".to_string()) } else { None },
                };
                let json = serde_json::to_string(&dec).unwrap();
                let back: PersistedDecision = serde_json::from_str(&json).unwrap();
                assert_eq!(dec, back);
            }

            /// Record then lookup returns the correct allow/deny value.
            #[test]
            fn record_lookup_roundtrip(
                ext_id in "[a-z]{1,8}",
                cap in "[a-z]{1,8}",
                allow in proptest::bool::ANY
            ) {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("perm.json");
                let mut store = PermissionStore::open(&path).unwrap();
                store.record(&ext_id, &cap, allow).unwrap();
                assert_eq!(store.lookup(&ext_id, &cap), Some(allow));
            }

            /// Lookup for unknown extension returns None.
            #[test]
            fn lookup_unknown_extension(ext in "[a-z]{1,10}", cap in "[a-z]{1,5}") {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("perm.json");
                let store = PermissionStore::open(&path).unwrap();
                assert_eq!(store.lookup(&ext, &cap), None);
            }

            /// Record overwrites previous decision for same (ext, cap).
            #[test]
            fn record_overwrites(ext in "[a-z]{1,8}", cap in "[a-z]{1,8}") {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("perm.json");
                let mut store = PermissionStore::open(&path).unwrap();
                store.record(&ext, &cap, true).unwrap();
                assert_eq!(store.lookup(&ext, &cap), Some(true));
                store.record(&ext, &cap, false).unwrap();
                assert_eq!(store.lookup(&ext, &cap), Some(false));
            }

            /// Revoke removes all decisions for an extension.
            #[test]
            fn revoke_removes_all(ext in "[a-z]{1,8}", cap1 in "[a-z]{1,5}", cap2 in "[a-z]{1,5}") {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("perm.json");
                let mut store = PermissionStore::open(&path).unwrap();
                store.record(&ext, &cap1, true).unwrap();
                store.record(&ext, &cap2, false).unwrap();
                store.revoke_extension(&ext).unwrap();
                assert_eq!(store.lookup(&ext, &cap1), None);
                assert_eq!(store.lookup(&ext, &cap2), None);
            }

            /// Days 365 is in 1971 (non-leap year 1970).
            #[test]
            fn days_to_ymd_year_boundary(_dummy in 0..1u8) {
                let (y, m, d) = days_to_ymd(365);
                assert_eq!(y, 1971);
                assert_eq!(m, 1);
                assert_eq!(d, 1);
            }

            /// Leap day 2000 (day 10957 from epoch) is Feb 29.
            #[test]
            fn days_to_ymd_leap_day_2000(_dummy in 0..1u8) {
                // 2000-02-29 is day 11016 from epoch
                // 1970-01-01 + 11016 days
                let (y, m, d) = days_to_ymd(11016);
                assert_eq!((y, m, d), (2000, 2, 29));
            }
        }
    }
}
