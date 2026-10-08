//! Read the session-wide workdir even when the runtime loaded only a V2 tail.
//!
//! This is metadata discovery, not transcript validation or permission to run
//! tools. Execution must still open the original store through `Session` and
//! validate its identity before constructing a runtime. JSONL media references
//! are intentionally not hydrated just to discover a workspace.

use std::borrow::Cow;
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use asupersync::channel::oneshot;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::{SessionWorkdir, WORKDIR_BINDING_PREFIX, inspect_session_workdir};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::file_identity::FileIdentity;
use crate::session::{Session, SessionEntry, SessionHeader, ensure_session_file_readable};

// Match the native JSONL line ceiling. Workdir metadata itself is much smaller;
// never deserialize an arbitrary custom payload into a recovery decision.
const MAX_LINE_BYTES: usize = 100 * 1024 * 1024;
const MAX_BINDING_BYTES: usize = 64 * 1024;

/// Read-only metadata for an original saved store, including its latest
/// session-wide attachment rather than only entries on the selected branch.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedSessionWorkdir {
    pub session_id: String,
    pub source_path: PathBuf,
    pub workdir: SessionWorkdir,
    #[serde(skip)]
    stamp: Option<SourceStamp>,
}

impl SavedSessionWorkdir {
    /// Check that a separately opened runtime session still belongs to this
    /// metadata observation. JSONL replacement or mutation during the open is
    /// rejected. SQLite is already fully hydrated by its native reader, so its
    /// decoded attachment is checked directly instead of trusting file mtimes
    /// that do not account for WAL writes.
    pub fn check_loaded_session(&self, session: &Session) -> Result<()> {
        if session.header.id != self.session_id
            || Path::new(&session.header.cwd) != self.workdir.original_cwd.as_path()
            || session.path.as_deref() != Some(self.source_path.as_path())
        {
            return Err(changed_source());
        }
        if let Some(stamp) = &self.stamp {
            stamp.check_path(&self.source_path)?;
        } else {
            let current = inspect_session_workdir(session)?;
            if current.bound_cwd != self.workdir.bound_cwd {
                return Err(changed_source());
            }
        }
        Ok(())
    }

    /// Resolve the saved workspace before loading project configuration or
    /// constructing tools. An explicit runtime directory is a consistency
    /// assertion, not permission to replace the session's attachment.
    /// Neither this method nor discovery changes the process cwd.
    pub fn resolve_runtime_cwd(&self, requested: Option<&Path>) -> Result<PathBuf> {
        let expected = super::WorkdirHealth::inspect(&self.workdir.bound_cwd)
            .into_directory(&self.workdir.bound_cwd)?;
        if let Some(requested) = requested {
            let actual = super::WorkdirHealth::inspect(requested).into_directory(requested)?;
            if expected != actual {
                return Err(Error::session(format!(
                    "PI_SESSION_WORKDIR_MISMATCH: session is attached to {expected:?}, \
                     but this runtime uses {actual:?}. Resume from the attached directory, \
                     explicitly attach to this directory, or start a new session."
                )));
            }
        }
        Ok(expected)
    }

    /// Require the selected runtime's primary directory, without changing cwd
    /// or treating an additional workspace root as a replacement attachment.
    pub fn check_runtime_cwd(&self, runtime_cwd: &Path) -> Result<()> {
        self.resolve_runtime_cwd(Some(runtime_cwd)).map(|_| ())
    }
}

