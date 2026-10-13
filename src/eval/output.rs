//! Per-invocation Python console capture and native ToolUpdate snapshots.
//!
//! Keep bounded UTF-8 head/tail data, not one String proportional to output.
//! Updates replace the previous preview; the final formatter receives the
//! captured stdout/stderr exactly once, independently of rich-display frames.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{ToolOutput, ToolUpdate};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const CAPTURE_BYTES: usize = 512 * 1024;
const PREVIEW_BYTES: usize = 32 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const UPDATE_INTERVAL: Duration = Duration::from_millis(50);

pub(super) type Update = Box<dyn Fn(ToolUpdate) + Send + Sync>;

struct BoundedText {
    head: String,
    tail: VecDeque<u8>,
    head_closed: bool,
    total: u64,
    limit: usize,
}

impl BoundedText {
    fn new(limit: usize) -> Self {
        Self { head: String::new(), tail: VecDeque::new(), head_closed: false, total: 0, limit }
    }

    fn push(&mut self, text: &str) {
        self.total = self.total.saturating_add(u64::try_from(text.len()).unwrap_or(u64::MAX));
        let mut remaining = text;
        if !self.head_closed {
            let available = (self.limit / 2).saturating_sub(self.head.len());
            let mut end = text.len().min(available);
            while !text.is_char_boundary(end) { end -= 1; }
            self.head.push_str(&text[..end]);
            remaining = &text[end..];
            // Freeze even when a multi-byte scalar leaves a few spare bytes;
            // a later ASCII write must not move ahead of earlier tail data.
            self.head_closed = end < text.len() || self.head.len() == self.limit / 2;
        }
        let tail_limit = self.limit.saturating_sub(self.head.len());
        for byte in remaining.bytes() {
            self.tail.push_back(byte);
            if self.tail.len() > tail_limit { let _ = self.tail.pop_front(); }
        }
        while self.tail.front().is_some_and(|byte| byte & 0xc0 == 0x80) {
            let _ = self.tail.pop_front();
        }
    }

    fn retained(&self) -> usize { self.head.len() + self.tail.len() }

    fn omitted(&self) -> u64 {
        self.total.saturating_sub(u64::try_from(self.retained()).unwrap_or(u64::MAX))
    }

    fn render(&self) -> String {
        let tail: Vec<u8> = self.tail.iter().copied().collect();
        let tail = String::from_utf8(tail).expect("capture keeps UTF-8 scalar boundaries");
        let mut text = self.head.clone();
        let omitted = self.omitted();
        if omitted > 0 {
            text.push_str(&format!("\n[EVAL_OUTPUT_TRUNCATED: {omitted} UTF-8 bytes omitted]\n"));
        }
        text.push_str(&tail);
        text
    }

    fn preview(&self) -> (String, u64) {
        let bytes: Vec<u8> = self.head.bytes().chain(self.tail.iter().copied())
            .skip(self.retained().saturating_sub(PREVIEW_BYTES))
            .skip_while(|byte| byte & 0xc0 == 0x80)
            .collect();
        let omitted = self.total.saturating_sub(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let text = String::from_utf8(bytes).expect("preview starts at a UTF-8 scalar boundary");
        if omitted == 0 { (text, 0) } else {
            (format!("[EVAL_OUTPUT_PREVIEW: {omitted} earlier UTF-8 bytes omitted]\n{text}"), omitted)
        }
    }
}

pub(super) struct Capture {
    stdout: BoundedText,
    stderr: BoundedText,
    sequence: u64,
    dirty: bool,
    last_update: Option<Instant>,
    on_update: Option<Update>,
}

fn invalid(message: &str) -> Error {
    Error::tool("eval", format!("EVAL_PROTOCOL: {message}; EVAL_STATE_RESET: Python kernel discarded"))
}

impl Capture {
    pub(super) fn new(on_update: Option<Update>) -> Self {
        Self {
            stdout: BoundedText::new(CAPTURE_BYTES),
            stderr: BoundedText::new(CAPTURE_BYTES),
            sequence: 0,
            dirty: false,
            last_update: None,
            on_update,
        }
    }

