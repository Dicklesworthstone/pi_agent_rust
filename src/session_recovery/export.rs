//! Bounded inspection exports, deliberately separate from session persistence.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::{
    AgentCx, BUFFER_BYTES, FileIdentity, RecoveredEntry, RecoveryReport, Result, RetainedBudget,
    check_owner, graph, invalid, regular,
};

const MAX_EXPORT_BYTES: usize = 256 * 1024 * 1024;

mod session_copy;

/// Select historical records, not a new active session or provider context.
#[derive(Debug, Clone, Copy)]
pub enum RecoverySelection<'a> {
    /// Every record in its original physical order, including other branches.
    All,
    /// The saved selected path, root first. An explicit root selects no records.
    Active,
    /// The root-to-entry path ending at this exact saved ID.
    Branch(&'a str),
}

/// Machine-readable exports always retain the whole graph and omission report.
#[derive(Debug, Clone, Copy)]
pub enum RecoveryExport<'a> {
    Json,
    Text(RecoverySelection<'a>),
}

struct CheckedWriter<'a, W> {
    inner: &'a mut W,
    owner: &'a AgentCx,
    remaining: usize,
}

impl<W: Write> Write for CheckedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Interrupted would make write_all retry forever after cancellation.
        check_owner(self.owner)
            .map_err(|_| io::Error::other("session recovery export cancelled or denied"))?;
        if bytes.len() > self.remaining {
            return Err(io::Error::other(
                "session recovery export byte limit exceeded",
            ));
        }
        let count = bytes.len().min(BUFFER_BYTES);
        let written = self.inner.write(&bytes[..count])?;
        if written > count {
            return Err(io::Error::other(
                "export writer returned an invalid byte count",
            ));
        }
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        check_owner(self.owner)
            .map_err(|_| io::Error::other("session recovery export cancelled or denied"))?;
        self.inner.flush()
    }
}

fn unsafe_display(ch: char) -> bool {
    ch.is_control()
        || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// Preserve ordinary Unicode and multiline text, but never emit terminal
/// escapes, carriage returns, bidi overrides or invisible line separators.
/// Double literal backslashes to distinguish them from generated escapes.
fn safe_text(writer: &mut impl Write, text: &str, multiline: bool) -> io::Result<()> {
    let mut start = 0;
    for (index, ch) in text.char_indices() {
        let allowed_layout = multiline && matches!(ch, '\n' | '\t');
        if ch != '\\' && (!unsafe_display(ch) || allowed_layout) {
            continue;
        }
        writer.write_all(&text.as_bytes()[start..index])?;
        if ch == '\\' {
            writer.write_all(b"\\\\")?;
        } else {
            write!(writer, "\\u{{{:x}}}", u32::from(ch))?;
        }
        start = index + ch.len_utf8();
    }
    writer.write_all(&text.as_bytes()[start..])
}

fn json_line(writer: &mut impl Write, value: &impl Serialize) -> Result<()> {
    // One record is already bounded by the retained-report limit. Compact
    // JSON avoids indentation amplification on deeply nested custom metadata.
    let json = serde_json::to_string(value)?;
    safe_text(writer, &json, false)?;
    writeln!(writer)?;
    Ok(())
}

fn metadata_without<'a>(value: &'a Value, excluded: &str) -> BTreeMap<&'a str, &'a Value> {
    value
        .as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter(|(key, _)| key.as_str() != excluded)
        .map(|(key, value)| (key.as_str(), value))
        .collect()
}

fn render_entry(writer: &mut impl Write, entry: &RecoveredEntry) -> Result<()> {
    write!(writer, "\n--- line {} | id ", entry.line)?;
    safe_text(writer, &entry.id, false)?;
    writer.write_all(b" | parent ")?;
    safe_text(
        writer,
        entry.parent_id.as_deref().unwrap_or("<root>"),
        false,
    )?;
    writer.write_all(b" ---\n")?;
    let Some(message) = entry.record.get("message") else {
        return json_line(writer, &entry.record);
    };
    writer.write_all(b"Record: ")?;
    json_line(writer, &metadata_without(&entry.record, "message"))?;
    writer.write_all(b"Message: ")?;
    json_line(writer, &metadata_without(message, "content"))?;
    match message.get("content") {
        Some(Value::String(text)) => {
            safe_text(writer, text, true)?;
            writeln!(writer)?;
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                let field = match block.get("type").and_then(Value::as_str) {
                    Some("text") => "text",
                    Some("thinking") => "thinking",
                    _ => {
                        json_line(writer, block)?;
                        continue;
                    }
                };
                json_line(writer, &metadata_without(block, field))?;
                if let Some(text) = block.get(field).and_then(Value::as_str) {
                    safe_text(writer, text, true)?;
                    writeln!(writer)?;
                }
            }
        }
        Some(value) => json_line(writer, value)?,
        None => {} // bash/summary/custom roles retain their fields in Message.
    }
    Ok(())
}

