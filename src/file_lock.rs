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

#[cfg(unix)]
use std::ffi::{OsStr, OsString};
use std::fs;
#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
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
    meta.modified()
        .is_ok_and(|mtime| is_stale_modified(mtime, stale))
}

fn is_stale_modified(modified: SystemTime, stale: Duration) -> bool {
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age > stale)
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

#[cfg(unix)]
fn stat_identifier_to_u64<T>(value: T, field: &'static str) -> io::Result<u64>
where
    u64: TryFrom<T>,
{
    u64::try_from(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("lock {field} overflow")))
}

#[cfg(unix)]
fn lock_identity_at(directory: &File, lock_name: &OsStr) -> io::Result<LockIdentity> {
    let stat = rustix::fs::statat(directory, lock_name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)?;
    let modified_seconds = u64::try_from(stat.st_mtime).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "lock mtime predates the Unix epoch",
        )
    })?;
    let modified_nanoseconds = u32::try_from(stat.st_mtime_nsec).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "lock mtime has invalid nanoseconds",
        )
    })?;
    if modified_nanoseconds >= 1_000_000_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "lock mtime has out-of-range nanoseconds",
        ));
    }
    let modified = SystemTime::UNIX_EPOCH
        .checked_add(Duration::new(modified_seconds, modified_nanoseconds))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "lock mtime overflow"))?;
    let device = stat_identifier_to_u64(stat.st_dev, "device id")?;
    let inode = stat_identifier_to_u64(stat.st_ino, "inode")?;

    Ok(LockIdentity {
        modified,
        is_dir: rustix::fs::FileType::from_raw_mode(stat.st_mode)
            == rustix::fs::FileType::Directory,
        device,
        inode,
    })
}

