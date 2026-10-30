//! Cancellation-owned transport for the streaming child protocol.
//!
//! A child may exit while a descendant still holds either pipe's write end.
//! Blocking readers cannot then be joined, even after their receiver is dropped.
//! Here the run future owns Unix nonblocking descriptors or Windows overlapped
//! readers and bounded partial frames. Dropping it retires the read ends without
//! waiting for EOF or leaving a per-child reader thread behind.

use super::{
    AgentCx, DRAIN_BATCH, Deadline, PIPE_DRAIN_TIMEOUT, PipeFrame, PipeKind, SubagentResult,
    UpdateCallback, apply_child_frame, check_budget, poll_pause, protocol,
};
use std::io::{self, BufRead, BufReader, Read};
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BUFFER_BYTES: usize = 8192;
// Limit work on incomplete frames too, not just the number of completed lines.
// A continuously writing stream cannot monopolize an executor or starve its peer.
const CHUNKS_PER_DRAIN: usize = 32;
const PIPE_TIMEOUT: &str =
    "PI_SUBAGENT_PIPE_TIMEOUT: child pipes did not close after process termination";

pub(super) struct FramedPipe<R> {
    reader: Option<BufReader<R>>,
    partial: Vec<u8>,
    frame_limit: usize,
}

#[cfg(unix)]
impl<R: Read + AsFd> FramedPipe<R> {
    pub(super) fn new(pipe: R, frame_limit: usize) -> io::Result<Self> {
        // Preserve all unrelated flags. The descriptors are private to this
        // transport; nothing else reads or switches them back to blocking mode.
        let flags = rustix::fs::fcntl_getfl(&pipe).map_err(io::Error::from)?;
        rustix::fs::fcntl_setfl(&pipe, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(io::Error::from)?;
        Ok(Self::from_nonblocking(pipe, frame_limit))
    }
}

impl<R: Read> FramedPipe<R> {
    fn from_nonblocking(pipe: R, frame_limit: usize) -> Self {
        Self {
            reader: Some(BufReader::with_capacity(BUFFER_BYTES, pipe)),
            partial: Vec::new(),
            frame_limit,
        }
    }

    /// `None` means no complete frame this slice, not necessarily EOF. Retain
    /// partial bytes across WouldBlock, including a split multibyte character.
    /// CRLF, empty lines and a final unterminated frame match `read_frame`.
    pub(super) fn next_frame(
        &mut self,
        chunks_left: &mut usize,
    ) -> Result<Option<Vec<u8>>, &'static str> {
        while *chunks_left > 0 {
            let Some(reader) = self.reader.as_mut() else {
                return Ok(None);
            };
            *chunks_left -= 1;
            let chunk = match reader.fill_buf() {
                Ok(chunk) => chunk,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.reader = None;
                    self.partial.clear();
                    return Err(protocol::PIPE_ERROR);
                }
            };
            if chunk.is_empty() {
                self.reader = None;
                return Ok((!self.partial.is_empty()).then(|| std::mem::take(&mut self.partial)));
            }
            let newline = chunk.iter().position(|byte| *byte == b'\n');
            let count = newline.unwrap_or(chunk.len());
            if count > self.frame_limit.saturating_sub(self.partial.len()) {
                self.reader = None;
                self.partial.clear();
                return Err(protocol::FRAME_LIMIT);
            }
            self.partial.extend_from_slice(&chunk[..count]);
            reader.consume(count + usize::from(newline.is_some()));
            if newline.is_some() {
                if self.partial.last() == Some(&b'\r') {
                    self.partial.pop();
                }
                return Ok(Some(std::mem::take(&mut self.partial)));
            }
        }
        Ok(None)
    }

    pub(super) const fn is_closed(&self) -> bool {
        self.reader.is_none()
    }
}

pub(super) struct ChildPipes<O, E> {
    stdout: FramedPipe<O>,
    stderr: FramedPipe<E>,
}