    pub(super) fn accept(&mut self, frame: &Value, expected_id: u64) -> Result<()> {
        if frame.get("id").and_then(Value::as_u64) != Some(expected_id)
            || frame.as_object().is_none_or(|object| object.len() != 2)
        {
            return Err(invalid("stream belongs to another cell or has ambiguous fields"));
        }
        let stream = &frame["stream"];
        if stream.as_object().is_none_or(|object| object.len() != 3) {
            return Err(invalid("malformed stream frame"));
        }
        let sequence = self.sequence.checked_add(1).ok_or_else(|| invalid("output sequence exhausted"))?;
        if stream.get("sequence").and_then(Value::as_u64) != Some(sequence) {
            return Err(invalid("out-of-order or duplicated output frame"));
        }
        let text = stream.get("text").and_then(Value::as_str)
            .ok_or_else(|| invalid("stream text must be a string"))?;
        if text.len() > CHUNK_BYTES { return Err(invalid("stream chunk exceeds 64 KiB")); }
        let target = match stream.get("name").and_then(Value::as_str) {
            Some("stdout") => &mut self.stdout,
            Some("stderr") => &mut self.stderr,
            _ => return Err(invalid("unknown output stream")),
        };
        target.push(text);
        self.sequence = sequence;
        self.dirty |= !text.is_empty();
        Ok(())
    }

    pub(super) fn publish(&mut self, force: bool) { self.publish_at(Instant::now(), force); }

    fn publish_at(&mut self, now: Instant, force: bool) {
        let Some(callback) = &self.on_update else { return; };
        if !self.dirty || !force && self.last_update.is_some_and(|last| now.saturating_duration_since(last) < UPDATE_INTERVAL) {
            return;
        }
        let (out, out_omitted) = self.stdout.preview();
        let (err, err_omitted) = self.stderr.preview();
        let details = self.details(out_omitted, err_omitted, true);
        // No native mutex or RefCell borrow is held across caller code.
        callback(ToolUpdate {
            content: vec![ContentBlock::Text(TextContent::new(format!("{out}{err}")))],
            details: Some(json!({"kernel":"python", "status":"running", "output":details})),
        });
        self.dirty = false;
        self.last_update = Some(now);
    }

    fn details(&self, stdout_omitted: u64, stderr_omitted: u64, preview: bool) -> Value {
        json!({
            "schema":"pi.eval.output.v1", "sequence":self.sequence, "preview":preview,
            "stdoutBytes":self.stdout.total, "stderrBytes":self.stderr.total,
            "stdoutOmittedBytes":stdout_omitted, "stderrOmittedBytes":stderr_omitted,
            "truncated":stdout_omitted > 0 || stderr_omitted > 0,
        })
    }

    pub(super) fn finish_response(&self, response: &mut Value) -> Result<()> {
        if response.get("outputSequence").and_then(Value::as_u64) != Some(self.sequence) {
            return Err(invalid("final output sequence does not match received frames"));
        }
        // The stream is the sole source of console bytes. Accepting a second
        // copy in the final response would duplicate or silently replace it.
        if response.get("stdout").and_then(Value::as_str) != Some("")
            || response.get("stderr").and_then(Value::as_str) != Some("")
        {
            return Err(invalid("final response duplicates streamed console output"));
        }
        response["stdout"] = Value::String(self.stdout.render());
        response["stderr"] = Value::String(self.stderr.render());
        Ok(())
    }

    pub(super) fn annotate(&self, output: &mut ToolOutput) {
        if self.sequence > 0 {
            if let Some(Value::Object(details)) = output.details.as_mut() {
                details.insert("output".into(), self.details(self.stdout.omitted(), self.stderr.omitted(), false));
            }
        }
    }

