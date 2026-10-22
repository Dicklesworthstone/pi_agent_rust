//! Bounded, non-blocking admission to a single child-stdin pump.
//!
//! Runtime and reader threads enqueue complete frames; only this dedicated
//! pump touches the blocking pipe. Queue acceptance is not delivery. The
//! response wait owns the request deadline, and failures retire the transport
//! rather than retrying a frame whose delivery is uncertain.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use super::{MAX_FRAME_BYTES, PendingMap, TransportError, lock};

// A stalled pipe must still admit the traffic the client itself can generate:
// all pending requests, their cancellations, and a close/open cycle for the
// 128-document working set. The old 64-frame cap could retire a healthy server
// halfway through invalidate_all(), before the writer got another timeslice.
// This remains a hard count bound; the independent byte budget below is NOT
// enlarged, including for the frame currently blocked in the pipe.
const MAX_QUEUED_FRAMES: usize = 2 * super::MAX_PENDING_REQUESTS + 256;
// Includes the frame being written, not just frames still in the queue.
const MAX_OUTBOUND_BYTES: usize = MAX_FRAME_BYTES + 64 * 1024;

struct ByteBudget {
    used: AtomicUsize,
    limit: usize,
}

struct Reservation {
    budget: Arc<ByteBudget>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct Frame {
    bytes: Vec<u8>,
    request_id: Option<u64>,
    _reservation: Option<Reservation>,
}

struct QueueState {
    frames: VecDeque<Frame>,
    cancellations: VecDeque<u64>,
    capacity: usize,
    writer_closed: bool,
    receiver_closed: bool,
}

impl QueueState {
    fn take_next(&mut self) -> Option<Frame> {
        if let Some(id) = self.cancellations.pop_front() {
            // Cancellation controls contain only a u64 id. Their separate
            // MAX_PENDING_REQUESTS bound reserves less than 128 KiB even when
            // the ordinary byte budget is entirely held by a blocked write.
            return Some(Frame {
                bytes: super::encode_frame(&serde_json::json!({
                    "jsonrpc": "2.0", "method": "$/cancelRequest", "params": { "id": id }
                })),
                request_id: None,
                _reservation: None,
            });
        }
        self.frames.pop_front()
    }
}

struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

struct FrameReceiver {
    queue: Arc<Queue>,
}

impl FrameReceiver {
    fn recv(&self) -> io::Result<Frame> {
        let mut state = lock(&self.queue.state);
        loop {
            if let Some(frame) = state.take_next() {
                return Ok(frame);
            }
            if state.writer_closed {
                return Err(closed());
            }
            state = self
                .queue
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    #[cfg(test)]
    fn try_recv(&self) -> Result<Frame, std::sync::mpsc::TryRecvError> {
        let mut state = lock(&self.queue.state);
        state.take_next().ok_or(if state.writer_closed {
            std::sync::mpsc::TryRecvError::Disconnected
        } else {
            std::sync::mpsc::TryRecvError::Empty
        })
    }
}

impl Drop for FrameReceiver {
    fn drop(&mut self) {
        let mut state = lock(&self.queue.state);
        state.receiver_closed = true;
        state.frames.clear();
        state.cancellations.clear();
    }
}

/// A frame-admission writer. `flush` checks transport health; it does not wait
/// for pipe drainage. Waiting for the protocol response establishes delivery.
pub(super) struct QueuedWriter {
    queue: Arc<Queue>,
    budget: Arc<ByteBudget>,
    alive: Arc<AtomicBool>,
}

/// Wake an idle writer and release queued frame reservations on retirement.
/// This owns only the queue, never the writer or its thread handle.
#[derive(Clone)]
pub(super) struct ShutdownHandle {
    queue: Arc<Queue>,
}

impl ShutdownHandle {
    pub(super) fn close(&self) {
        let mut state = lock(&self.queue.state);
        state.writer_closed = true;
        state.frames.clear();
        state.cancellations.clear();
        drop(state);
        self.queue.ready.notify_one();
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "server writer is closed")
}

/// Publish terminal state before draining, sharing the pending-map lock with
/// request admission. Completions are sent outside the lock and never block.
pub(super) fn close_pending(pending: &PendingMap, alive: &AtomicBool, error: &TransportError) {
    alive.store(false, Ordering::Release);
    let abandoned = std::mem::take(&mut *lock(pending));
    for (_, sender) in abandoned {
        let _ = sender.try_send(Err(error.clone()));
    }
}

impl QueuedWriter {
    fn channel(
        alive: Arc<AtomicBool>,
        capacity: usize,
        byte_limit: usize,
    ) -> (Self, FrameReceiver) {
        let queue = Arc::new(Queue {
            state: Mutex::new(QueueState {
                frames: VecDeque::new(),
                cancellations: VecDeque::new(),
                capacity,
                writer_closed: false,
                receiver_closed: false,
            }),
            ready: Condvar::new(),
        });
        (
            Self {
                queue: Arc::clone(&queue),
                budget: Arc::new(ByteBudget {
                    used: AtomicUsize::new(0),
                    limit: byte_limit,
                }),
                alive,
            },
            FrameReceiver { queue },
        )
    }

    pub(super) fn start(
        mut pipe: impl Write + Send + 'static,
        pending: Arc<PendingMap>,
        alive: Arc<AtomicBool>,
        process: Option<Arc<super::ServerProcess>>,
    ) -> io::Result<(Self, std::thread::JoinHandle<()>)> {
        let (writer, receiver) =
            Self::channel(Arc::clone(&alive), MAX_QUEUED_FRAMES, MAX_OUTBOUND_BYTES);
        let cleanup = process.map(|process| super::ServerWorkerGuard {
            process,
            pending: Arc::clone(&pending),
            alive: Arc::clone(&alive),
            writer: writer.shutdown_handle(),
        });
        let worker = std::thread::Builder::new()
            .name("pi-lsp-stdin".to_string())
            .spawn(move || {
                let _cleanup = cleanup;
                let error = loop {
                    let Ok(frame) = receiver.recv() else {
                        break TransportError::Closed("server writer queue closed".to_string());
                    };
                    if !alive.load(Ordering::Acquire) {
                        break TransportError::Closed("server transport retired".to_string());
                    }
                    // Taking a frame and withdrawing a queued request share
                    // one lock. Once claimed, complete the frame before any
                    // cancellation control; never emit half a JSON-RPC frame.
                    if let Err(error) = pipe.write_all(&frame.bytes).and_then(|()| pipe.flush()) {
                        break TransportError::Io(format!("server pipe write failed: {error}"));
                    }
                    // Drop releases the byte reservation only after the entire
                    // frame was written. There is no replay on a partial write.
                };
                close_pending(&pending, &alive, &error);
                // Dropping the receiver also releases every queued reservation.
            })?;
        Ok((writer, worker))
    }

    pub(super) fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            queue: Arc::clone(&self.queue),
        }
    }

