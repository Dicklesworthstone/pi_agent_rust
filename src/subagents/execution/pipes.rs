//! Cancellation-owned Unix transport for the streaming child protocol.
//!
//! A child may exit while a descendant still holds either pipe's write end.
//! Blocking readers cannot then be joined, even after their receiver is dropped.
//! Here the run future owns nonblocking descriptors and bounded partial frames;
//! dropping it closes the read ends without waiting for EOF or leaving threads.

use super::{
    AgentCx, DRAIN_BATCH, Deadline, PIPE_DRAIN_TIMEOUT, PipeFrame, PipeKind, SubagentResult,
    UpdateCallback, apply_child_frame, check_budget, poll_pause, protocol,
};
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::AsFd;
use std::process::{ChildStderr, ChildStdout};
use std::time::{Duration, Instant};

const BUFFER_BYTES: usize = 8192;
// Limit work on incomplete frames too, not just the number of completed lines.
// A continuously writing stream cannot monopolize an executor or starve its peer.
const CHUNKS_PER_DRAIN: usize = 32;
const PIPE_TIMEOUT: &str =
    "PI_SUBAGENT_PIPE_TIMEOUT: child pipes did not close after process termination";

struct FramedPipe<R> {
    reader: Option<BufReader<R>>,
    partial: Vec<u8>,
    frame_limit: usize,
}

impl<R: Read + AsFd> FramedPipe<R> {
    fn new(pipe: R, frame_limit: usize) -> io::Result<Self> {
        // Preserve all unrelated flags. The descriptors are private to this
        // transport; nothing else reads or switches them back to blocking mode.
        let flags = rustix::fs::fcntl_getfl(&pipe).map_err(io::Error::from)?;
        rustix::fs::fcntl_setfl(&pipe, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(io::Error::from)?;
        Ok(Self {
            reader: Some(BufReader::with_capacity(BUFFER_BYTES, pipe)),
            partial: Vec::new(),
            frame_limit,
        })
    }
}

impl<R: Read> FramedPipe<R> {
    /// `None` means no complete frame this slice, not necessarily EOF. Retain
    /// partial bytes across WouldBlock, including a split multibyte character.
    /// CRLF, empty lines and a final unterminated frame match `read_frame`.
    fn next_frame(&mut self, chunks_left: &mut usize) -> Result<Option<Vec<u8>>, &'static str> {
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
}

pub(super) struct ChildPipes<O = ChildStdout, E = ChildStderr> {
    stdout: FramedPipe<O>,
    stderr: FramedPipe<E>,
}

impl<O: Read + AsFd, E: Read + AsFd> ChildPipes<O, E> {
    pub(super) fn new(stdout: O, stderr: E) -> io::Result<Self> {
        Ok(Self {
            stdout: FramedPipe::new(stdout, protocol::MAX_FRAME_BYTES)?,
            stderr: FramedPipe::new(stderr, protocol::MAX_FRAME_BYTES)?,
        })
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
            PipeKind::Stdout => match String::from_utf8(bytes) {
                Ok(line) => PipeFrame::Data(kind, line),
                Err(_) => PipeFrame::Error("PI_SUBAGENT_PROTOCOL: child stdout is not UTF-8"),
            },
        }),
        Ok(None) => None,
        Err(error) => Some(PipeFrame::Error(error)),
    }
}

impl<O: Read, E: Read> ChildPipes<O, E> {
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

    fn is_closed(&self) -> bool {
        self.stdout.reader.is_none() && self.stderr.reader.is_none()
    }

    pub(super) async fn finish(
        self,
        protocol: &mut protocol::ChildProtocol,
        result: &mut SubagentResult,
        update: Option<&UpdateCallback>,
        owner: &AgentCx,
        work_deadline: Deadline,
    ) {
        self.finish_with_timeout(protocol, result, update, owner, work_deadline, PIPE_DRAIN_TIMEOUT)
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

#[cfg(test)]
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
        }]}).to_string()
    }

    fn assert_peer_closed(mut peer: UnixStream) {
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0_u8; 1]).unwrap(), 0, "reader descriptor survived drop");
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
            &b""[..], &b"\n"[..], &b"one\r\ntwo\nlast"[..],
            &b"final\r"[..], "日本語\n🦀\r\n".as_bytes(), &b"\n\n\r\n"[..],
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
        assert!(result.error.as_deref().unwrap().contains("stdout is not UTF-8"));
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
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
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
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        runtime.block_on(pipes.finish_with_timeout(
            &mut protocol, &mut result, None, &owner, deadline, Duration::ZERO,
        ));
        assert!(protocol.finish().is_ok(), "a valid answer alone is insufficient");
        assert!(result.is_error);
        assert_eq!(result.error.as_deref(), Some(PIPE_TIMEOUT));
        assert_peer_closed(out);
        assert_peer_closed(err);
    }

    #[test]
    fn cancellation_closes_open_peers_without_waiting_for_eof() {
        let (pipes, out, err) = pair();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        owner.cancel_with(asupersync::types::CancelKind::User, Some("cancel child drain"));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        runtime.block_on(async {
            let mut finish = Box::pin(pipes.finish(&mut protocol, &mut result, None, &owner, deadline));
            assert!(futures::poll!(&mut finish).is_ready(), "cancellation must not await EOF");
        });
        assert!(matches!(result.status, super::super::SubagentStatus::Cancelled));
        assert_peer_closed(out);
        assert_peer_closed(err);
    }

    #[test]
    fn dropping_a_pending_drain_closes_readers_despite_live_writer_peers() {
        let (pipes, out, err) = pair();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        let mut protocol = protocol::ChildProtocol::default();
        let mut result = result();
        runtime.block_on(async {
            let mut finish = Box::pin(pipes.finish(&mut protocol, &mut result, None, &owner, deadline));
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
            assert!(started.elapsed() < Duration::from_secs(10), "pipe drainage stalled");
            pipes.drain(&mut protocol, &mut result, None);
            assert!(!result.is_error, "{:?}", result.error);
            if let Some(status) = child.child.as_mut().unwrap().try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        child.stop_descendants();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread().build().unwrap();
        let owner = AgentCx::from_cx(runtime.request_cx_with_budget(asupersync::Budget::new()));
        let deadline = Deadline::for_request(Some(Duration::from_secs(10)), None).unwrap();
        runtime.block_on(pipes.finish(&mut protocol, &mut result, None, &owner, deadline));
        assert!(!result.is_error, "{:?}", result.error);
        assert!(protocol.finish().is_ok());
        assert_eq!(result.output, "complete answer");
        assert!(result.stderr.contains("diagnostic frame"));
    }
}