    /// Keep Python's existing tool-Error convention while retaining a bounded
    /// tail already received before timeout, cancellation, crash or bad frames.
    pub(super) fn with_error(&self, error: Error) -> Error {
        if self.stdout.total == 0 && self.stderr.total == 0 { return error; }
        let (out, _) = self.stdout.preview();
        let (err, _) = self.stderr.preview();
        Error::tool("eval", format!("{error}\n[Partial Python output received before failure]\n{out}{err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn frame(id: u64, sequence: u64, name: &str, text: &str) -> Value {
        json!({"id":id,"stream":{"sequence":sequence,"name":name,"text":text}})
    }

    #[test]
    fn fragmented_output_retains_exact_prefix_tail_and_omitted_count() {
        let mut capture = BoundedText::new(16);
        for ch in "abcdefghijklmnopqrstuvwxyz".chars() { capture.push(&ch.to_string()); }
        assert_eq!(capture.retained(), 16);
        assert_eq!(capture.omitted(), 10);
        assert_eq!(capture.render(), "abcdefgh\n[EVAL_OUTPUT_TRUNCATED: 10 UTF-8 bytes omitted]\nstuvwxyz");
    }

    #[test]
    fn unicode_head_boundary_cannot_reorder_later_ascii_writes() {
        let mut capture = BoundedText::new(8);
        capture.push("界界");
        capture.push("a");
        assert_eq!(capture.render(), "界界a");
        capture.push("😀z");
        assert_eq!(capture.omitted(), 4);
        assert!(capture.render().starts_with("界\n[EVAL_OUTPUT_TRUNCATED"));
        assert!(capture.render().ends_with("😀z"));
        assert!(capture.retained() <= 8);
    }

    #[test]
    fn bounded_capture_handles_large_chunks_and_plain_output_without_changes() {
        let mut capture = BoundedText::new(64);
        capture.push(&"界".repeat(10000));
        assert!(capture.retained() <= 64);
        assert_eq!(capture.total, 30000);
        assert_eq!(capture.omitted() + capture.retained() as u64, capture.total);
        let mut plain = BoundedText::new(64);
        for text in ["hello", "\r", "\n", "😀", "\0"] { plain.push(text); }
        assert_eq!(plain.render(), "hello\r\n😀\0");
        assert_eq!(plain.omitted(), 0);
    }

    #[test]
    fn invalid_stream_frames_do_not_mutate_the_capture() {
        let mut capture = Capture::new(None);
        for bad in [
            frame(9, 1, "stdout", "bad"), frame(7, 2, "stdout", "bad"),
            frame(7, 1, "unknown", "bad"), frame(7, 1, "stdout", &"x".repeat(CHUNK_BYTES + 1)),
            json!({"id":7,"stream":{"sequence":1,"name":"stdout","text":"bad"},"ok":true}),
        ] {
            assert!(capture.accept(&bad, 7).is_err());
            assert_eq!(capture.sequence, 0);
            assert_eq!(capture.stdout.total, 0);
        }
        capture.accept(&frame(7, 1, "stdout", "good"), 7).unwrap();
        assert!(capture.accept(&frame(7, 1, "stdout", "duplicate"), 7).is_err());
        assert_eq!(capture.stdout.render(), "good");
    }

    #[test]
    fn updates_are_coalesced_replacement_snapshots_not_repeated_deltas() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let mut capture = Capture::new(Some(Box::new(move |event| sink.lock().unwrap().push(event))));
        let now = Instant::now();
        capture.accept(&frame(1, 1, "stdout", "first"), 1).unwrap();
        capture.publish_at(now, false);
        capture.accept(&frame(1, 2, "stdout", " second"), 1).unwrap();
        capture.publish_at(now, false);
        assert_eq!(events.lock().unwrap().len(), 1);
        capture.publish_at(now + UPDATE_INTERVAL, false);
        capture.publish_at(now + UPDATE_INTERVAL, true);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[1].content[0], ContentBlock::Text(text) if text.text == "first second"));
        assert_eq!(events[1].details.as_ref().unwrap()["output"]["stdoutBytes"], 12);
    }