/// Open a saved session for an existing runtime without changing its workspace.
/// Discover session-wide metadata before native loading, then verify that the
/// loaded transcript still belongs to that observation before installation.
pub async fn open_session_for_runtime(path: &Path, runtime_cwd: &Path) -> Result<Session> {
    let saved = inspect_saved_session_workdir(path).await?;
    saved.check_runtime_cwd(runtime_cwd)?;
    let source_path = saved.source_path.to_str().ok_or_else(|| {
        Error::session("PI_SESSION_WORKDIR_INVALID_ENCODING: session path must be UTF-8")
    })?;
    let session = Session::open(source_path).await?;
    saved.check_loaded_session(&session)?;
    saved.check_runtime_cwd(runtime_cwd)?;
    Ok(session)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceStamp {
    identity: FileIdentity,
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    changed: (i64, i64),
}

impl SourceStamp {
    fn from_metadata(identity: FileIdentity, metadata: &Metadata) -> Result<Self> {
        if !metadata.is_file() {
            return Err(Error::session(
                "Workdir discovery requires a regular session file",
            ));
        }
        Ok(Self {
            identity,
            len: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            changed: {
                use std::os::unix::fs::MetadataExt as _;
                (metadata.ctime(), metadata.ctime_nsec())
            },
        })
    }

    fn of_file(file: &File) -> Result<Self> {
        Self::from_metadata(FileIdentity::of_open_file(file)?, &file.metadata()?)
    }

    fn check_path(&self, path: &Path) -> Result<()> {
        let current = Self::from_metadata(
            FileIdentity::of_path_nofollow(path)?,
            &std::fs::symlink_metadata(path)?,
        )?;
        if self != &current {
            return Err(changed_source());
        }
        Ok(())
    }
}

fn changed_source() -> Error {
    Error::session(
        "PI_SESSION_WORKDIR_SOURCE_CHANGED: saved session changed during workspace discovery; \
         retry discovery before resuming. No workspace was substituted.",
    )
}

#[derive(Deserialize)]
struct MetadataRow<'a> {
    #[serde(rename = "type")]
    kind: Cow<'a, str>,
    #[serde(default, rename = "customType")]
    custom_type: Option<Cow<'a, str>>,
    #[serde(borrow)]
    data: Option<&'a RawValue>,
}

fn invalid_row(line: usize, error: impl std::fmt::Display) -> Error {
    Error::session(format!(
        "PI_SESSION_WORKDIR_SOURCE_INVALID: cannot read workspace metadata at line {line}: \
         {error}. Inspect the original session with --session-audit before recovery."
    ))
}

/// Project only the header and most recent reserved custom entry. Unknown
/// ordinary entries are not interpreted here; the normal session reader owns
/// transcript integrity. Invalid JSON, duplicate headers and malformed workdir
/// records cannot turn into permission to revive an older directory.
fn read_projection(
    reader: &mut impl BufRead,
    cancelled: &AtomicBool,
    max_line_bytes: usize,
) -> Result<Session> {
    let mut projection = Session::in_memory();
    let mut saw_header = false;
    let mut line_number = 0usize;
    let mut line = Vec::new();
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(Error::Aborted);
        }
        line.clear();
        let limit = u64::try_from(max_line_bytes)
            .unwrap_or(u64::MAX - 2)
            .saturating_add(2);
        let read = (&mut *reader).take(limit).read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        line_number += 1;
        let content = line.strip_suffix(b"\n").unwrap_or(&line);
        if content.len() > max_line_bytes {
            return Err(invalid_row(
                line_number,
                "JSONL line exceeds the metadata reader limit",
            ));
        }
        let text = std::str::from_utf8(content).map_err(|error| invalid_row(line_number, error))?;
        let text = if line_number == 1 {
            text.trim_start_matches('\u{feff}')
        } else {
            text
        };
        if text.trim().is_empty() {
            continue;
        }
        let row: MetadataRow<'_> =
            serde_json::from_str(text).map_err(|error| invalid_row(line_number, error))?;
        if !saw_header {
            if row.kind != "session" {
                return Err(invalid_row(line_number, "missing session header"));
            }
            let header: SessionHeader =
                serde_json::from_str(text).map_err(|error| invalid_row(line_number, error))?;
            if header.id.trim().is_empty() {
                return Err(invalid_row(line_number, "empty session ID"));
            }
            projection.header = header;
            saw_header = true;
            continue;
        }
        if row.kind == "session" {
            return Err(invalid_row(line_number, "duplicate session header"));
        }
        if row
            .custom_type
            .as_deref()
            .is_some_and(|kind| kind.starts_with(WORKDIR_BINDING_PREFIX))
        {
            if row.kind != "custom" {
                return Err(invalid_row(
                    line_number,
                    "workdir metadata is not a custom entry",
                ));
            }
            if row
                .data
                .is_some_and(|data| data.get().len() > MAX_BINDING_BYTES)
            {
                return Err(invalid_row(
                    line_number,
                    "workdir attachment exceeds 64 KiB",
                ));
            }
            let entry: SessionEntry =
                serde_json::from_str(text).map_err(|error| invalid_row(line_number, error))?;
            projection.entries.clear();
            projection.entries.push(entry);
        }
    }
    if !saw_header {
        return Err(invalid_row(line_number, "missing session header"));
    }
    Ok(projection)
}

