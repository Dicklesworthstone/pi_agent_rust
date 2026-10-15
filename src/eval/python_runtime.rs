//! Discover Python before sending any cell, without changing retained kernels.
//!
//! Discovery is native and bounded; no shell, activation script, installer or
//! application code is used as a probe. The probe is not executable attestation.

use super::session::CellSession;
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};
use crate::tools::{ProcessCleanupMode, ProcessGuard};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MAX_CANDIDATES: usize = 128;
const PROBE_BUDGET: Duration = Duration::from_secs(2);
const DISCOVERY_BUDGET: Duration = Duration::from_secs(8);
const PROBE: &str = "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 78)";

impl super::EvalTool {
    /// Construct an independent eval instance with an explicit Python runtime.
    ///
    /// Unlike a process-wide `PI_EVAL_PYTHON` change, this selection applies
    /// only to this tool and its retained session kernels. Other environment
    /// inputs are captured as usual. A bare name uses the captured absolute
    /// PATH directories; a relative path is workspace-relative; `~/` expands.
    /// No shell is involved. A failed explicit selection never falls back to
    /// a different runtime. JavaScript behavior is unchanged.
    ///
    /// Construction validates path syntax and bounds, but does not spawn a
    /// process or require the interpreter to exist yet. The first Python cell
    /// performs the normal owned probe under its original hard deadline.
    ///
    /// # Errors
    ///
    /// Returns `EVAL_PY_INPUT` for empty/NUL-containing selections, unsupported
    /// drive-relative paths, unavailable home expansion, or discovery limits.
    pub fn new_with_python(
        cwd: &Path,
        interpreter: impl Into<OsString>,
    ) -> Result<Self> {
        let interpreter = interpreter.into();
        if interpreter.is_empty() {
            return Err(input_error("explicit SDK interpreter cannot be empty"));
        }
        let mut tool = Self::new(cwd);
        // This explicit constructor overrides the process setting, including
        // an invalid empty setting, without mutating the process environment.
        tool.python_environment.empty_override = false;
        tool.python_environment.candidates(&interpreter, cwd)?;
        tool.python_path = interpreter;
        Ok(tool)
    }
}

/// Capture host selection inputs once, like PI_EVAL_PYTHON itself. Filesystem
/// candidates are checked on each fresh spawn, not cached across namespaces.
#[derive(Clone, Debug)]
pub(super) struct Environment {
    path: Option<OsString>,
    virtual_env: Option<OsString>,
    conda_prefix: Option<OsString>,
    home: Option<PathBuf>,
    empty_override: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Explicit,
    ActiveVenv,
    Conda,
    DotVenv,
    Venv,
    Path,
}

impl Source {
    const fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::ActiveVenv => "active-venv",
            Self::Conda => "conda",
            Self::DotVenv => "project-dot-venv",
            Self::Venv => "project-venv",
            Self::Path => "path",
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Resolved {
    pub(super) executable: PathBuf,
    source: Source,
    root: Option<PathBuf>,
    path: OsString,
}

impl Resolved {
    pub(super) fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.env("PATH", &self.path)
            .env("PYTHONUNBUFFERED", "1")
            .env("PYTHONIOENCODING", "utf-8");
        if let Some(root) = &self.root {
            // Equivalent selection essentials, not a shell activation. Keep
            // PYTHONPATH and unrelated application variables unchanged.
            command.env_remove("PYTHONHOME");
            if self.source == Source::Conda {
                command.env("CONDA_PREFIX", root).env_remove("VIRTUAL_ENV");
            } else {
                command.env("VIRTUAL_ENV", root).env_remove("CONDA_PREFIX");
            }
        }
        command
    }

    pub(super) fn annotate(&self, output: &mut crate::tools::ToolOutput) {
        if let Some(serde_json::Value::Object(details)) = output.details.as_mut() {
            details.insert("pythonRuntime".into(), serde_json::json!({
                "executable": self.executable.to_string_lossy(),
                "source": self.source.label(),
            }));
        }
    }
}

impl Environment {
    pub(super) fn capture() -> (OsString, Self) {
        let requested = std::env::var_os("PI_EVAL_PYTHON");
        let environment = Self {
            path: std::env::var_os("PATH"),
            virtual_env: std::env::var_os("VIRTUAL_ENV").filter(|v| !v.is_empty()),
            conda_prefix: std::env::var_os("CONDA_PREFIX").filter(|v| !v.is_empty()),
            home: dirs::home_dir(),
            empty_override: requested.as_ref().is_some_and(|v| v.is_empty()),
        };
        (requested.unwrap_or_default(), environment)
    }

