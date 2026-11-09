//! Cancellation-aware native MCP pipes.
//!
//! Process-tree cleanup does not guarantee pipe EOF: a descendant can leave
//! the process group or retain a duplicated handle. Keep all parent endpoints
//! nonblocking, but present ordinary blocking-style Read/Write to the existing
//! frame decoder and single writer. WouldBlock must not escape into the frame
//! decoder: it owns a partially assembled newline-delimited JSON message.

use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use std::process::{ChildStderr, ChildStdin, ChildStdout};
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const RETRY_INTERVAL: Duration = Duration::from_millis(25);

/// The parent endpoints only. The child's opposite pipe ends retain their
/// ordinary blocking behavior. Prepare every endpoint before starting workers.
#[cfg(unix)]
pub struct StdioPipes {
    pub(crate) stdin: CancellablePipe<ChildStdin>,
    pub(crate) stdout: CancellablePipe<ChildStdout>,
    pub(crate) stderr: CancellablePipe<ChildStderr>,
}

#[cfg(unix)]
pub fn prepare(
    stdin: ChildStdin,
    stdout: ChildStdout,
    stderr: ChildStderr,
    alive: &Arc<AtomicBool>,
) -> io::Result<StdioPipes> {
    Ok(StdioPipes {
        stdin: CancellablePipe::new(stdin, Arc::clone(alive))?,
        stdout: CancellablePipe::new(stdout, Arc::clone(alive))?,
        stderr: CancellablePipe::new(stderr, Arc::clone(alive))?,
    })
}

/// Windows anonymous child pipes cannot be made nonblocking after spawn.
/// Prepare synchronous child handles backed by the existing shared overlapped
/// reactor before starting the child, then retire the command's handle copies.
#[cfg(windows)]
pub(crate) struct WindowsStdioPipes {
    pub(crate) stdin: CancellablePipe<WindowsInput>,
    pub(crate) stdout: CancellablePipe<WindowsOutput>,
    pub(crate) stderr: CancellablePipe<WindowsOutput>,
}

#[cfg(windows)]
pub(crate) fn prepare_command(
    command: &mut Command,
    alive: &Arc<AtomicBool>,
) -> io::Result<WindowsStdioPipes> {
    use crate::jobs::windows_job_pipe;

    let (stdin, child_stdin) = windows_job_pipe::stdin_pair()?;
    let (stdout, child_stdout) = windows_job_pipe::pair()?;
    let (stderr, child_stderr) = windows_job_pipe::pair()?;
    // Configure only after every pair exists. A preparation failure drops all
    // admitted endpoints without spawning a child or a per-connection worker.
    command
        .stdin(Stdio::from(child_stdin))
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr));
    Ok(WindowsStdioPipes {
        stdin: CancellablePipe {
            inner: WindowsInput(stdin),
            alive: Arc::clone(alive),
        },
        stdout: CancellablePipe {
            inner: WindowsOutput(stdout),
            alive: Arc::clone(alive),
        },
        stderr: CancellablePipe {
            inner: WindowsOutput(stderr),
            alive: Arc::clone(alive),
        },
    })
}

#[cfg(windows)]
pub(crate) struct WindowsInput(crate::jobs::windows_job_pipe::Writer);

#[cfg(windows)]
impl Write for WindowsInput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.try_write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        // As with Mio's NamedPipe::flush, accepted writes remain owned by the
        // reactor until completion. This is not a server acknowledgment; the
        // JSON-RPC response supplies that. Drop cancels pending input instead
        // of waiting for an unresponsive or retained client handle to read it.
        Ok(())
    }
}

#[cfg(windows)]
pub(crate) struct WindowsOutput(crate::jobs::windows_job_pipe::Reader);

#[cfg(windows)]
impl Read for WindowsOutput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.try_read(buffer)
    }
}

pub struct CancellablePipe<T> {
    inner: T,
    alive: Arc<AtomicBool>,
}

#[cfg(unix)]
impl<T: AsFd> CancellablePipe<T> {
    fn new(inner: T, alive: Arc<AtomicBool>) -> io::Result<Self> {
        let flags = rustix::fs::fcntl_getfl(&inner)?;
        rustix::fs::fcntl_setfl(&inner, flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(Self { inner, alive })
    }
}

impl<T> CancellablePipe<T> {
    fn retry<R>(&mut self, mut operation: impl FnMut(&mut T) -> io::Result<R>) -> io::Result<R> {
        loop {
            // Check even after Interrupted: a signal storm must not prevent
            // teardown. Never report cancellation as EOF or return success for
            // an incomplete write; that would conceal uncertain delivery.
            if !self.alive.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "MCP stdio connection stopped",
                ));
            }
            match operation(&mut self.inner) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    // These are dedicated OS workers, not async executor tasks.
                    // Retrying allocates no buffer and holds no mutex.
                    std::thread::sleep(RETRY_INTERVAL);
                }
                result => return result,
            }
        }
    }
}

