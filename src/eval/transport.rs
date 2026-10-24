//! Bounded Python protocol transport. The child is not a trusted parser peer:
//! malformed, unterminated, oversized, and misaddressed frames retire its kernel.
//!
//! Backpressure comes from the caller's one-slot synchronous channel. Never
//! drain an over-limit frame: the host will close/kill the checked-out kernel.

use crate::error::{Error, Result};
use serde_json::Value;
use std::io::{self, BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// Includes JSON escaping and rich displays, not an allowance for stdout RAM.
/// At most one complete frame is queued in addition to the reader's frame.
pub(super) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

const PIPE_RETRY_INTERVAL: Duration = Duration::from_millis(25);
type ReaderFrame = std::result::Result<String, String>;
type ReaderFrames = mpsc::Receiver<ReaderFrame>;

#[derive(Default)]
struct ReaderState {
    stopped: AtomicBool,
    #[cfg(test)]
    read_waits: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    send_waits: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    finished: AtomicBool,
}

/// Owned by the retained kernel, not by an individual cell. Kernel teardown
/// calls stop_receiving before process cleanup to release both idle reads and
/// backpressured sends. Dropping this guard also cancels idle Unix reads.
pub(super) struct ReaderStop {
    state: Arc<ReaderState>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ReaderStop {
    fn stop(&self) {
        self.state.stopped.store(true, Ordering::SeqCst);
    }

    pub(super) fn stop_receiving(&self, receiver: &mut ReaderFrames) {
        self.stop();
        // Disconnect the original receiver before a potentially slow process
        // reap. This wakes an existing blocking send immediately and preserves
        // normal queue throughput: full queues need no polling timer.
        let (sender, closed) = mpsc::channel();
        drop(sender);
        *receiver = closed;
    }
}

impl Drop for ReaderStop {
    fn drop(&mut self) {
        self.stop();
        // Never put a blocking thread join on async kernel teardown. On Unix
        // the reader observes stop between bounded nonblocking retries. A
        // completed worker may be joined without waiting for its peer's EOF.
        if self
            .worker
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

/// Change only a parent endpoint. The child's opposite pipe end is a distinct
/// open file description and keeps its normal blocking behavior.
#[cfg(unix)]
pub(super) fn prepare_pipe(pipe: &impl std::os::fd::AsFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(pipe)?;
    rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

pub(super) fn spawn_reader(
    stdout: std::process::ChildStdout,
) -> io::Result<(ReaderFrames, ReaderStop)> {
    #[cfg(unix)]
    prepare_pipe(&stdout)?;
    spawn_prepared_reader(stdout)
}

fn spawn_prepared_reader<R: Read + Send + 'static>(
    reader: R,
) -> io::Result<(ReaderFrames, ReaderStop)> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let state = Arc::new(ReaderState::default());
    let worker_state = Arc::clone(&state);
    let worker = std::thread::Builder::new()
        .name("eval-py-read".into())
        .spawn(move || {
            #[cfg(test)]
            let _finished = ReaderFinished(Arc::clone(&worker_state));
            // run_reader owns and closes its endpoint before the test-only
            // completion receipt is published, including on unwind.
            run_reader(reader, &sender, &worker_state);
        })?;
    Ok((
        receiver,
        ReaderStop {
            state,
            worker: Some(worker),
        },
    ))
}

#[cfg(test)]
struct ReaderFinished(Arc<ReaderState>);

#[cfg(test)]
impl Drop for ReaderFinished {
    fn drop(&mut self) {
        self.0.finished.store(true, Ordering::Release);
    }
}

struct CancellableReader<'a, R> {
    reader: R,
    state: &'a ReaderState,
}

impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            if self.state.stopped.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "Python protocol reader stopped",
                ));
            }
            match self.reader.read(bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    #[cfg(test)]
                    self.state.read_waits.fetch_add(1, Ordering::Release);
                    // This is a dedicated OS worker, not an executor task.
                    // Keep retrying below the frame decoder so it retains its
                    // partial UTF-8, JSON bytes, and accumulated frame limit.
                    std::thread::sleep(PIPE_RETRY_INTERVAL);
                }
                result => return result,
            }
        }
    }
}

