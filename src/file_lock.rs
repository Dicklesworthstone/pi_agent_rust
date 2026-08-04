//! Cross-implementation file locking compatible with Node `proper-lockfile`.
//!
//! Upstream TS pi (`@earendil-works/pi-coding-agent`) locks the shared files under
//! `~/.pi/agent/` (`auth.json`, `settings.json`, `sessions/session-index`) with
//! [`proper-lockfile`](https://www.npmjs.com/package/proper-lockfile) `4.1.2`.
//! That protocol represents a held lock as a **directory** created atomically with
//! `mkdir(2)` at `<target>.lock`; existence means "held", release is `rmdir(2)`,
//! and a lock whose directory mtime is older than a staleness threshold may be
//! reclaimed (`rmdir` + re-`mkdir`).
//!
//! pi_agent_rust historically used `flock(2)` (via `fs4`) on a persistent, never-
//! deleted **regular file** at the same `<target>.lock` path. That is mutually
//! incompatible with proper-lockfile in both directions:
//!
//! * proper-lockfile's `mkdir` sees the leftover regular file and returns `EEXIST`;
//!   its stale-reclaim then calls `rmdir` on that regular file and fails with
//!   `ENOTDIR`, permanently poisoning the lock path (upstream issue
//!   earendil-works/pi#1871).
//! * a rust `open(O_CREAT)` against the directory proper-lockfile creates fails
//!   with `EISDIR`.
//!
//! This module makes pi_agent_rust speak proper-lockfile's directory protocol so
//! the two implementations mutually exclude correctly, can reclaim each other's
//! stale locks, and never leave a poisoning regular file behind. When it
//! encounters a stale leftover regular file (from an older pi_agent_rust build) it
//! removes it, healing the poisoning for the TS side as well.
//!
//! Constants mirror proper-lockfile's defaults: `stale = 10_000ms` and
//! `update = stale / 2`. Refreshing the lock-directory mtime is required even for
//! usually-short critical sections: a delayed writer must never become stealable
//! merely because scheduling or filesystem I/O exceeded the stale threshold.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

/// proper-lockfile default `stale` threshold. A lock directory whose mtime is
/// older than this is considered abandoned and may be reclaimed.
const STALE: Duration = Duration::from_secs(10);

/// proper-lockfile's default refresh interval (`stale / 2`).
const UPDATE: Duration = Duration::from_secs(5);

/// ENOTDIR raw errno (a component of the path — here the lock path itself — is a
/// regular file). `io::ErrorKind::NotADirectory` is unstable, so match the errno.
#[cfg(unix)]
const ENOTDIR: i32 = 20;

/// Compute the proper-lockfile lock-directory path for `target`: `<target>.lock`.
/// Mirrors proper-lockfile's `getLockFile` (`${file}.lock`).
pub fn lock_path_for(target: &Path) -> PathBuf {
    let mut p = target.as_os_str().to_os_string();
    p.push(".lock");
    PathBuf::from(p)
}

/// True when `meta`'s mtime is older than the stale threshold.
///
/// Mirrors proper-lockfile's `isLockStale`: `stat.mtime < Date.now() - stale`.
/// A future mtime (clock skew) or an unreadable mtime is treated as *fresh*
/// (i.e. held) so we never steal a lock we cannot prove is abandoned.
fn is_stale(meta: &fs::Metadata, stale: Duration) -> bool {
    meta.modified().is_ok_and(|mtime| {
        SystemTime::now()
            .duration_since(mtime)
            .is_ok_and(|age| age > stale)
    })
}

/// Remove whatever occupies the lock path so acquisition can retry.
///
/// A directory is removed with `rmdir` (matching proper-lockfile). A regular
/// file or symlink is a legacy `flock` poisoning artifact from an older
/// pi_agent_rust build (proper-lockfile never creates one); remove it too so the
/// path stops poisoning the TS side. Errors are ignored: a concurrent acquirer
/// may have already removed it, and the subsequent `mkdir` is the real arbiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LockIdentity {
    modified: SystemTime,
    is_dir: bool,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn lock_identity(meta: &fs::Metadata) -> io::Result<LockIdentity> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;

    Ok(LockIdentity {
        modified: meta.modified()?,
        is_dir: meta.is_dir(),
        #[cfg(unix)]
        device: meta.dev(),
        #[cfg(unix)]
        inode: meta.ino(),
    })
}

fn reclaim_if_unchanged(lock_path: &Path, observed: &fs::Metadata) {
    let Ok(current) = fs::symlink_metadata(lock_path) else {
        return;
    };
    let (Ok(current_identity), Ok(observed_identity)) =
        (lock_identity(&current), lock_identity(observed))
    else {
        return;
    };
    if current_identity != observed_identity {
        return;
    }
    if current.is_dir() {
        let _ = fs::remove_dir(lock_path);
    } else {
        let _ = fs::remove_file(lock_path);
    }
}

fn remove_owned_dir(lock_path: &Path, expected: LockIdentity) {
    let still_owned = fs::symlink_metadata(lock_path)
        .and_then(|meta| lock_identity(&meta))
        .is_ok_and(|identity| identity == expected);
    if still_owned {
        let _ = fs::remove_dir(lock_path);
    }
}