    fn absolute(&self, path: &OsStr, cwd: &Path) -> Result<PathBuf> {
        if path.as_encoded_bytes().contains(&0) {
            return Err(input_error("interpreter paths cannot contain NUL"));
        }
        let path = Path::new(path);
        let expanded = if let Ok(suffix) = path.strip_prefix("~") {
            self.home.as_ref().ok_or_else(|| input_error("cannot expand ~ without a home directory"))?
                .join(suffix)
        } else {
            path.to_path_buf()
        };
        // Do NOT canonicalize: following bin/python's symlink would lose the
        // venv entry path and make Python initialize its base installation.
        let absolute = if expanded.is_absolute() { expanded } else { cwd.join(expanded) };
        if !absolute.is_absolute() {
            return Err(input_error("drive-relative interpreter paths are not supported"));
        }
        Ok(absolute)
    }

    fn candidates(&self, requested: &OsStr, cwd: &Path) -> Result<Vec<Resolved>> {
        if requested.as_encoded_bytes().contains(&0) {
            return Err(input_error("interpreter paths cannot contain NUL"));
        }
        if self.empty_override && requested.is_empty() {
            return Err(input_error("PI_EVAL_PYTHON is empty; unset it to enable discovery"));
        }
        if self.path.as_ref().is_some_and(|p| p.as_encoded_bytes().len() > 64 * 1024) {
            return Err(input_error("PATH exceeds the 64 KiB discovery limit"));
        }
        let cwd = std::path::absolute(cwd)
            .map_err(|error| input_error(&format!("cannot resolve workspace: {error}")))?;
        let mut candidates = Vec::new();
        if !requested.is_empty() {
            let path = Path::new(requested);
            let bare = path.components().count() == 1
                && matches!(path.components().next(), Some(std::path::Component::Normal(_)))
                && requested != OsStr::new("~");
            if bare {
                self.add_path_candidates(&mut candidates, &[requested])?;
            } else {
                self.add(&mut candidates, self.absolute(requested, &cwd)?, Source::Explicit, None)?;
            }
            for candidate in &mut candidates { candidate.source = Source::Explicit; }
            return Ok(candidates);
        }
        if let Some(root) = &self.virtual_env {
            self.add_environment(&mut candidates, self.absolute(root, &cwd)?, Source::ActiveVenv)?;
        }
        if let Some(root) = &self.conda_prefix {
            self.add_environment(&mut candidates, self.absolute(root, &cwd)?, Source::Conda)?;
        }
        self.add_environment(&mut candidates, cwd.join(".venv"), Source::DotVenv)?;
        self.add_environment(&mut candidates, cwd.join("venv"), Source::Venv)?;
        self.add_path_candidates(&mut candidates, &[OsStr::new("python3"), OsStr::new("python")])?;
        Ok(candidates)
    }

    fn add_environment(&self, output: &mut Vec<Resolved>, root: PathBuf, source: Source) -> Result<()> {
        if cfg!(windows) {
            let executable = if source == Source::Conda {
                root.join("python.exe")
            } else {
                root.join("Scripts").join("python.exe")
            };
            self.add(output, executable, source, Some(root))
        } else {
            for name in ["python", "python3"] {
                self.add(output, root.join("bin").join(name), source, Some(root.clone()))?;
            }
            Ok(())
        }
    }

    fn add_path_candidates(&self, output: &mut Vec<Resolved>, names: &[&OsStr]) -> Result<()> {
        for name in names {
            for directory in self.path.as_deref().into_iter().flat_map(std::env::split_paths) {
                // Like native Windows shell discovery, never implicitly pick
                // an executable from the working tree via empty/relative PATH.
                if !directory.is_absolute() { continue; }
                let mut executable = directory.join(name);
                if cfg!(windows) && executable.extension().is_none() {
                    executable.set_extension("exe");
                }
                self.add(output, executable, Source::Path, None)?;
            }
        }
        Ok(())
    }

