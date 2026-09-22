//! Capability-scoped filesystem connector.
//!
//! Reads are bounded by the tool read limit; writes accept at most 16 MiB of
//! decoded data. Unix read/write handles are opened through no-follow pinned
//! parents and validated before data I/O. Writes remain in-place, not atomic
//! transactions; storage failure can leave a partial update. Other operations
//! and concurrent directory relocation require additional race hardening.

use super::{
    CapabilityManifest, Error, ExtensionPolicy, FsConnector, FsOp, FsScopes, HostCallError,
    HostCallErrorCode, HostCallPayload, HostResultPayload, PolicyDecision, Result,
    strip_unc_prefix,
};
use base64::Engine as _;
use serde_json::{Value, json};
use sha2::Digest as _;
use std::borrow::Cow;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

// ============================================================================
// Connectors
// ============================================================================

impl FsOp {
    pub(super) fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("read") {
            Some(Self::Read)
        } else if value.eq_ignore_ascii_case("write") {
            Some(Self::Write)
        } else if value.eq_ignore_ascii_case("list") || value.eq_ignore_ascii_case("readdir") {
            Some(Self::List)
        } else if value.eq_ignore_ascii_case("stat") {
            Some(Self::Stat)
        } else if value.eq_ignore_ascii_case("mkdir") {
            Some(Self::Mkdir)
        } else if value.eq_ignore_ascii_case("delete")
            || value.eq_ignore_ascii_case("remove")
            || value.eq_ignore_ascii_case("rm")
        {
            Some(Self::Delete)
        } else {
            None
        }
    }

    pub(super) const fn required_capability(self) -> &'static str {
        match self {
            Self::Read | Self::List | Self::Stat => "read",
            Self::Write | Self::Mkdir | Self::Delete => "write",
        }
    }
}

impl FsScopes {
    pub fn least_privilege_for_cwd(cwd: &Path) -> Result<Self> {
        let root = canonicalize_root(cwd)?;
        Ok(Self {
            // Least-privilege default for unknown/new extensions: read-only project access.
            read_declared: true,
            write_declared: false,
            read_roots: vec![root],
            write_roots: Vec::new(),
        })
    }

    pub fn for_cwd(cwd: &Path) -> Result<Self> {
        let root = canonicalize_root(cwd)?;
        Ok(Self {
            read_declared: true,
            write_declared: true,
            read_roots: vec![root.clone()],
            write_roots: vec![root],
        })
    }

    pub fn from_manifest(manifest: Option<&CapabilityManifest>, cwd: &Path) -> Result<Self> {
        let Some(manifest) = manifest else {
            return Self::least_privilege_for_cwd(cwd);
        };

        let mut read_declared = false;
        let mut write_declared = false;
        let mut read_roots = Vec::new();
        let mut write_roots = Vec::new();

        for req in &manifest.capabilities {
            let cap = req.capability.trim().to_ascii_lowercase();
            if cap != "read" && cap != "write" {
                continue;
            }
            if cap == "read" {
                read_declared = true;
            } else {
                write_declared = true;
            }
            let Some(scope) = &req.scope else {
                continue;
            };
            let Some(paths) = &scope.paths else {
                continue;
            };

            for raw in paths {
                let root = resolve_scoped_root(raw, cwd)?;
                if cap == "read" {
                    read_roots.push(root);
                } else {
                    write_roots.push(root);
                }
            }
        }

        let fallback = canonicalize_root(cwd)?;
        if read_declared && read_roots.is_empty() {
            read_roots.push(fallback.clone());
        }
        if write_declared && write_roots.is_empty() {
            write_roots.push(fallback);
        }

        Ok(Self {
            read_declared,
            write_declared,
            read_roots,
            write_roots,
        })
    }

    fn roots_for_capability(&self, capability: &str) -> &[PathBuf] {
        if capability.eq_ignore_ascii_case("read") {
            if self.read_declared {
                &self.read_roots
            } else {
                &[]
            }
        } else if self.write_declared {
            &self.write_roots
        } else {
            &[]
        }
    }
}

impl FsConnector {
    pub fn new(cwd: impl AsRef<Path>, policy: ExtensionPolicy, scopes: FsScopes) -> Result<Self> {
        let cwd = canonicalize_root(cwd.as_ref())?;
        Ok(Self {
            cwd,
            policy,
            scopes,
        })
    }

    pub fn handle_host_call(
        &self,
        call: &HostCallPayload,
        extension_id: Option<&str>,
    ) -> HostResultPayload {
        if !call.method.trim().eq_ignore_ascii_case("fs") {
            return HostResultPayload {
                call_id: call.call_id.clone(),
                output: json!({}),
                is_error: true,
                error: Some(HostCallError {
                    code: HostCallErrorCode::InvalidRequest,
                    message: "Unsupported hostcall method for FsConnector".to_string(),
                    details: Some(json!({ "method": call.method })),
                    retryable: None,
                }),
                chunk: None,
            };
        }

        let result = self.handle_fs_params(&call.params, extension_id);
        match result {
            Ok(output) => HostResultPayload {
                call_id: call.call_id.clone(),
                output,
                is_error: false,
                error: None,
                chunk: None,
            },
            Err(error) => HostResultPayload {
                call_id: call.call_id.clone(),
                output: json!({}),
                is_error: true,
                error: Some(error),
                chunk: None,
            },
        }
    }

