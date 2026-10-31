//! Final charges for a session which has left the foreground. The journal is
//! separate from both transcript backends, so a retained provider request never
//! rewrites a transcript which another session instance may now be saving.

use super::{Error, RECORDED_BILLING_FAILURE, Result, add_cost};
use crate::file_identity::FileIdentity;
use crate::session::{Session, read_persisted_session_header};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(super) const MAX_ACCOUNTS: usize = 1024;
pub(super) const MAX_TARGETS: usize = 32;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024;
const SETTLEMENT_LIMIT: Duration = Duration::from_millis(250);
const LOCK_POLL: Duration = Duration::from_millis(2);
const MAX_REFERENCE_BYTES: usize = 8 * 1024;

/// Absolute account counters also deduplicate a charge mirrored into two fork
/// journals. Inherited spend and imported late charges never advance a writer's
/// own counter.
pub(super) type Coverage = BTreeMap<String, f64>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Writer {
    pub baseline_own: f64,
    pub own_total: f64,
    /// A finalized counter is only a lower bound after billing is lost.
    /// Keep that refusal with the writer across later successful settlements.
    #[serde(default)]
    pub billing_unavailable: bool,
    /// Only the last owner of an account may publish its immutable final counter.
    #[serde(default)]
    pub closed: bool,
}

pub(super) type Writers = BTreeMap<String, Writer>;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Journal {
    version: u8,
    session_id: String,
    pub writers: Writers,
}

impl Journal {
    pub fn empty(session_id: &str) -> Self {
        Self {
            version: 2,
            session_id: session_id.to_owned(),
            writers: BTreeMap::new(),
        }
    }

    pub fn billing_unavailable(&self) -> bool {
        self.writers
            .values()
            .any(|writer| writer.billing_unavailable)
    }

    pub fn ensure_billing_available(&self) -> Result<()> {
        if self.billing_unavailable() {
            return Err(Error::session(RECORDED_BILLING_FAILURE));
        }
        Ok(())
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

    pub fn uncovered_since(&self, coverage: &Coverage) -> f64 {
        self.writers.iter().fold(0.0, |sum, (account, writer)| {
            // A known counter predates this sink's baseline when the outgoing
            // source was saved again after a descendant captured its receipt.
            // That later baseline cannot refund the descendant's intervening bill.
            let covered = coverage
                .get(account)
                .copied()
                .unwrap_or(writer.baseline_own);
            add_cost(sum, (writer.own_total - covered).max(0.0))
        })
    }

    pub fn cover(&self, coverage: &mut Coverage) -> Result<()> {
        for (account, writer) in &self.writers {
            let observed = coverage.entry(account.clone()).or_default();
            *observed = observed.max(writer.own_total);
        }
        validate_coverage(coverage)
    }

    pub fn merge(&mut self, other: &Self) -> Result<()> {
        let mut writers = self.writers.clone();
        for (account, incoming) in &other.writers {
            if let Some(previous) = writers.get_mut(account) {
                merge_writer(previous, incoming)?;
            } else {
                writers.insert(account.clone(), incoming.clone());
            }
        }
        if writers.len() > MAX_ACCOUNTS {
            return Err(Error::session("too many late model cost accounts"));
        }
        self.writers = writers;
        Ok(())
    }
}

pub(super) fn merge_writer(previous: &mut Writer, incoming: &Writer) -> Result<()> {
    if (previous.closed && incoming.own_total > previous.own_total)
        || (incoming.closed && incoming.own_total < previous.own_total)
    {
        return Err(Error::session("closed model cost counter changed"));
    }
    previous.baseline_own = previous.baseline_own.min(incoming.baseline_own);
    previous.own_total = previous.own_total.max(incoming.own_total);
    previous.billing_unavailable |= incoming.billing_unavailable;
    previous.closed |= incoming.closed;
    Ok(())
}

/// A serialized capability can name only one native bounded ledger. It never
/// names a transcript and cannot be promoted to a retirement write capability.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct LedgerRef {
    directory: PathBuf,
    journal: String,
    session_id: String,
}

impl LedgerRef {
    pub fn key(&self) -> PathBuf {
        self.directory.join(&self.journal)
    }

    pub fn validate(&self) -> Result<()> {
        let digest = self
            .journal
            .strip_prefix(".pi-cost-")
            .and_then(|name| name.strip_suffix(".json"));
        if !self.directory.is_absolute()
            || self.directory.as_os_str().as_encoded_bytes().len() > 4096
            || self.directory.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )
            })
            || self.session_id.is_empty()
            || self.session_id.len() > 1024
            || !digest.is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            || serde_json::to_vec(self)?.len() > MAX_REFERENCE_BYTES
        {
            return Err(Error::session("invalid late model cost ledger reference"));
        }
        Ok(())
    }

    pub fn open(&self, accounts: BTreeSet<String>) -> Result<ReadOrigin> {
        self.validate()?;
        if accounts.len() > MAX_ACCOUNTS
            || accounts
                .iter()
                .any(|account| uuid::Uuid::parse_str(account).is_err())
        {
            return Err(Error::session("invalid late model cost ledger accounts"));
        }
        if std::fs::canonicalize(&self.directory)? != self.directory {
            return Err(Error::session("late model cost directory moved"));
        }
        let directory = open_directory(&self.directory)?;
        let directory_identity = FileIdentity::of_open_file(&directory)?;
        let origin = Arc::new(Origin {
            key: self.key(),
            session_id: self.session_id.clone(),
            source: None,
            parent: self.directory.clone(),
            directory,
            directory_identity,
            journal_name: self.journal.clone(),
        });
        origin.validate_directory()?;
        Ok(ReadOrigin {
            origin,
            required_accounts: accounts.clone(),
            accounts,
            all_accounts: false,
            observed: Arc::new(Mutex::new(Writers::new())),
        })
    }
}

