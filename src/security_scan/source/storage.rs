//! Confined persistence for source reports and review decisions. All relative
//! components are opened without following links. Writes stage a new private
//! file and publish it atomically; no existing inode is opened for writing.
//!
//! The workspace is selected by the trusted host. These descriptor boundaries
//! are not a sandbox against other code running as the same OS user.

use super::{Disposition, MAX_FINDINGS, MAX_SOURCE_BYTES, Result, scan_error};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

pub(super) const DEFAULT_REPORT: &str = ".pi/security-scan.sarif";
const DISPOSITIONS: &str = ".pi/security-dispositions.json";
const STORE_LIMIT: u64 = 4 * 1024 * 1024;

pub(super) fn check_owner() -> Result<()> {
    super::operation::checkpoint()
}

fn relative_file(raw: &str) -> Result<PathBuf> {
    if raw.is_empty()
        || raw.len() > 4096
        || raw.contains(['\\', ':'])
        || raw.chars().any(char::is_control)
        || raw.ends_with('/')
    {
        return Err(scan_error(
            "expected a bounded, control-free relative file path",
        ));
    }
    let mut normalized = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            _ => return Err(scan_error("file path must stay inside the workspace")),
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(scan_error("file path must name a file"));
    }
    Ok(normalized)
}

pub(super) fn report_path(raw: &str) -> Result<PathBuf> {
    let path = relative_file(raw)?;
    if path
        .extension()
        .is_none_or(|extension| extension != "sarif")
    {
        return Err(scan_error("source report paths must end in .sarif"));
    }
    Ok(path)
}

// Reject duplicate fingerprints before a map can silently keep the final
// decision. Disposition's derived deserializer also rejects duplicate fields.
fn decode_store(bytes: &[u8]) -> Result<BTreeMap<String, Disposition>> {
    struct UniqueStore;
    impl<'de> serde::de::Visitor<'de> for UniqueStore {
        type Value = BTreeMap<String, Disposition>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded map of unique source finding decisions")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut store = BTreeMap::new();
            while let Some((identity, decision)) = map.next_entry::<String, Disposition>()? {
                if store.len() >= MAX_FINDINGS || store.insert(identity, decision).is_some() {
                    return Err(serde::de::Error::custom(
                        "duplicate or excessive dispositions",
                    ));
                }
            }
            Ok(store)
        }
    }
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let store =
        serde::de::Deserializer::deserialize_map(&mut decoder, UniqueStore).map_err(|_| {
            scan_error("corrupt disposition store; existing decisions were not changed")
        })?;
    decoder
        .end()
        .map_err(|_| scan_error("trailing data in disposition store"))?;
    validate_store(&store)?;
    Ok(store)
}

