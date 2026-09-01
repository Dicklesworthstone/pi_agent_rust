//! MCP server trust lifecycle (bd-cv653.6.1).
//!
//! Server processes are capability-equivalent to `exec`: a configured server
//! never spawns until an operator explicitly acknowledges it. States:
//! `pending` (never acknowledged) → `acknowledged` (may spawn) and `denied`
//! (never spawn, fail-closed). Every transition is audit-logged with
//! operator provenance, and a trust decision binds to the server's
//! fingerprint — changing any execution-relevant configuration re-pends the
//! server.
//!
//! v1 acknowledgement surface: `/mcp trust <name>` (explicit command beats a
//! modal while the TUI stack is mid-migration). Executing a pending server's
//! tool returns a typed `[MCP_TRUST_PENDING]` refusal naming the remedy.

use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// Trust record format version.
const TRUST_SCHEMA_VERSION: u32 = 2;
const TRUST_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TRUST_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// One server's trust state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    /// Explicitly reset or invalidated by a schema migration; may not spawn.
    Pending,
    /// Explicitly acknowledged by the operator; may spawn.
    Acknowledged,
    /// Explicitly denied; never spawns (fail-closed).
    Denied,
}

/// An audit entry for one transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustAuditEntry {
    /// ISO-8601 timestamp.
    pub at: String,
    /// `acknowledged` | `denied` | `reset`.
    pub action: String,
    /// Who acted (`operator` for the local CLI user).
    pub by: String,
    /// Fingerprint the action applied to.
    pub fingerprint: String,
}

/// One server's persisted record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustRecord {
    pub state: TrustState,
    /// Fingerprint of the spawn target when the decision was made.
    pub fingerprint: String,
    pub by: String,
    pub at: String,
    /// bd-sp5o3: canonical identity of the resolved executable bound when
    /// the operator acknowledged. Legacy records carry None and are treated
    /// as fail-closed "missing binding" at the pre-spawn seam.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<super::config::StoredExecutionIdentity>,
    #[serde(default)]
    pub audit: Vec<TrustAuditEntry>,
}

/// On-disk store shape.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TrustFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    servers: HashMap<String, TrustRecord>,
}

/// The trust store (file-backed, line of truth for spawns).
#[derive(Debug)]
pub struct TrustStore {
    path: PathBuf,
    schema_version: u32,
    servers: HashMap<String, TrustRecord>,
}

#[cfg(unix)]
#[derive(Debug)]
pub(crate) struct TrustWriteGuard {
    directory: File,
    target_name: OsString,
    parent_path: PathBuf,
    _global_lock: crate::file_lock::DirLockAt,
    _lock: crate::file_lock::DirLockAt,
}

#[cfg(windows)]
#[derive(Debug)]
pub(crate) struct TrustWriteGuard {
    _directories: Vec<WindowsTrustDirectoryGuard>,
    _lock: crate::file_lock::DirLock,
}

#[cfg(all(not(unix), not(windows)))]
pub(crate) type TrustWriteGuard = crate::file_lock::DirLock;

#[cfg(windows)]
#[derive(Debug)]
struct WindowsTrustDirectoryGuard {
    path: PathBuf,
    identity: (u32, u64),
    handle: std::fs::File,
}

#[cfg(unix)]
#[derive(Debug)]
struct TrustTempFile {
    file: File,
    directory: File,
    name: OsString,
    persisted: bool,
}

#[cfg(unix)]
impl TrustTempFile {
    fn persist_to(&mut self, target_name: &OsStr) -> std::io::Result<()> {
        rustix::fs::renameat(&self.directory, &self.name, &self.directory, target_name)
            .map_err(std::io::Error::from)?;
        self.persisted = true;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for TrustTempFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = rustix::fs::unlinkat(&self.directory, &self.name, rustix::fs::AtFlags::empty());
        }
    }
}

/// The effective trust decision for a server right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustDecision {
    /// May spawn.
    Acknowledged,
    /// Never recorded, or the fingerprint changed since the decision.
    Pending,
    /// Explicitly denied.
    Denied,
}

#[cfg(unix)]
fn trust_target_parts(path: &Path) -> std::io::Result<(&Path, &OsStr)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let target_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("MCP trust path has no filename: {}", path.display()),
        )
    })?;
    Ok((parent, target_name))
}

#[cfg(unix)]
fn open_trust_directory_nofollow(path: &Path, create: bool) -> std::io::Result<File> {
    use std::path::Component;

    let flags = rustix::fs::OFlags::RDONLY
        | rustix::fs::OFlags::DIRECTORY
        | rustix::fs::OFlags::NOFOLLOW
        | rustix::fs::OFlags::CLOEXEC;
    let descriptor = rustix::fs::open(
        if path.is_absolute() { "/" } else { "." },
        flags,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let mut directory = File::from(descriptor);
    let mut pending = std::collections::VecDeque::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => pending.push_back(name.to_os_string()),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "secure MCP trust paths must not contain parent or prefix components: {}",
                        path.display()
                    ),
                ));
            }
        }
    }

    let mut symlink_budget: u8 = 8;
    while let Some(name) = pending.pop_front() {
        let child = match rustix::fs::openat(&directory, &name, flags, rustix::fs::Mode::empty()) {
            Ok(child) => child,
            Err(rustix::io::Errno::NOENT) if create => {
                match rustix::fs::mkdirat(
                    &directory,
                    &name,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
                ) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(std::io::Error::from(error)),
                }
                rustix::fs::openat(&directory, &name, flags, rustix::fs::Mode::empty())
                    .map_err(std::io::Error::from)?
            }
            Err(errno @ (rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR)) => {
                let Some((components, absolute)) =
                    crate::platform::read_trusted_symlink_component(&directory, &name)
                else {
                    return Err(std::io::Error::from(errno));
                };
                symlink_budget = symlink_budget
                    .checked_sub(1)
                    .ok_or_else(|| std::io::Error::from(rustix::io::Errno::LOOP))?;
                for component in components.into_iter().rev() {
                    pending.push_front(component);
                }
                if absolute {
                    directory = File::from(
                        rustix::fs::open("/", flags, rustix::fs::Mode::empty())
                            .map_err(std::io::Error::from)?,
                    );
                }
                continue;
            }
            Err(error) => return Err(std::io::Error::from(error)),
        };
        directory = File::from(child);
    }
    Ok(directory)
}