fn publish_reader_frame(
    sender: &mpsc::SyncSender<ReaderFrame>,
    frame: ReaderFrame,
    state: &ReaderState,
) -> bool {
    if state.stopped.load(Ordering::SeqCst) {
        return false;
    }
    match sender.try_send(frame) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Disconnected(_)) => false,
        Err(mpsc::TrySendError::Full(pending)) => {
            #[cfg(test)]
            state.send_waits.fetch_add(1, Ordering::Release);
            // Move, do not clone, the pending bounded frame. Kernel teardown
            // disconnects this receiver before process cleanup, waking a
            // blocked send without imposing a polling delay on normal output.
            sender.send(pending).is_ok()
        }
    }
}

fn run_reader<R: Read>(reader: R, sender: &mpsc::SyncSender<ReaderFrame>, state: &ReaderState) {
    let mut reader = BufReader::new(CancellableReader { reader, state });
    while !state.stopped.load(Ordering::SeqCst) {
        let frame = match read_frame(&mut reader) {
            Ok(None) => return,
            Ok(Some(line)) => Ok(line),
            Err(error) => Err(format!("EVAL_PROTOCOL_READ: {error}")),
        };
        let failed = frame.is_err();
        // Also check after reading: BufReader may hold complete later frames
        // without calling its underlying cancellation-aware reader again.
        if !publish_reader_frame(sender, frame, state) || failed {
            return;
        }
    }
}

pub(super) fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<String>> {
    read_frame_with_limit(reader, MAX_FRAME_BYTES)
}

fn read_frame_with_limit(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            outcome => outcome?,
        };
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EVAL_PROTOCOL: incomplete frame",
                ))
            };
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let payload = end.unwrap_or(available.len());
        // Check before extending; even a newline-free writer cannot make this
        // reader allocate proportionally to the child process's output.
        if payload > limit.saturating_sub(bytes.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "EVAL_PROTOCOL_LIMIT: frame exceeds 16 MiB",
            ));
        }
        bytes.extend_from_slice(&available[..payload]);
        reader.consume(payload + usize::from(end.is_some()));
        if end.is_some() {
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "EVAL_PROTOCOL: frame is not UTF-8",
                )
            });
        }
    }
}

fn invalid(message: &str) -> Error {
    Error::tool(
        "eval",
        format!("EVAL_PROTOCOL: {message}; EVAL_STATE_RESET: Python kernel discarded"),
    )
}

pub(super) fn validate_bridge(frame: &Value) -> Result<()> {
    // A peer may not make a successful final result double as a tool request.
    if frame.get("ok").is_some() || !frame.is_object() {
        return Err(invalid("ambiguous bridge response"));
    }
    let bridge = &frame["bridge"];
    if bridge
        .get("call")
        .and_then(Value::as_u64)
        .is_none_or(|id| id == 0)
        || bridge
            .get("tool")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || !bridge.get("input").is_some_and(Value::is_object)
    {
        return Err(invalid("malformed bridge request"));
    }
    Ok(())
}

/// Admit a native effect only for this cell and an unambiguous bounded frame.
/// IDs are assigned before async queueing, not in dispatch order; synchronous
/// helpers may interleave, so monotonically ordered delivery is not required.
pub(super) fn validate_bridge_owner(
    frame: &Value,
    expected_id: u64,
    frame_bytes: usize,
) -> Result<()> {
    validate_bridge(frame)?;
    if frame_bytes >= 1024 * 1024 {
        return Err(invalid("EVAL_BRIDGE_LIMIT: tool request exceeds 1 MiB"));
    }
    if frame.get("id").and_then(Value::as_u64) != Some(expected_id) {
        return Err(invalid("bridge belongs to a different cell"));
    }
    if frame.as_object().is_none_or(|object| object.len() != 2)
        || frame["bridge"]
            .as_object()
            .is_none_or(|object| object.len() != 3)
    {
        return Err(invalid("ambiguous bridge envelope"));
    }
    let bridge = &frame["bridge"];
    if bridge["tool"]
        .as_str()
        .is_none_or(|name| name.chars().count() > 128)
    {
        return Err(invalid(
            "EVAL_BRIDGE_LIMIT: tool name exceeds 128 characters",
        ));
    }
    Ok(())
}

