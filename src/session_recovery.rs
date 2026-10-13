//! Explicit, read-only text recovery from an authoritative JSONL transcript.
//!
//! This is not a degraded `Session::open`: the result has no persistence or
//! provider operations. Native image/media blocks are omitted without opening
//! their data or sidecars. Normal session loading keeps its integrity checks.
//! Only version-3 transcripts with explicit, unambiguous entry IDs are accepted.
//! The source is read twice and compared by digest and file identity; this
//! detects observed changes, not an atomic snapshot against a hostile writer.

use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::file_identity::FileIdentity;
use crate::session::{Session, SessionEntry, SessionHeader};

mod strict_json;
#[cfg(test)]
mod tests;

const SCHEMA: &str = "pi.session_text_recovery.v1";
const BUFFER_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy)]
struct Limits {
    source: u64,
    row: usize,
    retained: usize,
    entries: usize,
    nodes: usize,
}

const LIMITS: Limits = Limits {
    source: 512 * 1024 * 1024,
    row: 100 * 1024 * 1024,
    retained: 64 * 1024 * 1024,
    entries: 100_000,
    nodes: 1_000_000,
};

fn invalid(reason: impl std::fmt::Display) -> Error {
    Error::session(format!("PI_SESSION_TEXT_RECOVERY_INVALID: {reason}"))
}

fn check_owner(owner: &AgentCx) -> Result<()> {
    if !owner.capabilities().io {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "session text recovery requires filesystem I/O authority",
        )
        .into());
    }
    owner.checkpoint().map_err(|_| Error::Aborted)
}

/// One validated native record, with only native image/media content replaced.
/// The original line number and graph identities are retained for inspection.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveredEntry {
    line: usize,
    id: String,
    parent_id: Option<String>,
    record: Value,
}

impl RecoveredEntry {
    pub const fn line(&self) -> usize {
        self.line
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.parent_id.as_deref()
    }

    pub const fn record(&self) -> &Value {
        &self.record
    }
}

/// A deliberate omission, not evidence that an attachment is missing or corrupt.
/// `storage` describes the saved data shape; no reference is followed or decoded.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentOmission {
    pub entry_id: String,
    pub block_index: usize,
    pub kind: String,
    pub mime_type: Option<String>,
    pub name: Option<String>,
    pub storage: &'static str,
}

/// A separate inspection report. It cannot be saved or resumed as a Session.
/// JSON serialization is a report envelope, never a replacement session JSONL.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryReport {
    schema: &'static str,
    source_sha256: String,
    source_bytes: u64,
    header: Value,
    entries: Vec<RecoveredEntry>,
    selected_leaf: Option<String>,
    selection_source: &'static str,
    omissions: Vec<AttachmentOmission>,
    unterminated_final_row: bool,
    #[serde(skip)]
    active_path: Vec<usize>,
}

impl RecoveryReport {
    pub const fn header(&self) -> &Value {
        &self.header
    }

    pub fn entries(&self) -> &[RecoveredEntry] {
        &self.entries
    }

    pub fn selected_leaf(&self) -> Option<&str> {
        self.selected_leaf.as_deref()
    }

    /// Root-to-leaf records only; compaction and rewind markers remain records.
    /// This is historical inspection, not reconstruction of provider context.
    pub fn active_path(&self) -> impl Iterator<Item = &RecoveredEntry> {
        self.active_path.iter().map(|index| &self.entries[*index])
    }

    pub fn omissions(&self) -> &[AttachmentOmission] {
        &self.omissions
    }

    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }
}

impl Session {
    /// Inspect text when ordinary open cannot hydrate an attachment. This
    /// synchronous, bounded operation never opens sidecars or creates files.
    /// SDK callers can use `pi::sdk::Session::recover_text(path)`.
    pub fn recover_text(path: impl AsRef<Path>) -> Result<RecoveryReport> {
        recover_session_text(path)
    }
}

/// Recover a structurally valid version-3 JSONL transcript without media I/O.
///
/// Limits: 512 MiB source, 100 MiB per physical row, 64 MiB retained report
/// payload, 100,000 entries, one million JSON nodes/keys, 64 nesting levels.
/// A malformed row, ambiguous graph, unsupported format or observed source
/// change fails the entire operation; a successful prefix is never returned.
/// Text and metadata are not secret-redacted. Treat the report as private.
/// Blocking filesystem calls cannot be preempted, but every read chunk and
/// graph traversal checks the current owner's I/O authority and cancellation.
pub fn recover_session_text(path: impl AsRef<Path>) -> Result<RecoveryReport> {
    let owner = AgentCx::for_current_or_request();
    recover(path.as_ref(), LIMITS, &owner, || {})
}

