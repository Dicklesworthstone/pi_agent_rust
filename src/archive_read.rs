//! Bounded, read-only navigation of ZIP archives and ZIP-based packages.
//!
//! The caller admits and opens the outer file within the active workspace.
//! This module consumes that descriptor (or document bytes), never reopens a
//! pathname, follows archive links, extracts files, or launches a subprocess.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::rc::Rc;

use zip::{CompressionMethod, ZipArchive};

use crate::error::{Error, Result};

const MAX_ARCHIVE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_TOTAL_MEMBER_BYTES: u64 = 64 * 1024 * 1024;
const MAX_MEMBER_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const MAX_MEMBER_NAME_BYTES: usize = 4096;
const MAX_METADATA_READ_BYTES: u64 = 8 * 1024 * 1024;
const MAX_READ_OPERATIONS: u64 = 100_000;

/// The outer archive and an optional member selected with `archive.zip:member`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchivePath {
    pub archive_path: String,
    pub member: String,
}

/// Recognize OMP's archive path syntax without performing filesystem access.
/// URL routing and trailing line selectors belong to the caller.
pub(crate) fn parse_path(path: &str) -> Result<Option<ArchivePath>> {
    if path.contains("://") {
        return Ok(None);
    }
    for (separator, _) in path.match_indices(':') {
        let archive_path = &path[..separator];
        if is_archive_path(archive_path) {
            return Ok(Some(ArchivePath {
                archive_path: archive_path.to_owned(),
                member: normalize_member(&path[separator + 1..], true)?,
            }));
        }
    }
    Ok(is_archive_path(path).then(|| ArchivePath {
        archive_path: path.to_owned(),
        member: String::new(),
    }))
}