fn read_jsonl(path: &Path, cancelled: &AtomicBool) -> Result<SavedSessionWorkdir> {
    ensure_session_file_readable(path)?;
    let file = File::open(path)?;
    let stamp = SourceStamp::of_file(&file)?;
    stamp.check_path(path)?;
    // A concurrently growing file must not keep a metadata worker alive
    // forever. A changed length/identity at settlement rejects the observation.
    let mut reader = BufReader::new((&file).take(stamp.len));
    let projection = read_projection(&mut reader, cancelled, MAX_LINE_BYTES)?;
    if SourceStamp::of_file(&file)? != stamp {
        return Err(changed_source());
    }
    stamp.check_path(path)?;
    let workdir = inspect_session_workdir(&projection)?;
    Ok(SavedSessionWorkdir {
        session_id: projection.header.id,
        source_path: path.to_path_buf(),
        workdir,
        stamp: Some(stamp),
    })
}

struct CancelRead(Arc<AtomicBool>);

impl Drop for CancelRead {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Inspect a saved session without starting a model, extension, or tool and
/// without saving, repairing, or moving it. JSONL discovery scans bounded lines
/// on a worker and ignores V2 hydration settings and media sidecars. SQLite
/// uses its native full-session reader, including its existing integrity and
/// read-only access rules; that optional backend may hydrate attachments.
///
/// This does not authorize execution or replace transcript validation. Open
/// the original store normally, then call [`SavedSessionWorkdir::check_loaded_session`]
/// and [`SavedSessionWorkdir::check_runtime_cwd`] before constructing its runtime.
pub async fn inspect_saved_session_workdir(path: &Path) -> Result<SavedSessionWorkdir> {
    ensure_session_file_readable(path)?;
    let path = std::fs::canonicalize(path)?;
    if path
        .extension()
        .is_some_and(|extension| extension == "sqlite")
    {
        let text = path
            .to_str()
            .ok_or_else(|| Error::session("Session path is not UTF-8"))?;
        let (session, diagnostics) = Session::open_with_diagnostics(text).await?;
        if !diagnostics.skipped_entries.is_empty() || !diagnostics.orphaned_parent_links.is_empty()
        {
            return Err(invalid_row(0, "session has recovery diagnostics"));
        }
        let workdir = inspect_session_workdir(&session)?;
        return Ok(SavedSessionWorkdir {
            session_id: session.header.id,
            source_path: path,
            workdir,
            stamp: None,
        });
    }
    let (tx, mut rx) = oneshot::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let _cancel_on_drop = CancelRead(Arc::clone(&cancelled));
    std::thread::Builder::new()
        .name("session-workdir-metadata".to_string())
        .spawn(move || {
            let result = read_jsonl(&path, &cancelled);
            let cx = AgentCx::for_request();
            let _ = tx.send(cx.cx(), result);
        })?;
    let cx = AgentCx::for_current_or_request();
    rx.recv(cx.cx())
        .await
        .map_err(|_| Error::session("Workdir metadata discovery was interrupted"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_workdir::{WORKDIR_BINDING_TYPE, WorkdirHealth, attach_session_workdir};
    use serde_json::json;

    fn fixture(path: &Path, cwd: &Path) -> Session {
        let mut session = Session::in_memory();
        session.header.cwd = cwd.to_str().unwrap().to_string();
        session.path = Some(path.to_path_buf());
        session
    }

    fn write_fixture(session: &Session) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&session.header).unwrap();
        bytes.push(b'\n');
        for entry in &session.entries {
            serde_json::to_writer(&mut bytes, entry).unwrap();
            bytes.push(b'\n');
        }
        std::fs::write(session.path.as_ref().unwrap(), &bytes).unwrap();
        bytes
    }

    #[test]
    fn saved_binding_survives_a_missing_original_and_a_partial_runtime_view() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().canonicalize().unwrap();
        let mut session = fixture(&target.join("saved.jsonl"), &target.join("old-project"));
        attach_session_workdir(&mut session, &target).unwrap();
        let before = write_fixture(&session);
        let saved = read_jsonl(session.path.as_ref().unwrap(), &AtomicBool::new(false)).unwrap();
        assert_eq!(saved.workdir.bound_cwd, target);
        assert!(matches!(
            saved.workdir.health,
            WorkdirHealth::Available { .. }
        ));
        // A tail or active-path view can exclude every binding. Never infer
        // session-wide state from that intentionally incomplete entry subset.
        session.entries.clear();
        saved.check_loaded_session(&session).unwrap();
        saved.check_runtime_cwd(&target).unwrap();
        assert_eq!(
            std::fs::read(session.path.as_ref().unwrap()).unwrap(),
            before
        );
        assert_eq!(
            session.header.cwd,
            target.join("old-project").to_str().unwrap()
        );
    }

