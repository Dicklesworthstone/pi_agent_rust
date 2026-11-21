//! Bounded, read-admitted instruction snapshots for native delegation.
//!
//! Admission resolves every requested file before any child starts. The runner
//! keeps the captured text through queueing, schema retries and explicit revival;
//! it never asks a child to reopen a path that could have changed meanwhile.

use std::ffi::OsString;
use std::fs::File;
use std::io::{Read as _, Seek as _, Write as _};
use std::path::Path;
use std::process::Stdio;

use sha2::{Digest as _, Sha256};

use super::{Deadline, Error, LaunchAdmission, Result, SubagentTask, SubagentTool, Value};
use crate::agent_cx::AgentCx;
use crate::file_identity::FileIdentity;

pub(super) const MAX_BYTES: usize = 4 * 1024 * 1024;
const CHUNK_BYTES: usize = 16 * 1024;

fn invalid(message: &str) -> Error {
    Error::tool("subagent", format!("PI_SUBAGENT_TASK_FILE: {message}"))
}

fn validate_text(text: &str) -> Result<()> {
    if text.len() > MAX_BYTES {
        return Err(Error::tool(
            "subagent",
            "PI_SUBAGENT_TASK_LIMIT: delegation instructions exceed 4 MiB",
        ));
    }
    if text.trim().is_empty() || text.contains('\0') {
        return Err(invalid(
            "task requires nonempty instructions without NUL bytes",
        ));
    }
    Ok(())
}

pub(super) fn validate_fields(input: &Value) -> Result<()> {
    if input.get("taskFileSha256").is_some() {
        return Err(invalid("taskFileSha256 is output metadata, not an input"));
    }
    let Some(value) = input.get("taskFile") else {
        return input
            .get("task")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("provide task or taskFile"))
            .and_then(validate_text);
    };
    if input.get("task").is_some() {
        return Err(invalid("provide task or taskFile, never both"));
    }
    let source = value
        .as_str()
        .filter(|source| {
            !source.is_empty() && source.len() <= 4096 && !source.chars().any(char::is_control)
        })
        .ok_or_else(|| invalid("taskFile must be a nonempty reference of at most 4096 bytes"))?;
    if let Some(name) = source.strip_prefix("local://") {
        if name.trim().is_empty() {
            return Err(invalid("local:// requires a scratch document name"));
        }
    } else if crate::url_router::has_scheme(source) || !Path::new(source).is_absolute() {
        return Err(invalid(
            "taskFile requires an absolute path or local:// scratch document",
        ));
    }
    Ok(())
}

pub(super) fn validate_request(input: &Value) -> Result<()> {
    let batch = input.get("tasks").is_some() || input.get("chain").is_some();
    if !batch {
        return validate_fields(input);
    }
    if input.get("taskFile").is_some() || input.get("taskFileSha256").is_some() {
        return Err(invalid(
            "taskFile belongs on each batch entry, not the batch",
        ));
    }
    for key in ["tasks", "chain"] {
        if let Some(entries) = input.get(key).and_then(Value::as_array) {
            for entry in entries {
                validate_fields(entry)?;
            }
        }
    }
    Ok(())
}

fn check(owner: &AgentCx, deadline: Deadline) -> Result<()> {
    owner.checkpoint().map_err(|_| Error::Aborted)?;
    if !owner.capabilities().io {
        return Err(invalid("reading task instructions requires I/O capability"));
    }
    deadline.check().map_err(invalid)
}

fn source_bytes(
    tool: &SubagentTool,
    source: &str,
    owner: &AgentCx,
    deadline: Deadline,
) -> Result<Vec<u8>> {
    if let Some(name) = source.strip_prefix("local://") {
        let text = crate::url_router::read_local_bounded(name, MAX_BYTES)
            .map_err(|error| invalid(&error.to_string()))?;
        check(owner, deadline)?;
        return Ok(text.into_bytes());
    }
    let source = Path::new(source);
    let path = crate::tools::enforce_read_scope_with_roots(
        source,
        &tool.cwd,
        &tool.global_dir,
        &tool.workspace,
    )?;
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES as u64 {
        return Err(invalid(
            "taskFile must resolve to a regular file of at most 4 MiB",
        ));
    }
    crate::session::ensure_session_file_readable(&path)?;
    let identity = FileIdentity::of_path_nofollow(&path)?;
    // A substituted FIFO must fail instead of waiting for a writer. Do not
    // follow a substituted final symlink after the scope check either.
    #[cfg(unix)]
    let mut file = File::from(
        rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    #[cfg(windows)]
    let mut file: File = {
        use std::os::windows::fs::OpenOptionsExt as _;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
            .open(&path)?
    };
    #[cfg(not(any(unix, windows)))]
    let mut file = File::open(&path)?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || opened.len() != metadata.len()
        || FileIdentity::of_open_file(&file)? != identity
        || crate::tools::enforce_read_scope_with_roots(
            source,
            &tool.cwd,
            &tool.global_dir,
            &tool.workspace,
        )? != path
    {
        return Err(invalid("taskFile changed while opening"));
    }
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; CHUNK_BYTES];
    loop {
        check(owner, deadline)?;
        let read = match file.read(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        if read == 0 {
            break;
        }
        if read > MAX_BYTES.saturating_sub(bytes.len()) {
            return Err(invalid("taskFile exceeds 4 MiB"));
        }
        let captured = chunk
            .get(..read)
            .ok_or_else(|| invalid("taskFile read exceeded its input buffer"))?;
        bytes.extend_from_slice(captured);
    }
    let after = file.metadata()?;
    if after.len() != opened.len()
        || after.modified()? != opened.modified()?
        || bytes.len() as u64 != opened.len()
        || FileIdentity::of_path_nofollow(&path)? != identity
    {
        return Err(invalid("taskFile changed while reading"));
    }
    check(owner, deadline)?;
    Ok(bytes)
}

pub(super) fn prepare<'a>(
    tool: &SubagentTool,
    tasks: impl IntoIterator<Item = &'a mut SubagentTask>,
    deadline: Deadline,
) -> Result<()> {
    let owner = AgentCx::for_current_or_request();
    for task in tasks {
        let Some(source) = &task.task_file else {
            validate_text(&task.task)?;
            continue;
        };
        // Re-admission is not a second read. Only this module can set the
        // digest; deserialization explicitly skips it.
        if task.task_file_sha256.is_some() {
            validate_text(&task.task)?;
            continue;
        }
        if !task.task.is_empty() {
            return Err(invalid("provide task or taskFile, never both"));
        }
        validate_fields(&serde_json::json!({"taskFile": source}))?;
        check(&owner, deadline)?;
        let bytes = source_bytes(tool, source, &owner, deadline)?;
        let digest = crate::package_manager::hex_encode(&Sha256::digest(&bytes));
        let text =
            String::from_utf8(bytes).map_err(|_| invalid("taskFile must contain valid UTF-8"))?;
        validate_text(&text)?;
        task.task = text;
        task.task_file_sha256 = Some(digest);
    }
    Ok(())
}

