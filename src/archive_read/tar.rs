//! Streaming TAR views over an admitted descriptor. Never extract or follow
//! members. Validate the entire stream before returning even an early member:
//! a duplicate later name, a corrupt gzip footer or truncated tail is an error.
//!
//! Header layout/checksum: GNU tar manual, Basic Tar Format. Supported here:
//! V7, ustar, PAX and GNU long-name headers, optionally gzip-compressed. Extended
//! metadata can name or size a member, never authorize links or sparse payloads.

use std::io::{self, Read, Seek};

use super::{
    ArchiveEntry, ArchiveReadOutcome, BTreeMap, BoundedReader, Cell, MAX_ARCHIVE_BYTES,
    MAX_ENTRIES, MAX_MEMBER_BYTES, MAX_MEMBER_NAME_BYTES, MAX_METADATA_READ_BYTES,
    MAX_READ_OPERATIONS, MAX_TOTAL_MEMBER_BYTES, Rc, ReadBudget, archive_error,
    normalize_member, render_directory,
};
use crate::error::Result;

const BLOCK: usize = 512;
const MAX_EXTENSION_BYTES: u64 = 64 * 1024;
const MAX_PAX_RECORDS: usize = 256;

pub(super) fn recognizes(bytes: &[u8]) -> bool {
    if bytes.len() != BLOCK {
        return false;
    }
    bytes.iter().all(|byte| *byte == 0)
        || &bytes[257..263] == b"ustar\0"
        || &bytes[257..263] == b"ustar "
        || (bytes[257..].iter().all(|byte| *byte == 0) && checksum(bytes).is_ok())
}

pub(super) fn read<R: Read + Seek>(
    reader: R,
    member: &str,
    gzip: bool,
) -> Result<ArchiveReadOutcome> {
    let budget = Rc::new(Cell::new(ReadBudget {
        bytes: MAX_ARCHIVE_BYTES,
        operations: MAX_READ_OPERATIONS,
    }));
    let reader = BoundedReader::new(reader, budget)?;
    if gzip {
        // Decode every gzip member and reach its footer. A decoder which stops
        // after the first member could hide appended data or a corrupt trailer.
        scan(flate2::read::MultiGzDecoder::new(reader), member)
    } else {
        scan(reader, member)
    }
}

/// Separate expanded-stream and operation limits also bound padding, headers
/// and compressed bombs. No entire expanded archive is retained in memory.
struct Stream<R> {
    inner: R,
    bytes: u64,
    operations: u64,
}

impl<R: Read> Read for Stream<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.operations == 0 {
            return Err(io::Error::other("TAR read operation budget exceeded"));
        }
        self.operations -= 1;
        // One-byte probe distinguishes exact EOF from excess expanded data.
        let remaining = MAX_ARCHIVE_BYTES.saturating_sub(self.bytes) + 1;
        let allowed = buffer.len().min(usize::try_from(remaining).unwrap_or(usize::MAX));
        let count = self.inner.read(&mut buffer[..allowed])?;
        self.bytes += u64::try_from(count).unwrap_or(u64::MAX);
        if self.bytes > MAX_ARCHIVE_BYTES {
            return Err(io::Error::other("TAR exceeds the 100 MiB expanded-stream limit"));
        }
        Ok(count)
    }
}

fn block(reader: &mut impl Read) -> Result<Option<[u8; BLOCK]>> {
    let mut header = [0; BLOCK];
    let mut offset = 0;
    while offset < BLOCK {
        match reader.read(&mut header[offset..]) {
            Ok(0) if offset == 0 => return Ok(None),
            Ok(0) => return Err(archive_error("Truncated TAR block")),
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(archive_error(format!("Cannot decode TAR: {error}"))),
        }
    }
    Ok(Some(header))
}

