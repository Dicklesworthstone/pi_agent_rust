use asupersync::sync::Mutex;
use chrono::Utc;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::Read;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdout, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use url::Url;

use super::{AgentState, Cmd, PiApp, PiMsg};
use crate::session::Session;

#[cfg(feature = "clipboard")]
use arboard::Clipboard as ArboardClipboard;

const SHARE_COMMAND_OUTPUT_MAX_BYTES: u64 = 64 * 1024;
const SHARE_AUTH_TIMEOUT: Duration = Duration::from_secs(15);
const SHARE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const SHARE_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const SHARE_PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

fn share_command_output_max_bytes_usize() -> usize {
    usize::try_from(SHARE_COMMAND_OUTPUT_MAX_BYTES).unwrap_or(usize::MAX)
}

fn capture_share_snapshot(
    session: &Arc<Mutex<Session>>,
) -> crate::error::Result<Option<(String, Option<String>)>> {
    let Ok(guard) = session.try_lock() else {
        return Ok(None);
    };
    let html = guard.to_share_html()?;
    let session_name = guard
        .get_name_ref()
        .map(|name| sanitize_share_label(name, &guard.header.cwd))
        .filter(|name| !name.is_empty());
    Ok(Some((html, session_name)))
}

#[derive(Debug)]
pub(super) struct ShareCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

/// Owns the complete lifecycle of a `/share` subprocess and its pipe readers.
///
/// Dropping an in-flight command is fail-closed: the isolated process tree is
/// terminated, the root child is reaped, and both bounded pipe readers are
/// joined before ownership is released.
struct ShareProcess {
    child: Option<Child>,
    stdout_reader: Option<JoinHandle<std::result::Result<Vec<u8>, String>>>,
    stderr_reader: Option<JoinHandle<std::result::Result<Vec<u8>, String>>>,
    reader_stop: Arc<AtomicBool>,
    launch_guard: crate::tools::JobGatedChildLaunch,
}

impl ShareProcess {
    fn new(child: Child, launch_guard: crate::tools::JobGatedChildLaunch) -> Self {
        Self {
            child: Some(child),
            stdout_reader: None,
            stderr_reader: None,
            reader_stop: Arc::new(AtomicBool::new(false)),
            launch_guard,
        }
    }

    fn stop_readers(&self) {
        self.reader_stop.store(true, Ordering::Release);
    }

    fn take_stdout(&mut self) -> std::io::Result<ChildStdout> {
        self.child
            .as_mut()
            .and_then(|child| child.stdout.take())
            .ok_or_else(|| std::io::Error::other("share command stdout pipe missing"))
    }

    fn take_stderr(&mut self) -> std::io::Result<ChildStderr> {
        self.child
            .as_mut()
            .and_then(|child| child.stderr.take())
            .ok_or_else(|| std::io::Error::other("share command stderr pipe missing"))
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child
            .as_mut()
            .map_or(Ok(None), std::process::Child::try_wait)
    }

    fn join_reader(
        reader: Option<JoinHandle<std::result::Result<Vec<u8>, String>>>,
        stream: &str,
        deadline: Instant,
    ) -> std::io::Result<Vec<u8>> {
        let reader = reader.ok_or_else(|| {
            std::io::Error::other(format!("share command {stream} reader missing"))
        })?;
        while !reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(SHARE_COMMAND_POLL_INTERVAL);
        }
        if !reader.is_finished() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("share command {stream} pipe did not close after process-tree cleanup"),
            ));
        }
        reader
            .join()
            .map_err(|_| std::io::Error::other(format!("share command {stream} reader panicked")))?
            .map_err(std::io::Error::other)
    }

    fn finish(mut self, status: ExitStatus) -> std::io::Result<ShareCommandOutput> {
        // A successful root can leave descendants holding its pipe descriptors.
        // Close the isolated tree before joining readers so completion remains
        // bounded even when a child daemonizes. `try_wait` already reaped the
        // root, so only the process group/job can still contain live processes.
        if let Some(child) = self.child.as_ref() {
            crate::tools::kill_process_group_tree(Some(child.id()));
        }
        let _ = self.child.take();
        self.stop_readers();
        let drain_deadline = Instant::now() + SHARE_PIPE_DRAIN_TIMEOUT;
        let stdout_result = Self::join_reader(self.stdout_reader.take(), "stdout", drain_deadline);
        let stderr_result = Self::join_reader(self.stderr_reader.take(), "stderr", drain_deadline);
        let mut stdout = stdout_result?;
        let mut stderr = stderr_result?;
        let max_bytes = share_command_output_max_bytes_usize();
        let stdout_truncated = stdout.len() > max_bytes;
        let stderr_truncated = stderr.len() > max_bytes;
        stdout.truncate(max_bytes);
        stderr.truncate(max_bytes);
        Ok(ShareCommandOutput {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }

    fn terminate_and_reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            crate::tools::kill_process_group_tree(Some(child.id()));
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stop_readers();
        let drain_deadline = Instant::now() + SHARE_PIPE_DRAIN_TIMEOUT;
        if let Some(reader) = self.stdout_reader.take() {
            let _ = Self::join_reader(Some(reader), "stdout", drain_deadline);
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = Self::join_reader(Some(reader), "stderr", drain_deadline);
        }
    }
}