    #[test]
    fn final_output_is_assembled_once_and_sequence_gaps_are_errors() {
        let mut capture = Capture::new(None);
        capture.accept(&frame(2, 1, "stdout", "out"), 2).unwrap();
        capture.accept(&frame(2, 2, "stderr", "err"), 2).unwrap();
        let mut response = json!({"stdout":"","stderr":"","outputSequence":1});
        assert!(capture.finish_response(&mut response).is_err());
        response["outputSequence"] = json!(2);
        capture.finish_response(&mut response).unwrap();
        assert_eq!(response["stdout"], "out");
        assert_eq!(response["stderr"], "err");
        assert!(capture.finish_response(&mut response).is_err());
    }

    #[test]
    fn previews_and_partial_errors_have_independent_small_bounds() {
        let mut capture = Capture::new(None);
        for sequence in 1..=30 {
            capture.accept(&frame(3, sequence, "stdout", &"界".repeat(10000)), 3).unwrap();
        }
        let (preview, omitted) = capture.stdout.preview();
        assert!(preview.len() < PREVIEW_BYTES + 128);
        assert!(omitted > 0);
        assert!(capture.stdout.render().len() < CAPTURE_BYTES + 128);
        let error = capture.with_error(Error::tool("eval", "EVAL_TIMEOUT: stopped"));
        assert!(error.to_string().contains("EVAL_TIMEOUT"));
        assert!(error.to_string().contains("Partial Python output"));
        assert!(error.to_string().len() < PREVIEW_BYTES + 512);
    }

