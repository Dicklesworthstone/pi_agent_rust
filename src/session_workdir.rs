//! Explicit, provenance-preserving session workdir recovery.
//!
//! A recorded path is not permission to silently substitute the caller's cwd.
//! Inspect before constructing project configuration, tools, or extensions;
//! attach only after the user has explicitly selected a replacement directory.
//! These helpers do not change the process cwd or move/delete session files.
//!
//! Bindings are session-wide custom entries, not model messages. The original
//! header cwd remains immutable. In-memory helpers require a fully hydrated
//! session; `inspect_saved_session_workdir` also supports lazily loaded stores.

mod persisted;
pub use persisted::{
    SavedSessionWorkdir, inspect_saved_session_workdir, open_session_for_runtime,
};

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::session::{Session, SessionEntry, ensure_session_directory_readable};

/// Reserved custom-entry type for an explicit workdir attachment.
pub const WORKDIR_BINDING_TYPE: &str = "pi.session.workdir.v1";
const WORKDIR_BINDING_PREFIX: &str = "pi.session.workdir.";

/// Filesystem health is separate from the user's decision to attach a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkdirHealth {
    Available { canonical_path: PathBuf },
    Missing,
    NotDirectory,
    InvalidPath,
    Inaccessible { reason: String },
}

impl WorkdirHealth {
    /// Inspect without creating directories or guessing a base for legacy
    /// relative paths. Symlink aliases are compared by canonical identity.
    #[must_use]
    pub fn inspect(path: &Path) -> Self {
        if !path.is_absolute() {
            return Self::InvalidPath;
        }
        let probe = || -> io::Result<PathBuf> {
            if !std::fs::metadata(path)?.is_dir() {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "not a directory",
                ));
            }
            // Also enforce mode-bit denial under privileged test runners.
            ensure_session_directory_readable(path)?;
            // The OS remains authoritative for ACLs and platform-specific errors.
            let _directory = std::fs::read_dir(path)?;
            std::fs::canonicalize(path)
        };
        match probe() {
            Ok(canonical_path) => Self::Available { canonical_path },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::Missing,
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => Self::NotDirectory,
            Err(error) => Self::Inaccessible {
                reason: error.to_string(),
            },
        }
    }

    fn into_directory(self, path: &Path) -> Result<PathBuf> {
        match self {
            Self::Available { canonical_path } => Ok(canonical_path),
            health => Err(Error::session(format!(
                "PI_SESSION_WORKDIR_UNAVAILABLE: workdir {path:?} is {health:?}. \
                 Locate its replacement and explicitly attach, or start a new session. \
                 The recorded session has not been discarded."
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkdirBinding {
    original_cwd: String,
    previous_cwd: String,
    cwd: String,
}

/// Read-only recovery information suitable for an interactive or JSON frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionWorkdir {
    pub original_cwd: PathBuf,
    pub bound_cwd: PathBuf,
    pub health: WorkdirHealth,
}

fn binding_path(session: &Session) -> Result<PathBuf> {
    for entry in session.entries.iter().rev() {
        let SessionEntry::Custom(custom) = entry else {
            continue;
        };
        if !custom.custom_type.starts_with(WORKDIR_BINDING_PREFIX) {
            continue;
        }
        if custom.custom_type != WORKDIR_BINDING_TYPE {
            return Err(Error::session(
                "PI_SESSION_WORKDIR_BINDING_UNSUPPORTED: unknown workdir attachment version; \
                 refusing to fall back to an older directory",
            ));
        }
        let invalid = || {
            Error::session(
                "PI_SESSION_WORKDIR_BINDING_INVALID: malformed workdir attachment; \
                 refusing to fall back to another directory",
            )
        };
        let binding: WorkdirBinding =
            serde_json::from_value(custom.data.clone().ok_or_else(invalid)?)
                .map_err(|_| invalid())?;
        if binding.original_cwd != session.header.cwd || !Path::new(&binding.cwd).is_absolute() {
            return Err(invalid());
        }
        return Ok(PathBuf::from(binding.cwd));
    }
    Ok(PathBuf::from(&session.header.cwd))
}

/// Inspect the latest session-wide attachment, falling back only when no
/// attachment exists. An invalid latest record never revives an older binding.
pub fn inspect_session_workdir(session: &Session) -> Result<SessionWorkdir> {
    let bound_cwd = binding_path(session)?;
    let health = WorkdirHealth::inspect(&bound_cwd);
    Ok(SessionWorkdir {
        original_cwd: PathBuf::from(&session.header.cwd),
        bound_cwd,
        health,
    })
}

/// Resolve the selected workdir or return an actionable recovery error.
/// Read-only operations such as export/import must not call this guard.
pub fn require_session_workdir(session: &Session) -> Result<PathBuf> {
    let state = inspect_session_workdir(session)?;
    state.health.into_directory(&state.bound_cwd)
}

/// Guard a runtime which has already selected its workspace. Never change cwd
/// after configuration or trust decisions have been constructed.
pub fn check_session_workdir(session: &Session, runtime_cwd: &Path) -> Result<()> {
    let expected = require_session_workdir(session)?;
    let actual = WorkdirHealth::inspect(runtime_cwd).into_directory(runtime_cwd)?;
    if expected != actual {
        return Err(Error::session(format!(
            "PI_SESSION_WORKDIR_MISMATCH: session is attached to {expected:?}, \
             but this runtime uses {actual:?}. Resume from the attached directory, \
             explicitly attach to this directory, or start a new session."
        )));
    }
    Ok(())
}

/// Record explicit user consent to use `target` as this session's workdir.
///
/// Returns whether an entry was appended. The caller must save successfully
/// before launching a runtime. A rejected target leaves the session untouched;
/// attaching to an equivalent symlink alias is a no-op. Neither session ID,
/// original cwd, parent-session provenance nor persistence path is rewritten.
/// Lazily loaded stores are hydrated before resolving the previous attachment,
/// which may belong to another branch.
pub fn attach_session_workdir(session: &mut Session, target: &Path) -> Result<bool> {
    let canonical = WorkdirHealth::inspect(target).into_directory(target)?;
    let cwd = canonical
        .to_str()
        .ok_or_else(|| {
            Error::session("PI_SESSION_WORKDIR_INVALID_ENCODING: attachment requires a UTF-8 path")
        })?
        .to_owned();
    session.ensure_full_v2_hydration_before_save()?;
    let previous = binding_path(session)?;
    if let WorkdirHealth::Available { canonical_path } = WorkdirHealth::inspect(&previous)
        && canonical_path == canonical
    {
        return Ok(false);
    }
    let binding = WorkdirBinding {
        original_cwd: session.header.cwd.clone(),
        previous_cwd: previous
            .to_str()
            .ok_or_else(|| {
                Error::session("PI_SESSION_WORKDIR_INVALID_ENCODING: recorded workdir is not UTF-8")
            })?
            .to_owned(),
        cwd,
    };
    let data = serde_json::to_value(binding)?;
    session.append_custom_entry(WORKDIR_BINDING_TYPE.to_owned(), Some(data));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_at(path: &Path) -> Session {
        let mut session = Session::in_memory();
        session.header.cwd = path.to_str().expect("UTF-8 test path").to_owned();
        session
    }

    #[test]
    fn health_distinguishes_missing_file_and_relative_paths() {
        let root = tempfile::tempdir().expect("tempdir");
        let file = root.path().join("file");
        std::fs::write(&file, "not a directory").expect("write file");
        assert_eq!(WorkdirHealth::inspect(&file), WorkdirHealth::NotDirectory);
        assert_eq!(
            WorkdirHealth::inspect(&file.join("child")),
            WorkdirHealth::NotDirectory
        );
        assert_eq!(
            WorkdirHealth::inspect(&root.path().join("gone")),
            WorkdirHealth::Missing
        );
        assert_eq!(
            WorkdirHealth::inspect(Path::new(".")),
            WorkdirHealth::InvalidPath
        );
        assert_eq!(
            WorkdirHealth::inspect(Path::new("")),
            WorkdirHealth::InvalidPath
        );
    }

    #[test]
    fn missing_workdir_is_not_substituted_by_runtime_cwd() {
        let root = tempfile::tempdir().expect("tempdir");
        let session = session_at(&root.path().join("gone"));
        let error = check_session_workdir(&session, root.path()).expect_err("must reject");
        assert!(error.to_string().contains("PI_SESSION_WORKDIR_UNAVAILABLE"));
        assert!(session.entries.is_empty());
    }

    #[test]
    fn another_existing_workdir_requires_explicit_attachment() {
        let original = tempfile::tempdir().expect("original");
        let replacement = tempfile::tempdir().expect("replacement");
        let mut session = session_at(original.path());
        let header = serde_json::to_value(&session.header).expect("header");
        assert!(check_session_workdir(&session, replacement.path()).is_err());
        assert!(attach_session_workdir(&mut session, replacement.path()).expect("attach"));
        check_session_workdir(&session, replacement.path()).expect("attached workspace");
        assert!(check_session_workdir(&session, original.path()).is_err());
        assert_eq!(
            serde_json::to_value(&session.header).expect("header"),
            header
        );
        assert!(!attach_session_workdir(&mut session, replacement.path()).expect("no-op"));
        assert_eq!(session.entries.len(), 1);
    }

    #[test]
    fn moved_directory_can_be_attached_without_rewriting_provenance() {
        let root = tempfile::tempdir().expect("tempdir");
        let old = root.path().join("old");
        let new = root.path().join("new");
        std::fs::create_dir(&old).expect("create");
        let mut session = session_at(&old);
        session.path = Some(root.path().join("conversation.jsonl"));
        let original_path = session.path.clone();
        std::fs::rename(&old, &new).expect("rename");
        assert!(require_session_workdir(&session).is_err());
        attach_session_workdir(&mut session, &new).expect("attach moved workdir");
        assert_eq!(session.header.cwd, old.to_str().expect("UTF-8 path"));
        assert_eq!(session.path, original_path);
        assert_eq!(
            require_session_workdir(&session).expect("resolve"),
            new.canonicalize().expect("canonical")
        );
    }

    #[test]
    fn rejected_attachment_does_not_mutate_history() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut session = session_at(root.path());
        let header = serde_json::to_value(&session.header).expect("header");
        assert!(attach_session_workdir(&mut session, &root.path().join("missing")).is_err());
        assert!(session.entries.is_empty());
        assert_eq!(
            serde_json::to_value(&session.header).expect("header"),
            header
        );
    }

    #[test]
    fn latest_binding_wins_and_serializes_with_session_entries() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        let third = tempfile::tempdir().expect("third");
        let mut session = session_at(first.path());
        attach_session_workdir(&mut session, second.path()).expect("attach second");
        attach_session_workdir(&mut session, third.path()).expect("attach third");
        let mut imported = Session::in_memory();
        imported.header =
            serde_json::from_value(serde_json::to_value(&session.header).expect("encode"))
                .expect("decode");
        imported.entries =
            serde_json::from_value(serde_json::to_value(&session.entries).expect("encode"))
                .expect("decode");
        check_session_workdir(&imported, third.path())
            .expect("latest binding survives serialization");
        assert!(check_session_workdir(&imported, second.path()).is_err());
    }

    #[test]
    fn malformed_latest_binding_never_falls_back() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut session = session_at(root.path());
        session.append_custom_entry(
            WORKDIR_BINDING_TYPE.to_owned(),
            Some(serde_json::json!({"cwd": "/"})),
        );
        let error = require_session_workdir(&session).expect_err("invalid metadata");
        assert!(
            error
                .to_string()
                .contains("PI_SESSION_WORKDIR_BINDING_INVALID")
        );
    }

    #[test]
    fn unknown_binding_version_never_revives_an_older_attachment() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        let mut session = session_at(first.path());
        attach_session_workdir(&mut session, second.path()).expect("known attachment");
        session.append_custom_entry("pi.session.workdir.v2".to_owned(), None);
        let error = require_session_workdir(&session).expect_err("unsupported version");
        assert!(
            error
                .to_string()
                .contains("PI_SESSION_WORKDIR_BINDING_UNSUPPORTED")
        );
        let count = session.entries.len();
        assert!(attach_session_workdir(&mut session, first.path()).is_err());
        assert_eq!(session.entries.len(), count);
    }

    #[test]
    fn binding_cannot_claim_a_different_original_workspace() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut session = session_at(root.path());
        session.append_custom_entry(
            WORKDIR_BINDING_TYPE.to_owned(),
            Some(serde_json::json!({
                "originalCwd": "another-session",
                "previousCwd": session.header.cwd,
                "cwd": root.path(),
            })),
        );
        assert!(require_session_workdir(&session).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn denied_directory_is_inaccessible_even_under_a_privileged_runner() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let denied = root.path().join("denied");
        std::fs::create_dir(&denied).expect("create");
        let permissions = std::fs::metadata(&denied).expect("metadata").permissions();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000))
            .expect("deny access");
        let health = WorkdirHealth::inspect(&denied);
        std::fs::set_permissions(&denied, permissions).expect("restore permissions");
        assert!(matches!(health, WorkdirHealth::Inaccessible { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_is_not_a_workspace_change() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join("target");
        let alias = root.path().join("alias");
        std::fs::create_dir(&target).expect("create");
        std::os::unix::fs::symlink(&target, &alias).expect("symlink");
        let mut session = session_at(&target);
        check_session_workdir(&session, &alias).expect("same workspace");
        assert!(!attach_session_workdir(&mut session, &alias).expect("no-op"));
        assert!(session.entries.is_empty());
    }
}