#[cfg(unix)]
fn trust_parent_identity_matches(path: &Path, expected: &File) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    let (parent, _) = trust_target_parts(path)?;
    let current = open_trust_directory_nofollow(parent, false)?;
    let expected_metadata = expected.metadata()?;
    let current_metadata = current.metadata()?;
    Ok(expected_metadata.dev() == current_metadata.dev()
        && expected_metadata.ino() == current_metadata.ino())
}

#[cfg(unix)]
fn read_trust_file_at(
    directory: &File,
    target_name: &OsStr,
    path: &Path,
) -> std::io::Result<Option<String>> {
    let descriptor = match rustix::fs::openat(
        directory,
        target_name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "MCP trust file must be a regular non-link file: {}",
                    path.display()
                ),
            ));
        }
        Err(error) => return Err(std::io::Error::from(error)),
    };
    let file = File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "MCP trust file must be a regular non-link file: {}",
                path.display()
            ),
        ));
    }
    if metadata.len() > MAX_TRUST_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "MCP trust file exceeds the {MAX_TRUST_FILE_BYTES}-byte limit: {}",
                path.display()
            ),
        ));
    }
    let mut content = String::new();
    file.take(MAX_TRUST_FILE_BYTES + 1)
        .read_to_string(&mut content)?;
    if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_TRUST_FILE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "MCP trust file exceeds the {MAX_TRUST_FILE_BYTES}-byte limit: {}",
                path.display()
            ),
        ));
    }
    Ok(Some(content))
}

#[cfg(unix)]
fn create_trust_temp_file(directory: &File) -> std::io::Result<TrustTempFile> {
    let owned_directory = directory.try_clone()?;
    for _ in 0..16 {
        let name = OsString::from(format!(".mcp-trust.tmp-{}", uuid::Uuid::new_v4().simple()));
        match rustix::fs::openat(
            directory,
            &name,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        ) {
            Ok(descriptor) => {
                return Ok(TrustTempFile {
                    file: File::from(descriptor),
                    directory: owned_directory,
                    name,
                    persisted: false,
                });
            }
            Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(std::io::Error::from(error)),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique MCP trust temporary file",
    ))
}

#[cfg(unix)]
fn acquire_global_trust_lock(path: &Path) -> std::io::Result<crate::file_lock::DirLockAt> {
    acquire_global_trust_lock_for(path, TRUST_LOCK_TIMEOUT)
}

#[cfg(unix)]
fn stable_trust_lock_path(path: &Path) -> std::io::Result<PathBuf> {
    let (parent, target_name) = trust_target_parts(path)?;
    let mut existing = parent.to_path_buf();
    let mut missing = Vec::new();
    let canonical_parent = loop {
        match std::fs::canonicalize(&existing) {
            Ok(canonical) => break canonical,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!(
                            "cannot find an existing ancestor for MCP trust path {}",
                            path.display()
                        ),
                    )
                })?;
                missing.push(name.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!(
                                "cannot find an existing ancestor for MCP trust path {}",
                                path.display()
                            ),
                        )
                    })?
                    .to_path_buf();
            }
            Err(err) => return Err(err),
        }
    };
    let mut normalized = canonical_parent;
    for component in missing.into_iter().rev() {
        normalized.push(component);
    }
    normalized.push(target_name);
    Ok(normalized)
}

#[cfg(unix)]
pub(crate) fn acquire_global_trust_lock_for(
    path: &Path,
    timeout: Duration,
) -> std::io::Result<crate::file_lock::DirLockAt> {
    use std::os::unix::fs::MetadataExt as _;

    let euid = rustix::process::geteuid().as_raw();
    let lock_root = PathBuf::from(format!("/tmp/pi-agent-rust-mcp-trust-locks-{euid}"));
    let directory = open_trust_directory_nofollow(&lock_root, true)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != euid || metadata.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "the UID-scoped MCP trust lock directory is not private and owner-controlled",
        ));
    }

    let stable_path = stable_trust_lock_path(path)?;
    let mut digest = Sha256::new();
    digest.update(b"pi_agent_rust:mcp-global-trust-lock:v1\0");
    digest.update(stable_path.as_os_str().as_encoded_bytes());
    let target_name = OsString::from(format!(
        "trust-{}",
        crate::package_manager::hex_encode(&digest.finalize())
    ));
    crate::file_lock::DirLockAt::acquire_for(&directory, &target_name, timeout)
}

#[cfg(windows)]
fn windows_metadata_is_reparse(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(windows)]
fn reject_windows_reparse_components(path: &Path) -> std::io::Result<()> {
    for component in path.ancestors() {
        match std::fs::symlink_metadata(component) {
            Ok(metadata) if windows_metadata_is_reparse(&metadata) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "MCP trust path must not traverse a Windows reparse point: {}",
                        component.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn windows_file_identity(metadata: &std::fs::Metadata) -> std::io::Result<(u32, u64)> {
    use std::os::windows::fs::MetadataExt as _;

    metadata
        .volume_serial_number()
        .zip(metadata.file_index())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Windows did not expose a stable MCP trust file identity",
            )
        })
}