#[cfg(unix)]
impl<O: Read + AsFd, E: Read + AsFd> ChildPipes<O, E> {
    pub(super) fn new(stdout: O, stderr: E) -> io::Result<Self> {
        Ok(Self {
            stdout: FramedPipe::new(stdout, protocol::MAX_FRAME_BYTES)?,
            stderr: FramedPipe::new(stderr, protocol::MAX_FRAME_BYTES)?,
        })
    }
}

#[cfg(windows)]
pub(super) struct WindowsReader(crate::jobs::windows_job_pipe::Reader);

#[cfg(windows)]
impl Read for WindowsReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        // Reader::read waits on a condvar for dedicated job pump threads.
        // A subagent future must only poll, preserving its per-slice budgets.
        self.0.try_read(buffer)
    }
}

#[cfg(windows)]
impl ChildPipes<WindowsReader, WindowsReader> {
    fn from_readers(
        stdout: crate::jobs::windows_job_pipe::Reader,
        stderr: crate::jobs::windows_job_pipe::Reader,
    ) -> Self {
        Self {
            stdout: FramedPipe::from_nonblocking(WindowsReader(stdout), protocol::MAX_FRAME_BYTES),
            stderr: FramedPipe::from_nonblocking(WindowsReader(stderr), protocol::MAX_FRAME_BYTES),
        }
    }

    pub(super) fn configure(command: &mut Command) -> io::Result<Self> {
        let (stdout, stdout_writer) = crate::jobs::windows_job_pipe::pair()?;
        let (stderr, stderr_writer) = crate::jobs::windows_job_pipe::pair()?;
        command
            .stdout(Stdio::from(stdout_writer))
            .stderr(Stdio::from(stderr_writer));
        Ok(Self::from_readers(stdout, stderr))
    }
}

fn next_pipe_frame<R: Read>(
    pipe: &mut FramedPipe<R>,
    kind: PipeKind,
    chunks_left: &mut usize,
) -> Option<PipeFrame> {
    match pipe.next_frame(chunks_left) {
        Ok(Some(bytes)) => Some(match kind {
            PipeKind::Stderr => PipeFrame::Data(kind, String::from_utf8_lossy(&bytes).into_owned()),
            PipeKind::Stdout => String::from_utf8(bytes).map_or(
                PipeFrame::Error("PI_SUBAGENT_PROTOCOL: child stdout is not UTF-8"),
                |line| PipeFrame::Data(kind, line),
            ),
        }),
        Ok(None) => None,
        Err(error) => Some(PipeFrame::Error(error)),
    }
}

impl<O: Read, E: Read> ChildPipes<O, E> {
    #[cfg(windows)]
    pub(super) fn into_frames(self) -> (FramedPipe<O>, FramedPipe<E>) {
        (self.stdout, self.stderr)
    }

    pub(super) fn drain(
        &mut self,
        protocol: &mut protocol::ChildProtocol,
        result: &mut SubagentResult,
        update: Option<&UpdateCallback>,
    ) {
        let mut stdout_chunks = CHUNKS_PER_DRAIN;
        let mut stderr_chunks = CHUNKS_PER_DRAIN;
        // Alternate the streams and bound each one's bytes and frame count.
        // An unfinished stdout line must not prevent draining a full stderr.
        for _ in 0..DRAIN_BATCH / 2 {
            let stdout = next_pipe_frame(&mut self.stdout, PipeKind::Stdout, &mut stdout_chunks);
            let stderr = next_pipe_frame(&mut self.stderr, PipeKind::Stderr, &mut stderr_chunks);
            let made_frame = stdout.is_some() || stderr.is_some();
            for frame in [stdout, stderr].into_iter().flatten() {
                apply_child_frame(frame, protocol, result, update);
            }
            if !made_frame || result.is_error {
                break;
            }
        }
    }

    const fn is_closed(&self) -> bool {
        self.stdout.is_closed() && self.stderr.is_closed()
    }

    pub(super) async fn finish(
        self,
        protocol: &mut protocol::ChildProtocol,
        result: &mut SubagentResult,
        update: Option<&UpdateCallback>,
        owner: &AgentCx,
        work_deadline: Deadline,
    ) {
        self.finish_with_timeout(
            protocol,
            result,
            update,
            owner,
            work_deadline,
            PIPE_DRAIN_TIMEOUT,
        )
        .await;
    }