fn refresh_identity(lock_path: &Path) -> io::Result<LockIdentity> {
    filetime::set_file_mtime(lock_path, filetime::FileTime::now())?;
    lock_identity(&fs::symlink_metadata(lock_path)?)
}

/// Exponential backoff with light jitter, capped, mirroring the previous
/// `fs4`-based retry loops in this crate.
fn backoff(attempt: u32) -> Duration {
    let base_ms: u64 = 10;
    let cap_ms: u64 = 500;
    let sleep_ms = base_ms
        .checked_shl(attempt.min(5))
        .unwrap_or(cap_ms)
        .min(cap_ms);
    let jitter = (sleep_ms / 4).max(1);
    Duration::from_millis(sleep_ms / 2 + jitter)
}

/// A held directory lock. Releases (`rmdir`) on drop.
///
/// The directory protocol is inherently mutually exclusive; there is no
/// shared/read variant (upstream TS pi likewise takes an exclusive lock for both
/// reads and writes), so a single [`DirLock`] serves both the read and write
/// paths.
#[derive(Debug)]
#[must_use = "the lock is released as soon as the DirLock is dropped"]
pub struct DirLock {
    lock_path: PathBuf,
    stop_heartbeat: Option<mpsc::Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
    expected_identity: Arc<Mutex<LockIdentity>>,
    compromised: Arc<AtomicBool>,
}

impl DirLock {
    /// Acquire the directory lock at `lock_path` (an already-computed
    /// `<target>.lock` path), waiting up to `timeout`.
    ///
    /// Semantics match proper-lockfile: `mkdir` to acquire; on `EEXIST`, reclaim
    /// the lock if its mtime is stale, otherwise wait and retry until `timeout`.
    pub fn acquire(lock_path: &Path, timeout: Duration) -> io::Result<Self> {
        Self::acquire_with_timing(lock_path, timeout, STALE, UPDATE)
    }

    fn acquire_with_timing(
        lock_path: &Path,
        timeout: Duration,
        stale: Duration,
        update: Duration,
    ) -> io::Result<Self> {
        if let Some(parent) = lock_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let start = Instant::now();
        let mut attempt: u32 = 0;
        loop {
            match fs::create_dir(lock_path) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        let _ = fs::set_permissions(lock_path, fs::Permissions::from_mode(0o700));
                    }
                    return Self::start_heartbeat(lock_path, update);
                }
                Err(e) if is_already_exists(&e) => {
                    // Something occupies the path. Decide held-vs-stale exactly as
                    // proper-lockfile does, via the mtime of whatever is there.
                    match fs::symlink_metadata(lock_path) {
                        Ok(meta) => {
                            if is_stale(&meta, stale) {
                                reclaim_if_unchanged(lock_path, &meta);
                                attempt = 0; // reclaimed: retry promptly
                            }
                            // fresh: fall through to wait/retry
                        }
                        // Vanished between mkdir and stat: retry promptly.
                        Err(e) if e.kind() == io::ErrorKind::NotFound => attempt = 0,
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }

            if start.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out waiting for lock at {}", lock_path.display()),
                ));
            }
            std::thread::sleep(backoff(attempt));
            attempt = attempt.saturating_add(1);
        }
    }

    fn start_heartbeat(lock_path: &Path, update: Duration) -> io::Result<Self> {
        let acquired_identity = lock_identity(&fs::symlink_metadata(lock_path)?)?;
        let initial_identity = match refresh_identity(lock_path) {
            Ok(identity) => identity,
            Err(error) => {
                remove_owned_dir(lock_path, acquired_identity);
                return Err(error);
            }
        };
        let expected_identity = Arc::new(Mutex::new(initial_identity));
        let compromised = Arc::new(AtomicBool::new(false));
        let (stop_tx, stop_rx) = mpsc::channel();
        let heartbeat_path = lock_path.to_path_buf();
        let heartbeat_expected = Arc::clone(&expected_identity);
        let heartbeat_compromised = Arc::clone(&compromised);
        let heartbeat = match thread::Builder::new()
            .name("pi-file-lock-heartbeat".to_string())
            .spawn(move || {
                loop {
                    match stop_rx.recv_timeout(update) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }

                    let expected = *heartbeat_expected
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let current =
                        fs::symlink_metadata(&heartbeat_path).and_then(|meta| lock_identity(&meta));
                    let still_owned = current.is_ok_and(|identity| identity == expected);
                    if !still_owned {
                        heartbeat_compromised.store(true, Ordering::Release);
                        break;
                    }
                    if let Ok(identity) = refresh_identity(&heartbeat_path) {
                        *heartbeat_expected
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = identity;
                    } else {
                        heartbeat_compromised.store(true, Ordering::Release);
                        break;
                    }
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                remove_owned_dir(lock_path, initial_identity);
                return Err(error);
            }
        };
        Ok(Self {
            lock_path: lock_path.to_path_buf(),
            stop_heartbeat: Some(stop_tx),
            heartbeat: Some(heartbeat),
            expected_identity,
            compromised,
        })
    }

    /// Acquire the directory lock for a `target` file, computing the
    /// `<target>.lock` path with [`lock_path_for`].
    pub fn acquire_for(target: &Path, timeout: Duration) -> io::Result<Self> {
        Self::acquire(&lock_path_for(target), timeout)
    }
}