    fn handle_fs_params(
        &self,
        params: &Value,
        extension_id: Option<&str>,
    ) -> std::result::Result<Value, HostCallError> {
        let op = params
            .get("op")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let op = FsOp::parse(op).ok_or_else(|| HostCallError {
            code: HostCallErrorCode::InvalidRequest,
            message: "Invalid fs op".to_string(),
            details: Some(json!({ "op": op })),
            retryable: None,
        })?;

        let capability = op.required_capability();
        let policy_check = self.policy.evaluate_for(capability, extension_id);
        if policy_check.decision != PolicyDecision::Allow {
            return Err(HostCallError {
                code: HostCallErrorCode::Denied,
                message: "Capability denied by policy".to_string(),
                details: Some(json!({
                    "capability": policy_check.capability,
                    "decision": format!("{:?}", policy_check.decision),
                    "reason": policy_check.reason,
                })),
                retryable: None,
            });
        }

        let roots = self.scopes.roots_for_capability(capability);
        if roots.is_empty() {
            return Err(HostCallError {
                code: HostCallErrorCode::Denied,
                message: format!("No allowed roots configured for '{capability}'"),
                details: Some(json!({
                    "capability": capability,
                    "hint": "Declare capability_manifest scope.paths for this capability."
                })),
                retryable: None,
            });
        }

        let path_str = params
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .ok_or_else(|| HostCallError {
                code: HostCallErrorCode::InvalidRequest,
                message: "Missing fs path".to_string(),
                details: None,
                retryable: None,
            })?;

        let target = resolve_target_path(&self.cwd, path_str)?;

        let canonical_target = match op {
            // Unlink and lstat operate on the directory entry, not the object
            // a final symlink names. Canonicalizing that leaf can delete the
            // target (including a whole directory tree) instead of the link.
            FsOp::Delete => canonicalize_leaf_nofollow(&target),
            FsOp::Stat if params.get("follow_symlinks").and_then(Value::as_bool) == Some(false) => {
                canonicalize_leaf_nofollow(&target)
            }
            FsOp::Read | FsOp::List | FsOp::Stat => canonicalize_existing(&target),
            FsOp::Write | FsOp::Mkdir => canonicalize_for_create(&target),
        }?;

        // bd-cv653.3.12: the containment decision routes through the same
        // helper as tool enforcement so prefix semantics cannot drift.
        let matched_root = if crate::workspace::any_root_contains(roots, &canonical_target) {
            roots
                .iter()
                .find(|root| canonical_target.starts_with(root.as_path()))
        } else {
            None
        };

        if matched_root.is_none() {
            let root_hashes = roots.iter().map(|root| hash_path(root)).collect::<Vec<_>>();
            tracing::warn!(
                event = "ext.fs.denied",
                op = ?op,
                capability = capability,
                path_hash = %hash_path(&canonical_target),
                scope_roots = ?root_hashes,
                "Denied fs operation outside allowlist",
            );
            return Err(HostCallError {
                code: HostCallErrorCode::Denied,
                message: "Path outside allowed scope; update capability_manifest scope.paths"
                    .to_string(),
                details: Some(json!({
                    "capability": capability,
                    "path_hash": hash_path(&canonical_target),
                    "scope_roots": root_hashes,
                    "hint": "Add an allowed path to capability_manifest scope.paths."
                })),
                retryable: None,
            });
        }

        // A directory scope grants access to its contents, not permission to
        // remove the scope itself. File-scoped grants still permit unlinking
        // that file, and a symlink to a root is unlinked rather than followed.
        if matches!(op, FsOp::Delete)
            && roots.iter().any(|root| root == &canonical_target)
            && fs::symlink_metadata(&canonical_target)
                .map_err(|err| fs_path_error("stat", &canonical_target, &err))?
                .is_dir()
        {
            return Err(HostCallError {
                code: HostCallErrorCode::Denied,
                message: "Cannot delete an allowed scope root directory".to_string(),
                details: Some(json!({ "path_hash": hash_path(&canonical_target) })),
                retryable: None,
            });
        }

        let matched_root_hash = matched_root.map(|root| hash_path(root)).unwrap_or_default();
        tracing::info!(
            event = "ext.fs.call",
            op = ?op,
            capability = capability,
            path_hash = %hash_path(&canonical_target),
            scope_root = %matched_root_hash,
            "Executing fs operation",
        );

        match op {
            FsOp::Read => fs_op_read(params, &canonical_target),
            FsOp::Write => fs_op_write(params, &canonical_target),
            FsOp::List => fs_op_list(&canonical_target),
            FsOp::Stat => fs_op_stat(params, &canonical_target),
            FsOp::Mkdir => fs_op_mkdir(&canonical_target),
            FsOp::Delete => fs_op_delete(params, &canonical_target),
        }
    }
}

