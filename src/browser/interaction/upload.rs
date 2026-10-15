//! Real file-input selection from bounded private copies of workspace files.
//! Copies stay owned by the session: browser File objects may read them later.

use super::{Document, References, element_call, resolve};
use crate::agent_cx::AgentCx;
use crate::browser::cdp::Cdp;
use crate::browser::{output, policy, required};
use crate::error::{Error, Result};
use crate::tools::ToolOutput;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

const MAX_FILES: usize = 10;
const MAX_CALL_BYTES: u64 = 20 * 1024 * 1024;
const MAX_SESSION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BATCHES: usize = 32;

#[derive(Default)]
pub(in crate::browser) struct Store {
    batches: Vec<Staged>,
    bytes: u64,
}

struct Staged {
    directory: tempfile::TempDir,
    paths: Vec<String>,
    files: Vec<FileInfo>,
    bytes: u64,
}

#[derive(Serialize)]
struct FileInfo {
    name: String,
    size: u64,
}

fn error(message: impl Into<String>) -> Error {
    Error::tool("browser", message)
}

pub(in crate::browser) fn validate(args: &Value) -> Result<Vec<PathBuf>> {
    let object = args
        .as_object()
        .ok_or_else(|| error("upload arguments must be an object"))?;
    for field in object.keys() {
        if !matches!(
            field.as_str(),
            "action" | "tab" | "frame" | "selector" | "files" | "timeout_ms"
        ) {
            return Err(error(format!("unsupported upload parameter: {field}")));
        }
    }
    let selector = required(args, "selector")?;
    if selector.is_empty() || selector.len() > 4096 || selector.contains('\0') {
        return Err(error(
            "upload selector must be nonempty and at most 4096 bytes",
        ));
    }
    let files = args.get("files").and_then(Value::as_array)
        .filter(|files| files.len() <= MAX_FILES)
        .ok_or_else(|| error("upload requires files: an array of at most 10 workspace-relative paths; [] clears the selection"))?;
    files
        .iter()
        .map(|value| {
            let path = value
                .as_str()
                .filter(|value| {
                    !value.is_empty()
                        && value.len() <= 4096
                        && !value.contains('\0')
                        && !value.contains('\\')
                })
                .ok_or_else(|| {
                    error("upload paths must be nonempty UTF-8 strings with slash separators")
                })?;
            let path = PathBuf::from(path);
            if path.is_absolute()
                || path.file_name().is_none()
                || path
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
            {
                return Err(error(
                    "uploads accept only workspace-relative files, without parent traversal",
                ));
            }
            Ok(path)
        })
        .collect()
}

impl Store {
    pub(in crate::browser) fn clear(&mut self) {
        self.batches.clear();
        self.bytes = 0;
    }