    async fn finish_with_timeout(
        mut self,
        protocol: &mut protocol::ChildProtocol,
        result: &mut SubagentResult,
        update: Option<&UpdateCallback>,
        owner: &AgentCx,
        work_deadline: Deadline,
        drain_timeout: Duration,
    ) {
        let started = Instant::now();
        loop {
            let in_budget = check_budget(owner, work_deadline, result);
            // Preserve already-ready diagnostics on failure, but never wait for
            // a failed producer or invoke its remaining stdout callbacks.
            self.drain(protocol, result, update);
            if !in_budget || !check_budget(owner, work_deadline, result) {
                return;
            }
            if started.elapsed() >= drain_timeout {
                result.fail(PIPE_TIMEOUT.to_string());
                return;
            }
            if self.is_closed() {
                return;
            }
            poll_pause(owner).await;
        }
        // Both readers and partial frames are dropped on EVERY return path,
        // including cancellation of this future while its timer is pending.
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Cursor, Write};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    fn socket_pipe(limit: usize) -> (FramedPipe<UnixStream>, UnixStream) {
        let (reader, writer) = UnixStream::pair().unwrap();
        let pipe = FramedPipe::new(reader, limit).unwrap();
        assert_nonblocking(&pipe);
        (pipe, writer)
    }

    fn assert_nonblocking<R: Read + AsFd>(pipe: &FramedPipe<R>) {
        // Fail before a potentially blocking read if the flag setup is removed.
        let flags = rustix::fs::fcntl_getfl(pipe.reader.as_ref().unwrap().get_ref()).unwrap();
        assert!(flags.contains(rustix::fs::OFlags::NONBLOCK));
    }

