//! Host-granted application state and read-only configuration for PiJS extensions.
//!
//! These roots are data capabilities, never JavaScript module search roots.
//! Configuration grants name one file inside a protected namespace. Reads and
//! mutations use no-follow handles and reject linked files; the ordinary node:fs
//! overlay remains unchanged elsewhere.

use super::{PiJsModuleState, current_extension_id};
use crate::extensions::{ExtensionPolicy, PolicyDecision};
use crate::file_identity::FileIdentity;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use rquickjs::function::Func;
use rquickjs::{Ctx, Exception};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

const MAX_STORAGE_WRITE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
enum StorageAccess {
    ReadWriteDirectory,
    ReadOnlyFile { name: OsString },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StorageGrant {
    /// The protected namespace, including for an exact-file grant. Other files
    /// beneath it must never regain authority through the workspace fallback.
    root: PathBuf,
    access: StorageAccess,
}

#[derive(Debug, Default)]
pub(super) struct PersistentStorage {
    roots: HashMap<String, Vec<StorageGrant>>,
    foreign_roots: HashMap<String, Vec<PathBuf>>,
    policy: Option<ExtensionPolicy>,
}

fn denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

/// Preserve errno as data: extensions must distinguish absent configuration
/// from denied or failed reads without inspecting platform-specific messages.
pub(super) fn filesystem_error(
    ctx: &Ctx<'_>,
    path: &str,
    operation: &str,
    error: &io::Error,
) -> rquickjs::Error {
    let code = match error.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        io::ErrorKind::InvalidInput => "EINVAL",
        io::ErrorKind::IsADirectory => "EISDIR",
        io::ErrorKind::NotADirectory => "ENOTDIR",
        io::ErrorKind::Unsupported => "ENOTSUP",
        _ => "EIO",
    };
    let result = Exception::from_message(
        ctx.clone(),
        &format!("{code}: {error}, {operation} '{path}'"),
    )
    .and_then(|exception| {
        exception.as_object().set("code", code)?;
        exception.as_object().set("syscall", operation)?;
        exception.as_object().set("path", path)?;
        Ok(exception.throw())
    });
    match result {
        Ok(error) | Err(error) => error,
    }
}

#[cfg(windows)]
fn boundary_component_key(component: Component<'_>) -> String {
    use std::path::Prefix;
    match component {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                format!("drive:{}", char::from(drive.to_ascii_lowercase()))
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                format!(
                    "unc:{}\\{}",
                    server.to_string_lossy().to_lowercase(),
                    share.to_string_lossy().to_lowercase()
                )
            }
            Prefix::DeviceNS(name)
                if name.as_encoded_bytes().len() == 2
                    && name.as_encoded_bytes()[0].is_ascii_alphabetic()
                    && name.as_encoded_bytes()[1] == b':' =>
            {
                format!(
                    "drive:{}",
                    char::from(name.as_encoded_bytes()[0].to_ascii_lowercase())
                )
            }
            other => format!("prefix:{other:?}"),
        },
        Component::Normal(name) => format!(
            "name:{}",
            name.to_string_lossy()
                .split(':')
                .next()
                .unwrap_or_default()
                .trim_end_matches(['.', ' '])
                .to_lowercase()
        ),
        Component::RootDir => "root".to_string(),
        Component::CurDir => "current".to_string(),
        Component::ParentDir => "parent".to_string(),
    }
}

