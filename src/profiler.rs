//! Sampling profiler front-end (`--profile` / `PI_PROFILE=1`, bd-cv653.7.12.1).
//!
//! Wraps the `pprof` crate behind the opt-in `profiler` feature. Samples at
//! 99 Hz while the session runs, periodically snapshots **folded** stacks
//! into `<agent-dir>/profiles/current.folded` (atomic overwrite) so even
//! hard exits leave the last window on disk, and supports an SVG
//! flamegraph render.
//!
//! Known limitation (inherited from pprof): samples land on the thread that
//! started the profiler — the async runtime's worker threads are captured
//! only insofar as they run on that thread. Documented, acceptable for v1
//! hotspot triage.

use std::path::{Path, PathBuf};

/// Sample rate for the session profiler.
pub const SAMPLE_HZ: i32 = 99;
/// How often a folded-stack snapshot hits disk during a run.
const SNAPSHOT_INTERVAL_SECS: u64 = 10;

#[cfg(feature = "profiler")]
mod imp {
    use super::*;
    use std::sync::Mutex;

    static GUARD: Mutex<Option<pprof::ProfilerGuard<'static>>> = Mutex::new(None);

    /// Start sampling on the calling thread.
    pub fn start() -> Result<(), String> {
        let mut slot = GUARD.lock().map_err(|_| "profiler lock poisoned")?;
        if slot.is_some() {
            return Ok(());
        }
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(SAMPLE_HZ)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .map_err(|e| format!("profiler start failed: {e}"))?;
        *slot = Some(guard);
        Ok(())
    }

    /// Folded-stack snapshot of everything sampled so far.
    pub fn folded_snapshot() -> Result<Vec<String>, String> {
        let mut slot = GUARD.lock().map_err(|_| "profiler lock poisoned")?;
        let Some(guard) = slot.as_ref() else {
            return Err("profiler not running".into());
        };
        let report = guard.report().build().map_err(|e| format!("{e}"))?;
        let mut lines = Vec::new();
        for (frames, count) in report.data.iter() {
            let stack = frames
                .frames
                .iter()
                .rev()
                .map(|group| {
                    group
                        .iter()
                        .map(|sym| sym.name())
                        .collect::<Vec<_>>()
                        .join("[inlined]")
                })
                .collect::<Vec<_>>()
                .join(";");
            lines.push(format!("{stack} {count}"));
        }
        Ok(lines)
    }

    /// Stop sampling and drop the guard.
    pub fn stop() {
        if let Ok(mut slot) = GUARD.lock() {
            *slot = None;
        }
    }
}

#[cfg(not(feature = "profiler"))]
mod imp {
    pub fn start() -> Result<(), String> {
        Err("built without the `profiler` feature".into())
    }
    pub fn folded_snapshot() -> Result<Vec<String>, String> {
        Err("built without the `profiler` feature".into())
    }
    pub const fn stop() {}
}

pub use imp::{folded_snapshot, start, stop};

/// Directory where periodic snapshots land.
pub fn profiles_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("profiles")
}

/// Write a folded snapshot under `agent_dir/profiles/`. Returns the path.
///
/// # Errors
/// Snapshot or write failures surface verbatim.
pub fn write_snapshot(agent_dir: &Path) -> Result<PathBuf, String> {
    let dir = profiles_dir(agent_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir profiles: {e}"))?;
    let lines = folded_snapshot()?;
    // Samples are cumulative-since-start: one stamped file per interval
    // accumulates O(n^2) bytes over a long session (and contradicted the
    // documented "overwrites current.folded" behavior). Overwrite the one
    // documented target atomically (temp + rename).
    let path = dir.join("current.folded");
    let tmp = dir.join(".current.folded.tmp");
    std::fs::write(&tmp, lines.join("\n")).map_err(|e| format!("write snapshot: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("persist snapshot: {e}"))?;
    Ok(path)
}

/// Spawn the periodic-snapshot thread.
///
/// Every [`SNAPSHOT_INTERVAL_SECS`] the current folded state overwrites
/// `profiles/current.folded`, so even a hard exit leaves the last window
/// on disk. A no-op without the feature.
pub fn spawn_snapshot_thread(agent_dir: &Path) {
    let dir = agent_dir.to_path_buf();
    std::thread::Builder::new()
        .name("pi-profile-snap".into())
        .spawn(move || {
            let mut warned = false;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(SNAPSHOT_INTERVAL_SECS));
                match write_snapshot(&dir) {
                    Ok(path) => {
                        tracing::debug!(event = "pi.profile.snapshot", path = %path.display());
                    }
                    Err(err) if !warned => {
                        // A full disk / unwritable dir must not silently
                        // produce an empty run.
                        warned = true;
                        tracing::warn!(event = "pi.profile.snapshot", error = %err);
                    }
                    Err(_) => {}
                }
            }
        })
        .ok();
}

/// Aggregate a `.folded` file into `(total_samples, top rows)` sorted by
/// inclusive sample count. Each line is `frame;frame;… count`.
#[must_use]
pub fn top_from_folded(content: &str, top_n: usize) -> (u64, Vec<(String, u64)>) {
    let mut totals: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    let mut grand = 0u64;
    for line in content.lines() {
        let Some((stack, count)) = line.rsplit_once(' ') else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        grand += count;
        // Attribute the sample to every prefix frame (inclusive totals).
        let frames: Vec<&str> = stack.split(';').collect();
        for depth in 1..=frames.len() {
            let prefix = frames[..depth].join(";");
            *totals.entry(prefix).or_insert(0) += count;
        }
    }
    let mut rows: Vec<(String, u64)> = totals.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.truncate(top_n);
    (grand, rows)
}

#[cfg(feature = "profiler")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_snapshot_stop_roundtrip() {
        start().expect("start");
        // Busy-spin briefly so the sampler has something to see on this
        // thread.
        let mut sink = 0u64;
        for i in 0..20_000_000u64 {
            sink = sink.wrapping_add(i);
        }
        assert_ne!(sink, u64::MAX.saturating_sub(1));
        let lines = folded_snapshot().expect("snapshot");
        assert!(!lines.is_empty(), "expected sampled frames");
        stop();
    }

    #[test]
    fn top_from_folded_aggregates_prefixes() {
        let content = "a;b;c 2\na;b 1\nx 3\n";
        let (grand, rows) = top_from_folded(content, 10);
        assert_eq!(grand, 6);
        assert_eq!(rows[0], ("a".to_string(), 3));
        assert!(rows.contains(&("a;b".to_string(), 3)));
        assert!(rows.contains(&("x".to_string(), 3)));
    }
}
