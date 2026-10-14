//! Request-scoped reverse DAP service. Reader threads enqueue detached JSON;
//! only the original async request may invoke the host, under its own owner.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::{AgentCx, DapError, DapTransport, Lifetime, ReplyWriter, encode_frame, lock, next_sequence};
use std::io::Write as _;

/// Return a DAP response body or a refusal. The callback must enforce its own
/// command/argument policy; registration is not permission to execute code.
/// Cancelled callbacks are dropped, not detached. Already performed effects
/// are not rolled back, so callback-owned resources need ordinary RAII cleanup.
pub type ReverseRequestHandler = Arc<
    dyn Fn(String, Value) -> BoxFuture<'static, std::result::Result<Value, String>> + Send + Sync,
>;

const MAX_QUEUED: usize = 8;
const MAX_REQUESTS: usize = 32;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_ERROR_BYTES: usize = 4096;

#[derive(Default)]
pub(super) struct State {
    active: Mutex<Option<Arc<Window>>>,
}

struct Window {
    queue: Mutex<Queue>,
}

#[derive(Default)]
struct Queue {
    closed: bool,
    requests: VecDeque<Request>,
    seen: BTreeSet<u64>,
}

struct Request {
    sequence: u64,
    command: String,
    arguments: Value,
}

impl State {
    pub(super) fn close(&self) {
        let previous = {
            let mut active = lock(&self.active);
            if let Some(window) = active.as_ref() {
                lock(&window.queue).closed = true;
            }
            active.take()
        };
        drop(previous);
    }

    /// None preserves the existing default-refusal path. False is malformed
    /// input/overflow: retire the connection rather than silently lose work.
    pub(super) fn enqueue(&self, message: &Value) -> Option<bool> {
        if message["type"] != "request" {
            return None;
        }
        let window = lock(&self.active).clone()?;
        let mut queue = lock(&window.queue);
        if queue.closed {
            return None;
        }
        let Some(sequence) = message["seq"].as_u64() else { return Some(false) };
        let Some(command) = message["command"].as_str()
            .filter(|command| !command.is_empty() && command.len() <= 256) else {
            return Some(false);
        };
        let empty = json!({});
        let arguments = message.get("arguments").unwrap_or(&empty);
        if !arguments.is_object()
            || serde_json::to_vec(arguments).map_or(true, |bytes| bytes.len() > MAX_ARGUMENT_BYTES)
            || queue.requests.len() >= MAX_QUEUED
            || queue.seen.len() >= MAX_REQUESTS
            || !queue.seen.insert(sequence)
        {
            return Some(false);
        }
        queue.requests.push_back(Request {
            sequence, command: command.to_string(), arguments: arguments.clone(),
        });
        Some(true)
    }
}

pub(super) struct Permit {
    life: Arc<Lifetime>,
    window: Arc<Window>,
    owner: AgentCx,
    deadline: Instant,
    timeout: Duration,
    handler: ReverseRequestHandler,
    writer: super::StdSyncSender<super::Frame>,
    sequence: Arc<super::AtomicU64>,
}

impl Permit {
    pub(super) fn open(
        transport: &DapTransport,
        owner: AgentCx,
        timeout: Duration,
        handler: ReverseRequestHandler,
    ) -> std::result::Result<Self, DapError> {
        let deadline = Instant::now().checked_add(timeout)
            .ok_or_else(|| DapError::Transport("reverse request timeout is too large".into()))?;
        let window = Arc::new(Window { queue: Mutex::new(Queue::default()) });
        {
            let mut active = lock(&transport.life.reverse.active);
            if active.is_some() || !transport.life.alive.load(Ordering::SeqCst) {
                return Err(DapError::Transport("reverse request window busy or connection closed".into()));
            }
            *active = Some(Arc::clone(&window));
        }
        Ok(Self {
            life: Arc::clone(&transport.life), window, owner, deadline, timeout, handler,
            writer: transport.writer.clone(), sequence: Arc::clone(&transport.next_seq),
        })
    }

    fn checkpoint(&self) -> std::result::Result<(), DapError> {
        self.owner.checkpoint().map_err(|_| DapError::Cancelled)?;
        if Instant::now() >= self.deadline {
            return Err(DapError::Timeout {
                timeout_ms: u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
            });
        }
        if !self.life.alive.load(Ordering::SeqCst) || lock(&self.window.queue).closed {
            return Err(DapError::Transport("reverse request window closed".into()));
        }
        Ok(())
    }

    pub(super) async fn drive(&self) -> DapError {
        loop {
            if let Err(error) = self.checkpoint() { return error; }
            let request = lock(&self.window.queue).requests.pop_front();
            if let Some(request) = request {
                let mut reply = Reply { permit: self, request: Some(request) };
                let request = reply.request.as_ref().expect("new reply");
                let command = request.command.clone();
                let arguments = request.arguments.clone();
                // Even callback construction and its synchronous destructor
                // belong to the originating owner. No native mutex is held.
                let result = self.owner.with_current(async {
                    (self.handler)(command, arguments).await
                }).await;
                if let Err(error) = self.checkpoint() { return error; }
                if let Err(error) = reply.finish(result) { return error; }
            } else {
                self.owner.time().sleep(Duration::from_millis(10)).await;
            }
        }
    }

    fn respond(&self, request: &Request, result: std::result::Result<Value, String>)
        -> std::result::Result<(), DapError>
    {
        if !self.life.alive.load(Ordering::SeqCst) {
            return Err(DapError::Transport("reverse reply connection closed".into()));
        }
        let sequence = next_sequence(&self.sequence)?;
        let result = match result {
            Ok(body) if !body.is_object() => Err("reverse host returned a non-object body".into()),
            other => other,
        };
        let mut message = json!({
            "seq":sequence, "type":"response", "request_seq":request.sequence,
            "command":request.command, "success":result.is_ok(),
        });
        match result {
            Ok(body) => message["body"] = body,
            Err(mut error) => {
                if error.len() > MAX_ERROR_BYTES {
                    let mut end = MAX_ERROR_BYTES;
                    while !error.is_char_boundary(end) { end -= 1; }
                    error.truncate(end);
                    error.push_str(" [truncated]");
                }
                message["message"] = Value::String(error);
            }
        }
        let result = ReplyWriter(self.writer.clone()).write_all(&encode_frame(&message));
        if let Err(error) = result {
            self.life.close("reverse reply could not enter the bounded writer");
            return Err(DapError::Transport(format!("reverse reply failed: {error}")));
        }
        Ok(())
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let requests = {
            let mut active = lock(&self.life.reverse.active);
            let mut queue = lock(&self.window.queue);
            queue.closed = true;
            if active.as_ref().is_some_and(|window| Arc::ptr_eq(window, &self.window)) {
                active.take();
            }
            std::mem::take(&mut queue.requests)
        };
        // Queued requests are refused, never inherited by a later window.
        for request in requests {
            let _ = self.respond(&request, Err("reverse host request scope ended".into()));
        }
    }
}

struct Reply<'a> {
    permit: &'a Permit,
    request: Option<Request>,
}

impl Reply<'_> {
    fn finish(&mut self, result: std::result::Result<Value, String>) -> std::result::Result<(), DapError> {
        let request = self.request.take().expect("reply sent once");
        self.permit.respond(&request, result)
    }
}

impl Drop for Reply<'_> {
    fn drop(&mut self) {
        if let Some(request) = self.request.take() {
            let _ = self.permit.respond(&request, Err(
                "reverse host request interrupted; completed effects are not rolled back".into(),
            ));
        }
    }
}

#[cfg(test)]
mod tests;
