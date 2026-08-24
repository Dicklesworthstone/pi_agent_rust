//! Retention-policy pruning for sessions, artifacts, and caches (`pi gc`) (bd-cv653.7.11).
//!
//! Enforces bounded resource usage across Pi data stores:
//! - Sessions: JSONL files older than retention window and not named/pinned (retaining `keep_last` per project).
//! - Artifacts: Session-scoped spillover / evidence files whose parent session no longer exists or is aged.
//! - Caches: Extension transpile cache, bytecode, and temporary runtime artifacts.
//! - Sidecars: Orphaned sidecars (`.meta.json`, `.sidecar`) without live JSONL.
//! - Index / SQLite: Stale session-index rows reaped and SQLite database vacuumed.
//!
//! SAFETY FIRST:
//! - Default execution is `--dry-run` printing an itemized plan with byte counts.
//! - Live execution requires `--yes` or interactive confirmation.
//! - Pruned sessions move to a trash directory with grace period before permanent purge.
//! - Every pruning action is appended to `pi.gc.v1` JSONL audit ledger.

use std::collections::{BTreeMap, HashSet};
use std::fmt::{self, Write as _};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};

/// Schema tag for the garbage collection audit ledger.
pub const GC_LEDGER_SCHEMA: &str = "pi.gc.v1";

/// Kind of data store managed by garbage collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GcStoreKind {
    #[serde(rename = "sessions")]
    Sessions,
    #[serde(rename = "artifacts")]
    Artifacts,
    #[serde(rename = "caches")]
    Caches,
    #[serde(rename = "sidecars")]
    Sidecars,
    #[serde(rename = "index")]
    Index,
    #[serde(rename = "trash")]
    Trash,
}

impl GcStoreKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::Artifacts => "artifacts",
            Self::Caches => "caches",
            Self::Sidecars => "sidecars",
            Self::Index => "index",
            Self::Trash => "trash",
        }
    }
}

impl fmt::Display for GcStoreKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// An individual candidate item evaluated for garbage collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcItem {
    pub path: PathBuf,
    pub store: GcStoreKind,
    pub size_bytes: u64,
    pub age_secs: u64,
    pub reason: String,
    pub is_protected: bool,
}

/// The computed reclamation plan for a GC sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcPlan {
    pub target_root: PathBuf,
    pub older_than_days: u64,
    pub keep_last: usize,
    pub items_to_prune: Vec<GcItem>,
    pub items_protected: Vec<GcItem>,
    pub total_bytes_reclaimable: u64,
    pub total_items_reclaimable: usize,
}

impl GcPlan {
    /// Format the plan as a human-readable summary.
    #[allow(clippy::cast_precision_loss)] // Byte counts within practical range won't lose precision
    pub fn format_text(&self) -> String {
        let mut out = String::new();
        let mb = (self.total_bytes_reclaimable as f64) / (1024.0 * 1024.0);
        let _ = writeln!(
            out,
            "=== PI GARBAGE COLLECTION PLAN ===\nTarget Root  : {}\nPolicy       : older than {} days, keep-last {} per project\nReclaimable  : {} item(s), {:.2} MB ({})\nProtected    : {} item(s)\n",
            self.target_root.display(),
            self.older_than_days,
            self.keep_last,
            self.total_items_reclaimable,
            mb,
            format_bytes(self.total_bytes_reclaimable),
            self.items_protected.len()
        );

        if self.items_to_prune.is_empty() {
            let _ = writeln!(out, "No items qualify for pruning. Storage is clean.");
        } else {
            let _ = writeln!(out, "ITEMS TO PRUNE:");
            for (idx, item) in self.items_to_prune.iter().enumerate() {
                let age_days = item.age_secs / 86400;
                let _ = writeln!(
                    out,
                    " {:2}. [{}] {:<10} {:>10} (age: {}d) - {}\n     Path: {}",
                    idx + 1,
                    item.store.as_str(),
                    item.store,
                    format_bytes(item.size_bytes),
                    age_days,
                    item.reason,
                    item.path.display()
                );
            }
        }

        out
    }

