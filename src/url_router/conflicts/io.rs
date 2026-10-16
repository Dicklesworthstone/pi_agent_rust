//! Bounded, cancellable Git queries and regular-file snapshot admission.

use std::io::Read as _;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use crate::agent_cx::AgentCx;
use crate::tools::{ProcessCleanupMode, ProcessGuard};
use super::{Result, fail};

const QUERY_BYTES: u64 = 1024 * 1024;
pub(super) const FILE_BYTES: u64 = 4 * 1024 * 1024;

pub(super) struct Budget {
    owner: AgentCx,
    deadline: Instant,
}

impl Budget {
    pub(super) fn new() -> Result<Self> {
        let owner = AgentCx::for_current_or_request();
        let capabilities = owner.capabilities();
        if !capabilities.io || !capabilities.spawn || !capabilities.time {
            return Err(fail("PERMISSION", "conflict inspection requires I/O, process and timer authority"));
        }
        let now = owner.timer_driver().map_or_else(asupersync::time::wall_now, |timer| timer.now());
        let inherited = owner.budget().deadline.map_or(Duration::from_secs(10), |deadline| {
            Duration::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
        });
        let budget = Self { owner, deadline: Instant::now() + inherited.min(Duration::from_secs(10)) };
        budget.check()?;
        Ok(budget)
    }

    pub(super) fn check(&self) -> Result<()> {
        self.owner.checkpoint().map_err(|_| fail("CANCELLED", "conflict operation cancelled"))?;
        if Instant::now() >= self.deadline { return Err(fail("TIMEOUT", "conflict operation budget exhausted")); }
        Ok(())
    }

    pub(super) fn git(&self, cwd: &Path, args: &[&str]) -> Result<Vec<u8>> {
        self.check()?;
        let stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        let mut command = crate::tools::command_with_default_sigpipe_in_dir(std::ffi::OsStr::new("git"), cwd)?;
        command.args(args).current_dir(cwd).env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(std::process::Stdio::null())
            .stdout(stdout.try_clone()?).stderr(stderr.try_clone()?);
        crate::tools::isolate_command_process_group(&mut command);
        let child = self.owner.process().spawn_checked(&mut command)?;
        let mut child = ProcessGuard::new(child, ProcessCleanupMode::ProcessGroupTree);
        // Always reap on an error path, including cancellation and byte limits.
        let outcome = (|| {
            loop {
                self.check()?;
                if stdout.metadata()?.len() > QUERY_BYTES || stderr.metadata()?.len() > QUERY_BYTES {
                    return Err(fail("LIMIT", "Git conflict inventory exceeds 1 MiB"));
                }
                if let Some(status) = child.try_wait_child()? {
                    if !status.success() { return Err(fail("GIT", "Git could not inspect the working tree")); }
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })();
        let _ = child.terminate_and_wait();
        outcome?;
        self.check()?;
        use std::io::{Seek as _, SeekFrom};
        let mut stdout = stdout;
        stdout.seek(SeekFrom::Start(0))?;
        let mut data = Vec::new();
        stdout.take(QUERY_BYTES + 1).read_to_end(&mut data)?;
        if data.len() as u64 > QUERY_BYTES { return Err(fail("LIMIT", "Git conflict inventory exceeds 1 MiB")); }
        Ok(data)
    }
}

/// Reject symlink ancestors and nonregular sources before any content read.
/// Unix traversal is descriptor-relative and no-follow, including FIFO swaps.
/// Transactional publication later retains the existing LSP path-race limits.
pub(super) fn read(root: &Path, relative: &Path) -> Result<String> {
    if relative.as_os_str().is_empty() || relative.components().any(|part| !matches!(part, Component::Normal(_))) {
        return Err(fail("PATH", "conflict path is not a relative Git worktree entry"));
    }
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags, open, openat};
        let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
        let descriptor = open(root, flags | OFlags::DIRECTORY, Mode::empty()).map_err(std::io::Error::from)?;
        let mut file = std::fs::File::from(descriptor);
        let mut components = relative.components().peekable();
        while let Some(Component::Normal(name)) = components.next() {
            let flags = if components.peek().is_some() { flags | OFlags::DIRECTORY } else { flags };
            file = std::fs::File::from(openat(&file, name, flags, Mode::empty()).map_err(std::io::Error::from)?);
        }
        file
    };
    #[cfg(not(unix))]
    let file = {
        let mut path = root.to_path_buf();
        for part in relative.components() {
            path.push(part);
            if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(fail("PATH", "conflict paths cannot follow symbolic links"));
            }
        }
        std::fs::File::open(path)?
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() { return Err(fail("PATH", "conflict source is not a regular file")); }
    if metadata.len() > FILE_BYTES { return Err(fail("LIMIT", "conflict source exceeds 4 MiB")); }
    let mut bytes = Vec::new();
    file.take(FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_BYTES { return Err(fail("LIMIT", "conflict source exceeds 4 MiB")); }
    String::from_utf8(bytes).map_err(|_| fail("ENCODING", "conflict source is not UTF-8; resolve this binary conflict explicitly"))
}