fn validate_store(store: &BTreeMap<String, Disposition>) -> Result<()> {
    if store.len() > MAX_FINDINGS {
        return Err(scan_error("disposition store exceeds 10000 entries"));
    }
    for (identity, entry) in store {
        if !super::lowercase_hex(identity, 16)
            || !matches!(
                entry.status.as_str(),
                "false-positive" | "accepted" | "fixed"
            )
            || entry.reason.trim().is_empty()
            || entry.reason.len() > 2048
        {
            return Err(scan_error(
                "invalid disposition identity, status or bounded reason",
            ));
        }
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
mod platform {
    use super::*;
    use rustix::fd::OwnedFd;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use std::fs::File;
    use std::io::{Read as _, Write as _};

    pub(super) struct Root {
        directory: OwnedFd,
    }

    impl Root {
        pub(super) fn open(path: &Path) -> Result<Self> {
            check_owner()?;
            // Resolve a trusted host-selected workspace once. All untrusted
            // paths below it use no-follow descriptor-relative operations.
            let path = std::fs::canonicalize(path)
                .map_err(|_| scan_error("cannot resolve source report workspace"))?;
            let directory = rustix::fs::open(
                &path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|_| scan_error("cannot pin source report workspace"))?;
            Ok(Self { directory })
        }

        fn parent(&self, path: &Path, create: bool) -> std::io::Result<OwnedFd> {
            let mut directory = rustix::fs::openat(
                &self.directory,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )?;
            if let Some(parent) = path.parent() {
                for part in parent.components() {
                    let Component::Normal(name) = part else {
                        return Err(std::io::Error::other("invalid relative parent"));
                    };
                    let flags =
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
                    let next = match rustix::fs::openat(&directory, name, flags, Mode::empty()) {
                        Ok(next) => next,
                        Err(rustix::io::Errno::NOENT) if create => {
                            match rustix::fs::mkdirat(
                                &directory,
                                name,
                                Mode::from_bits_truncate(0o700),
                            ) {
                                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                                Err(error) => return Err(error.into()),
                            }
                            rustix::fs::openat(&directory, name, flags, Mode::empty())?
                        }
                        Err(error) => return Err(error.into()),
                    };
                    directory = next;
                }
            }
            Ok(directory)
        }

        pub(super) fn read(&self, raw: &str, limit: u64) -> Result<Option<Vec<u8>>> {
            check_owner()?;
            let path = relative_file(raw)?;
            let parent = match self.parent(&path, false) {
                Ok(parent) => parent,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(_) => {
                    return Err(scan_error(
                        "cannot open report parent without following links",
                    ));
                }
            };
            let file = match rustix::fs::openat(
                &parent,
                path.file_name().expect("validated file name"),
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            ) {
                Ok(file) => File::from(file),
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(_) => return Err(scan_error("cannot open report without following links")),
            };
            let metadata = file
                .metadata()
                .map_err(|_| scan_error("cannot inspect report"))?;
            if !metadata.is_file() || metadata.len() > limit {
                return Err(scan_error("report must be a bounded regular file"));
            }
            let mut bytes = Vec::new();
            file.take(limit + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| scan_error("report read failed"))?;
            if bytes.len() as u64 > limit {
                return Err(scan_error("report grew beyond its byte budget"));
            }
            check_owner()?;
            Ok(Some(bytes))
        }

        // Only Pi-owned state permits replacement. Caller-selected exports
        // are always create-only, including when another file wins a race.
        pub(super) fn publish(&self, raw: &str, bytes: &[u8], replace: bool) -> Result<()> {
            check_owner()?;
            let path = relative_file(raw)?;
            if bytes.len() as u64 > MAX_SOURCE_BYTES
                || replace && raw != DEFAULT_REPORT && raw != "security-dispositions.json"
            {
                return Err(scan_error("invalid source persistence request"));
            }
            let parent = self
                .parent(&path, true)
                .map_err(|_| scan_error("cannot pin report parent without following links"))?;
            let name = path.file_name().expect("validated file name");
            if replace {
                match rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW) {
                    Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {}
                    Err(rustix::io::Errno::NOENT) => {}
                    _ => return Err(scan_error("refusing to replace non-regular source state")),
                }
            }
            let staging = format!(".pi-source-{}.tmp", uuid::Uuid::new_v4());
            let descriptor = rustix::fs::openat(
                &parent,
                staging.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(|_| scan_error("cannot create private source staging file"))?;
            let _stage = Stage {
                parent: &parent,
                name: &staging,
            };
            let mut file = File::from(descriptor);
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|_| {
                    scan_error("source staging write/sync failed; destination unchanged")
                })?;
            super::super::operation::publish(|| {
                let result = if replace {
                    rustix::fs::renameat(&parent, staging.as_str(), &parent, name)
                } else {
                    rustix::fs::linkat(&parent, staging.as_str(), &parent, name, AtFlags::empty())
                };
                result.map_err(|_| {
                    scan_error("source report publication failed; no file was truncated")
                })
            })
        }

        // Persistent lock inode: never unlink after use. Replacing/removing a
        // lock file would let two cooperating processes hold different locks.
        pub(super) fn lock_store(&self) -> Result<(Self, File)> {
            check_owner()?;
            let path = Path::new(".pi/security-dispositions.lock");
            let parent = self
                .parent(path, true)
                .map_err(|_| scan_error("cannot pin disposition directory"))?;
            let descriptor = rustix::fs::openat(
                &parent,
                "security-dispositions.lock",
                OFlags::RDWR
                    | OFlags::CREATE
                    | OFlags::CLOEXEC
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(|_| scan_error("cannot open disposition lock"))?;
            let file = File::from(descriptor);
            if !file
                .metadata()
                .map_err(|_| scan_error("cannot inspect disposition lock"))?
                .is_file()
            {
                return Err(scan_error("disposition lock must be a regular file"));
            }
            file.try_lock().map_err(|_| {
                scan_error("disposition store is busy or cannot be locked; retry the operation")
            })?;
            check_owner()?;
            // Keep this .pi identity for the entire read/modify/write, not
            // just the lock open. A renamed parent cannot switch maps midway.
            Ok((Self { directory: parent }, file))
        }
    }

    struct Stage<'a> {
        parent: &'a OwnedFd,
        name: &'a str,
    }
    impl Drop for Stage<'_> {
        fn drop(&mut self) {
            let _ = rustix::fs::unlinkat(self.parent, self.name, AtFlags::empty());
        }
    }
}