#[cfg(windows)]
fn validate_windows_trust_directory_guard(
    guard: &WindowsTrustDirectoryGuard,
) -> std::io::Result<()> {
    let handle_metadata = guard.handle.metadata()?;
    let path_metadata = std::fs::symlink_metadata(&guard.path)?;
    if !handle_metadata.is_dir()
        || !path_metadata.is_dir()
        || windows_metadata_is_reparse(&handle_metadata)
        || windows_metadata_is_reparse(&path_metadata)
        || windows_file_identity(&handle_metadata)? != guard.identity
        || windows_file_identity(&path_metadata)? != guard.identity
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "MCP trust directory changed while pinned: {}",
                guard.path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_trust_directory_guards(
    guards: &[WindowsTrustDirectoryGuard],
) -> std::io::Result<()> {
    for guard in guards {
        validate_windows_trust_directory_guard(guard)?;
    }
    Ok(())
}

#[cfg(windows)]
fn open_or_create_windows_trust_parent(
    path: &Path,
    create: bool,
) -> std::io::Result<(PathBuf, Vec<WindowsTrustDirectoryGuard>)> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::path::Component;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;

    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute_path.parent().unwrap_or_else(|| Path::new("."));
    let mut current = PathBuf::new();
    let mut guards = Vec::new();
    for component in parent.components() {
        match component {
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                continue;
            }
            Component::RootDir => {
                current.push(component.as_os_str());
                continue;
            }
            Component::CurDir => continue,
            Component::ParentDir => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "secure MCP trust paths must not contain parent components: {}",
                        path.display()
                    ),
                ));
            }
            Component::Normal(name) => current.push(name),
        }

        let initial_metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && create => {
                match std::fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(err) => return Err(err),
                }
                std::fs::symlink_metadata(&current)?
            }
            Err(err) => return Err(err),
        };
        if !initial_metadata.is_dir() || windows_metadata_is_reparse(&initial_metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "MCP trust path must contain only ordinary directories: {}",
                    current.display()
                ),
            ));
        }
        let identity = windows_file_identity(&initial_metadata)?;
        // Omitting FILE_SHARE_DELETE pins this component against rename or
        // replacement for the lifetime of the guard.
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&current)?;
        let opened_metadata = handle.metadata()?;
        if !opened_metadata.is_dir()
            || windows_metadata_is_reparse(&opened_metadata)
            || windows_file_identity(&opened_metadata)? != identity
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "MCP trust directory changed while opening: {}",
                    current.display()
                ),
            ));
        }
        guards.push(WindowsTrustDirectoryGuard {
            path: current.clone(),
            identity,
            handle,
        });
    }
    validate_windows_trust_directory_guards(&guards)?;
    Ok((absolute_path, guards))
}