    pub(in crate::browser) async fn execute(
        &mut self,
        owner: &AgentCx,
        cdp: &mut Cdp,
        cwd: &Path,
        refs: Option<&References>,
        args: &Value,
        allowlist: Option<&[String]>,
    ) -> Result<ToolOutput> {
        let paths = validate(args)?;
        if !paths.is_empty() && self.batches.len() >= MAX_BATCHES {
            return Err(error(
                "upload staging reached 32 batches; finish pending transfers and stop/restart the browser session",
            ));
        }
        let selector = required(args, "selector")?;
        // Re-check the current document, not just the earlier target listing,
        // before any local bytes are read or exposed to the page.
        let before = allowed_document(owner, cdp, allowlist).await?;
        let node = resolve(owner, cdp, selector, refs)
            .await?
            .ok_or_else(|| error("upload selector did not match a file input"))?;
        let control = element_call(owner, cdp, node, "file_input", json!({})).await?;
        check_control(&control, paths.len())?;
        // Lookup and control inspection may themselves yield to navigation.
        // Refuse before staging, not only after local files have been copied.
        unchanged_document(owner, cdp, allowlist, &before).await?;
        let staged = if paths.is_empty() {
            None
        } else {
            Some(stage(
                owner,
                cwd,
                &paths,
                MAX_SESSION_BYTES.saturating_sub(self.bytes),
            )?)
        };
        unchanged_document(owner, cdp, allowlist, &before).await?;
        // Preparing private copies can take time. Revalidate the same backend
        // node rather than resolving a selector that could name a replacement.
        let control = element_call(owner, cdp, node, "file_input", json!({})).await?;
        check_control(&control, paths.len())?;
        unchanged_document(owner, cdp, allowlist, &before).await?;
        owner
            .checkpoint()
            .map_err(|_| error("upload cancelled before file selection"))?;
        let (sent, expected) = staged.as_ref().map_or_else(
            || (Vec::new(), json!([])),
            |staged| (staged.paths.clone(), json!(staged.files)),
        );
        if let Some(staged) = staged {
            self.bytes += staged.bytes;
            // Retain BEFORE sending: cancellation may happen after Chromium has
            // accepted the FileList but before its acknowledgement reaches us.
            self.batches.push(staged);
        }
        let selected = if paths.is_empty() {
            // Chromium 144 treats DOM.setFileInputFiles([]) as a no-op. Use
            // the isolated world's native setter and dispatch input/change.
            element_call(owner, cdp, node, "clear_files", json!({})).await?
        } else {
            cdp.command(
                owner,
                "DOM.setFileInputFiles",
                json!({
                    "backendNodeId": node, "files": sent
                }),
            )
            .await?;
            element_call(owner, cdp, node, "file_input", json!({})).await?
        };
        if selected.get("files") != Some(&expected) {
            return Err(error(
                "file input did not retain the requested names and sizes; the page may have replaced the selection",
            ));
        }
        if allowed_document(owner, cdp, allowlist).await? != before {
            return Err(error(
                "page navigated during file selection; file data may already have been consumed",
            ));
        }
        Ok(output(
            format!(
                "Selected {} file(s) for {selector}. Page scripts may read or upload them immediately; Pi did not click a submit button.",
                paths.len()
            ),
            json!({
                "action": "upload", "selector": selector, "files": expected,
                "file_count": paths.len(), "staged_session_bytes": self.bytes,
                "frame_id": before.frame,
                "backend": "cdp", "submit_button_clicked": false
            }),
        ))
    }
}

async fn allowed_document(
    owner: &AgentCx,
    cdp: &mut Cdp,
    allowlist: Option<&[String]>,
) -> Result<Document> {
    // Page.getFrameTree on the owning renderer describes its local root, not
    // necessarily the selected (same-process) descendant. frame_document also
    // checks every pinned ancestor and renderer incarnation for scoped calls.
    let frame = cdp.frame_document(owner).await?;
    admit_document(&frame, cdp.frame_selected(), allowlist)
}

fn admit_document(
    frame: &Value,
    scoped: bool,
    allowlist: Option<&[String]>,
) -> Result<Document> {
    let url = required(frame, "url")?;
    // A scoped srcdoc inherits the embedding document's authority, already
    // checked by frame_document. Never apply that exception to an unscoped root.
    if !scoped || url != "about:srcdoc" {
        policy::check_navigation(url, allowlist)?;
    }
    let id = |key: &str| -> Result<String> {
        frame[key]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or_else(|| error("upload document has an invalid identity"))
    };
    Ok(Document {
        frame: id("id")?,
        loader: id("loaderId")?,
    })
}

async fn unchanged_document(
    owner: &AgentCx,
    cdp: &mut Cdp,
    allowlist: Option<&[String]>,
    expected: &Document,
) -> Result<()> {
    if allowed_document(owner, cdp, allowlist).await? != *expected {
        return Err(error(
            "page navigated while preparing upload; take a new snapshot",
        ));
    }
    Ok(())
}

fn check_control(control: &Value, count: usize) -> Result<()> {
    let multiple = control
        .get("multiple")
        .and_then(Value::as_bool)
        .ok_or_else(|| error("file input did not expose its multiple attribute"))?;
    let directory = control
        .get("directory")
        .and_then(Value::as_bool)
        .ok_or_else(|| error("file input did not expose its directory attribute"))?;
    if directory {
        return Err(error(
            "directory-upload controls are not supported; select an ordinary file input",
        ));
    }
    if count > 1 && !multiple {
        return Err(error("this file input accepts only one file"));
    }
    Ok(())
}