    fn add(&self, output: &mut Vec<Resolved>, executable: PathBuf, source: Source, root: Option<PathBuf>) -> Result<()> {
        if output.iter().any(|candidate| candidate.executable == executable) { return Ok(()); }
        if output.len() >= MAX_CANDIDATES {
            return Err(input_error("more than 128 interpreter candidates; set PI_EVAL_PYTHON to a path"));
        }
        let mut paths = vec![executable.parent().ok_or_else(|| input_error("interpreter has no parent directory"))?.to_path_buf()];
        if cfg!(windows) && source == Source::Conda && let Some(root) = &root {
            paths.push(root.join("Library").join("bin"));
            paths.push(root.join("Scripts"));
        }
        paths.extend(self.path.as_deref().into_iter().flat_map(std::env::split_paths));
        let path = std::env::join_paths(paths).map_err(|_| input_error("selected interpreter cannot be represented in PATH"))?;
        output.push(Resolved { executable, source, root, path });
        Ok(())
    }

    pub(super) async fn resolve(&self, requested: &OsStr, cwd: &Path, hard_deadline: Instant, session: &CellSession) -> Result<Resolved> {
        let owner = AgentCx::for_current_or_request();
        let deadline = hard_deadline.min(Instant::now() + DISCOVERY_BUDGET);
        let candidates = self.candidates(requested, cwd)?;
        let mut failures = Vec::new();
        for candidate in candidates {
            check(&owner, hard_deadline, session).await?;
            if Instant::now() >= deadline {
                return Err(Error::tool("eval", "EVAL_PY_DISCOVERY_TIMEOUT: interpreter discovery exceeded 8 seconds; no cell was sent"));
            }
            let mut command = candidate.command();
            command.args(["-c", PROBE]).current_dir(cwd)
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            crate::tools::isolate_command_process_group(&mut command);
            let process = match crate::tools::spawn_command_with_job_discipline(&mut command) {
                Ok(process) => process,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    // Do not search around a process-policy/permission denial
                    // or silently change an explicitly selected executable.
                    return Err(Error::tool("eval", format!("EVAL_PY_PROBE: {} interpreter could not start: {error}; no cell was sent", candidate.source.label())));
                }
            };
            let mut process = ProcessGuard::new(process, ProcessCleanupMode::ProcessGroupTree);
            let probe_deadline = deadline.min(Instant::now() + PROBE_BUDGET);
            let status = loop {
                check(&owner, hard_deadline, session).await?;
                if Instant::now() >= probe_deadline { break None; }
                match process.try_wait_child() {
                    Ok(Some(status)) => break Some(status),
                    Ok(None) => owner.time().sleep(Duration::from_millis(20)).await,
                    Err(error) => return Err(Error::tool("eval", format!("EVAL_PY_PROBE: cannot inspect interpreter: {error}"))),
                }
            };
            // Reap the probe's entire owned tree, even after a root exit. No
            // detached discovery workers or pipes survive to execute the cell.
            process.terminate_and_wait().map_err(|error| Error::tool("eval", format!("EVAL_PY_PROBE: cleanup failed: {error}")))?;
            check(&owner, hard_deadline, session).await?;
            if Instant::now() >= deadline {
                return Err(Error::tool("eval", "EVAL_PY_DISCOVERY_TIMEOUT: interpreter discovery exceeded 8 seconds; no cell was sent"));
            }
            if status.is_some_and(|status| status.success()) { return Ok(candidate); }
            let reason = if status.is_some() { "failed-version/startup-check" } else { "probe-timeout" };
            failures.push(format!("{}:{reason}", candidate.source.label()));
            if !requested.is_empty() { break; }
        }
        Err(Error::tool("eval", format!(
            "EVAL_PY_MISSING: no usable Python 3.10+ interpreter; set PI_EVAL_PYTHON to an executable. No cell was sent. Checks: {}",
            if failures.is_empty() { "candidates not found".into() } else { failures.join(", ") }
        )))
    }
}

fn input_error(message: &str) -> Error {
    Error::tool("eval", format!("EVAL_PY_INPUT: {message}"))
}

async fn check(owner: &AgentCx, deadline: Instant, session: &CellSession) -> Result<()> {
    owner.checkpoint().map_err(|_| Error::tool("eval", "EVAL_CANCELLED: interpreter discovery stopped; no cell was sent"))?;
    session.check_tool().await?;
    owner.checkpoint().map_err(|_| Error::tool("eval", "EVAL_CANCELLED: interpreter discovery stopped; no cell was sent"))?;
    if Instant::now() >= deadline {
        return Err(Error::tool("eval", "EVAL_TIMEOUT: interpreter discovery exhausted the cell budget; no cell was sent"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