    #[cfg(test)]
    pub(super) fn has_in_flight_frame(&self) -> bool {
        lock(&self.queue.state).frames.is_empty() && self.budget.used.load(Ordering::Acquire) != 0
    }

    pub(super) fn write_request(&self, bytes: &[u8], id: u64) -> io::Result<()> {
        self.enqueue(bytes, Some(id)).map(|_| ())
    }

    /// Admit a document transaction in one queue operation. Refusal leaves
    /// every frame with its caller; acceptance transfers every frame in order.
    /// Each frame retains its own byte reservation until the pump writes it.
    pub(super) fn write_notification_batch(&self, frames: &mut Vec<Vec<u8>>) -> io::Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(closed());
        }
        let bytes = frames.iter().try_fold(0_usize, |total, frame| {
            total.checked_add(frame.len()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "notification batch size overflow",
                )
            })
        })?;
        self.validate_frame_size(bytes)?;
        let mut state = lock(&self.queue.state);
        if state.receiver_closed || state.writer_closed {
            return Err(closed());
        }
        // A batch larger than the entire queue can never become admissible.
        // Distinguish that from temporary pressure before checking occupancy.
        if frames.len() > state.capacity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "notification batch exceeds the writer frame capacity",
            ));
        }
        if state.frames.len() > state.capacity - frames.len() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "server writer frame queue full",
            ));
        }
        self.budget
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|total| *total <= self.budget.limit)
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "server writer byte budget exhausted",
                )
            })?;
        // No fallible admission remains. The pump cannot claim a prefix
        // until the complete transaction is present in the queue.
        for bytes in frames.drain(..) {
            let reservation = Reservation {
                budget: Arc::clone(&self.budget),
                bytes: bytes.len(),
            };
            state.frames.push_back(Frame {
                bytes,
                request_id: None,
                _reservation: Some(reservation),
            });
        }
        drop(state);
        self.queue.ready.notify_one();
        Ok(())
    }

    pub(super) fn validate_frame_size(&self, bytes: usize) -> io::Result<()> {
        if bytes > self.budget.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "server frame exceeds the writer byte budget",
            ));
        }
        Ok(())
    }

    /// Withdraw an unclaimed request, or cancel it immediately after the
    /// current write. This never touches the pipe or the ordinary admission
    /// budget. The caller holds request admission while retiring its pending
    /// slot, so at most MAX_PENDING_REQUESTS written requests can await cancel.
    pub(super) fn cancel_request(&self, id: u64) -> io::Result<()> {
        let mut state = lock(&self.queue.state);
        if !self.alive.load(Ordering::Acquire) || state.receiver_closed {
            return Err(closed());
        }
        if let Some(index) = state
            .frames
            .iter()
            .position(|frame| frame.request_id == Some(id))
        {
            // Dropping the removed frame immediately releases both count and
            // byte capacity. There is no remote request to cancel.
            drop(state.frames.remove(index));
            return Ok(());
        }
        if state.cancellations.len() >= super::MAX_PENDING_REQUESTS {
            return Err(io::Error::other("server cancellation limit exceeded"));
        }
        state.cancellations.push_back(id);
        drop(state);
        self.queue.ready.notify_one();
        Ok(())
    }

    fn enqueue(&self, bytes: &[u8], request_id: Option<u64>) -> io::Result<usize> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(closed());
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        self.validate_frame_size(bytes.len())?;
        let mut state = lock(&self.queue.state);
        if state.receiver_closed || state.writer_closed {
            return Err(closed());
        }
        if state.frames.len() >= state.capacity {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "server writer frame queue full",
            ));
        }
        // Reject count pressure before reserving or copying a potentially
        // large frame on every admission retry. The pump never holds this
        // lock while writing the pipe.
        self.budget
            .used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes.len())
                    .filter(|total| *total <= self.budget.limit)
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "server writer byte budget exhausted",
                )
            })?;
        let reservation = Reservation {
            budget: Arc::clone(&self.budget),
            bytes: bytes.len(),
        };
        let frame = Frame {
            bytes: bytes.to_vec(),
            request_id,
            _reservation: Some(reservation),
        };
        state.frames.push_back(frame);
        drop(state);
        self.queue.ready.notify_one();
        Ok(bytes.len())
    }
}

impl Drop for QueuedWriter {
    fn drop(&mut self) {
        lock(&self.queue.state).writer_closed = true;
        self.queue.ready.notify_one();
    }
}

impl Write for QueuedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.enqueue(bytes, None)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.alive.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(closed())
        }
    }
}