#[cfg(unix)]
fn open_lock_dir_at(directory: &File, lock_name: &OsStr) -> io::Result<File> {
    let descriptor = rustix::fs::openat(
        directory,
        lock_name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    Ok(File::from(descriptor))
}

#[cfg(unix)]
fn reclaim_if_unchanged_at(directory: &File, lock_name: &OsStr, observed: LockIdentity) {
    let Ok(current) = lock_identity_at(directory, lock_name) else {
        return;
    };
    if current != observed {
        return;
    }
    let flags = if current.is_dir {
        rustix::fs::AtFlags::REMOVEDIR
    } else {
        rustix::fs::AtFlags::empty()
    };
    let _ = rustix::fs::unlinkat(directory, lock_name, flags);
}

#[cfg(unix)]
fn remove_owned_dir_at(directory: &File, lock_name: &OsStr, expected: LockIdentity) {
    let still_owned = lock_identity_at(directory, lock_name)
        .is_ok_and(|identity| identity == expected && identity.is_dir);
    if still_owned {
        let _ = rustix::fs::unlinkat(directory, lock_name, rustix::fs::AtFlags::REMOVEDIR);
    }
}

#[cfg(unix)]
struct CreatedLockCleanup {
    directory: Arc<File>,
    lock_name: OsString,
    expected: LockIdentity,
    armed: bool,
}

#[cfg(unix)]
impl CreatedLockCleanup {
    const fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for CreatedLockCleanup {
    fn drop(&mut self) {
        if self.armed {
            remove_owned_dir_at(&self.directory, &self.lock_name, self.expected);
        }
    }
}

#[cfg(unix)]
fn refresh_identity_at(lock_handle: &File) -> io::Result<LockIdentity> {
    rustix::fs::futimens(
        lock_handle,
        &rustix::fs::Timestamps {
            last_access: rustix::fs::Timespec {
                tv_sec: 0,
                tv_nsec: rustix::fs::UTIME_OMIT,
            },
            last_modification: rustix::fs::Timespec {
                tv_sec: 0,
                tv_nsec: rustix::fs::UTIME_NOW,
            },
        },
    )
    .map_err(io::Error::from)?;
    lock_identity(&lock_handle.metadata()?)
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

/// A proper-lockfile-compatible lock whose parent directory is held by descriptor.
///
/// Path-based locking is sufficient while every ancestor remains stable. Security-sensitive
/// atomic writes already hold their destination directory open, though, and must keep the lock
/// attached to that exact directory if an ancestor is concurrently renamed or replaced. This
/// variant performs every operation relative to the supplied descriptor: acquisition, stale
/// reclaim, heartbeat, ownership validation, and release.
#[cfg(unix)]
#[derive(Debug)]
#[must_use = "the lock is released as soon as the DirLockAt is dropped"]
pub struct DirLockAt {
    directory: Arc<File>,
    lock_name: OsString,
    lock_handle: Arc<File>,
    stop_heartbeat: Option<mpsc::Sender<()>>,
    heartbeat: Option<JoinHandle<()>>,
    expected_identity: Arc<Mutex<LockIdentity>>,
    compromised: Arc<AtomicBool>,
}

#[cfg(unix)]
impl DirLockAt {
    /// Acquire the proper-lockfile directory for `target_name` inside `directory`.
    ///
    /// The caller must keep using the same directory descriptor for the protected write. The
    /// resulting physical entry is still named `<target_name>.lock`, so pathname-based upstream
    /// clients and [`DirLock`] observe the same mutually exclusive lock.
    pub fn acquire_for(
        directory: &File,
        target_name: &OsStr,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::acquire_for_with_timing(directory, target_name, timeout, STALE, UPDATE)
    }

    fn acquire_for_with_timing(
        directory: &File,
        target_name: &OsStr,
        timeout: Duration,
        stale: Duration,
        update: Duration,
    ) -> io::Result<Self> {
        let mut lock_name = target_name.to_os_string();
        lock_name.push(".lock");
        let directory = Arc::new(directory.try_clone()?);
        let start = Instant::now();
        let mut attempt: u32 = 0;

        loop {
            match rustix::fs::mkdirat(
                &*directory,
                &lock_name,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
            ) {
                Ok(()) => {
                    // Capture the identity immediately after our successful mkdir. If opening the
                    // directory subsequently fails (for example because the process exhausts file
                    // descriptors), cleanup may remove only this exact entry. A concurrently
                    // replaced lock must remain untouched.
                    let acquired_identity = lock_identity_at(&directory, &lock_name)?;
                    let mut cleanup = CreatedLockCleanup {
                        directory: Arc::clone(&directory),
                        lock_name: lock_name.clone(),
                        expected: acquired_identity,
                        armed: true,
                    };
                    if !acquired_identity.is_dir {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "new descriptor-relative lock is not a directory",
                        ));
                    }
                    let lock_handle = open_lock_dir_at(&directory, &lock_name)?;
                    let opened_identity = lock_identity(&lock_handle.metadata()?)?;
                    if opened_identity != acquired_identity {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "descriptor-relative lock changed while it was being opened",
                        ));
                    }
                    lock_handle.set_permissions(fs::Permissions::from_mode(0o700))?;
                    let result =
                        Self::start_heartbeat(directory, lock_name, Arc::new(lock_handle), update);
                    if result.is_ok() {
                        cleanup.disarm();
                    }
                    return result;
                }
                Err(rustix::io::Errno::EXIST) => match lock_identity_at(&directory, &lock_name) {
                    Ok(identity) => {
                        if is_stale_modified(identity.modified, stale) {
                            reclaim_if_unchanged_at(&directory, &lock_name, identity);
                            attempt = 0;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => attempt = 0,
                    Err(error) => return Err(error),
                },
                Err(error) => return Err(io::Error::from(error)),
            }

            if start.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for descriptor-relative lock at {}",
                        lock_name.to_string_lossy()
                    ),
                ));
            }
            std::thread::sleep(backoff(attempt));
            attempt = attempt.saturating_add(1);
        }
    }

    fn start_heartbeat(
        directory: Arc<File>,
        lock_name: OsString,
        lock_handle: Arc<File>,
        update: Duration,
    ) -> io::Result<Self> {
        let acquired_identity = lock_identity(&lock_handle.metadata()?)?;
        let initial_identity = match refresh_identity_at(&lock_handle) {
            Ok(identity) => identity,
            Err(error) => {
                remove_owned_dir_at(&directory, &lock_name, acquired_identity);
                return Err(error);
            }
        };
        let expected_identity = Arc::new(Mutex::new(initial_identity));
        let compromised = Arc::new(AtomicBool::new(false));
        let (stop_tx, stop_rx) = mpsc::channel();
        let heartbeat_directory = Arc::clone(&directory);
        let heartbeat_name = lock_name.clone();
        let heartbeat_handle = Arc::clone(&lock_handle);
        let heartbeat_expected = Arc::clone(&expected_identity);
        let heartbeat_compromised = Arc::clone(&compromised);
        let heartbeat = match thread::Builder::new()
            .name("pi-file-lock-at-heartbeat".to_string())
            .spawn(move || {
                loop {
                    match stop_rx.recv_timeout(update) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }

                    let expected = *heartbeat_expected
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let still_owned = lock_identity_at(&heartbeat_directory, &heartbeat_name)
                        .is_ok_and(|identity| identity == expected);
                    if !still_owned {
                        heartbeat_compromised.store(true, Ordering::Release);
                        break;
                    }
                    if let Ok(identity) = refresh_identity_at(&heartbeat_handle) {
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
                remove_owned_dir_at(&directory, &lock_name, initial_identity);
                return Err(error);
            }
        };

        Ok(Self {
            directory,
            lock_name,
            lock_handle,
            stop_heartbeat: Some(stop_tx),
            heartbeat: Some(heartbeat),
            expected_identity,
            compromised,
        })
    }
}