/// Recognize lexical aliases before a lookup can follow a forbidden link away
/// from its boundary. This only classifies paths for denial: owned grants still
/// require validation of the original path and strict canonical containment.
fn within_boundary(path: &Path, root: &Path) -> bool {
    fn components_match(path: &Path, root: &Path) -> bool {
        let mut path = path.components();
        root.components().all(|root| {
            path.next().is_some_and(|path| {
                #[cfg(windows)]
                {
                    boundary_component_key(path) == boundary_component_key(root)
                }
                #[cfg(not(windows))]
                {
                    path == root
                }
            })
        })
    }
    if components_match(path, root) {
        return true;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    components_match(&normalized, root)
}

fn validate_components(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(denied("persistent storage requires an absolute path"));
    }
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(denied("persistent storage paths must not contain '..'"));
        }
        #[cfg(windows)]
        match component {
            Component::Prefix(prefix)
                if !matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::UNC(_, _)
                ) =>
            {
                return Err(denied("persistent storage does not accept device paths"));
            }
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                let stem = name
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches(' ')
                    .to_ascii_uppercase();
                let port = stem
                    .strip_prefix("COM")
                    .or_else(|| stem.strip_prefix("LPT"));
                if name.contains(['<', '>', ':', '"', '|', '?', '*'])
                    || name.chars().any(|ch| ch <= '\u{1f}')
                    || name.ends_with(['.', ' '])
                    || matches!(
                        stem.as_str(),
                        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                    )
                    || port.is_some_and(|number| {
                        matches!(
                            number,
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                        )
                    })
                {
                    return Err(denied(
                        "persistent storage does not accept Windows device names, streams or reserved characters",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn metadata_is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn reject_link_components(path: &Path) -> io::Result<()> {
    validate_components(path)?;
    for ancestor in path.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata_is_link(&metadata) => {
                return Err(denied(
                    "persistent storage must not traverse symlinks or reparse points",
                ));
            }
            Ok(metadata) => {
                if !metadata.is_file() && !metadata.is_dir() {
                    return Err(denied(
                        "persistent storage accepts only regular files and directories",
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;
                    if metadata.is_file() && metadata.nlink() != 1 {
                        return Err(denied(
                            "persistent storage must not access hard-linked files",
                        ));
                    }
                }
                #[cfg(windows)]
                if metadata.is_file() {
                    use std::os::windows::fs::OpenOptionsExt as _;
                    let handle = std::fs::OpenOptions::new()
                        .access_mode(0)
                        .share_mode(0x1 | 0x2 | 0x4)
                        .custom_flags(0x0020_0000)
                        .open(ancestor)?;
                    if winapi_util::file::information(&handle)?.number_of_links() != 1 {
                        return Err(denied(
                            "persistent storage must not access hard-linked files",
                        ));
                    }
                }
                #[cfg(not(any(unix, windows)))]
                let _ = metadata;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

impl PersistentStorage {
    pub(super) fn set_policy(&mut self, policy: Option<ExtensionPolicy>) {
        self.policy = policy;
    }

    pub(super) fn register(&mut self, root: PathBuf, owner: &str, foreign: bool) -> io::Result<()> {
        if owner.trim().is_empty() || root.parent().is_none() {
            return Err(denied(
                "persistent storage requires an owner and a non-root directory",
            ));
        }
        validate_components(&root)?;
        if foreign {
            // A deny boundary is metadata, not a grant. It must survive an
            // inaccessible, linked, or absent namespace without preventing an
            // unrelated extension from loading. Preserve the lexical path so
            // requests through a forbidden link are denied before resolution.
            let entries = self.foreign_roots.entry(owner.to_string()).or_default();
            if !entries.contains(&root) {
                entries.push(root);
            }
            return Ok(());
        }
        self.register_grant(root, owner, StorageAccess::ReadWriteDirectory)
    }

    pub(super) fn register_configuration_file(
        &mut self,
        file: PathBuf,
        owner: &str,
    ) -> io::Result<()> {
        // Do not canonicalize away a linked leaf before checking it. Missing
        // files/directories are legitimate: registration creates neither.
        reject_link_components(&file)?;
        if let Ok(metadata) = std::fs::symlink_metadata(&file)
            && !metadata.is_file()
        {
            return Err(denied("a configuration grant must name a regular file"));
        }
        let root = file
            .parent()
            .filter(|root| root.parent().is_some())
            .ok_or_else(|| denied("configuration requires a non-root parent directory"))?;
        let name = file
            .file_name()
            .ok_or_else(|| denied("configuration requires a file name"))?
            .to_os_string();
        self.register_grant(
            root.to_path_buf(),
            owner,
            StorageAccess::ReadOnlyFile { name },
        )
    }

    fn register_grant(
        &mut self,
        root: PathBuf,
        owner: &str,
        access: StorageAccess,
    ) -> io::Result<()> {
        if owner.trim().is_empty() || root.parent().is_none() {
            return Err(denied(
                "persistent storage requires an owner and a non-root directory",
            ));
        }
        reject_link_components(&root)?;
        let root = crate::extensions::safe_canonicalize(&root);
        if let Ok(metadata) = std::fs::symlink_metadata(&root)
            && !metadata.is_dir()
        {
            return Err(denied("a persistent storage root must be a directory"));
        }
        for (other, roots) in &self.roots {
            if roots.iter().any(|grant| {
                (other != owner || grant.access != access)
                    && (within_boundary(&root, &grant.root) || within_boundary(&grant.root, &root))
            }) {
                return Err(denied(
                    "persistent storage grants with different owners or access must not overlap",
                ));
            }
        }
        if self.foreign_roots.values().flatten().any(|other_root| {
            within_boundary(&root, other_root) || within_boundary(other_root, &root)
        }) {
            return Err(denied(
                "persistent storage grant overlaps a denied namespace",
            ));
        }
        let entries = self.roots.entry(owner.to_string()).or_default();
        let grant = StorageGrant { root, access };
        if !entries.contains(&grant) {
            entries.push(grant);
        }
        Ok(())
    }

    pub(super) fn clear(&mut self) {
        self.roots.clear();
        self.foreign_roots.clear();
    }

    pub(super) fn is_registered(&self, path: &Path) -> bool {
        self.roots
            .values()
            .flatten()
            .map(|grant| &grant.root)
            .chain(self.foreign_roots.values().flatten())
            .any(|root| within_boundary(path, root))
    }

    pub(super) fn resolve(&self, path: &Path, owner: Option<&str>) -> io::Result<Option<PathBuf>> {
        self.resolve_grant(path, owner)
            .map(|resolved| resolved.map(|(path, _)| path))
    }

    fn resolve_grant(
        &self,
        path: &Path,
        owner: Option<&str>,
    ) -> io::Result<Option<(PathBuf, &StorageGrant)>> {
        if self.roots.is_empty() && self.foreign_roots.is_empty() {
            return Ok(None);
        }
        if self
            .foreign_roots
            .values()
            .flatten()
            .any(|root| within_boundary(path, root))
        {
            return Err(denied(
                "persistent storage namespace is not granted to this extension",
            ));
        }
        let registered_input = self.is_registered(path);
        if registered_input {
            // Windows canonicalization opens the target. Reject device names
            // and namespace syntax before it can touch a special OS object.
            validate_components(path)?;
        }
        let checked = crate::extensions::safe_canonicalize(path);
        if self
            .foreign_roots
            .values()
            .flatten()
            .any(|root| within_boundary(&checked, root))
        {
            return Err(denied(
                "persistent storage namespace is not granted to this extension",
            ));
        }
        let registered = registered_input || self.is_registered(&checked);
        if !registered {
            return Ok(None);
        }
        reject_link_components(path)?;
        let owned = owner.and_then(|id| self.roots.get(id));
        let grant = owned.and_then(|roots| {
            roots
                .iter()
                .filter(|grant| match grant.access {
                    StorageAccess::ReadWriteDirectory => checked.starts_with(&grant.root),
                    // Classification only. Exact target validation below never
                    // grants authority based on Windows case folding alone.
                    StorageAccess::ReadOnlyFile { .. } => within_boundary(&checked, &grant.root),
                })
                .max_by_key(|grant| grant.root.components().count())
        });
        let Some(grant) = grant else {
            return Err(denied(
                "persistent storage belongs to a different extension",
            ));
        };
        let root = &grant.root;
        for (other, roots) in &self.roots {
            if Some(other.as_str()) != owner
                && roots.iter().any(|other_grant| {
                    within_boundary(&checked, &other_grant.root)
                        && other_grant.root.components().count() >= root.components().count()
                })
            {
                return Err(denied(
                    "persistent storage belongs to a different extension",
                ));
            }
        }
        if let StorageAccess::ReadOnlyFile { name } = &grant.access {
            let granted_file = root.join(name);
            reject_link_components(&granted_file)?;
            let canonical_grant = crate::extensions::safe_canonicalize(&granted_file);
            let same_file = checked == canonical_grant;
            #[cfg(windows)]
            let same_file = same_file || {
                // Case-sensitive Windows directories can contain distinct
                // config.json and Config.JSON files. Only actual file identity
                // can authorize an alternate spelling of an existing file.
                match (
                    FileIdentity::of_path_nofollow(&checked),
                    FileIdentity::of_path_nofollow(&granted_file),
                ) {
                    (Ok(requested), Ok(granted)) => requested == granted,
                    _ => false,
                }
            };
            if !same_file {
                return Err(denied(
                    "configuration access is restricted to the granted file",
                ));
            }
            // Always open the literal host-selected file. Canonical/identity
            // checks classify aliases; they must not redirect the actual I/O
            // to a new sibling or linked target after a filesystem race.
            return Ok(Some((granted_file, grant)));
        }
        if !path.starts_with(root) && !checked.starts_with(root) {
            return Err(denied("persistent storage path escaped its granted root"));
        }
        Ok(Some((checked, grant)))
    }

    pub(super) fn allows(&self, path: &Path, owner: Option<&str>, capability: &str) -> bool {
        self.resolve_grant(path, owner).is_ok_and(|resolved| {
            resolved.is_some_and(|(_, grant)| {
                capability == "read"
                    || (capability == "write" && grant.access == StorageAccess::ReadWriteDirectory)
            })
        }) && self.policy.as_ref().is_none_or(|policy| {
            policy.evaluate_for(capability, owner).decision == PolicyDecision::Allow
        })
    }
}

/// Pins every directory component used by a persistent mutation.
pub(super) struct StorageParent {
    path: PathBuf,
    #[cfg(unix)]
    directory: File,
    #[cfg(windows)]
    guards: Vec<crate::mcp::trust::WindowsTrustDirectoryGuard>,
}

impl StorageParent {
    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn verify(&self) -> io::Result<()> {
        reject_link_components(&self.path)?;
        #[cfg(unix)]
        {
            let parent = self
                .path
                .parent()
                .ok_or_else(|| denied("storage path has no parent"))?;
            if FileIdentity::of_path_nofollow(parent)?
                != FileIdentity::of_open_file(&self.directory)?
            {
                return Err(denied("persistent storage parent changed during access"));
            }
        }
        #[cfg(windows)]
        crate::mcp::trust::validate_windows_trust_directory_guards(&self.guards)?;
        Ok(())
    }
}

#[cfg(unix)]
fn open_directory(path: &Path, create: bool) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    validate_components(path)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = File::from(rustix::fs::open("/", flags, Mode::empty())?);
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let child = match rustix::fs::openat(&directory, name, flags, Mode::empty()) {
            Ok(child) => child,
            Err(rustix::io::Errno::NOENT) if create => {
                match rustix::fs::mkdirat(&directory, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                rustix::fs::openat(&directory, name, flags, Mode::empty())?
            }
            Err(error) => return Err(error.into()),
        };
        directory = File::from(child);
    }
    Ok(directory)
}

pub(super) fn pin_parent(path: &Path, create: bool) -> io::Result<StorageParent> {
    reject_link_components(path)?;
    #[cfg(unix)]
    let guard = StorageParent {
        path: path.to_path_buf(),
        directory: open_directory(
            path.parent()
                .ok_or_else(|| denied("storage path has no parent"))?,
            create,
        )?,
    };
    #[cfg(windows)]
    let guard = {
        let (path, guards) = crate::mcp::trust::open_or_create_windows_trust_parent(path, create)?;
        StorageParent { path, guards }
    };
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "persistent storage requires secure filesystem handles",
    ));
    #[cfg(any(unix, windows))]
    {
        guard.verify()?;
        Ok(guard)
    }
}

#[cfg(unix)]
fn open_leaf(parent: &StorageParent, access: rustix::fs::OFlags) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let name = parent
        .path
        .file_name()
        .ok_or_else(|| denied("storage file has no name"))?;
    // A leaf can change after preflight. Never wait for a FIFO peer or acquire
    // a controlling terminal before the caller validates the opened object.
    let flags = access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    Ok(File::from(rustix::fs::openat(
        &parent.directory,
        name,
        flags,
        Mode::RUSR | Mode::WUSR,
    )?))
}

fn open_file(parent: &StorageParent, append: bool, exclusive: bool) -> io::Result<File> {
    parent.verify()?;
    #[cfg(unix)]
    let file = {
        use rustix::fs::OFlags;
        let mut flags = OFlags::WRONLY | OFlags::CREATE;
        if append {
            flags |= OFlags::APPEND;
        }
        if exclusive {
            flags |= OFlags::EXCL;
        }
        open_leaf(parent, flags)?
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .write(true)
            .append(append)
            .create(!exclusive)
            .create_new(exclusive)
            .share_mode(0x1 | 0x2)
            .custom_flags(0x0020_0000)
            .open(&parent.path)?
    };
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "persistent storage requires secure filesystem handles",
    ));
    #[cfg(any(unix, windows))]
    {
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata_is_link(&metadata) {
            return Err(denied("persistent storage requires a regular file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() != 1 {
                return Err(denied(
                    "persistent storage must not modify hard-linked files",
                ));
            }
        }
        #[cfg(windows)]
        if winapi_util::file::information(&file)?.number_of_links() != 1 {
            return Err(denied(
                "persistent storage must not modify hard-linked files",
            ));
        }
        if FileIdentity::of_open_file(&file)? != FileIdentity::of_path_nofollow(&parent.path)? {
            return Err(denied("persistent storage file changed while opening"));
        }
        parent.verify()?;
        Ok(file)
    }
}

fn authorized_read_path(
    path: &Path,
    owner: Option<&str>,
    state: &Rc<RefCell<PiJsModuleState>>,
) -> io::Result<Option<PathBuf>> {
    let module_state = state.borrow();
    let registry = &module_state.persistent_storage;
    let Some(path) = registry.resolve(path, owner)? else {
        return Ok(None);
    };
    let checked = crate::extensions::safe_canonicalize(&path);
    if super::path_is_in_registered_extension_root(&checked, state)
        && !super::path_is_in_owned_extension_root(&checked, owner, state)
    {
        return Err(denied(
            "persistent data overlaps another extension's code root",
        ));
    }
    if !registry.allows(&path, owner, "read") {
        return Err(denied("persistent storage read capability denied"));
    }
    Ok(Some(path))
}

fn open_read_file(path: &Path) -> io::Result<(File, StorageParent)> {
    let parent = pin_parent(path, false)?;
    #[cfg(unix)]
    let file = open_leaf(&parent, rustix::fs::OFlags::RDONLY)?;
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .custom_flags(0x0020_0000)
            .open(parent.path())?
    };
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "persistent storage requires secure filesystem handles",
    ));
    #[cfg(any(unix, windows))]
    {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata_is_link(&metadata)
            || FileIdentity::of_open_file(&file)? != FileIdentity::of_path_nofollow(path)?
        {
            return Err(denied(
                "persistent storage read target changed or is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() != 1 {
                return Err(denied("persistent storage must not read hard-linked files"));
            }
        }
        #[cfg(windows)]
        if winapi_util::file::information(&file)?.number_of_links() != 1 {
            return Err(denied("persistent storage must not read hard-linked files"));
        }
        parent.verify()?;
        Ok((file, parent))
    }
}