impl<T: Read> Read for CancellablePipe<T> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.retry(|inner| inner.read(buffer))
    }
}

impl<T: Write> Write for CancellablePipe<T> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        // Preserve partial-write counts. The standard write_all advances its
        // remaining slice only after successful writes, never on WouldBlock.
        self.retry(|inner| inner.write(buffer))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.retry(Write::flush)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::BufReader;
    #[cfg(unix)]
    use std::os::fd::BorrowedFd;
    use std::sync::mpsc::{SyncSender, sync_channel};

    fn live() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(true))
    }

    enum ReadStep {
        Bytes(VecDeque<u8>),
        Error(io::ErrorKind),
        Stop(Arc<AtomicBool>),
    }

    struct ScriptedReader(VecDeque<ReadStep>);

    impl Read for ScriptedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            loop {
                match self.0.front_mut() {
                    None => return Ok(0),
                    Some(ReadStep::Error(kind)) => {
                        let kind = *kind;
                        let _ = self.0.pop_front();
                        return Err(io::Error::from(kind));
                    }
                    Some(ReadStep::Stop(alive)) => {
                        alive.store(false, Ordering::SeqCst);
                        let _ = self.0.pop_front();
                        return Err(io::ErrorKind::Interrupted.into());
                    }
                    Some(ReadStep::Bytes(bytes)) if bytes.is_empty() => {
                        let _ = self.0.pop_front();
                    }
                    Some(ReadStep::Bytes(bytes)) => {
                        let count = buffer.len().min(bytes.len());
                        for byte in &mut buffer[..count] {
                            *byte = bytes.pop_front().expect("count bounded by available bytes");
                        }
                        return Ok(count);
                    }
                }
            }
        }
    }

    fn bytes(value: &[u8]) -> ReadStep {
        ReadStep::Bytes(value.iter().copied().collect())
    }

    #[test]
    #[cfg(unix)]
    fn nonblocking_setup_preserves_flags_and_the_opposite_endpoint() {
        let (reader, writer) = io::pipe().expect("native pipe");
        let reader_flags = rustix::fs::fcntl_getfl(&reader).expect("reader flags");
        let writer_flags = rustix::fs::fcntl_getfl(&writer).expect("writer flags");
        let reader = CancellablePipe::new(reader, live()).expect("prepare reader");
        assert_eq!(
            rustix::fs::fcntl_getfl(&reader.inner).expect("prepared reader flags"),
            reader_flags | rustix::fs::OFlags::NONBLOCK
        );
        assert_eq!(
            rustix::fs::fcntl_getfl(&writer).expect("writer flags"),
            writer_flags
        );
    }

    #[test]
    fn fragmented_utf8_frames_survive_idle_gaps_and_interruptions() {
        let first = serde_json::json!({"jsonrpc":"2.0", "id":1, "result":{"text":"界"}});
        let second = serde_json::json!({"jsonrpc":"2.0", "id":2, "result":{}});
        let encoded = serde_json::to_string(&first).expect("first frame");
        let split = encoded.find('界').expect("multibyte fixture") + 1;
        // Split inside a UTF-8 scalar, then complete the first frame and place
        // a second frame in the same incoming chunk. No intermediate decode.
        let mut final_chunk = encoded.as_bytes()[split..].to_vec();
        final_chunk.extend_from_slice(b"\r\n");
        final_chunk.extend_from_slice(
            serde_json::to_string(&second)
                .expect("second frame")
                .as_bytes(),
        );
        final_chunk.push(b'\n');
        let source = ScriptedReader(VecDeque::from([
            bytes(&encoded.as_bytes()[..split]),
            ReadStep::Error(io::ErrorKind::WouldBlock),
            ReadStep::Error(io::ErrorKind::Interrupted),
            bytes(&final_chunk),
        ]));
        let mut reader = BufReader::with_capacity(
            7,
            CancellablePipe {
                inner: source,
                alive: live(),
            },
        );
        assert_eq!(
            super::super::read_stdio_message(&mut reader).expect("first message"),
            Some(first)
        );
        assert_eq!(
            super::super::read_stdio_message(&mut reader).expect("second message"),
            Some(second)
        );
        assert_eq!(
            super::super::read_stdio_message(&mut reader).expect("real EOF"),
            None
        );
    }

    #[test]
    fn idle_gaps_do_not_reset_the_frame_limit() {
        let source = ScriptedReader(VecDeque::from([
            bytes(b"{\"text\":"),
            ReadStep::Error(io::ErrorKind::WouldBlock),
            bytes(b"\"too large\"}\n"),
        ]));
        let mut reader = BufReader::new(CancellablePipe {
            inner: source,
            alive: live(),
        });
        let error = super::super::read_stdio_message_with_limit(&mut reader, 12)
            .expect_err("aggregate frame limit must survive an idle gap");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn true_eof_in_a_partial_frame_remains_a_protocol_error() {
        let source = ScriptedReader(VecDeque::from([
            bytes(b"{\"jsonrpc\":\"2.0\""),
            ReadStep::Error(io::ErrorKind::Interrupted),
        ]));
        let mut reader = BufReader::new(CancellablePipe {
            inner: source,
            alive: live(),
        });
        let error = super::super::read_stdio_message(&mut reader).expect_err("unterminated frame");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn cancellation_of_a_partial_frame_is_not_eof_or_invalid_json() {
        let alive = live();
        let source = ScriptedReader(VecDeque::from([
            bytes(b"{\"jsonrpc\":\"2.0\""),
            ReadStep::Stop(Arc::clone(&alive)),
        ]));
        let mut reader = BufReader::new(CancellablePipe {
            inner: source,
            alive,
        });
        let error =
            super::super::read_stdio_message(&mut reader).expect_err("cancelled partial frame");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
    }

    #[test]
    fn stopped_reader_does_not_consume_buffered_peer_data() {
        let source = ScriptedReader(VecDeque::from([bytes(b"private")]));
        let mut reader = CancellablePipe {
            inner: source,
            alive: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(reader.read(&mut []).expect("empty read"), 0);
        let mut buffer = [0u8; 7];
        let error = reader.read(&mut buffer).expect_err("stopped read");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(buffer, [0u8; 7]);
        assert!(matches!(
            reader.inner.0.front(),
            Some(ReadStep::Bytes(bytes)) if bytes.len() == 7
        ));
    }

    struct ShortWriter {
        attempts: usize,
        written: Vec<u8>,
        flushes: usize,
    }

    impl Write for ShortWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.attempts += 1;
            match self.attempts {
                2 => return Err(io::ErrorKind::WouldBlock.into()),
                4 => return Err(io::ErrorKind::Interrupted.into()),
                _ => {}
            }
            let count = buffer.len().min(3);
            self.written.extend_from_slice(&buffer[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn backpressured_partial_writes_do_not_duplicate_request_bytes() {
        let mut writer = CancellablePipe {
            inner: ShortWriter {
                attempts: 0,
                written: Vec::new(),
                flushes: 0,
            },
            alive: live(),
        };
        let frame = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
        writer.write_all(frame).expect("complete frame");
        writer.flush().expect("flush frame");
        assert_eq!(writer.inner.written.as_slice(), frame.as_slice());
        assert_eq!(writer.inner.flushes, 1);
    }

    #[test]
    fn stopped_connection_does_not_write_or_flush() {
        let mut writer = CancellablePipe {
            inner: ShortWriter {
                attempts: 0,
                written: Vec::new(),
                flushes: 0,
            },
            alive: Arc::new(AtomicBool::new(false)),
        };
        assert_eq!(
            writer.write(b"secret").expect_err("stopped write").kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(
            writer.flush().expect_err("stopped flush").kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert_eq!(writer.inner.attempts, 0);
        assert_eq!(writer.inner.flushes, 0);
        assert!(writer.inner.written.is_empty());
        assert_eq!(writer.write(&[]).expect("empty write"), 0);
    }

    #[test]
    fn cancellation_is_rechecked_after_interrupted_io() {
        let alive = live();
        let stop = Arc::clone(&alive);
        let mut pipe = CancellablePipe {
            inner: 0usize,
            alive,
        };
        let error = pipe
            .retry(|attempts| -> io::Result<()> {
                *attempts += 1;
                stop.store(false, Ordering::SeqCst);
                Err(io::ErrorKind::Interrupted.into())
            })
            .expect_err("interruption must not swallow cancellation");
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(pipe.inner, 1);
    }

    #[test]
    fn fatal_io_errors_are_not_retried_or_disguised_as_eof() {
        let mut pipe = CancellablePipe {
            inner: 0usize,
            alive: live(),
        };
        let error = pipe
            .retry(|attempts| -> io::Result<()> {
                *attempts += 1;
                Err(io::ErrorKind::PermissionDenied.into())
            })
            .expect_err("fatal I/O error");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(pipe.inner, 1);
    }

    /// Signal only after the kernel reports actual backpressure. Tests never
    /// infer that a worker is blocked merely from a sleep or thread start.
    struct ObserveBlocked<T> {
        inner: T,
        blocked: Option<SyncSender<()>>,
    }

    impl<T> ObserveBlocked<T> {
        fn observe<R>(&mut self, result: io::Result<R>) -> io::Result<R> {
            if result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
                && let Some(sender) = self.blocked.take()
            {
                let _ = sender.try_send(());
            }
            result
        }
    }

    #[cfg(unix)]
    impl<T: AsFd> AsFd for ObserveBlocked<T> {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.inner.as_fd()
        }
    }

    impl<T: Read> Read for ObserveBlocked<T> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let result = self.inner.read(buffer);
            self.observe(result)
        }
    }

    impl<T: Write> Write for ObserveBlocked<T> {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let result = self.inner.write(buffer);
            self.observe(result)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    #[test]
    #[cfg(unix)]
    fn idle_native_reader_stops_with_writer_retained_and_retires_its_fd() {
        for _ in 0..8 {
            let (reader, mut writer) = io::pipe().expect("native pipe");
            let alive = live();
            let (blocked_tx, blocked_rx) = sync_channel(1);
            let (done_tx, done_rx) = sync_channel(1);
            let observed = ObserveBlocked {
                inner: reader,
                blocked: Some(blocked_tx),
            };
            let mut reader =
                CancellablePipe::new(observed, Arc::clone(&alive)).expect("prepare reader");
            let worker = std::thread::spawn(move || {
                let result = reader.read(&mut [0u8]);
                drop(reader);
                let _ = done_tx.send(result.map_err(|error| error.kind()));
            });
            let blocked = blocked_rx.recv_timeout(Duration::from_secs(2));
            alive.store(false, Ordering::SeqCst);
            let stopped_with_writer_held = done_rx.recv_timeout(Duration::from_secs(2));
            if stopped_with_writer_held.is_err() {
                // Release the peer even on regression, so a blocking reader
                // can retire instead of leaking a worker from a failing test.
                drop(writer);
                let _ = done_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("reader retires after releasing its peer");
                worker.join().expect("reader worker");
                panic!("reader did not stop while the peer writer remained open");
            }
            worker.join().expect("reader worker");
            blocked.expect("kernel observed the idle read");
            assert_eq!(
                stopped_with_writer_held.expect("worker result"),
                Err(io::ErrorKind::ConnectionAborted)
            );
            // A child that another test thread forked while the reader was
            // open keeps a copy of it until its exec closes the descriptor.
            // Only a reader that outlives that window was leaked.
            let started = std::time::Instant::now();
            while !writer
                .write(b"x")
                .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
            {
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "the retired reader's fd stayed open"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn saturated_native_writer_stops_with_reader_retained() {
        let (reader, writer) = io::pipe().expect("native pipe");
        let alive = live();
        let (blocked_tx, blocked_rx) = sync_channel(1);
        let (done_tx, done_rx) = sync_channel(1);
        let observed = ObserveBlocked {
            inner: writer,
            blocked: Some(blocked_tx),
        };
        let mut writer =
            CancellablePipe::new(observed, Arc::clone(&alive)).expect("prepare writer");
        let worker = std::thread::spawn(move || {
            // Do not assume a pipe capacity. Reuse one small buffer until the
            // actual pipe fills; cancellation stops even a partially sent frame.
            let error = loop {
                if let Err(error) = writer.write_all(&[b'x'; 8192]) {
                    break error.kind();
                }
            };
            drop(writer);
            let _ = done_tx.send(error);
        });
        let blocked = blocked_rx.recv_timeout(Duration::from_secs(2));
        alive.store(false, Ordering::SeqCst);
        let stopped_with_reader_held = done_rx.recv_timeout(Duration::from_secs(2));
        drop(reader);
        if stopped_with_reader_held.is_err() {
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("writer retires after releasing its peer");
        }
        worker.join().expect("writer worker");
        blocked.expect("kernel observed a full pipe");
        assert_eq!(
            stopped_with_reader_held.expect("stopped with reader retained"),
            io::ErrorKind::ConnectionAborted
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_idle_reader_stops_with_duplicated_writers_retained() {
        for _ in 0..8 {
            let (reader, writer) = crate::jobs::windows_job_pipe::pair().expect("Windows output");
            let retained = writer.try_clone().expect("duplicated peer writer");
            let alive = live();
            let (blocked_tx, blocked_rx) = sync_channel(1);
            let (done_tx, done_rx) = sync_channel(1);
            let mut reader = CancellablePipe {
                inner: ObserveBlocked {
                    inner: WindowsOutput(reader),
                    blocked: Some(blocked_tx),
                },
                alive: Arc::clone(&alive),
            };
            let worker = std::thread::spawn(move || {
                let result = reader.read(&mut [0]).map_err(|error| error.kind());
                drop(reader);
                let _ = done_tx.send(result);
            });
            let blocked = blocked_rx.recv_timeout(Duration::from_secs(2));
            alive.store(false, Ordering::SeqCst);
            let stopped = done_rx.recv_timeout(Duration::from_secs(2));
            // Release both peers before joining, even if cancellation regresses.
            // A passing observation must precede that release, not depend on it.
            drop(writer);
            drop(retained);
            worker.join().expect("Windows output worker");
            blocked.expect("idle overlapped read observed");
            assert_eq!(
                stopped.expect("reader retires while both peers remain open"),
                Err(io::ErrorKind::ConnectionAborted)
            );
        }
    }

    #[test]
    #[cfg(windows)]
    fn windows_backpressured_writer_stops_with_duplicated_readers_retained() {
        let (mut writer, reader) =
            crate::jobs::windows_job_pipe::stdin_pair().expect("Windows input");
        let retained = reader.try_clone().expect("duplicated peer reader");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut accepted = 0;
        loop {
            // A single asynchronous write can briefly report WouldBlock even
            // when the pipe has room. Fill beyond Mio's 64 KiB pipe buffer so
            // the cancellation probe observes an actual unread full pipe.
            match writer.try_write(&[b'x'; 8192]) {
                Ok(count) => {
                    assert!(count > 0 && count <= 8192);
                    accepted += count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock && accepted > 64 * 1024 => {
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => panic!("prepare saturated MCP stdin: {error}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "stdin never saturated"
            );
        }
        let alive = live();
        let (blocked_tx, blocked_rx) = sync_channel(1);
        let (done_tx, done_rx) = sync_channel(1);
        let mut writer = CancellablePipe {
            inner: ObserveBlocked {
                inner: WindowsInput(writer),
                blocked: Some(blocked_tx),
            },
            alive: Arc::clone(&alive),
        };
        let worker = std::thread::spawn(move || {
            // More than the pipe's capacity, with no reader consuming bytes.
            // write_all must not mistake WouldBlock for delivery or retry an
            // already accepted prefix when cancellation interrupts the frame.
            let result = writer
                .write_all(&vec![b'x'; 256 * 1024])
                .map_err(|error| error.kind());
            drop(writer);
            let _ = done_tx.send(result);
        });
        let blocked = blocked_rx.recv_timeout(Duration::from_secs(2));
        alive.store(false, Ordering::SeqCst);
        let stopped = done_rx.recv_timeout(Duration::from_secs(2));
        drop(reader);
        drop(retained);
        worker.join().expect("Windows input worker");
        blocked.expect("overlapped input backpressure observed");
        assert_eq!(
            stopped.expect("writer retires while both peers remain open"),
            Err(io::ErrorKind::ConnectionAborted)
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_command_copies_are_the_only_client_handles_before_spawn() {
        let mut command = Command::new("pi-mcp-never-spawned-fixture");
        let alive = live();
        let mut pipes = prepare_command(&mut command, &alive).expect("prepare native stdio");
        assert!(matches!(
            pipes.stdout.inner.read(&mut [0]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        ));
        assert!(matches!(
            pipes.stderr.inner.read(&mut [0]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock
        ));
        drop(command);
        let (done_tx, done_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            let stdout = pipes.stdout.read(&mut [0]).map_err(|error| error.kind());
            let stderr = pipes.stderr.read(&mut [0]).map_err(|error| error.kind());
            drop(pipes);
            let _ = done_tx.send((stdout, stderr));
        });
        let eof = done_rx.recv_timeout(Duration::from_secs(2));
        alive.store(false, Ordering::SeqCst);
        worker.join().expect("unspawned pipe cleanup worker");
        assert_eq!(
            eof.expect("dropping an unspawned command closes both output peers"),
            (Ok(0), Ok(0))
        );
    }
}