/// Revoking a live added root also fences queued children, schema retries and
/// explicit revival. The check never reopens or substitutes instruction bytes.
pub(super) fn admission<'a>(
    tool: &SubagentTool,
    tasks: impl IntoIterator<Item = &'a SubagentTask>,
) -> Option<LaunchAdmission> {
    let paths: Vec<_> = tasks
        .into_iter()
        .filter_map(|task| task.task_file.as_deref())
        .filter(|source| !source.starts_with("local://"))
        .map(std::path::PathBuf::from)
        .collect();
    if paths.is_empty() {
        return None;
    }
    let cwd = tool.cwd.clone();
    let global_dir = tool.global_dir.clone();
    let workspace = tool.workspace.clone();
    Some(std::sync::Arc::new(move || {
        for path in &paths {
            crate::tools::enforce_read_scope_with_roots(path, &cwd, &global_dir, &workspace)?;
        }
        Ok(())
    }))
}

/// Task text can exceed the OS's argv limit. A private anonymous
/// regular file supplies stdin without a blocking writer or a named file for
/// the child to race/reopen. Each retry gets its own cursor over its own text.
pub(super) fn stdin(task: &str, owner: &AgentCx, deadline: Deadline) -> Result<Stdio> {
    validate_text(task)?;
    check(owner, deadline)?;
    let mut file = tempfile::tempfile()?;
    file.write_all(b"Task: ")?;
    for chunk in task.as_bytes().chunks(CHUNK_BYTES) {
        check(owner, deadline)?;
        file.write_all(chunk)?;
    }
    file.rewind()?;
    check(owner, deadline)?;
    Ok(Stdio::from(file))
}

/// Keep the fully composed definition/schema prompt out of the command line.
/// Both prompt flags already accept a file path, so the child reads this private
/// snapshot instead of reopening an authored definition that may have changed.
/// The caller must retain the file until its process has been retired: spawning
/// alone does not mean the child has finished loading its startup arguments.
pub(super) fn snapshot_prompt(
    args: &mut [OsString],
    owner: &AgentCx,
    deadline: Deadline,
) -> Result<Option<tempfile::NamedTempFile>> {
    // child_options appends this pair last. Inspect that known position rather
    // than mistaking another option's literal value for a prompt flag.
    let Some(index) = args.len().checked_sub(2).filter(|&index| {
        args[index] == "--system-prompt" || args[index] == "--append-system-prompt"
    }) else {
        return Ok(None);
    };
    let prompt = args
        .get(index + 1)
        .and_then(|arg| arg.to_str())
        .filter(|prompt| prompt.len() <= MAX_BYTES && !prompt.contains('\0'))
        .ok_or_else(|| {
            Error::tool(
                "subagent",
                "PI_SUBAGENT_PROMPT_LIMIT: system instructions must be UTF-8 without NUL bytes and at most 4 MiB",
            )
        })?;
    check(owner, deadline)?;
    let mut file = tempfile::Builder::new()
        .prefix("pi-subagent-prompt-")
        .suffix(".txt")
        .tempfile()?;
    for chunk in prompt.as_bytes().chunks(CHUNK_BYTES) {
        check(owner, deadline)?;
        file.write_all(chunk)?;
    }
    file.flush()?;
    check(owner, deadline)?;
    // TMPDIR may itself be relative; an isolated child can have another cwd.
    args[index + 1] = std::fs::canonicalize(file.path())?.into_os_string();
    Ok(Some(file))
}

#[cfg(test)]
mod tests;
