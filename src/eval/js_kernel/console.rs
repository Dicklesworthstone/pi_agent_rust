//! Invocation-owned native JavaScript console output.
//!
//! Production cells send bounded chunks to the Rust host, which alone owns
//! callbacks and capture. Raw kernel tests retain the same bounded capture
//! locally. Coercion finishes before borrowing the sink; arguments are never
//! joined into a second unbounded String. Backpressure observes revocation.

use super::DeadlineCell;
use crate::eval::output::Capture;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{Duration, Instant};

const CHUNK_BYTES: usize = 64 * 1024;

pub(super) struct ConsoleCapture {
    capture: Option<Capture>,
    output: Option<SyncSender<Value>>,
    sequence: u64,
    active: bool,
}

impl ConsoleCapture {
    pub(super) fn new() -> Self {
        Self { capture: None, output: None, sequence: 0, active: false }
    }

    pub(super) fn begin(&mut self, output: Option<SyncSender<Value>>) {
        self.capture = output.is_none().then(|| Capture::new("js", None));
        self.output = output;
        self.sequence = 0;
        self.active = true;
    }

    fn write(
        &mut self,
        name: &str,
        mut text: &str,
        deadline: &DeadlineCell,
        origin: Instant,
    ) -> Result<(), String> {
        if !self.active {
            return Err("EVAL_OUTPUT_CLOSED: console capture is outside an active cell".into());
        }
        while !text.is_empty() {
            if deadline.expired(origin) {
                return Err("EVAL_TIMEOUT: console output exceeded the cell budget".into());
            }
            let mut end = text.len().min(CHUNK_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let sequence = self.sequence.checked_add(1)
                .ok_or_else(|| "EVAL_OUTPUT_LIMIT: console sequence exhausted".to_string())?;
            // The channel belongs to exactly one invocation. This internal
            // id is not a session identity or a guest-supplied identifier.
            let mut frame = json!({
                "id": 1,
                "stream": {"sequence": sequence, "name": name, "text": &text[..end]},
            });
            if let Some(output) = &self.output {
                loop {
                    if deadline.expired(origin) {
                        return Err("EVAL_TIMEOUT: console delivery exceeded the cell budget".into());
                    }
                    match output.try_send(frame) {
                        Ok(()) => break,
                        Err(TrySendError::Full(pending)) => {
                            frame = pending;
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            return Err("EVAL_OUTPUT_CLOSED: console host disconnected".into());
                        }
                    }
                }
            } else if let Some(capture) = &mut self.capture {
                capture.accept(&frame, 1).map_err(|error| error.to_string())?;
            } else {
                return Err("EVAL_OUTPUT_CLOSED: no console owner".into());
            }
            self.sequence = sequence;
            text = &text[end..];
        }
        Ok(())
    }

    fn append_fragment(
        &mut self,
        stream: &str,
        pending: &mut String,
        mut text: &str,
        deadline: &DeadlineCell,
        origin: Instant,
    ) -> Result<(), String> {
        while !text.is_empty() {
            let available = CHUNK_BYTES - pending.len();
            let mut end = text.len().min(available);
            while !text.is_char_boundary(end) { end -= 1; }
            pending.push_str(&text[..end]);
            text = &text[end..];
            if !text.is_empty() || pending.len() == CHUNK_BYTES {
                self.write(stream, pending, deadline, origin)?;
                pending.clear();
            }
        }
        Ok(())
    }

    fn write_arguments(
        &mut self,
        name: &str,
        arguments: Vec<rquickjs::Coerced<String>>,
        deadline: &DeadlineCell,
        origin: Instant,
    ) -> Result<(), String> {
        // Small log calls produce one frame, not one per argument/separator.
        // A large call never retains more than one extra 64 KiB staging chunk.
        let mut pending = String::new();
        for (index, argument) in arguments.into_iter().enumerate() {
            if index != 0 {
                self.append_fragment(name, &mut pending, " ", deadline, origin)?;
            }
            self.append_fragment(name, &mut pending, &argument.0, deadline, origin)?;
        }
        self.append_fragment(name, &mut pending, "\n", deadline, origin)?;
        self.write(name, &pending, deadline, origin)
    }

    pub(super) fn finish(&mut self) -> Result<String, String> {
        self.active = false;
        // Closing precedes the final reply. Once the host receives that reply,
        // draining this channel must observe every successfully sent frame.
        self.output.take();
        let Some(capture) = self.capture.take() else { return Ok(String::new()); };
        let mut response = json!({
            "stdout": "", "stderr": "", "outputSequence": self.sequence,
        });
        capture.finish_response(&mut response).map_err(|error| error.to_string())?;
        let mut text = response["stdout"].as_str().unwrap_or_default().to_string();
        text.push_str(response["stderr"].as_str().unwrap_or_default());
        Ok(text)
    }
}

pub(super) fn install(
    ctx: &rquickjs::Ctx<'_>,
    capture: &Rc<RefCell<ConsoleCapture>>,
    deadline: &DeadlineCell,
    origin: Instant,
) -> rquickjs::Result<()> {
    use rquickjs::function::{Func, Rest};

    let console = rquickjs::Object::new(ctx.clone())?;
    for (method, stream) in [
        ("log", "stdout"), ("info", "stdout"), ("debug", "stdout"),
        ("error", "stderr"), ("warn", "stderr"),
    ] {
        let sink = Rc::clone(capture);
        let deadline = deadline.clone();
        console.set(method, Func::from(
            move |ctx: rquickjs::Ctx<'_>, arguments: Rest<rquickjs::Coerced<String>>| {
                // Rest coercion may call guest code. It has already completed
                // when this borrow starts, so reentrant toString is safe.
                let result = sink.borrow_mut().write_arguments(stream, arguments.0, &deadline, origin);
                result.map_err(|error| rquickjs::Exception::throw_message(&ctx, &error))
            },
        ))?;
    }
    ctx.globals().set("console", console)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::js_kernel::{JsCellResponse, JsKernel};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn cell(kernel: &JsKernel, code: &str) -> JsCellResponse {
        kernel.submit(code.to_string(), Instant::now() + Duration::from_secs(10))
            .unwrap().recv_timeout(Duration::from_secs(15)).unwrap()
    }

