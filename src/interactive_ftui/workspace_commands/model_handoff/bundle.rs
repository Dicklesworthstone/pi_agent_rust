//! Portable handoff bundles: explicit local publication, never automatic delivery.
//! Stage a bounded private file, then publish without replacing any existing path.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Brief, MAX_FOCUS_BYTES, Snapshot, failure};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};

pub(super) const MAX_BUNDLE_BYTES: usize = 256 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const USAGE: &str = "Usage: /handoff --model [--export <absolute-path.json>] [-- <focus>]. Quote paths containing spaces as a JSON string. Export creates a new local bundle only; it never overwrites, sends to a recipient, changes sessions or starts work.";

pub(super) struct Options<'a> {
    pub focus: &'a str,
    pub export: Option<PathBuf>,
}

fn word(input: &str) -> (&str, &str) {
    input
        .trim_start()
        .split_once(char::is_whitespace)
        .unwrap_or_else(|| (input.trim_start(), ""))
}

fn invalid() -> Error {
    Error::validation(USAGE)
}

impl<'a> Options<'a> {
    pub fn parse(args: &'a str) -> Result<Self> {
        // JSON escaping can expand one path byte up to six input bytes.
        if args.len() > MAX_PATH_BYTES * 6 + MAX_FOCUS_BYTES + 128 || args.contains('\0') {
            return Err(invalid());
        }
        let (verb, mut tail) = word(args.trim());
        if verb != "--model" {
            return Err(invalid());
        }
        let export = if word(tail).0 == "--export" {
            tail = word(tail).1.trim_start();
            let path = if tail.starts_with('"') {
                let mut values = serde_json::Deserializer::from_str(tail).into_iter::<String>();
                let path = values.next().ok_or_else(invalid)?.map_err(|_| invalid())?;
                tail = &tail[values.byte_offset()..];
                if tail.chars().next().is_some_and(|c| !c.is_whitespace()) {
                    return Err(invalid());
                }
                path
            } else {
                let (path, rest) = word(tail);
                tail = rest;
                path.to_owned()
            };
            if path.is_empty() || path.len() > MAX_PATH_BYTES || path.contains('\0') {
                return Err(invalid());
            }
            let path = PathBuf::from(path);
            if !path.is_absolute() || path.file_name().is_none() {
                return Err(invalid());
            }
            Some(path)
        } else {
            None
        };
        let tail = tail.trim();
        let focus = if tail.is_empty() {
            ""
        } else {
            let (separator, focus) = word(tail);
            let focus = focus.trim();
            if separator != "--" || focus.is_empty() || focus.len() > MAX_FOCUS_BYTES {
                return Err(invalid());
            }
            focus
        };
        Ok(Self { focus, export })
    }
}

fn authorized(owner: &AgentCx) -> Result<()> {
    owner
        .checkpoint()
        .map_err(|_| failure("HANDOFF_EXPORT_CANCELLED"))?;
    if !owner.capabilities().io {
        return Err(failure("HANDOFF_EXPORT_PERMISSION"));
    }
    Ok(())
}

fn require_absent(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(failure("HANDOFF_EXPORT_EXISTS")),
        Err(_) => Err(failure("HANDOFF_EXPORT_PATH")),
    }
}

/// Resolve the user-selected parent before a paid model request. No directory
/// is created and an existing file/directory/symlink is never an output target.
/// This is not a sandbox against a hostile process replacing parent directories.
pub(super) struct Destination {
    path: PathBuf,
    parent: PathBuf,
    owner: AgentCx,
}

impl Destination {
    #[allow(clippy::needless_pass_by_value)] // used as `Option::map(Destination::prepare)`
    pub fn prepare(path: PathBuf) -> Result<Self> {
        let owner = AgentCx::for_current_or_request();
        authorized(&owner)?;
        let name = path
            .file_name()
            .ok_or_else(|| failure("HANDOFF_EXPORT_PATH"))?;
        let parent = path
            .parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .filter(|parent| parent.is_dir())
            .ok_or_else(|| failure("HANDOFF_EXPORT_PATH"))?;
        let path = parent.join(name);
        if path.to_str().is_none() {
            return Err(failure("HANDOFF_EXPORT_PATH"));
        }
        require_absent(&path)?;
        Ok(Self {
            path,
            parent,
            owner,
        })
    }

