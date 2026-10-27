//! Explicit takeover of an authored Authorization header by managed OAuth.
//!
//! Login storage is keyed by the complete definition with that one header
//! removed. A takeover marker binds the grant to the original fingerprint,
//! so newly authored credentials cannot silently inherit an older login.
//! File-backed definitions are repaired at their winning source after login;
//! ACP and extension definitions remain owned by the supplying client.

#[cfg(unix)]
use std::ffi::OsString;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{McpManager, RestartState, ServerEntry, ServerHealth, TransportConstructionAttempt};
use crate::agent_cx::AgentCx;
use crate::error::Result;
use crate::file_identity::FileIdentity;
use crate::mcp::auth::AuthBinding;
use crate::mcp::config::{ConfiguredServer, MAX_MCP_CONFIG_BYTES, Provenance};
use crate::mcp::trust::TrustStore;

#[cfg(test)]
mod tests;

const SOURCE_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

fn source_changed() -> crate::error::Error {
    super::tool_err(
        "MCP_AUTH_SOURCE_CHANGED",
        "the MCP source changed or could not be verified; reload the server before reauthenticating",
    )
}

fn source_io() -> crate::error::Error {
    super::tool_err(
        "MCP_AUTH_SOURCE_IO",
        "the selected MCP source could not be updated durably",
    )
}

fn has_authorization(config: &ConfiguredServer) -> bool {
    config.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
}

pub(super) fn binding_for(
    config: &ConfiguredServer,
    cwd: &Path,
    workspace: &Path,
    global_dir: &Path,
) -> Result<AuthBinding> {
    let mut repaired = config.clone();
    repaired.headers.retain(|(name, _)| !name.eq_ignore_ascii_case("authorization"));
    let identity = repaired.fingerprint_for_workspace(cwd, workspace);
    let url = config.url.as_deref().ok_or_else(|| {
        super::tool_err("MCP_AUTH_UNSUPPORTED", "OAuth requires an HTTP MCP endpoint")
    })?;
    let binding = AuthBinding::new(global_dir, &identity, url)?;
    Ok(if has_authorization(config) {
        binding.for_static_authorization(config.fingerprint_for_workspace(cwd, workspace))
    } else {
        binding
    })
}

pub(super) struct Repair {
    source: Option<SourceRepair>,
}

impl Repair {
    pub(super) async fn prepare(
        entry: &Arc<ServerEntry>,
        workspace: &Path,
    ) -> Result<Arc<Self>> {
        let config = entry.config.clone();
        let workspace = workspace.to_path_buf();
        asupersync::runtime::spawn_blocking(move || {
            let source = if has_authorization(&config)
                && !matches!(config.provenance, Provenance::Acp | Provenance::Extension)
            {
                Some(SourceRepair::prepare(&config, &workspace)?)
            } else {
                None
            };
            Ok(Arc::new(Self { source }))
        })
        .await
    }

    pub(super) fn check_unchanged(&self) -> Result<()> {
        self.source.as_ref().map_or(Ok(()), SourceRepair::check_unchanged)
    }