fn resolve_target_path(cwd: &Path, raw: &str) -> std::result::Result<PathBuf, HostCallError> {
    if raw.is_empty() {
        return Err(HostCallError {
            code: HostCallErrorCode::InvalidRequest,
            message: "Path is empty".to_string(),
            details: None,
            retryable: None,
        });
    }

    let path = Path::new(raw);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    })
}

fn canonicalize_root(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .map(strip_unc_prefix)
        .map_err(|err| Error::extension(format!("canonicalize: {err}")))
}

fn resolve_scoped_root(raw: &str, cwd: &Path) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::validation("Capability scope path is empty"));
    }

    let path = Path::new(raw);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };

    canonicalize_root(&resolved)
}

fn canonicalize_existing(path: &Path) -> std::result::Result<PathBuf, HostCallError> {
    std::fs::canonicalize(path)
        .map(strip_unc_prefix)
        .map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("canonicalize: {err}"),
            details: Some(json!({ "path": path.display().to_string() })),
            retryable: None,
        })
}

fn canonicalize_for_create(path: &Path) -> std::result::Result<PathBuf, HostCallError> {
    // Resolve every existing component as it becomes reachable. Merely
    // canonicalizing an ancestor and normalizing a missing suffix is unsafe:
    // missing/../outside-link/file can expose a symlink *after* normalization.
    // Do not create anything while deciding which scope authorizes the path.
    use std::path::Component;

    let mut resolved = PathBuf::new();
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    return Err(HostCallError {
                        code: HostCallErrorCode::Denied,
                        message: "Path escapes filesystem root".to_string(),
                        details: None,
                        retryable: None,
                    });
                }
            }
            Component::Normal(part) => {
                resolved.push(part);
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        // A dangling link is an error, not an absent path.
                        resolved = canonicalize_existing(&resolved)?;
                        if components.peek().is_some()
                            && !fs::metadata(&resolved)
                                .map_err(|err| fs_path_error("stat", &resolved, &err))?
                                .is_dir()
                        {
                            return Err(HostCallError {
                                code: HostCallErrorCode::InvalidRequest,
                                message: "An intermediate path component is not a directory"
                                    .to_string(),
                                details: None,
                                retryable: None,
                            });
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(fs_path_error("stat", &resolved, &err)),
                }
            }
        }
    }
    Ok(resolved)
}

fn canonicalize_leaf_nofollow(path: &Path) -> std::result::Result<PathBuf, HostCallError> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return canonicalize_existing(path);
    };
    let target = canonicalize_existing(parent)?.join(name);
    fs::symlink_metadata(&target).map_err(|err| fs_path_error("stat", &target, &err))?;
    Ok(target)
}

fn fs_path_error(operation: &str, path: &Path, err: &std::io::Error) -> HostCallError {
    HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("{operation}: {err}"),
        details: Some(json!({ "path_hash": hash_path(path) })),
        retryable: None,
    }
}

fn hash_path(path: &Path) -> String {
    let mut hasher = sha2::Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    crate::package_manager::hex_encode(&digest)
}

const FS_WRITE_MAX_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
enum FsEncoding {
    Utf8,
    Base64,
}

fn invalid_fs_request(message: &str) -> HostCallError {
    HostCallError {
        code: HostCallErrorCode::InvalidRequest,
        message: message.to_string(),
        details: None,
        retryable: None,
    }
}

fn fs_encoding(params: &Value) -> std::result::Result<FsEncoding, HostCallError> {
    let encoding = match params.get("encoding") {
        None => return Ok(FsEncoding::Utf8),
        Some(Value::String(value)) => value.trim(),
        Some(_) => {
            return Err(invalid_fs_request(
                "Invalid encoding; expected utf8 or base64",
            ));
        }
    };
    if encoding.eq_ignore_ascii_case("utf8") || encoding.eq_ignore_ascii_case("utf-8") {
        Ok(FsEncoding::Utf8)
    } else if encoding.eq_ignore_ascii_case("base64") {
        Ok(FsEncoding::Base64)
    } else {
        Err(invalid_fs_request(
            "Invalid encoding; expected utf8 or base64",
        ))
    }
}

fn write_size_error(limit: usize) -> HostCallError {
    HostCallError {
        code: HostCallErrorCode::InvalidRequest,
        message: format!("FS_WRITE_TOO_LARGE: decoded write data exceeds {limit} bytes"),
        details: Some(json!({ "max_bytes": limit })),
        retryable: None,
    }
}