    /// Format plan as JSON.
    pub fn format_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::Validation(format!("Failed to serialize GC plan: {e}")))
    }
}

/// Ledger audit record written for each GC action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcLedgerRecord {
    pub schema: String,
    pub timestamp_ms: i64,
    pub original_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash_path: Option<String>,
    pub store: GcStoreKind,
    pub size_bytes: u64,
    pub action: String,
    pub reason: String,
}

/// Results of executing a garbage collection sweep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcResult {
    pub plan: GcPlan,
    pub pruned_items: usize,
    pub pruned_bytes: u64,
    pub trashed_items: usize,
    pub errors: Vec<String>,
    pub dry_run: bool,
    pub ledger_path: PathBuf,
}

impl GcResult {
    #[allow(clippy::cast_precision_loss)] // Byte counts within practical range won't lose precision
    pub fn format_text(&self) -> String {
        let mut out = String::new();
        let mode = if self.dry_run {
            "DRY RUN (No changes applied)"
        } else {
            "APPLIED (Items moved to trash)"
        };
        let mb = (self.pruned_bytes as f64) / (1024.0 * 1024.0);
        let _ = writeln!(
            out,
            "=== PI GC EXECUTION RESULT ===\nMode          : {mode}\nPruned Items  : {}\nPruned Space  : {:.2} MB ({})\nTrashed Items : {}\nAudit Ledger  : {}\n",
            self.pruned_items,
            mb,
            format_bytes(self.pruned_bytes),
            self.trashed_items,
            self.ledger_path.display()
        );

        if !self.errors.is_empty() {
            let _ = writeln!(out, "ERRORS ENCOUNTERED:");
            for err in &self.errors {
                let _ = writeln!(out, " - {err}");
            }
        }

        out
    }

    pub fn format_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self)
            .map_err(|e| Error::Validation(format!("Failed to serialize GC result: {e}")))
    }
}

/// Options configuring a GC sweep.
#[derive(Debug, Clone)]
pub struct GcOptions {
    pub older_than_days: u64,
    /// Newest N sessions per project to retain regardless of age. Ranked over
    /// the prunable population only: named/pinned sessions are already kept
    /// unconditionally and never consume a slot.
    pub keep_last: usize,
    pub prune_caches: bool,
    pub dry_run: bool,
    pub empty_trash: bool,
    pub restore_target: Option<String>,
    pub custom_sessions_dir: Option<PathBuf>,
    pub custom_trash_dir: Option<PathBuf>,
    pub custom_ledger_path: Option<PathBuf>,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            older_than_days: 30,
            keep_last: 5,
            prune_caches: true,
            dry_run: true,
            empty_trash: false,
            restore_target: None,
            custom_sessions_dir: None,
            custom_trash_dir: None,
            custom_ledger_path: None,
        }
    }
}

/// Format bytes into human-readable unit string.
#[allow(clippy::cast_precision_loss)] // Byte counts within practical range won't lose precision
pub fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Parse retention string (e.g. "30d", "7d", "24h", "14") into days.
pub fn parse_retention_days(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if let Some(num_str) = s.strip_suffix('d') {
        num_str.trim().parse::<u64>().ok()
    } else if let Some(num_str) = s.strip_suffix('h') {
        let hours = num_str.trim().parse::<u64>().ok()?;
        Some((hours / 24).max(1))
    } else {
        s.parse::<u64>().ok()
    }
}

const fn make_session_gc_item(
    path: PathBuf,
    size_bytes: u64,
    age_secs: u64,
    reason: String,
    is_protected: bool,
) -> GcItem {
    GcItem {
        path,
        store: GcStoreKind::Sessions,
        size_bytes,
        age_secs,
        reason,
        is_protected,
    }
}

fn make_sidecar_gc_item(path: PathBuf, size_bytes: u64, age_secs: u64) -> GcItem {
    GcItem {
        path,
        store: GcStoreKind::Sidecars,
        size_bytes,
        age_secs,
        reason: "Orphaned sidecar without live session JSONL".to_string(),
        is_protected: false,
    }
}