    #[test]
    fn native_console_methods_keep_arguments_and_separate_streams() {
        let kernel = JsKernel::spawn().unwrap();
        let output = cell(&kernel,
            "console.log('a', 2); console.error('err'); console.info('b'); console.warn('warn'); console.debug('c');");
        assert!(output.ok, "{:?}", output.error);
        assert_eq!(output.console, "a 2\nb\nc\nerr\nwarn\n");
        assert!(output.result.is_none());
    }

    #[test]
    fn large_unicode_console_keeps_bounded_head_tail_without_changing_cell_result() {
        let kernel = JsKernel::spawn().unwrap();
        let output = cell(&kernel,
            "console.log('HEAD'); console.log('界'.repeat(400000)); console.log('TAIL'); 42");
        assert!(output.ok, "{:?}", output.error);
        assert_eq!(output.result.as_deref(), Some("42"));
        assert!(output.console.starts_with("HEAD\n"));
        assert!(output.console.ends_with("TAIL\n"));
        assert!(output.console.contains("EVAL_OUTPUT_TRUNCATED"));
        assert!(output.console.len() < 512 * 1024 + 256);
        assert!(!output.console.contains('\u{fffd}'));
        let next = cell(&kernel, "console.log('next');");
        assert_eq!(next.console, "next\n");
    }

    #[test]
    fn console_before_exception_survives_and_coercion_can_log_reentrantly() {
        let kernel = JsKernel::spawn().unwrap();
        let output = cell(&kernel,
            "globalThis.kept = 42; console.log({toString() { console.log('inner'); return 'outer'; }}); throw new Error('expected');");
        assert!(!output.ok);
        assert!(output.error.as_deref().unwrap().contains("expected"));
        assert_eq!(output.console, "inner\nouter\n");
        assert_eq!(cell(&kernel, "kept").result.as_deref(), Some("42"));
    }

    #[test]
    fn closed_and_expired_native_sinks_reject_writes() {
        let origin = Instant::now();
        let deadline = DeadlineCell::new(Arc::new(AtomicBool::new(false)));
        let mut capture = ConsoleCapture::new();
        assert!(capture.write("stdout", "bad", &deadline, origin).unwrap_err()
            .contains("EVAL_OUTPUT_CLOSED"));
        capture.begin(None);
        capture.write("stdout", "retained", &deadline, origin).unwrap();
        deadline.set(origin, origin);
        assert!(capture.write("stdout", "late", &deadline, origin).unwrap_err()
            .contains("EVAL_TIMEOUT"));
        assert_eq!(capture.finish().unwrap(), "retained");
        assert!(capture.write("stderr", "bad", &deadline, origin).unwrap_err()
            .contains("EVAL_OUTPUT_CLOSED"));
    }

    #[test]
    fn a_full_console_channel_observes_its_deadline_without_dropping_accepted_frames() {
        let origin = Instant::now();
        let deadline = DeadlineCell::new(Arc::new(AtomicBool::new(false)));
        deadline.set(origin + Duration::from_millis(100), origin);
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let mut capture = ConsoleCapture::new();
        capture.begin(Some(sender));
        capture.write("stdout", "accepted", &deadline, origin).unwrap();
        let error = capture.write("stdout", "blocked", &deadline, origin).unwrap_err();
        assert!(error.contains("EVAL_TIMEOUT"));
        assert!(origin.elapsed() < Duration::from_secs(5));
        assert_eq!(receiver.try_recv().unwrap()["stream"]["text"], "accepted");
        assert_eq!(capture.sequence, 1);
        assert!(capture.finish().unwrap().is_empty());
        assert!(matches!(receiver.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected)));
    }

    #[test]
    fn dropping_a_kernel_revokes_a_worker_blocked_on_console_backpressure() {
        let kernel = JsKernel::spawn().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reply = kernel.submit_with_output(
            "while (true) console.log('busy');".into(),
            Instant::now() + Duration::from_secs(60), Some(sender),
        ).unwrap();
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(kernel);
        assert!(matches!(reply.recv_timeout(Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)));
    }
}