impl TrustStore {
    /// Load from `path` (absent file → empty store; malformed → error,
    /// fail-closed).
    ///
    /// # Errors
    ///
    /// Returns an error when the file exists but cannot be parsed.
    pub fn load(path: &Path) -> Result<Self> {
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|err| {
                    Error::tool(
                        "mcp",
                        format!("[MCP_TRUST_IO] cannot resolve current directory: {err}"),
                    )
                })?
                .join(path)
        };
        let content = Self::read_content(&path)?;
        let (schema_version, servers) = Self::parse_content(&path, content.as_deref())?;
        Ok(Self {
            path,
            schema_version,
            servers,
        })
    }

    fn parse_content(
        path: &Path,
        content: Option<&str>,
    ) -> Result<(u32, HashMap<String, TrustRecord>)> {
        let Some(content) = content else {
            return Ok((TRUST_SCHEMA_VERSION, HashMap::new()));
        };
        let file: TrustFile = serde_json::from_str(content).map_err(|err| {
            Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_CORRUPT] {} is not valid: {err}; \
                     move it aside to reset all MCP trust decisions",
                    path.display()
                ),
            )
        })?;
        Ok((file.version, file.servers))
    }

    #[cfg(unix)]
    fn read_content(path: &Path) -> Result<Option<String>> {
        let (parent, target_name) = trust_target_parts(path).map_err(|err| {
            Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] invalid trust path {}: {err}",
                    path.display()
                ),
            )
        })?;
        let directory = match open_trust_directory_nofollow(parent, false) {
            Ok(directory) => directory,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot open {}: {err}", parent.display()),
                ));
            }
        };
        let content = read_trust_file_at(&directory, target_name, path).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] cannot read {}: {err}", path.display()),
            )
        })?;
        match trust_parent_identity_matches(path, &directory) {
            Ok(true) => Ok(content),
            Ok(false) => Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust directory changed while reading {}",
                    path.display()
                ),
            )),
            Err(err) => Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] cannot revalidate trust directory after reading {}: {err}",
                    path.display()
                ),
            )),
        }
    }

    #[cfg(windows)]
    fn read_content(path: &Path) -> Result<Option<String>> {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let (operation_path, parent_guards) = match open_or_create_windows_trust_parent(path, false)
        {
            Ok(result) => result,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::tool(
                    "mcp",
                    format!(
                        "[MCP_TRUST_IO] cannot pin trust parent for {}: {err}",
                        path.display()
                    ),
                ));
            }
        };
        let path = operation_path.as_path();
        reject_windows_reparse_components(path).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] cannot secure {}: {err}", path.display()),
            )
        })?;
        let path_metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot inspect {}: {err}", path.display()),
                ));
            }
        };
        if windows_metadata_is_reparse(&path_metadata) || !path_metadata.is_file() {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust file must be a regular non-reparse file: {}",
                    path.display()
                ),
            ));
        }
        if path_metadata.len() > MAX_TRUST_FILE_BYTES {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust file exceeds the {MAX_TRUST_FILE_BYTES}-byte limit: {}",
                    path.display()
                ),
            ));
        }
        let expected_identity = windows_file_identity(&path_metadata).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] cannot identify {}: {err}", path.display()),
            )
        })?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|err| {
                Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot open {}: {err}", path.display()),
                )
            })?;
        let opened_metadata = file.metadata().map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] cannot inspect {}: {err}", path.display()),
            )
        })?;
        if !opened_metadata.is_file()
            || windows_metadata_is_reparse(&opened_metadata)
            || opened_metadata.len() > MAX_TRUST_FILE_BYTES
            || windows_file_identity(&opened_metadata).map_err(|err| {
                Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot identify {}: {err}", path.display()),
                )
            })? != expected_identity
        {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust file changed while opening or is unsafe: {}",
                    path.display()
                ),
            ));
        }
        let mut content = String::new();
        file.take(MAX_TRUST_FILE_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|err| {
                Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot read {}: {err}", path.display()),
                )
            })?;
        if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_TRUST_FILE_BYTES {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust file exceeds the {MAX_TRUST_FILE_BYTES}-byte limit: {}",
                    path.display()
                ),
            ));
        }
        reject_windows_reparse_components(path).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] trust path changed while reading: {err}"),
            )
        })?;
        let current_metadata = std::fs::symlink_metadata(path).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] trust file changed while reading: {err}"),
            )
        })?;
        if windows_metadata_is_reparse(&current_metadata)
            || windows_file_identity(&current_metadata).map_err(|err| {
                Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot re-identify trust file: {err}"),
                )
            })? != expected_identity
        {
            return Err(Error::tool(
                "mcp",
                "[MCP_TRUST_IO] trust file was replaced while reading",
            ));
        }
        validate_windows_trust_directory_guards(&parent_guards).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] trust parent changed while reading: {err}"),
            )
        })?;
        Ok(Some(content))
    }

    #[cfg(all(not(unix), not(windows)))]
    fn read_content(path: &Path) -> Result<Option<String>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot inspect {}: {err}", path.display()),
                ));
            }
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > MAX_TRUST_FILE_BYTES
        {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust file must be a bounded regular non-link file: {}",
                    path.display()
                ),
            ));
        }
        let file = std::fs::File::open(path).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] cannot open {}: {err}", path.display()),
            )
        })?;
        let opened_metadata = file.metadata().map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] cannot inspect {}: {err}", path.display()),
            )
        })?;
        if !opened_metadata.is_file() || opened_metadata.len() > MAX_TRUST_FILE_BYTES {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust file became unsafe: {}",
                    path.display()
                ),
            ));
        }
        let mut content = String::new();
        file.take(MAX_TRUST_FILE_BYTES + 1)
            .read_to_string(&mut content)
            .map_err(|err| {
                Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot read {}: {err}", path.display()),
                )
            })?;
        if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_TRUST_FILE_BYTES {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust file exceeds the {MAX_TRUST_FILE_BYTES}-byte limit: {}",
                    path.display()
                ),
            ));
        }
        Ok(Some(content))
    }

    /// The decision for `name` running `fingerprint` right now.
    #[must_use]
    pub fn decision(&self, name: &str, fingerprint: &str) -> TrustDecision {
        if self.schema_version != TRUST_SCHEMA_VERSION {
            return TrustDecision::Pending;
        }
        match self.servers.get(name) {
            Some(record) if record.fingerprint == fingerprint => match record.state {
                TrustState::Pending => TrustDecision::Pending,
                TrustState::Acknowledged => TrustDecision::Acknowledged,
                TrustState::Denied => TrustDecision::Denied,
            },
            // Missing record or a fingerprint change (config edited) → pending.
            _ => TrustDecision::Pending,
        }
    }

    /// Lock and reload the durable store, then return the decision together
    /// with the held cross-process guard. Keeping the guard alive makes a
    /// trust-gated local side effect linearizable with acknowledge/deny/reset:
    /// a transition either precedes the effect and blocks it, or follows it.
    pub(crate) fn locked_decision(
        &mut self,
        name: &str,
        fingerprint: &str,
    ) -> Result<(TrustDecision, TrustWriteGuard)> {
        let guard = self.lock_and_reload()?;
        Ok((self.decision(name, fingerprint), guard))
    }

    /// Record an acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub fn acknowledge(&mut self, name: &str, fingerprint: &str, by: &str) -> Result<()> {
        self.transition(
            name,
            fingerprint,
            TrustState::Acknowledged,
            by,
            "acknowledged",
        )
    }

    /// Record a denial (fail-closed; never spawns until reset).
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub fn deny(&mut self, name: &str, fingerprint: &str, by: &str) -> Result<()> {
        self.transition(name, fingerprint, TrustState::Denied, by, "denied")
    }

    /// Forget a server (re-pends it on next use).
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub fn reset(&mut self, name: &str, by: &str) -> Result<()> {
        let file_guard = self.lock_and_reload()?;
        self.migrate_schema_if_needed();
        if let Some(record) = self.servers.get_mut(name) {
            let at = now_iso();
            record.audit.push(TrustAuditEntry {
                at: at.clone(),
                action: "reset".to_string(),
                by: by.to_string(),
                fingerprint: record.fingerprint.clone(),
            });
            record.state = TrustState::Pending;
            record.by = by.to_string();
            record.at = at;
            // Reset also drops any stale execution binding: a re-ack must
            // re-derive it against the CURRENT PATH/contents (bd-sp5o3).
            record.execution = None;
        }
        self.save(&file_guard)
    }

    fn transition(
        &mut self,
        name: &str,
        fingerprint: &str,
        state: TrustState,
        by: &str,
        action: &str,
    ) -> Result<()> {
        self.transition_execution(name, fingerprint, state, by, action, None)
    }

    /// Shared state machine with an optional execution-identity binding
    /// (bd-sp5o3): only acknowledgements carry one.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    fn transition_execution(
        &mut self,
        name: &str,
        fingerprint: &str,
        state: TrustState,
        by: &str,
        action: &str,
        execution: Option<super::config::StoredExecutionIdentity>,
    ) -> Result<()> {
        let file_guard = self.lock_and_reload()?;
        self.migrate_schema_if_needed();
        let at = now_iso();
        let audit = TrustAuditEntry {
            at: at.clone(),
            action: action.to_string(),
            by: by.to_string(),
            fingerprint: fingerprint.to_string(),
        };
        let record = self
            .servers
            .entry(name.to_string())
            .or_insert_with(|| TrustRecord {
                state,
                fingerprint: fingerprint.to_string(),
                by: by.to_string(),
                at: at.clone(),
                execution: None,
                audit: Vec::new(),
            });
        record.state = state;
        record.fingerprint = fingerprint.to_string();
        record.by = by.to_string();
        record.at = at;
        if execution.is_some() {
            record.execution = execution;
        } else if !matches!(state, TrustState::Acknowledged) {
            // Denials keep whatever binding existed (they block regardless);
            // pending transitions drop stale bindings.
            if matches!(state, TrustState::Pending) {
                record.execution = None;
            }
        }
        record.audit.push(audit);
        self.save(&file_guard)
    }

    /// Record an acknowledgement together with the canonical execution
    /// identity that was shown to and approved by the operator (bd-sp5o3).
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub fn acknowledge_execution(
        &mut self,
        name: &str,
        fingerprint: &str,
        by: &str,
        execution: super::config::StoredExecutionIdentity,
    ) -> Result<()> {
        self.transition_execution(
            name,
            fingerprint,
            TrustState::Acknowledged,
            by,
            "acknowledged",
            Some(execution),
        )
    }

    /// The stored binding for an ACKNOWLEDGED record matching this exact
    /// fingerprint; anything else yields `None` (fail-closed upstream).
    #[must_use]
    pub fn acknowledged_execution(
        &self,
        name: &str,
        fingerprint: &str,
    ) -> Option<super::config::StoredExecutionIdentity> {
        if self.schema_version != TRUST_SCHEMA_VERSION {
            return None;
        }
        match self.servers.get(name) {
            Some(record)
                if record.fingerprint == fingerprint
                    && record.state == TrustState::Acknowledged =>
            {
                record.execution.clone()
            }
            _ => None,
        }
    }

    #[cfg(unix)]
    fn lock_and_reload(&mut self) -> Result<TrustWriteGuard> {
        // This path-keyed lock lives under a UID-owned directory rooted in
        // stable `/tmp`, so all processes retain one lock domain even if the
        // trust file's own parent directory is concurrently renamed.
        let global_lock = acquire_global_trust_lock(&self.path).map_err(|err| {
            Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] cannot acquire stable trust lock for {}: {err}",
                    self.path.display()
                ),
            )
        })?;
        let (parent, target_name) = trust_target_parts(&self.path).map_err(|err| {
            Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] invalid trust path {}: {err}",
                    self.path.display()
                ),
            )
        })?;
        let parent_path = parent.to_path_buf();
        let target_name = target_name.to_os_string();
        let directory = open_trust_directory_nofollow(&parent_path, true).map_err(|err| {
            Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] cannot securely create or open {}: {err}",
                    parent_path.display()
                ),
            )
        })?;
        let lock =
            crate::file_lock::DirLockAt::acquire_for(&directory, &target_name, TRUST_LOCK_TIMEOUT)
                .map_err(|err| {
                    Error::tool(
                        "mcp",
                        format!("[MCP_TRUST_IO] cannot lock {}: {err}", self.path.display()),
                    )
                })?;
        self.ensure_parent_unchanged(&directory, "after acquiring the trust lock")?;
        let content = read_trust_file_at(&directory, &target_name, &self.path).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] cannot read {}: {err}", self.path.display()),
            )
        })?;
        self.ensure_parent_unchanged(&directory, "after reloading the trust file")?;
        let (schema_version, servers) = Self::parse_content(&self.path, content.as_deref())?;
        self.schema_version = schema_version;
        self.servers = servers;
        Ok(TrustWriteGuard {
            directory,
            target_name,
            parent_path,
            _global_lock: global_lock,
            _lock: lock,
        })
    }

    #[cfg(windows)]
    fn lock_and_reload(&mut self) -> Result<TrustWriteGuard> {
        let (operation_path, directories) = open_or_create_windows_trust_parent(&self.path, true)
            .map_err(|err| {
            Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] cannot pin trust parent for {}: {err}",
                    self.path.display()
                ),
            )
        })?;
        let lock = crate::file_lock::DirLock::acquire_for(&operation_path, TRUST_LOCK_TIMEOUT)
            .map_err(|err| {
                Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot lock {}: {err}", self.path.display()),
                )
            })?;
        validate_windows_trust_directory_guards(&directories).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] trust parent changed while locking: {err}"),
            )
        })?;
        let fresh = Self::load(&operation_path)?;
        validate_windows_trust_directory_guards(&directories).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] trust parent changed while reloading: {err}"),
            )
        })?;
        self.schema_version = fresh.schema_version;
        self.servers = fresh.servers;
        Ok(TrustWriteGuard {
            _directories: directories,
            _lock: lock,
        })
    }

    #[cfg(all(not(unix), not(windows)))]
    fn lock_and_reload(&mut self) -> Result<TrustWriteGuard> {
        Err(Error::tool(
            "mcp",
            format!(
                "[MCP_TRUST_IO] secure trust transitions are unsupported on this platform: {}",
                self.path.display()
            ),
        ))
    }

    #[cfg(unix)]
    fn ensure_parent_unchanged(&self, directory: &File, phase: &str) -> Result<()> {
        match trust_parent_identity_matches(&self.path, directory) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] trust directory changed {phase}: {}",
                    self.path.display()
                ),
            )),
            Err(err) => Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] cannot revalidate trust directory {phase} for {}: {err}",
                    self.path.display()
                ),
            )),
        }
    }

    fn migrate_schema_if_needed(&mut self) {
        if self.schema_version == TRUST_SCHEMA_VERSION {
            return;
        }
        let at = now_iso();
        let old_version = self.schema_version;
        for record in self.servers.values_mut() {
            record.audit.push(TrustAuditEntry {
                at: at.clone(),
                action: format!("schema_reset_v{old_version}"),
                by: "system".to_string(),
                fingerprint: record.fingerprint.clone(),
            });
            record.state = TrustState::Pending;
            record.by = "system".to_string();
            record.at.clone_from(&at);
        }
        self.schema_version = TRUST_SCHEMA_VERSION;
    }

    #[cfg(unix)]
    fn save(&self, guard: &TrustWriteGuard) -> Result<()> {
        self.save_with_before_persist(guard, || Ok(()))
    }

    #[cfg(unix)]
    fn save_with_before_persist<F>(&self, guard: &TrustWriteGuard, before_persist: F) -> Result<()>
    where
        F: FnOnce() -> std::io::Result<()>,
    {
        debug_assert_eq!(
            guard.parent_path.as_path(),
            self.path.parent().unwrap_or_else(|| Path::new("."))
        );
        self.ensure_parent_unchanged(&guard.directory, "before creating the trust temporary file")?;
        let file = TrustFile {
            version: TRUST_SCHEMA_VERSION,
            servers: self.servers.clone(),
        };
        let rendered = serde_json::to_string_pretty(&file)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] serialize failed: {err}")))?;
        if u64::try_from(rendered.len()).unwrap_or(u64::MAX) > MAX_TRUST_FILE_BYTES {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] serialized trust store exceeds the {MAX_TRUST_FILE_BYTES}-byte limit"
                ),
            ));
        }
        let mut temp = create_trust_temp_file(&guard.directory)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] temp file: {err}")))?;
        std::io::Write::write_all(&mut temp.file, rendered.as_bytes())
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] write: {err}")))?;
        temp.file
            .sync_all()
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] sync: {err}")))?;
        before_persist().map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] pre-persist verification failed: {err}"),
            )
        })?;
        self.ensure_parent_unchanged(&guard.directory, "before trust persistence")?;
        temp.persist_to(&guard.target_name)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] persist: {err}")))?;
        guard
            .directory
            .sync_all()
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] directory sync: {err}")))?;
        // `renameat` above is the commit point. A parent-path displacement
        // after that point cannot redirect the descriptor-relative write; a
        // later load through the displaced path remains fail-closed. Do not
        // report a failed transition after the durable commit has succeeded.
        match trust_parent_identity_matches(&self.path, &guard.directory) {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                event = "pi.mcp.trust_parent_displaced_after_commit",
                "MCP trust transition committed to the pinned directory after its path moved"
            ),
            Err(_) => tracing::warn!(
                event = "pi.mcp.trust_parent_revalidation_failed_after_commit",
                "MCP trust transition committed but its directory path could not be revalidated"
            ),
        }
        Ok(())
    }

    #[cfg(windows)]
    fn save(&self, guard: &TrustWriteGuard) -> Result<()> {
        validate_windows_trust_directory_guards(&guard._directories).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] trust parent changed before saving: {err}"),
            )
        })?;
        let file = TrustFile {
            version: TRUST_SCHEMA_VERSION,
            servers: self.servers.clone(),
        };
        let rendered = serde_json::to_string_pretty(&file)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] serialize failed: {err}")))?;
        if u64::try_from(rendered.len()).unwrap_or(u64::MAX) > MAX_TRUST_FILE_BYTES {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] serialized trust store exceeds the {MAX_TRUST_FILE_BYTES}-byte limit"
                ),
            ));
        }
        // Atomic write: temp + rename in the same directory.
        let mut temp =
            tempfile::NamedTempFile::new_in(self.path.parent().unwrap_or_else(|| Path::new(".")))
                .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] temp file: {err}")))?;
        std::io::Write::write_all(&mut temp, rendered.as_bytes())
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] write: {err}")))?;
        temp.as_file()
            .sync_all()
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] sync: {err}")))?;
        validate_windows_trust_directory_guards(&guard._directories).map_err(|err| {
            Error::tool(
                "mcp",
                format!("[MCP_TRUST_IO] trust parent changed before persistence: {err}"),
            )
        })?;
        temp.persist(&self.path)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] persist: {}", err.error)))?;
        if validate_windows_trust_directory_guards(&guard._directories).is_err() {
            tracing::warn!(
                event = "pi.mcp.trust_parent_revalidation_failed_after_commit",
                "MCP trust transition committed but its Windows parent handles could not be revalidated"
            );
        }
        Ok(())
    }

    #[cfg(all(not(unix), not(windows)))]
    fn save(&self, _guard: &TrustWriteGuard) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                Error::tool(
                    "mcp",
                    format!("[MCP_TRUST_IO] cannot create {}: {err}", parent.display()),
                )
            })?;
        }
        let file = TrustFile {
            version: TRUST_SCHEMA_VERSION,
            servers: self.servers.clone(),
        };
        let rendered = serde_json::to_string_pretty(&file)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] serialize failed: {err}")))?;
        if u64::try_from(rendered.len()).unwrap_or(u64::MAX) > MAX_TRUST_FILE_BYTES {
            return Err(Error::tool(
                "mcp",
                format!(
                    "[MCP_TRUST_IO] serialized trust store exceeds the {MAX_TRUST_FILE_BYTES}-byte limit"
                ),
            ));
        }
        let mut temp =
            tempfile::NamedTempFile::new_in(self.path.parent().unwrap_or_else(|| Path::new(".")))
                .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] temp file: {err}")))?;
        std::io::Write::write_all(&mut temp, rendered.as_bytes())
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] write: {err}")))?;
        temp.as_file()
            .sync_all()
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] sync: {err}")))?;
        temp.persist(&self.path)
            .map_err(|err| Error::tool("mcp", format!("[MCP_TRUST_IO] persist: {}", err.error)))?;
        Ok(())
    }

    /// Read-only view of all records (for `/mcp` listing).
    #[must_use]
    pub const fn records(&self) -> &HashMap<String, TrustRecord> {
        &self.servers
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_by_default_and_after_fingerprint_change() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut store = TrustStore::load(&path).expect("load");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Pending);

        store.acknowledge("srv", "fp1", "operator").expect("ack");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Acknowledged);
        // Config change re-pends.
        assert_eq!(store.decision("srv", "fp2"), TrustDecision::Pending);
    }

    #[test]
    fn denial_is_fail_closed_and_sticky() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut store = TrustStore::load(&path).expect("load");
        store.deny("srv", "fp1", "operator").expect("deny");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Denied);
        // Acknowledging over a denial is a fresh explicit act.
        store.acknowledge("srv", "fp1", "operator").expect("ack");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Acknowledged);
    }

    #[test]
    fn persistence_roundtrip_with_audit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        {
            let mut store = TrustStore::load(&path).expect("load");
            store.acknowledge("srv", "fp1", "operator").expect("ack");
            store.deny("srv", "fp2", "operator").expect("deny");
        }
        let store = TrustStore::load(&path).expect("reload");
        assert_eq!(store.decision("srv", "fp2"), TrustDecision::Denied);
        let record = &store.records()["srv"]; // ubs:ignore test index — presence is the assertion
        assert_eq!(record.audit.len(), 2);
        assert_eq!(record.audit[0].action, "acknowledged");
        assert_eq!(record.audit[1].action, "denied");
        assert!(record.audit.iter().all(|a| a.by == "operator"));
    }

    #[test]
    fn reset_re_pends() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut store = TrustStore::load(&path).expect("load");
        store.acknowledge("srv", "fp1", "operator").expect("ack");
        store.reset("srv", "operator").expect("reset");
        assert_eq!(store.decision("srv", "fp1"), TrustDecision::Pending);
        let reloaded = TrustStore::load(&path).expect("reload");
        let record = &reloaded.records()["srv"]; // ubs:ignore test index — reset preserves it
        assert_eq!(record.state, TrustState::Pending);
        assert_eq!(
            record.audit.last().map(|entry| entry.action.as_str()),
            Some("reset")
        );
    }

    #[test]
    fn corrupt_store_fails_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        std::fs::write(&path, "{not json").expect("write");
        let err = TrustStore::load(&path).expect_err("corrupt must fail");
        assert!(err.to_string().contains("MCP_TRUST_CORRUPT"), "{err}");
    }

    #[test]
    fn legacy_schema_re_pends_even_matching_fingerprint() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let fingerprint = "a".repeat(64);
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 1,
                "servers": {
                    "srv": {
                        "state": "acknowledged",
                        "fingerprint": fingerprint,
                        "by": "operator",
                        "at": "2026-08-26T00:00:00Z",
                        "audit": []
                    }
                }
            })
            .to_string(),
        )
        .expect("write legacy store");
        let store = TrustStore::load(&path).expect("load legacy store");
        assert_eq!(
            store.decision("srv", &"a".repeat(64)),
            TrustDecision::Pending
        );
    }

    #[test]
    fn legacy_records_stay_pending_after_unrelated_v2_transition() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let fingerprint = "a".repeat(64);
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 1,
                "servers": {
                    "legacy": {
                        "state": "acknowledged",
                        "fingerprint": fingerprint,
                        "by": "operator",
                        "at": "2026-08-26T00:00:00Z",
                        "audit": []
                    }
                }
            })
            .to_string(),
        )
        .expect("write legacy store");
        let mut store = TrustStore::load(&path).expect("load legacy store");
        store
            .acknowledge("current", &"b".repeat(64), "operator")
            .expect("write current transition");

        let reloaded = TrustStore::load(&path).expect("reload migrated store");
        assert_eq!(
            reloaded.decision("legacy", &"a".repeat(64)),
            TrustDecision::Pending
        );
        assert_eq!(reloaded.records()["legacy"].state, TrustState::Pending);
        assert!(
            reloaded.records()["legacy"]
                .audit
                .iter()
                .any(|entry| entry.action == "schema_reset_v1"),
            "migration must preserve a durable quarantine audit"
        );
    }

    #[test]
    fn stale_writer_cannot_resurrect_a_denied_record() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut initial = TrustStore::load(&path).expect("load initial store");
        initial
            .acknowledge("server-a", "fp-a", "operator")
            .expect("ack server A");

        let mut denying_writer = TrustStore::load(&path).expect("load denying writer");
        let mut stale_unrelated_writer = TrustStore::load(&path).expect("load stale writer");
        denying_writer
            .deny("server-a", "fp-a", "operator")
            .expect("deny server A");
        stale_unrelated_writer
            .acknowledge("server-b", "fp-b", "operator")
            .expect("ack server B from stale snapshot");

        let reloaded = TrustStore::load(&path).expect("reload final store");
        assert_eq!(reloaded.decision("server-a", "fp-a"), TrustDecision::Denied);
        assert_eq!(
            reloaded.decision("server-b", "fp-b"),
            TrustDecision::Acknowledged
        );
    }

    #[test]
    fn simultaneous_writers_preserve_both_transitions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut initial = TrustStore::load(&path).expect("load initial store");
        initial
            .acknowledge("server-a", "fp-a", "operator")
            .expect("ack server A");

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let denying_path = path.clone();
        let denying_barrier = std::sync::Arc::clone(&barrier);
        let denying = std::thread::spawn(move || {
            let mut store = TrustStore::load(&denying_path).expect("load denying writer");
            denying_barrier.wait();
            store
                .deny("server-a", "fp-a", "operator")
                .expect("deny server A");
        });
        let acknowledging_path = path.clone();
        let acknowledging_barrier = std::sync::Arc::clone(&barrier);
        let acknowledging = std::thread::spawn(move || {
            let mut store =
                TrustStore::load(&acknowledging_path).expect("load acknowledging writer");
            acknowledging_barrier.wait();
            store
                .acknowledge("server-b", "fp-b", "operator")
                .expect("ack server B");
        });
        barrier.wait();
        denying.join().expect("denying writer");
        acknowledging.join().expect("acknowledging writer");

        let reloaded = TrustStore::load(&path).expect("reload final store");
        assert_eq!(reloaded.decision("server-a", "fp-a"), TrustDecision::Denied);
        assert_eq!(
            reloaded.decision("server-b", "fp-b"),
            TrustDecision::Acknowledged
        );
    }

    #[test]
    fn held_execution_decision_serializes_a_concurrent_denial() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let mut initial = TrustStore::load(&path).expect("load initial store");
        initial
            .acknowledge("server", "fingerprint", "operator")
            .expect("acknowledge server");

        let mut executing = TrustStore::load(&path).expect("load executing reader");
        let (decision, execution_guard) = executing
            .locked_decision("server", "fingerprint")
            .expect("lock execution decision");
        assert_eq!(decision, TrustDecision::Acknowledged);

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (denied_tx, denied_rx) = std::sync::mpsc::channel();
        let denying_path = path.clone();
        let denying = std::thread::spawn(move || {
            let mut store = TrustStore::load(&denying_path).expect("load denying writer");
            started_tx.send(()).expect("announce denial attempt");
            store
                .deny("server", "fingerprint", "operator")
                .expect("persist denial");
            denied_tx.send(()).expect("announce completed denial");
        });
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("denial thread started");
        assert!(
            denied_rx.recv_timeout(Duration::from_millis(25)).is_err(),
            "denial must wait until the acknowledged local-execution seam ends"
        );

        drop(execution_guard);
        denied_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("denial completed after execution seam");
        denying.join().expect("denying writer");
        let final_store = TrustStore::load(&path).expect("reload final store");
        assert_eq!(
            final_store.decision("server", "fingerprint"),
            TrustDecision::Denied
        );
    }

    #[cfg(unix)]
    #[test]
    fn execution_lock_domain_survives_trust_parent_replacement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("trust-parent");
        std::fs::create_dir(&parent).expect("create trust parent");
        let path = parent.join("trust.json");
        let mut initial = TrustStore::load(&path).expect("load initial store");
        initial
            .acknowledge("server", "fingerprint", "operator")
            .expect("acknowledge server");

        let mut executing = TrustStore::load(&path).expect("load executing reader");
        let (decision, execution_guard) = executing
            .locked_decision("server", "fingerprint")
            .expect("lock execution decision");
        assert_eq!(decision, TrustDecision::Acknowledged);

        let displaced = temp.path().join("displaced-trust-parent");
        std::fs::rename(&parent, &displaced).expect("displace trust parent");
        std::fs::create_dir(&parent).expect("replace trust parent");
        let aliased_path = parent.join(".").join("trust.json");
        let error = acquire_global_trust_lock_for(&aliased_path, Duration::from_millis(25))
            .expect_err("replacement path must retain the original global lock domain");
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);

        drop(execution_guard);
        let mut denial = TrustStore::load(&aliased_path).expect("load replacement alias");
        denial
            .deny("server", "fingerprint", "operator")
            .expect("persist denial after execution seam");
        assert_eq!(
            TrustStore::load(&path)
                .expect("reload replacement path")
                .decision("server", "fingerprint"),
            TrustDecision::Denied
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_trust_file_fails_closed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let real_path = temp.path().join("real-trust.json");
        let link_path = temp.path().join("trust.json");
        std::fs::write(&real_path, r#"{"version":2,"servers":{}}"#).expect("write target");
        symlink(&real_path, &link_path).expect("create trust symlink");

        let error = TrustStore::load(&link_path).expect_err("trust symlink must fail closed");
        assert!(
            error.to_string().contains("regular non-link file"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replaced_parent_cannot_redirect_persist() {
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("trust-parent");
        std::fs::create_dir(&parent).expect("create trust parent");
        let path = parent.join("trust.json");
        let mut store = TrustStore::load(&path).expect("load");
        let guard = store.lock_and_reload().expect("lock pinned parent");
        let at = now_iso();
        store.servers.insert(
            "srv".to_string(),
            TrustRecord {
                state: TrustState::Denied,
                fingerprint: "fp".to_string(),
                by: "operator".to_string(),
                at: at.clone(),
                execution: None,
                audit: vec![TrustAuditEntry {
                    at,
                    action: "denied".to_string(),
                    by: "operator".to_string(),
                    fingerprint: "fp".to_string(),
                }],
            },
        );
        let displaced = temp.path().join("displaced-trust-parent");
        let error = store
            .save_with_before_persist(&guard, || {
                std::fs::rename(&parent, &displaced)?;
                std::fs::create_dir(&parent)
            })
            .expect_err("ancestor replacement must abort persistence");

        assert!(
            error.to_string().contains("trust directory changed"),
            "{error}"
        );
        assert!(
            !path.exists(),
            "replacement directory must not receive the trust decision"
        );
        assert!(
            !displaced.join("trust.json").exists(),
            "aborted write must not publish into the displaced directory"
        );
    }
    /// bd-sp5o3: the acknowledged record persists its bound canonical
    /// identity across reloads, and any non-acknowledged state stops
    /// exposing it (fail-closed for the pre-spawn seam).
    #[test]
    fn acknowledge_execution_binds_identity_until_state_leaves_acknowledged() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        let fingerprint = "c".repeat(64);
        let identity = crate::mcp::config::StoredExecutionIdentity {
            resolved_path: "/usr/local/bin/agent-fixture".to_string(),
            content_sha256: "d".repeat(64),
        };

        let mut store = TrustStore::load(&path).expect("load fresh store");
        store
            .acknowledge_execution("srv", &fingerprint, "operator", identity.clone())
            .expect("bind execution on ack");
        assert_eq!(
            store.decision("srv", &fingerprint),
            TrustDecision::Acknowledged
        );

        let reloaded = TrustStore::load(&path).expect("reload");
        assert_eq!(
            reloaded.acknowledged_execution("srv", &fingerprint),
            Some(identity)
        );

        // Denial immediately revokes exposure of the binding.
        let mut reloaded = TrustStore::load(&path).expect("reload after first load");
        reloaded
            .deny("srv", &fingerprint, "operator")
            .expect("persist denial");
        let reloaded = TrustStore::load(&path).expect("reload after deny");
        assert_eq!(
            reloaded.acknowledged_execution("srv", &fingerprint),
            None,
            "non-acknowledged states must not expose bindings"
        );
    }

    /// bd-sp5o3: a v2 acknowledgement WITHOUT an execution field is exactly
    /// the pre-hardening shape; it must read back as no binding so the spawn
    /// seam forces a fresh `/mcp trust` instead of trusting raw strings.
    #[test]
    fn legacy_v2_record_without_execution_reads_as_unbound() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("trust.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 2,
                "servers": {
                    "srv": {
                        "state": "acknowledged",
                        "fingerprint": "e".repeat(64),
                        "by": "operator",
                        "at": "2026-08-27T00:00:00Z",
                        "audit": []
                    }
                }
            })
            .to_string(),
        )
        .expect("write v2 record without execution");

        let store = TrustStore::load(&path).expect("load legacy-shaped store");
        assert_eq!(
            store.decision("srv", &"e".repeat(64)),
            TrustDecision::Acknowledged
        );
        assert!(
            store
                .acknowledged_execution("srv", &"e".repeat(64))
                .is_none()
        );
    }
}
