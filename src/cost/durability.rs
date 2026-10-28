//! Final charges for a session which has left the foreground. The journal is
//! separate from both transcript backends, so a retained provider request never
//! rewrites a transcript which another session instance may now be saving.

use super::{Error, Result, add_cost};
use crate::file_identity::FileIdentity;
use crate::session::{Session, read_persisted_session_header};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(super) const MAX_ACCOUNTS: usize = 1024;
pub(super) const MAX_TARGETS: usize = 32;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024;
const SETTLEMENT_LIMIT: Duration = Duration::from_millis(250);
const LOCK_POLL: Duration = Duration::from_millis(2);

/// Absolute account counters also deduplicate a charge mirrored into two fork
/// journals. Inherited spend and imported late charges never advance a writer's
/// own counter.
pub(super) type Coverage = BTreeMap<String, f64>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Writer {
    pub baseline_own: f64,
    pub own_total: f64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Journal {
    version: u8,
    session_id: String,
    pub writers: BTreeMap<String, Writer>,
}

impl Journal {
    fn empty(session_id: &str) -> Self {
        Self {
            version: 1,
            session_id: session_id.to_owned(),
            writers: BTreeMap::new(),
        }
    }

    pub fn uncovered(&self, coverage: &Coverage) -> f64 {
        self.writers.iter().fold(0.0, |sum, (account, writer)| {
            let covered = coverage.get(account).copied().unwrap_or(0.0);
            add_cost(
                sum,
                (writer.own_total - covered.max(writer.baseline_own)).max(0.0),
            )
        })
    }

    pub fn cover(&self, coverage: &mut Coverage) -> Result<()> {
        for (account, writer) in &self.writers {
            let observed = coverage.entry(account.clone()).or_default();
            *observed = observed.max(writer.own_total);
        }
        validate_coverage(coverage)
    }
}

pub(super) fn validate_coverage(coverage: &Coverage) -> Result<()> {
    if coverage.len() > MAX_ACCOUNTS
        || coverage.iter().any(|(account, amount)| {
            uuid::Uuid::parse_str(account).is_err() || !amount.is_finite() || *amount < 0.0
        })
    {
        return Err(Error::session("invalid late model cost coverage"));
    }
    Ok(())
}

/// A read view has no write capability. A retirement promotes exactly this
/// target after the source checkpoint has an acknowledged persistence receipt.
pub(super) struct Origin {
    pub key: PathBuf,
    session_id: String,
    source: PathBuf,
    parent: PathBuf,
    directory: File,
    directory_identity: FileIdentity,
    journal_name: String,
}

impl Origin {
    pub fn for_session(session: &Session) -> Result<Option<Arc<Self>>> {
        let Some(path) = &session.path else {
            return Ok(None);
        };
        let source = match std::fs::canonicalize(path) {
            Ok(source) => source,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && !session.has_partial_v2_hydration()
                    && !session.entry_is_persisted(0) =>
            {
                // An explicit --session path can be selected before the first
                // save. It has no journal to import and grants no write sink.
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let metadata = std::fs::symlink_metadata(&source)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(Error::session("late model cost source must be a regular file"));
        }
        let parent = source
            .parent()
            .ok_or_else(|| Error::session("late model cost source has no parent"))?
            .to_path_buf();
        let directory = open_directory(&parent)?;
        let directory_identity = FileIdentity::of_open_file(&directory)?;
        if FileIdentity::of_path_nofollow(&parent)? != directory_identity {
            return Err(Error::session("late model cost directory changed during open"));
        }
        // Include the session ID in the filename: reusing a source pathname for
        // a different session cannot inherit or poison its predecessor's ledger.
        let name = source
            .file_name()
            .ok_or_else(|| Error::session("late model cost source has no filename"))?;
        let mut digest = Sha256::new();
        digest.update(name.as_encoded_bytes());
        digest.update([0]);
        digest.update(session.header.id.as_bytes());
        let suffix = crate::package_manager::hex_encode(&digest.finalize());
        let journal_name = format!(".pi-cost-{suffix}.json");
        Ok(Some(Arc::new(Self {
            key: parent.join(&journal_name),
            session_id: session.header.id.clone(),
            source,
            parent,
            directory,
            directory_identity,
            journal_name,
        })))
    }

    pub fn validate_source(&self) -> Result<()> {
        self.validate_directory()?;
        let header = read_persisted_session_header(&self.source)?;
        if header.id != self.session_id {
            return Err(Error::session("late model cost source session changed"));
        }
        Ok(())
    }

    fn validate_directory(&self) -> Result<()> {
        let metadata = std::fs::symlink_metadata(&self.parent)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || std::fs::canonicalize(&self.parent)? != self.parent
            || FileIdentity::of_path_nofollow(&self.parent)? != self.directory_identity
        {
            return Err(Error::session("late model cost directory changed"));
        }
        Ok(())
    }

    fn open_member(&self, name: &OsStr, create: bool) -> Result<File> {
        self.validate_directory()?;
        #[cfg(unix)]
        if create {
            crate::platform::EffectiveModeAccessContext::current()?.ensure(
                &self.directory.metadata()?,
                &self.parent,
                crate::platform::UNIX_ACCESS_WRITE | crate::platform::UNIX_ACCESS_SEARCH,
                "late model cost directory",
            )?;
        }
        #[cfg(unix)]
        let file = {
            let flags = if create {
                rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CREATE
            } else {
                rustix::fs::OFlags::RDONLY
            };
            let descriptor = rustix::fs::openat(
                &self.directory,
                name,
                flags
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map_err(std::io::Error::from)?;
            File::from(descriptor)
        };
        #[cfg(not(unix))]
        let file = {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(create).create(create).truncate(false);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt as _;
                options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
            }
            options.open(self.parent.join(name))?
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(Error::session("late model cost journal must be a regular file"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(Error::session("late model cost journal is a reparse point"));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() != 1 {
                return Err(Error::session("late model cost journal has multiple links"));
            }
            let access = crate::platform::UNIX_ACCESS_READ
                | if create { crate::platform::UNIX_ACCESS_WRITE } else { 0 };
            crate::platform::EffectiveModeAccessContext::current()?.ensure(
                &metadata,
                &self.parent.join(name),
                access,
                "late model cost journal",
            )?;
        }
        self.validate_directory()?;
        Ok(file)
    }

    pub fn read(&self) -> Result<Journal> {
        let file = match self.open_member(OsStr::new(&self.journal_name), false) {
            Ok(file) => file,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.validate_directory()?;
                return Ok(Journal::empty(&self.session_id));
            }
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take(MAX_JOURNAL_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(Error::session("late model cost journal exceeds its size limit"));
        }
        let journal: Journal = serde_json::from_slice(&bytes)?;
        if journal.version != 1
            || journal.session_id != self.session_id
            || journal.writers.len() > MAX_ACCOUNTS
            || journal.writers.iter().any(|(account, writer)| {
                uuid::Uuid::parse_str(account).is_err()
                    || !writer.baseline_own.is_finite()
                    || writer.baseline_own < 0.0
                    || !writer.own_total.is_finite()
                    || writer.own_total < writer.baseline_own
            })
        {
            return Err(Error::session("invalid late model cost journal"));
        }
        Ok(journal)
    }

    fn write(&self, journal: &Journal, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        let encoded = serde_json::to_vec(journal)?;
        if encoded.len() as u64 > MAX_JOURNAL_BYTES {
            return Err(Error::session("late model cost journal exceeds its size limit"));
        }
        self.validate_directory()?;
        #[cfg(unix)]
        {
            // Create and rename relative to the retained directory descriptor.
            // Renaming an ancestor cannot redirect even the staging write.
            let temporary_name = format!(".pi-cost-{}.pending", uuid::Uuid::new_v4());
            let descriptor = rustix::fs::openat(
                &self.directory,
                &temporary_name,
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::CLOEXEC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map_err(std::io::Error::from)?;
            let mut temporary = File::from(descriptor);
            temporary.write_all(&encoded)?;
            temporary.sync_all()?;
            remaining(deadline)?;
            self.validate_directory()?;
            rustix::fs::renameat(
                &self.directory,
                &temporary_name,
                &self.directory,
                &self.journal_name,
            )
            .map_err(std::io::Error::from)?;
            self.directory.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            let mut temporary = tempfile::NamedTempFile::new_in(&self.parent)?;
            temporary.write_all(&encoded)?;
            temporary.as_file().sync_all()?;
            remaining(deadline)?;
            self.validate_directory()?;
            temporary
                .persist(self.parent.join(&self.journal_name))
                .map_err(|error| Error::from(error.error))?;
        }
        Ok(())
    }

    fn update(&self, account: &str, baseline: f64, total: f64, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        let lock_name = format!("{}.lock", self.journal_name);
        let lock = self.open_member(OsStr::new(&lock_name), true)?;
        loop {
            remaining(deadline)?;
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    std::thread::sleep(LOCK_POLL.min(remaining(deadline)?));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        // Dropping this handle releases the dedicated journal lock. Transcript
        // locks are never acquired here, including during canceled cleanup.
        let mut journal = self.read()?;
        if !journal.writers.contains_key(account) && journal.writers.len() >= MAX_ACCOUNTS {
            return Err(Error::session("too many late model cost accounts"));
        }
        let writer = journal.writers.entry(account.to_owned()).or_insert(Writer {
            baseline_own: baseline,
            own_total: baseline,
        });
        if writer.baseline_own > baseline || writer.own_total > total {
            return Err(Error::session("late model cost counter moved backwards"));
        }
        // Keep the first baseline on repeated retirement. A later checkpoint
        // covers it independently; resetting it would erase unimported charges.
        writer.own_total = total;
        self.write(&journal, deadline)
    }
}

pub(super) struct Retirement {
    pub origin: Arc<Origin>,
    pub baseline: f64,
    // Retain the initiating capability owner for this fixed-target cleanup.
    // Cancellation may stop new work; it cannot revoke an admitted bill.
    _owner: asupersync::Cx,
}

impl Retirement {
    pub fn prepare(origin: Arc<Origin>, baseline: f64, owner: &asupersync::Cx) -> Result<Self> {
        let capabilities = owner.capabilities();
        if !capabilities.io || !capabilities.time {
            return Err(Error::session("late model cost retirement requires I/O and timer authority"));
        }
        origin.validate_source()?;
        Ok(Self {
            origin,
            baseline,
            _owner: owner.clone(),
        })
    }

    pub fn persist(&self, account: &str, total: f64, deadline: Instant) -> Result<()> {
        self.origin.update(account, self.baseline, total, deadline)
    }
}

pub(super) fn settlement_deadline(deadline: Option<Instant>) -> Instant {
    let limit = Instant::now() + SETTLEMENT_LIMIT;
    deadline.map_or(limit, |deadline| deadline.min(limit))
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Error::session("late model cost persistence deadline expired"));
    }
    Ok(remaining)
}

fn open_directory(path: &Path) -> Result<File> {
    #[cfg(unix)]
    {
        let descriptor = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        Ok(File::from(descriptor))
    }
    #[cfg(not(unix))]
    {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.custom_flags(0x0220_0000); // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
            options.share_mode(0x0000_0003); // READ | WRITE; pin the directory against deletion.
        }
        Ok(options.open(path)?)
    }
}
