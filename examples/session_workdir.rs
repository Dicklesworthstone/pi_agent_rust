//! Explicit session workdir recovery without loading a model or extensions.
//!
//! Build with `dsr wrap cargo build --example session_workdir`.
//! See `docs/session-workdir-recovery.md` for the inspect/attach/resume flow.
//! This developer frontend does not change the shipping binary inventory.

#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::{Parser, Subcommand};
use pi::session::Session;
use pi::session_workdir::{
    SessionWorkdir, WorkdirHealth, attach_session_workdir, inspect_session_workdir,
    require_session_workdir,
};
use pi::{Error, PiResult};
use serde::Serialize;

const V2_OPEN_MODE: &str = "PI_SESSION_V2_OPEN_MODE";

#[derive(Debug, Parser)]
#[command(about = "Inspect and explicitly recover a session's working directory")]
struct Arguments {
    #[command(subcommand)]
    command: RecoveryCommand,
}

#[derive(Debug, Subcommand)]
enum RecoveryCommand {
    /// Read workdir health without changing the session or starting Pi.
    Inspect { session: PathBuf },
    /// Record an explicitly selected replacement workdir; do not launch Pi.
    Attach { session: PathBuf, workdir: PathBuf },
    /// Start Pi in the recorded or explicitly attached workdir.
    Resume {
        session: PathBuf,
        /// Exact Pi executable to launch (resolved before changing child cwd).
        #[arg(long)]
        pi_binary: PathBuf,
    },
    /// Discard the resume intention, not the file: start a fresh session.
    StartNew {
        workdir: PathBuf,
        #[arg(long)]
        pi_binary: PathBuf,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentResult {
    changed: bool,
    workdir: SessionWorkdir,
}

struct Launch {
    executable: PathBuf,
    workdir: PathBuf,
    session: Option<PathBuf>,
}

impl Launch {
    fn new(executable: &Path, workdir: PathBuf, session: Option<PathBuf>) -> PiResult<Self> {
        let executable = std::fs::canonicalize(executable)?;
        if !executable.is_file() {
            return Err(Error::validation("--pi-binary must name an executable file"));
        }
        Ok(Self {
            executable,
            workdir,
            session,
        })
    }

    fn command(&self) -> PiResult<Command> {
        let WorkdirHealth::Available { canonical_path } = WorkdirHealth::inspect(&self.workdir) else {
            return Err(Error::session(
                "PI_SESSION_WORKDIR_UNAVAILABLE: workdir disappeared before launch; no Pi process started",
            ));
        };
        if canonical_path != self.workdir {
            return Err(Error::session(
                "PI_SESSION_WORKDIR_CHANGED: workdir now resolves elsewhere; inspect and attach again",
            ));
        }
        let mut command = Command::new(&self.executable);
        command.current_dir(&self.workdir);
        if let Some(session) = &self.session {
            command.arg("--session").arg(session);
        }
        Ok(command)
    }
}

enum Prepared {
    Json(serde_json::Value),
    Launch(Launch),
}

fn absolute_user_path(path: &Path) -> PiResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

async fn load_session(path: &Path) -> PiResult<(PathBuf, Session)> {
    let path = std::fs::canonicalize(path)?;
    let text = path.to_str().ok_or_else(|| {
        Error::validation("session recovery currently requires a UTF-8 session-file path")
    })?;
    let (session, diagnostics) = Session::open_with_diagnostics(text).await?;
    // Do not turn a read-time corruption recovery into an automatic rewrite
    // or a new model turn. The normal audit/reconcile tools remain available.
    if diagnostics.warning_lines().into_iter().next().is_some() {
        return Err(Error::session(
            "PI_SESSION_RECOVERY_DIAGNOSTICS: session needs inspection with --session-audit before workdir recovery",
        ));
    }
    Ok((path, session))
}

async fn prepare(command: RecoveryCommand) -> PiResult<Prepared> {
    match command {
        RecoveryCommand::Inspect { session } => {
            let (_, session) = load_session(&session).await?;
            Ok(Prepared::Json(serde_json::to_value(
                inspect_session_workdir(&session)?,
            )?))
        }
        RecoveryCommand::Attach { session, workdir } => {
            let (_, mut session) = load_session(&session).await?;
            let target = absolute_user_path(&workdir)?;
            let changed = attach_session_workdir(&mut session, &target)?;
            if changed {
                // Saving is a required barrier: never report an attachment as
                // durable or launch against it after a persistence failure.
                session.save().await?;
            }
            Ok(Prepared::Json(serde_json::to_value(AttachmentResult {
                changed,
                workdir: inspect_session_workdir(&session)?,
            })?))
        }
        RecoveryCommand::Resume { session, pi_binary } => {
            let (path, session) = load_session(&session).await?;
            let workdir = require_session_workdir(&session)?;
            Ok(Prepared::Launch(Launch::new(
                &pi_binary,
                workdir,
                Some(path),
            )?))
        }
        RecoveryCommand::StartNew { workdir, pi_binary } => {
            let workdir = absolute_user_path(&workdir)?;
            let WorkdirHealth::Available { canonical_path } = WorkdirHealth::inspect(&workdir) else {
                return Err(Error::validation(format!(
                    "workdir {workdir:?} is not an accessible directory"
                )));
            };
            Ok(Prepared::Launch(Launch::new(
                &pi_binary,
                canonical_path,
                None,
            )?))
        }
    }
}

fn run(arguments: Arguments) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // Tear down the recovery runtime before handing the terminal to Pi. The
    // child starts in its chosen workdir BEFORE configuration/trust/tool setup.
    let prepared = {
        let reactor = asupersync::runtime::reactor::create_reactor()?;
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()?;
        runtime.block_on(Box::pin(prepare(arguments.command)))?
    };
    match prepared {
        Prepared::Json(value) => {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, &value)?;
            writeln!(stdout)?;
            Ok(ExitCode::SUCCESS)
        }
        Prepared::Launch(launch) => {
            // No shell, no interpolated command string, and no inherited resume
            // arguments which could replace the explicitly selected session.
            let status = launch.command()?.status()?;
            Ok(exit_code(status))
        }
    }
}

fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    let code = status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1);
    ExitCode::from(code)
}

fn full_hydration_command(
    executable: &Path,
    arguments: impl IntoIterator<Item = OsString>,
) -> Command {
    let mut command = Command::new(executable);
    command.args(arguments).env(V2_OPEN_MODE, "full");
    command
}

fn relaunch_with_full_hydration() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let status = full_hydration_command(&executable, std::env::args_os().skip(1)).status()?;
    Ok(exit_code(status))
}

fn main() -> ExitCode {
    // Parse help/errors before spawning anything. A bounded V2 snapshot can
    // omit an older binding even though it contains the conversation tail.
    // Use a child-local environment override rather than unsafe process-wide
    // set_var (Rust 2024), and do this before creating any runtime threads.
    let arguments = Arguments::parse();
    let result = if std::env::var(V2_OPEN_MODE).as_deref() == Ok("full") {
        run(arguments)
    } else {
        relaunch_with_full_hydration()
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_requires_an_explicit_executable() {
        assert!(Arguments::try_parse_from(["recover", "resume", "session.jsonl"]).is_err());
        assert!(
            Arguments::try_parse_from([
                "recover",
                "resume",
                "session.jsonl",
                "--pi-binary",
                "/usr/bin/pi",
            ])
            .is_ok()
        );
    }

    #[test]
    fn attach_requires_explicit_session_and_target() {
        assert!(Arguments::try_parse_from(["recover", "attach", "session.jsonl"]).is_err());
        assert!(
            Arguments::try_parse_from(["recover", "attach", "session.jsonl", "/workspace"])
                .is_ok()
        );
    }

    #[test]
    fn launch_uses_exact_session_argument_without_shell_parsing() {
        let root = tempfile::tempdir().expect("tempdir");
        let workdir = root.path().canonicalize().expect("canonical");
        let session = workdir.join("session with spaces;not-a-command.jsonl");
        let launch = Launch {
            executable: std::env::current_exe().expect("test executable"),
            workdir: workdir.clone(),
            session: Some(session.clone()),
        };
        let command = launch.command().expect("command");
        assert_eq!(command.get_current_dir(), Some(workdir.as_path()));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("--session"), session.as_os_str()]
        );
    }

    #[test]
    fn start_new_does_not_resume_or_delete_a_session() {
        let root = tempfile::tempdir().expect("tempdir");
        let workdir = root.path().canonicalize().expect("canonical");
        let existing = workdir.join("keep.jsonl");
        std::fs::write(&existing, "untouched").expect("write");
        let launch = Launch {
            executable: std::env::current_exe().expect("test executable"),
            workdir,
            session: None,
        };
        let command = launch.command().expect("command");
        assert_eq!(command.get_args().count(), 0);
        assert_eq!(std::fs::read_to_string(existing).expect("read"), "untouched");
    }

    #[test]
    fn launch_rejects_a_workdir_removed_after_preparation() {
        let root = tempfile::tempdir().expect("tempdir");
        let original = root.path().join("original");
        std::fs::create_dir(&original).expect("create");
        let launch = Launch {
            executable: std::env::current_exe().expect("test executable"),
            workdir: original.canonicalize().expect("canonical"),
            session: None,
        };
        std::fs::rename(&original, root.path().join("moved")).expect("rename");
        assert!(launch.command().is_err());
    }

    #[test]
    fn recovery_child_forces_full_hydration_without_changing_parent_environment() {
        let before = std::env::var_os(V2_OPEN_MODE);
        let executable = std::env::current_exe().expect("test executable");
        let arguments = [OsString::from("inspect"), OsString::from("a session.jsonl")];
        let command = full_hydration_command(&executable, arguments.clone());
        assert_eq!(command.get_program(), executable.as_os_str());
        assert_eq!(command.get_args().collect::<Vec<_>>(), arguments.iter().map(OsString::as_os_str).collect::<Vec<_>>());
        assert!(command.get_envs().any(|(key, value)| {
            key == std::ffi::OsStr::new(V2_OPEN_MODE)
                && value == Some(std::ffi::OsStr::new("full"))
        }));
        assert_eq!(std::env::var_os(V2_OPEN_MODE), before);
    }
}