#[derive(Clone)]
pub(super) struct ReadOrigin {
    // Private: callers cannot extract an Origin and acquire write authority.
    origin: Arc<Origin>,
    pub accounts: BTreeSet<String>,
    required_accounts: BTreeSet<String>,
    pub all_accounts: bool,
    // Valid reads are monotonic evidence, not another path capability. Sharing
    // this cache cannot make a clone follow an abandoned candidate's directory.
    observed: Arc<Mutex<Writers>>,
}

impl ReadOrigin {
    pub fn from_source(origin: Arc<Origin>, accounts: BTreeSet<String>) -> Self {
        Self {
            origin,
            accounts,
            required_accounts: BTreeSet::new(),
            all_accounts: false,
            observed: Arc::new(Mutex::new(Writers::new())),
        }
    }

    pub fn watch_source(origin: Arc<Origin>, accounts: BTreeSet<String>) -> Self {
        Self {
            origin,
            accounts,
            required_accounts: BTreeSet::new(),
            all_accounts: true,
            observed: Arc::new(Mutex::new(Writers::new())),
        }
    }

    pub fn reference(&self) -> LedgerRef {
        self.origin.reference()
    }

    pub fn merge(&mut self, other: Self) -> Result<()> {
        if self.reference() != other.reference() {
            return Err(Error::session("model cost ledger identity changed"));
        }
        let observed = other.observations();
        self.remember(&observed)?;
        self.accounts.extend(other.accounts);
        self.required_accounts.extend(other.required_accounts);
        self.all_accounts |= other.all_accounts;
        Ok(())
    }

    pub fn require(&mut self, accounts: BTreeSet<String>) {
        self.required_accounts.extend(accounts);
    }