fn number(bytes: &[u8]) -> Result<u64> {
    if bytes.first().is_some_and(|byte| byte & 0x80 != 0) {
        // GNU base-256 positive sizes. Reject negative and overflowing values.
        if bytes[0] & 0x40 != 0 {
            return Err(archive_error("Negative TAR numeric field"));
        }
        return bytes.iter().enumerate().try_fold(0_u64, |value, (index, byte)| {
            let byte = if index == 0 { byte & 0x7f } else { *byte };
            value.checked_mul(256).and_then(|value| value.checked_add(u64::from(byte)))
                .ok_or_else(|| archive_error("TAR numeric field overflow"))
        });
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| archive_error("Invalid TAR numeric field"))?
        .trim_matches(['\0', ' ']);
    if text.is_empty() {
        return Ok(0);
    }
    if !text.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return Err(archive_error("Invalid TAR octal field"));
    }
    u64::from_str_radix(text, 8).map_err(|_| archive_error("TAR numeric field overflow"))
}

fn checksum(header: &[u8]) -> Result<()> {
    let actual: u64 = header.iter().enumerate().map(|(index, byte)| {
        if (148..156).contains(&index) { 32 } else { u64::from(*byte) }
    }).sum();
    if number(&header[148..156])? != actual {
        return Err(archive_error("Invalid TAR header checksum"));
    }
    Ok(())
}

fn field(bytes: &[u8]) -> Result<&str> {
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    if bytes[end..].iter().any(|byte| *byte != 0) {
        return Err(archive_error("Ambiguous NUL-padded TAR name"));
    }
    std::str::from_utf8(&bytes[..end]).map_err(|_| archive_error("TAR name is not UTF-8"))
}

fn validate_header(header: &[u8; BLOCK]) -> Result<()> {
    checksum(header)?;
    let magic = &header[257..265];
    if (&magic[..6] == b"ustar\0" && &magic[6..] == b"00") || magic == b"ustar  \0" {
        return Ok(());
    }
    if header[257..].iter().any(|byte| *byte != 0) {
        return Err(archive_error("Unsupported TAR header format"));
    }
    Ok(())
}

fn header_name(header: &[u8; BLOCK]) -> Result<String> {
    let name = field(&header[..100])?;
    if &header[257..263] == b"ustar\0" {
        let prefix = field(&header[345..500])?;
        return Ok(if prefix.is_empty() { name.to_owned() } else { format!("{prefix}/{name}") });
    }
    // GNU uses the POSIX prefix area for other metadata, not a name prefix.
    Ok(name.to_owned())
}

fn payload(reader: &mut impl Read, size: u64, retain: bool) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    if retain {
        output.try_reserve_exact(usize::try_from(size)
            .map_err(|_| archive_error("TAR member size is not representable"))?)
            .map_err(|_| archive_error("Cannot allocate TAR member buffer"))?;
    }
    let mut remaining = size;
    let mut buffer = [0; 32 * 1024];
    while remaining != 0 {
        let count = buffer.len().min(usize::try_from(remaining).unwrap_or(usize::MAX));
        reader.read_exact(&mut buffer[..count])
            .map_err(|error| archive_error(format!("Truncated or unreadable TAR member: {error}")))?;
        if retain {
            output.extend_from_slice(&buffer[..count]);
        }
        remaining -= u64::try_from(count).unwrap_or(u64::MAX);
    }
    let padding = (512 - size % 512) % 512;
    reader.read_exact(&mut buffer[..usize::try_from(padding).unwrap_or(0)])
        .map_err(|error| archive_error(format!("Truncated TAR member padding: {error}")))?;
    Ok(output)
}

/// None inherits; Some(None) explicitly removes an inherited field. In a
/// local PAX header, removal means use this member's ordinary TAR field.
#[derive(Default)]
struct Pax {
    path: Option<Option<String>>,
    size: Option<Option<u64>>,
}

impl Pax {
    fn update(&mut self, other: Self) {
        if other.path.is_some() { self.path = other.path; }
        if other.size.is_some() { self.size = other.size; }
    }
}

fn decimal(bytes: &[u8]) -> Result<u64> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(archive_error("Invalid PAX decimal field"));
    }
    bytes.iter().try_fold(0_u64, |value, byte| {
        value.checked_mul(10).and_then(|value| value.checked_add(u64::from(byte - b'0')))
            .ok_or_else(|| archive_error("PAX decimal overflow"))
    })
}