fn is_archive_path(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|extension| {
        ["zip", "jar", "war", "ear", "apk", "whl"]
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

fn archive_error(message: impl Into<String>) -> Error {
    Error::tool("read", format!("[ARCHIVE_READ] {}", message.into()))
}

fn normalize_member(path: &str, allow_empty: bool) -> Result<String> {
    if path.len() > MAX_MEMBER_NAME_BYTES
        || path.starts_with(['/', '\\'])
        || path.chars().any(|ch| ch.is_control() || ch == ':')
    {
        return Err(archive_error("Archive member path is unsafe or too long"));
    }
    let mut normalized = String::new();
    for component in path.split(['/', '\\']) {
        match component {
            "" | "." => continue,
            ".." => return Err(archive_error("Parent traversal in archive member path")),
            _ => {}
        }
        if !normalized.is_empty() {
            normalized.push('/');
        }
        normalized.push_str(component);
    }
    if normalized.is_empty() && !allow_empty {
        return Err(archive_error("Archive contains an empty member name"));
    }
    Ok(normalized)
}

#[derive(Clone, Copy)]
struct ReadBudget {
    bytes: u64,
    operations: u64,
}

/// Bound both parser work and archive offsets without implementing ZIP framing.
struct BoundedReader<R> {
    inner: R,
    size: u64,
    position: u64,
    budget: Rc<Cell<ReadBudget>>,
}

impl<R: Read + Seek> BoundedReader<R> {
    fn new(mut inner: R, budget: Rc<Cell<ReadBudget>>) -> Result<Self> {
        let size = inner.seek(SeekFrom::End(0))?;
        if size > MAX_ARCHIVE_BYTES {
            return Err(archive_error("Archive exceeds the 100 MiB input limit"));
        }
        inner.seek(SeekFrom::Start(0))?;
        Ok(Self {
            inner,
            size,
            position: 0,
            budget,
        })
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.position == self.size {
            return Ok(0);
        }
        let mut budget = self.budget.get();
        if budget.bytes == 0 || budget.operations == 0 {
            return Err(io::Error::other("Archive read budget exceeded"));
        }
        let allowed = budget.bytes.min(self.size - self.position);
        let count = buffer
            .len()
            .min(usize::try_from(allowed).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buffer[..count])?;
        let consumed = u64::try_from(read).unwrap_or(u64::MAX);
        self.position += consumed;
        budget.bytes -= consumed;
        budget.operations -= 1;
        self.budget.set(budget);
        Ok(read)
    }
}

impl<R: Seek> Seek for BoundedReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let mut budget = self.budget.get();
        if budget.operations == 0 {
            return Err(io::Error::other("Archive read budget exceeded"));
        }
        budget.operations -= 1;
        self.budget.set(budget);
        let target = match position {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(offset) => self.size.checked_add_signed(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
        }
        .filter(|target| *target <= self.size)
        .ok_or_else(|| io::Error::other("Archive seek is outside the admitted file"))?;
        self.inner.seek(SeekFrom::Start(target))?;
        self.position = target;
        Ok(target)
    }
}

#[derive(Debug)]
struct ArchiveEntry {
    index: usize,
    size: u64,
    compressed_size: u64,
    is_directory: bool,
}

/// Shared native ZIP boundary for archive navigation and Office documents.
pub(crate) struct ArchiveReader<R> {
    archive: ZipArchive<BoundedReader<R>>,
    entries: BTreeMap<String, ArchiveEntry>,
    budget: Rc<Cell<ReadBudget>>,
}

impl<R: Read + Seek> ArchiveReader<R> {
    /// Index and validate a ZIP using bounded metadata I/O before reading members.
    pub(crate) fn new(reader: R) -> Result<Self> {
        let budget = Rc::new(Cell::new(ReadBudget {
            bytes: MAX_METADATA_READ_BYTES,
            operations: MAX_READ_OPERATIONS,
        }));
        let reader = BoundedReader::new(reader, Rc::clone(&budget))?;
        let mut archive = ZipArchive::new(reader)
            .map_err(|error| archive_error(format!("Cannot open ZIP: {error}")))?;
        if archive.len() > MAX_ENTRIES {
            return Err(archive_error("Archive exceeds the 4096-entry limit"));
        }
        let entries = index_entries(&archive)?;
        // Also rejects identical raw names which the crate's lookup index
        // would otherwise collapse, contradictory headers, and shared ranges.
        archive
            .validate_header_consistency()
            .map_err(|error| archive_error(format!("Invalid ZIP headers: {error}")))?;
        Ok(Self {
            archive,
            entries,
            budget,
        })
    }

    /// Read a single regular member, checking its CRC and declared length.
    /// The output cap applies to actual decompression, not only ZIP metadata.
    pub(crate) fn read_member(&mut self, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let name = normalize_member(name, false)?;
        let entry = self
            .entries
            .get(&name)
            .ok_or_else(|| archive_error(format!("Archive member not found: {name}")))?;
        if entry.is_directory {
            return Err(archive_error(format!(
                "Archive member is a directory: {name}"
            )));
        }
        let max_bytes = max_bytes.min(MAX_MEMBER_BYTES);
        if entry.size > max_bytes {
            return Err(archive_error(format!(
                "Archive member {name} exceeds the {max_bytes}-byte read limit"
            )));
        }
        self.budget.set(ReadBudget {
            bytes: entry.compressed_size.saturating_add(128 * 1024),
            operations: MAX_READ_OPERATIONS,
        });
        let mut member = self
            .archive
            .by_index(entry.index)
            .map_err(|error| archive_error(format!("Cannot open {name}: {error}")))?;
        let mut bytes = Vec::new();
        member
            .by_ref()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| archive_error(format!("Cannot decode {name}: {error}")))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(archive_error(format!(
                "Decompressed member {name} exceeds its read limit"
            )));
        }
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != entry.size {
            return Err(archive_error(format!(
                "Archive member {name} has an inconsistent size"
            )));
        }
        Ok(bytes)
    }
}

