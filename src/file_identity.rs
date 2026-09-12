//! Platform file identity for the TOCTOU checks in `jobs` and `mcp::trust`.
//!
//! Both modules follow the same shape: read an identity for a path, open it,
//! confirm the open handle is still that same file, do the work, confirm again.
//! On Unix the identity is `(st_dev, st_ino)` off a `stat`.
//!
//! Windows has the equivalent pair — the volume serial number and the file
//! index from `GetFileInformationByHandle` — but `std`'s accessors for them
//! (`std::os::windows::fs::MetadataExt::{volume_serial_number, file_index}`)
//! are still behind the unstable `windows_by_handle` feature, so a crate that
//! must also build on stable cannot call them. This module reads the same two
//! values through `winapi-util`, which wraps `GetFileInformationByHandle`
//! safely and is already in the dependency graph.
//!
//! The Windows numbers come from an open handle, never from a `Metadata`
//! obtained by path, so `of_path_nofollow` opens the path with
//! `FILE_FLAG_OPEN_REPARSE_POINT` (never traverse a final symlink or junction)
//! and `FILE_FLAG_BACKUP_SEMANTICS` (a directory can be opened at all). It asks
//! for no access rights, so it succeeds against files another process holds
//! open for exclusive read/write — `GetFileInformationByHandle` needs a handle,
//! not read permission.

use std::path::Path;

/// A file's identity on this platform: equal iff two references name the same
/// file, and stable across a rename of the path that reached it.
///
/// Compare identities only against identities read on the same machine and
/// within the same process lifetime — an inode number, like a Windows file
/// index, is reused after the file is deleted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    /// `st_dev` on Unix, the volume serial number on Windows.
    volume: u64,
    /// `st_ino` on Unix, the file index on Windows.
    index: u64,
}

impl FileIdentity {
    /// Identity of an already-open file, read from the handle itself.
    ///
    /// This is the side of a comparison that cannot be raced: the handle
    /// already refers to one specific file, whatever has happened to the path
    /// since it was opened.
    #[cfg(unix)]
    pub fn of_open_file(file: &std::fs::File) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = file.metadata()?;
        Ok(Self {
            volume: metadata.dev(),
            index: metadata.ino(),
        })
    }

    /// Identity of `path`, without following a final symlink.
    #[cfg(unix)]
    pub fn of_path_nofollow(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            volume: metadata.dev(),
            index: metadata.ino(),
        })
    }

    /// Identity of an already-open file, read from the handle itself.
    #[cfg(windows)]
    pub fn of_open_file(file: &std::fs::File) -> std::io::Result<Self> {
        let information = winapi_util::file::information(file)?;
        Ok(Self {
            volume: information.volume_serial_number(),
            index: information.file_index(),
        })
    }

    /// Identity of `path`, without following a final reparse point.
    #[cfg(windows)]
    pub fn of_path_nofollow(path: &Path) -> std::io::Result<Self> {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_SHARE_DELETE: u32 = 0x0000_0004;

        let handle = std::fs::OpenOptions::new()
            // No access rights: this only has to be a handle to the object.
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        Self::of_open_file(&handle)
    }

    /// Identity of an already-open file on a platform with neither interface.
    #[cfg(not(any(unix, windows)))]
    pub fn of_open_file(_file: &std::fs::File) -> std::io::Result<Self> {
        Err(unsupported())
    }

    /// Identity of a path on a platform with neither interface.
    #[cfg(not(any(unix, windows)))]
    pub fn of_path_nofollow(_path: &Path) -> std::io::Result<Self> {
        Err(unsupported())
    }
}

/// Fail closed where no identity interface exists: callers use this to refuse
/// a racy operation, so an error is the safe answer and a fabricated identity
/// that compares equal to itself is not.
#[cfg(not(any(unix, windows)))]
fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this platform exposes no stable file identity",
    )
}

#[cfg(test)]
mod tests {
    use super::FileIdentity;

    #[test]
    fn a_file_matches_itself_through_both_constructors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("artifact.log");
        std::fs::write(&path, b"payload").expect("write");

        let by_path = FileIdentity::of_path_nofollow(&path).expect("identity by path");
        let file = std::fs::File::open(&path).expect("open");
        let by_handle = FileIdentity::of_open_file(&file).expect("identity by handle");

        assert_eq!(
            by_path, by_handle,
            "the same file must have one identity whether reached by path or by handle"
        );
    }

    #[test]
    fn a_same_name_replacement_is_a_different_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("artifact.log");
        std::fs::write(&path, b"first").expect("write first");
        let opened = std::fs::File::open(&path).expect("open first");
        let first = FileIdentity::of_open_file(&opened).expect("identity of first");

        std::fs::remove_file(&path).expect("remove first");
        std::fs::write(&path, b"second").expect("write second");
        let second = FileIdentity::of_path_nofollow(&path).expect("identity of second");

        assert_ne!(
            first, second,
            "a replacement at the same path must not inherit the original's identity"
        );
        assert_eq!(
            first,
            FileIdentity::of_open_file(&opened).expect("re-identify the open handle"),
            "the open handle still names the file it was opened on"
        );
    }

    #[test]
    fn two_distinct_files_do_not_share_an_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let left = dir.path().join("left.log");
        let right = dir.path().join("right.log");
        std::fs::write(&left, b"left").expect("write left");
        std::fs::write(&right, b"right").expect("write right");

        assert_ne!(
            FileIdentity::of_path_nofollow(&left).expect("left identity"),
            FileIdentity::of_path_nofollow(&right).expect("right identity"),
        );
    }

    #[test]
    fn a_directory_has_an_identity_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).expect("create nested");

        assert_ne!(
            FileIdentity::of_path_nofollow(dir.path()).expect("parent identity"),
            FileIdentity::of_path_nofollow(&nested).expect("nested identity"),
            "a directory identity must distinguish a parent from its child"
        );
    }

    #[test]
    fn a_missing_path_reports_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = FileIdentity::of_path_nofollow(&dir.path().join("absent.log"))
            .expect_err("a missing path has no identity");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