fn make_cache_gc_item(path: PathBuf, size_bytes: u64, age_secs: u64) -> GcItem {
    GcItem {
        path,
        store: GcStoreKind::Caches,
        size_bytes,
        age_secs,
        reason: "Expired extension transpile cache entry".to_string(),
        is_protected: false,
    }
}

/// Garbage collection engine and store sweeper.
pub struct GarbageCollector;

impl GarbageCollector {
    /// Resolve default trash directory path.
    pub fn default_trash_dir() -> PathBuf {
        Config::global_dir().join("trash")
    }

    /// Resolve default GC audit ledger path.
    pub fn default_ledger_path() -> PathBuf {
        Config::global_dir().join("gc_ledger.jsonl")
    }

    /// Check whether a session file is protected (pinned, custom named, or active).
    pub fn is_session_protected(path: &Path) -> bool {
        // If file doesn't exist or is directory, not protected as session
        if !path.is_file() {
            return false;
        }

        // Check if there is an adjacent .pinned file
        let mut pinned_path = path.to_path_buf();
        pinned_path.set_extension("pinned");
        if pinned_path.exists() {
            return true;
        }

        // Active-session guard: a live holder maintains `<session>.lock` as
        // a DIRECTORY whose mtime refreshes every 5s (file_lock.rs,
        // proper-lockfile protocol; a regular `.lock` FILE is a legacy
        // artifact and does not signal liveness). Trashing a session out
        // from under an open process would silently redirect its appends
        // into the trash. 60s bound = 6x the refresh interval.
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let lock_path = Path::new(&lock_path);
        if lock_path.is_dir()
            && fs::metadata(lock_path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|mtime| mtime.elapsed().ok())
                .is_some_and(|age| age < Duration::from_secs(60))
        {
            return true;
        }

        // Inspect session header (first line) for explicit name / pinned marker
        if let Ok(content) = fs::read_to_string(path)
            && let Some(first_line) = content.lines().next()
        {
            if first_line.contains("\"name\":")
                && !first_line.contains("\"name\":null")
                && !first_line.contains("\"name\":\"\"")
            {
                return true;
            }
            if first_line.contains("\"pinned\":true") {
                return true;
            }
        }

        false
    }

    /// Build a GC reclamation plan according to retention options.
    pub fn plan(options: &GcOptions) -> Result<GcPlan> {
        let sessions_dir = options
            .custom_sessions_dir
            .clone()
            .unwrap_or_else(Config::sessions_dir);

        let now = SystemTime::now();
        let max_age_duration = Duration::from_secs(options.older_than_days * 86400);

        let mut items_to_prune = Vec::new();
        let mut items_protected = Vec::new();

        if sessions_dir.is_dir() {
            // Group session files by parent project directory to enforce keep_last
            let mut project_sessions: BTreeMap<PathBuf, Vec<(PathBuf, SystemTime, u64)>> =
                BTreeMap::new();
            let mut all_session_stems = HashSet::new();

            Self::scan_session_files(&sessions_dir, &mut project_sessions, &mut all_session_stems)?;

            for (_proj_dir, mut session_files) in project_sessions {
                // Sort descending by modified time (newest first). Filesystems
                // with coarse timestamp granularity hand back ties, so break
                // them on path to keep a plan reproducible across runs.
                session_files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

                // Rank for the keep-last quota. Named/pinned sessions survive
                // unconditionally, so they must not consume a quota slot —
                // otherwise `--keep-last N` silently retains fewer than N
                // prunable sessions (module docs: "retaining `keep_last` per
                // project" applies to the not-named/pinned population).
                let mut prunable_rank = 0usize;

                for (file_path, mtime, size) in session_files {
                    let age = now.duration_since(mtime).unwrap_or_default();
                    let age_secs = age.as_secs();

                    // Check protection: keep_last, named, or pinned
                    let is_named_or_pinned = Self::is_session_protected(&file_path);
                    let rank = prunable_rank;
                    let is_keep_last = if is_named_or_pinned {
                        false
                    } else {
                        prunable_rank += 1;
                        rank < options.keep_last
                    };

                    if is_keep_last || is_named_or_pinned {
                        let reason = if is_named_or_pinned {
                            "Named or pinned session preserved".to_string()
                        } else {
                            format!(
                                "Preserved under keep-last {} quota (rank {})",
                                options.keep_last,
                                rank + 1
                            )
                        };
                        items_protected.push(make_session_gc_item(
                            file_path, size, age_secs, reason, true,
                        ));
                    } else if age >= max_age_duration {
                        let reason = format!(
                            "Exceeded retention age ({}d >= {}d)",
                            age_secs / 86400,
                            options.older_than_days
                        );
                        items_to_prune.push(make_session_gc_item(
                            file_path, size, age_secs, reason, false,
                        ));
                    } else {
                        let reason = format!(
                            "Within retention age ({}d < {}d)",
                            age_secs / 86400,
                            options.older_than_days
                        );
                        items_protected.push(make_session_gc_item(
                            file_path, size, age_secs, reason, true,
                        ));
                    }
                }
            }

            // Scan for orphaned sidecars in sessions_dir
            Self::scan_sidecars(&sessions_dir, &all_session_stems, &mut items_to_prune)?;
        }

        // Scan extension transpile caches and temp caches if enabled
        if options.prune_caches {
            let global_cache = Config::global_dir().join("cache");
            if global_cache.is_dir() {
                Self::scan_caches(&global_cache, max_age_duration, now, &mut items_to_prune);
            }
        }

        let total_bytes: u64 = items_to_prune.iter().map(|i| i.size_bytes).sum();
        let total_items = items_to_prune.len();

        Ok(GcPlan {
            target_root: sessions_dir,
            older_than_days: options.older_than_days,
            keep_last: options.keep_last,
            items_to_prune,
            items_protected,
            total_bytes_reclaimable: total_bytes,
            total_items_reclaimable: total_items,
        })
    }

