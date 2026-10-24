use super::*;
use serde_json::json;
use std::sync::Mutex;

#[derive(Default)]
struct Recorded {
    bytes: Vec<u8>,
    writes: usize,
    flushes: usize,
}

struct Writer {
    record: Arc<Mutex<Recorded>>,
    short: usize,
    interrupts: usize,
    backpressure_after: Option<usize>,
    fail_after: Option<usize>,
    fail_flush: bool,
    revoke_after: Option<Arc<AtomicBool>>,
}

impl Writer {
    fn new(record: &Arc<Mutex<Recorded>>) -> Self {
        Self {
            record: Arc::clone(record),
            short: 3,
            interrupts: 0,
            backpressure_after: None,
            fail_after: None,
            fail_flush: false,
            revoke_after: None,
        }
    }
}

impl Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut record = self.record.lock().unwrap();
        record.writes += 1;
        if self.interrupts > 0 {
            self.interrupts -= 1;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self
            .backpressure_after
            .is_some_and(|end| record.bytes.len() >= end)
        {
            self.backpressure_after = None;
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if self.fail_after.is_some_and(|end| record.bytes.len() >= end) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let size = bytes.len().min(self.short);
        record.bytes.extend_from_slice(&bytes[..size]);
        if let Some(revoked) = &self.revoke_after {
            revoked.store(true, Ordering::SeqCst);
        }
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.record.lock().unwrap().flushes += 1;
        if self.fail_flush {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
}

fn send(writer: Writer, bytes: &[u8], revoked: &AtomicBool) -> Result<Writer> {
    write_once(
        writer,
        bytes,
        revoked,
        &AgentCx::for_testing_with_io(),
        Instant::now() + std::time::Duration::from_secs(5),
    )
}

#[test]
fn partial_writes_and_interrupted_syscalls_preserve_exactly_one_frame() {
    let record = Arc::new(Mutex::new(Recorded::default()));
    let mut writer = Writer::new(&record);
    writer.interrupts = 2;
    let frame = encode(&json!({"id":7,"code":"'界'\n42"})).unwrap();
    assert!(send(writer, &frame, &AtomicBool::new(false)).is_ok());
    let record = record.lock().unwrap();
    assert_eq!(record.bytes, frame);
    assert_eq!(record.flushes, 1);
    assert_eq!(
        record.bytes.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
}

#[test]
fn partial_failure_never_restarts_from_the_beginning() {
    let record = Arc::new(Mutex::new(Recorded::default()));
    let mut writer = Writer::new(&record);
    writer.fail_after = Some(6);
    let error = send(writer, b"first-second-third\n", &AtomicBool::new(false))
        .err()
        .unwrap();
    assert!(error.to_string().contains("delivery uncertain, no replay"));
    let record = record.lock().unwrap();
    assert_eq!(record.bytes, b"first-");
    assert_eq!(record.flushes, 0);
}

#[test]
fn flush_failure_is_not_a_success_receipt_or_permission_to_replay() {
    let record = Arc::new(Mutex::new(Recorded::default()));
    let mut writer = Writer::new(&record);
    writer.fail_flush = true;
    let error = send(writer, b"complete\n", &AtomicBool::new(false))
        .err()
        .unwrap();
    assert!(error.to_string().contains("flush failed"));
    let record = record.lock().unwrap();
    assert_eq!(record.bytes, b"complete\n");
    assert_eq!(record.flushes, 1);
}

#[test]
fn cancellation_and_dropped_leases_prevent_further_write_chunks() {
    let record = Arc::new(Mutex::new(Recorded::default()));
    let revoked = Arc::new(AtomicBool::new(false));
    let mut writer = Writer::new(&record);
    writer.revoke_after = Some(Arc::clone(&revoked));
    assert!(send(writer, b"remaining\n", &revoked).is_err());
    assert_eq!(record.lock().unwrap().bytes, b"rem");
    let stopped = Arc::new(AtomicBool::new(false));
    {
        let _lease = WriteLease(Arc::clone(&stopped));
    }
    let record = Arc::new(Mutex::new(Recorded::default()));
    assert!(send(Writer::new(&record), b"must not start\n", &stopped).is_err());
    assert_eq!(record.lock().unwrap().writes, 0);
}

#[test]
fn expired_writer_never_dispatches_the_first_byte() {
    let record = Arc::new(Mutex::new(Recorded::default()));
    let result = write_once(
        Writer::new(&record),
        b"no\n",
        &AtomicBool::new(false),
        &AgentCx::for_testing_with_io(),
        Instant::now(),
    );
    assert!(result.err().unwrap().to_string().contains("EVAL_TIMEOUT"));
    assert_eq!(record.lock().unwrap().writes, 0);
}

#[test]
fn frame_budget_counts_json_escaping_and_the_newline() {
    // A JSON string contributes two quotes and one terminating newline.
    let exact = json!("x".repeat(MAX_FRAME_BYTES - 3));
    assert_eq!(encode(&exact).unwrap().len(), MAX_FRAME_BYTES);
    assert!(encode(&json!("x".repeat(MAX_FRAME_BYTES - 2))).is_err());
    assert!(encode(&json!("\0".repeat(MAX_FRAME_BYTES / 6 + 1))).is_err());
}

#[test]
fn early_next_request_is_retained_before_following_output() {
    let (tx, rx) = mpsc::channel();
    let console = |sequence| {
        json!({"id":7,"stream":{"name":"stdout","sequence":sequence,"text":"x"}}).to_string()
    };
    let bridge = json!({"id":7,"bridge":{"call":2,"tool":"read","input":{}}}).to_string();
    tx.send(Ok(console(1))).unwrap();
    tx.send(Ok(bridge.clone())).unwrap();
    tx.send(Ok(console(2))).unwrap();
    let lines = Mutex::new(rx);
    let mut pending = None;
    let mut capture = Capture::new("python", None);
    drain_during_delivery(&lines, &mut pending, 7, &mut capture).unwrap();
    assert_eq!(pending.as_deref(), Some(bridge.as_str()));
    // Re-polling before the writer receipt cannot consume a frame past it.
    drain_during_delivery(&lines, &mut pending, 7, &mut capture).unwrap();
    assert_eq!(pending.take().unwrap(), bridge);
    drain_during_delivery(&lines, &mut pending, 7, &mut capture).unwrap();
    let mut response = json!({"outputSequence":2,"stdout":"","stderr":"","displays":[]});
    capture.finish_response(&mut response).unwrap();
    assert_eq!(response["stdout"], "xx");
}

#[test]
fn early_final_response_is_deferred_not_mistaken_for_protocol_corruption() {
    let (tx, rx) = mpsc::channel();
    let complete = json!({"id":7,"ok":true,"result":"42","stdout":"","stderr":"","outputSequence":0,"displays":[]}).to_string();
    tx.send(Ok(complete.clone())).unwrap();
    drop(tx);
    let mut pending = None;
    drain_during_delivery(
        &Mutex::new(rx),
        &mut pending,
        7,
        &mut Capture::new("python", None),
    )
    .unwrap();
    assert_eq!(pending, Some(complete));
}

#[cfg(unix)]
#[test]
fn public_eval_hard_deadline_covers_a_kernel_that_never_reads_its_input() {
    use crate::tools::Tool;
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let shim = dir.path().join("nonreading-python");
    // Controlled interpreter peer: admit the fixed startup probe, but leave
    // the application's input pipe unread. The fixture self-terminates too.
    std::fs::write(
        &shim,
        r#"#!/bin/sh
case "$2" in
  *"with open(sys.argv"*) printf 'started\n' >> launches; exec sleep 8 ;;
  *) exit 0 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut tool = super::super::super::EvalTool::new(dir.path());
    tool.python_path = shim.into_os_string();
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap();
    let started = Instant::now();
    let error = runtime
        .block_on(tool.execute(
            "blocked-input",
            json!({
                "code":format!("#{}\n42", "x".repeat(4 * 1024 * 1024)), "timeout_secs":2
            }),
            None,
        ))
        .unwrap_err();
    assert!(error.to_string().contains("EVAL_TIMEOUT"), "{error}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "input write blocked the executor"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("launches")).unwrap(),
        "started\n"
    );
    assert!(tool.kernel.lock().unwrap().is_none());
}

#[test]
fn public_eval_accepts_large_requests_and_native_replies_in_the_same_retained_kernel() {
    use crate::model::ContentBlock;
    use crate::tools::Tool;
    if !std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data.txt"), "native result\n").unwrap();
    let tool = super::super::super::EvalTool::new(dir.path());
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .blocking_threads(1, 8)
        .build()
        .unwrap();
    runtime.block_on(async {
        let out = tool.execute("large", json!({"code":format!(
            "#{}\nanswer = await tool.call('read', {{'path':'data.txt'}})\nassert 'native result' in answer\nsaved = 42\nsaved", "x".repeat(256 * 1024)
        )}), None).await.unwrap();
        assert!(!out.is_error, "{out:?}");
        let restored = tool.execute("again", json!({"code":"saved"}), None).await.unwrap();
        assert!(!restored.is_error);
        assert!(restored.content.iter().any(|block| matches!(block, ContentBlock::Text(text) if text.text.trim() == "42")));
    });
}

#[test]
fn transient_backpressure_after_partial_delivery_preserves_the_remaining_frame() {
    let record = Arc::new(Mutex::new(Recorded::default()));
    let mut writer = Writer::new(&record);
    writer.interrupts = 1;
    writer.backpressure_after = Some(6);
    let frame = encode(&json!({"id":7,"code":"'界'\n42"})).expect("encoded frame");
    assert!(send(writer, &frame, &AtomicBool::new(false)).is_ok());
    let record = record.lock().unwrap();
    assert_eq!(record.bytes, frame);
    assert_eq!(record.flushes, 1);
}

#[cfg(unix)]
#[test]
fn saturated_native_input_observes_abandonment_and_deadline_with_reader_retained() {
    use std::time::Duration;

    struct ObservedWriter {
        pipe: io::PipeWriter,
        blocked: Option<mpsc::SyncSender<()>>,
    }

    impl Write for ObservedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let result = self.pipe.write(bytes);
            if result
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
                && let Some(blocked) = self.blocked.take()
            {
                let _ = blocked.try_send(());
            }
            result
        }

        fn flush(&mut self) -> io::Result<()> {
            self.pipe.flush()
        }
    }

    for abandon in [true, false] {
        let (reader, mut pipe) = io::pipe().expect("native input pipe");
        crate::eval::transport::prepare_pipe(&pipe).expect("nonblocking input");
        // Fill the actual kernel buffer, without assuming a platform capacity.
        loop {
            match pipe.write(&[b'x'; 8192]) {
                Ok(0) => panic!("pipe stopped before reaching backpressure"),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill native input pipe: {error}"),
            }
        }
        let (blocked_tx, blocked_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let revoked = Arc::new(AtomicBool::new(false));
        let lease = WriteLease(Arc::clone(&revoked));
        let owner = AgentCx::for_testing_with_io();
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(if abandon { 5 } else { 2 });
            let result = write_once(
                ObservedWriter {
                    pipe,
                    blocked: Some(blocked_tx),
                },
                b"unsent-cell\n",
                &revoked,
                &owner,
                deadline,
            )
            .map(|_| ())
            .map_err(|error| error.to_string());
            let _ = done_tx.send(result);
        });
        let blocked = blocked_rx.recv_timeout(Duration::from_secs(2));
        let mut lease = Some(lease);
        if abandon {
            drop(lease.take());
        }
        let stopped_with_reader_held = done_rx.recv_timeout(Duration::from_secs(3));
        // Release the peer and lease even on a failing implementation so its
        // worker can unwind instead of stranding the test's blocking thread.
        drop(reader);
        drop(lease);
        if stopped_with_reader_held.is_err() {
            let _ = done_rx
                .recv_timeout(Duration::from_secs(3))
                .expect("writer retires after releasing its peer");
        }
        worker.join().expect("input writer worker");
        blocked.expect("actual kernel backpressure before cancellation");
        let error = stopped_with_reader_held
            .expect("bounded input cancellation")
            .expect_err("a full pipe must not produce a success receipt");
        let expected = if abandon {
            "EVAL_CANCELLED"
        } else {
            "EVAL_TIMEOUT"
        };
        assert!(error.contains(expected), "{error}");
        assert!(error.contains("EVAL_STATE_RESET"), "{error}");
    }
}
