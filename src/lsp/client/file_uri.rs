//! Native file identities at the language-server boundary.
//!
//! URI conversion is lexical: it never opens a file or resolves a network
//! host. Decode once, preserve Unix filename bytes, and distinguish Windows
//! drive/UNC roots from a URI authority. Reject ambiguous syntax before a
//! general URL parser could silently normalize it into a different target.

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Convert an absolute native path into a file URI, without lossy Unicode.
///
/// The string API returns an empty (invalid) URI for an unrepresentable path;
/// dispatching code must use [`try_path_to_uri`] to report that error instead.
/// Neither API invents a relative path or a replacement-character filename.
#[must_use]
pub fn path_to_uri(path: &Path) -> String {
    try_path_to_uri(path).unwrap_or_default()
}

/// Checked native path conversion for document and workspace dispatch.
///
/// # Errors
/// Returns `LSP_FILE_URI` for relative paths, parent traversal, or filenames
/// that cannot be represented without changing their native identity.
pub fn try_path_to_uri(path: &Path) -> Result<String> {
    let invalid = || Error::tool("lsp", "[LSP_FILE_URI] path has no unambiguous absolute file URI");
    if !path.is_absolute() || path.components().any(|part| part == Component::ParentDir) {
        return Err(invalid());
    }
    // This existing dependency handles native drive prefixes, verbatim disk
    // and UNC paths, and percent-encodes Unix bytes without UTF-8 replacement.
    let uri = url::Url::from_file_path(path).map_err(|()| invalid())?.to_string();
    // Reject NULs, unsupported Windows device names and other identities our
    // inverse cannot safely represent. No filesystem access is performed.
    let native = uri_to_path(&uri).ok_or_else(invalid)?;
    // Use the inverse's normalized drive letter / UNC hostname on Windows
    // so local snapshots and equivalent server URI spellings use one key.
    url::Url::from_file_path(native).map(String::from).map_err(|()| invalid())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode only path octets, never the authority. Escaped separators may not
/// become structural path boundaries; '+' remains a literal filename byte.
fn decode_path(raw: &str) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(raw.len());
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        let byte = if byte == b'%' {
            let value = hex(bytes.next()?)? * 16 + hex(bytes.next()?)?;
            if value == b'/' || (cfg!(windows) && value == b'\\') {
                return None;
            }
            value
        } else {
            byte
        };
        if byte == 0 {
            return None;
        }
        decoded.push(byte);
    }
    if decoded.split(|byte| *byte == b'/').any(|part| matches!(part, b"." | b"..")) {
        return None;
    }
    Some(decoded)
}

/// Convert a file URI into an absolute native path. Non-file schemes,
/// queries/fragments, malformed escapes, NULs and traversal are rejected.
/// A remote authority is a UNC host on Windows and unsupported on Unix;
/// it can never become a path relative to the current workspace.
#[must_use]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let (scheme, rest) = uri.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file")
        || rest.bytes().any(|byte| byte.is_ascii_control() || matches!(byte, b' ' | b'\\' | b'?' | b'#'))
    {
        return None;
    }
    let slash = rest.find('/')?;
    let (authority, raw_path) = rest.split_at(slash);
    // Network names are literal labels, not credentials, ports, percent
    // escapes or Windows drive letters misplaced in the authority slot.
    if !authority.is_empty()
        && (authority.split('.').any(str::is_empty)
            || !authority.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')))
    {
        return None;
    }
    if raw_path.starts_with("//") {
        return None; // Do not reinterpret an empty authority as a UNC prefix.
    }
    let decoded = decode_path(raw_path)?;
    native_path(authority, decoded)
}

#[cfg(unix)]
fn native_path(authority: &str, bytes: Vec<u8>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;

    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(windows)]
fn native_path(authority: &str, bytes: Vec<u8>) -> Option<PathBuf> {
    windows_path(authority, &bytes).map(PathBuf::from)
}

#[cfg(not(any(unix, windows)))]
fn native_path(authority: &str, bytes: Vec<u8>) -> Option<PathBuf> {
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        return None;
    }
    let path = PathBuf::from(String::from_utf8(bytes).ok()?);
    path.is_absolute().then_some(path)
}

// Kept pure and compiled in Unix tests too, so drive/UNC parsing is exercised
// even before the native Windows DSR lane runs. No host lookup or OS call.
#[cfg(any(windows, test))]
fn windows_path(authority: &str, bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let text = text.strip_prefix('/')?;
    let local = authority.is_empty() || authority.eq_ignore_ascii_case("localhost");
    let (prefix, tail) = if local {
        let (drive, tail) = text.split_once('/')?;
        let drive = drive.as_bytes();
        if drive.len() != 2 || !drive[0].is_ascii_alphabetic() || drive[1] != b':' {
            return None;
        }
        (format!("{}:\\", char::from(drive[0].to_ascii_uppercase())), tail)
    } else {
        if text.is_empty() || text.starts_with('/') {
            return None;
        }
        (format!("\\\\{}\\", authority.to_ascii_lowercase()), text)
    };
    for part in tail.split('/').filter(|part| !part.is_empty()) {
        // Without a verbatim prefix, Windows silently aliases trailing dots /
        // spaces and treats reserved basenames as devices. A file URI cannot
        // authorize those aliases or alternate data streams.
        if part.ends_with(['.', ' '])
            || part.chars().any(|ch| ch.is_control() || matches!(ch, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
            || windows_device_name(part)
        {
            return None;
        }
    }
    Some(format!("{prefix}{}", tail.replace('/', "\\")))
}

#[cfg(any(windows, test))]
fn windows_device_name(part: &str) -> bool {
    let name = part.split('.').next().unwrap_or_default().trim_end_matches(' ').to_ascii_uppercase();
    matches!(name.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || name.strip_prefix("COM").or_else(|| name.strip_prefix("LPT"))
            .is_some_and(|suffix| matches!(suffix.as_bytes(), [b'1'..=b'9'])
                || matches!(suffix, "¹" | "²" | "³"))
}

/// Canonical lexical identity shared by notifications and local snapshots.
/// This does not canonicalize filesystem aliases or contact a UNC server.
pub(super) fn normalize_uri(uri: &str) -> Option<String> {
    try_path_to_uri(&uri_to_path(uri)?).ok()
}

#[cfg(test)]
mod tests;
