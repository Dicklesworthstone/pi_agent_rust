//! Session Store V2 segmented append log + sidecar index primitives.
//!
//! This module provides the storage core requested by Phase-2 performance work:
//! - Segment append writer
//! - Sidecar offset index rows
//! - Reader helpers
//! - Integrity validation (checksum + payload hash)

use crate::error::{Error, Result};
use crate::session::SessionEntry;
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
enum ArtifactWriteMode {
    Append,
    CreateNew,
    Preserve,
    Replace,
}

fn reject_non_private_regular_file(path: &Path, metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file() {
        return Err(Error::session(format!(
            "expected a regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::session(format!(
                "session artifact has non-private permissions: {}",
                path.display()
            )));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(Error::session(format!(
                "session artifact is a Windows reparse point: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn metadata_identity_matches(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ArtifactFileIdentity {
    #[cfg(unix)]
    Unix { device: u64, inode: u64 },
    #[cfg(windows)]
    Windows {
        volume_serial_number: Option<u32>,
        file_index: Option<u64>,
        creation_time: u64,
    },
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

fn artifact_file_identity(metadata: &fs::Metadata) -> ArtifactFileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        ArtifactFileIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        ArtifactFileIdentity::Windows {
            volume_serial_number: metadata.volume_serial_number(),
            file_index: metadata.file_index(),
            creation_time: metadata.creation_time(),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        ArtifactFileIdentity::Unsupported
    }
}

fn artifact_file_identity_matches(
    observed: Option<&ArtifactFileIdentity>,
    current: Option<&ArtifactFileIdentity>,
) -> bool {
    #[cfg(any(unix, windows))]
    {
        observed == current
    }
    #[cfg(not(any(unix, windows)))]
    {
        observed.is_none() && current.is_none()
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsArtifactDirectoryGuard {
    path: PathBuf,
    identity: ArtifactFileIdentity,
    handle: File,
}

#[cfg(windows)]
fn validate_windows_directory_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> std::io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "session artifact path traverses a non-directory or Windows reparse point: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_windows_artifact_directory_guards(
    guards: &[WindowsArtifactDirectoryGuard],
) -> std::io::Result<()> {
    for guard in guards {
        let handle_metadata = guard.handle.metadata()?;
        let path_metadata = fs::symlink_metadata(&guard.path)?;
        validate_windows_directory_metadata(&guard.path, &handle_metadata)?;
        validate_windows_directory_metadata(&guard.path, &path_metadata)?;
        if artifact_file_identity(&handle_metadata) != guard.identity
            || artifact_file_identity(&path_metadata) != guard.identity
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "session artifact directory changed while it was pinned: {}",
                    guard.path.display()
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn open_or_create_windows_artifact_directory_components(
    path: &Path,
    create: bool,
) -> std::io::Result<(PathBuf, Vec<WindowsArtifactDirectoryGuard>)> {
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
    let mut current = PathBuf::new();
    let mut guards = Vec::new();

    for component in absolute_path.components() {
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
                        "secure session artifact paths must not contain parent components: {}",
                        path.display()
                    ),
                ));
            }
            Component::Normal(name) => current.push(name),
        }

        let initial_metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                fs::symlink_metadata(&current)?
            }
            Err(error) => return Err(error),
        };
        validate_windows_directory_metadata(&current, &initial_metadata)?;
        let initial_identity = artifact_file_identity(&initial_metadata);

        // FILE_SHARE_DELETE is intentionally omitted. Retaining each handle pins
        // every traversed component against rename/replacement until the caller's
        // path-based operation has completed.
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&current)?;
        let opened_metadata = handle.metadata()?;
        validate_windows_directory_metadata(&current, &opened_metadata)?;
        if artifact_file_identity(&opened_metadata) != initial_identity {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "session artifact directory changed while it was being opened: {}",
                    current.display()
                ),
            ));
        }
        guards.push(WindowsArtifactDirectoryGuard {
            path: current.clone(),
            identity: initial_identity,
            handle,
        });
    }

    validate_windows_artifact_directory_guards(&guards)?;
    Ok((absolute_path, guards))
}

#[cfg(windows)]
fn open_or_create_windows_artifact_parent(
    path: &Path,
    create: bool,
) -> std::io::Result<(PathBuf, Vec<WindowsArtifactDirectoryGuard>)> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = absolute_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let (_, guards) = open_or_create_windows_artifact_directory_components(parent, create)?;
    Ok((absolute_path, guards))
}

#[cfg(windows)]
fn validate_windows_regular_file_path_matches(
    path: &Path,
    opened_file: &File,
    operation: &str,
) -> Result<()> {
    let current_metadata = fs::symlink_metadata(path)?;
    let opened_metadata = opened_file.metadata()?;
    reject_non_private_regular_file(path, &current_metadata)?;
    reject_non_private_regular_file(path, &opened_metadata)?;
    if artifact_file_identity(&current_metadata) != artifact_file_identity(&opened_metadata) {
        return Err(Error::session(format!(
            "artifact path changed before {operation}: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn open_nofollow_componentwise(
    path: &Path,
    oflags: rustix::fs::OFlags,
    mode: rustix::fs::Mode,
) -> std::io::Result<File> {
    use std::path::Component;

    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => names.push(name),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "secure artifact paths must not contain parent or prefix components: {}",
                        path.display()
                    ),
                ));
            }
        }
    }

    let base = if path.is_absolute() { "/" } else { "." };
    let descriptor = rustix::fs::open(
        base,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let mut directory = File::from(descriptor);

    for (index, name) in names.iter().enumerate() {
        let is_last = index + 1 == names.len();
        let flags = if is_last {
            oflags | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC
        } else {
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC
        };
        let child = rustix::fs::openat(
            &directory,
            *name,
            flags,
            if is_last {
                mode
            } else {
                rustix::fs::Mode::empty()
            },
        )
        .map_err(std::io::Error::from)?;
        if is_last {
            return Ok(File::from(child));
        }
        directory = File::from(child);
    }

    Ok(directory)
}

#[cfg(target_os = "linux")]
fn open_nofollow(
    path: &Path,
    oflags: rustix::fs::OFlags,
    mode: rustix::fs::Mode,
) -> std::io::Result<File> {
    let descriptor = match rustix::fs::openat2(
        rustix::fs::CWD,
        path,
        oflags | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        mode,
        rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
    ) {
        Ok(descriptor) => descriptor,
        Err(rustix::io::Errno::NOSYS) => {
            return open_nofollow_componentwise(path, oflags, mode);
        }
        Err(error) => return Err(std::io::Error::from(error)),
    };
    Ok(File::from(descriptor))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_nofollow(
    path: &Path,
    oflags: rustix::fs::OFlags,
    mode: rustix::fs::Mode,
) -> std::io::Result<File> {
    open_nofollow_componentwise(path, oflags, mode)
}

fn open_regular_file_for_read(path: &Path) -> Result<Option<File>> {
    #[cfg(windows)]
    let (operation_path, parent_guards) = match open_or_create_windows_artifact_parent(path, false)
    {
        Ok(context) => context,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::Io(Box::new(error))),
    };
    #[cfg(windows)]
    let path = operation_path.as_path();
    let initial_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(Error::Io(Box::new(err))),
    };
    if initial_metadata.file_type().is_symlink() {
        return Err(Error::session(format!(
            "expected a regular non-symlink file: {}",
            path.display()
        )));
    }
    reject_non_private_regular_file(path, &initial_metadata)?;

    #[cfg(unix)]
    {
        let file = open_nofollow(path, rustix::fs::OFlags::RDONLY, rustix::fs::Mode::empty())?;
        let opened_metadata = file.metadata()?;
        reject_non_private_regular_file(path, &opened_metadata)?;
        if !metadata_identity_matches(&initial_metadata, &opened_metadata) {
            return Err(Error::session(format!(
                "file changed while it was being opened: {}",
                path.display()
            )));
        }
        Ok(Some(file))
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        let file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let opened_metadata = file.metadata()?;
        reject_non_private_regular_file(path, &opened_metadata)?;
        let opened_identity = artifact_file_identity(&opened_metadata);
        if artifact_file_identity(&initial_metadata) != opened_identity {
            return Err(Error::session(format!(
                "file changed while it was being opened: {}",
                path.display()
            )));
        }
        let current_metadata = fs::symlink_metadata(path)?;
        reject_non_private_regular_file(path, &current_metadata)?;
        if artifact_file_identity(&current_metadata) != opened_identity {
            return Err(Error::session(format!(
                "file path changed after descriptor open: {}",
                path.display()
            )));
        }
        validate_windows_artifact_directory_guards(&parent_guards)?;
        Ok(Some(file))
    }

    #[cfg(not(any(unix, windows)))]
    {
        Err(Error::session(format!(
            "secure artifact reads are unsupported on this platform: {}",
            path.display()
        )))
    }
}

fn validate_opened_regular_file_for_write(
    path: &Path,
    initial_metadata: Option<&fs::Metadata>,
    file: &File,
) -> Result<()> {
    let opened_metadata = file.metadata()?;
    if !opened_metadata.is_file() {
        return Err(Error::session(format!(
            "opened artifact is not a regular file: {}",
            path.display()
        )));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if initial_metadata
            .is_some_and(|metadata| !metadata_identity_matches(metadata, &opened_metadata))
        {
            return Err(Error::session(format!(
                "artifact changed while it was being opened: {}",
                path.display()
            )));
        }
        let current_metadata = fs::symlink_metadata(path)?;
        if current_metadata.file_type().is_symlink()
            || !metadata_identity_matches(&current_metadata, &opened_metadata)
        {
            return Err(Error::session(format!(
                "artifact path changed after descriptor open: {}",
                path.display()
            )));
        }

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        reject_non_private_regular_file(path, &file.metadata()?)?;
    }

    #[cfg(windows)]
    {
        let opened_identity = artifact_file_identity(&opened_metadata);
        if initial_metadata
            .is_some_and(|metadata| artifact_file_identity(metadata) != opened_identity)
        {
            return Err(Error::session(format!(
                "artifact changed while it was being opened: {}",
                path.display()
            )));
        }
        reject_non_private_regular_file(path, &opened_metadata)?;
        let current_metadata = fs::symlink_metadata(path)?;
        reject_non_private_regular_file(path, &current_metadata)?;
        if artifact_file_identity(&current_metadata) != opened_identity {
            return Err(Error::session(format!(
                "artifact path changed after descriptor open: {}",
                path.display()
            )));
        }
    }

    Ok(())
}

fn open_regular_file_for_write(
    path: &Path,
    create: bool,
    write_mode: ArtifactWriteMode,
) -> Result<File> {
    #[cfg(windows)]
    let (operation_path, parent_guards) = open_or_create_windows_artifact_parent(path, create)?;
    #[cfg(windows)]
    let path = operation_path.as_path();
    let initial_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(Error::session(format!(
                    "refusing to write non-regular or linked artifact: {}",
                    path.display()
                )));
            }
            Some(metadata)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && create => None,
        Err(err) => return Err(Error::Io(Box::new(err))),
    };

    if initial_metadata.is_some() && matches!(write_mode, ArtifactWriteMode::CreateNew) {
        return Err(Error::session(format!(
            "refusing to replace an existing immutable artifact: {}",
            path.display()
        )));
    }

    #[cfg(unix)]
    let file = {
        let mut flags = rustix::fs::OFlags::WRONLY;
        if create {
            flags |= rustix::fs::OFlags::CREATE;
        }
        if matches!(write_mode, ArtifactWriteMode::CreateNew) {
            flags |= rustix::fs::OFlags::EXCL;
        }
        if matches!(write_mode, ArtifactWriteMode::Append) {
            flags |= rustix::fs::OFlags::APPEND;
        }
        let mode = if create {
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR
        } else {
            // Linux openat2(2), like openat(2), requires mode to be zero when
            // neither O_CREAT nor O_TMPFILE is present.
            rustix::fs::Mode::empty()
        };
        open_nofollow(path, flags, mode)?
    };

    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        let mut options = fs::OpenOptions::new();
        options.write(true);
        if matches!(write_mode, ArtifactWriteMode::CreateNew)
            || (create && initial_metadata.is_none())
        {
            options.create_new(true);
        } else {
            options.create(create);
        }
        if matches!(write_mode, ArtifactWriteMode::Append) {
            options.append(true);
        }
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?
    };

    #[cfg(not(any(unix, windows)))]
    let file = return Err(Error::session(format!(
        "secure artifact writes are unsupported on this platform: {}",
        path.display()
    )));

    validate_opened_regular_file_for_write(path, initial_metadata.as_ref(), &file)?;
    #[cfg(windows)]
    validate_windows_artifact_directory_guards(&parent_guards)?;
    if matches!(write_mode, ArtifactWriteMode::Replace) {
        file.set_len(0)?;
    }
    Ok(file)
}

fn open_directory_nofollow(path: &Path, create: bool) -> Result<File> {
    #[cfg(windows)]
    let (operation_path, component_guards) =
        open_or_create_windows_artifact_directory_components(path, create)?;
    #[cfg(windows)]
    let path = operation_path.as_path();
    #[cfg(not(windows))]
    let _ = create;
    let initial_metadata = fs::symlink_metadata(path)?;
    if initial_metadata.file_type().is_symlink() || !initial_metadata.is_dir() {
        return Err(Error::session(format!(
            "expected a real session-store directory: {}",
            path.display()
        )));
    }

    #[cfg(unix)]
    let directory = open_nofollow(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
        rustix::fs::Mode::empty(),
    )?;

    #[cfg(windows)]
    let directory = {
        component_guards
            .last()
            .ok_or_else(|| {
                Error::session(format!(
                    "session-store directory has no pinnable component: {}",
                    path.display()
                ))
            })?
            .handle
            .try_clone()?
    };

    #[cfg(not(any(unix, windows)))]
    let directory = return Err(Error::session(format!(
        "secure artifact directories are unsupported on this platform: {}",
        path.display()
    )));

    let opened_metadata = directory.metadata()?;
    if !opened_metadata.is_dir() {
        return Err(Error::session(format!(
            "opened session-store path is not a directory: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        if !metadata_identity_matches(&initial_metadata, &opened_metadata) {
            return Err(Error::session(format!(
                "session-store directory changed while opening: {}",
                path.display()
            )));
        }
    }
    #[cfg(windows)]
    {
        validate_windows_directory_metadata(path, &initial_metadata)?;
        validate_windows_directory_metadata(path, &opened_metadata)?;
        let opened_identity = artifact_file_identity(&opened_metadata);
        if artifact_file_identity(&initial_metadata) != opened_identity {
            return Err(Error::session(format!(
                "session-store directory changed while opening: {}",
                path.display()
            )));
        }
        let current_metadata = fs::symlink_metadata(path)?;
        validate_windows_directory_metadata(path, &current_metadata)?;
        if artifact_file_identity(&current_metadata) != opened_identity {
            return Err(Error::session(format!(
                "session-store directory path changed after descriptor open: {}",
                path.display()
            )));
        }
        validate_windows_artifact_directory_guards(&component_guards)?;
    }
    Ok(directory)
}

#[cfg(unix)]
fn create_directory_tree_nofollow(path: &Path) -> Result<()> {
    if path_entry_exists(path)? {
        drop(open_directory_nofollow(path, false)?);
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| Error::session(format!("directory has no filename: {}", path.display())))?;
    create_directory_tree_nofollow(parent)?;
    let parent_directory = open_directory_nofollow(parent, false)?;
    match rustix::fs::mkdirat(
        &parent_directory,
        name,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR | rustix::fs::Mode::XUSR,
    ) {
        Ok(()) => Ok(()),
        Err(err) if std::io::Error::from(err).kind() == std::io::ErrorKind::AlreadyExists => {
            drop(open_directory_nofollow(path, false)?);
            Ok(())
        }
        Err(err) => Err(Error::Io(Box::new(std::io::Error::from(err)))),
    }
}

fn open_private_directory(path: &Path, create: bool) -> Result<File> {
    #[cfg(unix)]
    if create {
        create_directory_tree_nofollow(path)?;
    }
    let directory = open_directory_nofollow(path, create)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if create {
            directory.set_permissions(fs::Permissions::from_mode(0o700))?;
        } else if directory.metadata()?.permissions().mode() & 0o077 != 0 {
            return Err(Error::session(format!(
                "session-store directory has non-private permissions: {}",
                path.display()
            )));
        }
    }
    Ok(directory)
}

fn validate_private_directory_entry(path: &Path) -> Result<()> {
    let directory = open_directory_nofollow(path, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let metadata = directory.metadata()?;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::session(format!(
                "session-store directory has non-private permissions: {}",
                path.display()
            )));
        }
    }
    #[cfg(not(unix))]
    drop(directory);
    Ok(())
}

fn regular_file_len(path: &Path) -> Result<u64> {
    let file = open_regular_file_for_read(path)?
        .ok_or_else(|| Error::session(format!("artifact not found: {}", path.display())))?;
    Ok(file.metadata()?.len())
}

fn path_entry_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(Error::Io(Box::new(err))),
    }
}

struct PrivateReadDir {
    inner: fs::ReadDir,
    #[cfg(windows)]
    _component_guards: Vec<WindowsArtifactDirectoryGuard>,
}

impl Iterator for PrivateReadDir {
    type Item = std::io::Result<fs::DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

fn read_private_directory(path: &Path) -> std::io::Result<PrivateReadDir> {
    #[cfg(windows)]
    {
        let (operation_path, component_guards) =
            open_or_create_windows_artifact_directory_components(path, false)?;
        let inner = fs::read_dir(&operation_path)?;
        validate_windows_artifact_directory_guards(&component_guards)?;
        Ok(PrivateReadDir {
            inner,
            _component_guards: component_guards,
        })
    }

    #[cfg(not(windows))]
    {
        Ok(PrivateReadDir {
            inner: fs::read_dir(path)?,
        })
    }
}

#[cfg(unix)]
fn reopen_named_regular_file_matching(
    directory: &File,
    name: &std::ffi::OsStr,
    opened_file: &File,
    display_path: &Path,
) -> Result<File> {
    let current = rustix::fs::openat(
        directory,
        name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    let opened_metadata = opened_file.metadata()?;
    let current_metadata = current.metadata()?;
    reject_non_private_regular_file(display_path, &opened_metadata)?;
    reject_non_private_regular_file(display_path, &current_metadata)?;
    if !metadata_identity_matches(&opened_metadata, &current_metadata) {
        return Err(Error::session(format!(
            "artifact source path changed before mutation: {}",
            display_path.display()
        )));
    }
    Ok(current)
}

#[cfg(all(
    unix,
    not(any(target_os = "espidf", target_os = "redox")),
    any(test, not(any(target_os = "linux", target_vendor = "apple")))
))]
fn publish_regular_file_via_hard_link_no_replace(
    source_file: &File,
    source_directory: &File,
    source_name: &std::ffi::OsStr,
    target_directory: &File,
    target_name: &std::ffi::OsStr,
) -> std::io::Result<()> {
    // linkat(2) creates the destination atomically and never replaces an
    // existing name. Filesystems without hard-link support fail before the
    // source is unlinked, which is the only safe fallback when renameat2-style
    // no-replace publication is unavailable.
    use std::os::fd::AsRawFd as _;

    #[cfg(any(target_os = "android", target_os = "linux"))]
    let descriptor_path = PathBuf::from(format!("/proc/self/fd/{}", source_file.as_raw_fd()));
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    let descriptor_path = PathBuf::from(format!("/dev/fd/{}", source_file.as_raw_fd()));
    rustix::fs::linkat(
        rustix::fs::CWD,
        &descriptor_path,
        target_directory,
        target_name,
        rustix::fs::AtFlags::SYMLINK_FOLLOW,
    )
    .map_err(std::io::Error::from)?;
    target_directory.sync_all()?;
    let current_source = rustix::fs::openat(
        source_directory,
        source_name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(std::io::Error::from)?;
    if !metadata_identity_matches(&source_file.metadata()?, &current_source.metadata()?) {
        return Err(std::io::Error::other(
            "artifact source path changed after publication; retained the published target and refused to unlink the replacement",
        ));
    }
    rustix::fs::unlinkat(source_directory, source_name, rustix::fs::AtFlags::empty())
        .map_err(std::io::Error::from)?;
    source_directory.sync_all()?;
    Ok(())
}

#[cfg(target_os = "espidf")]
fn publish_regular_file_via_hard_link_no_replace(
    _source_file: &File,
    _source_directory: &File,
    _source_name: &std::ffi::OsStr,
    _target_directory: &File,
    _target_name: &std::ffi::OsStr,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace artifact publication requires hard-link support",
    ))
}

fn rename_regular_file(source: &Path, target: &Path) -> Result<()> {
    let source_file = open_regular_file_for_read(source)?
        .ok_or_else(|| Error::session(format!("artifact not found: {}", source.display())))?;
    drop(source_file);
    if let Some(target_file) = open_regular_file_for_read(target)? {
        drop(target_file);
    }
    #[cfg(unix)]
    let source_parent = source
        .parent()
        .ok_or_else(|| Error::session("artifact source has no parent directory"))?;
    #[cfg(unix)]
    let target_parent = target
        .parent()
        .ok_or_else(|| Error::session("artifact target has no parent directory"))?;
    #[cfg(unix)]
    let source_name = source
        .file_name()
        .ok_or_else(|| Error::session("artifact source has no filename"))?;
    #[cfg(unix)]
    let target_name = target
        .file_name()
        .ok_or_else(|| Error::session("artifact target has no filename"))?;
    #[cfg(unix)]
    let source_directory = open_private_directory(source_parent, false)?;
    #[cfg(unix)]
    let target_directory = open_private_directory(target_parent, false)?;

    #[cfg(windows)]
    let (source_operation_path, source_parent_guards) =
        open_or_create_windows_artifact_parent(source, false)?;
    #[cfg(windows)]
    let (target_operation_path, target_parent_guards) =
        open_or_create_windows_artifact_parent(target, false)?;

    #[cfg(unix)]
    rustix::fs::renameat(
        &source_directory,
        source_name,
        &target_directory,
        target_name,
    )
    .map_err(std::io::Error::from)?;

    #[cfg(windows)]
    {
        validate_windows_artifact_directory_guards(&source_parent_guards)?;
        validate_windows_artifact_directory_guards(&target_parent_guards)?;
        fs::rename(&source_operation_path, &target_operation_path)?;
        validate_windows_artifact_directory_guards(&source_parent_guards)?;
        validate_windows_artifact_directory_guards(&target_parent_guards)?;
    }

    #[cfg(not(any(unix, windows)))]
    return Err(Error::session("secure artifact rename is unsupported"));

    #[cfg(unix)]
    target_directory.sync_all()?;
    Ok(())
}

fn rename_regular_file_no_replace(source: &Path, target: &Path) -> Result<()> {
    rename_regular_file_no_replace_with(source, target, || Ok(()))
}