fn stage(owner: &AgentCx, cwd: &Path, paths: &[PathBuf], budget: u64) -> Result<Staged> {
    owner
        .checkpoint()
        .map_err(|_| error("upload cancelled before reading local files"))?;
    if !owner.capabilities().io || !owner.capabilities().entropy {
        return Err(error(
            "upload staging requires I/O and entropy capabilities",
        ));
    }
    let limit = budget.min(MAX_CALL_BYTES);
    let directory = tempfile::Builder::new()
        .prefix("pi-browser-upload-")
        .tempdir()?;
    let mut staged = Staged {
        directory,
        paths: Vec::new(),
        files: Vec::new(),
        bytes: 0,
    };
    for (index, relative) in paths.iter().enumerate() {
        owner
            .checkpoint()
            .map_err(|_| error("upload cancelled while staging files"))?;
        let mut source = open_source(cwd, relative)?;
        let metadata = source.metadata()?;
        let remaining = limit.saturating_sub(staged.bytes);
        if !metadata.is_file() || metadata.len() > remaining {
            return Err(error(
                "upload inputs must be regular files within the 20 MiB call and 64 MiB session budgets",
            ));
        }
        let name = relative
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| error("upload file name must be UTF-8"))?;
        let folder = staged.directory.path().join(index.to_string());
        std::fs::create_dir(&folder)?;
        let destination = folder.join(name);
        let mut target = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)?;
        let mut copied = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            owner
                .checkpoint()
                .map_err(|_| error("upload cancelled while copying a file"))?;
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            copied = copied.saturating_add(u64::try_from(count).expect("buffer length fits u64"));
            if copied > remaining {
                return Err(error(
                    "upload input grew beyond its byte budget while being read",
                ));
            }
            target.write_all(&buffer[..count])?;
        }
        target.sync_all()?;
        staged.paths.push(
            destination
                .to_str()
                .ok_or_else(|| error("browser upload staging directory must have a UTF-8 path"))?
                .to_owned(),
        );
        staged.files.push(FileInfo {
            name: name.into(),
            size: copied,
        });
        staged.bytes += copied;
    }
    Ok(staged)
}

// Open every component through its pinned parent. After pinning the selected
// workspace root, neither a leaf symlink nor a parent swap redirects the read.
#[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
fn open_source(cwd: &Path, relative: &Path) -> Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let root = std::fs::canonicalize(cwd)?;
    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut directory = std::fs::File::from(
        open(Path::new("/"), directory_flags, Mode::empty()).map_err(std::io::Error::from)?,
    );
    for part in root.components() {
        if let Component::Normal(name) = part {
            directory = std::fs::File::from(
                openat(&directory, name, directory_flags, Mode::empty()).map_err(|_| {
                    error("upload workspace root could not be pinned without following links")
                })?,
            );
        }
    }
    let parts: Vec<_> = relative
        .components()
        .filter(|part| !matches!(part, Component::CurDir))
        .collect();
    for (index, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err(error("upload path escaped its workspace"));
        };
        let last = index + 1 == parts.len();
        let flags = if last {
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK
        } else {
            directory_flags
        };
        let file = std::fs::File::from(openat(&directory, *name, flags, Mode::empty()).map_err(
            |_| error("upload file is missing, inaccessible or passes through a symbolic link"),
        )?);
        if last {
            return Ok(file);
        }
        directory = file;
    }
    Err(error("upload path does not name a file"))
}

