//! Service Python's owned output while one native tool future is pending.
//!
//! The Python asyncio loop can print or display during await tool.call. Its
//! single-slot reader channel must keep draining even when the native tool
//! itself waits for an output callback. No second tool is dispatched here.

use std::future::Future;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{output::Capture, session::CellSession};
use crate::agent_cx::AgentCx;
use crate::error::{Error, Result};

const DRAIN_FRAMES: usize = 8;
const DRAIN_TARGET_BYTES: usize = 256 * 1024;
const IDLE_POLL: Duration = Duration::from_millis(15);

type Lines = Mutex<mpsc::Receiver<std::result::Result<String, String>>>;

fn stopped(code: &str, message: &str) -> Error {
    Error::tool("eval", format!(
        "{code}: {message}; EVAL_STATE_RESET: Python kernel discarded"
    ))
}

fn checkpoint(owner: &AgentCx, deadline: Instant) -> Result<()> {
    owner.checkpoint().map_err(|_| stopped("EVAL_CANCELLED", "Python tool wait cancelled"))?;
    if Instant::now() >= deadline {
        return Err(stopped("EVAL_TIMEOUT", "Python tool wait exhausted the cell budget"));
    }
    Ok(())
}

/// A frame is indivisible; a single admitted rich frame may exceed the target
/// byte slice but never the transport/content caps. Yield between slices and
/// poll the native future after each slice, even under continuous output.
fn drain(
    lines: &Lines,
    id: u64,
    capture: &mut Capture,
    owner: &AgentCx,
    deadline: Instant,
) -> Result<()> {
    let mut bytes = 0usize;
    for _ in 0..DRAIN_FRAMES {
        checkpoint(owner, deadline)?;
        let received = lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner).try_recv();
        let line = match received {
            Ok(Ok(line)) => line,
            Ok(Err(error)) => return Err(stopped("EVAL_PROTOCOL_READ", &error)),
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(stopped("EVAL_KERNEL_CRASH", "Python exited during a tool exchange"));
            }
        };
        bytes = bytes.saturating_add(line.len());
        if !line.trim().is_empty() {
            let frame: Value = serde_json::from_str(&line)
                .map_err(|_| stopped("EVAL_PROTOCOL", "invalid output during Python tool wait"))?;
            if frame.get("stream").is_none() {
                return Err(stopped("EVAL_PROTOCOL", "expected output, not another request or completion, during Python tool wait"));
            }
            // The ordinary capture validates exact envelope/cell/sequence,
            // console bounds and MIME frames, and writes archives only once.
            capture.accept(&frame, id)?;
        }
        if bytes >= DRAIN_TARGET_BYTES { break; }
    }
    Ok(())
}

/// Poll the same native operation while servicing only this cell's stream.
/// The enclosing lifecycle watchdog also covers pending identity resolution.
/// Cancellation drops the operation; its own cleanup and the checked-out
/// PyKernel remain the owners. No work or reader is detached by this adapter.
pub(super) async fn wait_python<F: Future>(
    lines: &Lines,
    id: u64,
    deadline: Instant,
    capture: &mut Capture,
    session: &CellSession,
    operation: F,
) -> Result<F::Output> {
    let owner = AgentCx::for_current_or_request();
    let mut pending = Box::pin(operation);
    loop {
        checkpoint(&owner, deadline)?;
        session.check_tool().await?;
        checkpoint(&owner, deadline)?;
        drain(lines, id, capture, &owner, deadline)?;
        capture.publish(false);
        // Callbacks can cancel or switch conversations. Neither native
        // dispatch nor its reply may use authority from before that callback.
        checkpoint(&owner, deadline)?;
        session.check_tool().await?;
        checkpoint(&owner, deadline)?;
        let sleeper = Box::pin(asupersync::time::sleep(
            asupersync::time::wall_now(), IDLE_POLL.min(deadline.saturating_duration_since(Instant::now())),
        ));
        match futures::future::select(pending, sleeper).await {
            futures::future::Either::Left((reply, _)) => {
                checkpoint(&owner, deadline)?;
                session.check_tool().await?;
                checkpoint(&owner, deadline)?;
                return Ok(reply);
            }
            futures::future::Either::Right(((), operation)) => pending = operation,
        }
    }
}

#[cfg(test)]
mod tests;