#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
mod platform {
    use super::*;
    pub(super) struct Root;
    fn unsupported<T>() -> Result<T> {
        Err(scan_error(
            "confined source persistence requires supported Unix descriptor APIs",
        ))
    }
    #[allow(clippy::unused_self)]
    impl Root {
        pub(super) fn open(_path: &Path) -> Result<Self> {
            unsupported()
        }
        pub(super) fn read(&self, _path: &str, _limit: u64) -> Result<Option<Vec<u8>>> {
            unsupported()
        }
        pub(super) fn publish(&self, _path: &str, _bytes: &[u8], _replace: bool) -> Result<()> {
            unsupported()
        }
        pub(super) fn lock_store(&self) -> Result<(Self, std::fs::File)> {
            unsupported()
        }
    }
}

pub(super) struct Storage(platform::Root);
impl Storage {
    pub(super) fn open(path: &Path) -> Result<Self> {
        Ok(Self(platform::Root::open(path)?))
    }

    pub(super) fn load(&self) -> Result<BTreeMap<String, Disposition>> {
        self.0
            .read(DISPOSITIONS, STORE_LIMIT)?
            .map_or_else(|| Ok(BTreeMap::new()), |bytes| decode_store(&bytes))
    }

    pub(super) fn save(&self, store: &BTreeMap<String, Disposition>) -> Result<()> {
        validate_store(store)?;
        let (root, _lock) = self.0.lock_store()?;
        // Never replace a corrupt persisted map with an apparently valid one.
        if let Some(bytes) = root.read("security-dispositions.json", STORE_LIMIT)? {
            decode_store(&bytes)?;
        }
        Self::write_store(&root, store)
    }

    fn write_store(root: &platform::Root, store: &BTreeMap<String, Disposition>) -> Result<()> {
        validate_store(store)?;
        let bytes = serde_json::to_vec_pretty(store)
            .map_err(|_| scan_error("cannot encode disposition store"))?;
        if bytes.len() as u64 > STORE_LIMIT {
            return Err(scan_error("disposition store exceeds 4 MiB"));
        }
        root.publish("security-dispositions.json", &bytes, true)
    }

    pub(super) fn update(&self, identity: String, entry: Disposition) -> Result<usize> {
        let (root, _lock) = self.0.lock_store()?;
        let mut store = match root.read("security-dispositions.json", STORE_LIMIT)? {
            Some(bytes) => decode_store(&bytes)?,
            None => BTreeMap::new(),
        };
        store.insert(identity, entry);
        Self::write_store(&root, &store)?;
        Ok(store.len())
    }

    pub(super) fn read_report(&self, raw: &str) -> Result<Vec<u8>> {
        let path = report_path(raw)?;
        self.0
            .read(path.to_str().expect("UTF-8 request"), MAX_SOURCE_BYTES)?
            .ok_or_else(|| scan_error("source baseline is missing"))
    }

    pub(super) fn write_report(&self, selected: Option<&str>, bytes: &[u8]) -> Result<()> {
        let path = report_path(selected.unwrap_or(DEFAULT_REPORT))?;
        let raw = path.to_str().expect("UTF-8 request");
        self.0.publish(raw, bytes, selected.is_none())
    }
}

#[cfg(test)]
mod tests;