#[cfg(unix)]
fn set_share_pipe_nonblocking(pipe: &impl std::os::fd::AsFd) -> std::io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(pipe)?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

fn read_share_pipe<R: Read>(
    mut reader: R,
    stop: &AtomicBool,
) -> std::result::Result<Vec<u8>, String> {
    let capture_limit = share_command_output_max_bytes_usize().saturating_add(1);
    let mut captured = Vec::with_capacity(capture_limit.min(8192));
    let mut chunk = [0_u8; 8192];
    let mut reads_after_stop = 0_u8;
    loop {
        let stopping = stop.load(Ordering::Acquire);
        if stopping && reads_after_stop >= 8 {
            break;
        }
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let remaining = capture_limit.saturating_sub(captured.len());
                if remaining > 0 {
                    captured.extend_from_slice(&chunk[..remaining.min(read)]);
                }
                if stopping {
                    reads_after_stop = reads_after_stop.saturating_add(1);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(SHARE_COMMAND_POLL_INTERVAL);
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(captured)
}

impl Drop for ShareProcess {
    fn drop(&mut self) {
        self.terminate_and_reap();
    }
}

pub(super) async fn run_command_output(
    program: &str,
    args: &[OsString],
    cwd: &Path,
    abort_signal: &crate::agent::AbortSignal,
) -> std::io::Result<ShareCommandOutput> {
    run_command_output_with_timeout(program, args, cwd, abort_signal, SHARE_UPLOAD_TIMEOUT).await
}

async fn run_command_output_with_timeout(
    program: &str,
    args: &[OsString],
    cwd: &Path,
    abort_signal: &crate::agent::AbortSignal,
    timeout: Duration,
) -> std::io::Result<ShareCommandOutput> {
    use asupersync::time::{sleep, wall_now};
    use std::process::Stdio;

    if abort_signal.is_aborted() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "command aborted before spawn",
        ));
    }

    let (mut child, launch_guard) = crate::tools::command_with_job_gate_in_dir(program, args, cwd)?;
    child
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::tools::isolate_command_process_group(&mut child);
    let mut child = child.spawn()?;
    if !crate::tools::attach_child_job_discipline(&child) {
        crate::tools::kill_process_group_tree(Some(child.id()));
        let _ = child.kill();
        let _ = child.wait();
        return Err(std::io::Error::other(
            "share command could not establish required process-tree isolation",
        ));
    }
    let mut process = ShareProcess::new(child, launch_guard);
    if abort_signal.is_aborted() {
        process.terminate_and_reap();
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "command aborted before target launch",
        ));
    }
    process.launch_guard.release()?;
    let stdout = process.take_stdout()?;
    let stderr = process.take_stderr()?;
    #[cfg(unix)]
    {
        set_share_pipe_nonblocking(&stdout)?;
        set_share_pipe_nonblocking(&stderr)?;
    }
    let stdout_stop = Arc::clone(&process.reader_stop);
    process.stdout_reader = Some(
        std::thread::Builder::new()
            .name("share-stdout".into())
            .spawn(move || read_share_pipe(stdout, &stdout_stop))?,
    );
    let stderr_stop = Arc::clone(&process.reader_stop);
    process.stderr_reader = Some(
        std::thread::Builder::new()
            .name("share-stderr".into())
            .spawn(move || read_share_pipe(stderr, &stderr_stop))?,
    );

    let started = Instant::now();
    loop {
        // Completion wins a same-tick race with cancellation. This preserves a
        // successfully-created gist URL instead of falsely reporting cancellation.
        if let Some(status) = process.try_wait()? {
            return process.finish(status);
        }

        if abort_signal.is_aborted() {
            process.terminate_and_reap();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "command aborted",
            ));
        }

        if started.elapsed() >= timeout {
            process.terminate_and_reap();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command timed out after {} seconds", timeout.as_secs()),
            ));
        }

        sleep(wall_now(), SHARE_COMMAND_POLL_INTERVAL).await;
    }
}