fn pax(mut bytes: &[u8]) -> Result<Pax> {
    let mut fields = Pax::default();
    let mut seen = std::collections::BTreeSet::new();
    while !bytes.is_empty() {
        let space = bytes.iter().position(|byte| *byte == b' ')
            .ok_or_else(|| archive_error("Invalid PAX record length"))?;
        let length = usize::try_from(decimal(&bytes[..space])?)
            .map_err(|_| archive_error("PAX length overflow"))?;
        let record = bytes.get(..length).filter(|record| {
            record.len() > space + 3 && record.last() == Some(&b'\n')
        }).ok_or_else(|| archive_error("Truncated or invalid PAX record"))?;
        let body = &record[space + 1..length - 1];
        let equal = body.iter().position(|byte| *byte == b'=')
            .ok_or_else(|| archive_error("Missing PAX key separator"))?;
        let key = std::str::from_utf8(&body[..equal])
            .map_err(|_| archive_error("Invalid PAX key"))?;
        if key.is_empty() || key.len() > 256 || !seen.insert(key) || seen.len() > MAX_PAX_RECORDS {
            return Err(archive_error("Duplicate, oversized or excessive PAX keys"));
        }
        let value = &body[equal + 1..];
        match key {
            "path" => {
                fields.path = Some(if value.is_empty() { None } else {
                    let path = std::str::from_utf8(value)
                        .map_err(|_| archive_error("PAX path is not UTF-8"))?;
                    normalize_member(path, true)?;
                    Some(path.to_owned())
                });
            }
            "size" => fields.size = Some(if value.is_empty() { None } else { Some(decimal(value)?) }),
            "hdrcharset" if matches!(value, b"" | b"UTF-8" | b"ISO-IR 10646 2000 UTF-8") => {}
            // Inert ownership/timestamp/comment/xattr metadata is not displayed,
            // restored or applied to the filesystem. Unknown framing, sparse
            // and link extensions are refused, not treated as dense file data.
            "mtime" | "atime" | "ctime" | "uid" | "gid" | "uname" | "gname" | "comment" | "charset" => {}
            key if key.starts_with("SCHILY.xattr.") || key.starts_with("LIBARCHIVE.xattr.") => {}
            _ => return Err(archive_error("Unsupported PAX attribute or payload encoding")),
        }
        bytes = &bytes[length..];
    }
    Ok(fields)
}