fn write_data(params: &Value, limit: usize) -> std::result::Result<Cow<'_, [u8]>, HostCallError> {
    let encoding = fs_encoding(params)?;
    let data = params
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_fs_request("Missing write data"))?;
    match encoding {
        FsEncoding::Utf8 => {
            if data.len() > limit {
                return Err(write_size_error(limit));
            }
            // A UTF-8 write already owns these bytes in the hostcall payload.
            Ok(Cow::Borrowed(data.as_bytes()))
        }
        FsEncoding::Base64 => {
            // Reject before allocating the decoded buffer. Padding can make
            // the last quartet exceed the exact byte limit by up to two, so
            // retain the decoded-size check as well.
            if data.len() > limit.div_ceil(3).saturating_mul(4) {
                return Err(write_size_error(limit));
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .map_err(|_| invalid_fs_request("Invalid base64 write data"))?;
            if bytes.len() > limit {
                return Err(write_size_error(limit));
            }
            Ok(Cow::Owned(bytes))
        }
    }
}

#[cfg(unix)]
fn open_io_parent(path: &Path, create: bool) -> std::io::Result<(fs::File, &std::ffi::OsStr)> {
    use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
    use std::path::Component;

    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "expected an absolute normalized file path",
        )
    };
    let parent = path.parent().ok_or_else(invalid)?;
    let name = path.file_name().ok_or_else(invalid)?;
    if !path.is_absolute()
        || parent
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(invalid());
    }

    // Resolve in-scope symlinks once in the authorization phase. A symlink
    // introduced afterwards must fail here instead of redirecting the I/O.
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = fs::File::from(open("/", flags, Mode::empty())?);
    for component in parent.components() {
        let Component::Normal(part) = component else {
            continue;
        };
        let next = match openat(&directory, part, flags, Mode::empty()) {
            Err(rustix::io::Errno::NOENT) if create => {
                match mkdirat(&directory, part, Mode::from_raw_mode(0o777)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                // Also checks a racing creator: directories only, no links.
                openat(&directory, part, flags, Mode::empty())?
            }
            result => result?,
        };
        directory = fs::File::from(next);
    }
    Ok((directory, name))
}

#[cfg(unix)]
fn open_io_file(path: &Path, write: bool) -> std::io::Result<fs::File> {
    use rustix::fs::{Mode, OFlags, openat};

    let (parent, name) = open_io_parent(path, write)?;
    let access = if write {
        OFlags::WRONLY | OFlags::CREATE
    } else {
        OFlags::RDONLY
    };
    let flags = access | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY;
    // In particular, no O_TRUNC. The opened object's metadata is checked by
    // open_regular_file before any destructive update. NONBLOCK prevents a
    // substituted FIFO from waiting for an untrusted peer during open.
    Ok(fs::File::from(openat(
        &parent,
        name,
        flags,
        Mode::from_raw_mode(0o666),
    )?))
}

#[cfg(not(unix))]
fn open_io_file(path: &Path, write: bool) -> std::io::Result<fs::File> {
    if write
        && let Some(parent) = path.parent()
    {
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.read(!write).write(write).create(write);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        // SECURITY_ANONYMOUS: arbitrary paths must not grant named-pipe
        // servers the ability to impersonate this process. Rust adds SQOS_PRESENT.
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .security_qos_flags(0);
    }
    // The Windows leaf is protected, but this is not a directory-pinned walk
    // on non-Unix targets; it does not claim to close parent reparse races.
    options.open(path)
}

fn regular_file_metadata(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return false;
        }
    }
    meta.is_file() && !meta.file_type().is_symlink()
}

fn open_regular_file(
    path: &Path,
    write: bool,
) -> std::result::Result<(fs::File, fs::Metadata), HostCallError> {
    // Reject known special files before opening them, but never use this
    // path-based observation as a substitute for checking the actual handle.
    match fs::symlink_metadata(path) {
        Ok(meta) if !regular_file_metadata(&meta) => {
            return Err(invalid_fs_request("Path is not a regular file"));
        }
        Ok(_) => {}
        Err(error) if write && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(fs_path_error("stat", path, &error)),
    }
    let file = open_io_file(path, write).map_err(|error| fs_path_error("open", path, &error))?;
    validate_opened_file(path, file)
}

fn validate_opened_file(
    path: &Path,
    file: fs::File,
) -> std::result::Result<(fs::File, fs::Metadata), HostCallError> {
    let meta = file
        .metadata()
        .map_err(|error| fs_path_error("fstat", path, &error))?;
    if !regular_file_metadata(&meta) {
        return Err(invalid_fs_request("Opened object is not a regular file"));
    }
    Ok((file, meta))
}