#[derive(PartialEq, Eq)]
struct Stamp {
    identity: FileIdentity,
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

fn regular(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    metadata.is_file() && !metadata.file_type().is_symlink()
}

impl Stamp {
    fn read(path: &Path, file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        if !regular(&metadata) || !regular(&std::fs::symlink_metadata(path)?) {
            return Err(invalid("source is not a regular non-symlink file"));
        }
        let identity = FileIdentity::of_open_file(file)?;
        if identity != FileIdentity::of_path_nofollow(path)? {
            return Err(invalid("source identity changed"));
        }
        Ok(Self {
            identity,
            length: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            changed: {
                use std::os::unix::fs::MetadataExt as _;
                (metadata.ctime(), metadata.ctime_nsec())
            },
        })
    }

    fn verify(&self, path: &Path, file: &File) -> Result<()> {
        if *self != Self::read(path, file)? {
            return Err(invalid("source changed during recovery"));
        }
        Ok(())
    }
}

fn open_source(path: &Path) -> Result<File> {
    if !regular(&std::fs::symlink_metadata(path)?) {
        return Err(invalid("source must be a regular non-symlink file"));
    }
    crate::session::ensure_session_file_readable(path)?;
    let identity = FileIdentity::of_path_nofollow(path)?;
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags};
        let fd = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        File::from(fd)
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
            .open(path)?
    };
    #[cfg(not(any(unix, windows)))]
    let file = File::open(path)?;
    if !regular(&file.metadata()?) || FileIdentity::of_open_file(&file)? != identity {
        return Err(invalid("source changed while opening"));
    }
    Ok(file)
}

fn read_row(
    reader: &mut impl BufRead,
    max: usize,
    remaining: &mut u64,
    owner: &AgentCx,
    digest: &mut Sha256,
) -> Result<Vec<u8>> {
    let mut row = Vec::new();
    loop {
        check_owner(owner)?;
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if available.is_empty() {
            return Ok(row);
        }
        let count = available.iter().position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if count > max.saturating_sub(row.len()) || count as u64 > *remaining {
            return Err(invalid("source or row byte limit exceeded"));
        }
        let complete = available[count - 1] == b'\n';
        row.extend_from_slice(&available[..count]);
        digest.update(&available[..count]);
        *remaining -= count as u64;
        reader.consume(count);
        if complete {
            return Ok(row);
        }
    }
}

struct RetainedBudget(usize);

impl Write for RetainedBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len())
            .ok_or_else(|| io::Error::other("retained text recovery limit exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn charge(budget: &mut RetainedBudget, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(budget, value).map_err(|_| invalid("retained report byte limit exceeded"))
}

fn identifier(value: &Value) -> Result<&str> {
    value.as_str().filter(|id| !id.trim().is_empty() && id.len() <= 1024)
        .ok_or_else(|| invalid("entry IDs must be nonempty strings of at most 1024 bytes"))
}

fn omit_attachments(
    record: &mut Value,
    id: &str,
    omissions: &mut Vec<AttachmentOmission>,
    budget: &mut RetainedBudget,
) -> Result<()> {
    if record.get("type").and_then(Value::as_str) != Some("message") {
        return Ok(());
    }
    let Some(blocks) = record.pointer_mut("/message/content").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    for (block_index, block) in blocks.iter_mut().enumerate() {
        let Some(kind @ ("image" | "media")) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        let omission = AttachmentOmission {
            entry_id: id.to_string(),
            block_index,
            kind: kind.to_string(),
            mime_type: block.get("mimeType").and_then(Value::as_str).map(str::to_string),
            name: block.get("name").and_then(Value::as_str).map(str::to_string),
            storage: match block.get("data") {
                Some(Value::String(_)) => "inline",
                Some(Value::Object(_)) => "reference_not_inspected",
                _ => "unrecognized_not_inspected",
            },
        };
        charge(budget, &omission)?;
        *block = json!({"type":"text", "text":format!(
            "[{kind} omitted by read-only session recovery; attachment not inspected]"
        )});
        omissions.push(omission);
    }
    Ok(())
}

fn graph(entries: &[RecoveredEntry], leaf: Option<&str>, owner: &AgentCx) -> Result<Vec<usize>> {
    let mut positions = HashMap::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        check_owner(owner)?;
        if positions.insert(entry.id.as_str(), index).is_some() {
            return Err(invalid("duplicate entry ID"));
        }
    }
    let parents = entries.iter().map(|entry| {
        entry.parent_id.as_deref().map(|parent| {
            positions.get(parent).copied().ok_or_else(|| invalid("missing parent entry"))
        }).transpose()
    }).collect::<Result<Vec<_>>>()?;
    // Each edge is visited at most twice. Forward references and independent
    // roots are valid; cycles in unselected branches still reject the report.
    let mut colors = vec![0_u8; entries.len()];
    for start in 0..entries.len() {
        let mut cursor = Some(start);
        while let Some(index) = cursor {
            check_owner(owner)?;
            match colors[index] {
                1 => return Err(invalid("cyclic parent graph")),
                2 => break,
                _ => colors[index] = 1,
            }
            cursor = parents[index];
        }
        cursor = Some(start);
        while let Some(index) = cursor {
            check_owner(owner)?;
            if colors[index] != 1 {
                break;
            }
            colors[index] = 2;
            cursor = parents[index];
        }
    }
    let mut cursor = leaf.map(|id| {
        positions.get(id).copied().ok_or_else(|| invalid("selected leaf is missing"))
    }).transpose()?;
    let mut path = Vec::new();
    while let Some(index) = cursor {
        check_owner(owner)?;
        path.push(index);
        cursor = parents[index];
    }
    path.reverse();
    Ok(path)
}