    /// Called with the connection lane held and the old transport retired.
    /// Persistence runs to completion once its final admission check passes,
    /// even if the waiting UI disappears immediately after the atomic rename.
    pub(super) async fn publish(
        self: &Arc<Self>,
        manager: &McpManager,
        entry: &Arc<ServerEntry>,
        owner: &AgentCx,
        lane: asupersync::sync::OwnedMutexGuard<()>,
    ) -> Result<(Arc<ServerEntry>, asupersync::sync::OwnedMutexGuard<()>)> {
        if self.source.is_none() {
            return Ok((Arc::clone(entry), lane));
        }
        let repair = Arc::clone(self);
        let manager = McpManager { inner: Arc::clone(&manager.inner) };
        let entry = Arc::clone(entry);
        let owner = owner.clone();
        let abandoned = Arc::new(AtomicBool::new(false));
        let worker_abandoned = Arc::clone(&abandoned);
        let mut attempt = TransportConstructionAttempt::new(abandoned);
        let result = asupersync::runtime::spawn_blocking(move || {
            let _scope = owner.cx().clone().set_current_restricted();
            let check = || {
                super::catalog::check_request_owner(&owner)?;
                manager.check_running()?;
                if worker_abandoned.load(Ordering::Acquire) {
                    return Err(super::tool_err("MCP_CANCELLED", "MCP source repair was cancelled"));
                }
                if !Arc::ptr_eq(&manager.entry(&entry.config.name)?, &entry) {
                    return Err(super::tool_err(
                        "MCP_AUTH_SUPERSEDED", "the MCP server definition changed during authentication",
                    ));
                }
                Ok(())
            };
            check()?;
            let source = repair.source.as_ref().ok_or_else(source_changed)?;
            let expected = manager.trust_fingerprint_for(&entry);
            let repaired_fingerprint = source.repaired.fingerprint_for_workspace(
                &entry.effective_cwd(&manager.inner.cwd), &manager.inner.cwd,
            );
            let replacement = Arc::new(ServerEntry {
                config: source.repaired.clone(),
                cwd_override: entry.cwd_override.clone(),
                connect_lane: Arc::clone(&entry.connect_lane),
                transport: Mutex::new(None),
                tools_cache: Mutex::new(None),
                tools_cache_generation: std::sync::atomic::AtomicU64::new(0),
                health: Mutex::new(ServerHealth::NotStarted),
                restarts: Mutex::new(RestartState::default()),
            });
            let _local_trust = McpManager::lock(&manager.inner.trust_lock);
            let mut store = TrustStore::load(&manager.inner.trust_path)?;
            let persisted = store.repair_authorization(
                &entry.config.name, &expected, &repaired_fingerprint,
                || source.commit(&check),
            );
            // After publication the in-memory definition must agree with the
            // source even if a subsequent fsync/trust write fails. The new
            // fingerprint remains subject to the durable trust decision.
            if source.committed.load(Ordering::Acquire) {
                let mut servers = McpManager::lock(&manager.inner.servers);
                if !servers.get(&entry.config.name).is_some_and(|current| Arc::ptr_eq(current, &entry)) {
                    return Err(super::tool_err(
                        "MCP_AUTH_SUPERSEDED", "the MCP server changed after its source repair committed",
                    ));
                }
                servers.insert(entry.config.name.clone(), Arc::clone(&replacement));
            }
            persisted?;
            // Keep the actual owning guard in this worker through publication.
            // Dropping the async waiter must not let queued deny/logout act on
            // the old entry while this admitted transaction installs its heir.
            Ok((replacement, lane))
        })
        .await;
        attempt.disarm();
        result
    }
}

struct SourceRepair {
    source: PathBuf,
    target: PathBuf,
    parent: PathBuf,
    #[cfg(unix)]
    name: OsString,
    identity: FileIdentity,
    // Keeping the original open prevents file-identity reuse during login.
    original: File,
    content: Vec<u8>,
    rendered: Vec<u8>,
    repaired: ConfiguredServer,
    committed: AtomicBool,
    #[cfg(unix)]
    directory: File,
    #[cfg(windows)]
    directories: Vec<crate::mcp::trust::WindowsTrustDirectoryGuard>,
}

impl SourceRepair {
    fn prepare(config: &ConfiguredServer, workspace: &Path) -> Result<Self> {
        let source = config.source_file.clone();
        let target = std::fs::canonicalize(&source).map_err(|_| source_changed())?;
        let parent = target.parent().ok_or_else(source_changed)?.to_path_buf();
        #[cfg(unix)]
        let name = target.file_name().ok_or_else(source_changed)?.to_os_string();
        #[cfg(unix)]
        let directory = crate::mcp::trust::open_trust_directory_nofollow(&parent, false)
            .map_err(|_| source_changed())?;
        #[cfg(windows)]
        let (_, directories) = crate::mcp::trust::open_or_create_windows_trust_parent(&target, false)
            .map_err(|_| source_changed())?;
        let original = {
            #[cfg(unix)]
            { open_source(&directory, &name)? }
            #[cfg(not(unix))]
            { open_source(&target)? }
        };
        let identity = FileIdentity::of_open_file(&original).map_err(|_| source_changed())?;
        let content = read_source(&original)?;
        let text = std::str::from_utf8(&content).map_err(|_| source_changed())?;
        let (rendered, repaired) = crate::mcp::config::authorization_repair_document(
            config, workspace, text,
        ).map_err(|_| source_changed())?;
        let repair = Self {
            source, target, parent, identity, original, content,
            rendered: rendered.into_bytes(), repaired,
            committed: AtomicBool::new(false),
            #[cfg(unix)]
            name,
            #[cfg(unix)]
            directory,
            #[cfg(windows)]
            directories,
        };
        repair.check_unchanged()?;
        Ok(repair)
    }

    fn check_location(&self) -> Result<()> {
        if std::fs::canonicalize(&self.source).map_err(|_| source_changed())? != self.target {
            return Err(source_changed());
        }
        #[cfg(unix)]
        if FileIdentity::of_path_nofollow(&self.parent).map_err(|_| source_changed())?
            != FileIdentity::of_open_file(&self.directory).map_err(|_| source_changed())?
        {
            return Err(source_changed());
        }
        #[cfg(windows)]
        crate::mcp::trust::validate_windows_trust_directory_guards(&self.directories)
            .map_err(|_| source_changed())?;
        Ok(())
    }