pub(super) fn parse_gist_url_and_id(output: &str) -> Option<(String, String)> {
    for raw in output.split_whitespace() {
        let candidate_url = raw.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';'));
        let Ok(url) = Url::parse(candidate_url) else {
            continue;
        };
        let Some(host) = url.host_str() else {
            continue;
        };
        if url.scheme() != "https"
            || host != "gist.github.com"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            continue;
        }
        let Some(segments) = url.path_segments().map(|segments| {
            segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>()
        }) else {
            continue;
        };

        // Canonical gist links are exactly `/owner/<gist-id>`.
        // Avoid false positives like profile URLs (`/owner`).
        if segments.len() != 2 {
            continue;
        }

        let gist_id = segments[1];
        if gist_id.is_empty() {
            continue;
        }
        return Some((url.to_string(), gist_id.to_string()));
    }
    None
}

fn validated_share_viewer_url(gist_id: &str) -> std::io::Result<Url> {
    let candidate = crate::session::get_share_viewer_url(gist_id);
    let url = Url::parse(&candidate)
        .map_err(|_| std::io::Error::other("share viewer URL is not a valid absolute URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment() != Some(gist_id)
    {
        return Err(std::io::Error::other(
            "share viewer URL must be credential-free HTTPS without a query or pre-existing fragment",
        ));
    }
    Ok(url)
}

pub(super) fn format_command_output(output: &ShareCommandOutput) -> String {
    let mut stdout = if output.stdout_truncated {
        String::new()
    } else {
        sanitize_command_diagnostic(&String::from_utf8_lossy(&output.stdout))
    };
    let mut stderr = if output.stderr_truncated {
        String::new()
    } else {
        sanitize_command_diagnostic(&String::from_utf8_lossy(&output.stderr))
    };
    stdout = stdout.trim().to_string();
    stderr = stderr.trim().to_string();
    if output.stdout_truncated {
        if !stdout.is_empty() {
            stdout.push('\n');
        }
        let _ = write!(
            stdout,
            "[stdout omitted after exceeding {SHARE_COMMAND_OUTPUT_MAX_BYTES} bytes]"
        );
    }
    if output.stderr_truncated {
        if !stderr.is_empty() {
            stderr.push('\n');
        }
        let _ = write!(
            stderr,
            "[stderr omitted after exceeding {SHARE_COMMAND_OUTPUT_MAX_BYTES} bytes]"
        );
    }
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => "(no output)".to_string(),
        (false, true) => format!("stdout:\n{stdout}"),
        (true, false) => format!("stderr:\n{stderr}"),
        (false, false) => format!("stdout:\n{stdout}\n\nstderr:\n{stderr}"),
    }
}

fn sanitize_command_diagnostic(raw: &str) -> String {
    let screened = crate::memory::screen_secrets(raw);
    let mut safe = String::with_capacity(screened.len());
    for character in screened.chars() {
        if matches!(character, '\n' | '\t') {
            safe.push(character);
        } else if character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            safe.extend(character.escape_unicode());
        } else {
            safe.push(character);
        }
    }
    safe
}