    fn python() -> Option<(tempfile::TempDir, crate::eval::EvalTool)> {
        if !std::process::Command::new("python3").arg("--version")
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
            .status().is_ok_and(|status| status.success())
        {
            eprintln!("Skipping: python3 not available");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let tool = crate::eval::EvalTool::new(dir.path());
        Some((dir, tool))
    }

    fn run<F: std::future::Future>(future: F) -> F::Output {
        asupersync::runtime::RuntimeBuilder::new().build().unwrap().block_on(future)
    }

    fn text(content: &[ContentBlock]) -> String {
        content.iter().filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        }).collect()
    }

    #[test]
    fn actual_eval_update_unblocks_the_cell_before_it_can_finish() {
        use crate::tools::Tool;
        let Some((dir, tool)) = python() else { return; };
        let gate = dir.path().join("progress-gate");
        let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let callback_seen = Arc::clone(&seen);
        let callback = Box::new(move |event: ToolUpdate| {
            if text(&event.content).contains("progress-marker") {
                std::fs::write(&gate, "admitted").unwrap();
                callback_seen.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let code = concat!(
            "import time\nfrom pathlib import Path\nprint('progress-marker', flush=True)\n",
            "deadline = time.monotonic() + 10\n",
            "while not Path('progress-gate').exists():\n",
            "    if time.monotonic() >= deadline: raise RuntimeError('no live callback')\n",
            "    time.sleep(0.01)\n",
            "print('finished-once')\n42",
        );
        let output = run(tool.execute("live", json!({"code":code}), Some(callback))).unwrap();
        assert!(!output.is_error, "{}", text(&output.content));
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(text(&output.content).matches("progress-marker").count(), 1);
        assert_eq!(text(&output.content).matches("finished-once").count(), 1);
        assert!(text(&output.content).trim().ends_with("42"));
    }

    #[test]
    fn actual_eval_large_console_output_has_head_tail_and_honest_metadata() {
        use crate::tools::Tool;
        let Some((_dir, tool)) = python() else { return; };
        let output = run(tool.execute("large", json!({
            "code":"print('BEGIN-' + '界' * 500000 + '-END', end='')"
        }), None)).unwrap();
        assert!(!output.is_error, "{}", text(&output.content));
        let rendered = text(&output.content);
        assert!(rendered.contains("BEGIN-"));
        assert!(rendered.trim().ends_with("-END"));
        assert!(rendered.contains("EVAL_OUTPUT_TRUNCATED"));
        assert!(rendered.len() < CAPTURE_BYTES + 256);
        let meta = &output.details.as_ref().unwrap()["output"];
        assert_eq!(meta["stdoutBytes"], 1_500_010);
        assert_eq!(meta["stderrBytes"], 0);
        assert_eq!(meta["preview"], false);
        assert_eq!(meta["truncated"], true);
        assert!(meta["stdoutOmittedBytes"].as_u64().unwrap() > 900_000);
    }

    #[test]
    fn actual_eval_keeps_console_and_rich_display_before_an_exception() {
        use crate::tools::Tool;
        let Some((_dir, tool)) = python() else { return; };
        let output = run(tool.execute("mixed", json!({"code":concat!(
            "import sys\nprint('kept-once')\nprint('warning-once', file=sys.stderr)\n",
            "display({'answer':42})\nraise ValueError('expected-failure')"
        )}), None)).unwrap();
        assert!(output.is_error);
        let rendered = text(&output.content);
        assert_eq!(rendered.matches("kept-once").count(), 1);
        assert_eq!(rendered.matches("warning-once").count(), 1);
        assert!(rendered.contains("ValueError: expected-failure"));
        let details = output.details.as_ref().unwrap();
        assert_eq!(details["jsonOutputs"], json!([{"answer":42}]));
        assert_eq!(details["output"]["stdoutBytes"], 10);
        assert_eq!(details["output"]["stderrBytes"], 13);
    }

    #[test]
    fn actual_eval_timeout_retains_received_tail_without_becoming_success() {
        use crate::tools::Tool;
        let Some((_dir, tool)) = python() else { return; };
        run(tool.execute("seed", json!({"code":"keep = 7"}), None)).unwrap();
        let error = run(tool.execute("timeout", json!({
            "code":"print('timeout-progress', flush=True)\nimport time\ntime.sleep(60)",
            "timeout_secs":2
        }), None)).unwrap_err();
        assert!(error.to_string().contains("EVAL_TIMEOUT"));
        assert!(error.to_string().contains("Partial Python output"));
        assert!(error.to_string().contains("timeout-progress"));
        let next = run(tool.execute("fresh", json!({"code":"'keep' in globals()"}), None)).unwrap();
        assert!(text(&next.content).contains("False"));
        assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    }

    #[test]
    fn actual_eval_callback_cancellation_stops_continuous_output_and_preserves_tail() {
        use crate::tools::Tool;
        let Some((_dir, tool)) = python() else { return; };
        let owner = crate::agent_cx::AgentCx::for_testing();
        let stop = owner.clone();
        let callback = Box::new(move |_: ToolUpdate| { stop.set_cancel_requested(true); });
        let started = Instant::now();
        let error = run(owner.with_current(tool.execute("cancel", json!({
            "code":"while True: print('cancel-progress', flush=True)", "timeout_secs":60
        }), Some(callback)))).unwrap_err();
        assert!(error.to_string().contains("EVAL_CANCELLED"), "{error}");
        assert!(error.to_string().contains("cancel-progress"));
        assert!(started.elapsed() < Duration::from_secs(10));
        let next = run(tool.execute("fresh", json!({"code":"42"}), None)).unwrap();
        assert!(!next.is_error);
        assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    }

    #[test]
    fn actual_eval_retained_tasks_cannot_print_into_another_cells_result() {
        use crate::tools::Tool;
        let Some((_dir, tool)) = python() else { return; };
        let seeded = run(tool.execute("task", json!({"code":concat!(
            "import asyncio\n",
            "async def late():\n    await asyncio.sleep(0.05)\n    print('foreign-output')\n",
            "pending = asyncio.create_task(late())"
        )}), None)).unwrap();
        assert!(!seeded.is_error);
        let output = run(tool.execute("later", json!({"code":concat!(
            "await asyncio.sleep(0.1)\n",
            "try:\n    await pending\nexcept RuntimeError as error:\n    message = str(error)\nmessage"
        )}), None)).unwrap();
        assert!(!output.is_error, "{}", text(&output.content));
        assert!(text(&output.content).contains("EVAL_OUTPUT_CLOSED"));
        assert!(!text(&output.content).contains("foreign-output"));
    }
}