fn fs_op_read(params: &Value, path: &Path) -> std::result::Result<Value, HostCallError> {
    let encoding = fs_encoding(params)?;
    let (file, meta) = open_regular_file(path, false)?;
    if meta.len() > crate::tools::READ_TOOL_MAX_BYTES {
        return Err(HostCallError {
            code: HostCallErrorCode::Io,
            message: format!(
                "File is too large ({} bytes). Max allowed is {} bytes.",
                meta.len(),
                crate::tools::READ_TOOL_MAX_BYTES
            ),
            details: None,
            retryable: None,
        });
    }

    let mut bytes = Vec::new();
    file.take(crate::tools::READ_TOOL_MAX_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("read: {err}"),
            details: None,
            retryable: None,
        })?;

    if bytes.len() as u64 > crate::tools::READ_TOOL_MAX_BYTES {
        return Err(HostCallError {
            code: HostCallErrorCode::Io,
            message: format!(
                "File is too large (exceeds max allowed {} bytes).",
                crate::tools::READ_TOOL_MAX_BYTES
            ),
            details: None,
            retryable: None,
        });
    }

    match encoding {
        FsEncoding::Utf8 => {
            let text = String::from_utf8(bytes).map_err(|_| HostCallError {
                code: HostCallErrorCode::InvalidRequest,
                message: "File is not valid UTF-8; use base64 encoding".to_string(),
                details: Some(json!({ "encoding": "base64" })),
                retryable: None,
            })?;
            Ok(json!({ "encoding": "utf8", "text": text }))
        }
        FsEncoding::Base64 => {
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            Ok(json!({ "encoding": "base64", "data": data }))
        }
    }
}

fn fs_op_write(params: &Value, path: &Path) -> std::result::Result<Value, HostCallError> {
    // Validate size/encoding before parent creation or opening the destination.
    let bytes = write_data(params, FS_WRITE_MAX_BYTES)?;
    let (mut file, _) = open_regular_file(path, true)?;
    file.set_len(0)
        .map_err(|error| fs_path_error("truncate", path, &error))?;
    file.write_all(&bytes)
        .map_err(|error| fs_path_error("write", path, &error))?;

    Ok(json!({ "bytes_written": bytes.len() }))
}

fn fs_op_list(path: &Path) -> std::result::Result<Value, HostCallError> {
    let read_dir = fs::read_dir(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("read_dir: {err}"),
        details: None,
        retryable: None,
    })?;

    let mut entries = Vec::new();
    for entry in read_dir {
        if entries.len() >= crate::tools::LS_SCAN_HARD_LIMIT {
            return Err(HostCallError {
                code: HostCallErrorCode::Io,
                message: format!(
                    "Directory scan limit reached ({} entries).",
                    crate::tools::LS_SCAN_HARD_LIMIT
                ),
                details: None,
                retryable: None,
            });
        }

        let entry = entry.map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("read_dir entry: {err}"),
            details: None,
            retryable: None,
        })?;
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = fs::symlink_metadata(entry.path()).map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("metadata: {err}"),
            details: None,
            retryable: None,
        })?;
        let kind = if meta.file_type().is_symlink() {
            "symlink"
        } else if meta.is_dir() {
            "dir"
        } else if meta.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(json!({ "name": name, "kind": kind }));
    }

    Ok(json!({ "entries": entries }))
}

fn fs_op_stat(params: &Value, path: &Path) -> std::result::Result<Value, HostCallError> {
    let follow = params
        .get("follow_symlinks")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let meta = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    }
    .map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("stat: {err}"),
        details: None,
        retryable: None,
    })?;

    Ok(json!({
        "is_file": meta.is_file(),
        "is_dir": meta.is_dir(),
        "is_symlink": meta.file_type().is_symlink(),
        "len": meta.len(),
    }))
}

fn fs_op_mkdir(path: &Path) -> std::result::Result<Value, HostCallError> {
    fs::create_dir_all(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("mkdir: {err}"),
        details: None,
        retryable: None,
    })?;
    Ok(json!({ "created": true }))
}

fn fs_op_delete(params: &Value, path: &Path) -> std::result::Result<Value, HostCallError> {
    let recursive = params
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let meta = fs::symlink_metadata(path).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("stat: {err}"),
        details: None,
        retryable: None,
    })?;

    if meta.is_dir() && !meta.file_type().is_symlink() {
        if recursive {
            fs::remove_dir_all(path)
        } else {
            fs::remove_dir(path)
        }
        .map_err(|err| HostCallError {
            code: HostCallErrorCode::Io,
            message: format!("remove_dir: {err}"),
            details: None,
            retryable: None,
        })?;
        return Ok(json!({ "deleted": true, "kind": "dir" }));
    }

    remove_file_or_link(path, &meta).map_err(|err| HostCallError {
        code: HostCallErrorCode::Io,
        message: format!("remove_file: {err}"),
        details: None,
        retryable: None,
    })?;

    Ok(json!({ "deleted": true, "kind": "file" }))
}