#[cfg(unix)]
impl Drop for DirLockAt {
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
        let handle_identity = self
            .lock_handle
            .metadata()
            .and_then(|metadata| lock_identity(&metadata));
        if handle_identity.is_ok_and(|identity| identity == expected) {
            remove_owned_dir_at(&self.directory, &self.lock_name, expected);
        }
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

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_lock_interoperates_with_path_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let directory = open_test_directory(dir.path());
        let lock_path = dir.path().join("auth.json.lock");
        let guard =
            DirLockAt::acquire_for(&directory, OsStr::new("auth.json"), Duration::from_secs(1))
                .expect("acquire descriptor-relative lock");

        assert!(lock_path.is_dir(), "lock must use the proper-lockfile name");
        let error = DirLock::acquire(&lock_path, Duration::from_millis(100))
            .expect_err("the pathname client must observe the held descriptor lock");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        drop(guard);
        assert!(
            !lock_path.exists(),
            "descriptor-relative drop must release the shared lock entry"
        );
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_lock_survives_ancestor_swap_and_cleans_original_directory() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let parent = dir.path().join("agent");
        let moved_parent = dir.path().join("moved-agent");
        let redirected_parent = dir.path().join("redirected");
        fs::create_dir(&parent).expect("create lock parent");
        fs::create_dir(&redirected_parent).expect("create redirected parent");
        let directory = open_test_directory(&parent);
        let guard = DirLockAt::acquire_for_with_timing(
            &directory,
            OsStr::new("auth.json"),
            Duration::from_secs(1),
            Duration::from_millis(180),
            Duration::from_millis(40),
        )
        .expect("acquire descriptor-relative lock");

        fs::rename(&parent, &moved_parent).expect("move original parent");
        symlink(&redirected_parent, &parent).expect("replace parent with redirect");
        std::thread::sleep(Duration::from_millis(260));

        let moved_lock = moved_parent.join("auth.json.lock");
        let error = DirLock::acquire(&moved_lock, Duration::from_millis(120))
            .expect_err("the descriptor heartbeat must keep the moved live lock fresh");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            !redirected_parent.join("auth.json.lock").exists(),
            "the replacement path must never receive lock state"
        );

        drop(guard);
        assert!(
            !moved_lock.exists(),
            "drop must remove the owned lock from the descriptor-pinned directory"
        );
        assert!(
            !redirected_parent.join("auth.json.lock").exists(),
            "release must not touch the replacement path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_lock_reclaims_stale_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let directory = open_test_directory(dir.path());
        let lock_path = dir.path().join("auth.json.lock");
        fs::write(&lock_path, b"").expect("create stale legacy lock file");
        filetime_set(&lock_path, SystemTime::now() - Duration::from_secs(30));

        let guard = DirLockAt::acquire_for(
            &directory,
            OsStr::new("auth.json"),
            Duration::from_millis(500),
        )
        .expect("reclaim stale descriptor-relative lock");
        assert!(
            lock_path.is_dir(),
            "the stale legacy file must become a live lock directory"
        );
        drop(guard);
        assert!(!lock_path.exists(), "the acquired lock must be released");
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_failed_open_cleanup_preserves_replacement_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let directory = open_test_directory(dir.path());
        let lock_name = OsStr::new("auth.json.lock");
        let lock_path = dir.path().join(lock_name);
        let preserved_path = dir.path().join("auth.json.lock.original");
        fs::create_dir(&lock_path).expect("create original lock");
        let original_identity = lock_identity_at(&directory, lock_name).expect("original identity");
        let cleanup = CreatedLockCleanup {
            directory: Arc::new(directory.try_clone().expect("clone directory")),
            lock_name: lock_name.to_os_string(),
            expected: original_identity,
            armed: true,
        };
        fs::rename(&lock_path, &preserved_path).expect("move original lock");
        fs::create_dir(&lock_path).expect("create replacement lock");

        drop(cleanup);

        assert!(
            lock_path.is_dir(),
            "identity-checked cleanup must not remove a replacement lock"
        );
        assert!(
            preserved_path.is_dir(),
            "the original fixture remains isolated"
        );
    }

    #[cfg(unix)]
    fn open_test_directory(path: &Path) -> File {
        let descriptor = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .expect("open test directory");
        File::from(descriptor)
    }

    // Minimal mtime setter (avoids adding a dev-dep); uses std `File::set_times`.
    fn filetime_set(path: &Path, when: SystemTime) {
        let f = fs::File::open(path).expect("open for set_times");
        let times = fs::FileTimes::new().set_modified(when).set_accessed(when);
        f.set_times(times).expect("set_times");
    }
}