/// Exercise the real bounded queue with a deliberately paused consumer, while
/// keeping a connected fixture's original pipe owner alive for restoration.
#[cfg(test)]
pub(in crate::lsp) struct PausedOutbound<'a> {
    client: &'a super::JsonRpcClient,
    receiver: FrameReceiver,
    original: Option<QueuedWriter>,
}

#[cfg(test)]
impl<'a> PausedOutbound<'a> {
    pub(super) fn new(client: &'a super::JsonRpcClient, capacity: usize, bytes: usize) -> Self {
        let (writer, receiver) = QueuedWriter::channel(Arc::clone(&client.alive), capacity, bytes);
        let original = std::mem::replace(&mut *lock(&client.writer), writer);
        Self {
            client,
            receiver,
            original: Some(original),
        }
    }

    pub(in crate::lsp) fn drain(&self) -> Vec<serde_json::Value> {
        let mut frames = Vec::new();
        while let Ok(frame) = self.receiver.try_recv() {
            frames.push(
                super::read_frame(&mut std::io::BufReader::new(frame.bytes.as_slice()))
                    .expect("complete queued frame")
                    .expect("queued message"),
            );
        }
        frames
    }
}

#[cfg(test)]
impl Drop for PausedOutbound<'_> {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            *lock(&self.client.writer) = original;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::io::BufReader;
    use std::sync::Mutex;
    use std::sync::mpsc::{Receiver, SyncSender};
    use std::time::Duration;

    #[test]
    fn notification_batch_refusal_reserves_neither_a_prefix_nor_capacity() {
        for byte_pressure in [false, true] {
            let alive = Arc::new(AtomicBool::new(true));
            let (writer, receiver) = QueuedWriter::channel(
                alive,
                if byte_pressure { 3 } else { 2 },
                if byte_pressure { 12 } else { 64 },
            );
            writer.enqueue(b"held", Some(7)).unwrap();
            let mut frames = vec![b"first".to_vec(), b"second".to_vec()];
            assert_eq!(
                writer
                    .write_notification_batch(&mut frames)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(frames, [b"first".to_vec(), b"second".to_vec()]);
            assert_eq!(writer.budget.used.load(Ordering::Acquire), 4);
            let held = receiver.try_recv().unwrap();
            assert_eq!(held.request_id, Some(7));
            assert!(receiver.try_recv().is_err(), "no batch prefix was accepted");
            drop(held);

            writer.write_notification_batch(&mut frames).unwrap();
            assert!(frames.is_empty());
            assert_eq!(writer.budget.used.load(Ordering::Acquire), 11);
            assert_eq!(receiver.try_recv().unwrap().bytes, b"first");
            assert_eq!(writer.budget.used.load(Ordering::Acquire), 6);
            assert_eq!(receiver.try_recv().unwrap().bytes, b"second");
            assert_eq!(writer.budget.used.load(Ordering::Acquire), 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn notification_batches_reject_permanent_limits_without_retiring_pending_work() {
        for byte_limit in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let client = super::super::JsonRpcClient::spawn("cat", &[], &[], root.path()).unwrap();
            let first = serde_json::json!({
                "jsonrpc":"2.0", "method":"textDocument/didChange", "params":{}
            });
            let second = serde_json::json!({
                "jsonrpc":"2.0", "method":"textDocument/didSave", "params":{}
            });
            let bytes = super::super::encode_frame(&first).len()
                + super::super::encode_frame(&second).len();
            let paused = PausedOutbound::new(
                &client,
                if byte_limit { 3 } else { 1 },
                if byte_limit { bytes - 1 } else { 4096 },
            );
            let (_, response) = client
                .request("test/held", serde_json::Value::Null)
                .unwrap();
            let mut batch = super::super::JsonRpcClient::prepare_notifications(vec![
                ("textDocument/didChange", serde_json::json!({})),
                ("textDocument/didSave", serde_json::json!({})),
            ]);
            assert!(matches!(
                client.try_admit_notifications(&mut batch),
                Err(TransportError::Io(_))
            ));
            assert!(client.is_alive());
            assert_eq!(lock(&client.pending).len(), 1);
            assert!(matches!(
                response.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            let admitted = paused.drain();
            assert_eq!(admitted.len(), 1);
            assert_eq!(admitted[0]["method"], "test/held");
            assert!(matches!(
                client.try_admit_notifications(&mut batch),
                Err(TransportError::Io(_))
            ));
            assert!(paused.drain().is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn accepted_notification_batches_cannot_be_posted_twice() {
        let root = tempfile::tempdir().unwrap();
        let client = super::super::JsonRpcClient::spawn("cat", &[], &[], root.path()).unwrap();
        let paused = PausedOutbound::new(&client, 2, 4096);
        let mut batch = super::super::JsonRpcClient::prepare_notifications(vec![
            (
                "textDocument/didClose",
                serde_json::json!({"textDocument":{"uri":"file:///source"}}),
            ),
            (
                "textDocument/didOpen",
                serde_json::json!({"textDocument":{
                    "uri":"file:///source","languageId":"rust","version":1,"text":"source"
                }}),
            ),
        ]);
        client.try_admit_notifications(&mut batch).unwrap();
        let admitted = paused.drain();
        assert_eq!(admitted.len(), 2);
        assert_eq!(admitted[0]["method"], "textDocument/didClose");
        assert_eq!(admitted[1]["method"], "textDocument/didOpen");
        assert!(matches!(
            client.try_admit_notifications(&mut batch),
            Err(TransportError::Io(reason)) if reason.contains("already admitted")
        ));
        assert!(client.is_alive());
        assert!(paused.drain().is_empty());
    }

    #[test]
    fn reader_retires_all_pending_work_when_a_server_reply_cannot_be_admitted() {
        for byte_exhaustion in [false, true] {
            let alive = Arc::new(AtomicBool::new(true));
            let pending = Arc::new(Mutex::new(HashMap::new()));
            let (first_tx, first_rx) = std::sync::mpsc::sync_channel(1);
            let (second_tx, second_rx) = std::sync::mpsc::sync_channel(1);
            lock(&pending).insert(1, first_tx);
            lock(&pending).insert(2, second_tx);
            let limit = if byte_exhaustion { 1 } else { 4096 };
            let (mut queued, _outbound_rx) = QueuedWriter::channel(Arc::clone(&alive), 1, limit);
            if !byte_exhaustion {
                queued
                    .write_all(&super::super::encode_frame(&serde_json::json!({
                        "jsonrpc":"2.0", "method":"initialized"
                    })))
                    .expect("fill frame queue");
            }
            let writer = Arc::new(Mutex::new(queued));
            let (reader, mut server_stdout) = std::io::pipe().expect("server stdout pipe");
            let (notifications, _notification_rx) = std::sync::mpsc::sync_channel(1);
            let handled = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&handled);
            let handler: super::super::ServerRequestResponder = Arc::new(move |_, _| {
                observed.fetch_add(1, Ordering::SeqCst);
                Some(Ok(serde_json::json!({"applied":true})))
            });
            let reader_thread = std::thread::spawn(super::super::reader_loop(
                reader,
                Arc::clone(&pending),
                Arc::clone(&alive),
                Arc::clone(&writer),
                Arc::new(Mutex::new(super::super::TailBuffer::default())),
                notifications,
                Arc::new(std::sync::atomic::AtomicU64::new(0)),
                Arc::new(Mutex::new(Some(handler))),
            ));
            server_stdout
                .write_all(&super::super::encode_frame(&serde_json::json!({
                    "jsonrpc":"2.0", "id":"server/edit", "method":"workspace/applyEdit", "params":{}
                })))
                .expect("server request");
            // Keep server stdout open. A failed reply must retire the reader
            // now, not wait for a peer that is itself waiting for this reply.
            for receiver in [first_rx, second_rx] {
                let result = receiver
                    .recv_timeout(Duration::from_secs(5))
                    .expect("retired promptly");
                assert!(matches!(
                    result, Err(TransportError::Closed(reason))
                        if reason.contains("server reply queue failed")
                ));
            }
            reader_thread.join().expect("reader stopped");
            assert_eq!(
                handled.load(Ordering::SeqCst),
                1,
                "never replay host effects"
            );
            assert!(!alive.load(Ordering::Acquire));
            assert!(lock(&pending).is_empty());
            assert_eq!(
                lock(&writer).write(b"later").unwrap_err().kind(),
                io::ErrorKind::BrokenPipe
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn full_queue_refuses_only_the_unposted_request_and_capacity_can_be_reused() {
        for byte_exhaustion in [false, true] {
            let root = tempfile::tempdir().expect("tempdir");
            let client = super::super::JsonRpcClient::spawn("cat", &[], &[], root.path())
                .expect("transport");
            let first_frame = super::super::encode_frame(&serde_json::json!({
                "jsonrpc":"2.0", "id":1, "method":"test/first", "params":null
            }));
            let byte_limit = if byte_exhaustion {
                first_frame.len()
            } else {
                4096
            };
            let (queued, receiver) =
                QueuedWriter::channel(Arc::clone(&client.alive), 1, byte_limit);
            // Hold the real pump's sender so replacing admission for this
            // deterministic stall does not close the original transport.
            let _original = std::mem::replace(&mut *lock(&client.writer), queued);
            let (first_id, first_rx) = client.request("test/first", Value::Null).expect("first");
            assert!(matches!(
                client.request("test/other", Value::Null),
                Err(TransportError::Backpressure(_))
            ));
            assert!(
                client.is_alive(),
                "an unposted request cannot corrupt earlier traffic"
            );
            assert_eq!(lock(&client.pending).len(), 1);
            assert!(lock(&client.pending).contains_key(&first_id));
            assert!(matches!(
                first_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            let accepted = receiver.try_recv().expect("original request retained");
            assert_eq!(accepted.bytes, first_frame);
            drop(accepted);
            assert!(
                receiver.try_recv().is_err(),
                "rejected request must not be queued"
            );
            let (next_id, next_rx) = client
                .request("test/third", Value::Null)
                .expect("capacity reusable");
            assert_eq!(next_id, 3, "request identities are never reused");
            let (notification_tx, _notification_rx) = std::sync::mpsc::sync_channel(1);
            super::super::handle_message(
                &serde_json::json!({"jsonrpc":"2.0", "id":first_id, "result":"completed"}),
                &client.pending,
                &client.writer,
                &notification_tx,
                &client.dropped_notifications,
                &client.stderr_tail,
                &client.server_request_handler,
            )
            .expect("earlier response still routable");
            assert_eq!(
                first_rx.try_recv().expect("response").expect("success"),
                "completed"
            );
            client.kill();
            assert!(matches!(
                next_rx.try_recv(),
                Ok(Err(TransportError::Closed(_)))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn admission_retries_keep_one_identity_and_never_post_a_command_twice() {
        let root = tempfile::tempdir().expect("tempdir");
        let client =
            super::super::JsonRpcClient::spawn("cat", &[], &[], root.path()).expect("transport");
        let (queued, receiver) = QueuedWriter::channel(Arc::clone(&client.alive), 1, 4096);
        let _original = std::mem::replace(&mut *lock(&client.writer), queued);
        let (first_id, _response) = client.request("test/first", Value::Null).expect("first");
        let mut command = client
            .prepare_request(
                "workspace/executeCommand",
                serde_json::json!({"command":"run-once"}),
            )
            .expect("prepare once");
        for _ in 0..4 {
            assert!(matches!(
                client.try_admit_request(&mut command),
                Err(TransportError::Backpressure(_))
            ));
            assert!(client.is_alive());
            assert_eq!(lock(&client.pending).len(), 1);
            assert!(lock(&client.pending).contains_key(&first_id));
            assert_eq!(client.next_id.load(Ordering::SeqCst), 3);
        }
        drop(receiver.try_recv().expect("free capacity"));
        let (command_id, _response) = client
            .try_admit_request(&mut command)
            .expect("admit after drainage");
        assert_eq!(command_id, 2);
        assert!(matches!(
            client.try_admit_request(&mut command),
            Err(TransportError::Io(_))
        ));
        let frame = receiver.try_recv().expect("one command");
        let decoded = super::super::read_frame(&mut BufReader::new(frame.bytes.as_slice()))
            .expect("complete frame")
            .expect("command frame");
        assert_eq!(decoded["method"], "workspace/executeCommand");
        assert_eq!(decoded["params"]["command"], "run-once");
        assert_eq!(decoded["id"], serde_json::json!(command_id));
        assert!(
            receiver.try_recv().is_err(),
            "accepted work cannot be retried"
        );
        assert!(client.is_alive());
    }

    #[cfg(unix)]
    #[test]
    fn permanently_oversized_requests_do_not_wait_or_retire_other_work() {
        let root = tempfile::tempdir().expect("tempdir");
        let client =
            super::super::JsonRpcClient::spawn("cat", &[], &[], root.path()).expect("transport");
        let (queued, receiver) = QueuedWriter::channel(Arc::clone(&client.alive), 1, 4096);
        let budget = Arc::clone(&queued.budget);
        let _original = std::mem::replace(&mut *lock(&client.writer), queued);
        let (first_id, _response) = client.request("test/first", Value::Null).expect("first");
        let used = budget.used.load(Ordering::Acquire);
        assert!(matches!(
            client.request("test/oversized", Value::String("x".repeat(4096))),
            Err(TransportError::Io(reason)) if reason.contains("exceeds the writer byte budget")
        ));
        assert!(client.is_alive());
        assert_eq!(budget.used.load(Ordering::Acquire), used);
        assert_eq!(lock(&client.pending).len(), 1);
        assert!(lock(&client.pending).contains_key(&first_id));
        assert_eq!(
            receiver
                .try_recv()
                .expect("earlier request preserved")
                .request_id,
            Some(first_id)
        );
        assert!(receiver.try_recv().is_err());
        client
            .request("test/next", Value::Null)
            .expect("valid request remains admissible");
    }

    #[cfg(unix)]
    #[test]
    fn oversized_frame_is_rejected_even_when_all_completion_slots_are_occupied() {
        let root = tempfile::tempdir().expect("tempdir");
        let client =
            super::super::JsonRpcClient::spawn("cat", &[], &[], root.path()).expect("transport");
        let (queued, receiver) = QueuedWriter::channel(Arc::clone(&client.alive), 1, 256);
        let _original = std::mem::replace(&mut *lock(&client.writer), queued);
        for _ in 0..super::super::MAX_PENDING_REQUESTS {
            client
                .request("test/pending", Value::Null)
                .expect("admit unanswered request");
            drop(receiver.try_recv().expect("writer claims request"));
        }
        assert!(matches!(
            client.request("test/full", Value::Null),
            Err(TransportError::Backpressure(_))
        ));
        assert!(matches!(
            client.request("test/oversized", Value::String("x".repeat(256))),
            Err(TransportError::Io(reason)) if reason.contains("exceeds the writer byte budget")
        ));
        assert!(client.is_alive());
        assert_eq!(
            lock(&client.pending).len(),
            super::super::MAX_PENDING_REQUESTS
        );
        assert!(
            receiver.try_recv().is_err(),
            "rejected work was never posted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn abandoning_a_queued_request_releases_full_frame_and_byte_capacity() {
        for byte_exhaustion in [false, true] {
            let root = tempfile::tempdir().expect("tempdir");
            let client = super::super::JsonRpcClient::spawn("cat", &[], &[], root.path())
                .expect("transport");
            let frame = super::super::encode_frame(&serde_json::json!({
                "jsonrpc":"2.0", "id":1, "method":"test/queued", "params":null
            }));
            let limit = if byte_exhaustion { frame.len() } else { 4096 };
            let (queued, receiver) = QueuedWriter::channel(Arc::clone(&client.alive), 1, limit);
            let budget = Arc::clone(&queued.budget);
            let _original = std::mem::replace(&mut *lock(&client.writer), queued);
            let (id, response) = client.request("test/queued", Value::Null).expect("request");
            assert!(client.request("test/full", Value::Null).is_err());
            assert_eq!(budget.used.load(Ordering::Acquire), frame.len());

            // Use the real completion-owner drop path, with no receiver
            // progress possible between admission and abandonment.
            let started = std::time::Instant::now();
            drop(super::super::await_completion(
                response,
                Duration::from_secs(5),
                || client.cancel_request(id),
            ));
            assert!(started.elapsed() < Duration::from_secs(1));
            client.cancel_request(id);
            assert!(client.is_alive());
            assert!(lock(&client.pending).is_empty());
            assert_eq!(budget.used.load(Ordering::Acquire), 0);
            assert!(
                receiver.try_recv().is_err(),
                "neither request nor cancel was sent"
            );

            let (successor, _response) = client
                .request("test/next", Value::Null)
                .expect("withdrawal immediately frees capacity");
            let received = receiver.try_recv().expect("successor admitted");
            assert_eq!(received.request_id, Some(successor));
            assert!(receiver.try_recv().is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn all_written_requests_can_cancel_while_both_ordinary_limits_are_full() {
        let root = tempfile::tempdir().expect("tempdir");
        let client =
            super::super::JsonRpcClient::spawn("cat", &[], &[], root.path()).expect("transport");
        let (queued, receiver) = QueuedWriter::channel(Arc::clone(&client.alive), 1, 4096);
        let budget = Arc::clone(&queued.budget);
        let _original = std::mem::replace(&mut *lock(&client.writer), queued);
        let mut requests = Vec::new();
        for _ in 0..super::super::MAX_PENDING_REQUESTS {
            let (id, response) = client
                .request("test/pending", Value::Null)
                .expect("request");
            let frame = receiver.try_recv().expect("writer claims request");
            assert_eq!(frame.request_id, Some(id));
            drop(frame);
            requests.push((id, response));
        }
        assert!(client.request("test/over-limit", Value::Null).is_err());
        lock(&client.writer)
            .write_all(&[b'x'; 4096])
            .expect("hold the entire ordinary frame and byte budget");
        for (id, response) in requests {
            drop(super::super::await_completion(
                response,
                Duration::from_secs(5),
                || client.cancel_request(id),
            ));
            client.cancel_request(id);
        }
        assert!(client.is_alive());
        assert!(lock(&client.pending).is_empty());
        assert_eq!(budget.used.load(Ordering::Acquire), 4096);
        {
            let writer = lock(&client.writer);
            assert_eq!(
                lock(&writer.queue.state).cancellations.len(),
                super::super::MAX_PENDING_REQUESTS
            );
        }
        for id in 1..=super::super::MAX_PENDING_REQUESTS {
            let frame = receiver.try_recv().expect("reserved cancellation");
            assert!(frame.bytes.len() < 128, "bounded control frame");
            let decoded = super::super::read_frame(&mut BufReader::new(frame.bytes.as_slice()))
                .expect("complete control")
                .expect("control frame");
            assert_eq!(decoded["method"], "$/cancelRequest");
            assert_eq!(decoded["params"]["id"], serde_json::json!(id));
        }
        assert_eq!(
            receiver
                .try_recv()
                .expect("ordinary frame retained")
                .bytes
                .len(),
            4096
        );
        assert!(receiver.try_recv().is_err(), "cancellation is exactly once");
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[cfg(unix)]
    #[test]
    fn lost_document_notification_still_retires_the_transport() {
        let root = tempfile::tempdir().expect("tempdir");
        let client =
            super::super::JsonRpcClient::spawn("cat", &[], &[], root.path()).expect("transport");
        let (queued, _receiver) = QueuedWriter::channel(Arc::clone(&client.alive), 1, 4096);
        let _original = std::mem::replace(&mut *lock(&client.writer), queued);
        let (_, response) = client
            .request("test/pending", Value::Null)
            .expect("request");
        assert!(
            client
                .notify("textDocument/didChange", serde_json::json!({}))
                .is_err()
        );
        assert!(!client.is_alive());
        assert!(matches!(
            response.try_recv(),
            Ok(Err(TransportError::Closed(_)))
        ));
        assert!(lock(&client.pending).is_empty());
    }

    fn lifecycle_burst() -> Vec<(Value, Option<u64>)> {
        let mut frames = Vec::new();
        for id in 1..=super::super::MAX_PENDING_REQUESTS {
            let id = u64::try_from(id).expect("request id");
            frames.push((
                serde_json::json!({"jsonrpc":"2.0", "id":id, "method":"test/pending"}),
                Some(id),
            ));
        }
        for id in 1..=super::super::MAX_PENDING_REQUESTS {
            frames.push((
                serde_json::json!({"jsonrpc":"2.0", "method":"$/cancelRequest", "params":{"id":id}}),
                None,
            ));
        }
        // Match the document cache's supported working set, not the outbound
        // cap: reducing the queue back to 64 must make this regression fail.
        for method in ["textDocument/didClose", "textDocument/didOpen"] {
            for index in 0..128 {
                let mut document = serde_json::json!({"uri":format!("file:///work/{index}.rs")});
                if method == "textDocument/didOpen" {
                    document["languageId"] = serde_json::json!("rust");
                    document["version"] = serde_json::json!(1);
                    document["text"] = serde_json::json!("fn main() {}\n");
                }
                frames.push((
                    serde_json::json!({"jsonrpc":"2.0", "method":method,
                        "params":{"textDocument":document}}),
                    None,
                ));
            }
        }
        frames
    }

    #[test]
    fn supported_lifecycle_burst_fits_without_receiver_progress_and_stays_bounded() {
        let alive = Arc::new(AtomicBool::new(true));
        let (mut writer, receiver) =
            QueuedWriter::channel(Arc::clone(&alive), MAX_QUEUED_FRAMES, MAX_OUTBOUND_BYTES);
        let frames = lifecycle_burst();
        let mut total_bytes = 0;
        for (message, request_id) in &frames {
            let encoded = super::super::encode_frame(message);
            total_bytes += encoded.len();
            writer
                .enqueue(&encoded, *request_id)
                .expect("burst admission");
        }
        assert!(alive.load(Ordering::Acquire));
        assert_eq!(writer.budget.used.load(Ordering::Acquire), total_bytes);
        assert_eq!(
            writer.write(b"overflow").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(writer.budget.used.load(Ordering::Acquire), total_bytes);
        for (message, request_id) in frames {
            let frame = receiver.try_recv().expect("accepted frame");
            assert_eq!(frame.request_id, request_id);
            let decoded = super::super::read_frame(&mut BufReader::new(frame.bytes.as_slice()))
                .expect("complete frame");
            assert_eq!(decoded, Some(message));
        }
        assert!(receiver.try_recv().is_err());
        assert_eq!(writer.budget.used.load(Ordering::Acquire), 0);
        writer
            .write_all(b"later")
            .expect("capacity reusable after drainage");
    }

    struct PausedPipe {
        pipe: std::io::PipeWriter,
        entered: Option<SyncSender<()>>,
        resume: Receiver<()>,
    }

    impl Write for PausedPipe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).map_err(|_| closed())?;
                self.resume
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "test writer was not released")
                    })?;
            }
            self.pipe.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.pipe.flush()
        }
    }

    #[cfg(unix)]
    struct PausedTransport {
        client: super::super::JsonRpcClient,
        entered: Receiver<()>,
        resume: SyncSender<()>,
        reader: std::io::PipeReader,
        _original: QueuedWriter,
        writer_worker: std::thread::JoinHandle<()>,
    }

    #[cfg(unix)]
    fn paused_transport(root: &std::path::Path) -> PausedTransport {
        let client = super::super::JsonRpcClient::spawn("cat", &[], &[], root).expect("transport");
        let (reader, pipe) = std::io::pipe().expect("pipe");
        let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
        let (resume, resume_rx) = std::sync::mpsc::sync_channel(1);
        let (writer, writer_worker) = QueuedWriter::start(
            PausedPipe {
                pipe,
                entered: Some(entered_tx),
                resume: resume_rx,
            },
            Arc::clone(&client.pending),
            Arc::clone(&client.alive),
            None,
        )
        .expect("production pump");
        let original = std::mem::replace(&mut *lock(&client.writer), writer);
        PausedTransport {
            client,
            entered,
            resume,
            reader,
            _original: original,
            writer_worker,
        }
    }

    #[cfg(unix)]
    fn read_through_request(
        pipe: std::io::PipeReader,
        request_id: u64,
    ) -> (Receiver<Vec<Value>>, std::thread::JoinHandle<()>) {
        let (finished, received) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut frames = Vec::new();
            while let Some(frame) = super::super::read_frame(&mut reader).expect("read frame") {
                let last = frame.get("id").and_then(Value::as_u64) == Some(request_id);
                frames.push(frame);
                if last {
                    break;
                }
            }
            finished.send(frames).expect("report frames");
        });
        (received, reader)
    }

    #[test]
    fn stalled_production_pump_preserves_a_complete_document_invalidation_burst() {
        let (reader, pipe) = std::io::pipe().expect("pipe");
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (mut writer, writer_worker) = QueuedWriter::start(
            PausedPipe {
                pipe,
                entered: Some(entered_tx),
                resume: resume_rx,
            },
            Arc::clone(&pending),
            Arc::clone(&alive),
            None,
        )
        .expect("production pump");
        let first = serde_json::json!({"jsonrpc":"2.0", "method":"initialized"});
        writer
            .write_all(&super::super::encode_frame(&first))
            .expect("first frame");
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("writer is stalled");

        let mut expected = vec![first];
        for index in 0..128 {
            let close = serde_json::json!({"jsonrpc":"2.0", "method":"textDocument/didClose",
                "params":{"textDocument":{"uri":format!("file:///work/{index}.rs")}}});
            writer
                .write_all(&super::super::encode_frame(&close))
                .expect("all closes admitted");
            expected.push(close);
        }
        let (completion, _response) = std::sync::mpsc::sync_channel(1);
        lock(&pending).insert(7, completion);
        let lookup = serde_json::json!({"jsonrpc":"2.0", "id":7, "method":"workspace/symbol",
            "params":{"query":"after invalidation"}});
        writer
            .write_request(&super::super::encode_frame(&lookup), 7)
            .expect("lookup admitted");
        expected.push(lookup);
        assert!(alive.load(Ordering::Acquire));
        let budget = Arc::clone(&writer.budget);
        let (finished_tx, finished_rx) = std::sync::mpsc::sync_channel(1);
        let reader_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut received = Vec::new();
            while let Some(frame) = super::super::read_frame(&mut reader).expect("read frame") {
                received.push(frame);
            }
            finished_tx.send(received).expect("report frames");
        });
        resume_tx.send(()).expect("release writer");
        drop(writer);
        let received = finished_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("pump drained");
        reader_thread.join().expect("reader completed");
        assert_eq!(received, expected);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        writer_worker.join().expect("writer completed");
    }

    #[test]
    fn admission_is_bounded_without_any_receiver_progress() {
        let alive = Arc::new(AtomicBool::new(true));
        let (mut writer, receiver) = QueuedWriter::channel(alive, 2, 8);
        writer.write_all(b"abc").expect("first frame");
        writer.write_all(b"def").expect("second frame");
        assert_eq!(
            writer.write(b"g").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(writer.budget.used.load(Ordering::Acquire), 6);
        let first = receiver.try_recv().expect("first frame");
        assert_eq!(first.bytes, b"abc");
        // Moving a frame out of the queue does not free its in-flight budget.
        assert_eq!(
            writer.write(b"ghi").unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(writer.budget.used.load(Ordering::Acquire), 6);
        drop(first);
        writer.write_all(b"ghi").expect("released budget reused");
        assert_eq!(receiver.try_recv().expect("second frame").bytes, b"def");
        assert_eq!(receiver.try_recv().expect("third frame").bytes, b"ghi");
        assert_eq!(writer.budget.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn rejected_and_disconnected_frames_release_their_reservations() {
        let (mut writer, receiver) = QueuedWriter::channel(Arc::new(AtomicBool::new(true)), 1, 4);
        assert_eq!(
            writer.write(b"12345").unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(writer.budget.used.load(Ordering::Acquire), 0);
        writer.write_all(b"1234").expect("exact limit");
        drop(receiver);
        assert_eq!(writer.budget.used.load(Ordering::Acquire), 0);
        assert_eq!(
            writer.write(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(writer.budget.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn real_pipe_preserves_complete_frames_in_order() {
        let (reader, pipe) = std::io::pipe().expect("pipe");
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let (mut writer, writer_worker) =
            QueuedWriter::start(pipe, pending, alive, None).expect("pump");
        let first = serde_json::json!({"jsonrpc":"2.0", "id":1, "method":"initialize"});
        let second = serde_json::json!({"jsonrpc":"2.0", "method":"initialized"});
        writer
            .write_all(&super::super::encode_frame(&first))
            .expect("first");
        writer
            .write_all(&super::super::encode_frame(&second))
            .expect("second");
        drop(writer);
        let mut reader = BufReader::new(reader);
        assert_eq!(
            super::super::read_frame(&mut reader).expect("read first"),
            Some(first)
        );
        assert_eq!(
            super::super::read_frame(&mut reader).expect("read second"),
            Some(second)
        );
        assert_eq!(super::super::read_frame(&mut reader).expect("EOF"), None);
        writer_worker.join().expect("writer completed");
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::used_underscore_binding)]
    fn queued_abandoned_requests_and_cancels_do_not_reach_the_pipe() {
        let root = tempfile::tempdir().expect("tempdir");
        let PausedTransport {
            client,
            entered,
            resume,
            reader,
            _original,
            writer_worker,
        } = paused_transport(root.path());
        client
            .notify("initialized", Value::Null)
            .expect("first frame");
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("pipe is stalled");
        let (id, response) = client
            .request(
                "workspace/executeCommand",
                serde_json::json!({"command":"must-not-run"}),
            )
            .expect("queued request");
        drop(super::super::await_completion(
            response,
            Duration::from_secs(5),
            || client.cancel_request(id),
        ));
        let (successor, _response) = client.request("test/next", Value::Null).expect("successor");
        assert!(client.is_alive());
        assert_eq!(lock(&client.pending).len(), 1);
        let (finished, reader_thread) = read_through_request(reader, successor);
        resume.send(()).expect("release writer");
        let frames = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("pump drained");
        reader_thread.join().expect("reader completed");
        assert_eq!(
            frames.len(),
            2,
            "withdrawn work emits no request or cancellation"
        );
        assert_eq!(frames[0]["method"], "initialized");
        assert_eq!(frames[1]["method"], "test/next");
        assert!(client.is_alive());
        drop(_original);
        client.kill();
        writer_worker.join().expect("writer completed");
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::used_underscore_binding)]
    fn cancelling_a_claimed_write_precedes_queued_traffic_without_waiting_for_the_pipe() {
        let root = tempfile::tempdir().expect("tempdir");
        let PausedTransport {
            client,
            entered,
            resume,
            reader,
            _original,
            writer_worker,
        } = paused_transport(root.path());
        let (id, response) = client
            .request(
                "workspace/executeCommand",
                serde_json::json!({"command":"already-claimed"}),
            )
            .expect("request");
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("write has begun");
        client
            .notify(
                "textDocument/didClose",
                serde_json::json!({"textDocument":{"uri":"file:///closed.rs"}}),
            )
            .expect("queued lifecycle notification");
        let (successor, successor_response) =
            client.request("test/next", Value::Null).expect("successor");
        let started = std::time::Instant::now();
        drop(super::super::await_completion(
            response,
            Duration::from_secs(5),
            || client.cancel_request(id),
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        client.cancel_request(id);
        assert!(client.is_alive());
        assert_eq!(lock(&client.pending).len(), 1);
        let (finished, reader_thread) = read_through_request(reader, successor);
        resume.send(()).expect("release writer");
        let frames = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("pump drained");
        reader_thread.join().expect("reader completed");
        assert_eq!(frames.len(), 4, "one cancellation and no frame replay");
        assert_eq!(frames[0]["id"], serde_json::json!(id));
        assert_eq!(frames[0]["method"], "workspace/executeCommand");
        assert_eq!(frames[1]["method"], "$/cancelRequest");
        assert_eq!(frames[1]["params"]["id"], serde_json::json!(id));
        assert_eq!(frames[2]["method"], "textDocument/didClose");
        assert_eq!(frames[3]["id"], serde_json::json!(successor));
        let (notification_tx, _notification_rx) = std::sync::mpsc::sync_channel(1);
        super::super::handle_message(
            &serde_json::json!({"jsonrpc":"2.0", "id":successor, "result":"completed"}),
            &client.pending,
            &client.writer,
            &notification_tx,
            &client.dropped_notifications,
            &client.stderr_tail,
            &client.server_request_handler,
        )
        .expect("unrelated response still routable");
        assert_eq!(
            successor_response
                .try_recv()
                .expect("response")
                .expect("success"),
            "completed"
        );
        assert!(client.is_alive());
        drop(_original);
        client.kill();
        writer_worker.join().expect("writer completed");
    }

    #[test]
    fn broken_real_pipe_fails_pending_requests_and_retires_transport() {
        let (reader, pipe) = std::io::pipe().expect("pipe");
        drop(reader);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        lock(&pending).insert(7, sender);
        let alive = Arc::new(AtomicBool::new(true));
        let (mut writer, writer_worker) =
            QueuedWriter::start(pipe, Arc::clone(&pending), Arc::clone(&alive), None)
                .expect("pump");
        writer.write_all(b"frame").expect("queue admission");
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("failure delivered");
        assert!(matches!(result, Err(TransportError::Io(_))));
        assert!(!alive.load(Ordering::Acquire));
        assert!(lock(&pending).is_empty());
        assert_eq!(
            writer.write(b"later").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        writer_worker.join().expect("writer completed");
    }

    #[test]
    fn closure_does_not_block_on_an_already_completed_receiver() {
        let pending = Mutex::new(HashMap::new());
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.send(Ok(Value::Null)).expect("completed");
        lock(&pending).insert(1, sender);
        let alive = AtomicBool::new(true);
        close_pending(
            &pending,
            &alive,
            &TransportError::Closed("stop".to_string()),
        );
        assert_eq!(
            receiver.try_recv().expect("original result").unwrap(),
            Value::Null
        );
        assert!(!alive.load(Ordering::Acquire));
        assert!(lock(&pending).is_empty());
    }
}