fn index_entries<R: Read + Seek>(
    archive: &ZipArchive<BoundedReader<R>>,
) -> Result<BTreeMap<String, ArchiveEntry>> {
    let mut entries = BTreeMap::new();
    let mut total_size = 0_u64;
    for index in 0..archive.len() {
        let member = archive
            .by_index_data(index)
            .map_err(|error| archive_error(error.to_string()))?;
        let raw_name = member
            .name()
            .map_err(|error| archive_error(format!("Invalid archive member name: {error}")))?;
        let name = normalize_member(&raw_name, false)?;
        if member.encrypted() || member.is_symlink() {
            return Err(archive_error(format!(
                "Encrypted or symbolic-link member is unsupported: {name}"
            )));
        }
        if !matches!(
            member.compression(),
            CompressionMethod::Stored | CompressionMethod::Deflated
        ) {
            return Err(archive_error(format!(
                "Unsupported ZIP compression for {name}"
            )));
        }
        if member.unix_mode().is_some_and(|mode| {
            let kind = mode & 0o170_000;
            !matches!(kind, 0 | 0o040_000 | 0o100_000)
        }) {
            return Err(archive_error(format!(
                "Special archive member is unsupported: {name}"
            )));
        }
        total_size = total_size
            .checked_add(member.size())
            .filter(|size| *size <= MAX_TOTAL_MEMBER_BYTES)
            .ok_or_else(|| archive_error("Archive exceeds the 64 MiB total expanded-size limit"))?;
        let entry = ArchiveEntry {
            index,
            size: member.size(),
            compressed_size: member.compressed_size(),
            is_directory: member.is_dir(),
        };
        if entries.insert(name.clone(), entry).is_some() {
            return Err(archive_error(format!(
                "Ambiguous duplicate archive member: {name}"
            )));
        }
    }
    for name in entries.keys() {
        for (separator, _) in name.match_indices('/') {
            if entries
                .get(&name[..separator])
                .is_some_and(|entry| !entry.is_directory)
            {
                return Err(archive_error(format!(
                    "Archive file/directory conflict: {name}"
                )));
            }
        }
    }
    Ok(entries)
}

/// Rendered archive data for the read tool's existing pagination layer.
#[derive(Debug)]
pub(crate) struct ArchiveReadOutcome {
    pub content: String,
    pub member: String,
    pub is_directory: bool,
    pub entry_count: usize,
}

pub(crate) fn read_open_file(file: File, member: &str) -> Result<ArchiveReadOutcome> {
    read_archive(ArchiveReader::new(file)?, member)
}

fn read_archive<R: Read + Seek>(
    mut reader: ArchiveReader<R>,
    member: &str,
) -> Result<ArchiveReadOutcome> {
    let member = normalize_member(member, true)?;
    let prefix = if member.is_empty() {
        String::new()
    } else {
        format!("{member}/")
    };
    let is_directory = member.is_empty()
        || reader
            .entries
            .get(&member)
            .is_some_and(|entry| entry.is_directory)
        || reader.entries.keys().any(|name| name.starts_with(&prefix));
    let content = if is_directory {
        render_directory(&reader.entries, &prefix)
    } else {
        let bytes = reader.read_member(&member, MAX_MEMBER_BYTES)?;
        if bytes.contains(&0) {
            return Err(archive_error(format!(
                "Binary archive member cannot be read as text: {member}"
            )));
        }
        String::from_utf8(bytes)
            .map_err(|_| archive_error(format!("Archive member is not UTF-8 text: {member}")))?
    };
    Ok(ArchiveReadOutcome {
        content,
        member,
        is_directory,
        entry_count: reader.entries.len(),
    })
}