pub(super) fn validate_response(frame: &Value, expected_id: u64) -> Result<()> {
    if frame.get("id").and_then(Value::as_u64) != Some(expected_id) {
        return Err(invalid("response belongs to a different cell"));
    }
    // The Python receiver cannot safely resume after an interrupted or
    // malformed bridge exchange: unread reply bytes may remain on its stdin.
    // Guest code catching that exception must not make the channel reusable.
    if frame.get("bridgeError").is_some() {
        return Err(invalid("EVAL_BRIDGE_PROTOCOL: incomplete tool exchange"));
    }
    if frame.get("kernelError").is_some() {
        return Err(invalid(
            "EVAL_KERNEL_INCOMPLETE: cell did not settle safely",
        ));
    }
    let ok = frame
        .get("ok")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid("response status must be boolean"))?;
    if let Some(value) = frame.get("softTimeout") {
        let expired = value
            .as_bool()
            .ok_or_else(|| invalid("soft timeout status must be boolean"))?;
        if expired && ok {
            return Err(invalid("soft timeout cannot report success"));
        }
    }
    if !frame.get("stdout").is_some_and(Value::is_string)
        || !frame.get("stderr").is_some_and(Value::is_string)
        || frame.get("bridge").is_some()
    {
        return Err(invalid("malformed final response"));
    }
    if ok {
        if !frame
            .get("result")
            .is_some_and(|value| value.is_null() || value.is_string())
        {
            return Err(invalid("result must be text or null"));
        }
    } else if !frame.get("error").is_some_and(Value::is_string) {
        return Err(invalid("failed response must include error text"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufReader, Cursor};

    #[test]
    fn exact_limit_newline_and_following_frame_are_preserved() {
        for capacity in [1, 2, 7, 128] {
            let mut input = BufReader::with_capacity(capacity, Cursor::new(b"1234567\nnext\n"));
            assert_eq!(
                read_frame_with_limit(&mut input, 7).unwrap(),
                Some("1234567".into())
            );
            assert_eq!(
                read_frame_with_limit(&mut input, 7).unwrap(),
                Some("next".into())
            );
            assert_eq!(read_frame_with_limit(&mut input, 7).unwrap(), None);
        }
    }

    #[test]
    fn over_limit_input_fails_before_consuming_an_unbounded_frame() {
        let mut input = Cursor::new(vec![b'x'; 4096]);
        let error = read_frame_with_limit(&mut input, 16).unwrap_err();
        assert!(error.to_string().contains("EVAL_PROTOCOL_LIMIT"));
        assert_eq!(input.position(), 0);
        let mut input = BufReader::with_capacity(8, Cursor::new(b"12345678901234567\n"));
        assert!(
            read_frame_with_limit(&mut input, 16)
                .unwrap_err()
                .to_string()
                .contains("EVAL_PROTOCOL_LIMIT")
        );
    }

    #[test]
    fn split_unicode_and_crlf_decode_without_losing_bytes() {
        let mut input = BufReader::with_capacity(1, Cursor::new("界😀\r\n".as_bytes()));
        assert_eq!(
            read_frame_with_limit(&mut input, 8).unwrap(),
            Some("界😀\r".into())
        );
    }

    #[test]
    fn truncated_or_invalid_utf8_frames_are_not_valid_eof() {
        assert_eq!(
            read_frame_with_limit(&mut Cursor::new(b"unterminated"), 64)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            read_frame_with_limit(&mut Cursor::new(b"\xff\n"), 64)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            read_frame_with_limit(&mut Cursor::new(b"\n"), 64).unwrap(),
            Some(String::new())
        );
    }

    #[test]
    fn response_identity_and_status_fail_closed() {
        let valid = json!({"id":7,"ok":true,"stdout":"","stderr":"","result":null});
        assert!(validate_response(&valid, 7).is_ok());
        assert!(validate_response(&valid, 8).is_err());
        for (key, value) in [
            ("id", json!("7")),
            ("ok", json!("true")),
            ("stdout", Value::Null),
            ("result", json!(42)),
        ] {
            let mut bad = valid.clone();
            bad[key] = value;
            assert!(validate_response(&bad, 7).is_err());
        }
        assert!(
            validate_response(
                &json!({"id":7,"ok":false,"stdout":"","stderr":"","error":"failure"}),
                7
            )
            .is_ok()
        );
        assert!(validate_response(&json!({"id":7,"ok":false,"stdout":"","stderr":""}), 7).is_err());
    }

    #[test]
    fn bridge_arguments_and_final_response_cannot_be_confused() {
        let valid = json!({"bridge":{"call":1,"tool":"read","input":{"path":"README.md"}}});
        assert!(validate_bridge(&valid).is_ok());
        for bad in [
            json!({"bridge":{"call":0,"tool":"read","input":{}}}),
            json!({"bridge":{"call":1,"tool":"read","input":[]}}),
            json!({"bridge":{"call":1,"tool":"","input":{}}}),
            json!({"bridge":{"call":1,"tool":"read","input":{}},"ok":true}),
        ] {
            assert!(validate_bridge(&bad).is_err());
        }
    }

    #[test]
    fn poisoned_bridge_retires_even_an_apparently_successful_cell() {
        for marker in [json!("incomplete"), Value::Null, json!(false)] {
            for ok in [true, false] {
                let response = json!({
                    "id":7, "ok":ok, "stdout":"", "stderr":"",
                    "result":"42", "error":"caught", "bridgeError":marker,
                });
                let error = validate_response(&response, 7).unwrap_err().to_string();
                assert!(error.contains("EVAL_BRIDGE_PROTOCOL"));
                assert!(error.contains("EVAL_STATE_RESET"));
            }
        }
    }

    #[test]
    fn bridge_effect_admission_requires_current_cell_and_exact_envelope() {
        let frame = json!({"id":7,"bridge":{"call":9,"tool":"read","input":{"path":"x"}}});
        assert!(validate_bridge_owner(&frame, 7, 90).is_ok());
        for (id, size) in [(8, 90), (7, 1024 * 1024)] {
            assert!(validate_bridge_owner(&frame, id, size).is_err());
        }
        for (key, value) in [
            ("id", json!(true)),
            ("stream", json!({})),
            ("softTimeout", json!(true)),
        ] {
            let mut bad = frame.clone();
            bad[key] = value;
            assert!(validate_bridge_owner(&bad, 7, 90).is_err());
        }
        let mut bad = frame;
        bad["bridge"]["extra"] = Value::Null;
        assert!(validate_bridge_owner(&bad, 7, 90).is_err());
    }

    #[test]
    fn soft_timeout_and_unsettled_root_metadata_fail_closed() {
        let response = json!({"id":1,"ok":false,"stdout":"","stderr":"","error":"soft timeout","softTimeout":true});
        assert!(validate_response(&response, 1).is_ok());
        for marker in [Value::Null, json!(false), json!("pending root")] {
            let mut bad = response.clone();
            bad["kernelError"] = marker;
            assert!(
                validate_response(&bad, 1)
                    .unwrap_err()
                    .to_string()
                    .contains("EVAL_KERNEL_INCOMPLETE")
            );
        }
        let mut success = response.clone();
        success["ok"] = json!(true);
        success["result"] = Value::Null;
        assert!(validate_response(&success, 1).is_err());
        for marker in [Value::Null, json!(1), json!("true")] {
            let mut bad = response.clone();
            bad["softTimeout"] = marker;
            assert!(validate_response(&bad, 1).is_err());
        }
    }

    #[test]
    fn actual_kernel_wrong_cell_reply_is_rejected_and_state_is_retired() {
        use crate::tools::Tool;
        if !std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = crate::eval::EvalTool::new(dir.path());
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        runtime.block_on(async {
            let code = concat!(
                "keep = 42\n",
                "wire = tool.read.__globals__['_REAL_STDOUT']\n",
                "wire.write('{\"id\":999,\"ok\":true,\"stdout\":\"\",\"stderr\":\"\",\"result\":null}\\n')\n",
                "wire.flush()\n",
            );
            let error = tool.execute("bad", json!({"code":code}), None).await.unwrap_err();
            assert!(error.to_string().contains("different cell"), "{error}");
            assert!(error.to_string().contains("EVAL_STATE_RESET"));
            let next = tool.execute("next", json!({"code":"'keep' in globals()"}), None).await.unwrap();
            assert!(!next.is_error);
            assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
            assert!(matches!(&next.content[0], crate::model::ContentBlock::Text(text) if text.text.contains("False")));
        });
    }

    #[test]
    fn actual_kernel_newline_free_oversize_reply_is_not_misreported_as_timeout() {
        use crate::tools::Tool;
        if !std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = crate::eval::EvalTool::new(dir.path());
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let code = format!(
            "wire = tool.read.__globals__['_REAL_STDOUT']\nwire.write('x' * {})\nwire.flush()",
            MAX_FRAME_BYTES + 1
        );
        let error = runtime
            .block_on(tool.execute("oversize", json!({"code":code}), None))
            .unwrap_err();
        assert!(error.to_string().contains("EVAL_PROTOCOL_LIMIT"), "{error}");
        assert!(!error.to_string().contains("EVAL_TIMEOUT"), "{error}");
        let next = runtime
            .block_on(tool.execute("next", json!({"code":"42"}), None))
            .unwrap();
        assert!(!next.is_error);
        assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    }

    fn wait_until(mut ready: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !ready() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    #[cfg(unix)]
    #[test]
    fn stopped_native_reader_retires_its_fd_with_writer_retained() {
        use std::io::Write;
        for _ in 0..8 {
            let (reader, mut writer) = io::pipe().expect("native pipe");
            prepare_pipe(&reader).expect("nonblocking reader");
            let (frames, mut stop) = spawn_prepared_reader(reader).expect("reader worker");
            let blocked = wait_until(|| stop.state.read_waits.load(Ordering::Acquire) > 0);
            stop.stop();
            let retired = wait_until(|| stop.worker.as_ref().unwrap().is_finished());
            if !retired {
                drop(writer);
                assert!(
                    wait_until(|| stop.worker.as_ref().unwrap().is_finished()),
                    "reader failed to retire even after releasing its peer"
                );
                stop.worker.take().unwrap().join().expect("reader worker");
                panic!("reader required peer EOF instead of observing cancellation");
            }
            stop.worker.take().unwrap().join().expect("reader worker");
            assert!(
                blocked,
                "the kernel must report an idle read before cancellation"
            );
            assert!(stop.state.finished.load(Ordering::Acquire));
            assert!(matches!(
                frames.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ));
            // See the full-queue test below: a concurrent fork can hold a
            // copy of the reader until its exec closes it.
            assert!(
                wait_until(|| writer
                    .write(b"x")
                    .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe)),
                "the retired reader's fd stayed open"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn kernel_receiver_retirement_wakes_a_full_queue_with_peer_retained() {
        use std::io::Write;
        let (reader, mut writer) = io::pipe().expect("native pipe");
        prepare_pipe(&reader).expect("nonblocking reader");
        let (mut frames, mut stop) = spawn_prepared_reader(reader).expect("reader worker");
        writer.write_all(b"first\nsecond\n").expect("two frames");
        let blocked = wait_until(|| stop.state.send_waits.load(Ordering::Acquire) > 0);
        stop.stop_receiving(&mut frames);
        let retired = wait_until(|| stop.worker.as_ref().unwrap().is_finished());
        if !retired {
            drop(frames);
            drop(writer);
            assert!(
                wait_until(|| stop.worker.as_ref().unwrap().is_finished()),
                "reader did not retire after releasing its queue and pipe"
            );
            stop.worker.take().unwrap().join().expect("reader worker");
            panic!("full queue ignored kernel receiver retirement");
        }
        stop.worker.take().unwrap().join().expect("reader worker");
        assert!(
            blocked,
            "establish actual channel backpressure before cancellation"
        );
        assert!(matches!(
            frames.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        // A child that another test thread forked while the reader was open
        // keeps a copy of it until its exec closes the descriptor. Only a
        // reader that outlives that window was leaked.
        assert!(
            wait_until(|| writer
                .write(b"x")
                .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe)),
            "the retired reader's fd stayed open"
        );
    }

    struct GappedReader {
        first: Cursor<Vec<u8>>,
        second: Cursor<Vec<u8>>,
        gap: bool,
    }

    impl Read for GappedReader {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.first.position() < self.first.get_ref().len() as u64 {
                return self.first.read(bytes);
            }
            if self.gap {
                self.gap = false;
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.second.read(bytes)
        }
    }

    fn gapped_reader() -> GappedReader {
        GappedReader {
            first: Cursor::new(vec![0xe7]),
            second: Cursor::new(vec![0x95, 0x8c, b'\n', b'4', b'2', b'\n']),
            gap: true,
        }
    }

    #[test]
    fn nonblocking_gaps_preserve_utf8_and_following_frames() {
        let state = ReaderState::default();
        let mut reader = BufReader::new(CancellableReader {
            reader: gapped_reader(),
            state: &state,
        });
        assert_eq!(
            read_frame(&mut reader).expect("split scalar"),
            Some("界".into())
        );
        assert_eq!(
            read_frame(&mut reader).expect("following frame"),
            Some("42".into())
        );
        assert_eq!(read_frame(&mut reader).expect("EOF"), None);
        assert_eq!(state.read_waits.load(Ordering::Acquire), 1);
    }

    #[test]
    fn nonblocking_gaps_do_not_reset_the_frame_byte_limit() {
        let state = ReaderState::default();
        let mut reader = BufReader::new(CancellableReader {
            reader: gapped_reader(),
            state: &state,
        });
        let error =
            read_frame_with_limit(&mut reader, 2).expect_err("three-byte scalar exceeds limit");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(state.read_waits.load(Ordering::Acquire), 1);
    }

    #[test]
    fn reader_worker_distinguishes_real_eof_from_partial_frame_failure() {
        for (input, failed) in [(b"".as_slice(), false), (b"partial".as_slice(), true)] {
            let (frames, mut stop) =
                spawn_prepared_reader(Cursor::new(input)).expect("reader worker");
            if failed {
                let error = frames
                    .recv_timeout(Duration::from_secs(2))
                    .expect("error frame")
                    .expect_err("partial frame failure");
                assert!(error.contains("EVAL_PROTOCOL_READ"));
                assert!(error.contains("incomplete frame"));
            }
            assert!(matches!(
                frames.recv_timeout(Duration::from_secs(2)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ));
            stop.worker.take().unwrap().join().expect("reader worker");
            assert!(stop.state.finished.load(Ordering::Acquire));
        }
    }

    #[cfg(unix)]
    #[test]
    fn public_eval_reset_and_drop_stop_the_owned_protocol_readers() {
        use crate::tools::Tool;
        if !std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().expect("workspace");
        let tool = crate::eval::EvalTool::new(dir.path());
        let runtime = asupersync::runtime::RuntimeBuilder::new()
            .build()
            .expect("runtime");
        let first = runtime
            .block_on(tool.execute("seed", json!({"code":"retained = 41"}), None))
            .expect("seed cell");
        assert!(!first.is_error);
        let old = Arc::clone(
            &tool
                .kernel
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .reader_stop
                .state,
        );
        assert!(!old.stopped.load(Ordering::SeqCst));
        let reset = runtime
            .block_on(tool.execute("reset", json!({"code":"42", "reset":true}), None))
            .expect("reset cell");
        assert!(!reset.is_error);
        assert!(
            old.stopped.load(Ordering::SeqCst),
            "reset must revoke the old reader"
        );
        assert!(
            wait_until(|| old.finished.load(Ordering::Acquire)),
            "old reader did not retire"
        );
        let new = Arc::clone(
            &tool
                .kernel
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .reader_stop
                .state,
        );
        assert!(!Arc::ptr_eq(&old, &new));
        assert!(!new.stopped.load(Ordering::SeqCst));
        drop(tool);
        assert!(
            new.stopped.load(Ordering::SeqCst),
            "session drop must revoke its reader"
        );
        assert!(
            wait_until(|| new.finished.load(Ordering::Acquire)),
            "session reader did not retire"
        );
    }
}