    fn frame<R: Read>(pipe: &mut FramedPipe<R>) -> Result<Option<Vec<u8>>, &'static str> {
        let mut budget = CHUNKS_PER_DRAIN;
        pipe.next_frame(&mut budget)
    }

    fn result() -> SubagentResult {
        let agent = super::super::super::tan_agent_definition();
        let task = serde_json::from_value(json!({"agent":"tan","task":"pipe fixture"})).unwrap();
        SubagentResult::starting(&agent, task, None, Path::new("pi"), Path::new("."), &[])
    }

    type SocketPipes = ChildPipes<UnixStream, UnixStream>;

    fn pair() -> (SocketPipes, UnixStream, UnixStream) {
        let (stdout, out) = UnixStream::pair().unwrap();
        let (stderr, err) = UnixStream::pair().unwrap();
        let pipes = ChildPipes::new(stdout, stderr).unwrap();
        assert_nonblocking(&pipes.stdout);
        assert_nonblocking(&pipes.stderr);
        (pipes, out, err)
    }

    fn ended() -> String {
        json!({"type":"agent_end","messages":[{
            "role":"assistant","stopReason":"stop",
            "content":[{"type":"text","text":"complete answer"}]
        }]})
        .to_string()
    }

    fn assert_peer_closed(mut peer: UnixStream) {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(
            peer.read(&mut [0_u8; 1]).unwrap(),
            0,
            "reader descriptor survived drop"
        );
    }

    #[test]
    fn would_block_is_not_eof_and_preserves_a_partial_utf8_frame() {
        let (mut pipe, mut writer) = socket_pipe(32);
        assert_eq!(frame(&mut pipe).unwrap(), None);
        assert!(pipe.reader.is_some());
        writer.write_all(&[b'a', 0xe7]).unwrap();
        assert_eq!(frame(&mut pipe).unwrap(), None);
        assert_eq!(pipe.partial, [b'a', 0xe7]);
        writer.write_all(&[0x95, 0x8c, b'\n']).unwrap();
        assert_eq!(frame(&mut pipe).unwrap().unwrap(), "a界".as_bytes());
        assert!(pipe.partial.is_empty());
        assert!(pipe.reader.is_some());
    }

    #[test]
    fn nonblocking_framing_matches_the_existing_complete_frame_reader() {
        for input in [
            &b""[..],
            &b"\n"[..],
            &b"one\r\ntwo\nlast"[..],
            &b"final\r"[..],
            "日本語\n🦀\r\n".as_bytes(),
            &b"\n\n\r\n"[..],
        ] {
            let (mut pipe, mut writer) = socket_pipe(64);
            writer.write_all(input).unwrap();
            writer.shutdown(Shutdown::Write).unwrap();
            let mut expected = Vec::new();
            let mut reference = Cursor::new(input);
            while let Some(bytes) = protocol::read_frame(&mut reference).unwrap() {
                expected.push(bytes);
            }
            let mut actual = Vec::new();
            for _ in 0..32 {
                let mut budget = 1;
                if let Some(bytes) = pipe.next_frame(&mut budget).unwrap() {
                    actual.push(bytes);
                }
                if pipe.reader.is_none() {
                    break;
                }
            }
            assert!(pipe.reader.is_none(), "EOF was not consumed");
            assert_eq!(actual, expected, "{input:?}");
        }
    }

    #[test]
    fn an_exact_size_frame_can_wait_for_its_delimiter_without_overflowing() {
        let (mut pipe, mut writer) = socket_pipe(4);
        writer.write_all(b"abcd").unwrap();
        assert_eq!(frame(&mut pipe).unwrap(), None);
        writer.write_all(b"\nnext\n").unwrap();
        assert_eq!(frame(&mut pipe).unwrap(), Some(b"abcd".to_vec()));
        assert_eq!(frame(&mut pipe).unwrap(), Some(b"next".to_vec()));
    }

    #[test]
    fn a_one_byte_overflow_closes_the_reader_and_drops_the_partial_frame() {
        let (mut pipe, mut writer) = socket_pipe(4);
        writer.write_all(b"abcd").unwrap();
        assert_eq!(frame(&mut pipe).unwrap(), None);
        writer.write_all(b"e").unwrap();
        assert_eq!(frame(&mut pipe), Err(protocol::FRAME_LIMIT));
        assert!(pipe.reader.is_none());
        assert!(pipe.partial.is_empty());
        assert_eq!(frame(&mut pipe).unwrap(), None);
    }

    #[test]
    fn continuous_unterminated_output_gets_only_its_per_poll_byte_budget() {
        let zero = std::fs::File::open("/dev/zero").unwrap();
        let mut pipe = FramedPipe::new(zero, protocol::MAX_FRAME_BYTES).unwrap();
        let mut budget = 2;
        assert_eq!(pipe.next_frame(&mut budget).unwrap(), None);
        assert_eq!(budget, 0);
        assert_eq!(pipe.partial.len(), 2 * BUFFER_BYTES);
        assert_eq!(pipe.next_frame(&mut budget).unwrap(), None);
        assert_eq!(pipe.partial.len(), 2 * BUFFER_BYTES);
    }

    #[test]
    fn stderr_drains_while_stdout_is_an_incomplete_frame() {
        let (mut pipes, mut out, mut err) = pair();
        out.write_all(b"{\"type\":").unwrap();
        err.write_all(b"diagnostic\n").unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        pipes.drain(&mut protocol, &mut result, None);
        assert!(!result.is_error);
        assert!(result.stderr.contains("diagnostic"));
        assert!(!pipes.stdout.partial.is_empty());
        assert!(!pipes.is_closed());
    }

    #[test]
    fn stdout_is_strict_utf8_but_stderr_keeps_lossy_diagnostics() {
        let (mut pipes, mut out, mut err) = pair();
        out.write_all(b"\xff\n").unwrap();
        err.write_all(b"note\xff\n").unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        pipes.drain(&mut protocol, &mut result, None);
        assert!(result.is_error);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("stdout is not UTF-8")
        );
        assert!(result.stderr.contains("note\u{fffd}"));
    }

    #[test]
    fn completion_and_late_failure_are_both_read_across_many_drain_slices() {
        for malformed_tail in [false, true] {
            let (mut pipes, mut out, err) = pair();
            // More than one drain's frame quota, but less than socket capacity.
            for _ in 0..DRAIN_BATCH * 2 {
                writeln!(out, "{{\"type\":\"usage\"}}").unwrap();
            }
            writeln!(out, "{}", ended()).unwrap();
            if malformed_tail {
                writeln!(out, "not-json").unwrap();
            }
            out.shutdown(Shutdown::Write).unwrap();
            err.shutdown(Shutdown::Write).unwrap();
            let mut protocol = protocol::ChildProtocol::default();
            let mut result = result();
            pipes.drain(&mut protocol, &mut result, None);
            assert!(protocol.finish().is_err(), "quota must not be skipped");
            for _ in 0..16 {
                pipes.drain(&mut protocol, &mut result, None);
                if pipes.is_closed() || result.is_error {
                    break;
                }
            }
            assert_eq!(result.is_error, malformed_tail);
            assert_eq!(protocol.finish().is_err(), malformed_tail);
            assert_eq!(result.output, "complete answer");
        }
    }

    #[test]
    fn success_requires_both_eofs_and_accepts_a_final_frame_without_newline() {
        let (pipes, mut out, mut err) = pair();
        out.write_all(ended().as_bytes()).unwrap();
        err.write_all(b"last diagnostic").unwrap();
        out.shutdown(Shutdown::Write).unwrap();
        err.shutdown(Shutdown::Write).unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        runtime.block_on(pipes.finish(&mut protocol, &mut result, None, &owner, deadline));
        assert!(!result.is_error, "{:?}", result.error);
        assert!(protocol.finish().is_ok());
        assert_eq!(result.output, "complete answer");
        assert!(result.stderr.contains("last diagnostic"));
        assert_peer_closed(out);
        assert_peer_closed(err);
    }

    #[test]
    fn held_open_peer_cannot_authorize_success_after_the_drain_deadline() {
        let (pipes, mut out, err) = pair();
        writeln!(out, "{}", ended()).unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        runtime.block_on(pipes.finish_with_timeout(
            &mut protocol,
            &mut result,
            None,
            &owner,
            deadline,
            Duration::ZERO,
        ));
        assert!(
            protocol.finish().is_ok(),
            "a valid answer alone is insufficient"
        );
        assert!(result.is_error);
        assert_eq!(result.error.as_deref(), Some(PIPE_TIMEOUT));
        assert_peer_closed(out);
        assert_peer_closed(err);
    }

    #[test]
    fn cancellation_closes_open_peers_without_waiting_for_eof() {
        let (pipes, out, err) = pair();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        owner.cancel_with(
            asupersync::types::CancelKind::User,
            Some("cancel child drain"),
        );
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        runtime.block_on(async {
            let mut finish =
                Box::pin(pipes.finish(&mut protocol, &mut result, None, &owner, deadline));
            assert!(
                futures::poll!(&mut finish).is_ready(),
                "cancellation must not await EOF"
            );
        });
        assert!(matches!(
            result.status,
            super::super::SubagentStatus::Cancelled
        ));
        assert_peer_closed(out);
        assert_peer_closed(err);
    }

    #[test]
    fn dropping_a_pending_drain_closes_readers_despite_live_writer_peers() {
        let (pipes, out, err) = pair();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        runtime.block_on(async {
            let mut finish =
                Box::pin(pipes.finish(&mut protocol, &mut result, None, &owner, deadline));
            assert!(futures::poll!(&mut finish).is_pending());
            drop(finish);
        });
        assert_peer_closed(out);
        assert_peer_closed(err);
    }

    #[test]
    fn real_child_output_larger_than_both_pipe_capacities_is_drained() {
        use std::os::unix::process::CommandExt as _;
        use std::process::{Command, Stdio};

        let script = format!(
            "i=0; while [ \"$i\" -lt 4000 ]; do \
             printf '%s\\n' '{{\"type\":\"usage\"}}'; \
             printf '%s\\n' 'diagnostic frame to fill the stderr pipe' >&2; \
             i=$((i + 1)); done; printf '%s\\n' '{}'",
            ended()
        );
        let raw = Command::new("/bin/sh")
            .args(["-c", &script])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = super::super::ChildProcessGuard::new(raw);
        let stdout = child.child.as_mut().unwrap().stdout.take().unwrap();
        let stderr = child.child.as_mut().unwrap().stderr.take().unwrap();
        let mut pipes = ChildPipes::new(stdout, stderr).unwrap();
        assert_nonblocking(&pipes.stdout);
        assert_nonblocking(&pipes.stderr);
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        let started = Instant::now();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "pipe drainage stalled"
            );
            pipes.drain(&mut protocol, &mut result, None);
            assert!(!result.is_error, "{:?}", result.error);
            if let Some(status) = child.child.as_mut().unwrap().try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        child.stop_descendants();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        runtime.block_on(pipes.finish(&mut protocol, &mut result, None, &owner, deadline));
        assert!(!result.is_error, "{:?}", result.error);
        assert!(protocol.finish().is_ok());
        assert_eq!(result.output, "complete answer");
        assert!(result.stderr.contains("diagnostic frame"));
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use serde_json::json;
    use std::fs::File;
    use std::io::Write;
    use std::os::windows::process::CommandExt as _;
    use std::path::Path;

    type WindowsPipes = ChildPipes<WindowsReader, WindowsReader>;

    fn pair() -> (WindowsPipes, File, File) {
        let (stdout, out) = crate::jobs::windows_job_pipe::pair().unwrap();
        let (stderr, err) = crate::jobs::windows_job_pipe::pair().unwrap();
        (ChildPipes::from_readers(stdout, stderr), out, err)
    }

    fn result() -> SubagentResult {
        let agent = super::super::super::tan_agent_definition();
        let task = serde_json::from_value(json!({"agent":"tan","task":"pipe fixture"})).unwrap();
        SubagentResult::starting(&agent, task, None, Path::new("pi"), Path::new("."), &[])
    }

    fn ended() -> String {
        json!({"type":"agent_end","messages":[{
            "role":"assistant","stopReason":"stop",
            "content":[{"type":"text","text":"complete answer"}]
        }]})
        .to_string()
    }

    fn frame(pipe: &mut FramedPipe<WindowsReader>) -> Result<Option<Vec<u8>>, &'static str> {
        let mut budget = CHUNKS_PER_DRAIN;
        pipe.next_frame(&mut budget)
    }

    fn ready_frame(pipe: &mut FramedPipe<WindowsReader>) -> Result<Vec<u8>, &'static str> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(frame) = frame(pipe)? {
                return Ok(frame);
            }
            assert!(!pipe.is_closed(), "writer closed without a frame");
            assert!(
                Instant::now() < deadline,
                "overlapped output did not arrive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_writer_closed(writer: &mut File) {
        // Probe fewer bytes than the named pipe's capacity. If retirement
        // leaks its server handle, a retained writer stays writable instead
        // of blocking this test behind a full pipe.
        for _ in 0..200 {
            match writer.write(b"x") {
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return,
                Ok(1) => std::thread::sleep(Duration::from_millis(10)),
                result => panic!("unexpected reader-retirement probe: {result:?}"),
            }
        }
        panic!("dropped subagent reader retained its Windows server handle");
    }

    #[test]
    fn overlapped_framing_retains_split_utf8_and_enforces_the_frame_limit() {
        let _guard = crate::jobs::process_test_guard();
        let (reader, mut writer) = crate::jobs::windows_job_pipe::pair().unwrap();
        let mut pipe = FramedPipe::from_nonblocking(WindowsReader(reader), 4);
        assert_eq!(frame(&mut pipe).unwrap(), None);
        assert!(!pipe.is_closed(), "an idle writer is not EOF");
        writer.write_all(&[b'a', 0xe7]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while pipe.partial.len() < 2 {
            assert_eq!(frame(&mut pipe).unwrap(), None);
            assert!(Instant::now() < deadline, "partial output did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pipe.partial, [b'a', 0xe7]);
        writer.write_all(&[0x95, 0x8c, b'\n']).unwrap();
        assert_eq!(ready_frame(&mut pipe).unwrap(), "a界".as_bytes());
        assert!(!pipe.is_closed());
        writer.write_all(b"abcde").unwrap();
        assert_eq!(ready_frame(&mut pipe), Err(protocol::FRAME_LIMIT));
        assert!(pipe.is_closed());
        assert!(pipe.partial.is_empty());
        assert_writer_closed(&mut writer);
    }

    #[test]
    fn pending_and_cancelled_drains_retire_readers_with_duplicated_writers_held() {
        let _guard = crate::jobs::process_test_guard();
        for cancel_before_poll in [false, true] {
            let (pipes, out, err) = pair();
            // These copies model writers retained outside the child Job.
            // Neither killing the child nor closing its original handles can
            // supply EOF; dropping the future must retire both read handles.
            let mut retained_out = out.try_clone().unwrap();
            let mut retained_err = err.try_clone().unwrap();
            drop(out);
            drop(err);
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .unwrap();
            let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
            if cancel_before_poll {
                owner.cancel_with(
                    asupersync::types::CancelKind::User,
                    Some("cancel Windows child drain"),
                );
            }
            let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
            let mut protocol = protocol::ChildProtocol::default();
            let mut result = result();
            runtime.block_on(async {
                let mut finish =
                    Box::pin(pipes.finish(&mut protocol, &mut result, None, &owner, deadline));
                assert_eq!(futures::poll!(&mut finish).is_ready(), cancel_before_poll);
                drop(finish);
            });
            if cancel_before_poll {
                assert!(matches!(
                    result.status,
                    super::super::SubagentStatus::Cancelled
                ));
            }
            assert_writer_closed(&mut retained_out);
            assert_writer_closed(&mut retained_err);
        }
    }

    #[test]
    fn a_completed_answer_cannot_hide_writers_held_past_the_drain_deadline() {
        let _guard = crate::jobs::process_test_guard();
        let (mut pipes, mut out, mut err) = pair();
        writeln!(out, "{}", ended()).unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        let arrived_by = Instant::now() + Duration::from_secs(2);
        while result.output.is_empty() {
            pipes.drain(&mut protocol, &mut result, None);
            assert!(!result.is_error, "{:?}", result.error);
            assert!(Instant::now() < arrived_by, "completion did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(protocol.finish().is_ok());
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        runtime.block_on(pipes.finish_with_timeout(
            &mut protocol,
            &mut result,
            None,
            &owner,
            deadline,
            Duration::ZERO,
        ));
        assert!(result.is_error);
        assert_eq!(result.error.as_deref(), Some(PIPE_TIMEOUT));
        assert_writer_closed(&mut out);
        assert_writer_closed(&mut err);
    }

    #[test]
    fn real_child_drains_both_overlapped_streams_and_reaches_eof() {
        let _guard = crate::jobs::process_test_guard();
        let script = format!(
            r#"for /L %i in (1,1,4000) do @(echo {{"type":"usage"}} & echo diagnostic frame to fill the stderr pipe 1>&2) & echo {}"#,
            ended()
        );
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(30)), None).unwrap();
        let mut command = Command::new("cmd.exe");
        // cmd does not use CRT argument escaping. /S removes only the outer
        // quote pair, leaving this controlled script's JSON quotes intact.
        command
            .args(["/D", "/Q", "/E:ON", "/S", "/C"])
            .raw_arg(format!("\"{script}\""))
            .stdin(Stdio::null());
        let mut pipes = ChildPipes::configure(&mut command).unwrap();
        let mut child = super::super::ChildProcessGuard::spawn(&owner, &mut command).unwrap();
        // Match the production handoff. Keeping these parent writer copies
        // would prevent the EOF required for successful protocol acceptance.
        drop(command);
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        runtime.block_on(async {
            loop {
                assert!(deadline.check().is_ok(), "child output drainage stalled");
                pipes.drain(&mut protocol, &mut result, None);
                assert!(!result.is_error, "{:?}", result.error);
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    break;
                }
                poll_pause(&owner).await;
            }
            child.stop_descendants();
            pipes
                .finish(&mut protocol, &mut result, None, &owner, deadline)
                .await;
        });
        assert!(!result.is_error, "{:?}", result.error);
        assert!(protocol.finish().is_ok());
        assert_eq!(result.output, "complete answer");
        assert!(result.stderr.contains("diagnostic frame"));
        child.disarm();
    }
}