fn render_directory(entries: &BTreeMap<String, ArchiveEntry>, prefix: &str) -> String {
    let mut children = BTreeMap::new();
    for (name, entry) in entries {
        let Some(relative) = name
            .strip_prefix(prefix)
            .filter(|relative| !relative.is_empty())
        else {
            continue;
        };
        let (child, is_directory) = relative
            .split_once('/')
            .map_or((relative, entry.is_directory), |(child, _)| (child, true));
        children.entry(child).or_insert((is_directory, entry.size));
    }
    let mut output = String::new();
    for (name, (is_directory, size)) in &children {
        if *is_directory {
            let _ = writeln!(output, "{name}/");
        } else {
            let _ = writeln!(output, "{name} ({size} bytes)");
        }
    }
    if output.is_empty() {
        output.push_str("[Empty archive directory]\n");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write as _};
    use zip::write::SimpleFileOptions;

    fn fixture(entries: &[(&str, &[u8])], method: CompressionMethod) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(method);
        for (name, content) in entries {
            writer
                .start_file(*name, options)
                .expect("start fixture member");
            writer.write_all(content).expect("write fixture member");
        }
        writer
            .finish()
            .expect("finish fixture archive")
            .into_inner()
    }

    fn open_error(bytes: Vec<u8>) -> String {
        ArchiveReader::new(Cursor::new(bytes))
            .err()
            .expect("archive must be refused")
            .to_string()
    }

    #[test]
    fn archive_paths_use_single_colon_and_preserve_the_outer_path() {
        for path in [
            "build.ZIP",
            "deps.jar",
            "package.whl",
            "app.apk",
            "app.war",
            "app.ear",
        ] {
            let parsed = parse_path(path).expect("valid path").expect("archive path");
            assert_eq!(parsed.archive_path, path);
            assert_eq!(parsed.member, "");
        }
        let parsed = parse_path(r"C:\work\release.zip:src\./main.rs")
            .expect("valid path")
            .expect("archive path");
        assert_eq!(parsed.archive_path, r"C:\work\release.zip");
        assert_eq!(parsed.member, "src/main.rs");
        for path in [
            "src/main.rs",
            "folder.zip/file.txt",
            "https://example.com/a.zip",
            "slides.pptx",
        ] {
            assert_eq!(parse_path(path).expect("ordinary path"), None);
        }
        for path in [
            "x.zip:../secret",
            "x.zip:/absolute",
            "x.zip:C:\\secret",
            "x.zip:a\0b",
        ] {
            assert!(parse_path(path).is_err(), "unsafe path {path:?}");
        }
    }

    #[test]
    fn stored_and_deflated_members_and_implied_directories_are_readable() {
        for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
            let bytes = fixture(
                &[
                    ("src/main.rs", "fn main() {}\n// café\n".as_bytes()),
                    ("README.md", b"hello\n"),
                ],
                method,
            );
            let listing = read_archive(ArchiveReader::new(Cursor::new(&bytes)).unwrap(), "")
                .expect("list archive root");
            assert!(listing.is_directory);
            assert_eq!(listing.content, "README.md (6 bytes)\nsrc/\n");
            assert_eq!(listing.entry_count, 2);
            let directory = read_archive(ArchiveReader::new(Cursor::new(&bytes)).unwrap(), "src")
                .expect("list implied directory");
            assert_eq!(directory.content, "main.rs (22 bytes)\n");
            let content = read_archive(
                ArchiveReader::new(Cursor::new(&bytes)).unwrap(),
                "src/main.rs",
            )
            .expect("read text member");
            assert!(!content.is_directory);
            assert_eq!(content.content, "fn main() {}\n// café\n");
            assert_eq!(content.member, "src/main.rs");
        }
    }

    #[test]
    fn utf8_and_member_size_are_enforced_without_losing_other_members() {
        let bytes = fixture(
            &[
                ("large.txt", b"123456"),
                ("binary.bin", b"\xff\0"),
                ("invalid.txt", b"\xff"),
                ("ok.txt", b"ok"),
            ],
            CompressionMethod::Deflated,
        );
        let mut reader = ArchiveReader::new(Cursor::new(&bytes)).unwrap();
        assert!(
            reader
                .read_member("large.txt", 5)
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        assert_eq!(reader.read_member("ok.txt", 2).unwrap(), b"ok");
        let error = read_archive(
            ArchiveReader::new(Cursor::new(&bytes)).unwrap(),
            "binary.bin",
        )
        .unwrap_err();
        assert!(error.to_string().contains("Binary archive member"));
        let error = read_archive(
            ArchiveReader::new(Cursor::new(&bytes)).unwrap(),
            "invalid.txt",
        )
        .unwrap_err();
        assert!(error.to_string().contains("not UTF-8"));
        assert!(
            reader
                .read_member("missing", 100)
                .unwrap_err()
                .to_string()
                .contains("not found")
        );
    }

    #[test]
    fn traversal_normalization_collisions_and_file_directory_conflicts_are_rejected() {
        for entries in [
            vec![("../escape", b"x".as_slice())],
            vec![("a.txt", b"x".as_slice()), ("./a.txt", b"y".as_slice())],
            vec![("a", b"x".as_slice()), ("a/b.txt", b"y".as_slice())],
        ] {
            let message = open_error(fixture(&entries, CompressionMethod::Stored));
            assert!(message.contains("[ARCHIVE_READ]"));
        }
    }

    #[test]
    fn duplicate_raw_names_and_crc_corruption_are_rejected() {
        let mut duplicate = fixture(
            &[("first.txt", b"one"), ("other.txt", b"two")],
            CompressionMethod::Stored,
        );
        // Corrupt both the local and central names, preserving header sizes.
        // The ZIP crate's index collapses these names; its consistency check
        // must reject the ambiguity rather than choosing a member silently.
        let offsets: Vec<usize> = duplicate
            .windows(9)
            .enumerate()
            .filter_map(|(offset, window)| (window == b"other.txt").then_some(offset))
            .collect();
        assert_eq!(offsets.len(), 2);
        for offset in offsets {
            duplicate[offset..offset + 9].copy_from_slice(b"first.txt");
        }
        assert!(open_error(duplicate).contains("duplicate file names"));

        let mut corrupt = fixture(
            &[("file.txt", b"checksum payload")],
            CompressionMethod::Stored,
        );
        let offset = corrupt
            .windows(16)
            .position(|window| window == b"checksum payload")
            .unwrap();
        corrupt[offset] = b'!';
        let mut reader = ArchiveReader::new(Cursor::new(corrupt)).unwrap();
        assert!(reader.read_member("file.txt", 100).is_err());
    }

    #[test]
    fn links_and_excessive_entry_counts_are_rejected() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink("link", "../../outside", SimpleFileOptions::default())
            .expect("write symlink fixture");
        assert!(open_error(writer.finish().unwrap().into_inner()).contains("symbolic-link"));

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..=MAX_ENTRIES {
            writer
                .start_file(format!("f{index}"), SimpleFileOptions::default())
                .expect("write empty fixture member");
        }
        assert!(open_error(writer.finish().unwrap().into_inner()).contains("4096-entry"));
    }

    #[test]
    fn forged_expanded_sizes_cannot_bypass_actual_decompression_limits() {
        let original = fixture(&[("file.txt", b"123456789")], CompressionMethod::Deflated);
        let archive = ZipArchive::new(Cursor::new(&original)).unwrap();
        let entry = archive.by_index_data(0).unwrap();
        let local = usize::try_from(entry.header_start()).unwrap();
        let central = usize::try_from(entry.central_header_start()).unwrap();

        let mut understated = original.clone();
        // ZIP local uncompressed size is at +22; central size is at +24.
        // Obtain the header positions from the maintained ZIP reader.
        understated[local + 22..local + 26].copy_from_slice(&1_u32.to_le_bytes());
        understated[central + 24..central + 28].copy_from_slice(&1_u32.to_le_bytes());
        let mut reader = ArchiveReader::new(Cursor::new(understated)).unwrap();
        assert!(reader.read_member("file.txt", 3).is_err());

        let mut oversized = original.clone();
        let declared = u32::try_from(MAX_TOTAL_MEMBER_BYTES + 1).unwrap();
        oversized[local + 22..local + 26].copy_from_slice(&declared.to_le_bytes());
        oversized[central + 24..central + 28].copy_from_slice(&declared.to_le_bytes());
        assert!(open_error(oversized).contains("64 MiB"));
    }

    #[test]
    fn parser_io_budget_and_seeks_are_bounded() {
        let budget = Rc::new(Cell::new(ReadBudget {
            bytes: 2,
            operations: 10,
        }));
        let mut reader = BoundedReader::new(Cursor::new(b"abcdef"), budget).unwrap();
        let mut buffer = [0; 6];
        assert_eq!(reader.read(&mut buffer).unwrap(), 2);
        assert!(reader.read(&mut buffer).is_err());
        assert!(reader.seek(SeekFrom::Start(7)).is_err());
        assert!(reader.seek(SeekFrom::Current(-3)).is_err());
    }

    #[test]
    fn read_creates_no_extracted_files_and_preserves_archive_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("release.zip");
        let bytes = fixture(
            &[("nested/member.txt", b"original")],
            CompressionMethod::Deflated,
        );
        std::fs::write(&path, &bytes).unwrap();
        let file = File::open(&path).unwrap();
        let result = read_open_file(file, "nested/member.txt").unwrap();
        assert_eq!(result.content, "original");
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_archive_path_after_open_does_not_change_the_admitted_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("release.zip");
        let bytes = fixture(&[("member.txt", b"original")], CompressionMethod::Stored);
        std::fs::write(&path, bytes).unwrap();
        let file = File::open(&path).unwrap();
        std::fs::rename(&path, directory.path().join("moved.zip")).unwrap();
        let replacement = fixture(&[("member.txt", b"replacement")], CompressionMethod::Stored);
        std::fs::write(&path, replacement).unwrap();
        let result = read_open_file(file, "member.txt").unwrap();
        assert_eq!(result.content, "original");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }
}
