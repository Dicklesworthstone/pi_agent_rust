//! Native JavaScript console capture using the same bounded UTF-8 storage as Python.
//!
//! Coercion finishes before borrowing the sink. Never join all arguments into
//! another unbounded string; feed scalar-aligned chunks into the shared capture.
//! The deadline is checked between chunks as well as by QuickJS's interrupt.

use super::DeadlineCell;
use crate::eval::output::Capture;
use serde_json::json;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

const CHUNK_BYTES: usize = 64 * 1024;

pub(super) struct ConsoleCapture {
    capture: Capture,
    sequence: u64,
    active: bool,
}

impl ConsoleCapture {
    pub(super) fn new() -> Self {
        Self {
            capture: Capture::new(None),
            sequence: 0,
            active: false,
        }
    }

    pub(super) fn begin(&mut self) {
        self.capture = Capture::new(None);
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
            // One capture belongs to one invocation, so this internal id is
            // deliberately fixed. It is not a session or a guest-supplied id.
            self.capture.accept(&json!({
                "id": 1,
                "stream": {"sequence": sequence, "name": name, "text": &text[..end]},
            }), 1).map_err(|error| error.to_string())?;
            self.sequence = sequence;
            text = &text[end..];
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
        for (index, argument) in arguments.into_iter().enumerate() {
            if index != 0 {
                self.write(name, " ", deadline, origin)?;
            }
            self.write(name, &argument.0, deadline, origin)?;
        }
        self.write(name, "\n", deadline, origin)
    }

    pub(super) fn finish(&mut self) -> Result<String, String> {
        self.active = false;
        let mut response = json!({
            "stdout": "", "stderr": "", "outputSequence": self.sequence,
        });
        self.capture.finish_response(&mut response).map_err(|error| error.to_string())?;
        // As in Python, stdout precedes stderr in the final result. Each
        // stream preserves its own order and explicitly labels omitted bytes.
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
    use std::time::Duration;

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
        capture.begin();
        capture.write("stdout", "retained", &deadline, origin).unwrap();
        deadline.set(origin, origin);
        assert!(capture.write("stdout", "late", &deadline, origin).unwrap_err()
            .contains("EVAL_TIMEOUT"));
        assert_eq!(capture.finish().unwrap(), "retained");
        assert!(capture.write("stderr", "bad", &deadline, origin).unwrap_err()
            .contains("EVAL_OUTPUT_CLOSED"));
    }
}