/// `mkdir` reports a pre-existing entry as `AlreadyExists`; when the path
/// component is itself a regular file some platforms surface `ENOTDIR`. Treat
/// both as "already occupied" so the stale/heal path runs.
fn is_already_exists(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::AlreadyExists {
        return true;
    }
    #[cfg(unix)]
    {
        e.raw_os_error() == Some(ENOTDIR)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        if let Some(stop) = self.stop_heartbeat.take() {
            let _ = stop.send(());
        }
        if let Some(heartbeat) = self.heartbeat.take() {
            let _ = heartbeat.join();
        }
        if self.compromised.load(Ordering::Acquire) {
            return;
        }
        let expected = *self
            .expected_identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        remove_owned_dir(&self.lock_path, expected);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_appends_dot_lock() {
        assert_eq!(
            lock_path_for(Path::new("/x/auth.json")),
            PathBuf::from("/x/auth.json.lock")
        );
        assert_eq!(
            lock_path_for(Path::new("/x/sessions/session-index")),
            PathBuf::from("/x/sessions/session-index.lock")
        );
    }

    #[test]
    fn acquire_creates_dir_and_release_removes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lp = dir.path().join("auth.json.lock");
        {
            let _g = DirLock::acquire(&lp, Duration::from_secs(5)).expect("acquire");
            assert!(lp.is_dir(), "lock should be a directory while held");
        }
        assert!(!lp.exists(), "lock directory should be removed on drop");
    }

    #[test]
    fn second_acquire_times_out_while_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lp = dir.path().join("auth.json.lock");
        let _g = DirLock::acquire(&lp, Duration::from_secs(5)).expect("first acquire");
        let err = DirLock::acquire(&lp, Duration::from_millis(200))
            .expect_err("second acquire must time out while held");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn heartbeat_prevents_reclaiming_a_live_long_held_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lp = dir.path().join("session-index.lock");
        let guard = DirLock::acquire_with_timing(
            &lp,
            Duration::from_secs(1),
            Duration::from_millis(180),
            Duration::from_millis(40),
        )
        .expect("first acquire");

        std::thread::sleep(Duration::from_millis(260));
        let err = DirLock::acquire_with_timing(
            &lp,
            Duration::from_millis(120),
            Duration::from_millis(180),
            Duration::from_millis(40),
        )
        .expect_err("a refreshed live lock must not be reclaimed");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(guard);
    }

    #[cfg(unix)]
    #[test]
    fn displaced_owner_does_not_remove_the_replacement_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lp = dir.path().join("session-index.lock");
        let guard = DirLock::acquire(&lp, Duration::from_secs(1)).expect("first acquire");

        fs::remove_dir(&lp).expect("displace original lock");
        fs::create_dir(&lp).expect("create replacement lock");
        drop(guard);

        assert!(
            lp.is_dir(),
            "dropping a displaced owner must preserve the replacement lock"
        );
    }

    #[test]
    fn reclaims_stale_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lp = dir.path().join("auth.json.lock");
        fs::create_dir(&lp).expect("mkdir stale");
        let old = SystemTime::now() - Duration::from_secs(30);
        filetime_set(&lp, old);
        let g =
            DirLock::acquire(&lp, Duration::from_millis(500)).expect("should reclaim stale dir");
        assert!(lp.is_dir());
        drop(g);
    }

    #[test]
    fn does_not_reclaim_fresh_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lp = dir.path().join("auth.json.lock");
        fs::create_dir(&lp).expect("mkdir fresh");
        let err = DirLock::acquire(&lp, Duration::from_millis(200))
            .expect_err("must not steal a fresh foreign lock");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn heals_stale_leftover_regular_file() {
        // Simulates the poisoning artifact left by older flock-based pi_agent_rust.
        let dir = tempfile::tempdir().expect("tempdir");
        let lp = dir.path().join("auth.json.lock");
        fs::write(&lp, b"").expect("write leftover regular file");
        let old = SystemTime::now() - Duration::from_secs(30);
        filetime_set(&lp, old);
        assert!(lp.is_file());
        {
            let _g = DirLock::acquire(&lp, Duration::from_millis(500))
                .expect("should heal stale regular file and acquire");
            assert!(
                lp.is_dir(),
                "poisoning file must be replaced by a directory"
            );
        }
        assert!(!lp.exists());
    }

    // Minimal mtime setter (avoids adding a dev-dep); uses std `File::set_times`.
    fn filetime_set(path: &Path, when: SystemTime) {
        let f = fs::File::open(path).expect("open for set_times");
        let times = fs::FileTimes::new().set_modified(when).set_accessed(when);
        f.set_times(times).expect("set_times");
    }
}