pub(super) fn read_file(
    path: &Path,
    owner: Option<&str>,
    state: &Rc<RefCell<PiJsModuleState>>,
) -> io::Result<Option<Vec<u8>>> {
    let Some(path) = authorized_read_path(path, owner, state)? else {
        return Ok(None);
    };
    let (file, parent) = open_read_file(&path)?;
    let mut bytes = Vec::new();
    file.take((MAX_STORAGE_WRITE_BYTES as u64) + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STORAGE_WRITE_BYTES {
        return Err(denied("persistent storage read exceeds 64 MiB"));
    }
    parent.verify()?;
    Ok(Some(bytes))
}

/// Metadata uses the same literal grant and pinned file as content reads.
/// Never fall back to a case alias or follow a replacement link for stat or
/// realpath after admitting an exact-file configuration capability.
pub(super) fn configuration_metadata(
    path: &Path,
    owner: Option<&str>,
    state: &Rc<RefCell<PiJsModuleState>>,
) -> io::Result<Option<(PathBuf, Metadata)>> {
    let configuration = state
        .borrow()
        .persistent_storage
        .resolve_grant(path, owner)?
        .is_some_and(|(_, grant)| matches!(grant.access, StorageAccess::ReadOnlyFile { .. }));
    if !configuration {
        return Ok(None);
    }
    let path = authorized_read_path(path, owner, state)?
        .ok_or_else(|| denied("configuration grant disappeared during metadata access"))?;
    let (file, parent) = open_read_file(&path)?;
    let canonical = std::fs::canonicalize(&path).map(crate::extensions::strip_unc_prefix)?;
    parent.verify()?;
    if FileIdentity::of_open_file(&file)? != FileIdentity::of_path_nofollow(&canonical)? {
        return Err(denied(
            "configuration target changed during metadata access",
        ));
    }
    let metadata = file.metadata()?;
    parent.verify()?;
    Ok(Some((canonical, metadata)))
}

fn mutate(
    op: &str,
    path: &Path,
    args: &serde_json::Value,
    target: Option<&Path>,
) -> io::Result<()> {
    match op {
        "mkdir" => {
            if args
                .get("recursive")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                // The sentinel is never opened; pin_parent creates only its parent.
                pin_parent(&path.join(".pijs-directory-sentinel"), true)?.verify()
            } else {
                let parent = pin_parent(path, false)?;
                #[cfg(unix)]
                rustix::fs::mkdirat(
                    &parent.directory,
                    path.file_name()
                        .ok_or_else(|| denied("storage directory has no name"))?,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
                )?;
                #[cfg(windows)]
                std::fs::create_dir(parent.path())?;
                parent.verify()
            }
        }
        "write" | "append" => {
            let encoded = args
                .get("data")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| denied("storage write requires base64 data"))?;
            if encoded.len() > MAX_STORAGE_WRITE_BYTES.saturating_mul(4).div_ceil(3) + 4 {
                return Err(denied("persistent storage write exceeds 64 MiB"));
            }
            let bytes = BASE64
                .decode(encoded)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            if bytes.len() > MAX_STORAGE_WRITE_BYTES {
                return Err(denied("persistent storage write exceeds 64 MiB"));
            }
            let flag = args
                .get("flag")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(if op == "append" { "a" } else { "w" });
            if !matches!(flag, "w" | "wx" | "a" | "ax") {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "persistent write supports w, wx, a and ax flags",
                ));
            }
            let parent = pin_parent(path, false)?;
            let append = flag.starts_with('a');
            let mut file = open_file(&parent, append, flag.ends_with('x'))?;
            if !append {
                file.set_len(0)?;
            }
            file.write_all(&bytes)?;
            parent.verify()
        }
        "unlink" | "rmdir" => {
            let parent = pin_parent(path, false)?;
            #[cfg(unix)]
            rustix::fs::unlinkat(
                &parent.directory,
                path.file_name()
                    .ok_or_else(|| denied("storage path has no name"))?,
                if op == "rmdir" {
                    rustix::fs::AtFlags::REMOVEDIR
                } else {
                    rustix::fs::AtFlags::empty()
                },
            )?;
            #[cfg(windows)]
            if op == "rmdir" {
                std::fs::remove_dir(parent.path())?;
            } else {
                std::fs::remove_file(parent.path())?;
            }
            parent.verify()
        }
        "rename" => {
            let target = target.ok_or_else(|| denied("storage rename requires a target"))?;
            let source_parent = pin_parent(path, false)?;
            let target_parent = pin_parent(target, false)?;
            #[cfg(unix)]
            rustix::fs::renameat(
                &source_parent.directory,
                path.file_name()
                    .ok_or_else(|| denied("storage source has no name"))?,
                &target_parent.directory,
                target
                    .file_name()
                    .ok_or_else(|| denied("storage target has no name"))?,
            )?;
            #[cfg(windows)]
            std::fs::rename(source_parent.path(), target_parent.path())?;
            source_parent.verify()?;
            target_parent.verify()
        }
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported persistent storage operation",
        )),
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn install(
    ctx: &Ctx<'_>,
    state: &Rc<RefCell<PiJsModuleState>>,
    cwd: &str,
) -> rquickjs::Result<()> {
    ctx.globals().set(
        "__pi_host_persistent_storage_mkdtemp",
        Func::from({
            let state = Rc::clone(state);
            move |ctx: Ctx<'_>, prefix: String| -> rquickjs::Result<String> {
                let run = || -> io::Result<String> {
                    let owner = current_extension_id(&ctx);
                    let state = state.borrow();
                    let registry = &state.persistent_storage;
                    if !registry.allows(Path::new(&prefix), owner.as_deref(), "write") {
                        return Err(denied(
                            "persistent temporary directory prefix is outside the extension grant",
                        ));
                    }
                    for _ in 0..16 {
                        let suffix = uuid::Uuid::new_v4().simple().to_string();
                        let candidate = PathBuf::from(format!("{prefix}{}", &suffix[..6]));
                        if !registry.allows(&candidate, owner.as_deref(), "write") {
                            return Err(denied("persistent temporary directory escaped its grant"));
                        }
                        match mutate(
                            "mkdir",
                            &candidate,
                            &serde_json::json!({ "recursive": false }),
                            None,
                        ) {
                            Ok(()) => return Ok(candidate.to_string_lossy().into_owned()),
                            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(error) => return Err(error),
                        }
                    }
                    Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "persistent temporary directory attempts exhausted",
                    ))
                };
                run().map_err(|error| {
                    rquickjs::Error::new_loading_message(
                        &prefix,
                        format!("persistent storage mkdtemp: {error}"),
                    )
                })
            }
        }),
    )?;
    ctx.globals().set(
        "__pi_host_check_persistent_storage_input",
        Func::from({
            let state = Rc::clone(state);
            let cwd = PathBuf::from(cwd);
            move |ctx: Ctx<'_>, path: String, capability: String| -> rquickjs::Result<()> {
                let run = || -> io::Result<()> {
                    if !matches!(capability.as_str(), "read" | "write") {
                        return Err(denied("invalid persistent storage input capability"));
                    }
                    let requested = Path::new(&path);
                    let absolute = if requested.is_absolute() {
                        requested.to_path_buf()
                    } else {
                        cwd.join(requested)
                    };
                    let owner = current_extension_id(&ctx);
                    let state = state.borrow();
                    let registry = &state.persistent_storage;
                    // Inspect the original path before the JS overlay removes
                    // dot components or resolves virtual symbolic links. Paths
                    // unrelated to a storage grant keep their existing policy.
                    if registry.resolve(&absolute, owner.as_deref())?.is_some()
                        && !registry.allows(&absolute, owner.as_deref(), &capability)
                    {
                        return Err(denied("persistent storage capability denied"));
                    }
                    Ok(())
                };
                run().map_err(|error| filesystem_error(&ctx, &path, &capability, &error))
            }
        }),
    )?;
    ctx.globals().set(
        "__pi_host_is_persistent_storage_path",
        Func::from({
            let state = Rc::clone(state);
            move |path: String| -> bool {
                state
                    .borrow()
                    .persistent_storage
                    .is_registered(Path::new(&path))
            }
        }),
    )?;
    ctx.globals().set(
        "__pi_host_persistent_storage_mutate",
        Func::from({
            let state = Rc::clone(state);
            move |ctx: Ctx<'_>, op: String, path: String, args: String| -> rquickjs::Result<()> {
                let run = || -> io::Result<()> {
                    let owner = current_extension_id(&ctx);
                    let state = state.borrow();
                    let registry = &state.persistent_storage;
                    if !registry.allows(Path::new(&path), owner.as_deref(), "write") {
                        return Err(denied(
                            "persistent storage write denied by extension capability or scope",
                        ));
                    }
                    let resolved = registry
                        .resolve(Path::new(&path), owner.as_deref())?
                        .ok_or_else(|| denied("unregistered persistent storage path"))?;
                    // The grant is a capability boundary; its directory cannot be
                    // renamed or removed, even by its owner.
                    if matches!(op.as_str(), "rename" | "unlink" | "rmdir")
                        && registry
                            .roots
                            .values()
                            .flatten()
                            .any(|grant| grant.root == resolved)
                    {
                        return Err(denied("cannot remove a persistent storage root"));
                    }
                    let args: serde_json::Value = serde_json::from_str(&args)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                    let target = args
                        .get("target")
                        .and_then(serde_json::Value::as_str)
                        .map(Path::new);
                    let target = if let Some(target) = target {
                        if !registry.allows(target, owner.as_deref(), "write") {
                            return Err(denied(
                                "persistent storage rename target is outside the extension grant",
                            ));
                        }
                        let target =
                            registry.resolve(target, owner.as_deref())?.ok_or_else(|| {
                                denied("unregistered persistent storage rename target")
                            })?;
                        if registry
                            .roots
                            .values()
                            .flatten()
                            .any(|grant| grant.root == target)
                        {
                            return Err(denied("cannot overwrite a persistent storage root"));
                        }
                        Some(target)
                    } else {
                        None
                    };
                    mutate(&op, &resolved, &args, target.as_deref())
                };
                run().map_err(|error| {
                    rquickjs::Error::new_loading_message(
                        &path,
                        format!("persistent storage {op}: {error}"),
                    )
                })
            }
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_owner_and_foreign_boundaries_are_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let base = crate::extensions::safe_canonicalize(temp.path());
        let root = base.join("app-state");
        let mut registry = PersistentStorage::default();
        registry.register(root.clone(), "owner", false).unwrap();
        assert!(
            registry
                .resolve(&root.join("nested/data.db"), Some("owner"))
                .unwrap()
                .is_some()
        );
        assert!(
            registry
                .resolve(&root.join("data.db"), Some("peer"))
                .is_err()
        );
        assert!(registry.resolve(&root.join("data.db"), None).is_err());
        registry
            .register(root.join("nested"), "peer", true)
            .unwrap();
        assert!(
            registry
                .resolve(&root.join("nested/data.db"), Some("owner"))
                .is_err()
        );
        assert!(
            registry
                .resolve(
                    &base.join("unrelated/../app-state/nested/data.db"),
                    Some("owner")
                )
                .is_err()
        );
        assert!(
            registry
                .resolve(&root.join("../outside"), Some("owner"))
                .is_err()
        );
        assert!(
            registry
                .resolve(&base.join("app-state-other/data.db"), Some("owner"))
                .unwrap()
                .is_none()
        );
        registry.clear();
        assert!(
            registry
                .resolve(&root.join("data.db"), Some("owner"))
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn denied_linked_namespace_does_not_block_unrelated_grants() {
        let temp = tempfile::tempdir().unwrap();
        let base = crate::extensions::safe_canonicalize(temp.path());
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let linked = base.join("linked-state");
        std::os::unix::fs::symlink(&outside, &linked).unwrap();
        let healthy = base.join("healthy-state");
        let mut registry = PersistentStorage::default();
        assert!(registry.register(linked.clone(), "broken", false).is_err());
        registry.register(linked.clone(), "broken", true).unwrap();
        registry
            .register(healthy.clone(), "healthy", false)
            .unwrap();
        assert!(
            registry
                .resolve(&linked.join("secret"), Some("broken"))
                .is_err()
        );
        assert!(
            registry
                .resolve(&linked.join("secret"), Some("healthy"))
                .is_err()
        );
        assert!(
            registry
                .resolve(&healthy.join("data.db"), Some("healthy"))
                .unwrap()
                .is_some()
        );
        // Registering an unusable foreign namespace must not claim its link
        // target as new authority or make unrelated target paths inaccessible.
        assert!(
            registry
                .resolve(&outside.join("secret"), Some("healthy"))
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn storage_rejects_special_targets_without_io_and_keeps_regular_files_usable() {
        use std::os::unix::fs::FileTypeExt as _;

        let temp = tempfile::tempdir().unwrap();
        let root = crate::extensions::safe_canonicalize(temp.path());
        let fifo = root.join("events.pipe");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let socket = root.join("events.socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut state = PiJsModuleState::new();
        state
            .persistent_storage
            .register(root.clone(), "owner", false)
            .unwrap();
        let state = Rc::new(RefCell::new(state));
        let contents = serde_json::json!({ "data": BASE64.encode("session event") });
        for path in [&fifo, &socket] {
            assert!(
                state
                    .borrow()
                    .persistent_storage
                    .resolve(path, Some("owner"))
                    .is_err()
            );
            assert!(read_file(path, Some("owner"), &state).is_err());
            for operation in ["write", "append"] {
                assert!(mutate(operation, path, &contents, None).is_err());
            }
            assert!(pin_parent(path, false).is_err());
        }
        assert!(
            std::fs::symlink_metadata(&fifo)
                .unwrap()
                .file_type()
                .is_fifo()
        );
        assert!(
            std::fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_socket()
        );

        let regular = root.join("events.jsonl");
        mutate("write", &regular, &contents, None).unwrap();
        mutate("append", &regular, &contents, None).unwrap();
        assert_eq!(
            read_file(&regular, Some("owner"), &state).unwrap().unwrap(),
            b"session eventsession event"
        );
        let parent = pin_parent(&regular, false).unwrap();
        let opened = open_file(&parent, false, false).unwrap();
        assert_eq!(
            opened.metadata().unwrap().len(),
            26,
            "open must not truncate"
        );
        assert!(
            open_file(&parent, false, true).is_err(),
            "exclusive create must fail"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replaced_fifo_leaf_never_waits_for_a_pipe_peer() {
        use rustix::fs::{Mode, OFlags};
        use std::sync::mpsc;
        use std::time::Duration;

        let temp = tempfile::tempdir().unwrap();
        let root = crate::extensions::safe_canonicalize(temp.path());
        for (name, access) in [
            ("read", OFlags::RDONLY),
            ("write", OFlags::WRONLY | OFlags::CREATE),
            ("append", OFlags::WRONLY | OFlags::CREATE | OFlags::APPEND),
        ] {
            let path = root.join(name);
            // Model replacement after the caller's final path preflight.
            let parent = pin_parent(&path, false).unwrap();
            rustix::fs::mkfifoat(rustix::fs::CWD, &path, Mode::RUSR | Mode::WUSR).unwrap();
            let (sender, receiver) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                sender.send(open_leaf(&parent, access)).unwrap();
            });
            let result = receiver.recv_timeout(Duration::from_secs(2));
            // Keep a broken implementation bounded: release a blocking open
            // before joining, then fail with the original timeout evidence.
            let rescue = result.is_err().then(|| {
                let reader = rustix::fs::open(
                    &path,
                    OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .unwrap();
                let writer = rustix::fs::open(
                    &path,
                    OFlags::WRONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .unwrap();
                (reader, writer)
            });
            worker.join().unwrap();
            drop(rescue);
            let result = result.expect("storage leaf open must not wait for a pipe peer");
            if name == "read" {
                let file = result.unwrap();
                assert!(!file.metadata().unwrap().is_file());
                assert!(
                    rustix::fs::fcntl_getfl(&file)
                        .unwrap()
                        .contains(OFlags::NONBLOCK)
                );
            } else {
                assert!(result.is_err(), "a FIFO without a reader is not writable");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn denied_windows_boundaries_recognize_case_device_and_stream_aliases() {
        let mut registry = PersistentStorage::default();
        registry
            .register(
                PathBuf::from(r"C:\Users\Alice\.pi\context-mode"),
                "peer",
                true,
            )
            .unwrap();
        registry
            .register(
                PathBuf::from(r"\\server\share\home\.pi\context-mode"),
                "peer",
                true,
            )
            .unwrap();
        for path in [
            r"c:\users\ALICE\.PI\CONTEXT-MODE\sessions\db",
            r"C:\Users\Alice\.pi\context-mode.\sessions\db",
            r"C:\Users\Alice\.pi\context-mode:stream",
            r"C:\Users\Alice\.pi\other\..\context-mode\sessions\db",
            r"\\?\C:\Users\Alice\.pi\context-mode\sessions\db",
            r"\\.\C:\Users\Alice\.pi\context-mode\sessions\db",
            r"\\SERVER\SHARE\HOME\.PI\CONTEXT-MODE\sessions\db",
            r"\\?\UNC\SERVER\SHARE\home\.pi\context-mode\sessions\db",
        ] {
            assert!(
                registry.is_registered(Path::new(path)),
                "unclassified alias: {path}"
            );
            assert!(
                registry.resolve(Path::new(path), Some("peer")).is_err(),
                "foreign boundary was granted: {path}"
            );
        }
        assert!(!registry.is_registered(Path::new(r"C:\Users\Alice\.pi\context-mode-peer\db")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_storage_paths_reject_devices_streams_and_traversal() {
        for path in [
            r"C:\Users\alice\.pi\context-mode\sessions\state.db",
            r"\\server\share\state\data.db",
            r"C:\state\com10.db",
            r"C:\state\company.txt",
            r"C:\state\reports\évent-¹.db",
        ] {
            validate_components(Path::new(path)).unwrap();
        }
        for path in [
            r"C:relative",
            r"C:\state\..\outside",
            r"C:\state\file:secret",
            r"C:\state\NUL",
            r"C:\state\COM1.db",
            r"C:\state\com¹.db",
            r"C:\state\COM²",
            r"C:\state\Com³.log",
            r"C:\state\lpt¹.db",
            r"C:\state\LPT²",
            r"C:\state\LpT³.log",
            r"C:\state\NUL .db",
            r"C:\state\CONIN$",
            r"C:\state\conout$",
            "C:\\state\\control\u{1f}.db",
            r"C:\state\wildcard*.db",
            r"C:\state\question?.db",
            r"C:\state\file|pipe.db",
            r"C:\state\less<than.db",
            r"C:\state\greater>than.db",
            r#"C:\state\quote".db"#,
            r"C:\state\file.",
            r"\\.\pipe\secret",
            r"\\?\C:\state\data.db",
        ] {
            assert!(validate_components(Path::new(path)).is_err(), "{path}");
        }
    }
}
