//! Cancellation-aware Python input delivery using the existing blocking-I/O pool.
//!
//! Only a complete successful receipt restores the unique pipe to its kernel.
//! A cancelled/failed write is never replayed. Unix parent endpoints are
//! nonblocking, so retained peer descriptors cannot prevent the write lease
//! or cell deadline from being observed. Other platforms retain their native
//! pipe behavior and process-tree cleanup discipline.

use std::future::Future;
use std::io::{self, Write};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};
use std::time::Instant;

use serde_json::Value;

use super::{DRAIN_FRAMES, DRAIN_TARGET_BYTES, IDLE_POLL, checkpoint, stopped};
use super::super::{PyKernel, output::Capture, session::CellSession, transport::MAX_FRAME_BYTES};
use crate::agent_cx::AgentCx;
use crate::error::Result;

struct Frame(Vec<u8>);

impl Write for Frame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Reserve the record terminator inside the receiver's byte limit.
        if bytes.len() > (MAX_FRAME_BYTES - 1).saturating_sub(self.0.len()) {
            return Err(io::Error::other("Python input frame limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn encode(value: &Value) -> Result<Vec<u8>> {
    let mut frame = Frame(Vec::new());
    serde_json::to_writer(&mut frame, value)
        .map_err(|_| stopped("EVAL_PROTOCOL_LIMIT", "Python input exceeds the 16 MiB encoded frame budget"))?;
    frame.0.push(b'\n');
    Ok(frame.0)
}

struct WriteLease(Arc<AtomicBool>);

impl Drop for WriteLease {
    fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
}

fn write_once<W: Write>(
    mut pipe: W,
    frame: &[u8],
    revoked: &AtomicBool,
    owner: &AgentCx,
    deadline: Instant,
) -> Result<W> {
    let check = || {
        if revoked.load(Ordering::SeqCst) {
            return Err(stopped("EVAL_CANCELLED", "Python input delivery was abandoned; no replay"));
        }
        checkpoint(owner, deadline)
    };
    let mut written = 0;
    while written < frame.len() {
        check()?;
        let end = written.saturating_add(64 * 1024).min(frame.len());
        match pipe.write(&frame[written..end]) {
            Ok(0) => return Err(stopped("EVAL_IO", "Python input write stopped; delivery uncertain, no replay")),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // The unique Unix stdin endpoint is prepared as nonblocking
                // before the reader worker starts. Retry the remaining slice,
                // never the whole frame, and recheck revocation/deadline even
                // when a descendant keeps the other end open indefinitely.
                std::thread::sleep(
                    IDLE_POLL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Err(_) => return Err(stopped("EVAL_IO", "Python input write failed; delivery uncertain, no replay")),
        }
    }
    check()?;
    pipe.flush().map_err(|_| stopped("EVAL_IO", "Python input flush failed; delivery uncertain, no replay"))?;
    check()?;
    Ok(pipe)
}

/// The recipient may finish reading before the writer's receipt is scheduled.
/// Keep its first non-stream frame for the ordinary runner, not an error or a
/// second dispatch. Stop draining there to preserve its ordering with output.
fn drain_during_delivery(
    lines: &super::Lines,
    pending_line: &mut Option<String>,
    id: u64,
    capture: &mut Capture,
) -> Result<()> {
    if pending_line.is_some() { return Ok(()); }
    let mut bytes = 0usize;
    for _ in 0..DRAIN_FRAMES {
        let received = lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).try_recv();
        let line = match received {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => return Err(stopped("EVAL_PROTOCOL_READ", &error)),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(stopped("EVAL_KERNEL_CRASH", "Python exited during input delivery"));
            }
        };
        bytes = bytes.saturating_add(line.len());
        if !line.trim().is_empty() {
            let frame: Value = serde_json::from_str(&line)
                .map_err(|_| stopped("EVAL_PROTOCOL", "invalid output during Python input delivery"))?;
            if frame.get("stream").is_some() {
                capture.accept(&frame, id)?;
            } else {
                *pending_line = Some(line);
                break;
            }
        }
        if bytes >= DRAIN_TARGET_BYTES { break; }
    }
    Ok(())
}

async fn wait_receipt<F: Future<Output = Result<std::process::ChildStdin>>>(
    kernel: &mut PyKernel,
    id: u64,
    deadline: Instant,
    capture: &mut Capture,
    session: &CellSession,
    operation: F,
) -> Result<()> {
    let owner = AgentCx::for_current_or_request();
    let mut pending = Box::pin(operation);
    loop {
        checkpoint(&owner, deadline)?;
        session.check_tool().await?;
        checkpoint(&owner, deadline)?;
        drain_during_delivery(&kernel.lines, &mut kernel.pending_line, id, capture)?;
        session.check_tool().await?;
        checkpoint(&owner, deadline)?;
        capture.publish(false);
        checkpoint(&owner, deadline)?;
        session.check_tool().await?;
        checkpoint(&owner, deadline)?;
        let sleeper = Box::pin(asupersync::time::sleep(
            asupersync::time::wall_now(), IDLE_POLL.min(deadline.saturating_duration_since(Instant::now())),
        ));
        match futures::future::select(pending, sleeper).await {
            futures::future::Either::Left((pipe, _)) => {
                let pipe = pipe?;
                checkpoint(&owner, deadline)?;
                session.check_tool().await?;
                checkpoint(&owner, deadline)?;
                kernel.stdin = Some(pipe);
                return Ok(());
            }
            futures::future::Either::Right(((), operation)) => pending = operation,
        }
    }
}

pub(in crate::eval) async fn send_python(
    kernel: &mut PyKernel,
    id: u64,
    deadline: Instant,
    capture: &mut Capture,
    session: &CellSession,
    frame: &Value,
) -> Result<()> {
    let owner = AgentCx::for_current_or_request();
    checkpoint(&owner, deadline)?;
    session.check_tool().await?;
    checkpoint(&owner, deadline)?;
    let frame = encode(frame)?;
    checkpoint(&owner, deadline)?;
    let pipe = kernel.stdin.take()
        .ok_or_else(|| stopped("EVAL_IO", "Python input has no completed write receipt"))?;
    let lease = WriteLease(Arc::new(AtomicBool::new(false)));
    let revoked = Arc::clone(&lease.0);
    // No application code or tool operation is run by this worker. It owns
    // one bounded immutable frame and the sole ChildStdin until completion.
    let operation = asupersync::runtime::spawn_blocking_io(move || {
        write_once(pipe, &frame, &revoked, &owner, deadline)
    });
    wait_receipt(kernel, id, deadline, capture, session, operation).await
}

#[cfg(test)]
mod tests;