fn rename_regular_file_no_replace_with<F>(
    source: &Path,
    target: &Path,
    before_publish: F,
) -> Result<()>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let source_file = open_regular_file_for_read(source)?
        .ok_or_else(|| Error::session(format!("artifact not found: {}", source.display())))?;
    let source_identity = artifact_file_identity(&source_file.metadata()?);
    #[cfg(unix)]
    let source_parent = source
        .parent()
        .ok_or_else(|| Error::session("artifact source has no parent directory"))?;
    #[cfg(unix)]
    let target_parent = target
        .parent()
        .ok_or_else(|| Error::session("artifact target has no parent directory"))?;
    #[cfg(unix)]
    let source_name = source
        .file_name()
        .ok_or_else(|| Error::session("artifact source has no filename"))?;
    #[cfg(unix)]
    let target_name = target
        .file_name()
        .ok_or_else(|| Error::session("artifact target has no filename"))?;
    #[cfg(unix)]
    let source_directory = open_private_directory(source_parent, false)?;
    #[cfg(unix)]
    let target_directory = open_private_directory(target_parent, false)?;

    #[cfg(windows)]
    let (source_operation_path, source_parent_guards) =
        open_or_create_windows_artifact_parent(source, false)?;
    #[cfg(windows)]
    let (target_operation_path, target_parent_guards) =
        open_or_create_windows_artifact_parent(target, false)?;

    before_publish()?;

    #[cfg(unix)]
    let _source_name_guard =
        reopen_named_regular_file_matching(&source_directory, source_name, &source_file, source)?;

    #[cfg(windows)]
    validate_windows_regular_file_path_matches(
        &source_operation_path,
        &source_file,
        "publication",
    )?;

    #[cfg(any(target_os = "linux", target_vendor = "apple", target_os = "redox"))]
    rustix::fs::renameat_with(
        &source_directory,
        source_name,
        &target_directory,
        target_name,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)?;

    #[cfg(all(
        unix,
        not(any(target_os = "linux", target_vendor = "apple", target_os = "redox"))
    ))]
    publish_regular_file_via_hard_link_no_replace(
        &source_file,
        &source_directory,
        source_name,
        &target_directory,
        target_name,
    )?;

    #[cfg(windows)]
    {
        validate_windows_artifact_directory_guards(&source_parent_guards)?;
        validate_windows_artifact_directory_guards(&target_parent_guards)?;
        fs::hard_link(&source_operation_path, &target_operation_path)?;
        validate_windows_regular_file_path_matches(
            &target_operation_path,
            &source_file,
            "identity verification",
        )?;
        drop(source_file);
        fs::remove_file(&source_operation_path)?;
        validate_windows_artifact_directory_guards(&source_parent_guards)?;
        validate_windows_artifact_directory_guards(&target_parent_guards)?;
    }

    #[cfg(not(any(unix, windows)))]
    return Err(Error::session("secure artifact rename is unsupported"));

    #[cfg(any(target_os = "linux", target_vendor = "apple", target_os = "redox"))]
    target_directory.sync_all()?;
    let target_file = open_regular_file_for_read(target)?.ok_or_else(|| {
        Error::session(format!(
            "published artifact disappeared before identity verification: {}",
            target.display()
        ))
    })?;
    if artifact_file_identity(&target_file.metadata()?) != source_identity {
        return Err(Error::session(format!(
            "published artifact identity does not match its retained source: {}",
            target.display()
        )));
    }
    Ok(())
}

fn remove_regular_file(path: &Path) -> Result<()> {
    remove_regular_file_with(path, || Ok(()))
}

fn remove_regular_file_with<F>(path: &Path, before_remove: F) -> Result<()>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let file = open_regular_file_for_read(path)?
        .ok_or_else(|| Error::session(format!("artifact not found: {}", path.display())))?;
    #[cfg(unix)]
    let parent = path
        .parent()
        .ok_or_else(|| Error::session("artifact has no parent directory"))?;
    #[cfg(unix)]
    let name = path
        .file_name()
        .ok_or_else(|| Error::session("artifact has no filename"))?;
    #[cfg(unix)]
    let directory = open_private_directory(parent, false)?;

    #[cfg(windows)]
    let (operation_path, parent_guards) = open_or_create_windows_artifact_parent(path, false)?;

    before_remove()?;

    #[cfg(unix)]
    let _source_name_guard = reopen_named_regular_file_matching(&directory, name, &file, path)?;

    #[cfg(windows)]
    {
        validate_windows_regular_file_path_matches(&operation_path, &file, "removal")?;
        drop(file);
    }

    #[cfg(unix)]
    rustix::fs::unlinkat(&directory, name, rustix::fs::AtFlags::empty())
        .map_err(std::io::Error::from)?;

    #[cfg(windows)]
    {
        validate_windows_artifact_directory_guards(&parent_guards)?;
        fs::remove_file(&operation_path)?;
        validate_windows_artifact_directory_guards(&parent_guards)?;
    }

    #[cfg(not(any(unix, windows)))]
    return Err(Error::session("secure artifact removal is unsupported"));

    #[cfg(unix)]
    directory.sync_all()?;
    Ok(())
}

pub const SEGMENT_FRAME_SCHEMA: &str = "pi.session_store_v2.segment_frame.v1";
pub const OFFSET_INDEX_SCHEMA: &str = "pi.session_store_v2.offset_index.v1";
pub const CHECKPOINT_SCHEMA: &str = "pi.session_store_v2.checkpoint.v1";
pub const MANIFEST_SCHEMA: &str = "pi.session_store_v2.manifest.v1";
pub const MIGRATION_EVENT_SCHEMA: &str = "pi.session_store_v2.migration_event.v1";
const ROLLBACK_INTENT_SCHEMA: &str = "pi.session_store_v2.rollback_intent.v1";

/// Maximum size for a single frame line (100MB) to prevent OOM on corrupted files.
const MAX_FRAME_READ_BYTES: u64 = 100 * 1024 * 1024;
/// Manifests are compact summaries. Bound reads so corrupt input cannot turn
/// session discovery into an unbounded allocation.
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// Checkpoints are control-plane inputs to destructive rollback. Keep both the
/// read and write surface small enough to inspect without an attacker-driven
/// allocation.
const MAX_CHECKPOINT_BYTES: u64 = 1024 * 1024;
/// Rollback intent is a small control-plane record. The potentially large
/// retained index remains in its separately checksummed staged artifact.
const MAX_ROLLBACK_INTENT_BYTES: u64 = 1024 * 1024;
const MAX_CHECKPOINT_REASON_BYTES: usize = 64 * 1024;
const ROLLBACK_PREFLIGHT_REJECTION: &str = "rollback preflight rejected";

/// Initial chain hash before any frames are appended.
const GENESIS_CHAIN_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn checked_frame_read_len(byte_length: u64) -> Result<usize> {
    if byte_length > MAX_FRAME_READ_BYTES {
        return Err(Error::session(format!(
            "frame byte length {byte_length} exceeds absolute read limit {MAX_FRAME_READ_BYTES}"
        )));
    }
    usize::try_from(byte_length)
        .map_err(|_| Error::session(format!("byte length too large: {byte_length}")))
}

fn validate_encoded_frame_length(byte_length: u64, max_segment_bytes: u64) -> Result<()> {
    if byte_length > MAX_FRAME_READ_BYTES {
        return Err(Error::session(format!(
            "encoded frame length {byte_length} exceeds absolute read limit {MAX_FRAME_READ_BYTES}"
        )));
    }
    if byte_length > max_segment_bytes {
        return Err(Error::session(format!(
            "encoded frame length {byte_length} exceeds configured segment limit {max_segment_bytes}"
        )));
    }
    Ok(())
}

fn validate_wire_entry_id(entry_id: &str, field: &str) -> Result<()> {
    if !(1..=128).contains(&entry_id.len())
        || !entry_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(Error::session(format!(
            "{field} must match the V2 entry-id contract"
        )));
    }
    Ok(())
}

fn validate_lower_hex_sha256(digest: &str, field: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::session(format!(
            "{field} must be a lowercase SHA-256 hex digest"
        )));
    }
    Ok(())
}

fn validate_upper_hex_crc32c(checksum: &str, field: &str) -> Result<()> {
    if checksum.len() != 8
        || !checksum
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
    {
        return Err(Error::session(format!(
            "{field} must be an uppercase CRC32C hex checksum"
        )));
    }
    Ok(())
}

fn validate_rfc3339_timestamp(timestamp: &str, field: &str) -> Result<()> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map_err(|_| Error::session(format!("{field} must be an RFC 3339 timestamp")))?;
    Ok(())
}

fn validate_manifest_source_format(source_format: &str) -> Result<()> {
    if !matches!(source_format, "jsonl_v3" | "sqlite_v1" | "native_v2") {
        return Err(Error::session(format!(
            "unsupported manifest sourceFormat: {source_format}"
        )));
    }
    Ok(())
}

fn validate_manifest_session_id(session_id: &str) -> Result<()> {
    uuid::Uuid::parse_str(session_id)
        .map_err(|_| Error::session("manifest sessionId must be a UUID"))?;
    Ok(())
}

fn validate_migration_id(migration_id: &str) -> Result<()> {
    uuid::Uuid::parse_str(migration_id)
        .map_err(|_| Error::session("migrationId must be a UUID"))?;
    Ok(())
}

fn validate_correlation_id(correlation_id: &str) -> Result<()> {
    if !(8..=128).contains(&correlation_id.len())
        || !correlation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(Error::session(
            "correlationId must match the V2 migration-event contract",
        ));
    }
    Ok(())
}

fn validate_migration_event_document(event: &MigrationEvent) -> Result<()> {
    if event.schema != MIGRATION_EVENT_SCHEMA {
        return Err(Error::session(format!(
            "unsupported migration-event schema: {}",
            event.schema
        )));
    }
    validate_migration_id(&event.migration_id)?;
    if !matches!(
        event.phase.as_str(),
        "planned"
            | "staging_copy"
            | "integrity_check"
            | "cutover_commit"
            | "rollback"
            | "completed"
            | "failed"
    ) {
        return Err(Error::session(format!(
            "unsupported migration-event phase: {}",
            event.phase
        )));
    }
    validate_rfc3339_timestamp(&event.at, "migration-event at")?;
    if event.source_path.is_empty() || event.target_path.is_empty() {
        return Err(Error::session(
            "migration-event sourcePath and targetPath must be nonempty",
        ));
    }
    validate_manifest_source_format(&event.source_format)?;
    validate_manifest_source_format(&event.target_format)?;
    if !matches!(
        event.outcome.as_str(),
        "ok" | "recoverable_error" | "fatal_error"
    ) {
        return Err(Error::session(format!(
            "unsupported migration-event outcome: {}",
            event.outcome
        )));
    }
    if let Some(error_class) = event.error_class.as_deref()
        && !matches!(
            error_class,
            "integrity_mismatch" | "index_corruption" | "io_failure" | "atomicity_violation"
        )
    {
        return Err(Error::session(format!(
            "unsupported migration-event errorClass: {error_class}"
        )));
    }
    validate_correlation_id(&event.correlation_id)
}

fn validate_checkpoint_reason(reason: &str) -> Result<()> {
    if reason.len() > MAX_CHECKPOINT_REASON_BYTES {
        return Err(Error::session(format!(
            "checkpoint reason is {} bytes; limit is {MAX_CHECKPOINT_REASON_BYTES}",
            reason.len()
        )));
    }
    if !matches!(
        reason,
        "periodic" | "manual" | "pre_migration" | "post_migration" | "recovery"
    ) {
        return Err(Error::session(format!(
            "unsupported checkpoint reason: {reason}"
        )));
    }
    Ok(())
}

fn validate_manifest_encoded_length(byte_length: u64) -> Result<()> {
    if byte_length > MAX_MANIFEST_BYTES {
        return Err(Error::session(format!(
            "serialized manifest is {byte_length} bytes; limit is {MAX_MANIFEST_BYTES}"
        )));
    }
    Ok(())
}

fn validate_checkpoint_encoded_length(byte_length: u64) -> Result<()> {
    if byte_length > MAX_CHECKPOINT_BYTES {
        return Err(Error::session(format!(
            "serialized checkpoint is {byte_length} bytes; limit is {MAX_CHECKPOINT_BYTES}"
        )));
    }
    Ok(())
}

fn validate_rollback_intent_encoded_length(byte_length: u64) -> Result<()> {
    if byte_length > MAX_ROLLBACK_INTENT_BYTES {
        return Err(Error::session(format!(
            "serialized rollback intent is {byte_length} bytes; limit is {MAX_ROLLBACK_INTENT_BYTES}"
        )));
    }
    Ok(())
}

fn validate_checkpoint_document(
    checkpoint: &Checkpoint,
    expected_checkpoint_seq: u64,
    path: &Path,
) -> Result<()> {
    if checkpoint.schema != CHECKPOINT_SCHEMA {
        return Err(Error::session(format!(
            "unsupported checkpoint schema in {}: {}",
            path.display(),
            checkpoint.schema
        )));
    }
    if checkpoint.checkpoint_seq != expected_checkpoint_seq {
        return Err(Error::session(format!(
            "checkpoint sequence does not match requested sequence or filename in {}",
            path.display()
        )));
    }
    if checkpoint.checkpoint_seq == 0 {
        return Err(Error::session(format!(
            "checkpoint sequence must be positive in {}",
            path.display()
        )));
    }
    validate_rfc3339_timestamp(&checkpoint.at, "checkpoint at")?;
    if checkpoint.head_entry_seq == 0 {
        return Err(Error::session(format!(
            "checkpoint head sequence must be positive in {}",
            path.display()
        )));
    }
    validate_wire_entry_id(&checkpoint.head_entry_id, "checkpoint headEntryId")?;
    let expected_snapshot_ref = format!("checkpoints/{expected_checkpoint_seq:016}.json");
    if checkpoint.snapshot_ref != expected_snapshot_ref {
        return Err(Error::session(format!(
            "checkpoint snapshotRef is not canonical in {}",
            path.display()
        )));
    }
    if checkpoint.compacted_before_entry_seq > checkpoint.head_entry_seq {
        return Err(Error::session(format!(
            "checkpoint compacted boundary exceeds its head in {}",
            path.display()
        )));
    }
    validate_lower_hex_sha256(&checkpoint.chain_hash, "checkpoint chainHash")?;
    validate_checkpoint_reason(&checkpoint.reason)?;
    Ok(())
}

fn read_checkpoint_document(
    path: &Path,
    expected_checkpoint_seq: u64,
) -> Result<Option<Checkpoint>> {
    let Some(file) = open_regular_file_for_read(path)? else {
        return Ok(None);
    };
    let mut content = Vec::new();
    file.take(MAX_CHECKPOINT_BYTES.saturating_add(1))
        .read_to_end(&mut content)?;
    if validate_checkpoint_encoded_length(u64::try_from(content.len()).unwrap_or(u64::MAX)).is_err()
    {
        return Err(Error::session(format!(
            "checkpoint {} exceeds the {MAX_CHECKPOINT_BYTES} byte read limit",
            path.display()
        )));
    }
    let checkpoint: Checkpoint = serde_json::from_slice(&content).map_err(|err| {
        Error::session(format!(
            "failed to parse checkpoint {}: {err}",
            path.display()
        ))
    })?;
    validate_checkpoint_document(&checkpoint, expected_checkpoint_seq, path)?;
    Ok(Some(checkpoint))
}

fn validate_rollback_intent_document(intent: &RollbackIntent, path: &Path) -> Result<()> {
    if intent.schema != ROLLBACK_INTENT_SCHEMA {
        return Err(Error::session(format!(
            "unsupported rollback-intent schema in {}: {}",
            path.display(),
            intent.schema
        )));
    }
    validate_checkpoint_document(&intent.checkpoint, intent.checkpoint.checkpoint_seq, path)?;
    validate_migration_id(&intent.migration_id)?;
    validate_correlation_id(&intent.correlation_id)?;
    validate_lower_hex_sha256(
        &intent.retained_index_sha256,
        "rollback retainedIndexSha256",
    )?;
    if intent.retained_index_bytes == 0 {
        return Err(Error::session(
            "rollback retainedIndexBytes must be positive",
        ));
    }
    if let Some(manifest) = &intent.manifest {
        validate_manifest_session_id(&manifest.session_id)?;
        validate_manifest_source_format(&manifest.source_format)?;
        validate_rfc3339_timestamp(&manifest.created_at, "rollback manifest createdAt")?;
    }
    Ok(())
}