    fn observations(&self) -> Writers {
        self.observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn remember(&self, incoming: &Writers) -> Result<Writers> {
        let mut observed = self
            .observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut merged = observed.clone();
        for (account, writer) in incoming {
            if let Some(previous) = merged.get_mut(account) {
                merge_writer(previous, writer)?;
            } else {
                merged.insert(account.clone(), writer.clone());
            }
        }
        if merged.len() > MAX_ACCOUNTS {
            return Err(Error::session("too many observed model cost writers"));
        }
        *observed = merged.clone();
        Ok(merged)
    }

    pub fn scoped(&self, local_account: &str) -> Result<(Journal, Option<Self>)> {
        let journal = self.read()?;
        let accounts: BTreeSet<_> = journal
            .writers
            .iter()
            .filter(|(account, writer)| account.as_str() != local_account && !writer.closed)
            .map(|(account, _)| account.clone())
            .collect();
        if accounts.is_empty() {
            return Ok((journal, None));
        }
        let mut scoped = self.clone();
        scoped.accounts = accounts.clone();
        scoped.required_accounts = accounts;
        scoped.all_accounts = false;
        Ok((journal, Some(scoped)))
    }

    pub fn read(&self) -> Result<Journal> {
        let known = self.observations();
        let mut required = self.required_accounts.clone();
        for (account, writer) in &known {
            if self.all_accounts || self.accounts.contains(account) {
                if writer.closed {
                    required.remove(account);
                } else {
                    required.insert(account.clone());
                }
            }
        }
        let mut journal = self.origin.read_with_presence(!required.is_empty())?;
        if required
            .iter()
            .any(|account| !journal.writers.contains_key(account))
        {
            return Err(Error::session(
                "referenced model cost writer is unavailable",
            ));
        }
        if !self.all_accounts {
            journal
                .writers
                .retain(|account, _| self.accounts.contains(account));
        }
        // Compare to evidence captured before opening this file. Another reader
        // may observe a newer atomic rename while this read is in flight.
        if journal.writers.iter().any(|(account, writer)| {
            known.get(account).is_some_and(|previous| {
                writer.own_total < previous.own_total
                    || (previous.closed && !writer.closed)
                    || (previous.billing_unavailable && !writer.billing_unavailable)
            })
        }) {
            return Err(Error::session(
                "observed model cost journal moved backwards",
            ));
        }
        journal.writers = self.remember(&journal.writers)?;
        if !self.all_accounts {
            journal
                .writers
                .retain(|account, _| self.accounts.contains(account));
        }
        Ok(journal)
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
    source: Option<PathBuf>,
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
            source: Some(source),
            parent,
            directory,
            directory_identity,
            journal_name,
        })))
    }

    pub fn validate_source(&self) -> Result<()> {
        self.validate_directory()?;
        let source = self
            .source
            .as_ref()
            .ok_or_else(|| Error::session("a model cost read reference cannot authorize writes"))?;
        let header = read_persisted_session_header(source)?;
        if header.id != self.session_id {
            return Err(Error::session("late model cost source session changed"));
        }
        Ok(())
    }

    pub fn reference(&self) -> LedgerRef {
        LedgerRef {
            directory: self.parent.clone(),
            journal: self.journal_name.clone(),
            session_id: self.session_id.clone(),
        }
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
        self.read_with_presence(false)
    }

    fn read_with_presence(&self, required: bool) -> Result<Journal> {
        let file = match self.open_member(OsStr::new(&self.journal_name), false) {
            Ok(file) => file,
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.validate_directory()?;
                if required {
                    return Err(Error::session(
                        "referenced model cost ledger is unavailable",
                    ));
                }
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
        if !matches!(journal.version, 1 | 2)
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

    fn update(
        &self,
        account: &str,
        own: Writer,
        imported: &Writers,
        covered: &Coverage,
        deadline: Instant,
    ) -> Result<()> {
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
        let additional = usize::from(!journal.writers.contains_key(account))
            + imported
                .keys()
                .filter(|id| id.as_str() != account && !journal.writers.contains_key(*id))
                .count();
        if journal.writers.len().saturating_add(additional) > MAX_ACCOUNTS {
            return Err(Error::session("too many late model cost accounts"));
        }
        let writer = journal.writers.entry(account.to_owned()).or_insert(Writer {
            baseline_own: own.baseline_own,
            own_total: own.baseline_own,
            billing_unavailable: false,
            closed: false,
        });
        // Only this account owns its finalized counter. A decreasing local
        // counter is an accounting error; relayed observations below may be
        // older than another process's already-persisted observation.
        if writer.baseline_own > own.baseline_own
            || writer.own_total > own.own_total
            || (writer.closed && (!own.closed || writer.own_total != own.own_total))
        {
            return Err(Error::session("late model cost counter moved backwards"));
        }
        // Keep the first baseline on repeated retirement. A later checkpoint
        // covers it independently; resetting it would erase unimported charges.
        writer.own_total = own.own_total;
        writer.billing_unavailable |= own.billing_unavailable;
        writer.closed |= own.closed;
        for (id, observed) in imported {
            if id == account {
                continue;
            }
            // The saved checkpoint already includes its covered counters.
            // For an account first seen after retirement, retain the original
            // import floor instead of charging fees predating this tree.
            let baseline = covered
                .get(id)
                .copied()
                .unwrap_or(0.0)
                .max(observed.baseline_own);
            let writer = journal.writers.entry(id.clone()).or_insert(Writer {
                baseline_own: baseline,
                own_total: baseline,
                billing_unavailable: false,
                closed: false,
            });
            let mut observed = observed.clone();
            observed.baseline_own = baseline;
            merge_writer(writer, &observed)?;
        }
        journal.version = 2;
        self.write(&journal, deadline)
    }
}

pub(super) struct Retirement {
    pub origin: Arc<Origin>,
    pub baseline: f64,
    covered: Coverage,
    reader: ReadOrigin,
    // Retain the initiating capability owner for this fixed-target cleanup.
    // Cancellation may stop new work; it cannot revoke an admitted bill.
    _owner: asupersync::Cx,
}

impl Retirement {
    pub fn prepare(
        origin: Arc<Origin>,
        baseline: f64,
        covered: Coverage,
        owner: &asupersync::Cx,
    ) -> Result<Self> {
        let capabilities = owner.capabilities();
        if !capabilities.io || !capabilities.time {
            return Err(Error::session(
                "late model cost retirement requires I/O and timer authority",
            ));
        }
        validate_coverage(&covered)?;
        origin.validate_source()?;
        let reader = ReadOrigin::watch_source(Arc::clone(&origin), BTreeSet::new());
        Ok(Self {
            origin,
            baseline,
            covered,
            reader,
            _owner: owner.clone(),
        })
    }

    pub fn reader(&self) -> ReadOrigin {
        self.reader.clone()
    }

    pub fn persist(
        &self,
        account: &str,
        total: f64,
        billing_unavailable: bool,
        imported: &Writers,
        deadline: Instant,
    ) -> Result<()> {
        self.persist_with_closed(
            account,
            total,
            billing_unavailable,
            false,
            imported,
            deadline,
        )
    }

    pub fn persist_with_closed(
        &self,
        account: &str,
        total: f64,
        billing_unavailable: bool,
        closed: bool,
        imported: &Writers,
        deadline: Instant,
    ) -> Result<()> {
        let writer = Writer {
            baseline_own: self.baseline,
            own_total: total,
            billing_unavailable,
            closed,
        };
        self.origin
            .update(account, writer.clone(), imported, &self.covered, deadline)?;
        self.reader
            .remember(&Writers::from([(account.to_owned(), writer)]))?;
        Ok(())
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