fn scan(reader: impl Read, member: &str) -> Result<ArchiveReadOutcome> {
    let member = normalize_member(member, true)?;
    let mut reader = Stream { inner: reader, bytes: 0, operations: MAX_READ_OPERATIONS };
    let mut entries = BTreeMap::new();
    let mut selected = None;
    let mut total = 0_u64;
    let mut count = 0;
    let mut root_seen = false;
    let mut global = Pax::default();
    let mut local: Option<Pax> = None;
    let mut long_name: Option<String> = None;
    let mut metadata_bytes = 0_u64;
    loop {
        let header = block(&mut reader)?.ok_or_else(|| archive_error("Missing TAR end marker"))?;
        if header.iter().all(|byte| *byte == 0) {
            if local.is_some() || long_name.is_some() {
                return Err(archive_error("TAR extension has no following member"));
            }
            let end = block(&mut reader)?.ok_or_else(|| archive_error("Missing second TAR end block"))?;
            if end.iter().any(|byte| *byte != 0) {
                return Err(archive_error("Invalid TAR end marker"));
            }
            // Reaching decoder EOF verifies gzip checksums as well as rejecting
            // hidden appended archives, partial blocks and nonzero trailing data.
            while let Some(trailing) = block(&mut reader)? {
                if trailing.iter().any(|byte| *byte != 0) {
                    return Err(archive_error("Nonzero data after TAR end marker"));
                }
            }
            break;
        }
        count += 1;
        if count > MAX_ENTRIES {
            return Err(archive_error("TAR exceeds the 4096-header limit"));
        }
        validate_header(&header)?;
        let kind = header[156];
        let raw_size = number(&header[124..136])?;
        if matches!(kind, b'x' | b'g' | b'L') {
            normalize_member(&header_name(&header)?, false)?;
            metadata_bytes = metadata_bytes.checked_add(raw_size)
                .filter(|total| *total <= MAX_METADATA_READ_BYTES)
                .ok_or_else(|| archive_error("TAR extended metadata exceeds 8 MiB"))?;
            if raw_size > MAX_EXTENSION_BYTES {
                return Err(archive_error("TAR extension exceeds 64 KiB"));
            }
            if local.is_some() || long_name.is_some() {
                return Err(archive_error("Ambiguous consecutive TAR member extensions"));
            }
            if kind == b'L' && raw_size > MAX_MEMBER_NAME_BYTES as u64 + 1 {
                return Err(archive_error("GNU TAR long name exceeds the path limit"));
            }
            let data = payload(&mut reader, raw_size, true)?;
            match kind {
                b'x' => local = Some(pax(&data)?),
                b'g' => global.update(pax(&data)?),
                _ => {
                    let name = data.strip_suffix(&[0])
                        .ok_or_else(|| archive_error("GNU TAR long name lacks a NUL terminator"))?;
                    let name = std::str::from_utf8(name)
                        .map_err(|_| archive_error("GNU TAR long name is not UTF-8"))?;
                    normalize_member(name, false)?;
                    long_name = Some(name.to_owned());
                }
            }
            continue;
        }
        if !matches!(kind, 0 | b'0' | b'5') {
            return Err(archive_error("Unsupported TAR member type (links, special files or extensions)"));
        }
        let path_override = local.as_ref().and_then(|fields| fields.path.as_ref())
            .or(global.path.as_ref()).and_then(|path| path.as_deref());
        let raw_name = match long_name.take().or_else(|| path_override.map(str::to_owned)) {
            Some(name) => name,
            None => header_name(&header)?,
        };
        let directory = kind == b'5' || (kind == 0 && raw_name.ends_with('/'));
        let name = normalize_member(&raw_name, directory)?;
        let size = local.as_ref().and_then(|fields| fields.size)
            .or(global.size).flatten().unwrap_or(raw_size);
        local = None;
        if directory && size != 0 {
            return Err(archive_error("TAR directory has a nonzero size"));
        }
        total = total.checked_add(size).filter(|total| *total <= MAX_TOTAL_MEMBER_BYTES)
            .ok_or_else(|| archive_error("TAR exceeds the 64 MiB total member-size limit"))?;
        if name.is_empty() {
            if root_seen {
                return Err(archive_error("Duplicate TAR root directory"));
            }
            root_seen = true;
            continue;
        }
        let retain = !directory && name == member;
        if retain && size > MAX_MEMBER_BYTES {
            return Err(archive_error("TAR member exceeds the 16 MiB read limit"));
        }
        let entry = ArchiveEntry { index: count - 1, size, compressed_size: size, is_directory: directory };
        if entries.insert(name.clone(), entry).is_some() {
            return Err(archive_error(format!("Ambiguous duplicate archive member: {name}")));
        }
        let bytes = payload(&mut reader, size, retain)?;
        if retain {
            selected = Some(bytes);
        }
    }
    for name in entries.keys() {
        for (separator, _) in name.match_indices('/') {
            if entries.get(&name[..separator]).is_some_and(|entry| !entry.is_directory) {
                return Err(archive_error(format!("Archive file/directory conflict: {name}")));
            }
        }
    }
    let prefix = if member.is_empty() { String::new() } else { format!("{member}/") };
    let is_directory = member.is_empty()
        || entries.get(&member).is_some_and(|entry| entry.is_directory)
        || entries.keys().any(|name| name.starts_with(&prefix));
    let content = if is_directory {
        render_directory(&entries, &prefix)
    } else {
        let bytes = selected.ok_or_else(|| archive_error(format!("Archive member not found: {member}")))?;
        if bytes.contains(&0) {
            return Err(archive_error(format!("Binary archive member cannot be read as text: {member}")));
        }
        String::from_utf8(bytes).map_err(|_| archive_error(format!("Archive member is not UTF-8 text: {member}")))?
    };
    Ok(ArchiveReadOutcome { content, member, is_directory, entry_count: entries.len() })
}

#[cfg(test)]
mod tests;