impl RecoveryReport {
    fn indices(&self, selection: RecoverySelection<'_>, owner: &AgentCx) -> Result<Vec<usize>> {
        match selection {
            RecoverySelection::All => Ok((0..self.entries.len()).collect()),
            RecoverySelection::Active => Ok(self.active_path.clone()),
            RecoverySelection::Branch(id) => graph(&self.entries, Some(id), owner),
        }
    }

    fn render_text(
        &self,
        indices: &[usize],
        selection: RecoverySelection<'_>,
        writer: &mut impl Write,
    ) -> Result<()> {
        writer.write_all(b"Pi session text recovery (read-only inspection)\n")?;
        writer.write_all(
            b"Media omitted without inspection. This is NOT a resumable session or a repair.\n",
        )?;
        writer.write_all(b"Private history: text and metadata are not secret-redacted.\n")?;
        writeln!(writer, "Source SHA-256: {}", self.source_sha256)?;
        writeln!(writer, "Source bytes: {}", self.source_bytes)?;
        match selection {
            RecoverySelection::All => {
                writer.write_all(b"Selection: all records in saved order\n")?;
            }
            RecoverySelection::Active => {
                writer.write_all(b"Selection: saved active path, root first\n")?;
            }
            RecoverySelection::Branch(id) => {
                writer.write_all(b"Selection: root-to-entry path ending at ")?;
                safe_text(writer, id, false)?;
                writeln!(writer)?;
            }
        }
        writeln!(
            writer,
            "Records displayed: {} of {}",
            indices.len(),
            self.entries.len()
        )?;
        writeln!(
            writer,
            "Media omissions in whole source: {}",
            self.omissions.len()
        )?;
        if self.unterminated_final_row {
            writer
                .write_all(b"Source ended without a newline; no source repair was performed.\n")?;
        }
        writer.write_all(b"Header: ")?;
        json_line(writer, &self.header)?;
        for index in indices {
            render_entry(writer, &self.entries[*index])?;
        }
        writer.write_all(b"\nEnd of read-only recovery export.\n")?;
        Ok(())
    }

    /// Export all records as readable text. Controls are escaped; the original
    /// text remains available in `record()` and the JSON report. The source and
    /// sidecars are not reopened. Output errors can leave a prefix in a caller-
    /// supplied writer; use `export_file` for complete, create-only publication.
    pub fn write_text(&self, writer: &mut impl Write) -> Result<()> {
        self.write_selected_text(RecoverySelection::All, writer)
    }

    /// Export one explicit history selection. Unknown IDs and size-limit
    /// failures are rejected before the first byte reaches the caller's writer.
    pub fn write_selected_text(
        &self,
        selection: RecoverySelection<'_>,
        writer: &mut impl Write,
    ) -> Result<()> {
        let owner = AgentCx::for_current_or_request();
        self.write_text_with_limit(selection, writer, MAX_EXPORT_BYTES, &owner)
    }

    fn write_text_with_limit(
        &self,
        selection: RecoverySelection<'_>,
        writer: &mut impl Write,
        limit: usize,
        owner: &AgentCx,
    ) -> Result<()> {
        check_owner(owner)?;
        let indices = self.indices(selection, owner)?;
        let mut count = RetainedBudget(limit);
        self.render_text(
            &indices,
            selection,
            &mut CheckedWriter {
                inner: &mut count,
                owner,
                remaining: limit,
            },
        )?;
        self.render_text(
            &indices,
            selection,
            &mut CheckedWriter {
                inner: writer,
                owner,
                remaining: limit,
            },
        )
    }

    /// Stream the complete JSON report, preserving Unicode and original text
    /// exactly as held by the report. Unlike write_text, JSON is intended for
    /// machine consumption, not as a terminal-control sanitizer.
    pub fn write_json(&self, writer: &mut impl Write) -> Result<()> {
        let owner = AgentCx::for_current_or_request();
        check_owner(&owner)?;
        let mut count = RetainedBudget(MAX_EXPORT_BYTES);
        serde_json::to_writer(&mut count, self)?;
        count.write_all(b"\n")?;
        let mut checked = CheckedWriter {
            inner: writer,
            owner: &owner,
            remaining: MAX_EXPORT_BYTES,
        };
        serde_json::to_writer(&mut checked, self)?;
        checked.write_all(b"\n")?;
        Ok(())
    }