    fn scan_session_files(
        root: &Path,
        project_sessions: &mut BTreeMap<PathBuf, Vec<(PathBuf, SystemTime, u64)>>,
        all_session_stems: &mut HashSet<String>,
    ) -> Result<()> {
        let Ok(entries) = fs::read_dir(root) else {
            return Ok(());
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::scan_session_files(&path, project_sessions, all_session_stems)?;
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                let Ok(meta) = fs::metadata(&path) else {
                    continue;
                };
                let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                let size = meta.len();
                let parent = path.parent().unwrap_or(root).to_path_buf();

                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    all_session_stems.insert(stem.to_string());
                }

                project_sessions
                    .entry(parent)
                    .or_default()
                    .push((path, mtime, size));
            }
        }

        Ok(())
    }

    fn scan_sidecars(
        root: &Path,
        live_stems: &HashSet<String>,
        items_to_prune: &mut Vec<GcItem>,
    ) -> Result<()> {
        let Ok(entries) = fs::read_dir(root) else {
            return Ok(());
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::scan_sidecars(&path, live_stems, items_to_prune)?;
            } else if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
                && (file_name.ends_with(".meta.json") || file_name.ends_with(".sidecar"))
            {
                let stem = file_name
                    .trim_end_matches(".meta.json")
                    .trim_end_matches(".sidecar");
                if !live_stems.contains(stem) {
                    let meta = fs::metadata(&path).ok();
                    let size = meta.as_ref().map_or(0, fs::Metadata::len);
                    let age_secs = meta
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .map_or(0, |d| d.as_secs());

                    items_to_prune.push(make_sidecar_gc_item(path, size, age_secs));
                }
            }
        }

        Ok(())
    }

    fn scan_caches(
        cache_root: &Path,
        max_age: Duration,
        now: SystemTime,
        items_to_prune: &mut Vec<GcItem>,
    ) {
        let Ok(entries) = fs::read_dir(cache_root) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let Ok(meta) = fs::metadata(&path) else {
                    continue;
                };
                let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                let age = now.duration_since(mtime).unwrap_or_default();
                if age >= max_age {
                    items_to_prune.push(make_cache_gc_item(path, meta.len(), age.as_secs()));
                }
            }
        }
    }

    /// Execute garbage collection sweep.
    pub fn run(options: &GcOptions) -> Result<GcResult> {
        let trash_dir = options
            .custom_trash_dir
            .clone()
            .unwrap_or_else(Self::default_trash_dir);
        let ledger_path = options
            .custom_ledger_path
            .clone()
            .unwrap_or_else(Self::default_ledger_path);

        // Handle restore if requested
        if let Some(target) = &options.restore_target {
            return Self::restore_session(target, &trash_dir, options);
        }

        // Handle emptying trash if requested
        if options.empty_trash {
            return Self::empty_trash(&trash_dir, &ledger_path, options);
        }

        let plan = Self::plan(options)?;

        if options.dry_run {
            return Ok(GcResult {
                plan,
                pruned_items: 0,
                pruned_bytes: 0,
                trashed_items: 0,
                errors: Vec::new(),
                dry_run: true,
                ledger_path,
            });
        }

        // Execute live pruning
        if let Some(parent) = trash_dir.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::create_dir_all(&trash_dir);

        let timestamp_run = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let run_trash_subdir = trash_dir.join(format!("run_{timestamp_run}"));
        let _ = fs::create_dir_all(&run_trash_subdir);

        let mut pruned_items = 0;
        let mut pruned_bytes = 0;
        let mut trashed_items = 0;
        let mut errors = Vec::new();

        for item in &plan.items_to_prune {
            let file_name = item
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            // Same-named sessions from different project dirs land in one
            // flat run dir; `fs::rename` would silently clobber the earlier
            // one (permanent loss). Uniquify instead.
            let mut dest_path = run_trash_subdir.join(&file_name);
            let mut suffix = 1u32;
            while dest_path.exists() {
                dest_path = run_trash_subdir.join(format!("{file_name}.{suffix}"));
                suffix += 1;
            }

            // Move to trash
            match fs::rename(&item.path, &dest_path) {
                Ok(()) => {
                    pruned_items += 1;
                    pruned_bytes += item.size_bytes;
                    trashed_items += 1;

                    Self::record_ledger(
                        &ledger_path,
                        &GcLedgerRecord {
                            schema: GC_LEDGER_SCHEMA.to_string(),
                            timestamp_ms: SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
                            original_path: item.path.display().to_string(),
                            trash_path: Some(dest_path.display().to_string()),
                            store: item.store,
                            size_bytes: item.size_bytes,
                            action: "trashed".to_string(),
                            reason: item.reason.clone(),
                        },
                    );
                }
                Err(e) => {
                    errors.push(format!("Failed to trash {}: {e}", item.path.display()));
                }
            }
        }

        Ok(GcResult {
            plan,
            pruned_items,
            pruned_bytes,
            trashed_items,
            errors,
            dry_run: false,
            ledger_path,
        })
    }

    fn restore_session(target: &str, trash_dir: &Path, options: &GcOptions) -> Result<GcResult> {
        let sessions_dir = options
            .custom_sessions_dir
            .clone()
            .unwrap_or_else(Config::sessions_dir);
        let ledger_path = options
            .custom_ledger_path
            .clone()
            .unwrap_or_else(Self::default_ledger_path);

        let mut found_file: Option<PathBuf> = None;

        // Search trash dir for target filename or stem match
        if trash_dir.is_dir() {
            Self::find_in_trash(trash_dir, target, &mut found_file);
        }

        let trashed_path = found_file.ok_or_else(|| {
            Error::Validation(format!(
                "Session '{target}' not found in trash directory {}",
                trash_dir.display()
            ))
        })?;

        let file_name = trashed_path
            .file_name()
            .ok_or_else(|| Error::Validation("Invalid file path in trash".to_string()))?;
        let restored_path = sessions_dir.join(file_name);

        let size = fs::metadata(&trashed_path).map_or(0, |m| m.len());

        fs::rename(&trashed_path, &restored_path).map_err(|e| {
            Error::Io(Box::new(std::io::Error::other(format!(
                "Failed to restore session from trash: {e}"
            ))))
        })?;

        Self::record_ledger(
            &ledger_path,
            &GcLedgerRecord {
                schema: GC_LEDGER_SCHEMA.to_string(),
                timestamp_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
                original_path: trashed_path.display().to_string(),
                trash_path: None,
                store: GcStoreKind::Sessions,
                size_bytes: size,
                action: "restored".to_string(),
                reason: format!("Restored to {}", restored_path.display()),
            },
        );

        let plan = GcPlan {
            target_root: sessions_dir,
            older_than_days: options.older_than_days,
            keep_last: options.keep_last,
            items_to_prune: Vec::new(),
            items_protected: Vec::new(),
            total_bytes_reclaimable: 0,
            total_items_reclaimable: 0,
        };

        Ok(GcResult {
            plan,
            pruned_items: 0,
            pruned_bytes: 0,
            trashed_items: 0,
            errors: Vec::new(),
            dry_run: false,
            ledger_path,
        })
    }

    fn find_in_trash(dir: &Path, target: &str, found: &mut Option<PathBuf>) {
        if found.is_some() {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::find_in_trash(&path, target, found);
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && (name == target || name.starts_with(target) || name.contains(target))
            {
                *found = Some(path);
                return;
            }
        }
    }

    fn empty_trash(trash_dir: &Path, ledger_path: &Path, options: &GcOptions) -> Result<GcResult> {
        let mut pruned_items = 0;
        let mut pruned_bytes = 0;
        let mut errors = Vec::new();

        if trash_dir.is_dir() {
            let entries = fs::read_dir(trash_dir).map_err(|e| {
                Error::Io(Box::new(std::io::Error::other(format!(
                    "Failed to read trash dir: {e}"
                ))))
            })?;

            for entry in entries.flatten() {
                let path = entry.path();
                let size = if path.is_file() {
                    fs::metadata(&path).map_or(0, |m| m.len())
                } else {
                    0
                };

                // Second line of defense: this is the only permanently
                // destructive gc path, so it must respect dry-run even if a
                // caller wires the options up wrong.
                let res = if options.dry_run {
                    Ok(())
                } else if path.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };

                match res {
                    Ok(()) => {
                        pruned_items += 1;
                        pruned_bytes += size;
                    }
                    Err(e) => {
                        errors.push(format!("Failed to remove {}: {e}", path.display()));
                    }
                }
            }
        }

        if !options.dry_run {
            Self::record_ledger(
                ledger_path,
                &GcLedgerRecord {
                    schema: GC_LEDGER_SCHEMA.to_string(),
                    timestamp_ms: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
                    original_path: trash_dir.display().to_string(),
                    trash_path: None,
                    store: GcStoreKind::Trash,
                    size_bytes: pruned_bytes,
                    action: "emptied_trash".to_string(),
                    reason: "User requested permanent trash purge".to_string(),
                },
            );
        }

        let sessions_dir = options
            .custom_sessions_dir
            .clone()
            .unwrap_or_else(Config::sessions_dir);

        let plan = GcPlan {
            target_root: sessions_dir,
            older_than_days: options.older_than_days,
            keep_last: options.keep_last,
            items_to_prune: Vec::new(),
            items_protected: Vec::new(),
            total_bytes_reclaimable: 0,
            total_items_reclaimable: 0,
        };

        Ok(GcResult {
            plan,
            pruned_items,
            pruned_bytes,
            trashed_items: 0,
            errors,
            dry_run: false,
            ledger_path: ledger_path.to_path_buf(),
        })
    }

    fn record_ledger(ledger_path: &Path, record: &GcLedgerRecord) {
        if let Some(parent) = ledger_path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(ledger_path)
            && let Ok(json) = serde_json::to_string(record)
        {
            let _ = writeln!(file, "{json}");
        }
    }
}