fn rollback_preflight_error(checkpoint_seq: u64, error: &Error) -> Error {
    Error::session(format!(
        "{ROLLBACK_PREFLIGHT_REJECTION} checkpoint {checkpoint_seq}: {error}"
    ))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct SegmentFrame {
    pub schema: Cow<'static, str>,
    pub segment_seq: u64,
    pub frame_seq: u64,
    pub entry_seq: u64,
    pub entry_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_entry_id: Option<String>,
    pub entry_type: String,
    pub timestamp: String,
    pub payload_sha256: String,
    pub payload_bytes: u64,
    pub payload: Box<RawValue>,
}

impl SegmentFrame {
    fn new(
        segment_seq: u64,
        frame_seq: u64,
        entry_seq: u64,
        entry_id: String,
        parent_entry_id: Option<String>,
        entry_type: String,
        payload: Box<RawValue>,
    ) -> Result<Self> {
        let (payload_sha256, payload_bytes) = payload_hash_and_size(&payload)?;
        Ok(Self {
            schema: Cow::Borrowed(SEGMENT_FRAME_SCHEMA),
            segment_seq,
            frame_seq,
            entry_seq,
            entry_id,
            parent_entry_id,
            entry_type,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            payload_sha256,
            payload_bytes,
            payload,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct OffsetIndexEntry {
    pub schema: Cow<'static, str>,
    pub entry_seq: u64,
    pub entry_id: String,
    pub segment_seq: u64,
    pub frame_seq: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub crc32c: String,
    pub state: Cow<'static, str>,
}

/// Current head position of the store (last written entry).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct StoreHead {
    pub segment_seq: u64,
    pub entry_seq: u64,
    pub entry_id: String,
}

/// Periodic checkpoint snapshot metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub schema: String,
    pub checkpoint_seq: u64,
    pub at: String,
    pub head_entry_seq: u64,
    pub head_entry_id: String,
    pub snapshot_ref: String,
    pub compacted_before_entry_seq: u64,
    pub chain_hash: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RollbackManifestContext {
    session_id: String,
    source_format: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RollbackIntent {
    schema: String,
    checkpoint: Checkpoint,
    migration_id: String,
    correlation_id: String,
    retained_index_sha256: String,
    retained_index_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    manifest: Option<RollbackManifestContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: String,
    pub store_version: u8,
    pub session_id: String,
    pub source_format: String,
    pub created_at: String,
    pub updated_at: String,
    pub head: StoreHead,
    pub counters: ManifestCounters,
    pub files: ManifestFiles,
    pub integrity: ManifestIntegrity,
    pub invariants: ManifestInvariants,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ManifestCounters {
    pub entries_total: u64,
    pub messages_total: u64,
    pub branches_total: u64,
    pub compactions_total: u64,
    pub bytes_total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ManifestFiles {
    pub segment_dir: String,
    pub segment_count: u64,
    pub index_path: String,
    pub checkpoint_dir: String,
    pub migration_ledger_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct ManifestIntegrity {
    pub chain_hash: String,
    pub manifest_hash: String,
    pub last_crc32c: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // invariants are naturally boolean checks
pub struct ManifestInvariants {
    pub parent_links_closed: bool,
    pub monotonic_entry_seq: bool,
    pub monotonic_segment_seq: bool,
    pub index_within_segment_bounds: bool,
    pub branch_heads_indexed: bool,
    pub checkpoints_monotonic: bool,
    pub hash_chain_valid: bool,
}

struct ManifestStoreFacts {
    entries_total: u64,
    messages_total: u64,
    branches_total: u64,
    compactions_total: u64,
    bytes_total: u64,
    segment_count: u64,
    head: StoreHead,
    chain_hash: String,
    last_crc32c: String,
}

struct ManifestIndexFacts {
    entries_total: u64,
    bytes_total: u64,
    segment_count: u64,
    head: StoreHead,
    last_crc32c: String,
}

fn validate_manifest_document(manifest: &Manifest, path: &Path) -> Result<()> {
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(Error::session(format!(
            "unsupported manifest schema in {}: {}",
            path.display(),
            manifest.schema
        )));
    }
    if manifest.store_version != 2 {
        return Err(Error::session(format!(
            "unsupported manifest storeVersion in {}: {}",
            path.display(),
            manifest.store_version
        )));
    }
    validate_manifest_session_id(&manifest.session_id)?;
    validate_manifest_source_format(&manifest.source_format)?;
    validate_rfc3339_timestamp(&manifest.created_at, "manifest createdAt")?;
    validate_rfc3339_timestamp(&manifest.updated_at, "manifest updatedAt")?;
    if manifest.counters.entries_total == 0 {
        if manifest.head.segment_seq != 0
            || manifest.head.entry_seq != 0
            || !manifest.head.entry_id.is_empty()
            || manifest.files.segment_count != 0
        {
            return Err(Error::session(format!(
                "empty manifest head/files coupling mismatch in {}",
                path.display()
            )));
        }
    } else {
        if manifest.head.segment_seq == 0
            || manifest.head.entry_seq == 0
            || manifest.files.segment_count == 0
        {
            return Err(Error::session(format!(
                "non-empty manifest head/files coupling mismatch in {}",
                path.display()
            )));
        }
        validate_wire_entry_id(&manifest.head.entry_id, "manifest head.entryId")?;
    }
    validate_lower_hex_sha256(&manifest.integrity.chain_hash, "manifest chainHash")?;
    validate_lower_hex_sha256(&manifest.integrity.manifest_hash, "manifest manifestHash")?;
    validate_upper_hex_crc32c(&manifest.integrity.last_crc32c, "manifest lastCrc32c")?;
    for (field, actual, expected) in [
        (
            "files.segmentDir",
            manifest.files.segment_dir.as_str(),
            "segments/",
        ),
        (
            "files.indexPath",
            manifest.files.index_path.as_str(),
            "index/offsets.jsonl",
        ),
        (
            "files.checkpointDir",
            manifest.files.checkpoint_dir.as_str(),
            "checkpoints/",
        ),
        (
            "files.migrationLedgerPath",
            manifest.files.migration_ledger_path.as_str(),
            "migrations/ledger.jsonl",
        ),
    ] {
        if actual != expected {
            return Err(Error::session(format!(
                "manifest {field} mismatch in {}: expected={expected} actual={actual}",
                path.display()
            )));
        }
    }

    let expected_hash = manifest.integrity.manifest_hash.clone();
    let mut canonical_manifest = manifest.clone();
    canonical_manifest.integrity.manifest_hash.clear();
    let actual_hash = manifest_hash_hex(&canonical_manifest)?;
    if expected_hash != actual_hash {
        return Err(Error::session(format!(
            "manifest hash mismatch in {}: expected={} actual={actual_hash}",
            path.display(),
            expected_hash
        )));
    }
    Ok(())
}

fn validate_manifest_invariants(manifest: &Manifest) -> Result<()> {
    for (field, valid) in [
        (
            "invariants.parentLinksClosed",
            manifest.invariants.parent_links_closed,
        ),
        (
            "invariants.monotonicEntrySeq",
            manifest.invariants.monotonic_entry_seq,
        ),
        (
            "invariants.monotonicSegmentSeq",
            manifest.invariants.monotonic_segment_seq,
        ),
        (
            "invariants.indexWithinSegmentBounds",
            manifest.invariants.index_within_segment_bounds,
        ),
        (
            "invariants.branchHeadsIndexed",
            manifest.invariants.branch_heads_indexed,
        ),
        (
            "invariants.checkpointsMonotonic",
            manifest.invariants.checkpoints_monotonic,
        ),
        (
            "invariants.hashChainValid",
            manifest.invariants.hash_chain_valid,
        ),
    ] {
        if !valid {
            return Err(Error::session(format!("manifest {field} is false")));
        }
    }
    Ok(())
}

fn derive_manifest_index_facts(index: &[OffsetIndexEntry]) -> Result<ManifestIndexFacts> {
    let entries_total = u64::try_from(index.len())
        .map_err(|_| Error::session("manifest entry count exceeds u64"))?;
    let bytes_total = index.iter().try_fold(0u64, |total, row| {
        total
            .checked_add(row.byte_length)
            .ok_or_else(|| Error::session("manifest byte count overflow"))
    })?;
    let mut segment_count = 0u64;
    let mut previous_segment_seq = None;
    for row in index {
        if previous_segment_seq != Some(row.segment_seq) {
            segment_count = segment_count
                .checked_add(1)
                .ok_or_else(|| Error::session("manifest segment count overflow"))?;
            previous_segment_seq = Some(row.segment_seq);
        }
    }
    let head = index.last().map_or(
        StoreHead {
            segment_seq: 0,
            entry_seq: 0,
            entry_id: String::new(),
        },
        |row| StoreHead {
            segment_seq: row.segment_seq,
            entry_seq: row.entry_seq,
            entry_id: row.entry_id.clone(),
        },
    );
    let last_crc32c = index
        .last()
        .map_or_else(|| "00000000".to_string(), |row| row.crc32c.clone());

    Ok(ManifestIndexFacts {
        entries_total,
        bytes_total,
        segment_count,
        head,
        last_crc32c,
    })
}

fn validate_resume_manifest_counters(manifest: &Manifest) -> Result<()> {
    let entries_total = manifest.counters.entries_total;
    for (field, count) in [
        ("counters.messagesTotal", manifest.counters.messages_total),
        ("counters.branchesTotal", manifest.counters.branches_total),
        (
            "counters.compactionsTotal",
            manifest.counters.compactions_total,
        ),
    ] {
        if count > entries_total {
            return Err(Error::session(format!(
                "manifest {field} exceeds counters.entriesTotal: count={count} entries={entries_total}"
            )));
        }
    }
    let classified_entries = manifest
        .counters
        .messages_total
        .checked_add(manifest.counters.compactions_total)
        .ok_or_else(|| Error::session("manifest classified-entry counter overflow"))?;
    if classified_entries > entries_total {
        return Err(Error::session(format!(
            "manifest message and compaction counters exceed counters.entriesTotal: classified={classified_entries} entries={entries_total}"
        )));
    }
    Ok(())
}

fn validate_resume_manifest_index_facts(
    manifest: &Manifest,
    facts: &ManifestIndexFacts,
) -> Result<()> {
    for (field, expected, actual) in [
        (
            "counters.entriesTotal",
            facts.entries_total,
            manifest.counters.entries_total,
        ),
        (
            "counters.bytesTotal",
            facts.bytes_total,
            manifest.counters.bytes_total,
        ),
        (
            "files.segmentCount",
            facts.segment_count,
            manifest.files.segment_count,
        ),
    ] {
        if actual != expected {
            return Err(manifest_mismatch(field, &expected, &actual));
        }
    }
    if manifest.head != facts.head {
        return Err(Error::session(format!(
            "manifest head does not match the current index: expected={:?} actual={:?}",
            facts.head, manifest.head
        )));
    }
    if manifest.integrity.last_crc32c != facts.last_crc32c {
        return Err(manifest_mismatch(
            "integrity.lastCrc32c",
            &facts.last_crc32c,
            &manifest.integrity.last_crc32c,
        ));
    }
    if facts.entries_total == 0 && manifest.integrity.chain_hash != GENESIS_CHAIN_HASH {
        return Err(manifest_mismatch(
            "integrity.chainHash",
            &GENESIS_CHAIN_HASH,
            &manifest.integrity.chain_hash,
        ));
    }
    validate_resume_manifest_counters(manifest)
}

fn derive_manifest_store_facts(
    index: &[OffsetIndexEntry],
    frames: &[SegmentFrame],
) -> Result<ManifestStoreFacts> {
    let entries_total = u64::try_from(index.len())
        .map_err(|_| Error::session("manifest entry count exceeds u64"))?;
    let messages_total = u64::try_from(
        frames
            .iter()
            .filter(|frame| frame.entry_type == "message")
            .count(),
    )
    .map_err(|_| Error::session("manifest message count exceeds u64"))?;
    let compactions_total = u64::try_from(
        frames
            .iter()
            .filter(|frame| frame.entry_type == "compaction")
            .count(),
    )
    .map_err(|_| Error::session("manifest compaction count exceeds u64"))?;
    let bytes_total = index.iter().try_fold(0u64, |total, row| {
        total
            .checked_add(row.byte_length)
            .ok_or_else(|| Error::session("manifest byte count overflow"))
    })?;
    let segment_count = u64::try_from(
        index
            .iter()
            .map(|row| row.segment_seq)
            .collect::<BTreeSet<_>>()
            .len(),
    )
    .map_err(|_| Error::session("manifest segment count exceeds u64"))?;

    let mut child_counts = std::collections::HashMap::<&str, u64>::new();
    for frame in frames {
        if let Some(parent_id) = frame.parent_entry_id.as_deref() {
            let count = child_counts.entry(parent_id).or_default();
            *count = count
                .checked_add(1)
                .ok_or_else(|| Error::session("manifest branch count overflow"))?;
        }
    }
    let branches_total = u64::try_from(
        child_counts
            .values()
            .filter(|&&child_count| child_count > 1)
            .count(),
    )
    .map_err(|_| Error::session("manifest branch count exceeds u64"))?;

    let head = index.last().map_or(
        StoreHead {
            segment_seq: 0,
            entry_seq: 0,
            entry_id: String::new(),
        },
        |row| StoreHead {
            segment_seq: row.segment_seq,
            entry_seq: row.entry_seq,
            entry_id: row.entry_id.clone(),
        },
    );
    let chain_hash = frames
        .iter()
        .fold(GENESIS_CHAIN_HASH.to_string(), |chain, frame| {
            chain_hash_step(&chain, &frame.payload_sha256)
        });
    let last_crc32c = index
        .last()
        .map_or_else(|| "00000000".to_string(), |row| row.crc32c.clone());

    Ok(ManifestStoreFacts {
        entries_total,
        messages_total,
        branches_total,
        compactions_total,
        bytes_total,
        segment_count,
        head,
        chain_hash,
        last_crc32c,
    })
}

fn manifest_mismatch(
    field: &str,
    expected: &impl std::fmt::Display,
    actual: &impl std::fmt::Display,
) -> Error {
    Error::session(format!(
        "manifest {field} mismatch: expected={expected} actual={actual}"
    ))
}

fn validate_manifest_store_facts(manifest: &Manifest, facts: &ManifestStoreFacts) -> Result<()> {
    for (field, expected, actual) in [
        (
            "counters.entriesTotal",
            facts.entries_total,
            manifest.counters.entries_total,
        ),
        (
            "counters.messagesTotal",
            facts.messages_total,
            manifest.counters.messages_total,
        ),
        (
            "counters.branchesTotal",
            facts.branches_total,
            manifest.counters.branches_total,
        ),
        (
            "counters.compactionsTotal",
            facts.compactions_total,
            manifest.counters.compactions_total,
        ),
        (
            "counters.bytesTotal",
            facts.bytes_total,
            manifest.counters.bytes_total,
        ),
        (
            "files.segmentCount",
            facts.segment_count,
            manifest.files.segment_count,
        ),
    ] {
        if actual != expected {
            return Err(manifest_mismatch(field, &expected, &actual));
        }
    }
    if manifest.head != facts.head {
        return Err(Error::session(format!(
            "manifest head does not match the current index: expected={:?} actual={:?}",
            facts.head, manifest.head
        )));
    }
    if manifest.integrity.chain_hash != facts.chain_hash {
        return Err(manifest_mismatch(
            "integrity.chainHash",
            &facts.chain_hash,
            &manifest.integrity.chain_hash,
        ));
    }
    if manifest.integrity.last_crc32c != facts.last_crc32c {
        return Err(manifest_mismatch(
            "integrity.lastCrc32c",
            &facts.last_crc32c,
            &manifest.integrity.last_crc32c,
        ));
    }
    Ok(())
}

fn group_validated_index_rows(
    index_rows: &[OffsetIndexEntry],
) -> Result<std::collections::BTreeMap<u64, Vec<&OffsetIndexEntry>>> {
    let mut last_entry_seq = 0u64;
    let mut last_segment_seq = 0u64;
    let mut entry_ids = std::collections::HashSet::with_capacity(index_rows.len());
    let mut rows_by_segment: std::collections::BTreeMap<u64, Vec<&OffsetIndexEntry>> =
        std::collections::BTreeMap::new();

    for row in index_rows {
        validate_offset_index_row_document(row)?;
        if !entry_ids.insert(row.entry_id.as_str()) {
            return Err(Error::session(format!(
                "duplicate entry_id detected in offset index: {}",
                row.entry_id
            )));
        }
        let expected_entry_seq = last_entry_seq
            .checked_add(1)
            .ok_or_else(|| Error::session("entry sequence overflow in offset index"))?;
        if row.entry_seq != expected_entry_seq {
            return Err(Error::session(format!(
                "entry sequence is not contiguous: expected={expected_entry_seq} actual={}",
                row.entry_seq,
            )));
        }
        if row.segment_seq == 0 || row.segment_seq < last_segment_seq {
            return Err(Error::session(format!(
                "segment sequence is not positive and monotonic at entry_seq={}: {}",
                row.entry_seq, row.segment_seq
            )));
        }
        if row.frame_seq == 0 {
            return Err(Error::session(format!(
                "frame sequence must be positive at entry_seq={}",
                row.entry_seq
            )));
        }
        last_entry_seq = row.entry_seq;
        last_segment_seq = row.segment_seq;
        rows_by_segment
            .entry(row.segment_seq)
            .or_default()
            .push(row);
    }
    Ok(rows_by_segment)
}

fn validate_offset_index_row_document(row: &OffsetIndexEntry) -> Result<()> {
    if row.schema.as_ref() != OFFSET_INDEX_SCHEMA {
        return Err(Error::session(format!(
            "unsupported offset-index schema at entry_seq={}: {}",
            row.entry_seq, row.schema
        )));
    }
    if row.state.as_ref() != "active" {
        return Err(Error::session(format!(
            "unsupported offset-index state at entry_seq={}: {}",
            row.entry_seq, row.state
        )));
    }
    if row.entry_seq == 0 {
        return Err(Error::session(
            "offset-index entry sequence must be positive",
        ));
    }
    if row.segment_seq == 0 {
        return Err(Error::session(format!(
            "offset-index segment sequence must be positive at entry_seq={}",
            row.entry_seq
        )));
    }
    if row.frame_seq == 0 {
        return Err(Error::session(format!(
            "offset-index frame sequence must be positive at entry_seq={}",
            row.entry_seq
        )));
    }
    if row.byte_length == 0 {
        return Err(Error::session(format!(
            "offset-index byte length must be positive at entry_seq={}",
            row.entry_seq
        )));
    }
    checked_frame_read_len(row.byte_length)?;
    validate_wire_entry_id(&row.entry_id, "offset-index entryId")?;
    validate_upper_hex_crc32c(&row.crc32c, "offset-index crc32c")?;
    Ok(())
}

fn decode_indexed_frame_record(
    mut record_bytes: Vec<u8>,
    row: &OffsetIndexEntry,
) -> Result<SegmentFrame> {
    validate_offset_index_row_document(row)?;
    let checksum = crc32c_upper(&record_bytes);
    if checksum != row.crc32c {
        return Err(Error::session(format!(
            "checksum mismatch for entry_seq={} expected={} actual={checksum}",
            row.entry_seq, row.crc32c
        )));
    }
    if record_bytes.last() != Some(&b'\n') {
        return Err(Error::session(format!(
            "indexed frame is not LF-terminated in segment {} at entry_seq={}",
            row.segment_seq, row.entry_seq
        )));
    }
    record_bytes.pop();
    let frame: SegmentFrame = serde_json::from_slice(&record_bytes)?;
    if frame.schema.as_ref() != SEGMENT_FRAME_SCHEMA {
        return Err(Error::session(format!(
            "unsupported segment-frame schema at entry_seq={}: {}",
            frame.entry_seq, frame.schema
        )));
    }
    if frame.entry_seq != row.entry_seq
        || frame.entry_id != row.entry_id
        || frame.segment_seq != row.segment_seq
        || frame.frame_seq != row.frame_seq
    {
        return Err(Error::session(format!(
            "index/frame mismatch at entry_seq={}",
            row.entry_seq
        )));
    }
    if let Some(parent_entry_id) = frame.parent_entry_id.as_deref() {
        validate_wire_entry_id(parent_entry_id, "segment-frame parentEntryId")?;
    }
    if !matches!(
        frame.entry_type.as_str(),
        "message"
            | "model_change"
            | "thinking_level_change"
            | "compaction"
            | "branch_summary"
            | "label"
            | "session_info"
            | "custom"
    ) {
        return Err(Error::session(format!(
            "unsupported segment-frame entryType at entry_seq={}: {}",
            row.entry_seq, frame.entry_type
        )));
    }
    validate_rfc3339_timestamp(&frame.timestamp, "segment-frame timestamp")?;

    let (payload_hash, payload_bytes) = payload_hash_and_size(&frame.payload)?;
    if frame.payload_sha256 != payload_hash || frame.payload_bytes != payload_bytes {
        return Err(Error::session(format!(
            "payload integrity mismatch at entry_seq={}",
            row.entry_seq
        )));
    }
    Ok(frame)
}

fn validate_indexed_frame(
    file: &mut File,
    segment_seq: u64,
    segment_len: u64,
    row: &OffsetIndexEntry,
    expected_frame_seq: &mut u64,
    expected_byte_offset: &mut u64,
    parent_by_entry: &mut std::collections::HashMap<String, Option<String>>,
) -> Result<()> {
    if row.frame_seq != *expected_frame_seq {
        return Err(Error::session(format!(
            "frame sequence is not contiguous in segment {segment_seq} at entry_seq={}: expected={} actual={}",
            row.entry_seq, *expected_frame_seq, row.frame_seq,
        )));
    }
    if row.byte_offset != *expected_byte_offset {
        return Err(Error::session(format!(
            "index byte ranges are not contiguous in segment {segment_seq} at entry_seq={}: expected_offset={} actual_offset={}",
            row.entry_seq, *expected_byte_offset, row.byte_offset,
        )));
    }

    let byte_len = checked_frame_read_len(row.byte_length)?;
    let end = row
        .byte_offset
        .checked_add(row.byte_length)
        .ok_or_else(|| Error::session("index byte range overflow"))?;
    if end > segment_len {
        return Err(Error::session(format!(
            "index out of bounds for segment {segment_seq}: end={end} len={segment_len}"
        )));
    }

    file.seek(SeekFrom::Start(row.byte_offset))?;
    let mut record_bytes = vec![0u8; byte_len];
    file.read_exact(&mut record_bytes)?;
    let frame = decode_indexed_frame_record(record_bytes, row)?;
    if parent_by_entry
        .insert(frame.entry_id.clone(), frame.parent_entry_id.clone())
        .is_some()
    {
        return Err(Error::session(format!(
            "duplicate entry_id detected in session store: {}",
            frame.entry_id
        )));
    }
    *expected_frame_seq = expected_frame_seq
        .checked_add(1)
        .ok_or_else(|| Error::session("frame sequence overflow during validation"))?;
    *expected_byte_offset = end;
    Ok(())
}

fn validate_segment_index_rows(
    segment_path: &Path,
    segment_seq: u64,
    rows: &[&OffsetIndexEntry],
    parent_by_entry: &mut std::collections::HashMap<String, Option<String>>,
) -> Result<()> {
    let mut file = open_regular_file_for_read(segment_path)?
        .ok_or_else(|| Error::session(format!("missing segment: {}", segment_path.display())))?;
    let segment_len = file.metadata()?.len();
    let mut expected_frame_seq = 1;
    let mut expected_byte_offset = 0;
    for row in rows {
        validate_indexed_frame(
            &mut file,
            segment_seq,
            segment_len,
            row,
            &mut expected_frame_seq,
            &mut expected_byte_offset,
            parent_by_entry,
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct MigrationVerification {
    pub entry_count_match: bool,
    pub hash_chain_match: bool,
    pub index_consistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[serde(deny_unknown_fields)]
pub struct MigrationEvent {
    pub schema: String,
    pub migration_id: String,
    pub phase: String,
    pub at: String,
    pub source_path: String,
    pub target_path: String,
    pub source_format: String,
    pub target_format: String,
    pub verification: MigrationVerification,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    pub correlation_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IndexSummary {
    pub entry_count: u64,
    pub first_entry_seq: u64,
    pub last_entry_seq: u64,
    pub last_entry_id: String,
}

struct RollbackPlan {
    checkpoint: Checkpoint,
    retained_index_rows: Vec<OffsetIndexEntry>,
    keep_len_by_segment: std::collections::HashMap<u64, u64>,
    segment_files: Vec<(u64, PathBuf)>,
}

struct StoreMutationLockGuard {
    file: File,
}

impl Drop for StoreMutationLockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[derive(Debug, Clone)]
pub struct SessionStoreV2 {
    root: PathBuf,
    max_segment_bytes: u64,
    next_segment_seq: u64,
    next_frame_seq: u64,
    next_entry_seq: u64,
    current_segment_bytes: u64,
    /// Running SHA-256 hash chain: `H(prev_chain || payload_sha256)`.
    chain_hash: String,
    /// Total bytes written across all segments.
    total_bytes: u64,
    /// Last entry ID written (for head tracking).
    last_entry_id: Option<String>,
    /// Last CRC32-C written (for integrity checkpoints).
    last_crc32c: String,
    /// Fast stale-writer detector, sampled while the store mutation lock is held.
    observed_index_bytes: u64,
    /// File identity pairs with byte length to catch same-size index replacement.
    observed_index_identity: Option<ArtifactFileIdentity>,
}

impl SessionStoreV2 {
    /// Open a store handle for read-only inspection without bootstrap recovery.
    pub fn open_for_inspection(root: impl AsRef<Path>, max_segment_bytes: u64) -> Result<Self> {
        if max_segment_bytes == 0 {
            return Err(Error::validation("max_segment_bytes must be > 0"));
        }

        let root = root.as_ref().to_path_buf();
        drop(open_private_directory(&root, false)?);
        for child in ["segments", "index"] {
            let path = root.join(child);
            if path_entry_exists(&path)? {
                drop(open_private_directory(&path, false)?);
            }
        }
        // Healthy resume deliberately does not require listing access to these
        // unrelated trees. Still reject links, special files, and non-private
        // directories from metadata; consumers perform a descriptor-backed
        // open when they actually use one of these surfaces.
        for child in ["checkpoints", "migrations", "tmp"] {
            let path = root.join(child);
            if path_entry_exists(&path)? {
                validate_private_directory_entry(&path)?;
            }
        }

        Ok(Self {
            root,
            max_segment_bytes,
            next_segment_seq: 1,
            next_frame_seq: 1,
            next_entry_seq: 1,
            current_segment_bytes: 0,
            chain_hash: GENESIS_CHAIN_HASH.to_string(),
            total_bytes: 0,
            last_entry_id: None,
            last_crc32c: "00000000".to_string(),
            observed_index_bytes: 0,
            observed_index_identity: None,
        })
    }

    pub fn create(root: impl AsRef<Path>, max_segment_bytes: u64) -> Result<Self> {
        if max_segment_bytes == 0 {
            return Err(Error::validation("max_segment_bytes must be > 0"));
        }

        let root = root.as_ref().to_path_buf();
        drop(open_private_directory(&root, true)?);
        for child in ["segments", "index", "checkpoints", "migrations", "tmp"] {
            drop(open_private_directory(&root.join(child), true)?);
        }

        let mut store = Self {
            root,
            max_segment_bytes,
            next_segment_seq: 1,
            next_frame_seq: 1,
            next_entry_seq: 1,
            current_segment_bytes: 0,
            chain_hash: GENESIS_CHAIN_HASH.to_string(),
            total_bytes: 0,
            last_entry_id: None,
            last_crc32c: "00000000".to_string(),
            observed_index_bytes: 0,
            observed_index_identity: None,
        };
        // Bootstrap is not purely observational: it may truncate an
        // unindexed active-segment tail or rebuild a stale/missing index.
        // Keep the store-scoped mutation lock for the entire recovery and
        // bootstrap sequence so a second handle cannot append concurrently
        // with those repairs.
        let _mutation_lock = store.lock_store_mutation()?;
        store.recover_pending_rollback_unlocked()?;
        if let Err(err) = store.bootstrap_from_disk() {
            if is_recoverable_index_error(&err) {
                if is_missing_active_segment_error(&err) && !store.segments_exist_with_data()? {
                    return Err(err);
                }
                tracing::warn!(
                    root = %store.root.display(),
                    error = %err,
                    "SessionStoreV2 bootstrap failed with recoverable index error; attempting index rebuild"
                );
                store.rebuild_index_unlocked()?;
                store.bootstrap_from_disk()?;
            } else {
                return Err(err);
            }
        }

        // Recovery path: segments exist but index file is missing or empty.
        // Rebuild from segment frames so resume does not appear as an empty session.
        if store.entry_count() == 0 && store.segments_exist_with_data()? {
            tracing::warn!(
                root = %store.root.display(),
                "SessionStoreV2 detected segment data with empty index; rebuilding index"
            );
            store.rebuild_index_unlocked()?;
            store.bootstrap_from_disk()?;
        }

        if let Err(err) = store.validate_integrity() {
            if is_recoverable_index_error(&err) {
                tracing::warn!(
                    root = %store.root.display(),
                    error = %err,
                    "SessionStoreV2 integrity validation failed with recoverable error; rebuilding index"
                );
                store.rebuild_index_unlocked()?;
                store.bootstrap_from_disk()?;
                store.validate_integrity()?;
            } else {
                return Err(err);
            }
        }
        Ok(store)
    }

    pub fn segment_file_path(&self, segment_seq: u64) -> PathBuf {
        self.root
            .join("segments")
            .join(format!("{segment_seq:016}.seg"))
    }

    pub fn index_file_path(&self) -> PathBuf {
        self.root.join("index").join("offsets.jsonl")
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    fn migration_ledger_path(&self) -> PathBuf {
        self.root.join("migrations").join("ledger.jsonl")
    }

    fn rollback_intent_path(&self) -> PathBuf {
        self.root.join("tmp").join("rollback.intent.json")
    }

    fn rollback_intent_tmp_path(&self) -> PathBuf {
        self.root.join("tmp").join("rollback.intent.tmp")
    }

    fn rollback_index_stage_path(&self) -> PathBuf {
        self.root.join("tmp").join("offsets.rollback.stage")
    }

    fn mutation_lock_path(&self) -> PathBuf {
        self.root.join("store.mutation.lock")
    }

    fn lock_store_mutation(&self) -> Result<StoreMutationLockGuard> {
        let file = open_regular_file_for_write(
            &self.mutation_lock_path(),
            true,
            ArtifactWriteMode::Preserve,
        )?;
        file.lock_exclusive()?;
        Ok(StoreMutationLockGuard { file })
    }

    fn index_observation(&self) -> Result<(u64, Option<ArtifactFileIdentity>)> {
        let Some(file) = open_regular_file_for_read(&self.index_file_path())? else {
            return Ok((0, None));
        };
        let metadata = file.metadata()?;
        Ok((metadata.len(), Some(artifact_file_identity(&metadata))))
    }

    fn refresh_runtime_state_if_stale_locked(&mut self) -> Result<()> {
        let (index_bytes, index_identity) = self.index_observation()?;
        let segment_files = self.list_segment_files()?;
        let segment_state_matches = match (self.head(), segment_files.last()) {
            (None, None) => true,
            (None, Some((_, path))) => regular_file_len(path)? == 0,
            (Some(head), Some((segment_seq, path))) => {
                *segment_seq == head.segment_seq
                    && regular_file_len(path)? == self.current_segment_bytes
            }
            (Some(_), None) => false,
        };
        if index_bytes != self.observed_index_bytes
            || !artifact_file_identity_matches(
                self.observed_index_identity.as_ref(),
                index_identity.as_ref(),
            )
            || !segment_state_matches
        {
            self.reset_runtime_state();
            self.bootstrap_from_disk()?;
            self.validate_integrity()?;
        }
        Ok(())
    }

    fn list_segment_files(&self) -> Result<Vec<(u64, PathBuf)>> {
        let segments_dir = self.root.join("segments");
        let mut segment_files = Vec::new();
        let entries = match read_private_directory(&segments_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::Io(Box::new(err))),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("seg") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(segment_seq) = stem.parse::<u64>() else {
                continue;
            };
            segment_files.push((segment_seq, path));
        }
        segment_files.sort_by_key(|(segment_seq, _)| *segment_seq);
        Ok(segment_files)
    }

    fn segments_exist_with_data(&self) -> Result<bool> {
        for (_, path) in self.list_segment_files()? {
            if regular_file_len(&path)? > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[allow(clippy::needless_pass_by_value)]
    pub fn append_entry(
        &mut self,
        entry_id: impl Into<String>,
        parent_entry_id: Option<String>,
        entry_type: impl Into<String>,
        payload: Value,
    ) -> Result<OffsetIndexEntry> {
        let _mutation_lock = self.lock_store_mutation()?;
        self.recover_pending_rollback_unlocked()?;
        self.refresh_runtime_state_if_stale_locked()?;
        self.append_entry_unlocked(entry_id, parent_entry_id, entry_type, payload)
    }

    #[allow(clippy::needless_pass_by_value)]
    fn append_entry_unlocked(
        &mut self,
        entry_id: impl Into<String>,
        parent_entry_id: Option<String>,
        entry_type: impl Into<String>,
        payload: Value,
    ) -> Result<OffsetIndexEntry> {
        let entry_id = entry_id.into();
        let entry_type = entry_type.into();

        // Convert the generic Value into a RawValue (string slice) to avoid
        // re-serializing the payload when writing the full frame.
        // We do this by first serializing the Value to a string, then
        // creating a Box<RawValue> from it.
        let raw_string = serde_json::to_string(&payload)?;
        let raw_payload = RawValue::from_string(raw_string)
            .map_err(|e| Error::session(format!("failed to convert payload to RawValue: {e}")))?;

        let mut frame = SegmentFrame::new(
            self.next_segment_seq,
            self.next_frame_seq,
            self.next_entry_seq,
            entry_id,
            parent_entry_id,
            entry_type,
            raw_payload,
        )?;
        let mut encoded = serde_json::to_vec(&frame)?;
        let mut line_len = line_length_u64(&encoded)?;
        validate_encoded_frame_length(line_len, self.max_segment_bytes)?;

        if self.current_segment_bytes > 0
            && self.current_segment_bytes.saturating_add(line_len) > self.max_segment_bytes
        {
            self.next_segment_seq = self
                .next_segment_seq
                .checked_add(1)
                .ok_or_else(|| Error::session("segment sequence overflow"))?;
            self.next_frame_seq = 1;
            self.current_segment_bytes = 0;

            frame = SegmentFrame::new(
                self.next_segment_seq,
                self.next_frame_seq,
                self.next_entry_seq,
                frame.entry_id.clone(),
                frame.parent_entry_id.clone(),
                frame.entry_type.clone(),
                frame.payload.clone(),
            )?;
            encoded = serde_json::to_vec(&frame)?;
            line_len = line_length_u64(&encoded)?;
            validate_encoded_frame_length(line_len, self.max_segment_bytes)?;
        }

        let segment_path = self.segment_file_path(self.next_segment_seq);

        // Prepare the write buffer by appending the newline to the encoded JSON
        let mut write_buf = encoded;
        write_buf.push(b'\n');

        let is_new_segment = self.next_frame_seq == 1;
        let write_mode = if is_new_segment {
            ArtifactWriteMode::Replace
        } else {
            ArtifactWriteMode::Preserve
        };
        let mut segment = open_regular_file_for_write(&segment_path, true, write_mode)?;

        let byte_offset = segment.seek(SeekFrom::End(0))?;
        if let Err(e) = segment.write_all(&write_buf) {
            let _ = segment.set_len(byte_offset);
            let _ = segment.sync_all();
            return Err(Error::from(e));
        }

        if let Err(error) = segment.sync_all() {
            let _ = segment.set_len(byte_offset);
            let _ = segment.sync_all();
            return Err(Error::from(error));
        }

        // Use write_buf (which includes the newline) for CRC calculation
        let crc = crc32c_upper(&write_buf);
        let index_entry = OffsetIndexEntry {
            schema: Cow::Borrowed(OFFSET_INDEX_SCHEMA),
            entry_seq: frame.entry_seq,
            entry_id: frame.entry_id.clone(),
            segment_seq: frame.segment_seq,
            frame_seq: frame.frame_seq,
            byte_offset,
            byte_length: line_len,
            crc32c: crc.clone(),
            state: Cow::Borrowed("active"),
        };

        if let Err(e) = append_jsonl_line(&self.index_file_path(), &index_entry) {
            // Rollback: truncate segment to remove the unindexed frame.
            let _ = segment.set_len(byte_offset);
            let _ = segment.sync_all();
            return Err(e);
        }
        let (observed_index_bytes, observed_index_identity) = self.index_observation()?;
        self.observed_index_bytes = observed_index_bytes;
        self.observed_index_identity = observed_index_identity;

        self.chain_hash = chain_hash_step(&self.chain_hash, &frame.payload_sha256);
        self.total_bytes = self.total_bytes.saturating_add(line_len);
        self.last_entry_id = Some(frame.entry_id);
        self.last_crc32c = crc;

        self.next_entry_seq = self
            .next_entry_seq
            .checked_add(1)
            .ok_or_else(|| Error::session("entry sequence overflow"))?;
        self.next_frame_seq = self
            .next_frame_seq
            .checked_add(1)
            .ok_or_else(|| Error::session("frame sequence overflow"))?;
        self.current_segment_bytes = self.current_segment_bytes.saturating_add(line_len);

        Ok(index_entry)
    }

    pub fn read_segment(&self, segment_seq: u64) -> Result<Vec<SegmentFrame>> {
        let path = self.segment_file_path(segment_seq);
        read_jsonl::<SegmentFrame>(&path)
    }

    pub fn read_index(&self) -> Result<Vec<OffsetIndexEntry>> {
        let path = self.index_file_path();
        read_jsonl::<OffsetIndexEntry>(&path)
    }

    /// Seek to a specific entry by `entry_seq` using the offset index.
    /// Returns `None` if the entry is not found.
    pub fn lookup_entry(&self, target_entry_seq: u64) -> Result<Option<SegmentFrame>> {
        let index_rows = self.read_index()?;
        let row = index_rows.iter().find(|r| r.entry_seq == target_entry_seq);
        let Some(row) = row else {
            return Ok(None);
        };
        let frame = SegmentFileReader::new(self).read_frame(row)?;
        if let Some(frame) = frame.as_ref() {
            validate_fetched_parent_graph(&index_rows, std::slice::from_ref(frame))?;
        }
        Ok(frame)
    }

    /// Read all entries with `entry_seq >= from_entry_seq` (tail reading).
    pub fn read_entries_from(&self, from_entry_seq: u64) -> Result<Vec<SegmentFrame>> {
        let index_rows = self.read_index()?;
        self.read_entries_from_index(&index_rows, from_entry_seq)
    }

    fn read_entries_from_index(
        &self,
        index_rows: &[OffsetIndexEntry],
        from_entry_seq: u64,
    ) -> Result<Vec<SegmentFrame>> {
        let mut frames = Vec::new();
        let mut reader = SegmentFileReader::new(self);
        for row in index_rows {
            if row.entry_seq < from_entry_seq {
                continue;
            }
            if let Some(frame) = reader.read_frame(row)? {
                frames.push(frame);
            }
        }
        validate_fetched_parent_graph(index_rows, &frames)?;
        Ok(frames)
    }

    /// Read all entries across all segments in entry_seq order.
    pub fn read_all_entries(&self) -> Result<Vec<SegmentFrame>> {
        self.read_entries_from(1)
    }

    pub(crate) fn read_all_entries_from_index(
        &self,
        index_rows: &[OffsetIndexEntry],
    ) -> Result<Vec<SegmentFrame>> {
        self.read_entries_from_index(index_rows, 1)
    }

    /// Read the last `count` entries by entry_seq using the offset index.
    pub fn read_tail_entries(&self, count: u64) -> Result<Vec<SegmentFrame>> {
        let index_rows = self.read_index()?;
        self.read_tail_entries_from_index(&index_rows, count)
    }

    pub(crate) fn read_tail_entries_from_index(
        &self,
        index_rows: &[OffsetIndexEntry],
        count: u64,
    ) -> Result<Vec<SegmentFrame>> {
        let total = index_rows.len();
        let skip = total.saturating_sub(usize::try_from(count).unwrap_or(usize::MAX));
        let mut frames = Vec::with_capacity(total.saturating_sub(skip));
        let mut reader = SegmentFileReader::new(self);
        for row in &index_rows[skip..] {
            if let Some(frame) = reader.read_frame(row)? {
                frames.push(frame);
            }
        }
        validate_fetched_parent_graph(index_rows, &frames)?;
        Ok(frames)
    }

    /// Read entries on the active branch from `leaf_entry_id` back to root.
    /// Returns frames in root→leaf order.
    pub fn read_active_path(&self, leaf_entry_id: &str) -> Result<Vec<SegmentFrame>> {
        let index_rows = self.read_index()?;
        self.read_active_path_from_index(&index_rows, leaf_entry_id)
    }

    pub(crate) fn read_active_path_from_index(
        &self,
        index_rows: &[OffsetIndexEntry],
        leaf_entry_id: &str,
    ) -> Result<Vec<SegmentFrame>> {
        let mut id_to_row: std::collections::HashMap<&str, &OffsetIndexEntry> =
            std::collections::HashMap::with_capacity(index_rows.len());
        for row in index_rows {
            if id_to_row.insert(row.entry_id.as_str(), row).is_some() {
                return Err(Error::session(format!(
                    "duplicate entry_id detected while reading active path: {}",
                    row.entry_id
                )));
            }
        }

        let mut frames = Vec::new();
        let mut current_id: Option<String> = Some(leaf_entry_id.to_string());
        let mut reader = SegmentFileReader::new(self);
        let mut visited = std::collections::HashSet::new();
        while let Some(ref entry_id) = current_id {
            if !visited.insert(entry_id.clone()) {
                return Err(Error::session(format!(
                    "cyclic parent chain detected while reading active path at entry_id={entry_id}"
                )));
            }
            let Some(&row) = id_to_row.get(entry_id.as_str()) else {
                if frames.is_empty() {
                    break;
                }
                return Err(Error::session(format!(
                    "missing parent entry detected while reading active path at entry_id={entry_id}"
                )));
            };
            match reader.read_frame(row)? {
                Some(frame) => {
                    if frame.entry_id != row.entry_id {
                        return Err(Error::session(format!(
                            "active path index/frame mismatch for entry_id={} frame={}",
                            row.entry_id, frame.entry_id
                        )));
                    }
                    current_id.clone_from(&frame.parent_entry_id);
                    frames.push(frame);
                }
                None => {
                    return Err(Error::session(format!(
                        "index references missing frame while reading active path at entry_id={entry_id}"
                    )));
                }
            }
        }
        frames.reverse();
        validate_fetched_parent_graph(index_rows, &frames)?;
        Ok(frames)
    }

    /// Total number of entries appended so far.
    pub const fn entry_count(&self) -> u64 {
        self.next_entry_seq.saturating_sub(1)
    }

    /// Current head position, or `None` if the store is empty.
    pub fn head(&self) -> Option<StoreHead> {
        self.last_entry_id.as_ref().map(|entry_id| StoreHead {
            segment_seq: self.next_segment_seq,
            entry_seq: self.next_entry_seq.saturating_sub(1),
            entry_id: entry_id.clone(),
        })
    }

    fn checkpoint_path(&self, checkpoint_seq: u64) -> PathBuf {
        self.root
            .join("checkpoints")
            .join(format!("{checkpoint_seq:016}.json"))
    }

    fn checkpoint_documents(&self) -> Result<Vec<Checkpoint>> {
        let checkpoint_dir = self.root.join("checkpoints");
        drop(open_private_directory(&checkpoint_dir, false)?);
        let mut checkpoints = Vec::new();
        for entry in read_private_directory(&checkpoint_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let checkpoint_seq = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u64>().ok())
                .ok_or_else(|| {
                    Error::session(format!(
                        "invalid checkpoint filename in {}",
                        checkpoint_dir.display()
                    ))
                })?;
            let checkpoint = read_checkpoint_document(&path, checkpoint_seq)?
                .ok_or_else(|| Error::session("checkpoint disappeared during validation"))?;
            checkpoints.push(checkpoint);
        }
        checkpoints.sort_by_key(|checkpoint| checkpoint.checkpoint_seq);
        Ok(checkpoints)
    }

    fn sync_store_data_for_checkpoint(&self) -> Result<()> {
        self.validate_integrity()?;
        for (_, path) in self.list_segment_files()? {
            open_regular_file_for_write(&path, false, ArtifactWriteMode::Preserve)?.sync_all()?;
        }
        open_regular_file_for_write(&self.index_file_path(), false, ArtifactWriteMode::Preserve)?
            .sync_all()?;
        Ok(())
    }

    /// Create a checkpoint snapshot at the current head.
    pub fn create_checkpoint(&mut self, checkpoint_seq: u64, reason: &str) -> Result<Checkpoint> {
        let _mutation_lock = self.lock_store_mutation()?;
        self.recover_pending_rollback_unlocked()?;
        self.refresh_runtime_state_if_stale_locked()?;
        self.create_checkpoint_unlocked(checkpoint_seq, reason)
    }

    fn create_checkpoint_unlocked(&self, checkpoint_seq: u64, reason: &str) -> Result<Checkpoint> {
        if checkpoint_seq == 0 {
            return Err(Error::session("checkpoint sequence must be positive"));
        }
        validate_checkpoint_reason(reason)?;
        let head = self
            .head()
            .ok_or_else(|| Error::session("cannot checkpoint an empty V2 store"))?;
        validate_wire_entry_id(&head.entry_id, "checkpoint headEntryId")?;
        validate_lower_hex_sha256(&self.chain_hash, "checkpoint chainHash")?;
        self.validate_checkpoints_monotonic()?;
        let compacted_before_entry_seq = 0;
        for existing in self.checkpoint_documents()? {
            if existing.checkpoint_seq == checkpoint_seq {
                return Err(Error::session(format!(
                    "checkpoint {checkpoint_seq} already exists"
                )));
            }
            if existing.checkpoint_seq < checkpoint_seq
                && (existing.head_entry_seq > head.entry_seq
                    || existing.compacted_before_entry_seq > compacted_before_entry_seq)
            {
                return Err(Error::session(format!(
                    "checkpoint {checkpoint_seq} would regress its preceding checkpoint"
                )));
            }
            if existing.checkpoint_seq > checkpoint_seq
                && (head.entry_seq > existing.head_entry_seq
                    || compacted_before_entry_seq > existing.compacted_before_entry_seq)
            {
                return Err(Error::session(format!(
                    "checkpoint {checkpoint_seq} would exceed its following checkpoint"
                )));
            }
        }
        self.sync_store_data_for_checkpoint()?;
        let snapshot_ref = format!("checkpoints/{checkpoint_seq:016}.json");
        let checkpoint = Checkpoint {
            schema: CHECKPOINT_SCHEMA.to_string(),
            checkpoint_seq,
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            head_entry_seq: head.entry_seq,
            head_entry_id: head.entry_id,
            snapshot_ref,
            compacted_before_entry_seq,
            chain_hash: self.chain_hash.clone(),
            reason: reason.to_string(),
        };
        let target_path = self.checkpoint_path(checkpoint_seq);
        validate_checkpoint_document(&checkpoint, checkpoint_seq, &target_path)?;
        let encoded = serde_json::to_vec_pretty(&checkpoint)?;
        validate_checkpoint_encoded_length(u64::try_from(encoded.len()).unwrap_or(u64::MAX))?;
        drop(open_private_directory(&self.root.join("tmp"), false)?);
        let tmp_path = self
            .root
            .join("tmp")
            .join(format!("{checkpoint_seq:016}.json.tmp"));

        // A crash may leave this deterministic staging path behind. The store mutation lock
        // excludes concurrent checkpoint writers, so replacing a verified regular temp file is
        // safe; `open_regular_file_for_write` still rejects links and special files. The final
        // checkpoint remains immutable because publication below uses a no-replace rename.
        let mut file = open_regular_file_for_write(&tmp_path, true, ArtifactWriteMode::Replace)?;
        let write_result: Result<()> = (|| {
            file.write_all(&encoded)?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = write_result {
            drop(file);
            let _ = remove_regular_file(&tmp_path);
            return Err(err);
        }
        drop(file);

        rename_regular_file_no_replace(&tmp_path, &target_path)?;
        sync_parent_dir(&target_path)?;
        Ok(checkpoint)
    }

    /// Read a checkpoint by sequence number.
    pub fn read_checkpoint(&self, checkpoint_seq: u64) -> Result<Option<Checkpoint>> {
        let checkpoint_dir = self.root.join("checkpoints");
        if !path_entry_exists(&checkpoint_dir)? {
            return Ok(None);
        }
        drop(open_private_directory(&checkpoint_dir, false)?);
        read_checkpoint_document(&self.checkpoint_path(checkpoint_seq), checkpoint_seq)
    }

    fn read_rollback_intent(&self) -> Result<Option<RollbackIntent>> {
        let tmp_dir = self.root.join("tmp");
        if !path_entry_exists(&tmp_dir)? {
            return Ok(None);
        }
        drop(open_private_directory(&tmp_dir, false)?);
        let path = self.rollback_intent_path();
        let Some(file) = open_regular_file_for_read(&path)? else {
            return Ok(None);
        };
        let mut content = Vec::new();
        file.take(MAX_ROLLBACK_INTENT_BYTES.saturating_add(1))
            .read_to_end(&mut content)?;
        validate_rollback_intent_encoded_length(u64::try_from(content.len()).unwrap_or(u64::MAX))?;
        let intent: RollbackIntent = serde_json::from_slice(&content).map_err(|error| {
            Error::session(format!(
                "failed to parse rollback intent {}: {error}",
                path.display()
            ))
        })?;
        validate_rollback_intent_document(&intent, &path)?;
        Ok(Some(intent))
    }

    fn write_rollback_intent(&self, intent: &RollbackIntent) -> Result<()> {
        drop(open_private_directory(&self.root.join("tmp"), false)?);
        let target_path = self.rollback_intent_path();
        if path_entry_exists(&target_path)? {
            return Err(Error::session(format!(
                "pending rollback intent already exists: {}",
                target_path.display()
            )));
        }
        validate_rollback_intent_document(intent, &target_path)?;
        let encoded = serde_json::to_vec_pretty(intent)?;
        validate_rollback_intent_encoded_length(u64::try_from(encoded.len()).unwrap_or(u64::MAX))?;
        let tmp_path = self.rollback_intent_tmp_path();
        let mut file = open_regular_file_for_write(&tmp_path, true, ArtifactWriteMode::Replace)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        rename_regular_file_no_replace(&tmp_path, &target_path)?;
        sync_parent_dir(&target_path)?;
        Ok(())
    }

    fn rollback_manifest_context(&self) -> Result<Option<RollbackManifestContext>> {
        Ok(self
            .read_manifest()?
            .map(|manifest| RollbackManifestContext {
                session_id: manifest.session_id,
                source_format: manifest.source_format,
                created_at: manifest.created_at,
            }))
    }

    pub fn append_migration_event(&self, event: MigrationEvent) -> Result<()> {
        let _mutation_lock = self.lock_store_mutation()?;
        self.append_migration_event_unlocked(event)
    }

    fn append_migration_event_unlocked(&self, mut event: MigrationEvent) -> Result<()> {
        if event.schema.is_empty() {
            event.schema = MIGRATION_EVENT_SCHEMA.to_string();
        }
        if event.at.is_empty() {
            event.at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        }
        validate_migration_event_document(&event)?;
        let ledger_path = self.migration_ledger_path();
        // The migration ledger is append-only forensic evidence. Unlike the
        // rebuildable offset index or recoverable primary JSONL tail, an
        // unterminated/invalid ledger must fail closed before a later event can
        // be concatenated onto corrupt bytes.
        drop(self.read_migration_events_unlocked()?);
        append_jsonl_line_durable(&ledger_path, &event)
    }

    pub fn read_migration_events(&self) -> Result<Vec<MigrationEvent>> {
        let _mutation_lock = self.lock_store_mutation()?;
        self.read_migration_events_unlocked()
    }

    fn read_migration_events_unlocked(&self) -> Result<Vec<MigrationEvent>> {
        let migrations_dir = self.root.join("migrations");
        if !path_entry_exists(&migrations_dir)? {
            return Ok(Vec::new());
        }
        drop(open_private_directory(&migrations_dir, false)?);
        let path = self.migration_ledger_path();
        let events = read_jsonl::<MigrationEvent>(&path)?;
        for event in &events {
            validate_migration_event_document(event)?;
        }
        Ok(events)
    }

    pub fn rollback_to_checkpoint(
        &mut self,
        checkpoint_seq: u64,
        migration_id: impl Into<String>,
        correlation_id: impl Into<String>,
    ) -> Result<MigrationEvent> {
        let migration_id = migration_id.into();
        let correlation_id = correlation_id.into();
        validate_migration_id(&migration_id)?;
        validate_correlation_id(&correlation_id)?;
        let _mutation_lock = self.lock_store_mutation()?;
        self.recover_pending_rollback_unlocked()?;
        self.refresh_runtime_state_if_stale_locked()?;

        let plan = self.prepare_rollback_plan(checkpoint_seq)?;
        let manifest = self
            .rollback_manifest_context()
            .map_err(|error| rollback_preflight_error(checkpoint_seq, &error))?;
        let stage_path = self.rollback_index_stage_path();
        let mut mutation_started = false;
        let rollback_result: Result<MigrationEvent> = (|| {
            mutation_started = true;
            let (retained_index_bytes, retained_index_sha256) =
                write_jsonl_lines_with_digest(&stage_path, &plan.retained_index_rows)?;
            let intent = RollbackIntent {
                schema: ROLLBACK_INTENT_SCHEMA.to_string(),
                checkpoint: plan.checkpoint,
                migration_id: migration_id.clone(),
                correlation_id: correlation_id.clone(),
                retained_index_sha256,
                retained_index_bytes,
                manifest,
            };
            self.write_rollback_intent(&intent)?;
            self.finish_rollback_intent(&intent)
        })();

        if let Err(error) = &rollback_result
            && mutation_started
        {
            return Err(self.record_rollback_failure(
                checkpoint_seq,
                &migration_id,
                &correlation_id,
                error,
            ));
        }
        rollback_result
    }

    fn prepare_rollback_plan(&self, checkpoint_seq: u64) -> Result<RollbackPlan> {
        (|| {
            self.validate_checkpoints_monotonic()?;
            let checkpoint = self
                .read_checkpoint(checkpoint_seq)?
                .ok_or_else(|| Error::session(format!("checkpoint {checkpoint_seq} not found")))?;
            self.build_rollback_plan(checkpoint)
        })()
        .map_err(|error| rollback_preflight_error(checkpoint_seq, &error))
    }

    fn build_rollback_plan(&self, checkpoint: Checkpoint) -> Result<RollbackPlan> {
        let index_rows = self.read_index()?;
        let retained_index_rows: Vec<_> = index_rows
            .into_iter()
            .filter(|row| row.entry_seq <= checkpoint.head_entry_seq)
            .collect();
        self.validate_rollback_prefix(&checkpoint, &retained_index_rows)?;

        let mut keep_len_by_segment = std::collections::HashMap::new();
        for row in &retained_index_rows {
            let end = row
                .byte_offset
                .checked_add(row.byte_length)
                .ok_or_else(|| Error::session("index byte range overflow during rollback"))?;
            keep_len_by_segment
                .entry(row.segment_seq)
                .and_modify(|current: &mut u64| *current = (*current).max(end))
                .or_insert(end);
        }

        let segment_files = self.list_segment_files()?;
        for (segment_seq, path) in &segment_files {
            let file = open_regular_file_for_read(path)?.ok_or_else(|| {
                Error::session(format!(
                    "segment disappeared during rollback planning: {}",
                    path.display()
                ))
            })?;
            if let Some(keep_len) = keep_len_by_segment.get(segment_seq)
                && *keep_len > file.metadata()?.len()
            {
                return Err(Error::session(format!(
                    "rollback retained length exceeds segment size: {}",
                    path.display()
                )));
            }
        }

        Ok(RollbackPlan {
            checkpoint,
            retained_index_rows,
            keep_len_by_segment,
            segment_files,
        })
    }

    fn validate_rollback_prefix(
        &self,
        checkpoint: &Checkpoint,
        retained_index_rows: &[OffsetIndexEntry],
    ) -> Result<()> {
        let expected_count = usize::try_from(checkpoint.head_entry_seq)
            .map_err(|_| Error::session("checkpoint head sequence exceeds usize"))?;
        if retained_index_rows.len() != expected_count {
            return Err(Error::session(format!(
                "checkpoint head sequence {} is outside the contiguous index prefix",
                checkpoint.head_entry_seq
            )));
        }
        for (offset, row) in retained_index_rows.iter().enumerate() {
            let expected_entry_seq = u64::try_from(offset)
                .map_err(|_| Error::session("rollback prefix length exceeds u64"))?
                .checked_add(1)
                .ok_or_else(|| Error::session("rollback prefix sequence overflow"))?;
            if row.entry_seq != expected_entry_seq {
                return Err(Error::session(format!(
                    "rollback prefix is not contiguous at entry_seq={}",
                    row.entry_seq
                )));
            }
        }

        let rows_by_segment = group_validated_index_rows(retained_index_rows)?;
        let mut parent_by_entry = std::collections::HashMap::new();
        for (segment_seq, rows) in rows_by_segment {
            validate_segment_index_rows(
                &self.segment_file_path(segment_seq),
                segment_seq,
                &rows,
                &mut parent_by_entry,
            )?;
        }
        validate_parent_graph_links(&parent_by_entry)?;
        validate_parent_graph_acyclic(&parent_by_entry)?;

        let mut chain_hash = GENESIS_CHAIN_HASH.to_string();
        let mut actual_head_id = String::new();
        let mut reader = SegmentFileReader::new(self);
        for row in retained_index_rows {
            let frame = reader
                .read_frame(row)?
                .ok_or_else(|| Error::session("rollback prefix references a missing frame"))?;
            chain_hash = chain_hash_step(&chain_hash, &frame.payload_sha256);
            actual_head_id = frame.entry_id;
        }
        if checkpoint.head_entry_id != actual_head_id {
            return Err(Error::session(format!(
                "checkpoint head id does not match retained prefix at entry_seq={}",
                checkpoint.head_entry_seq
            )));
        }
        if checkpoint.chain_hash != chain_hash {
            return Err(Error::session(format!(
                "checkpoint chain hash does not match retained prefix at entry_seq={}",
                checkpoint.head_entry_seq
            )));
        }
        Ok(())
    }

    fn reset_runtime_state(&mut self) {
        self.next_segment_seq = 1;
        self.next_frame_seq = 1;
        self.next_entry_seq = 1;
        self.current_segment_bytes = 0;
        self.chain_hash = GENESIS_CHAIN_HASH.to_string();
        self.total_bytes = 0;
        self.last_entry_id = None;
        self.last_crc32c = "00000000".to_string();
        self.observed_index_bytes = 0;
        self.observed_index_identity = None;
    }

    fn rollback_artifact_matches_intent(path: &Path, intent: &RollbackIntent) -> Result<bool> {
        let (byte_count, digest) = regular_file_sha256(path)?;
        Ok(byte_count == intent.retained_index_bytes && digest == intent.retained_index_sha256)
    }

    fn install_rollback_index(&self, intent: &RollbackIntent) -> Result<()> {
        let stage_path = self.rollback_index_stage_path();
        let index_path = self.index_file_path();
        if path_entry_exists(&stage_path)? {
            if !Self::rollback_artifact_matches_intent(&stage_path, intent)? {
                return Err(Error::session(
                    "staged rollback index does not match durable intent",
                ));
            }
            rename_regular_file(&stage_path, &index_path)?;
            sync_parent_dir(&index_path)?;
        } else if !Self::rollback_artifact_matches_intent(&index_path, intent)? {
            return Err(Error::session(
                "active rollback index does not match durable intent",
            ));
        }
        Ok(())
    }

    fn quarantine_rollback_segment_tail(plan: &RollbackPlan) -> Result<()> {
        for (segment_seq, path) in &plan.segment_files {
            match plan.keep_len_by_segment.get(segment_seq).copied() {
                Some(keep_len) if keep_len > 0 => {
                    let current_len = regular_file_len(path)?;
                    if current_len < keep_len {
                        return Err(Error::session(format!(
                            "rollback retained length exceeds segment size: {}",
                            path.display()
                        )));
                    }
                    if keep_len < current_len {
                        truncate_file_to(path, keep_len)?;
                    }
                }
                _ => {
                    let quarantined = quarantine_segment_file(path)?;
                    tracing::warn!(
                        segment = %path.display(),
                        quarantine = %quarantined.display(),
                        "SessionStoreV2 quarantined a post-checkpoint segment during rollback"
                    );
                }
            }
        }
        Ok(())
    }

    fn quarantine_future_checkpoints(&self, checkpoint_seq: u64) -> Result<()> {
        for checkpoint in self.checkpoint_documents()? {
            if checkpoint.checkpoint_seq <= checkpoint_seq {
                continue;
            }
            let path = self.checkpoint_path(checkpoint.checkpoint_seq);
            let quarantined = quarantine_segment_file(&path)?;
            tracing::warn!(
                checkpoint = checkpoint.checkpoint_seq,
                quarantine = %quarantined.display(),
                "SessionStoreV2 quarantined a checkpoint newer than the rollback target"
            );
        }
        Ok(())
    }

    fn recorded_rollback_event_unlocked(
        &self,
        migration_id: &str,
        correlation_id: &str,
        outcome: &str,
    ) -> Result<Option<MigrationEvent>> {
        Ok(self
            .read_migration_events_unlocked()?
            .into_iter()
            .find(|event| {
                event.phase == "rollback"
                    && event.migration_id == migration_id
                    && event.correlation_id == correlation_id
                    && event.outcome == outcome
            }))
    }

    fn finish_rollback_intent(&mut self, intent: &RollbackIntent) -> Result<MigrationEvent> {
        validate_rollback_intent_document(intent, &self.rollback_intent_path())?;
        self.install_rollback_index(intent)?;
        let plan = self.build_rollback_plan(intent.checkpoint.clone())?;
        Self::quarantine_rollback_segment_tail(&plan)?;
        self.quarantine_future_checkpoints(intent.checkpoint.checkpoint_seq)?;

        self.reset_runtime_state();
        self.bootstrap_from_disk()?;

        let verification = MigrationVerification {
            entry_count_match: self.entry_count() == intent.checkpoint.head_entry_seq,
            hash_chain_match: self.chain_hash == intent.checkpoint.chain_hash,
            index_consistent: self.validate_integrity().is_ok(),
        };
        if !verification.entry_count_match
            || !verification.hash_chain_match
            || !verification.index_consistent
        {
            return Err(Error::session(format!(
                "rollback verification failed for checkpoint {}",
                intent.checkpoint.checkpoint_seq
            )));
        }

        if let Some(context) = &intent.manifest {
            let manifest =
                self.write_manifest_unlocked(&context.session_id, &context.source_format)?;
            if manifest.created_at != context.created_at {
                return Err(Error::session(
                    "rollback manifest reconciliation changed createdAt",
                ));
            }
            let validated = self
                .validate_manifest_against_store()?
                .ok_or_else(|| Error::session("rollback manifest disappeared after rewrite"))?;
            if validated.session_id != context.session_id
                || validated.source_format != context.source_format
            {
                return Err(Error::session(
                    "rollback manifest identity changed during reconciliation",
                ));
            }
        }

        let event = MigrationEvent {
            schema: MIGRATION_EVENT_SCHEMA.to_string(),
            migration_id: intent.migration_id.clone(),
            phase: "rollback".to_string(),
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            source_path: intent.checkpoint.snapshot_ref.clone(),
            target_path: self.root.display().to_string(),
            source_format: "native_v2".to_string(),
            target_format: "native_v2".to_string(),
            verification,
            outcome: "ok".to_string(),
            error_class: None,
            correlation_id: intent.correlation_id.clone(),
        };
        let recorded = if let Some(recorded) = self.recorded_rollback_event_unlocked(
            &intent.migration_id,
            &intent.correlation_id,
            "ok",
        )? {
            recorded
        } else {
            self.append_migration_event_unlocked(event.clone())?;
            event
        };
        remove_regular_file(&self.rollback_intent_path())?;
        Ok(recorded)
    }

    fn record_rollback_failure(
        &self,
        checkpoint_seq: u64,
        migration_id: &str,
        correlation_id: &str,
        error: &Error,
    ) -> Error {
        let failure_result = (|| {
            if self
                .recorded_rollback_event_unlocked(migration_id, correlation_id, "fatal_error")?
                .is_some()
            {
                return Ok(());
            }
            self.append_migration_event_unlocked(MigrationEvent {
                schema: MIGRATION_EVENT_SCHEMA.to_string(),
                migration_id: migration_id.to_string(),
                phase: "rollback".to_string(),
                at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                source_path: self.checkpoint_path(checkpoint_seq).display().to_string(),
                target_path: self.root.display().to_string(),
                source_format: "native_v2".to_string(),
                target_format: "native_v2".to_string(),
                verification: MigrationVerification {
                    entry_count_match: false,
                    hash_chain_match: false,
                    index_consistent: false,
                },
                outcome: "fatal_error".to_string(),
                error_class: Some(rollback_failure_class(error).to_string()),
                correlation_id: correlation_id.to_string(),
            })
        })();

        match failure_result {
            Ok(()) => Error::session(error.to_string()),
            Err(ledger_error) => Error::session(format!(
                "{error}; additionally failed to persist fatal rollback evidence: {ledger_error}"
            )),
        }
    }

    fn recover_pending_rollback_unlocked(&mut self) -> Result<Option<MigrationEvent>> {
        let Some(intent) = self.read_rollback_intent()? else {
            return Ok(None);
        };
        match self.finish_rollback_intent(&intent) {
            Ok(event) => Ok(Some(event)),
            Err(error) => Err(self.record_rollback_failure(
                intent.checkpoint.checkpoint_seq,
                &intent.migration_id,
                &intent.correlation_id,
                &error,
            )),
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn write_manifest(
        &mut self,
        session_id: impl Into<String>,
        source_format: impl Into<String>,
    ) -> Result<Manifest> {
        let _mutation_lock = self.lock_store_mutation()?;
        self.recover_pending_rollback_unlocked()?;
        self.refresh_runtime_state_if_stale_locked()?;
        self.write_manifest_unlocked(session_id, source_format)
    }

    #[allow(clippy::too_many_lines)]
    fn write_manifest_unlocked(
        &self,
        session_id: impl Into<String>,
        source_format: impl Into<String>,
    ) -> Result<Manifest> {
        let session_id = session_id.into();
        let source_format = source_format.into();
        validate_manifest_session_id(&session_id)?;
        validate_manifest_source_format(&source_format)?;
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let created_at = self
            .read_manifest()?
            .map_or_else(|| now.clone(), |m| m.created_at);
        let index_rows = self.read_index()?;

        let mut parent_counts: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        let mut message_count = 0u64;
        let mut compaction_count = 0u64;
        let mut entry_ids = std::collections::HashSet::with_capacity(index_rows.len());
        let mut referenced_parent_ids = Vec::new();

        let mut recomputed_chain = GENESIS_CHAIN_HASH.to_string();
        let mut reader = SegmentFileReader::new(self);

        for row in &index_rows {
            if let Some(frame) = reader.read_frame(row)? {
                entry_ids.insert(frame.entry_id.clone());

                if frame.entry_type == "message" {
                    message_count = message_count.saturating_add(1);
                }
                if frame.entry_type == "compaction" {
                    compaction_count = compaction_count.saturating_add(1);
                }

                if let Some(parent_id) = frame.parent_entry_id.as_deref() {
                    *parent_counts.entry(parent_id.to_string()).or_insert(0) += 1;
                    referenced_parent_ids.push(parent_id.to_string());
                }

                recomputed_chain = chain_hash_step(&recomputed_chain, &frame.payload_sha256);
            }
        }

        let branches_total = u64::try_from(parent_counts.values().filter(|&&n| n > 1).count())
            .map_err(|_| Error::session("branch count exceeds u64"))?;
        let parent_links_closed = referenced_parent_ids
            .iter()
            .all(|parent_id| entry_ids.contains(parent_id));

        let mut monotonic_entry_seq = true;
        let mut monotonic_segment_seq = true;
        let mut last_entry_seq = 0u64;
        let mut last_segment_seq = 0u64;
        for row in &index_rows {
            if row.entry_seq <= last_entry_seq {
                monotonic_entry_seq = false;
            }
            if row.segment_seq < last_segment_seq {
                monotonic_segment_seq = false;
            }
            last_entry_seq = row.entry_seq;
            last_segment_seq = row.segment_seq;
        }

        let hash_chain_valid = recomputed_chain == self.chain_hash;

        let head = self.head().unwrap_or(StoreHead {
            segment_seq: 0,
            entry_seq: 0,
            entry_id: String::new(),
        });
        let segment_count = u64::try_from(
            index_rows
                .iter()
                .map(|row| row.segment_seq)
                .collect::<BTreeSet<_>>()
                .len(),
        )
        .map_err(|_| Error::session("segment count exceeds u64"))?;

        let mut manifest = Manifest {
            schema: MANIFEST_SCHEMA.to_string(),
            store_version: 2,
            session_id,
            source_format,
            created_at,
            updated_at: now,
            head,
            counters: ManifestCounters {
                entries_total: u64::try_from(index_rows.len())
                    .map_err(|_| Error::session("entry count exceeds u64"))?,
                messages_total: message_count,
                branches_total,
                compactions_total: compaction_count,
                bytes_total: self.total_bytes,
            },
            files: ManifestFiles {
                segment_dir: "segments/".to_string(),
                segment_count,
                index_path: "index/offsets.jsonl".to_string(),
                checkpoint_dir: "checkpoints/".to_string(),
                migration_ledger_path: "migrations/ledger.jsonl".to_string(),
            },
            integrity: ManifestIntegrity {
                chain_hash: self.chain_hash.clone(),
                manifest_hash: String::new(),
                last_crc32c: self.last_crc32c.clone(),
            },
            invariants: ManifestInvariants {
                parent_links_closed,
                monotonic_entry_seq,
                monotonic_segment_seq,
                index_within_segment_bounds: self.validate_integrity().is_ok(),
                branch_heads_indexed: self.validate_all_segment_bytes_indexed(&index_rows).is_ok(),
                checkpoints_monotonic: self.validate_checkpoints_monotonic().is_ok(),
                hash_chain_valid,
            },
        };
        manifest.integrity.manifest_hash = manifest_hash_hex(&manifest)?;
        let encoded = serde_json::to_vec_pretty(&manifest)?;
        validate_manifest_encoded_length(u64::try_from(encoded.len()).unwrap_or(u64::MAX))?;

        drop(open_private_directory(&self.root.join("tmp"), false)?);
        let tmp = self.root.join("tmp").join("manifest.json.tmp");

        let write_result: Result<()> = (|| {
            let mut file = open_regular_file_for_write(&tmp, true, ArtifactWriteMode::Replace)?;
            file.write_all(&encoded)?;
            file.sync_all()?;
            Ok(())
        })();

        if let Err(err) = write_result {
            let _ = remove_regular_file(&tmp);
            return Err(err);
        }

        let target_path = self.manifest_path();
        rename_regular_file(&tmp, &target_path)?;
        sync_parent_dir(&target_path)?;
        Ok(manifest)
    }

    pub fn read_manifest(&self) -> Result<Option<Manifest>> {
        let path = self.manifest_path();
        let Some(file) = open_regular_file_for_read(&path)? else {
            return Ok(None);
        };
        let mut content = Vec::new();
        file.take(MAX_MANIFEST_BYTES.saturating_add(1))
            .read_to_end(&mut content)?;
        if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
            return Err(Error::session(format!(
                "manifest {} exceeds the {} byte read limit",
                path.display(),
                MAX_MANIFEST_BYTES
            )));
        }
        let manifest: Manifest = serde_json::from_slice(&content).map_err(|err| {
            Error::session(format!(
                "Failed to parse manifest {}: {err}",
                path.display()
            ))
        })?;
        validate_manifest_document(&manifest, &path)?;
        Ok(Some(manifest))
    }

    fn validate_all_segment_bytes_indexed(&self, index: &[OffsetIndexEntry]) -> Result<()> {
        let mut indexed_ends = std::collections::BTreeMap::<u64, u64>::new();
        for row in index {
            let end = row
                .byte_offset
                .checked_add(row.byte_length)
                .ok_or_else(|| Error::session("index byte range overflow"))?;
            indexed_ends
                .entry(row.segment_seq)
                .and_modify(|current| *current = (*current).max(end))
                .or_insert(end);
        }
        let segments_dir = self.root.join("segments");
        let mut seen_segment_sequences = BTreeSet::new();
        for entry in read_private_directory(&segments_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("seg") {
                continue;
            }
            let segment_seq = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u64>().ok())
                .ok_or_else(|| {
                    Error::session(format!("invalid segment filename: {}", path.display()))
                })?;
            if !seen_segment_sequences.insert(segment_seq) {
                return Err(Error::session(format!(
                    "duplicate segment sequence {segment_seq} in {}",
                    segments_dir.display()
                )));
            }
            let segment = open_regular_file_for_read(&path)?
                .ok_or_else(|| Error::session("segment disappeared during validation"))?;
            let actual = segment.metadata()?.len();
            let indexed = indexed_ends.get(&segment_seq).copied().unwrap_or(0);
            if actual != indexed {
                return Err(Error::session(format!(
                    "segment byte coverage mismatch for {}: indexed={indexed} actual={actual}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    fn validate_checkpoints_monotonic(&self) -> Result<()> {
        let checkpoint_dir = self.root.join("checkpoints");
        if !path_entry_exists(&checkpoint_dir)? {
            return Ok(());
        }
        drop(open_private_directory(&checkpoint_dir, false)?);
        let entries = match read_private_directory(&checkpoint_dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(Error::Io(Box::new(err))),
        };
        let mut checkpoints = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let checkpoint_seq = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u64>().ok())
                .ok_or_else(|| {
                    Error::session(format!(
                        "invalid checkpoint filename in {}",
                        checkpoint_dir.display()
                    ))
                })?;
            checkpoints.push((checkpoint_seq, path));
        }
        checkpoints.sort_by_key(|(checkpoint_seq, _)| *checkpoint_seq);

        let mut previous_checkpoint_seq = None;
        let mut previous_head_entry_seq = None;
        let mut previous_compacted_before_entry_seq = None;
        for (filename_seq, path) in checkpoints {
            if previous_checkpoint_seq.is_some_and(|previous| filename_seq <= previous) {
                return Err(Error::session(
                    "checkpoint sequence is not strictly increasing",
                ));
            }
            let checkpoint = read_checkpoint_document(&path, filename_seq)?
                .ok_or_else(|| Error::session("checkpoint disappeared during validation"))?;
            if previous_head_entry_seq.is_some_and(|previous| checkpoint.head_entry_seq < previous)
            {
                return Err(Error::session(format!(
                    "checkpoint head sequence regresses in {}",
                    path.display()
                )));
            }
            if previous_compacted_before_entry_seq
                .is_some_and(|previous| checkpoint.compacted_before_entry_seq < previous)
            {
                return Err(Error::session(format!(
                    "checkpoint compacted boundary regresses in {}",
                    path.display()
                )));
            }
            previous_checkpoint_seq = Some(filename_seq);
            previous_head_entry_seq = Some(checkpoint.head_entry_seq);
            previous_compacted_before_entry_seq = Some(checkpoint.compacted_before_entry_seq);
        }
        Ok(())
    }

    /// Validate that a self-consistent manifest still describes the current
    /// segmented store. The manifest hash detects accidental byte-level edits;
    /// these derived-value checks also reject a recomputed hash over forged
    /// counters or head metadata.
    pub fn validate_manifest_against_store(&self) -> Result<Option<Manifest>> {
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.validate_integrity()?;
        let index = self.read_index()?;
        self.validate_all_segment_bytes_indexed(&index)?;
        self.validate_checkpoints_monotonic()?;
        validate_manifest_invariants(&manifest)?;
        let frames = self.read_all_entries()?;
        let facts = derive_manifest_store_facts(&index, &frames)?;
        validate_manifest_store_facts(&manifest, &facts)?;
        Ok(Some(manifest))
    }

    /// Validate the index and segment-file shape needed by bounded resume.
    ///
    /// This deliberately reads index rows and segment metadata, not frame
    /// payloads. Every frame selected by hydration is validated separately by
    /// [`SegmentFileReader::read_frame`], while full audits continue to use
    /// [`Self::validate_integrity`] and [`Self::validate_session_integrity`].
    fn validate_resume_index_structure(&self, index: &[OffsetIndexEntry]) -> Result<()> {
        let mut last_entry_seq = 0u64;
        let mut last_segment_seq = 0u64;
        let mut current_segment_seq = None;
        let mut current_segment_len = 0u64;
        let mut expected_frame_seq = 1u64;
        let mut expected_byte_offset = 0u64;
        let mut entry_ids = std::collections::HashSet::with_capacity(index.len());
        let mut indexed_segment_sequences = std::collections::HashSet::new();

        for row in index {
            validate_offset_index_row_document(row)?;
            if !entry_ids.insert(row.entry_id.as_str()) {
                return Err(Error::session(format!(
                    "duplicate entry_id detected in offset index: {}",
                    row.entry_id
                )));
            }
            let expected_entry_seq = last_entry_seq
                .checked_add(1)
                .ok_or_else(|| Error::session("entry sequence overflow in offset index"))?;
            if row.entry_seq != expected_entry_seq {
                return Err(Error::session(format!(
                    "entry sequence is not contiguous: expected={expected_entry_seq} actual={}",
                    row.entry_seq
                )));
            }
            if row.segment_seq < last_segment_seq {
                return Err(Error::session(format!(
                    "segment sequence is not monotonic at entry_seq={}: {}",
                    row.entry_seq, row.segment_seq
                )));
            }

            if current_segment_seq != Some(row.segment_seq) {
                if let Some(previous_segment_seq) = current_segment_seq
                    && expected_byte_offset != current_segment_len
                {
                    return Err(Error::session(format!(
                        "segment byte coverage mismatch for {}: indexed={expected_byte_offset} actual={current_segment_len}",
                        self.segment_file_path(previous_segment_seq).display()
                    )));
                }
                let segment_path = self.segment_file_path(row.segment_seq);
                let segment = open_regular_file_for_read(&segment_path)?.ok_or_else(|| {
                    Error::session(format!("missing segment: {}", segment_path.display()))
                })?;
                current_segment_len = segment.metadata()?.len();
                if current_segment_len > self.max_segment_bytes {
                    return Err(Error::session(format!(
                        "segment {} length {current_segment_len} exceeds configured segment limit {}",
                        segment_path.display(),
                        self.max_segment_bytes
                    )));
                }
                current_segment_seq = Some(row.segment_seq);
                indexed_segment_sequences.insert(row.segment_seq);
                expected_frame_seq = 1;
                expected_byte_offset = 0;
            }

            validate_encoded_frame_length(row.byte_length, self.max_segment_bytes)?;
            if row.frame_seq != expected_frame_seq {
                return Err(Error::session(format!(
                    "frame sequence is not contiguous in segment {} at entry_seq={}: expected={expected_frame_seq} actual={}",
                    row.segment_seq, row.entry_seq, row.frame_seq
                )));
            }
            if row.byte_offset != expected_byte_offset {
                return Err(Error::session(format!(
                    "index byte ranges are not contiguous in segment {} at entry_seq={}: expected_offset={expected_byte_offset} actual_offset={}",
                    row.segment_seq, row.entry_seq, row.byte_offset
                )));
            }
            let end = row
                .byte_offset
                .checked_add(row.byte_length)
                .ok_or_else(|| Error::session("index byte range overflow"))?;
            if end > current_segment_len {
                return Err(Error::session(format!(
                    "index out of bounds for segment {}: end={end} len={current_segment_len}",
                    row.segment_seq
                )));
            }
            expected_frame_seq = expected_frame_seq.checked_add(1).ok_or_else(|| {
                Error::session("frame sequence overflow during resume validation")
            })?;
            expected_byte_offset = end;
            last_entry_seq = row.entry_seq;
            last_segment_seq = row.segment_seq;
        }

        if let Some(segment_seq) = current_segment_seq
            && expected_byte_offset != current_segment_len
        {
            return Err(Error::session(format!(
                "segment byte coverage mismatch for {}: indexed={expected_byte_offset} actual={current_segment_len}",
                self.segment_file_path(segment_seq).display()
            )));
        }

        self.validate_resume_segment_inventory(&indexed_segment_sequences)
    }

    fn validate_resume_segment_inventory(
        &self,
        indexed_segment_sequences: &std::collections::HashSet<u64>,
    ) -> Result<()> {
        let segments_dir = self.root.join("segments");
        let mut seen_segment_sequences = std::collections::HashSet::new();
        for entry in read_private_directory(&segments_dir)? {
            let path = entry?.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("seg") {
                continue;
            }
            let segment_seq = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<u64>().ok())
                .ok_or_else(|| {
                    Error::session(format!("invalid segment filename: {}", path.display()))
                })?;
            if !seen_segment_sequences.insert(segment_seq) {
                return Err(Error::session(format!(
                    "duplicate segment sequence {segment_seq} in {}",
                    segments_dir.display()
                )));
            }
            let segment = open_regular_file_for_read(&path)?
                .ok_or_else(|| Error::session("segment disappeared during validation"))?;
            let actual = segment.metadata()?.len();
            if !indexed_segment_sequences.contains(&segment_seq) {
                return Err(Error::session(format!(
                    "segment byte coverage mismatch for {}: indexed=0 actual={actual}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn validate_resume_manifest_against_index(
        &self,
        index: &[OffsetIndexEntry],
    ) -> Result<Option<Manifest>> {
        let Some(manifest) = self.read_manifest()? else {
            return Ok(None);
        };
        self.validate_resume_index_structure(index)?;
        validate_manifest_invariants(&manifest)?;
        let facts = derive_manifest_index_facts(index)?;
        validate_resume_manifest_index_facts(&manifest, &facts)?;
        Ok(Some(manifest))
    }

    /// Validate the manifest and the artifacts required for a healthy resume.
    ///
    /// This is the bounded steady-state path: it validates manifest self-hash,
    /// declared invariants, index-derived facts, and segment metadata without
    /// scanning every frame. Checkpoints are rollback inputs, not
    /// active-session inputs. Full audits and rollback still validate all
    /// frames and checkpoints, while a read-only resume remains independent of
    /// unrelated checkpoint-directory permissions.
    pub fn validate_resume_manifest_against_store(&self) -> Result<Option<Manifest>> {
        let index = self.read_index()?;
        self.validate_resume_manifest_against_index(&index)
    }

    pub fn chain_hash(&self) -> &str {
        &self.chain_hash
    }

    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn index_summary(&self) -> Result<Option<IndexSummary>> {
        let rows = self.read_index()?;
        let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
            return Ok(None);
        };
        Ok(Some(IndexSummary {
            entry_count: u64::try_from(rows.len())
                .map_err(|_| Error::session("entry count exceeds u64"))?,
            first_entry_seq: first.entry_seq,
            last_entry_seq: last.entry_seq,
            last_entry_id: last.entry_id.clone(),
        }))
    }

    /// Rebuild the offset index by scanning all segment files.
    /// This is the recovery path when the index is missing or corrupted.
    #[allow(clippy::too_many_lines)]
    pub fn rebuild_index(&mut self) -> Result<u64> {
        let _mutation_lock = self.lock_store_mutation()?;
        self.recover_pending_rollback_unlocked()?;
        self.rebuild_index_unlocked()
    }

    #[allow(clippy::too_many_lines)]
    fn rebuild_index_unlocked(&mut self) -> Result<u64> {
        let mut rebuilt_count = 0u64;
        let index_path = self.index_file_path();
        let index_tmp_path = self.root.join("tmp").join("offsets.rebuild.tmp");

        if let Some(parent) = index_tmp_path.parent() {
            drop(open_private_directory(parent, false)?);
        }

        let mut index_writer = std::io::BufWriter::new(open_regular_file_for_write(
            &index_tmp_path,
            true,
            ArtifactWriteMode::Replace,
        )?);

        self.chain_hash = GENESIS_CHAIN_HASH.to_string();
        self.total_bytes = 0;
        self.last_entry_id = None;
        self.last_crc32c = "00000000".to_string();

        let segment_files = self.list_segment_files()?;
        let mut last_observed_seq = 0u64;

        'segments: for (i, (segment_seq, seg_path)) in segment_files.iter().enumerate() {
            let file = open_regular_file_for_read(seg_path)?.ok_or_else(|| {
                Error::session(format!(
                    "segment disappeared during index rebuild: {}",
                    seg_path.display()
                ))
            })?;
            let mut reader = BufReader::new(file);
            let mut byte_offset = 0u64;
            let mut line_number = 0u64;
            let mut expected_frame_seq = 1u64;
            let mut line = String::new();

            loop {
                line.clear();
                // Use bounded read to prevent OOM on corrupted files (e.g. missing newlines)
                let bytes_read =
                    match read_line_with_limit(&mut reader, &mut line, MAX_FRAME_READ_BYTES) {
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                            return Err(Error::session(format!(
                                "failed to read segment frame while rebuilding index: \
                             segment={} line={}: {e}",
                                seg_path.display(),
                                line_number.saturating_add(1),
                            )));
                        }
                        Err(e) => return Err(Error::Io(Box::new(e))),
                    };

                if bytes_read == 0 {
                    break;
                }
                line_number = line_number.saturating_add(1);
                let mut line_len = u64::try_from(bytes_read)
                    .map_err(|_| Error::session("line length exceeds u64"))?;

                if line.trim().is_empty() {
                    byte_offset = byte_offset.saturating_add(line_len);
                    continue;
                }

                let missing_newline = !line.ends_with('\n');
                let json_line = line.trim_end_matches('\n').trim_end_matches('\r');
                let frame: SegmentFrame = match serde_json::from_str(json_line) {
                    Ok(frame) => {
                        if missing_newline {
                            if i + 1 < segment_files.len() {
                                tracing::warn!(
                                    segment = %seg_path.display(),
                                    line_number,
                                    "SessionStoreV2 found an unsealed non-final segment during index rebuild; truncating segment and quarantining subsequent segments"
                                );
                                drop(reader);
                                truncate_file_to(seg_path, byte_offset)?;
                                quarantine_segment_tail(&segment_files[i + 1..])?;
                                break 'segments;
                            }

                            tracing::warn!(
                                segment = %seg_path.display(),
                                line_number,
                                "SessionStoreV2 encountered valid frame missing trailing newline; healing segment"
                            );
                            let mut f = open_regular_file_for_write(
                                seg_path,
                                false,
                                ArtifactWriteMode::Append,
                            )?;
                            f.write_all(b"\n")?;
                            f.sync_all()?;
                            line.push('\n');
                            line_len += 1;
                            // Consume the healed newline so the reader and offset accounting stay aligned.
                            let mut healed_newline = [0u8; 1];
                            reader.read_exact(&mut healed_newline).map_err(|err| {
                                Error::session(format!(
                                    "failed to consume healed newline while rebuilding index: \
                                     segment={} line={line_number}: {err}",
                                    seg_path.display()
                                ))
                            })?;
                            if healed_newline[0] != b'\n' {
                                return Err(Error::session(format!(
                                    "healed newline read back as non-newline byte while rebuilding index: \
                                     segment={} line={line_number}: 0x{:02X}",
                                    seg_path.display(),
                                    healed_newline[0]
                                )));
                            }
                        }
                        frame
                    }
                    Err(err) => {
                        let at_eof = reader.fill_buf().is_ok_and(<[u8]>::is_empty);
                        if !at_eof || !missing_newline {
                            return Err(Error::session(format!(
                                "failed to parse segment frame while rebuilding index: \
                                 segment={} line={line_number}: {err}",
                                seg_path.display()
                            )));
                        }
                        tracing::warn!(
                            segment = %seg_path.display(),
                            line_number,
                            error = %err,
                            at_eof,
                            missing_newline,
                            "SessionStoreV2 dropping corrupted frame during index rebuild; truncating segment and quarantining subsequent segments"
                        );
                        // Trim the incomplete tail so subsequent reads and appends remain valid.
                        drop(reader);
                        truncate_file_to(seg_path, byte_offset)?;
                        quarantine_segment_tail(&segment_files[i + 1..])?;
                        break 'segments;
                    }
                };

                if frame.segment_seq != *segment_seq || frame.frame_seq != expected_frame_seq {
                    tracing::warn!(
                        segment = %seg_path.display(),
                        line_number,
                        expected_segment_seq = *segment_seq,
                        actual_segment_seq = frame.segment_seq,
                        expected_frame_seq,
                        actual_frame_seq = frame.frame_seq,
                        "SessionStoreV2 detected mismatched embedded frame coordinates during rebuild; truncating segment and quarantining subsequent segments"
                    );
                    drop(reader);
                    truncate_file_to(seg_path, byte_offset)?;
                    quarantine_segment_tail(&segment_files[i + 1..])?;
                    break 'segments;
                }

                let expected_entry_seq = last_observed_seq
                    .checked_add(1)
                    .ok_or_else(|| Error::session("entry sequence overflow during rebuild"))?;
                if frame.entry_seq != expected_entry_seq {
                    tracing::warn!(
                        segment = %seg_path.display(),
                        line_number,
                        entry_seq = frame.entry_seq,
                        last_seq = last_observed_seq,
                        expected_seq = expected_entry_seq,
                        "SessionStoreV2 detected non-contiguous entry sequence during rebuild; truncating segment and quarantining subsequent segments"
                    );
                    drop(reader);
                    truncate_file_to(seg_path, byte_offset)?;
                    quarantine_segment_tail(&segment_files[i + 1..])?;
                    break 'segments;
                }
                last_observed_seq = frame.entry_seq;

                let crc = crc32c_upper(line.as_bytes());

                let index_entry = OffsetIndexEntry {
                    schema: Cow::Borrowed(OFFSET_INDEX_SCHEMA),
                    entry_seq: frame.entry_seq,
                    entry_id: frame.entry_id.clone(),
                    segment_seq: *segment_seq,
                    frame_seq: expected_frame_seq,
                    byte_offset,
                    byte_length: line_len,
                    crc32c: crc.clone(),
                    state: Cow::Borrowed("active"),
                };
                serde_json::to_writer(&mut index_writer, &index_entry)?;
                index_writer.write_all(b"\n")?;

                self.chain_hash = chain_hash_step(&self.chain_hash, &frame.payload_sha256);
                self.total_bytes = self.total_bytes.saturating_add(line_len);
                self.last_entry_id = Some(frame.entry_id);
                self.last_crc32c = crc;

                byte_offset = byte_offset.saturating_add(line_len);
                rebuilt_count = rebuilt_count.saturating_add(1);
                expected_frame_seq = expected_frame_seq
                    .checked_add(1)
                    .ok_or_else(|| Error::session("frame sequence overflow during rebuild"))?;
            }
        }

        index_writer.flush()?;
        let file = index_writer
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?;
        file.sync_all()?;
        drop(file); // Close the file handle before renaming (fixes Windows ERROR_SHARING_VIOLATION)

        // Atomically replace the old index with the rebuilt one
        rename_regular_file(&index_tmp_path, &index_path)?;
        sync_parent_dir(&index_path)?;

        self.next_segment_seq = 1;
        self.next_frame_seq = 1;
        self.next_entry_seq = 1;
        self.current_segment_bytes = 0;
        self.bootstrap_from_disk()?;

        Ok(rebuilt_count)
    }

    pub fn validate_integrity(&self) -> Result<()> {
        let index_rows = self.read_index()?;
        let rows_by_segment = group_validated_index_rows(&index_rows)?;
        let mut parent_by_entry: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::with_capacity(index_rows.len());
        for (segment_seq, rows) in rows_by_segment {
            let segment_path = self.segment_file_path(segment_seq);
            validate_segment_index_rows(&segment_path, segment_seq, &rows, &mut parent_by_entry)?;
        }
        validate_parent_graph_links(&parent_by_entry)?;
        validate_parent_graph_acyclic(&parent_by_entry)?;
        self.validate_all_segment_bytes_indexed(&index_rows)?;
        Ok(())
    }

    /// Validate both the generic segmented-store invariants and the duplicated
    /// metadata carried by serialized [`SessionEntry`] payloads.
    ///
    /// `append_entry` is intentionally usable by low-level store tests and
    /// tooling with arbitrary JSON payloads, so the generic integrity check
    /// cannot assume a session-entry schema. Session persistence and migration
    /// paths must use this stronger check.
    pub fn validate_session_integrity(&self) -> Result<()> {
        self.validate_integrity()?;
        for frame in self.read_all_entries()? {
            frame_to_session_entry(&frame)?;
        }
        Ok(())
    }

    fn bootstrap_from_disk(&mut self) -> Result<()> {
        let index_rows = self.read_index()?;
        let (observed_index_bytes, observed_index_identity) = self.index_observation()?;
        self.observed_index_bytes = observed_index_bytes;
        self.observed_index_identity = observed_index_identity;
        if let Some(last) = index_rows.last() {
            self.next_entry_seq = last
                .entry_seq
                .checked_add(1)
                .ok_or_else(|| Error::session("entry sequence overflow while bootstrapping"))?;
            self.next_segment_seq = last.segment_seq;
            self.next_frame_seq = last
                .frame_seq
                .checked_add(1)
                .ok_or_else(|| Error::session("frame sequence overflow while bootstrapping"))?;
            let segment_path = self.segment_file_path(last.segment_seq);
            let expected_segment_bytes = last.byte_offset.saturating_add(last.byte_length);
            let actual_segment_bytes = regular_file_len(&segment_path).map_err(|err| {
                Error::session(format!(
                    "failed to stat active segment {} while bootstrapping: {err}",
                    segment_path.display()
                ))
            })?;

            if actual_segment_bytes > expected_segment_bytes {
                tracing::warn!(
                    segment = %segment_path.display(),
                    expected = expected_segment_bytes,
                    actual = actual_segment_bytes,
                    "SessionStoreV2 truncating unindexed trailing bytes from active segment after crash recovery"
                );
                truncate_file_to(&segment_path, expected_segment_bytes)?;
            }
            self.current_segment_bytes = expected_segment_bytes;
            self.last_entry_id = Some(last.entry_id.clone());
            self.last_crc32c.clone_from(&last.crc32c);

            let mut chain = GENESIS_CHAIN_HASH.to_string();
            let mut total = 0u64;
            let mut reader = SegmentFileReader::new(self);
            for row in &index_rows {
                let frame = reader.read_frame(row)?.ok_or_else(|| {
                    Error::session(format!(
                        "index references missing frame during bootstrap: entry_seq={}, segment={}",
                        row.entry_seq, row.segment_seq
                    ))
                })?;
                chain = chain_hash_step(&chain, &frame.payload_sha256);
                total = total.saturating_add(row.byte_length);
            }
            self.chain_hash = chain;
            self.total_bytes = total;
        } else {
            self.chain_hash = GENESIS_CHAIN_HASH.to_string();
            self.total_bytes = 0;
            self.last_entry_id = None;
            self.last_crc32c = "00000000".to_string();
        }
        Ok(())
    }
}

fn rollback_failure_class(error: &Error) -> &'static str {
    match error {
        Error::Session(message) => {
            if message.to_ascii_lowercase().contains("index") {
                "index_corruption"
            } else if message.to_ascii_lowercase().contains("integrity")
                || message.to_ascii_lowercase().contains("hash")
                || message.to_ascii_lowercase().contains("checksum")
            {
                "integrity_mismatch"
            } else {
                "atomicity_violation"
            }
        }
        Error::Io(_) => "io_failure",
        _ => "atomicity_violation",
    }
}

fn is_recoverable_index_error(error: &Error) -> bool {
    match error {
        Error::Json(_) => true,
        Error::Io(err) => matches!(
            err.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData
        ),
        Error::Session(message) => {
            let lower = message.to_ascii_lowercase();
            lower.contains("checksum mismatch")
                || lower.contains("index out of bounds")
                || lower.contains("index/frame mismatch")
                || lower.contains("index references missing frame")
                || lower.contains("payload integrity mismatch")
                || lower.contains("entry sequence is not strictly increasing")
                || lower.contains("index byte range overflow")
                || lower.contains("encoded frame length")
                || lower.contains("failed to stat active segment")
                || lower.contains("unterminated jsonl line")
        }
        _ => false,
    }
}

fn is_missing_active_segment_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Session(message) if message.contains("failed to stat active segment")
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParentGraphVisitState {
    Visiting,
    Visited,
}

fn validate_parent_graph_links(
    parent_by_entry: &std::collections::HashMap<String, Option<String>>,
) -> Result<()> {
    for (entry_id, parent_id) in parent_by_entry {
        if let Some(parent_id) = parent_id.as_deref()
            && !parent_by_entry.contains_key(parent_id)
        {
            return Err(Error::session(format!(
                "missing parent entry detected in session store: entry_id={entry_id} parent_id={parent_id}"
            )));
        }
    }

    Ok(())
}

fn validate_parent_graph_acyclic(
    parent_by_entry: &std::collections::HashMap<String, Option<String>>,
) -> Result<()> {
    let mut visit_state: std::collections::HashMap<&str, ParentGraphVisitState> =
        std::collections::HashMap::with_capacity(parent_by_entry.len());

    for entry_id in parent_by_entry.keys() {
        if visit_state.get(entry_id.as_str()) == Some(&ParentGraphVisitState::Visited) {
            continue;
        }

        let mut stack = vec![(entry_id.as_str(), false)];
        while let Some((current_id, expanded)) = stack.pop() {
            if expanded {
                visit_state.insert(current_id, ParentGraphVisitState::Visited);
                continue;
            }

            match visit_state.get(current_id).copied() {
                Some(ParentGraphVisitState::Visited) => continue,
                Some(ParentGraphVisitState::Visiting) => {
                    return Err(Error::session(format!(
                        "cyclic parent chain detected in session store at entry_id={current_id}"
                    )));
                }
                None => {}
            }

            visit_state.insert(current_id, ParentGraphVisitState::Visiting);
            stack.push((current_id, true));

            if let Some(parent_id) = parent_by_entry
                .get(current_id)
                .and_then(std::option::Option::as_deref)
                && parent_by_entry.contains_key(parent_id)
            {
                stack.push((parent_id, false));
            }
        }
    }

    Ok(())
}

/// Convert a V2 `SegmentFrame` payload back into a `SessionEntry`.
pub fn frame_to_session_entry(frame: &SegmentFrame) -> Result<SessionEntry> {
    // Deserialize directly from the RawValue to avoid extra allocation/copying.
    // serde_json::from_str works on RawValue.get() which is &str.
    let entry: SessionEntry = serde_json::from_str(frame.payload.get()).map_err(|e| {
        Error::session(format!(
            "failed to deserialize SessionEntry from frame entry_id={}: {e}",
            frame.entry_id
        ))
    })?;

    let base_id = entry
        .base_id()
        .ok_or_else(|| Error::session("V2 frame payload is missing its entry ID"))?;
    if base_id != &frame.entry_id {
        return Err(Error::session(format!(
            "frame entry_id mismatch: frame={} entry={}",
            frame.entry_id, base_id
        )));
    }
    if entry.base().parent_id != frame.parent_entry_id {
        return Err(Error::session(format!(
            "frame parent_entry_id mismatch for entry {}: frame={:?} entry={:?}",
            frame.entry_id,
            frame.parent_entry_id,
            entry.base().parent_id
        )));
    }
    let payload_entry_type = session_entry_type(&entry);
    if !frame.entry_type.eq(payload_entry_type) {
        return Err(Error::session(format!(
            "frame entry_type mismatch for entry {}: frame={} entry={payload_entry_type}",
            frame.entry_id, frame.entry_type
        )));
    }

    Ok(entry)
}

const fn session_entry_type(entry: &SessionEntry) -> &'static str {
    match entry {
        SessionEntry::Message(_) => "message",
        SessionEntry::ModelChange(_) => "model_change",
        SessionEntry::ThinkingLevelChange(_) => "thinking_level_change",
        SessionEntry::Compaction(_) => "compaction",
        SessionEntry::BranchSummary(_) => "branch_summary",
        SessionEntry::Label(_) => "label",
        SessionEntry::SessionInfo(_) => "session_info",
        SessionEntry::Custom(_) => "custom",
    }
}

/// Extract the V2 frame arguments from a `SessionEntry`.
pub fn session_entry_to_frame_args(
    entry: &SessionEntry,
) -> Result<(String, Option<String>, String, Value)> {
    let base = entry.base();
    let entry_id = base
        .id
        .clone()
        .ok_or_else(|| Error::session("SessionEntry has no id"))?;
    let parent_entry_id = base.parent_id.clone();

    let entry_type = session_entry_type(entry);

    let payload = serde_json::to_value(entry).map_err(|e| {
        Error::session(format!(
            "failed to serialize SessionEntry to frame payload: {e}"
        ))
    })?;

    Ok((entry_id, parent_entry_id, entry_type.to_string(), payload))
}

/// Helper to cache the file descriptor when reading multiple frames sequentially.
struct SegmentFileReader<'a> {
    store: &'a SessionStoreV2,
    current_segment_seq: Option<u64>,
    current_file: Option<File>,
    current_len: u64,
}

impl<'a> SegmentFileReader<'a> {
    const fn new(store: &'a SessionStoreV2) -> Self {
        Self {
            store,
            current_segment_seq: None,
            current_file: None,
            current_len: 0,
        }
    }

    fn read_frame(&mut self, row: &OffsetIndexEntry) -> Result<Option<SegmentFrame>> {
        validate_encoded_frame_length(row.byte_length, self.store.max_segment_bytes)?;
        if self.current_segment_seq != Some(row.segment_seq) {
            self.current_segment_seq = Some(row.segment_seq);
            let path = self.store.segment_file_path(row.segment_seq);
            if let Some(file) = open_regular_file_for_read(&path)? {
                self.current_len = file.metadata()?.len();
                if self.current_len > self.store.max_segment_bytes {
                    return Err(Error::session(format!(
                        "segment {} length {} exceeds configured segment limit {}",
                        path.display(),
                        self.current_len,
                        self.store.max_segment_bytes
                    )));
                }
                self.current_file = Some(file);
            } else {
                self.current_file = None;
            }
        }

        let file = self.current_file.as_mut().ok_or_else(|| {
            Error::session(format!(
                "index references missing segment {}",
                self.store.segment_file_path(row.segment_seq).display()
            ))
        })?;

        let byte_len = checked_frame_read_len(row.byte_length)?;
        let end_offset = row
            .byte_offset
            .checked_add(row.byte_length)
            .ok_or_else(|| Error::session("index byte range overflow"))?;

        if end_offset > self.current_len {
            return Err(Error::session(format!(
                "index out of bounds for segment {}: end={} len={}",
                self.store.segment_file_path(row.segment_seq).display(),
                end_offset,
                self.current_len
            )));
        }

        file.seek(SeekFrom::Start(row.byte_offset))?;
        let mut buf = vec![0u8; byte_len];
        file.read_exact(&mut buf)?;
        let frame = decode_indexed_frame_record(buf, row)?;
        Ok(Some(frame))
    }
}

/// Validate the parent relationships visible to a bounded fetch without
/// reading unrelated frame bodies. Every fetched parent must exist in the
/// validated index, and cycles composed entirely of fetched frames (including
/// a self-parent) are rejected. Forward references remain valid because V2
/// preserves authoritative JSONL order; an exhaustive graph audit therefore
/// still requires fetching every frame.
fn validate_fetched_parent_graph(
    index_rows: &[OffsetIndexEntry],
    frames: &[SegmentFrame],
) -> Result<()> {
    let mut indexed_entry_ids = std::collections::HashSet::with_capacity(index_rows.len());
    for row in index_rows {
        if !indexed_entry_ids.insert(row.entry_id.as_str()) {
            return Err(Error::session(format!(
                "duplicate entry_id detected in offset index: {}",
                row.entry_id
            )));
        }
    }

    let mut fetched_parents = std::collections::HashMap::with_capacity(frames.len());
    for frame in frames {
        if let Some(parent_id) = frame.parent_entry_id.as_deref()
            && !indexed_entry_ids.contains(parent_id)
        {
            return Err(Error::session(format!(
                "missing parent entry detected for fetched frame: entry_id={} parent_id={parent_id}",
                frame.entry_id
            )));
        }
        if fetched_parents
            .insert(frame.entry_id.clone(), frame.parent_entry_id.clone())
            .is_some()
        {
            return Err(Error::session(format!(
                "duplicate fetched frame entry_id: {}",
                frame.entry_id
            )));
        }
    }
    validate_parent_graph_acyclic(&fetched_parents)
}

/// Compute next hash chain value: `SHA-256(prev_chain_hex || payload_sha256_hex)`.
fn chain_hash_step(prev_chain: &str, payload_sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prev_chain.as_bytes());
    hasher.update(payload_sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn manifest_hash_hex(manifest: &Manifest) -> Result<String> {
    let encoded = serde_json::to_vec(manifest)?;
    Ok(format!("{:x}", Sha256::digest(&encoded)))
}

/// Derive the V2 sidecar store root from a JSONL session file path.
pub fn v2_sidecar_path(jsonl_path: &Path) -> PathBuf {
    let stem = jsonl_path.file_stem().map_or_else(
        || "session".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    let parent = jsonl_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.v2"))
}

/// Check whether a V2 sidecar store exists for the given JSONL session.
pub fn has_v2_sidecar(jsonl_path: &Path) -> bool {
    let root = v2_sidecar_path(jsonl_path);
    root.join("manifest.json").exists() || root.join("index").join("offsets.jsonl").exists()
}

fn append_jsonl_line<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let file = open_regular_file_for_write(path, true, ArtifactWriteMode::Append)?;
    let mut writer = std::io::BufWriter::new(file);
    // Serialize directly to buffered file — avoids intermediate Vec<u8> allocation
    // while preventing excessive write syscalls.
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

fn append_jsonl_line_durable<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    append_jsonl_line_durable_with(path, value, File::sync_all)
}

fn append_jsonl_line_durable_with<T, F>(path: &Path, value: &T, sync: F) -> Result<()>
where
    T: Serialize,
    F: FnOnce(&File) -> std::io::Result<()>,
{
    let file = open_regular_file_for_write(path, true, ArtifactWriteMode::Append)?;
    let mut writer = std::io::BufWriter::new(file);
    serde_json::to_writer(&mut writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    let file = writer
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?;
    sync(&file)?;
    sync_parent_dir(path)?;
    Ok(())
}

fn truncate_file_to(path: &Path, len: u64) -> Result<()> {
    let file = open_regular_file_for_write(path, false, ArtifactWriteMode::Preserve)?;
    file.set_len(len)?;
    file.sync_all()?;
    Ok(())
}

fn quarantine_segment_file(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::session(format!("segment has no parent: {}", path.display())))?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| Error::session(format!("segment has no filename: {}", path.display())))?;

    for suffix in 0u32..10_000 {
        let backup_name = if suffix == 0 {
            format!("{file_name}.bak")
        } else {
            format!("{file_name}.bak.{suffix}")
        };
        let backup_path = parent.join(backup_name);
        if path_entry_exists(&backup_path)? {
            continue;
        }

        rename_regular_file(path, &backup_path).map_err(|err| {
            Error::session(format!(
                "failed to quarantine segment {} -> {}: {err}",
                path.display(),
                backup_path.display()
            ))
        })?;
        return Ok(backup_path);
    }

    Err(Error::session(format!(
        "failed to quarantine segment {}: exhausted backup suffixes",
        path.display()
    )))
}

fn quarantine_segment_tail(segment_files: &[(u64, PathBuf)]) -> Result<()> {
    for (_, path) in segment_files {
        let backup_path = quarantine_segment_file(path)?;
        tracing::warn!(
            segment = %path.display(),
            backup = %backup_path.display(),
            "SessionStoreV2 quarantined trailing segment during rebuild"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    open_private_directory(parent, false)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn write_jsonl_lines<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    write_jsonl_lines_with_digest(path, rows).map(|_| ())
}

fn write_jsonl_lines_with_digest<T: Serialize>(path: &Path, rows: &[T]) -> Result<(u64, String)> {
    let file = open_regular_file_for_write(path, true, ArtifactWriteMode::Replace)?;
    let mut writer = std::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut byte_count = 0u64;
    for row in rows {
        let mut encoded = serde_json::to_vec(row)?;
        encoded.push(b'\n');
        writer.write_all(&encoded)?;
        hasher.update(&encoded);
        byte_count = byte_count
            .checked_add(
                u64::try_from(encoded.len())
                    .map_err(|_| Error::session("serialized index row length exceeds u64"))?,
            )
            .ok_or_else(|| Error::session("serialized index length overflow"))?;
    }
    writer.flush()?;
    let file = writer
        .into_inner()
        .map_err(std::io::IntoInnerError::into_error)?;
    file.sync_all()?;
    Ok((byte_count, format!("{:x}", hasher.finalize())))
}

fn regular_file_sha256(path: &Path) -> Result<(u64, String)> {
    let mut file = open_regular_file_for_read(path)?
        .ok_or_else(|| Error::session(format!("artifact not found: {}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut byte_count = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        byte_count = byte_count
            .checked_add(
                u64::try_from(read)
                    .map_err(|_| Error::session("artifact read length exceeds u64"))?,
            )
            .ok_or_else(|| Error::session("artifact length overflow while hashing"))?;
    }
    Ok((byte_count, format!("{:x}", hasher.finalize())))
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>> {
    let Some(file) = open_regular_file_for_read(path)? else {
        return Ok(Vec::new());
    };
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = read_line_with_limit(&mut reader, &mut line, MAX_FRAME_READ_BYTES)
            .map_err(|e| Error::Io(Box::new(e)))?;
        if bytes_read == 0 {
            break;
        }
        if !line.ends_with('\n') {
            return Err(Error::session(format!(
                "unterminated JSONL line in {}",
                path.display()
            )));
        }
        if line.trim().is_empty() {
            continue;
        }
        let json_line = line.trim_end_matches('\n').trim_end_matches('\r');
        out.push(serde_json::from_str::<T>(json_line)?);
    }
    Ok(out)
}

fn payload_hash_and_size(payload: &RawValue) -> Result<(String, u64)> {
    // For RawValue, we can just get the string content directly.
    let bytes = payload.get().as_bytes();
    let payload_bytes = u64::try_from(bytes.len())
        .map_err(|_| Error::session(format!("payload is too large: {} bytes", bytes.len())))?;
    let hash = format!("{:x}", Sha256::digest(bytes));
    Ok((hash, payload_bytes))
}

fn line_length_u64(encoded: &[u8]) -> Result<u64> {
    let line_len = encoded
        .len()
        .checked_add(1)
        .ok_or_else(|| Error::session("line length overflow"))?;
    u64::try_from(line_len).map_err(|_| Error::session("line length exceeds u64"))
}

fn crc32c_upper(data: &[u8]) -> String {
    let crc = crc32c::crc32c(data);
    format!("{crc:08X}")
}

fn read_line_with_limit<R: BufRead>(
    reader: &mut R,
    buf: &mut String,
    limit: u64,
) -> std::io::Result<usize> {
    let mut take = reader.take(limit);
    let n = take.read_line(buf)?;
    if n > 0 && take.limit() == 0 && !buf.ends_with('\n') {
        // We reached the limit, but this might just be the exact end of the file.
        // Check if there is more data in the underlying reader.
        let is_eof = take.into_inner().fill_buf()?.is_empty();
        if !is_eof {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Line length exceeds limit of {limit} bytes"),
            ));
        }
    }
    Ok(n)
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;

    fn snapshot_artifacts(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
            let mut entries: Vec<_> = fs::read_dir(path)
                .expect("read snapshot directory")
                .map(|entry| entry.expect("read snapshot entry"))
                .collect();
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let entry_path = entry.path();
                let metadata = fs::symlink_metadata(&entry_path).expect("snapshot metadata");
                if metadata.is_dir() {
                    visit(root, &entry_path, snapshot);
                } else if metadata.is_file() {
                    snapshot.insert(
                        entry_path
                            .strip_prefix(root)
                            .expect("snapshot path beneath root")
                            .to_path_buf(),
                        fs::read(&entry_path).expect("snapshot artifact bytes"),
                    );
                }
            }
        }

        let mut snapshot = BTreeMap::new();
        visit(root, root, &mut snapshot);
        snapshot
    }

    fn append_test_entries(store: &mut SessionStoreV2, start: u64, end: u64) {
        let mut parent = store.head().map(|head| head.entry_id);
        for ordinal in start..=end {
            let entry_id = format!("entry-{ordinal:08}");
            store
                .append_entry(
                    entry_id.clone(),
                    parent,
                    "message",
                    json!({"ordinal": ordinal}),
                )
                .expect("append checkpoint fixture entry");
            parent = Some(entry_id);
        }
    }

    fn test_migration_event(migration_id: &str) -> MigrationEvent {
        MigrationEvent {
            schema: MIGRATION_EVENT_SCHEMA.to_string(),
            migration_id: migration_id.to_string(),
            phase: "completed".to_string(),
            at: "2026-01-01T00:00:00.000Z".to_string(),
            source_path: "source.jsonl".to_string(),
            target_path: "source.v2".to_string(),
            source_format: "jsonl_v3".to_string(),
            target_format: "native_v2".to_string(),
            verification: MigrationVerification {
                entry_count_match: true,
                hash_chain_match: true,
                index_consistent: true,
            },
            outcome: "ok".to_string(),
            error_class: None,
            correlation_id: "ledger_append_test".to_string(),
        }
    }

    fn assert_invalid_checkpoint_preserves_store(mut checkpoint_bytes: Vec<u8>) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 512).expect("create checkpoint store");
        append_test_entries(&mut store, 1, 5);
        let checkpoint = store
            .create_checkpoint(1, "manual")
            .expect("create checkpoint");
        append_test_entries(&mut store, 6, 9);

        if checkpoint_bytes.is_empty() {
            checkpoint_bytes = fs::read(store.checkpoint_path(1)).expect("read checkpoint fixture");
        }
        let checkpoint_path = store.checkpoint_path(1);
        let mut checkpoint_file =
            open_regular_file_for_write(&checkpoint_path, false, ArtifactWriteMode::Replace)
                .expect("open checkpoint fixture for tampering");
        checkpoint_file
            .write_all(&checkpoint_bytes)
            .expect("write invalid checkpoint fixture");
        checkpoint_file.sync_all().expect("sync invalid checkpoint");
        drop(checkpoint_file);

        let before = snapshot_artifacts(&root);
        let error = store
            .rollback_to_checkpoint(
                1,
                "00000000-0000-0000-0000-000000000001",
                "rollback_invalid_checkpoint",
            )
            .expect_err("invalid checkpoint must fail before rollback mutation");
        assert!(
            error.to_string().contains(ROLLBACK_PREFLIGHT_REJECTION),
            "unexpected rollback error: {error}"
        );
        assert_eq!(
            snapshot_artifacts(&root),
            before,
            "preflight rejection changed a V2 artifact; checkpoint={checkpoint:?}"
        );
    }

    fn mutated_checkpoint_bytes(field: &str, value: Value) -> Vec<u8> {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store =
            SessionStoreV2::create(&root, 4096).expect("create source checkpoint store");
        append_test_entries(&mut store, 1, 2);
        store
            .create_checkpoint(1, "manual")
            .expect("create source checkpoint");
        let mut document: Value = serde_json::from_slice(
            &fs::read(store.checkpoint_path(1)).expect("read source checkpoint"),
        )
        .expect("parse source checkpoint");
        document[field] = value;
        serde_json::to_vec_pretty(&document).expect("serialize mutated checkpoint")
    }

    fn stage_durable_rollback_intent(
        store: &SessionStoreV2,
        checkpoint_seq: u64,
    ) -> RollbackIntent {
        let plan = store
            .prepare_rollback_plan(checkpoint_seq)
            .expect("prepare rollback plan");
        let (retained_index_bytes, retained_index_sha256) = write_jsonl_lines_with_digest(
            &store.rollback_index_stage_path(),
            &plan.retained_index_rows,
        )
        .expect("stage retained index");
        let intent = RollbackIntent {
            schema: ROLLBACK_INTENT_SCHEMA.to_string(),
            checkpoint: plan.checkpoint,
            migration_id: "00000000-0000-0000-0000-000000000099".to_string(),
            correlation_id: "rollback_crash_recovery".to_string(),
            retained_index_sha256,
            retained_index_bytes,
            manifest: store
                .rollback_manifest_context()
                .expect("capture rollback manifest context"),
        };
        store
            .write_rollback_intent(&intent)
            .expect("persist rollback intent");
        intent
    }

    #[test]
    fn durable_jsonl_append_propagates_descriptor_sync_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("ledger.jsonl");
        let error = append_jsonl_line_durable_with(&path, &json!({"outcome": "ok"}), |_| {
            Err(std::io::Error::other("injected descriptor sync failure"))
        })
        .expect_err("a failed durability boundary must fail the ledger append");

        assert!(
            matches!(error, Error::Io(ref source) if source.kind() == std::io::ErrorKind::Other),
            "unexpected durable append error: {error}"
        );
    }

    #[test]
    fn migration_event_read_waits_for_in_flight_append() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let store = SessionStoreV2::create(&root, 4096).expect("create store");
        store
            .append_migration_event(test_migration_event("00000000-0000-0000-0000-000000000001"))
            .expect("seed migration ledger");

        let mutation_lock = store.lock_store_mutation().expect("lock mutation");
        let pending = test_migration_event("00000000-0000-0000-0000-000000000002");
        let encoded = serde_json::to_vec(&pending).expect("serialize pending event");
        let split = encoded.len() / 2;
        assert!(split > 0 && split < encoded.len());
        let mut ledger = open_regular_file_for_write(
            &store.migration_ledger_path(),
            false,
            ArtifactWriteMode::Append,
        )
        .expect("open migration ledger");
        ledger
            .write_all(&encoded[..split])
            .expect("write partial event");
        ledger.sync_all().expect("sync partial event");

        let reader_store = store;
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(0);
        let reader = std::thread::spawn(move || {
            started_tx.send(()).expect("announce reader start");
            finished_tx
                .send(reader_store.read_migration_events())
                .expect("return reader result");
        });
        started_rx.recv().expect("reader must start");
        match finished_rx.recv_timeout(std::time::Duration::from_millis(250)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("migration ledger reader disconnected while append lock was held")
            }
            Ok(result) => {
                panic!("migration ledger reader completed during a partial append: {result:?}")
            }
        }

        ledger
            .write_all(&encoded[split..])
            .expect("finish pending event");
        ledger.write_all(b"\n").expect("terminate pending event");
        ledger.sync_all().expect("sync completed event");
        drop(ledger);
        drop(mutation_lock);

        let events = finished_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reader must finish after append lock release")
            .expect("reader must observe only complete ledger records");
        reader.join().expect("join migration ledger reader");
        assert_eq!(events.len(), 2);
        assert_eq!(events[1], pending);
    }

    #[test]
    fn migration_append_rejects_corrupt_existing_ledger_without_mutation() {
        for case in [
            "unterminated",
            "invalid",
            "semantic",
            "unknown_event_field",
            "unknown_verification_field",
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().join(case);
            let store = SessionStoreV2::create(&root, 4096).expect("create store");
            store
                .append_migration_event(test_migration_event(
                    "00000000-0000-0000-0000-000000000001",
                ))
                .expect("seed migration ledger");

            let ledger_path = store.migration_ledger_path();
            let corrupt = match case {
                "unterminated" => {
                    let mut bytes = fs::read(&ledger_path).expect("read seeded ledger");
                    assert_eq!(bytes.pop(), Some(b'\n'));
                    bytes
                }
                "invalid" => b"{not valid migration JSON}\n".to_vec(),
                "semantic" => {
                    let mut event = test_migration_event("00000000-0000-0000-0000-000000000003");
                    event.phase = "invented_phase".to_string();
                    let mut bytes = serde_json::to_vec(&event).expect("serialize forged event");
                    bytes.push(b'\n');
                    bytes
                }
                "unknown_event_field" => {
                    let mut event = serde_json::to_value(test_migration_event(
                        "00000000-0000-0000-0000-000000000004",
                    ))
                    .expect("serialize event document");
                    event["unexpected"] = Value::Bool(true);
                    let mut bytes = serde_json::to_vec(&event).expect("serialize forged event");
                    bytes.push(b'\n');
                    bytes
                }
                "unknown_verification_field" => {
                    let mut event = serde_json::to_value(test_migration_event(
                        "00000000-0000-0000-0000-000000000005",
                    ))
                    .expect("serialize event document");
                    event["verification"]["unexpected"] = Value::Bool(true);
                    let mut bytes = serde_json::to_vec(&event).expect("serialize forged event");
                    bytes.push(b'\n');
                    bytes
                }
                _ => unreachable!("fixed corruption case"),
            };
            fs::write(&ledger_path, &corrupt).expect("install corrupt ledger fixture");
            let before = fs::read(&ledger_path).expect("snapshot corrupt ledger");

            store
                .read_migration_events()
                .expect_err("corrupt forensic ledger must fail at the read boundary");

            store
                .append_migration_event(test_migration_event(
                    "00000000-0000-0000-0000-000000000002",
                ))
                .expect_err("corrupt forensic ledger must reject a later append");
            assert_eq!(
                fs::read(&ledger_path).expect("reread rejected ledger"),
                before,
                "{case} ledger changed despite fail-closed append"
            );
        }
    }

    #[test]
    fn migration_event_append_rejects_contract_violations_without_mutation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = SessionStoreV2::create(tmp.path().join("store"), 4096).expect("create store");
        store
            .append_migration_event(test_migration_event("00000000-0000-0000-0000-000000000001"))
            .expect("seed valid migration ledger");
        let ledger_path = store.migration_ledger_path();
        let before = fs::read(&ledger_path).expect("snapshot valid ledger");

        let valid = test_migration_event("00000000-0000-0000-0000-000000000002");
        let mut cases = Vec::new();

        let mut bad_schema = valid.clone();
        bad_schema.schema = "pi.session_store_v2.migration_event.v999".to_string();
        cases.push(("schema", bad_schema));

        let mut bad_uuid = valid.clone();
        bad_uuid.migration_id = "not-a-uuid".to_string();
        cases.push(("UUID", bad_uuid));

        let mut bad_phase = valid.clone();
        bad_phase.phase = "copying".to_string();
        cases.push(("phase enum", bad_phase));

        let mut bad_timestamp = valid.clone();
        bad_timestamp.at = "yesterday".to_string();
        cases.push(("timestamp", bad_timestamp));

        let mut bad_path = valid.clone();
        bad_path.target_path.clear();
        cases.push(("path", bad_path));

        let mut bad_format = valid.clone();
        bad_format.source_format = "pickle_v1".to_string();
        cases.push(("format enum", bad_format));

        let mut bad_outcome = valid.clone();
        bad_outcome.outcome = "maybe".to_string();
        cases.push(("outcome enum", bad_outcome));

        let mut bad_error_class = valid.clone();
        bad_error_class.error_class = Some("unknown_failure".to_string());
        cases.push(("errorClass enum", bad_error_class));

        let mut bad_correlation = valid;
        bad_correlation.correlation_id = "short".to_string();
        cases.push(("correlation", bad_correlation));

        for (case, event) in cases {
            assert!(
                store.append_migration_event(event).is_err(),
                "{case} violation must reject append"
            );
            assert_eq!(
                fs::read(&ledger_path).expect("reread ledger after rejection"),
                before,
                "{case} rejection changed the ledger"
            );
        }
    }

    #[test]
    fn migration_event_correlation_contract_accepts_only_eight_to_128_safe_bytes() {
        let mut event = test_migration_event("00000000-0000-0000-0000-000000000001");
        event.correlation_id = "12345678".to_string();
        validate_migration_event_document(&event).expect("eight-byte correlation boundary");
        event.correlation_id = "a".repeat(128);
        validate_migration_event_document(&event).expect("128-byte correlation boundary");
        event.correlation_id = "a".repeat(7);
        validate_migration_event_document(&event).expect_err("seven bytes must fail");
        event.correlation_id = "a".repeat(129);
        validate_migration_event_document(&event).expect_err("129 bytes must fail");
        event.correlation_id = "unsafe/path".to_string();
        validate_migration_event_document(&event).expect_err("unsafe correlation byte must fail");
    }

    #[test]
    fn existing_regular_file_accepts_noncreating_write_open() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("existing.jsonl");
        let mut created = open_regular_file_for_write(&path, true, ArtifactWriteMode::CreateNew)
            .expect("create private regular file");
        created.write_all(b"seed\n").expect("write seed bytes");
        created.sync_all().expect("sync seed bytes");
        drop(created);

        open_regular_file_for_write(&path, false, ArtifactWriteMode::Preserve)
            .expect("open existing file without O_CREAT")
            .sync_all()
            .expect("sync existing file");
        assert_eq!(fs::read(&path).expect("read existing file"), b"seed\n");
    }

    #[cfg(unix)]
    #[test]
    fn no_replace_publication_rejects_target_created_at_publish_boundary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        drop(open_private_directory(tmp.path(), true).expect("privatize tempdir"));
        let source = tmp.path().join("source.tmp");
        let target = tmp.path().join("immutable.json");
        let mut source_file =
            open_regular_file_for_write(&source, true, ArtifactWriteMode::CreateNew)
                .expect("create source artifact");
        source_file
            .write_all(b"source")
            .expect("write source artifact");
        source_file.sync_all().expect("sync source artifact");
        drop(source_file);

        rename_regular_file_no_replace_with(&source, &target, || {
            fs::write(&target, b"racing publisher")
        })
        .expect_err("an atomic no-replace publication must lose to an existing target");

        assert_eq!(
            fs::read(&target).expect("read winning target"),
            b"racing publisher"
        );
        assert_eq!(fs::read(&source).expect("read retained source"), b"source");
    }

    #[cfg(unix)]
    #[test]
    fn no_replace_publication_rejects_a_swapped_source_name() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        drop(open_private_directory(tmp.path(), true).expect("privatize tempdir"));
        let source = tmp.path().join("source.tmp");
        let retained_source = tmp.path().join("retained-source.tmp");
        let target = tmp.path().join("immutable.json");
        let mut source_file =
            open_regular_file_for_write(&source, true, ArtifactWriteMode::CreateNew)
                .expect("create source artifact");
        source_file
            .write_all(b"intended source")
            .expect("write source artifact");
        source_file.sync_all().expect("sync source artifact");
        drop(source_file);

        rename_regular_file_no_replace_with(&source, &target, || {
            fs::rename(&source, &retained_source)?;
            fs::write(&source, b"replacement")?;
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600))
        })
        .expect_err("publication must reject a source-name replacement");

        assert!(!target.exists(), "a swapped source must not be published");
        assert_eq!(
            fs::read(&retained_source).expect("read intended source"),
            b"intended source"
        );
        assert_eq!(fs::read(&source).expect("read replacement"), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn removal_rejects_a_swapped_source_name_without_deleting_either_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        drop(open_private_directory(tmp.path(), true).expect("privatize tempdir"));
        let source = tmp.path().join("source.tmp");
        let retained_source = tmp.path().join("retained-source.tmp");
        let mut source_file =
            open_regular_file_for_write(&source, true, ArtifactWriteMode::CreateNew)
                .expect("create source artifact");
        source_file
            .write_all(b"intended source")
            .expect("write source artifact");
        source_file.sync_all().expect("sync source artifact");
        drop(source_file);

        remove_regular_file_with(&source, || {
            fs::rename(&source, &retained_source)?;
            fs::write(&source, b"replacement")?;
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600))
        })
        .expect_err("removal must reject a source-name replacement");

        assert_eq!(
            fs::read(&retained_source).expect("read intended source"),
            b"intended source"
        );
        assert_eq!(fs::read(&source).expect("read replacement"), b"replacement");
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn hard_link_publication_helper_never_replaces_an_existing_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let directory = open_private_directory(tmp.path(), true).expect("privatize tempdir");
        let source = tmp.path().join("source.tmp");
        let target = tmp.path().join("immutable.json");
        for (path, bytes) in [
            (&source, b"source".as_slice()),
            (&target, b"target".as_slice()),
        ] {
            let mut file = open_regular_file_for_write(path, true, ArtifactWriteMode::CreateNew)
                .expect("create private publication artifact");
            file.write_all(bytes).expect("write publication artifact");
            file.sync_all().expect("sync publication artifact");
        }

        let source_file = open_regular_file_for_read(&source)
            .expect("open source")
            .expect("source exists");
        let error = publish_regular_file_via_hard_link_no_replace(
            &source_file,
            &directory,
            source.file_name().expect("source name"),
            &directory,
            target.file_name().expect("target name"),
        )
        .expect_err("hard-link publication must reject an occupied target name");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).expect("read source"), b"source");
        assert_eq!(fs::read(&target).expect("read target"), b"target");
    }

    #[cfg(windows)]
    #[test]
    fn windows_artifact_handles_pin_parent_components_and_final_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = tmp.path().join("store").join("segments");
        drop(open_private_directory(&parent, true).expect("create pinned parent"));
        let artifact = parent.join("0000000000000001.seg");
        let mut created =
            open_regular_file_for_write(&artifact, true, ArtifactWriteMode::CreateNew)
                .expect("create private artifact");
        created.write_all(b"frame\n").expect("write artifact");
        created.sync_all().expect("sync artifact");
        drop(created);

        let (_operation_path, parent_guards) =
            open_or_create_windows_artifact_parent(&artifact, false)
                .expect("pin artifact parent components");
        assert!(!parent_guards.is_empty());
        let moved_parent = tmp.path().join("moved-segments");
        fs::rename(&parent, &moved_parent)
            .expect_err("a pinned directory component must reject replacement");

        let artifact_handle = open_regular_file_for_read(&artifact)
            .expect("open pinned artifact")
            .expect("artifact exists");
        let moved_artifact = parent.join("moved.seg");
        fs::rename(&artifact, &moved_artifact)
            .expect_err("a final artifact opened without FILE_SHARE_DELETE must reject rename");
        assert_eq!(
            artifact_handle.metadata().expect("opened metadata").len(),
            6
        );
    }

    #[test]
    fn checkpoint_limits_accept_exact_caps_and_reject_cap_plus_one() {
        validate_checkpoint_reason("manual").expect("contract checkpoint reason must be accepted");
        let reason_error = validate_checkpoint_reason(&"r".repeat(MAX_CHECKPOINT_REASON_BYTES + 1))
            .expect_err("reason cap plus one must fail");
        assert!(reason_error.to_string().contains("checkpoint reason"));

        validate_checkpoint_encoded_length(MAX_CHECKPOINT_BYTES)
            .expect("exact serialized checkpoint cap must be accepted");
        let encoded_error = validate_checkpoint_encoded_length(MAX_CHECKPOINT_BYTES + 1)
            .expect_err("serialized checkpoint cap plus one must fail");
        assert!(encoded_error.to_string().contains("serialized checkpoint"));

        validate_manifest_encoded_length(MAX_MANIFEST_BYTES)
            .expect("exact serialized manifest cap must be accepted");
        let manifest_error = validate_manifest_encoded_length(MAX_MANIFEST_BYTES + 1)
            .expect_err("serialized manifest cap plus one must fail");
        assert!(manifest_error.to_string().contains("serialized manifest"));
    }

    #[test]
    fn oversized_checkpoint_reason_is_rejected_before_artifact_mutation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 4096).expect("create store");
        let before = snapshot_artifacts(&root);
        store
            .create_checkpoint(1, &"r".repeat(MAX_CHECKPOINT_REASON_BYTES + 1))
            .expect_err("oversized reason must fail");
        assert_eq!(snapshot_artifacts(&root), before);
    }

    #[test]
    fn corrupt_oversized_and_forged_checkpoints_leave_all_artifacts_unchanged() {
        assert_invalid_checkpoint_preserves_store(b"{not-json".to_vec());
        let oversized_checkpoint_len = usize::try_from(MAX_CHECKPOINT_BYTES)
            .expect("the checkpoint read cap fits usize")
            .checked_add(1)
            .expect("checkpoint test length fits usize");
        assert_invalid_checkpoint_preserves_store(vec![b' '; oversized_checkpoint_len]);
        assert_invalid_checkpoint_preserves_store(mutated_checkpoint_bytes(
            "checkpointSeq",
            json!(2),
        ));
        assert_invalid_checkpoint_preserves_store(mutated_checkpoint_bytes(
            "snapshotRef",
            json!("checkpoints/../0000000000000001.json"),
        ));
        assert_invalid_checkpoint_preserves_store(mutated_checkpoint_bytes(
            "headEntrySeq",
            json!(500),
        ));
        assert_invalid_checkpoint_preserves_store(mutated_checkpoint_bytes(
            "headEntrySeq",
            json!(0),
        ));
        assert_invalid_checkpoint_preserves_store(mutated_checkpoint_bytes(
            "reason",
            json!("operator_note"),
        ));
        assert_invalid_checkpoint_preserves_store(mutated_checkpoint_bytes(
            "chainHash",
            json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        ));
        assert_invalid_checkpoint_preserves_store(mutated_checkpoint_bytes(
            "chainHash",
            json!("ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"),
        ));
    }

    #[test]
    fn checkpoint_creation_is_positive_immutable_and_neighbor_monotonic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 4096).expect("create store");
        append_test_entries(&mut store, 1, 2);
        store
            .create_checkpoint(0, "manual")
            .expect_err("checkpoint zero must be rejected");
        let checkpoint = store
            .create_checkpoint(2, "manual")
            .expect("create following checkpoint");
        let checkpoint_bytes =
            fs::read(store.checkpoint_path(2)).expect("read immutable checkpoint");
        store
            .create_checkpoint(2, "manual")
            .expect_err("an existing checkpoint must never be replaced");
        assert_eq!(
            fs::read(store.checkpoint_path(2)).expect("reread immutable checkpoint"),
            checkpoint_bytes
        );

        append_test_entries(&mut store, 3, 3);
        let error = store
            .create_checkpoint(1, "manual")
            .expect_err("inserted checkpoint must not exceed its following neighbor");
        assert!(error.to_string().contains("following checkpoint"));
        assert_eq!(checkpoint.head_entry_seq, 2);
    }

    #[test]
    fn checkpoint_creation_recovers_from_stale_regular_temp_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 4096).expect("create store");
        append_test_entries(&mut store, 1, 2);

        let checkpoint_tmp = root.join("tmp/0000000000000001.json.tmp");
        fs::write(
            &checkpoint_tmp,
            b"partial checkpoint from interrupted writer",
        )
        .expect("seed stale checkpoint temp file");

        let checkpoint = store
            .create_checkpoint(1, "manual")
            .expect("retry checkpoint creation after interrupted temp write");

        assert_eq!(checkpoint.checkpoint_seq, 1);
        assert!(
            !path_entry_exists(&checkpoint_tmp).expect("stat stale checkpoint temp path"),
            "successful publication must consume the staging file"
        );
        assert_eq!(
            store
                .read_checkpoint(1)
                .expect("read checkpoint")
                .expect("published checkpoint"),
            checkpoint
        );
    }

    #[test]
    fn create_recovers_durable_rollback_intent_before_index_publication() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 512).expect("create rollback store");
        append_test_entries(&mut store, 1, 3);
        store
            .create_checkpoint(1, "manual")
            .expect("create rollback checkpoint");
        append_test_entries(&mut store, 4, 7);
        stage_durable_rollback_intent(&store, 1);
        drop(store);

        let reopened = SessionStoreV2::create(&root, 512).expect("recover rollback intent");
        assert_eq!(reopened.entry_count(), 3);
        reopened
            .validate_integrity()
            .expect("validate recovered rollback");
        assert_eq!(
            reopened
                .read_migration_events()
                .expect("read rollback ledger")
                .iter()
                .filter(|event| event.phase == "rollback" && event.outcome == "ok")
                .count(),
            1
        );
        assert!(!path_entry_exists(&reopened.rollback_intent_path()).expect("stat intent"));
    }

    #[test]
    fn create_recovers_durable_rollback_intent_after_index_publication() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 512).expect("create rollback store");
        append_test_entries(&mut store, 1, 3);
        store
            .create_checkpoint(1, "manual")
            .expect("create rollback checkpoint");
        append_test_entries(&mut store, 4, 7);
        let intent = stage_durable_rollback_intent(&store, 1);
        store
            .install_rollback_index(&intent)
            .expect("publish rollback index");
        drop(store);

        let reopened = SessionStoreV2::create(&root, 512).expect("finish rollback recovery");
        assert_eq!(reopened.entry_count(), 3);
        reopened
            .validate_integrity()
            .expect("validate recovered rollback");
        assert!(!path_entry_exists(&reopened.rollback_intent_path()).expect("stat intent"));
    }

    #[test]
    fn corrupted_post_mutation_rollback_stage_emits_fatal_evidence() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 512).expect("create rollback store");
        append_test_entries(&mut store, 1, 3);
        store
            .create_checkpoint(1, "manual")
            .expect("create rollback checkpoint");
        append_test_entries(&mut store, 4, 7);
        stage_durable_rollback_intent(&store, 1);
        fs::write(store.rollback_index_stage_path(), b"corrupt staged index\n")
            .expect("corrupt staged rollback index");
        drop(store);

        SessionStoreV2::create(&root, 512)
            .expect_err("corrupt durable rollback stage must fail closed");
        let inspection =
            SessionStoreV2::open_for_inspection(&root, 512).expect("open failed store");
        let fatal_events = inspection
            .read_migration_events()
            .expect("read fatal rollback evidence");
        assert!(fatal_events.iter().any(|event| {
            event.phase == "rollback"
                && event.outcome == "fatal_error"
                && event.error_class.as_deref() == Some("index_corruption")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn linked_artifact_paths_are_rejected_without_touching_targets() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let outside_root = tmp.path().join("outside");
        fs::create_dir(&outside_root).expect("create outside directory");
        let linked_root = tmp.path().join("linked-store");
        symlink(&outside_root, &linked_root).expect("link store root");
        SessionStoreV2::create(&linked_root, 4096).expect_err("linked store root must be rejected");
        assert!(
            fs::read_dir(&outside_root)
                .expect("read outside directory")
                .next()
                .is_none(),
            "root-link rejection wrote through the link"
        );

        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 4096).expect("create safe store");
        let outside_file = tmp.path().join("outside-sentinel");
        fs::write(&outside_file, b"sentinel").expect("write outside sentinel");
        symlink(&outside_file, store.segment_file_path(1)).expect("link segment artifact");
        store
            .append_entry("entry-1", None, "message", json!({"n": 1}))
            .expect_err("linked segment must be rejected");
        assert_eq!(fs::read(&outside_file).expect("read sentinel"), b"sentinel");

        let checkpoint_root = tmp.path().join("checkpoint-store");
        let mut checkpoint_store =
            SessionStoreV2::create(&checkpoint_root, 4096).expect("create checkpoint store");
        append_test_entries(&mut checkpoint_store, 1, 1);
        let checkpoint_tmp = checkpoint_root.join("tmp/0000000000000001.json.tmp");
        symlink(&outside_file, &checkpoint_tmp).expect("link checkpoint temp artifact");
        checkpoint_store
            .create_checkpoint(1, "manual")
            .expect_err("linked checkpoint temp must be rejected");
        assert_eq!(fs::read(&outside_file).expect("read sentinel"), b"sentinel");
    }

    fn oversized_sparse_frame_fixture(store: &SessionStoreV2) -> (OffsetIndexEntry, PathBuf) {
        let byte_length = MAX_FRAME_READ_BYTES + 1;
        let segment_path = store.segment_file_path(1);
        let segment = open_regular_file_for_write(&segment_path, true, ArtifactWriteMode::Replace)
            .expect("create private sparse segment");
        segment
            .set_len(byte_length)
            .expect("extend sparse segment without allocating frame bytes");
        let row = OffsetIndexEntry {
            schema: Cow::Borrowed(OFFSET_INDEX_SCHEMA),
            entry_seq: 1,
            entry_id: "oversized".to_string(),
            segment_seq: 1,
            frame_seq: 1,
            byte_offset: 0,
            byte_length,
            crc32c: "00000000".to_string(),
            state: Cow::Borrowed("active"),
        };
        (row, segment_path)
    }

    #[test]
    fn segment_reader_rejects_sparse_frame_above_absolute_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = SessionStoreV2::create(
            tmp.path().join("store"),
            MAX_FRAME_READ_BYTES.saturating_mul(2),
        )
        .expect("create store with a caller-selected segment limit above the read cap");
        let (row, _segment_path) = oversized_sparse_frame_fixture(&store);

        let error = SegmentFileReader::new(&store)
            .read_frame(&row)
            .expect_err("oversized sparse frame must be rejected before allocation");
        assert!(error.to_string().contains("exceeds absolute read limit"));
    }

    #[test]
    fn integrity_validation_rejects_sparse_frame_above_absolute_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = SessionStoreV2::create(
            tmp.path().join("store"),
            MAX_FRAME_READ_BYTES.saturating_mul(2),
        )
        .expect("create store with a caller-selected segment limit above the read cap");
        let (row, _segment_path) = oversized_sparse_frame_fixture(&store);
        write_jsonl_lines(&store.index_file_path(), &[row]).expect("write oversized index row");

        let error = store
            .validate_integrity()
            .expect_err("integrity validation must reject before allocating frame bytes");
        assert!(error.to_string().contains("exceeds absolute read limit"));
    }

    #[test]
    fn frame_write_limits_accept_exact_caps_and_reject_cap_plus_one() {
        validate_encoded_frame_length(MAX_FRAME_READ_BYTES, MAX_FRAME_READ_BYTES)
            .expect("exact absolute and segment cap is readable");
        let absolute_error = validate_encoded_frame_length(MAX_FRAME_READ_BYTES + 1, u64::MAX)
            .expect_err("absolute cap plus one must fail");
        assert!(
            absolute_error
                .to_string()
                .contains("exceeds absolute read limit")
        );

        validate_encoded_frame_length(4096, 4096).expect("exact configured cap is readable");
        let configured_error = validate_encoded_frame_length(4097, 4096)
            .expect_err("configured cap plus one must fail");
        assert!(
            configured_error
                .to_string()
                .contains("exceeds configured segment limit")
        );
    }

    #[test]
    fn append_rejects_unreadable_frame_before_segment_mutation() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 32).expect("create tiny-segment store");

        let error = store
            .append_entry("entry-1", None, "message", json!({"content": "too large"}))
            .expect_err("encoded frame larger than segment cap must fail");
        assert!(
            error
                .to_string()
                .contains("exceeds configured segment limit")
        );
        assert_eq!(
            fs::metadata(store.segment_file_path(1)).map_or(0, |metadata| metadata.len()),
            0,
            "rejected frame must not mutate a segment"
        );
        assert!(!store.index_file_path().exists());
    }

    #[test]
    fn quarantine_segment_file_moves_segment_to_backup() {
        let tmp = tempfile::tempdir().expect("tempdir");
        drop(open_private_directory(tmp.path(), true).expect("privatize tempdir"));
        let segment = tmp.path().join("0000000000000002.seg");
        let mut file = open_regular_file_for_write(&segment, true, ArtifactWriteMode::Replace)
            .expect("open segment");
        file.write_all(b"hello").expect("write segment");
        drop(file);

        let backup = quarantine_segment_file(&segment).expect("quarantine segment");

        assert_eq!(backup, tmp.path().join("0000000000000002.seg.bak"));
        assert!(!segment.exists(), "original segment should be moved away");
        assert_eq!(fs::read(&backup).expect("read backup"), b"hello");
    }

    #[test]
    fn quarantine_segment_file_uses_next_available_backup_suffix() {
        let tmp = tempfile::tempdir().expect("tempdir");
        drop(open_private_directory(tmp.path(), true).expect("privatize tempdir"));
        let segment = tmp.path().join("0000000000000002.seg");
        let existing_backup = tmp.path().join("0000000000000002.seg.bak");
        let mut segment_file =
            open_regular_file_for_write(&segment, true, ArtifactWriteMode::Replace)
                .expect("open segment");
        segment_file.write_all(b"new").expect("write segment");
        drop(segment_file);
        let mut backup_file =
            open_regular_file_for_write(&existing_backup, true, ArtifactWriteMode::Replace)
                .expect("open existing backup");
        backup_file
            .write_all(b"old")
            .expect("write existing backup");
        drop(backup_file);

        let backup = quarantine_segment_file(&segment).expect("quarantine segment");

        assert_eq!(backup, tmp.path().join("0000000000000002.seg.bak.1"));
        assert_eq!(
            fs::read(&existing_backup).expect("read existing backup"),
            b"old"
        );
        assert_eq!(fs::read(&backup).expect("read new backup"), b"new");
    }

    #[test]
    fn create_recovers_from_index_row_that_references_missing_segment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 4096).expect("create store");
        store
            .append_entry("entry-1", None, "message", json!({"n": 1}))
            .expect("append entry");

        let mut rows = store.read_index().expect("read index");
        assert_eq!(rows.len(), 1);
        rows[0].segment_seq = 999;
        write_jsonl_lines(&store.index_file_path(), &rows).expect("write corrupted index");
        drop(store);

        let reopened = SessionStoreV2::create(&root, 4096).expect("reopen store");
        assert_eq!(reopened.entry_count(), 1);

        let rebuilt_rows = reopened.read_index().expect("read rebuilt index");
        assert_eq!(rebuilt_rows.len(), 1);
        assert_eq!(rebuilt_rows[0].segment_seq, 1);
        assert!(reopened.lookup_entry(1).expect("lookup entry").is_some());
    }

    #[test]
    fn create_drops_frame_with_mismatched_embedded_segment_seq_during_rebuild() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("store");
        let mut store = SessionStoreV2::create(&root, 4096).expect("create store");
        store
            .append_entry("entry-1", None, "message", json!({"n": 1}))
            .expect("append first entry");
        store
            .append_entry(
                "entry-2",
                Some("entry-1".to_string()),
                "message",
                json!({"n": 2}),
            )
            .expect("append second entry");

        let segment_path = store.segment_file_path(1);
        let mut frames = store.read_segment(1).expect("read segment");
        assert_eq!(frames.len(), 2);
        frames[1].segment_seq = 77;
        write_jsonl_lines(&segment_path, &frames).expect("write corrupted segment");
        remove_regular_file(&store.index_file_path()).expect("remove index");
        drop(store);

        let reopened = SessionStoreV2::create(&root, 4096).expect("reopen store");
        assert_eq!(reopened.entry_count(), 1);

        let rebuilt_rows = reopened.read_index().expect("read rebuilt index");
        assert_eq!(rebuilt_rows.len(), 1);
        assert_eq!(rebuilt_rows[0].entry_seq, 1);
        assert_eq!(reopened.read_segment(1).expect("read segment").len(), 1);
        assert!(reopened.lookup_entry(2).expect("lookup entry").is_none());
    }

    // ====================================================================
    // chain_hash_step
    // ====================================================================

    proptest! {
        #[test]
        fn chain_hash_output_is_64_hex(
            a in "[0-9a-f]{64}",
            b in "[0-9a-f]{64}",
        ) {
            let result = chain_hash_step(&a, &b);
            assert_eq!(result.len(), 64);
            assert!(result.chars().all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn chain_hash_deterministic(
            a in "[0-9a-f]{64}",
            b in "[0-9a-f]{64}",
        ) {
            assert_eq!(chain_hash_step(&a, &b), chain_hash_step(&a, &b));
        }

        #[test]
        fn chain_hash_non_commutative(
            a in "[0-9a-f]{64}",
            b in "[0-9a-f]{64}",
        ) {
            if a != b {
                assert_ne!(chain_hash_step(&a, &b), chain_hash_step(&b, &a));
            }
        }

        #[test]
        fn chain_hash_genesis_differs_from_step(payload in "[0-9a-f]{64}") {
            let step1 = chain_hash_step(GENESIS_CHAIN_HASH, &payload);
            assert_ne!(step1, GENESIS_CHAIN_HASH);
        }
    }

    // ====================================================================
    // crc32c_upper
    // ====================================================================

    proptest! {
        #[test]
        fn crc32c_output_is_8_uppercase_hex(data in prop::collection::vec(any::<u8>(), 0..500)) {
            let result = crc32c_upper(&data);
            assert_eq!(result.len(), 8);
            assert!(result.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F')));
        }

        #[test]
        fn crc32c_deterministic(data in prop::collection::vec(any::<u8>(), 0..500)) {
            assert_eq!(crc32c_upper(&data), crc32c_upper(&data));
        }

        #[test]
        fn crc32c_single_bit_sensitivity(byte in any::<u8>()) {
            let a = crc32c_upper(&[byte]);
            let b = crc32c_upper(&[byte ^ 1]);
            if byte != byte ^ 1 {
                assert_ne!(a, b, "flipping LSB should change CRC");
            }
        }
    }

    // ====================================================================
    // payload_hash_and_size
    // ====================================================================

    proptest! {
        #[test]
        fn payload_hash_is_64_hex(s in "[a-z]{0,50}") {
            let val = json!(s);
            let raw_string = serde_json::to_string(&val).unwrap();
            let raw = RawValue::from_string(raw_string).unwrap();
            let (hash, _size) = payload_hash_and_size(&raw).unwrap();
            assert_eq!(hash.len(), 64);
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn payload_size_matches_serialization(s in "[a-z]{0,50}") {
            let val = json!(s);
            let raw_string = serde_json::to_string(&val).unwrap();
            let raw = RawValue::from_string(raw_string).unwrap();
            let (_, size) = payload_hash_and_size(&raw).unwrap();
            let expected = serde_json::to_vec(&val).unwrap().len() as u64;
            assert_eq!(size, expected);
        }

        #[test]
        fn payload_hash_deterministic(n in 0i64..10000) {
            let val = json!(n);
            let raw_string = serde_json::to_string(&val).unwrap();
            let raw = RawValue::from_string(raw_string).unwrap();
            let (h1, s1) = payload_hash_and_size(&raw).unwrap();
            let (h2, s2) = payload_hash_and_size(&raw).unwrap();
            assert_eq!(h1, h2);
            assert_eq!(s1, s2);
        }
    }

    // ====================================================================
    // line_length_u64
    // ====================================================================

    proptest! {
        #[test]
        fn line_length_is_len_plus_one(data in prop::collection::vec(any::<u8>(), 0..1000)) {
            let result = line_length_u64(&data).unwrap();
            assert_eq!(result, data.len() as u64 + 1);
        }

        #[test]
        fn line_length_never_zero(data in prop::collection::vec(any::<u8>(), 0..100)) {
            let result = line_length_u64(&data).unwrap();
            assert!(result >= 1);
        }
    }

    // ====================================================================
    // v2_sidecar_path
    // ====================================================================

    proptest! {
        #[test]
        fn sidecar_path_ends_with_v2(stem in "[a-z]{1,10}") {
            let input = PathBuf::from(format!("/tmp/{stem}.jsonl"));
            let result = v2_sidecar_path(&input);
            let name = result.file_name().unwrap().to_str().unwrap();
            assert_eq!(
                Path::new(name).extension().and_then(|ext| ext.to_str()),
                Some("v2"),
                "expected .v2 suffix, got {name}"
            );
        }

        #[test]
        fn sidecar_path_preserves_parent(stem in "[a-z]{1,10}", dir in "[a-z]{1,8}") {
            let input = PathBuf::from(format!("/tmp/{dir}/{stem}.jsonl"));
            let result = v2_sidecar_path(&input);
            assert_eq!(
                result.parent().unwrap(),
                Path::new(&format!("/tmp/{dir}"))
            );
        }

        #[test]
        fn sidecar_path_deterministic(stem in "[a-z]{1,10}") {
            let input = PathBuf::from(format!("/sessions/{stem}.jsonl"));
            assert_eq!(v2_sidecar_path(&input), v2_sidecar_path(&input));
        }

        #[test]
        fn sidecar_path_contains_stem(stem in "[a-z]{1,10}") {
            let input = PathBuf::from(format!("/tmp/{stem}.jsonl"));
            let result = v2_sidecar_path(&input);
            let name = result.file_name().unwrap().to_str().unwrap();
            assert_eq!(name, format!("{stem}.v2"));
        }
    }
}