    #[test]
    fn only_latest_reserved_record_controls_recovery() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let mut session = fixture(&root.path().join("saved.jsonl"), root.path());
        attach_session_workdir(&mut session, &first).unwrap();
        let earlier = session.leaf_id.clone().unwrap();
        attach_session_workdir(&mut session, &second).unwrap();
        assert!(session.navigate_to(&earlier));
        write_fixture(&session);
        let saved = read_jsonl(session.path.as_ref().unwrap(), &AtomicBool::new(false)).unwrap();
        assert_eq!(saved.workdir.bound_cwd, second.canonicalize().unwrap());
        assert!(saved.check_runtime_cwd(&first).is_err());
        session.append_custom_entry(WORKDIR_BINDING_TYPE.into(), Some(json!({"cwd": second})));
        write_fixture(&session);
        assert!(
            read_jsonl(session.path.as_ref().unwrap(), &AtomicBool::new(false))
                .unwrap_err()
                .to_string()
                .contains("BINDING_INVALID")
        );
        session.append_custom_entry("pi.session.workdir.v2".into(), None);
        write_fixture(&session);
        assert!(
            read_jsonl(session.path.as_ref().unwrap(), &AtomicBool::new(false))
                .unwrap_err()
                .to_string()
                .contains("BINDING_UNSUPPORTED")
        );
    }

    #[test]
    fn discovery_does_not_hydrate_or_interpret_message_payloads() {
        let root = tempfile::tempdir().unwrap();
        let session = fixture(&root.path().join("saved.jsonl"), root.path());
        let mut bytes = write_fixture(&session);
        let payload = json!({
            "type": "message", "id": "opaque-message", "message": {
                "role": "user", "content": [{"type": "image", "data": "x".repeat(128 * 1024)}]
            }
        });
        serde_json::to_writer(&mut bytes, &payload).unwrap();
        std::fs::write(session.path.as_ref().unwrap(), &bytes).unwrap();
        let saved = read_jsonl(session.path.as_ref().unwrap(), &AtomicBool::new(false)).unwrap();
        assert_eq!(saved.session_id, session.header.id);
        assert_eq!(
            std::fs::read(session.path.as_ref().unwrap()).unwrap(),
            bytes
        );
    }

    #[test]
    fn changed_store_and_wrong_decoded_identity_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let mut session = fixture(&root.path().join("saved.jsonl"), root.path());
        write_fixture(&session);
        let saved = read_jsonl(session.path.as_ref().unwrap(), &AtomicBool::new(false)).unwrap();
        let id = session.header.id.clone();
        session.header.id = "different-session".into();
        assert!(saved.check_loaded_session(&session).is_err());
        session.header.id = id;
        let old = session
            .path
            .as_ref()
            .unwrap()
            .with_extension("retained-original");
        std::fs::rename(session.path.as_ref().unwrap(), &old).unwrap();
        write_fixture(&session);
        assert!(
            saved
                .check_loaded_session(&session)
                .unwrap_err()
                .to_string()
                .contains("SOURCE_CHANGED")
        );
        assert!(old.is_file());
    }

    #[test]
    fn invalid_json_duplicate_headers_and_line_limits_never_fall_back() {
        let root = tempfile::tempdir().unwrap();
        let session = fixture(&root.path().join("saved.jsonl"), root.path());
        let header = serde_json::to_string(&session.header).unwrap();
        for bytes in [
            format!("{header}\n{{broken"),
            format!("{header}\n{header}\n"),
            "{}\n".to_string(),
            String::new(),
        ] {
            assert!(
                read_projection(
                    &mut std::io::Cursor::new(bytes),
                    &AtomicBool::new(false),
                    MAX_LINE_BYTES
                )
                .is_err()
            );
        }
        assert!(
            read_projection(
                &mut std::io::Cursor::new(header),
                &AtomicBool::new(false),
                16
            )
            .is_err()
        );
    }

    #[test]
    fn cancelled_metadata_work_stops_before_reading() {
        let mut reader = std::io::Cursor::new(b"not JSON");
        assert!(matches!(
            read_projection(&mut reader, &AtomicBool::new(true), MAX_LINE_BYTES),
            Err(Error::Aborted)
        ));
        assert_eq!(reader.position(), 0);
        let signal = Arc::new(AtomicBool::new(false));
        drop(CancelRead(Arc::clone(&signal)));
        assert!(signal.load(Ordering::Acquire));
    }

    #[test]
    fn public_discovery_and_native_open_agree_without_writing_the_source() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cwd = root.path().canonicalize().unwrap();
            let mut original = fixture(&cwd.join("saved.jsonl"), &cwd.join("moved-away"));
            attach_session_workdir(&mut original, &cwd).unwrap();
            let before = write_fixture(&original);
            let saved = inspect_saved_session_workdir(original.path.as_ref().unwrap())
                .await
                .unwrap();
            let loaded = Session::open(saved.source_path.to_str().unwrap())
                .await
                .unwrap();
            saved.check_loaded_session(&loaded).unwrap();
            saved.check_runtime_cwd(&cwd).unwrap();
            assert_eq!(loaded.header.cwd, original.header.cwd);
            assert_eq!(std::fs::read(&saved.source_path).unwrap(), before);
        });
    }

    #[cfg(feature = "sqlite-sessions")]
    #[test]
    fn sqlite_discovery_uses_the_native_saved_binding() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let cwd = root.path().canonicalize().unwrap();
            let mut session = Session::create_with_dir_and_store(
                Some(cwd.clone()),
                crate::session::SessionStoreKind::Sqlite,
            );
            session.header.cwd = cwd.join("moved-away").to_str().unwrap().into();
            attach_session_workdir(&mut session, &cwd).unwrap();
            session.save().await.unwrap();
            let saved = inspect_saved_session_workdir(session.path.as_ref().unwrap())
                .await
                .unwrap();
            let loaded = Session::open(saved.source_path.to_str().unwrap())
                .await
                .unwrap();
            saved.check_loaded_session(&loaded).unwrap();
            saved.check_runtime_cwd(&cwd).unwrap();
            assert_eq!(loaded.header.cwd, session.header.cwd);
            assert_eq!(loaded.entries.len(), session.entries.len());
        });
    }
}