    pub fn stage(self, bytes: &[u8]) -> Result<Publication> {
        authorized(&self.owner)?;
        if bytes.len() > MAX_BUNDLE_BYTES {
            return Err(failure("HANDOFF_EXPORT_LIMIT"));
        }
        let mut file = tempfile::Builder::new()
            .prefix(".pi-handoff-")
            .tempfile_in(&self.parent)
            .map_err(|_| failure("HANDOFF_EXPORT_IO"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| failure("HANDOFF_EXPORT_IO"))?;
        }
        file.write_all(bytes)
            .map_err(|_| failure("HANDOFF_EXPORT_IO"))?;
        file.as_file()
            .sync_all()
            .map_err(|_| failure("HANDOFF_EXPORT_IO"))?;
        Ok(Publication {
            file,
            destination: self,
        })
    }
}

pub(super) struct Publication {
    file: tempfile::NamedTempFile,
    destination: Destination,
}

impl Publication {
    /// The export caller holds its source session lock after rechecking the
    /// snapshot. Import callers own a fully validated, immutable source buffer.
    pub fn publish(self) -> Result<String> {
        self.publish_named(
            "Saved handoff bundle",
            "No recipient was contacted and no work was resumed.",
        )
    }

    pub fn publish_session(self) -> Result<String> {
        self.publish_named("Created handoff session",
            "No model was called and the active session is unchanged. Resume the NEW file explicitly with pi --session; normal startup policies still apply.")
    }

    fn publish_named(self, label: &str, note: &str) -> Result<String> {
        authorized(&self.destination.owner)?;
        if std::fs::canonicalize(&self.destination.parent)
            .ok()
            .as_ref()
            != Some(&self.destination.parent)
        {
            return Err(failure("HANDOFF_EXPORT_PATH"));
        }
        require_absent(&self.destination.path)?;
        let display = serde_json::to_string(
            self.destination
                .path
                .to_str()
                .ok_or_else(|| failure("HANDOFF_EXPORT_PATH"))?,
        )?;
        let message = format!("{label}: {display}\n{note}");
        self.file
            .persist_noclobber(&self.destination.path)
            .map_err(|_| failure("HANDOFF_EXPORT_PUBLISH"))?;
        // No fallible operation follows publication. Cancellation afterwards
        // must not misreport an already-published artifact as absent.
        Ok(message)
    }
}

#[allow(clippy::needless_pass_by_value)] // callers in sibling modules pass an owned screened source
pub(super) fn document(
    input: &Snapshot,
    screened_source: Value,
    brief: &Brief,
    markdown: &str,
    model: &str,
    usage: &Value,
) -> Result<Value> {
    // Do not export raw session identifiers or the unscreened evidence. The
    // digest binds this source snapshot, but is not authentication or proof.
    let identity = serde_json::to_vec(&(&input.session_id, &input.leaf_id, &input.prompt))?;
    let digest = format!(
        "sha256:{}",
        crate::package_manager::hex_encode(&Sha256::digest(identity))
    );
    Ok(json!({
        "schema":"pi.handoff.bundle.v1",
        "generatedAt":chrono::Utc::now().to_rfc3339(),
        "model":model,
        "auxiliaryUsage":usage,
        "sourceSnapshotDigest":digest,
        "coverage":{
            "branchEntries":input.branch_entries,
            "includedEntries":input.sources.len(),
            "omittedEntries":input.omitted
        },
        "source":screened_source,
        "brief":brief,
        "markdown":markdown,
        "executionAuthorized":false,
        "warning":"Model synthesis with selected, redacted evidence; source membership is not semantic verification. Review omissions, actual results and the current user request before acting. No credentials or tool permissions are transferred."
    }))
}