    fn check_unchanged(&self) -> Result<()> {
        self.check_location()?;
        #[cfg(unix)]
        let current = open_source(&self.directory, &self.name)?;
        #[cfg(not(unix))]
        let current = open_source(&self.target)?;
        if FileIdentity::of_open_file(&current).map_err(|_| source_changed())? != self.identity
            || read_source(&current)? != self.content
        {
            return Err(source_changed());
        }
        self.check_location()
    }

    #[cfg(unix)]
    fn commit(&self, check: &impl Fn() -> Result<()>) -> Result<()> {
        let _lock = crate::file_lock::DirLockAt::acquire_for(
            &self.directory, &self.name, SOURCE_LOCK_TIMEOUT,
        ).map_err(|_| source_io())?;
        self.check_unchanged()?;
        check()?;
        let name = OsString::from(format!(".pi-mcp-reauth-{}.tmp", uuid::Uuid::new_v4()));
        let descriptor = rustix::fs::openat(
            &self.directory, &name,
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        ).map_err(|_| source_io())?;
        let mut temp = SourceTemp {
            file: File::from(descriptor),
            directory: self.directory.try_clone().map_err(|_| source_io())?,
            name, persisted: false,
        };
        temp.file.set_permissions(self.original.metadata().map_err(|_| source_io())?.permissions())
            .map_err(|_| source_io())?;
        temp.file.write_all(&self.rendered).map_err(|_| source_io())?;
        temp.file.sync_all().map_err(|_| source_io())?;
        self.check_unchanged()?;
        check()?;
        rustix::fs::renameat(&self.directory, &temp.name, &self.directory, &self.name)
            .map_err(|_| source_io())?;
        temp.persisted = true;
        self.committed.store(true, Ordering::Release);
        self.directory.sync_all().map_err(|_| source_io())?;
        self.check_location()
    }

    #[cfg(windows)]
    fn commit(&self, check: &impl Fn() -> Result<()>) -> Result<()> {
        let _lock = crate::file_lock::DirLock::acquire_for(&self.target, SOURCE_LOCK_TIMEOUT)
            .map_err(|_| source_io())?;
        self.check_unchanged()?;
        check()?;
        let mut temp = tempfile::NamedTempFile::new_in(&self.parent).map_err(|_| source_io())?;
        temp.as_file().set_permissions(self.original.metadata().map_err(|_| source_io())?.permissions())
            .map_err(|_| source_io())?;
        temp.write_all(&self.rendered).map_err(|_| source_io())?;
        temp.as_file().sync_all().map_err(|_| source_io())?;
        self.check_unchanged()?;
        check()?;
        temp.persist(&self.target).map_err(|_| source_io())?;
        self.committed.store(true, Ordering::Release);
        self.check_location()
    }

    #[cfg(not(any(unix, windows)))]
    fn commit(&self, _check: &impl Fn() -> Result<()>) -> Result<()> {
        Err(source_io())
    }
}

#[cfg(unix)]
fn open_source(directory: &File, name: &std::ffi::OsStr) -> Result<File> {
    let descriptor = rustix::fs::openat(
        directory, name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ).map_err(|_| source_changed())?;
    Ok(File::from(descriptor))
}

#[cfg(not(unix))]
fn open_source(path: &Path) -> Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.custom_flags(0x0020_0000).share_mode(0x0000_0001 | 0x0000_0002 | 0x0000_0004);
    }
    options.open(path).map_err(|_| source_changed())
}

fn read_source(file: &File) -> Result<Vec<u8>> {
    let metadata = file.metadata().map_err(|_| source_changed())?;
    if !metadata.is_file() || metadata.len() > MAX_MCP_CONFIG_BYTES as u64 {
        return Err(source_changed());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err(source_changed());
        }
    }
    let mut bytes = Vec::new();
    file.take((MAX_MCP_CONFIG_BYTES + 1) as u64).read_to_end(&mut bytes)
        .map_err(|_| source_changed())?;
    if bytes.len() > MAX_MCP_CONFIG_BYTES {
        return Err(source_changed());
    }
    Ok(bytes)
}

#[cfg(unix)]
struct SourceTemp {
    file: File,
    directory: File,
    name: OsString,
    persisted: bool,
}

#[cfg(unix)]
impl Drop for SourceTemp {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = rustix::fs::unlinkat(&self.directory, &self.name, rustix::fs::AtFlags::empty());
        }
    }
}