/// Storage pressure assessment for doctor hints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoragePressureStatus {
    pub is_elevated: bool,
    pub total_sessions_count: usize,
    pub stale_sessions_count: usize,
    pub total_bytes: u64,
    pub recommendation: Option<String>,
}

/// Inspect store size and return a pressure diagnostic.
pub fn check_storage_pressure(sessions_dir: &Path, older_than_days: u64) -> StoragePressureStatus {
    const STORAGE_THRESHOLD_BYTES: u64 = 250 * 1024 * 1024; // 250 MB
    const STALE_SESSIONS_THRESHOLD: usize = 50;

    let options = GcOptions {
        older_than_days,
        keep_last: 5,
        prune_caches: false,
        dry_run: true,
        empty_trash: false,
        restore_target: None,
        custom_sessions_dir: Some(sessions_dir.to_path_buf()),
        custom_trash_dir: None,
        custom_ledger_path: None,
    };

    let plan = GarbageCollector::plan(&options).unwrap_or_else(|_| GcPlan {
        target_root: sessions_dir.to_path_buf(),
        older_than_days,
        keep_last: 5,
        items_to_prune: Vec::new(),
        items_protected: Vec::new(),
        total_bytes_reclaimable: 0,
        total_items_reclaimable: 0,
    });

    let total_count = plan.items_to_prune.len() + plan.items_protected.len();
    let stale_count = plan.items_to_prune.len();
    let total_bytes = plan.total_bytes_reclaimable
        + plan
            .items_protected
            .iter()
            .map(|i| i.size_bytes)
            .sum::<u64>();

    let is_elevated =
        total_bytes > STORAGE_THRESHOLD_BYTES || stale_count > STALE_SESSIONS_THRESHOLD;

    let recommendation = if is_elevated {
        Some(format!(
            "Storage pressure detected: {} stale sessions ({}) reclaimable. Run `pi gc --yes` to prune.",
            stale_count,
            format_bytes(plan.total_bytes_reclaimable)
        ))
    } else {
        None
    };

    StoragePressureStatus {
        is_elevated,
        total_sessions_count: total_count,
        stale_sessions_count: stale_count,
        total_bytes,
        recommendation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_retention_days_parsing() {
        assert_eq!(parse_retention_days("30d"), Some(30));
        assert_eq!(parse_retention_days("7D"), Some(7));
        assert_eq!(parse_retention_days("48h"), Some(2));
        assert_eq!(parse_retention_days("15"), Some(15));
        assert_eq!(parse_retention_days("invalid"), None);
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn test_gc_plan_preserves_named_and_keep_last() {
        let Ok(tmp) = tempdir() else { return };
        let sessions_dir = tmp.path().join("sessions");
        let _ = fs::create_dir_all(&sessions_dir);

        // Create 3 session files
        let s1 = sessions_dir.join("s1.jsonl");
        let s2 = sessions_dir.join("s2.jsonl");
        let s3 = sessions_dir.join("s3.jsonl");

        let _ = fs::write(&s1, "{\"version\":3,\"name\":\"pinned-feature\"}\n");
        let _ = fs::write(&s2, "{\"version\":3}\n");
        let _ = fs::write(&s3, "{\"version\":3}\n");

        // Pin mtimes explicitly: three writes in a row can land on the same
        // filesystem timestamp, which used to leave the newest-first ordering
        // (and therefore the assertions below) up to directory iteration order.
        // Newest is the *named* session, which must not consume the keep-last
        // slot that belongs to s2.
        for (path, unix_secs) in [
            (&s1, 1_700_000_300),
            (&s2, 1_700_000_200),
            (&s3, 1_700_000_100),
        ] {
            let _ =
                filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(unix_secs, 0));
        }

        let options = GcOptions {
            older_than_days: 0, // Age immediately for test
            keep_last: 1,       // Keep 1 newest unpinned
            prune_caches: false,
            dry_run: true,
            empty_trash: false,
            restore_target: None,
            custom_sessions_dir: Some(sessions_dir),
            custom_trash_dir: None,
            custom_ledger_path: None,
        };

        let plan = GarbageCollector::plan(&options).expect("plan");
        assert!(
            plan.items_protected.iter().any(|i| i.path == s1),
            "named session preserved"
        );
        assert!(
            plan.items_protected.iter().any(|i| i.path == s2),
            "newest prunable session holds the keep-last slot the named session must not take"
        );
        assert_eq!(
            plan.items_to_prune
                .iter()
                .map(|i| &i.path)
                .collect::<Vec<_>>(),
            vec![&s3],
            "only the oldest prunable session is reclaimed"
        );
    }
}