#[cfg(not(all(unix, not(any(target_os = "espidf", target_os = "redox")))))]
fn open_source(_cwd: &Path, _relative: &Path) -> Result<std::fs::File> {
    Err(error(
        "confined browser uploads are not implemented on this operating system",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_upload_arguments_keep_path_limits_and_reject_implicit_dialog_consent() {
        let args = json!({"action":"upload","tab":"work","frame":"child",
            "selector":"#file","files":["documents/report.txt"]});
        assert_eq!(validate(&args).unwrap(), [PathBuf::from("documents/report.txt")]);
        for field in ["dialog_response", "diagnostics", "url", "wait_after"] {
            let mut invalid = args.clone();
            invalid[field] = json!({});
            assert!(validate(&invalid).is_err(), "{field}");
        }
        for files in [json!(["../private"]), json!(["/private"]), json!(vec!["x"; 11])] {
            let mut invalid = args.clone();
            invalid["files"] = files;
            assert!(validate(&invalid).is_err());
        }
    }

    #[test]
    fn scoped_srcdoc_uses_inherited_admission_not_an_unscoped_root_exception() {
        let srcdoc = json!({"id":"child","loaderId":"child-loader","url":"about:srcdoc"});
        assert_eq!(admit_document(&srcdoc, true, Some(&[])).unwrap(), Document {
            frame: "child".into(), loader: "child-loader".into(),
        });
        assert!(admit_document(&srcdoc, false, Some(&[])).is_err());
        let rules = vec!["example.com".to_owned()];
        let blocked = json!({"id":"child","loaderId":"child-loader","url":"https://blocked.test/file"});
        assert!(admit_document(&blocked, true, Some(&rules)).is_err());
    }

    #[test]
    fn uploads_require_complete_bounded_document_identity() {
        let frame = json!({"id":"child","loaderId":"child-loader","url":"about:blank"});
        assert!(admit_document(&frame, false, None).is_ok());
        for key in ["id", "loaderId"] {
            for value in [Value::Null, json!(""), json!("x".repeat(257)), json!("bad\nidentity")] {
                let mut invalid = frame.clone();
                invalid[key] = value;
                assert!(admit_document(&invalid, true, None).is_err());
            }
        }
    }

    #[test]
    fn file_paths_and_control_cardinality_fail_closed() {
        for files in [
            json!("file.txt"),
            json!(["../private"]),
            json!(["/etc/passwd"]),
            json!(["a/../../private"]),
            json!(["a\\private"]),
            json!([false]),
        ] {
            assert!(
                validate(&json!({"action":"upload","selector":"#input","files":files})).is_err()
            );
        }
        assert!(
            validate(&json!({"action":"upload","selector":"#input","files":[]}))
                .unwrap()
                .is_empty()
        );
        assert!(check_control(&json!({"multiple":false,"directory":false}), 2).is_err());
        assert!(check_control(&json!({"multiple":true,"directory":true}), 1).is_err());
        assert!(check_control(&json!({"multiple":true,"directory":false}), 2).is_ok());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn private_copies_preserve_order_names_bytes_and_outlive_source_changes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("other")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"first").unwrap();
        std::fs::write(dir.path().join("other/a.txt"), b"second").unwrap();
        let staged = stage(
            &AgentCx::for_request(),
            dir.path(),
            &["a.txt".into(), "other/a.txt".into()],
            64,
        )
        .unwrap();
        std::fs::write(dir.path().join("a.txt"), b"changed").unwrap();
        assert_eq!(std::fs::read(&staged.paths[0]).unwrap(), b"first");
        assert_eq!(std::fs::read(&staged.paths[1]).unwrap(), b"second");
        assert_ne!(staged.paths[0], staged.paths[1]);
        assert_eq!(staged.files[0].name, "a.txt");
        assert_eq!(staged.bytes, 11);
        assert!(
            stage(
                &AgentCx::for_request(),
                dir.path(),
                &["other/a.txt".into()],
                5
            )
            .is_err()
        );
        let temporary = staged.directory.path().to_path_buf();
        let mut store = Store {
            batches: vec![staged],
            bytes: 11,
        };
        assert!(temporary.is_dir());
        store.clear();
        assert!(!temporary.exists());
    }

    #[cfg(all(unix, not(any(target_os = "espidf", target_os = "redox"))))]
    #[test]
    fn links_directories_and_cancelled_reads_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("private"), b"secret").unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        symlink(outside.path().join("private"), root.path().join("leaf")).unwrap();
        for path in ["link/private", "leaf", "."] {
            assert!(stage(&AgentCx::for_request(), root.path(), &[path.into()], 1024).is_err());
        }
        let owner = AgentCx::for_request();
        owner.cancel_with(asupersync::types::CancelKind::User, Some("before upload"));
        assert!(stage(&owner, root.path(), &[], 1024).is_err());
    }
}