fn remove_file_or_link(path: &Path, _meta: &fs::Metadata) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt as _;
        if _meta.file_type().is_symlink_dir() {
            return fs::remove_dir(path);
        }
    }
    fs::remove_file(path)
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn connector(root: &Path) -> FsConnector {
        let policy = ExtensionPolicy {
            default_caps: vec!["read".to_string(), "write".to_string()],
            deny_caps: Vec::new(),
            ..ExtensionPolicy::default()
        };
        FsConnector::new(root, policy, FsScopes::for_cwd(root).expect("scopes")).expect("connector")
    }

    fn call(connector: &FsConnector, params: Value) -> std::result::Result<Value, HostCallError> {
        connector.handle_fs_params(&params, Some("fs-path-test"))
    }

    #[test]
    fn create_resolves_missing_components_without_creating_cancelled_prefixes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let connector = connector(temp.path());
        call(
            &connector,
            json!({"op": "write", "path": "missing/../nested/file", "data": "content"}),
        )
        .expect("write");
        assert!(!temp.path().join("missing").exists());
        assert_eq!(fs::read(temp.path().join("nested/file")).unwrap(), b"content");
        assert_eq!(
            call(&connector, json!({"op": "read", "path": "nested/file"})).unwrap()["text"],
            "content"
        );
    }

    #[test]
    fn create_rejects_an_existing_file_as_a_parent_before_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("file"), b"sentinel").unwrap();
        let error = call(
            &connector(temp.path()),
            json!({"op": "write", "path": "file/../new", "data": "bad"}),
        )
        .expect_err("not a directory");
        assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
        assert!(!temp.path().join("new").exists());
        assert_eq!(fs::read(temp.path().join("file")).unwrap(), b"sentinel");
    }

    #[test]
    fn recursive_delete_cannot_remove_the_scope_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("sentinel"), b"keep").unwrap();
        let connector = connector(temp.path());
        for path in [".", "child/.."] {
            fs::create_dir_all(temp.path().join("child")).unwrap();
            let error = call(
                &connector,
                json!({"op": "delete", "path": path, "recursive": true}),
            )
            .expect_err("scope root protected");
            assert_eq!(error.code, HostCallErrorCode::Denied);
            assert_eq!(fs::read(temp.path().join("sentinel")).unwrap(), b"keep");
        }
        call(
            &connector,
            json!({"op": "delete", "path": "child", "recursive": true}),
        )
        .expect("subdirectory deletion still works");
    }

    #[cfg(unix)]
    #[test]
    fn create_cannot_hide_an_outside_symlink_behind_missing_dot_dot() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, root.join("portal")).unwrap();
        let connector = connector(&root);
        for op in ["write", "mkdir"] {
            let error = call(
                &connector,
                json!({"op": op, "path": "missing/../portal/new", "data": "escaped"}),
            )
            .expect_err("outside target denied after normalization");
            assert_eq!(error.code, HostCallErrorCode::Denied);
            assert!(!outside.join("new").exists());
            assert!(!root.join("missing").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_through_an_in_scope_symlink_still_works() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir(temp.path().join("real")).unwrap();
        symlink("real", temp.path().join("alias")).unwrap();
        call(
            &connector(temp.path()),
            json!({"op": "write", "path": "missing/../alias/new", "data": "allowed"}),
        )
        .expect("in-scope symlink");
        assert_eq!(fs::read(temp.path().join("real/new")).unwrap(), b"allowed");
    }

    #[cfg(unix)]
    #[test]
    fn deleting_file_and_directory_links_preserves_the_targets() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir(temp.path().join("real")).unwrap();
        fs::write(temp.path().join("real/keep"), b"sentinel").unwrap();
        symlink("real/keep", temp.path().join("file-link")).unwrap();
        symlink("real", temp.path().join("dir-link")).unwrap();
        let connector = connector(temp.path());
        for path in ["file-link", "dir-link"] {
            call(
                &connector,
                json!({"op": "delete", "path": path, "recursive": true}),
            )
            .expect("unlink only");
            assert!(fs::symlink_metadata(temp.path().join(path)).is_err());
            assert_eq!(fs::read(temp.path().join("real/keep")).unwrap(), b"sentinel");
        }
    }

    #[cfg(unix)]
    #[test]
    fn dangling_links_can_be_statted_and_unlinked_but_not_written_through() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        symlink("absent", temp.path().join("link")).unwrap();
        let connector = connector(temp.path());
        let stat = call(
            &connector,
            json!({"op": "stat", "path": "link", "follow_symlinks": false}),
        )
        .expect("lstat dangling link");
        assert_eq!(stat["is_symlink"], true);
        assert_eq!(stat["is_file"], false);
        assert_eq!(stat["is_dir"], false);
        assert!(call(&connector, json!({"op": "stat", "path": "link"})).is_err());
        assert!(
            call(
                &connector,
                json!({"op": "write", "path": "link", "data": "bad"}),
            )
            .is_err()
        );
        assert!(!temp.path().join("absent").exists());
        call(&connector, json!({"op": "delete", "path": "link"})).expect("unlink");
    }

    #[cfg(unix)]
    #[test]
    fn outside_leaf_links_are_safe_to_unlink_but_parent_links_do_not_grant_access() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep"), b"sentinel").unwrap();
        symlink(&outside, root.join("portal")).unwrap();
        symlink(outside.join("keep"), root.join("leaf")).unwrap();
        let connector = connector(&root);
        for params in [
            json!({"op": "read", "path": "leaf"}),
            json!({"op": "delete", "path": "portal/keep"}),
            json!({"op": "stat", "path": "portal/keep", "follow_symlinks": false}),
        ] {
            let error = call(&connector, params).expect_err("outside scope");
            assert_eq!(error.code, HostCallErrorCode::Denied);
        }
        call(&connector, json!({"op": "delete", "path": "leaf"})).expect("safe unlink");
        assert_eq!(fs::read(outside.join("keep")).unwrap(), b"sentinel");
    }

    #[test]
    fn utf8_write_limit_counts_bytes_and_borrows_the_payload() {
        let params = json!({"data": "é🦀", "encoding": " UTF-8 "});
        let data = write_data(&params, 6).expect("exact byte boundary");
        assert!(matches!(data, Cow::Borrowed(_)));
        assert_eq!(data.as_ref(), "é🦀".as_bytes());
        assert!(write_data(&params, 5).is_err());
        assert_eq!(write_data(&json!({"data": ""}), 0).unwrap().len(), 0);
    }

    #[test]
    fn base64_write_limits_check_encoded_and_decoded_lengths() {
        for limit in 0..=8 {
            let bytes = vec![0xa5; limit];
            let params = json!({
                "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                "encoding": "BASE64"
            });
            assert_eq!(write_data(&params, limit).unwrap().as_ref(), bytes.as_slice());
            let oversized = json!({
                "data": base64::engine::general_purpose::STANDARD.encode(vec![0xa5; limit + 1]),
                "encoding": "base64"
            });
            let error = write_data(&oversized, limit).expect_err("one byte over");
            assert!(error.message.contains("FS_WRITE_TOO_LARGE"));
        }
        assert!(write_data(&json!({"data": "!!!!", "encoding": "base64"}), 4).is_err());
    }

    #[test]
    fn invalid_encoding_and_base64_do_not_open_or_create_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("absent/parent/file");
        for encoding in [json!(null), json!(false), json!(7), json!("hex"), json!("")] {
            let params = json!({"encoding": encoding, "data": "must not write"});
            assert_eq!(
                fs_op_read(&params, &path).unwrap_err().code,
                HostCallErrorCode::InvalidRequest
            );
            assert_eq!(
                fs_op_write(&params, &path).unwrap_err().code,
                HostCallErrorCode::InvalidRequest
            );
        }
        assert!(fs_op_write(&json!({"encoding": "base64", "data": "!!!!"}), &path).is_err());
        assert!(!temp.path().join("absent").exists());
        let existing = temp.path().join("existing");
        fs::write(&existing, b"keep").unwrap();
        assert!(fs_op_write(&json!({"encoding": "base64", "data": "!!!!"}), &existing).is_err());
        assert_eq!(fs::read(&existing).unwrap(), b"keep");
    }

    #[test]
    fn production_write_cap_preserves_existing_bytes_and_absent_parents() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("existing"), b"keep").unwrap();
        let connector = connector(temp.path());
        let mut params = json!({"op": "write", "data": "x".repeat(FS_WRITE_MAX_BYTES + 1)});
        for path in ["existing", "absent/parent/new"] {
            params["path"] = json!(path);
            let error = connector
                .handle_fs_params(&params, None)
                .expect_err("write cap");
            assert!(error.message.contains("FS_WRITE_TOO_LARGE"));
            assert_eq!(error.details.unwrap()["max_bytes"], FS_WRITE_MAX_BYTES);
            assert_eq!(fs::read(temp.path().join("existing")).unwrap(), b"keep");
            assert!(!temp.path().join("absent").exists());
        }
    }

    #[test]
    fn binary_roundtrip_and_shorter_or_empty_overwrites_use_the_checked_handle() {
        let temp = tempfile::tempdir().expect("tempdir");
        let connector = connector(temp.path());
        let binary = [0, 255, 1, 13, 10, 128];
        let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
        let written = call(
            &connector,
            json!({
                "op": "write", "path": "nested/data", "encoding": "base64", "data": encoded
            }),
        )
        .expect("binary write");
        assert_eq!(written["bytes_written"], binary.len());
        let read = call(
            &connector,
            json!({"op": "read", "path": "nested/data", "encoding": "base64"}),
        )
        .expect("binary read");
        assert_eq!(read["data"], encoded);
        assert!(call(&connector, json!({"op": "read", "path": "nested/data"})).is_err());
        for replacement in ["ok", ""] {
            call(
                &connector,
                json!({"op": "write", "path": "nested/data", "data": replacement}),
            )
            .expect("overwrite");
            assert_eq!(
                fs::read(temp.path().join("nested/data")).unwrap(),
                replacement.as_bytes()
            );
        }
    }

    #[test]
    fn directory_targets_are_rejected_without_changing_their_contents() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::create_dir(temp.path().join("directory")).unwrap();
        fs::write(temp.path().join("directory/keep"), b"keep").unwrap();
        let connector = connector(temp.path());
        for op in ["read", "write"] {
            let error = call(
                &connector,
                json!({"op": op, "path": "directory", "data": "bad"}),
            )
            .expect_err("regular files only");
            assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
        }
        assert_eq!(fs::read(temp.path().join("directory/keep")).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn leaf_replaced_after_authorization_cannot_redirect_data_io() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target");
        let outside = temp.path().join("outside");
        fs::write(&target, b"original").unwrap();
        fs::write(&outside, b"secret").unwrap();
        let authorized = canonicalize_existing(&target).unwrap();
        fs::rename(&target, temp.path().join("original")).unwrap();
        symlink(&outside, &target).unwrap();
        assert!(fs_op_read(&json!({}), &authorized).is_err());
        assert!(fs_op_write(&json!({"data": "bad"}), &authorized).is_err());
        // Bypass the preliminary metadata observation: the actual open must
        // independently refuse a link inserted in the stat-to-open interval.
        assert!(open_io_file(&authorized, false).is_err());
        assert!(open_io_file(&authorized, true).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"secret");
        assert_eq!(fs::read(temp.path().join("original")).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn parent_replaced_after_authorization_cannot_redirect_reads_writes_or_mkdirs() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        let parent = temp.path().join("parent");
        let outside = temp.path().join("outside");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(parent.join("file"), b"original").unwrap();
        fs::write(outside.join("file"), b"secret").unwrap();
        let authorized = canonicalize_existing(&parent.join("file")).unwrap();
        let new_file = canonicalize_for_create(&parent.join("new/deep/file")).unwrap();
        fs::rename(&parent, temp.path().join("original-parent")).unwrap();
        symlink(&outside, &parent).unwrap();
        assert!(fs_op_read(&json!({}), &authorized).is_err());
        assert!(fs_op_write(&json!({"data": "bad"}), &authorized).is_err());
        assert!(fs_op_write(&json!({"data": "bad"}), &new_file).is_err());
        assert!(!outside.join("new").exists());
        assert_eq!(fs::read(outside.join("file")).unwrap(), b"secret");
        assert_eq!(
            fs::read(temp.path().join("original-parent/file")).unwrap(),
            b"original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn raw_write_open_does_not_truncate_before_validation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("file");
        fs::write(&path, b"sentinel").unwrap();
        let path = canonicalize_existing(&path).unwrap();
        let file = open_io_file(&path, true).expect("open without truncating");
        assert!(
            rustix::fs::fcntl_getfl(&file)
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK)
        );
        assert_eq!(file.metadata().unwrap().len(), 8);
        assert_eq!(fs::read(&path).unwrap(), b"sentinel");
    }

    #[cfg(unix)]
    #[test]
    fn unix_parent_walk_rejects_non_normalized_paths_before_creation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = canonicalize_existing(temp.path()).unwrap();
        assert!(open_io_parent(&root.join("absent/../file"), true).is_err());
        assert!(open_io_parent(Path::new("relative/parent/file"), true).is_err());
        assert!(!root.join("absent").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_open_is_nonblocking_and_handle_validation_rejects_it() {
        use rustix::fs::{CWD, FileType, Mode, OFlags, mknodat, open};
        use std::sync::mpsc;
        use std::time::Duration;

        let temp = tempfile::tempdir().expect("tempdir");
        let fifo = temp.path().join("fifo");
        mknodat(CWD, &fifo, FileType::Fifo, Mode::from_raw_mode(0o600), 0).unwrap();
        let fifo = canonicalize_existing(&fifo).unwrap();
        let worker_path = fifo.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = sender.send(open_io_file(&worker_path, false));
        });
        let result = receiver.recv_timeout(Duration::from_secs(2));
        // A regression removing NONBLOCK must fail, not hang the test suite:
        // opening both FIFO ends releases a reader stuck in open before join.
        let rescue = result.is_err().then(|| {
            open(
                &fifo,
                OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .expect("release blocked FIFO open")
        });
        worker.join().expect("FIFO open worker");
        let file = result
            .expect("open must not wait for a FIFO writer")
            .expect("nonblocking open");
        let error = validate_opened_file(&fifo, file).expect_err("fstat rejects special object");
        assert_eq!(error.code, HostCallErrorCode::InvalidRequest);
        drop(rescue);
        assert!(fs_op_read(&json!({}), &fifo).is_err());
        assert!(fs_op_write(&json!({"data": "bad"}), &fifo).is_err());
    }
}