#[allow(clippy::too_many_lines)] // Keep admission, both read passes and final verification in order.
fn recover(
    path: &Path,
    limits: Limits,
    owner: &AgentCx,
    between_passes: impl FnOnce(),
) -> Result<RecoveryReport> {
    check_owner(owner)?;
    let file = open_source(path)?;
    let stamp = Stamp::read(path, &file)?;
    if stamp.length > limits.source {
        return Err(invalid("source byte limit exceeded"));
    }
    let mut reader = BufReader::with_capacity(BUFFER_BYTES, file);
    let mut remaining = limits.source;
    let mut nodes = limits.nodes;
    let mut retained = RetainedBudget(limits.retained);
    let mut digest = Sha256::new();
    let first = read_row(&mut reader, limits.row, &mut remaining, owner, &mut digest)?;
    let header = strict_json::parse(&first, &mut nodes).map_err(|_| invalid("invalid JSON header"))?;
    if header.get("type").and_then(Value::as_str) != Some("session")
        || header.get("version").and_then(Value::as_u64) != Some(3)
    {
        return Err(invalid("text recovery requires a version-3 JSONL session"));
    }
    identifier(&header["id"])?;
    let native_header = SessionHeader::deserialize(&header).map_err(|_| invalid("invalid native session header"))?;
    charge(&mut retained, &header)?;
    let mut report = RecoveryReport {
        schema: SCHEMA,
        source_sha256: String::new(),
        source_bytes: 0,
        header,
        entries: Vec::new(),
        selected_leaf: None,
        selection_source: "last_entry",
        omissions: Vec::new(),
        unterminated_final_row: !first.ends_with(b"\n"),
        active_path: Vec::new(),
    };
    let mut line = 1;
    loop {
        let row = read_row(&mut reader, limits.row, &mut remaining, owner, &mut digest)?;
        if row.is_empty() {
            break;
        }
        line += 1;
        report.unterminated_final_row = !row.ends_with(b"\n");
        if row.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if report.entries.len() >= limits.entries {
            return Err(invalid("entry count limit exceeded"));
        }
        let mut record = strict_json::parse(&row, &mut nodes)
            .map_err(|_| invalid(format!("invalid or over-limit JSON at line {line}")))?;
        let id = identifier(&record["id"])?.to_string();
        let parent_id = match record.get("parentId") {
            None | Some(Value::Null) => None,
            Some(parent) => Some(identifier(parent)?.to_string()),
        };
        omit_attachments(&mut record, &id, &mut report.omissions, &mut retained)?;
        // Validate the native shape only after replacing media. No attachment
        // decoder, cache, provider or Session constructor is involved.
        let _ = SessionEntry::deserialize(&record)
            .map_err(|_| invalid(format!("invalid native record at line {line}")))?;
        let entry = RecoveredEntry { line, id, parent_id, record };
        charge(&mut retained, &entry)?;
        report.entries.push(entry);
    }
    report.selected_leaf = match native_header.current_leaf {
        Some(leaf) => {
            report.selection_source = "header";
            if leaf.is_empty() { None } else { Some(leaf) }
        }
        None => report.entries.last().map(|entry| entry.id.clone()),
    };
    report.active_path = graph(&report.entries, report.selected_leaf.as_deref(), owner)?;
    let original: [u8; 32] = digest.finalize().into();
    report.source_bytes = limits.source - remaining;
    if report.source_bytes != stamp.length {
        return Err(invalid("source length changed during recovery"));
    }
    stamp.verify(path, reader.get_ref())?;
    between_passes();
    check_owner(owner)?;
    stamp.verify(path, reader.get_ref())?;
    reader.seek(SeekFrom::Start(0))?;
    let mut second = Sha256::new();
    let mut count = 0_u64;
    let mut buffer = [0_u8; BUFFER_BYTES];
    loop {
        check_owner(owner)?;
        let read = match reader.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if read == 0 { break; }
        count += read as u64;
        if count > stamp.length { return Err(invalid("source grew during verification")); }
        second.update(&buffer[..read]);
    }
    let verified: [u8; 32] = second.finalize().into();
    if count != stamp.length || verified != original {
        return Err(invalid("source content changed during recovery"));
    }
    stamp.verify(path, reader.get_ref())?;
    check_owner(owner)?;
    report.source_sha256 = crate::package_manager::hex_encode(&original);
    Ok(report)
}