    /// Publish a complete report to a NEW file. Existing destinations, including
    /// the source, hard links and dangling symlinks, are never overwritten.
    /// A private staging file is fully written and synced before no-clobber
    /// publication. This syncs file content, not the parent directory; it is
    /// not a crash-durable directory or hostile-directory confinement claim.
    pub fn export_file(
        &self,
        destination: impl AsRef<Path>,
        format: RecoveryExport<'_>,
    ) -> Result<()> {
        let owner = AgentCx::for_current_or_request();
        publish_new_file(
            destination.as_ref(),
            &owner,
            |_| Ok(()),
            |file| match format {
                RecoveryExport::Json => self.write_json(file),
                RecoveryExport::Text(selection) => self.write_selected_text(selection, file),
            },
        )
    }
}

/// Shared publication for inspection reports and explicitly requested session
/// copies. Validation runs before staging and again immediately before the
/// no-clobber commit; neither path can overwrite an existing destination.
fn publish_new_file(
    destination: &Path,
    owner: &AgentCx,
    validate: impl Fn(&Path) -> Result<()>,
    write: impl FnOnce(&mut std::fs::File) -> Result<()>,
) -> Result<()> {
    check_owner(owner)?;
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("export requires a filename"))?;
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    let destination = parent.join(name);
    match std::fs::symlink_metadata(&destination) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "recovery export never replaces an existing destination",
            )
            .into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    validate(&destination)?;
    let parent_identity = FileIdentity::of_path_nofollow(&parent)?;
    check_owner(owner)?;
    let mut staged = tempfile::Builder::new()
        .prefix(".pi-text-recovery-")
        .tempfile_in(&parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    write(staged.as_file_mut())?;
    check_owner(owner)?;
    staged.as_file().sync_all()?;
    if FileIdentity::of_path_nofollow(&parent)? != parent_identity
        || !regular(&std::fs::symlink_metadata(staged.path())?)
        || FileIdentity::of_path_nofollow(staged.path())?
            != FileIdentity::of_open_file(staged.as_file())?
    {
        return Err(invalid("export staging path changed before publication"));
    }
    validate(&destination)?;
    check_owner(owner)?;
    drop(
        staged
            .persist_noclobber(destination)
            .map_err(|error| error.error)?,
    );
    // Once admitted, a complete file may exist despite later cancellation.
    // This syncs the file, not its directory, as documented by export_file.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{header, transcript, user};
    use super::*;
    use serde_json::json;

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, RecoveryReport) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("saved.jsonl");
        transcript(
            &path,
            &header(Some("chosen")),
            &[
                user("root", None, json!("root text")),
                user("chosen", Some("root"), json!("chosen text")),
                user("other", Some("root"), json!("other branch")),
            ],
        );
        let report = super::super::recover_session_text(&path).unwrap();
        (dir, path, report)
    }

    #[test]
    fn all_active_and_explicit_branch_exports_are_distinct_and_nonmutating() {
        let (_dir, path, report) = fixture();
        let source = std::fs::read(&path).unwrap();
        let mut all = Vec::new();
        report.write_text(&mut all).unwrap();
        assert!(String::from_utf8(all).unwrap().contains("other branch"));
        for (selection, present, absent) in [
            (RecoverySelection::Active, "chosen text", "other branch"),
            (
                RecoverySelection::Branch("other"),
                "other branch",
                "chosen text",
            ),
        ] {
            let mut bytes = Vec::new();
            report.write_selected_text(selection, &mut bytes).unwrap();
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("root text"));
            assert!(text.contains(present));
            assert!(!text.contains(absent));
            assert!(text.contains("Records displayed: 2 of 3"));
        }
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert_eq!(report.selected_leaf(), Some("chosen"));
    }

    #[test]
    fn unknown_selection_and_export_size_refusal_write_nothing() {
        let (_dir, _path, report) = fixture();
        let mut bytes = Vec::new();
        assert!(
            report
                .write_selected_text(RecoverySelection::Branch("missing"), &mut bytes)
                .is_err()
        );
        assert!(bytes.is_empty());
        assert!(
            report
                .write_text_with_limit(
                    RecoverySelection::All,
                    &mut bytes,
                    16,
                    &AgentCx::for_request()
                )
                .is_err()
        );
        assert!(bytes.is_empty());
    }

    #[test]
    fn terminal_controls_are_escaped_but_json_keeps_original_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("controls.jsonl");
        let original =
            "line one\n界\tvalue\r\u{1b}]52;c;payload\u{7}\u{9b}2J\u{202e}spoof\\literal";
        transcript(
            &path,
            &header(None),
            &[user("id\u{1b}", None, json!(original))],
        );
        let report = super::super::recover_session_text(&path).unwrap();
        let mut text = Vec::new();
        report.write_text(&mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("line one\n界\tvalue"));
        for forbidden in ['\r', '\u{1b}', '\u{7}', '\u{9b}', '\u{202e}'] {
            assert!(!text.contains(forbidden));
        }
        assert!(text.contains("\\u{1b}"));
        assert!(text.contains("\\u{202e}"));
        assert!(text.contains("\\\\literal"));
        let mut json_bytes = Vec::new();
        report.write_json(&mut json_bytes).unwrap();
        let decoded: Value = serde_json::from_slice(&json_bytes).unwrap();
        assert_eq!(
            decoded["entries"][0]["record"]["message"]["content"],
            original
        );
    }

    #[test]
    fn publication_never_overwrites_source_existing_output_or_hardlink() {
        let (dir, source_path, report) = fixture();
        let original = std::fs::read(&source_path).unwrap();
        let format = RecoveryExport::Text(RecoverySelection::All);
        assert!(report.export_file(&source_path, format).is_err());
        let alias = dir.path().join("alias.txt");
        std::fs::hard_link(&source_path, &alias).unwrap();
        assert!(report.export_file(&alias, RecoveryExport::Json).is_err());
        let output = dir.path().join("report.txt");
        report.export_file(&output, format).unwrap();
        let exported = std::fs::read(&output).unwrap();
        assert!(exported.ends_with(b"End of read-only recovery export.\n"));
        assert!(report.export_file(&output, RecoveryExport::Json).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), exported);
        assert_eq!(std::fs::read(&source_path).unwrap(), original);
        assert_eq!(std::fs::read(&alias).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
    }

    #[test]
    fn failed_export_leaves_no_destination_or_staging_debris() {
        let (dir, source, report) = fixture();
        let output = dir.path().join("invalid.txt");
        assert!(
            report
                .export_file(
                    &output,
                    RecoveryExport::Text(RecoverySelection::Branch("missing"))
                )
                .is_err()
        );
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let output = dir.path().join("valid.json");
        report.export_file(&output, RecoveryExport::Json).unwrap();
        let decoded: Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
        assert_eq!(decoded["entries"].as_array().unwrap().len(), 3);
        assert!(source.is_file());
    }

    #[test]
    fn writer_failure_is_reported_and_never_changes_the_report() {
        struct Fails;
        impl Write for Fails {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "consumer closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (_dir, _source, report) = fixture();
        assert!(report.write_text(&mut Fails).is_err());
        assert!(report.write_json(&mut Fails).is_err());
        let mut successful = Vec::new();
        report.write_text(&mut successful).unwrap();
        assert!(!successful.is_empty());
        assert_eq!(report.entries().len(), 3);
    }

    #[test]
    fn cancellation_during_write_stops_instead_of_retrying_interrupted_forever() {
        struct CancelAfterWrite<'a> {
            owner: &'a AgentCx,
            writes: usize,
        }
        impl Write for CancelAfterWrite<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                assert_eq!(self.writes, 1, "no second write after cancellation");
                self.owner
                    .cancel_with(asupersync::types::CancelKind::User, Some("stop export"));
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (_dir, _source, report) = fixture();
        let owner = AgentCx::for_request();
        let mut writer = CancelAfterWrite {
            owner: &owner,
            writes: 0,
        };
        assert!(
            report
                .write_text_with_limit(
                    RecoverySelection::All,
                    &mut writer,
                    MAX_EXPORT_BYTES,
                    &owner
                )
                .is_err()
        );
        assert_eq!(writer.writes, 1);
    }

    #[cfg(unix)]
    #[test]
    fn new_exports_are_private_and_dangling_symlinks_are_not_followed() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let (dir, _source, report) = fixture();
        let output = dir.path().join("report.json");
        report.export_file(&output, RecoveryExport::Json).unwrap();
        assert_eq!(
            std::fs::metadata(output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let absent = dir.path().join("absent.txt");
        let linked = dir.path().join("linked.txt");
        symlink(&absent, &linked).unwrap();
        assert!(report.export_file(&linked, RecoveryExport::Json).is_err());
        assert!(!absent.exists());
        assert!(
            std::fs::symlink_metadata(linked)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