fn sanitize_share_label(raw: &str, workspace_cwd: &str) -> String {
    let bounded: String = raw.chars().take(512).collect();
    let mut screened = crate::memory::screen_secrets(&bounded);
    if !workspace_cwd.is_empty() && workspace_cwd != "/" {
        screened = screened.replace(workspace_cwd, "[REDACTED_CWD]");
    }
    let mut safe = String::with_capacity(screened.len().min(160));
    for (index, character) in screened.chars().enumerate() {
        if index >= 160 {
            break;
        }
        if character.is_whitespace()
            || character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        {
            safe.push(' ');
        } else {
            safe.push(character);
        }
    }
    safe.trim().to_string()
}

/// Build a gist description from the optional session name and current time.
pub(super) fn share_gist_description(session_name: Option<&str>) -> String {
    session_name.map_or_else(
        || format!("Pi session {}", Utc::now().format("%Y-%m-%dT%H:%M:%SZ")),
        |name| format!("Pi session: {name}"),
    )
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    fn test_output(stdout: &[u8], stderr: &[u8]) -> ShareCommandOutput {
        ShareCommandOutput {
            status: ExitStatus::default(),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    #[cfg(unix)]
    fn write_executable_script(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = dir.join("share-command-probe.sh");
        std::fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n"))
            .expect("write share command probe");
        let mut permissions = std::fs::metadata(&path)
            .expect("stat share command probe")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod share command probe");
        path
    }

    #[cfg(unix)]
    fn process_exists(pid: &str) -> bool {
        pid.parse::<i32>()
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
    }

    #[cfg(unix)]
    fn assert_process_exits(pid: &str) {
        for _ in 0..50 {
            if !process_exists(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("share subprocess {pid} survived cancellation");
    }

    // ── parse_gist_url_and_id ───────────────────────────────────────────

    #[test]
    fn parse_gist_url_simple() {
        let (url, id) = parse_gist_url_and_id("https://gist.github.com/user/abc123def456").unwrap();
        assert_eq!(url, "https://gist.github.com/user/abc123def456");
        assert_eq!(id, "abc123def456");
    }

    #[test]
    fn parse_gist_url_from_gh_output() {
        let output = "- Creating gist...\nhttps://gist.github.com/octocat/12345abcde\n";
        let (url, id) = parse_gist_url_and_id(output).unwrap();
        assert_eq!(url, "https://gist.github.com/octocat/12345abcde");
        assert_eq!(id, "12345abcde");
    }

    #[test]
    fn parse_gist_url_ignores_non_gist_urls() {
        assert!(parse_gist_url_and_id("https://github.com/user/repo").is_none());
        assert!(parse_gist_url_and_id("https://example.com/gist").is_none());
        assert!(parse_gist_url_and_id("http://gist.github.com/user/abc123").is_none());
    }

    #[test]
    fn parse_gist_url_empty_input() {
        assert!(parse_gist_url_and_id("").is_none());
    }

    #[test]
    fn parse_gist_url_no_urls() {
        assert!(parse_gist_url_and_id("just some plain text").is_none());
    }

    #[test]
    fn parse_gist_url_strips_quotes() {
        let (url, id) = parse_gist_url_and_id("\"https://gist.github.com/user/deadbeef\"").unwrap();
        assert_eq!(url, "https://gist.github.com/user/deadbeef");
        assert_eq!(id, "deadbeef");
    }

    #[test]
    fn parse_gist_url_trailing_punctuation() {
        let (_, id) =
            parse_gist_url_and_id("Created: https://gist.github.com/user/aaa111,").unwrap();
        assert_eq!(id, "aaa111");
    }

    #[test]
    fn parse_gist_url_ignores_profile_links() {
        assert!(parse_gist_url_and_id("https://gist.github.com/octocat").is_none());
        assert!(parse_gist_url_and_id("https://gist.github.com/octocat/").is_none());
    }

    #[test]
    fn parse_gist_url_ignores_non_canonical_paths() {
        assert!(parse_gist_url_and_id("https://gist.github.com/octocat/aaa111/raw").is_none());
    }

    #[test]
    fn parse_gist_url_rejects_query_fragment_and_credentials() {
        assert!(parse_gist_url_and_id("https://gist.github.com/octocat/aaa111?secret=1").is_none());
        assert!(parse_gist_url_and_id("https://gist.github.com/octocat/aaa111#raw").is_none());
        assert!(
            parse_gist_url_and_id("https://user:pass@gist.github.com/octocat/aaa111").is_none()
        );
    }

    // ── format_command_output ───────────────────────────────────────────

    #[test]
    fn format_output_both_empty() {
        assert_eq!(format_command_output(&test_output(&[], &[])), "(no output)");
    }

    #[test]
    fn format_output_only_stdout() {
        let output = test_output(b"hello world", &[]);
        assert_eq!(format_command_output(&output), "stdout:\nhello world");
    }

    #[test]
    fn format_output_only_stderr() {
        let output = test_output(&[], b"error msg");
        assert_eq!(format_command_output(&output), "stderr:\nerror msg");
    }

    #[test]
    fn format_output_both_present() {
        let output = test_output(b"out", b"err");
        assert_eq!(
            format_command_output(&output),
            "stdout:\nout\n\nstderr:\nerr"
        );
    }

    #[test]
    fn format_output_trims_whitespace() {
        let output = test_output(b"  trimmed  \n", &[]);
        assert_eq!(format_command_output(&output), "stdout:\ntrimmed");
    }

    #[test]
    fn format_output_reports_each_truncated_stream_without_hiding_stderr() {
        let mut output = test_output(b"out", b"evidence");
        output.stdout_truncated = true;
        output.stderr_truncated = true;
        let formatted = format_command_output(&output);
        assert!(formatted.contains("stdout:\n[stdout omitted after exceeding 65536 bytes]"));
        assert!(formatted.contains("stderr:\n[stderr omitted after exceeding 65536 bytes]"));
        assert!(!formatted.contains("stdout:\nout\n"));
        assert!(!formatted.contains("stderr:\nevidence\n"));
    }

    #[test]
    fn format_output_redacts_secrets_and_escapes_terminal_controls() {
        let output = test_output(
            &[],
            b"ghp_abcdefghijklmnopqrstuvwxyz\x1b]2;spoofed\xe2\x80\xaetail",
        );
        let formatted = format_command_output(&output);
        assert!(formatted.contains("[REDACTED_GITHUB_PAT]"));
        assert!(!formatted.contains("ghp_abcdefghijklmnopqrstuvwxyz"));
        assert!(!formatted.contains('\x1b'));
        assert!(!formatted.contains('\u{202e}'));
        assert!(formatted.contains("\\u{1b}"));
        assert!(formatted.contains("\\u{202e}"));
    }

    // ── subprocess lifecycle ────────────────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn command_output_is_bounded_while_both_pipes_are_drained() {
        asupersync::test_utils::run_test(|| async {
            let temp = tempfile::tempdir().expect("tempdir");
            let script = write_executable_script(
                temp.path(),
                "dd if=/dev/zero bs=70000 count=1 2>/dev/null\nprintf 'stderr-evidence' >&2",
            );
            let (_abort_handle, abort_signal) = crate::agent::AbortHandle::new();
            let output = run_command_output_with_timeout(
                script.to_str().expect("utf8 script path"),
                &[],
                temp.path(),
                &abort_signal,
                Duration::from_secs(5),
            )
            .await
            .expect("bounded share command");

            assert!(output.status.success(), "status: {:?}", output.status);
            assert_eq!(output.stdout.len(), share_command_output_max_bytes_usize());
            assert!(output.stdout_truncated);
            assert_eq!(output.stderr, b"stderr-evidence");
            assert!(!output.stderr_truncated);
        });
    }

    #[cfg(unix)]
    #[test]
    fn successful_root_cannot_leave_a_descendant_holding_pipes() {
        asupersync::test_utils::run_test(|| async {
            let temp = tempfile::tempdir().expect("tempdir");
            let marker = temp.path().join("leaked");
            let script =
                write_executable_script(temp.path(), "(sleep 1; printf leaked > \"$1\") &\nexit 0");
            let (_abort_handle, abort_signal) = crate::agent::AbortHandle::new();
            let output = run_command_output_with_timeout(
                script.to_str().expect("utf8 script path"),
                &[marker.as_os_str().to_os_string()],
                temp.path(),
                &abort_signal,
                Duration::from_secs(3),
            )
            .await
            .expect("successful root");

            assert!(output.status.success(), "status: {:?}", output.status);
            std::thread::sleep(Duration::from_millis(1100));
            assert!(!marker.exists(), "detached descendant survived root exit");
        });
    }

    #[cfg(unix)]
    #[test]
    fn pre_aborted_command_never_spawns() {
        asupersync::test_utils::run_test(|| async {
            let temp = tempfile::tempdir().expect("tempdir");
            let marker = temp.path().join("spawned");
            let script = write_executable_script(temp.path(), "printf spawned > \"$1\"");
            let (abort_handle, abort_signal) = crate::agent::AbortHandle::new();
            abort_handle.abort();

            let error = run_command_output_with_timeout(
                script.to_str().expect("utf8 script path"),
                &[marker.as_os_str().to_os_string()],
                temp.path(),
                &abort_signal,
                Duration::from_secs(5),
            )
            .await
            .expect_err("pre-aborted share command must not spawn");

            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            assert!(!marker.exists(), "pre-aborted command created its marker");
        });
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_and_reaps_root_and_grandchild() {
        asupersync::test_utils::run_test(|| async {
            let temp = tempfile::tempdir().expect("tempdir");
            let parent_pid = temp.path().join("parent.pid");
            let child_pid = temp.path().join("child.pid");
            let script = write_executable_script(
                temp.path(),
                "printf '%s' \"$$\" > \"$1\"\nsleep 5 &\nprintf '%s' \"$!\" > \"$2\"\nwait",
            );
            let (_abort_handle, abort_signal) = crate::agent::AbortHandle::new();

            let started = Instant::now();
            let error = run_command_output_with_timeout(
                script.to_str().expect("utf8 script path"),
                &[
                    parent_pid.as_os_str().to_os_string(),
                    child_pid.as_os_str().to_os_string(),
                ],
                temp.path(),
                &abort_signal,
                Duration::from_secs(2),
            )
            .await
            .expect_err("hung share command must time out");

            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "timeout waited for the grandchild to exit naturally"
            );
            let parent = std::fs::read_to_string(&parent_pid).expect("parent pid");
            let child = std::fs::read_to_string(&child_pid).expect("child pid");
            assert_process_exits(parent.trim());
            assert_process_exits(child.trim());
        });
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_and_reaps_root_and_grandchild() {
        asupersync::test_utils::run_test(|| async {
            let temp = tempfile::tempdir().expect("tempdir");
            let parent_pid = temp.path().join("parent.pid");
            let child_pid = temp.path().join("child.pid");
            let script = write_executable_script(
                temp.path(),
                "printf '%s' \"$$\" > \"$1\"\nsleep 5 &\nprintf '%s' \"$!\" > \"$2\"\nwait",
            );
            let (abort_handle, abort_signal) = crate::agent::AbortHandle::new();
            let parent_pid_for_abort = parent_pid.clone();
            let child_pid_for_abort = child_pid.clone();
            let aborter = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if parent_pid_for_abort.exists() && child_pid_for_abort.exists() {
                        abort_handle.abort();
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                false
            });

            let started = Instant::now();
            let error = run_command_output_with_timeout(
                script.to_str().expect("utf8 script path"),
                &[
                    parent_pid.as_os_str().to_os_string(),
                    child_pid.as_os_str().to_os_string(),
                ],
                temp.path(),
                &abort_signal,
                Duration::from_secs(5),
            )
            .await
            .expect_err("cancelled share command must not complete");

            assert!(aborter.join().expect("join share command aborter"));
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "cancellation waited for the grandchild to exit naturally"
            );
            let parent = std::fs::read_to_string(&parent_pid).expect("parent pid");
            let child = std::fs::read_to_string(&child_pid).expect("child pid");
            assert_process_exits(parent.trim());
            assert_process_exits(child.trim());
        });
    }

    // ── share_gist_description ──────────────────────────────────────────

    #[test]
    fn gist_description_with_name() {
        assert_eq!(
            share_gist_description(Some("my-session")),
            "Pi session: my-session"
        );
    }

    #[test]
    fn gist_description_without_name() {
        let desc = share_gist_description(None);
        assert!(desc.starts_with("Pi session "));
        assert!(desc.contains('T'));
    }

    #[test]
    fn share_snapshot_redacts_name_metadata_before_upload() {
        let mut raw = Session::in_memory();
        raw.header.cwd = "/private/workspaces/customer-secret".to_string();
        raw.set_name(
            "work /private/workspaces/customer-secret ghp_abcdefghijklmnopqrstuvwxyz\nspoof",
        );
        let session = Arc::new(Mutex::new(raw));
        let (_, name) = capture_share_snapshot(&session)
            .expect("share snapshot")
            .expect("uncontended snapshot");
        let description = share_gist_description(name.as_deref());
        assert!(description.contains("[REDACTED_CWD]"));
        assert!(description.contains("[REDACTED_GITHUB_PAT]"));
        assert!(!description.contains("customer-secret"));
        assert!(!description.contains("ghp_"));
        assert!(!description.contains('\n'));
    }

    #[test]
    fn share_snapshot_fails_fast_while_session_is_busy() {
        let session = Arc::new(Mutex::new(Session::in_memory()));
        let held = session.try_lock().expect("hold session lock");
        assert!(
            capture_share_snapshot(&session)
                .expect("busy result")
                .is_none()
        );
        drop(held);
        assert!(
            capture_share_snapshot(&session)
                .expect("share result")
                .is_some()
        );
    }
}

impl PiApp {
    #[allow(clippy::too_many_lines)]
    pub(super) fn handle_slash_share(&mut self, args: &str) -> Option<Cmd> {
        if self.agent_state != AgentState::Idle {
            self.status_message = Some("Cannot share while processing".to_string());
            return None;
        }

        if !args.trim().is_empty() {
            self.status_message = Some(
                "Usage: /share (uploads a secret, unlisted gist; anyone with its URL can view it; public sharing is disabled)"
                    .to_string(),
            );
            return None;
        }

        self.agent_state = AgentState::Processing;
        self.status_message = Some(
            "Sharing session... (secret gist, not private; transcript may still contain sensitive local context; Esc to cancel)"
                .to_string(),
        );

        let (abort_handle, abort_signal) = crate::agent::AbortHandle::new();
        self.abort_handle = Some(abort_handle);

        let event_tx = self.event_tx.clone();
        let runtime_handle = self.runtime_handle.clone();
        let session = Arc::clone(&self.session);
        let cwd = self.cwd.clone();
        let gh_path_override = self.config.gh_path.clone();

        runtime_handle.spawn(async move {
            let gh = gh_path_override
                .as_ref()
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "gh".to_string());
            let mut share_viewer_url = match validated_share_viewer_url("pending") {
                Ok(url) => url,
                Err(err) => {
                    let details = sanitize_command_diagnostic(&err.to_string());
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(format!("Refusing unsafe share viewer URL: {details}")),
                    )
                    .await;
                    return;
                }
            };

            let auth_args = vec![OsString::from("auth"), OsString::from("status")];
            match run_command_output_with_timeout(
                &gh,
                &auth_args,
                &cwd,
                &abort_signal,
                SHARE_AUTH_TIMEOUT,
            )
            .await
            {
                Ok(output) => {
                    if !output.status.success() {
                        let details = format_command_output(&output);
                        let message = format!(
                            "`gh` is not authenticated.\n\
                             Run `gh auth login` to authenticate, then retry `/share`.\n\n\
                             {details}"
                        );
                        let _ = crate::interactive::enqueue_pi_event(
                            &event_tx,
                            &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                            PiMsg::AgentError(message),
                        )
                        .await;
                        return;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let message = "GitHub CLI `gh` not found.\n\
                             Install it from https://cli.github.com, then run `gh auth login`."
                        .to_string();
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(message),
                    )
                    .await;
                    return;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::System("Share cancelled".to_string()),
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    let details = sanitize_command_diagnostic(&err.to_string());
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(format!("Failed to run `gh auth status`: {details}")),
                    )
                    .await;
                    return;
                }
            }

            if abort_signal.is_aborted() {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::System("Share cancelled".to_string()),
                )
                .await;
                return;
            }

            // Sharing is user-cancellable and must not become an unbounded wait
            // on a mutex held by another task. The app is already marked busy,
            // so brief contention is best surfaced as a retryable error.
            let (html, session_name) = match capture_share_snapshot(&session) {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current()
                            .unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(
                            "Session is busy; retry `/share` after the current session update finishes."
                                .to_string(),
                        ),
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    let details = sanitize_command_diagnostic(&err.to_string());
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current()
                            .unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(format!("Session cannot be shared: {details}")),
                    )
                    .await;
                    return;
                }
            };

            if abort_signal.is_aborted() {
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::System("Share cancelled".to_string()),
                )
                .await;
                return;
            }

            let gist_desc = share_gist_description(session_name.as_deref());

            let temp_file = match tempfile::Builder::new()
                .prefix("pi-share-")
                .suffix(".html")
                .tempfile()
            {
                Ok(file) => file,
                Err(err) => {
                    let details = sanitize_command_diagnostic(&err.to_string());
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(format!("Failed to create temp file: {details}")),
                    )
                    .await;
                    return;
                }
            };
            let temp_path = temp_file.into_temp_path();
            if let Err(err) = asupersync::fs::write(&temp_path, html.as_bytes()).await {
                let details = sanitize_command_diagnostic(&err.to_string());
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::AgentError(format!("Failed to write temp file: {details}")),
                )
                .await;
                return;
            }

            let gist_args = vec![
                OsString::from("gist"),
                OsString::from("create"),
                OsString::from("--public=false"),
                OsString::from("--desc"),
                OsString::from(&gist_desc),
                temp_path.as_os_str().to_os_string(),
            ];
            let output = match run_command_output(&gh, &gist_args, &cwd, &abort_signal).await {
                Ok(output) => output,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let message = "GitHub CLI `gh` not found.\n\
                             Install it from https://cli.github.com, then run `gh auth login`."
                        .to_string();
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(message),
                    )
                    .await;
                    return;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::System("Share cancelled".to_string()),
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    let details = sanitize_command_diagnostic(&err.to_string());
                    let _ = crate::interactive::enqueue_pi_event(
                        &event_tx,
                        &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                        PiMsg::AgentError(format!("Failed to run `gh gist create`: {details}")),
                    )
                    .await;
                    return;
                }
            };

            if !output.status.success() {
                let details = format_command_output(&output);
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::AgentError(format!("`gh gist create` failed.\n\n{details}")),
                )
                .await;
                return;
            }

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let Some((gist_url, gist_id)) = parse_gist_url_and_id(&stdout) else {
                let details = format_command_output(&output);
                let _ = crate::interactive::enqueue_pi_event(
                    &event_tx,
                    &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                    PiMsg::AgentError(format!(
                        "Failed to parse gist URL from `gh gist create` output.\n\n{details}"
                    )),
                )
                .await;
                return;
            };

            share_viewer_url.set_fragment(Some(&gist_id));
            let share_url = share_viewer_url.to_string();
            drop(temp_path);

            // Copy viewer URL to clipboard (best-effort).
            #[cfg(feature = "clipboard")]
            {
                if let Ok(mut clipboard) = ArboardClipboard::new() {
                    let _ = clipboard.set_text(share_url.clone());
                }
            }

            // Paragraph breaks: the TUI renders this as markdown, and a single
            // newline soft-wraps into the warning sentence, which can split
            // "Share URL:" or the link itself across two terminal lines.
            let message = format!(
                "Created secret gist (not private; anyone with the URL can view it). Recognized secrets and the exact workspace cwd were redacted, but the transcript may still contain sensitive local context.\n\n\
                 Share URL: {share_url}\n\nGist: {gist_url}"
            );
            let _ = crate::interactive::enqueue_pi_event(
                &event_tx,
                &asupersync::Cx::current().unwrap_or_else(asupersync::Cx::for_request),
                PiMsg::System(message),
            )
            .await;
        });
        None
    }
}
