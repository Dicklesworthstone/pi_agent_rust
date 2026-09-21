//! Parent-side validation of the native child's JSONL protocol.
//!
//! Process exit is not an agent completion. Only the final `agent_end` with a
//! successful assistant message authorizes a completed delegation. Streaming
//! text is a preview; reasoning and tool argument deltas are never answers.

use serde_json::Value;
#[cfg(any(not(unix), test))]
use std::io::BufRead;

pub(super) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// A frame can contain a whole turn history. Bound queued frames independently
/// from the answer preview instead of retaining 256 multi-megabyte histories.
#[cfg(any(not(unix), test))]
pub(super) const PIPE_QUEUE_CAPACITY: usize = 2;
const MAX_ANSWER_BYTES: usize = 256 * 1024;
const MAX_CONTENT_BLOCKS: usize = 4096;

pub(super) const FRAME_LIMIT: &str = "PI_SUBAGENT_FRAME_LIMIT: child output frame exceeds 8 MiB";
pub(super) const PIPE_ERROR: &str = "PI_SUBAGENT_PIPE_ERROR: failed to read child output";
const INVALID_FRAME: &str = "PI_SUBAGENT_PROTOCOL: child stdout is not a valid JSONL event";
const INVALID_MESSAGE: &str =
    "PI_SUBAGENT_PROTOCOL: child completion has an invalid assistant message";
const ANSWER_LIMIT: &str = "PI_SUBAGENT_OUTPUT_LIMIT: child answer exceeds 256 KiB; it cannot be accepted or schema-validated";

/// Read a complete line without first allocating an unbounded `String` through
/// `BufRead::lines`. CRLF and a complete final line without a newline are legal.
/// The caller decides whether a non-UTF-8 diagnostic is lossy or fatal.
#[cfg(any(not(unix), test))]
pub(super) fn read_frame<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, &'static str> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf().map_err(|_| PIPE_ERROR)?;
        if chunk.is_empty() {
            return Ok((!frame.is_empty()).then_some(frame));
        }
        let newline = chunk.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(chunk.len());
        if take > MAX_FRAME_BYTES.saturating_sub(frame.len()) {
            return Err(FRAME_LIMIT);
        }
        frame.extend_from_slice(&chunk[..take]);
        let consumed = take + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(Some(frame));
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct ChildProtocol {
    completed: bool,
    failure: Option<&'static str>,
}

impl ChildProtocol {
    /// Update a bounded preview. `true` means callers should publish an update.
    /// A protocol failure is sticky and can never be rescued by a later frame.
    pub(super) fn ingest(&mut self, line: &str, output: &mut String) -> Result<bool, &'static str> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let outcome = self.ingest_inner(line, output);
        if let Err(error) = outcome {
            self.completed = false;
            self.failure = Some(error);
        }
        outcome
    }

    /// The `message_update` arm: one streaming assistant event.
    ///
    /// Split out of [`Self::ingest_inner`], which the nested match over the
    /// update's own type pushed past the line limit. It is the only arm that
    /// has to distinguish several event shapes rather than one.
    fn ingest_message_update(event: &Value, output: &mut String) -> Result<bool, &'static str> {
        let update = event.get("assistantMessageEvent").ok_or(INVALID_FRAME)?;
        match update.get("type").and_then(Value::as_str) {
            Some("start") => {
                output.clear();
                Ok(true)
            }
            Some("text_delta") => {
                let delta = update
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or(INVALID_FRAME)?;
                append_answer(output, delta)?;
                Ok(!delta.is_empty())
            }
            // The final message, not accumulated previews, is the authority.
            // A provider may revise text before completion.
            Some("done") => {
                replace_answer(update.get("message").ok_or(INVALID_MESSAGE)?, output)?;
                Ok(true)
            }
            Some("error") => {
                // The agent may retry a provider failure. A subsequent
                // successful agent_end must still explicitly prove it.
                Ok(false)
            }
            // Never interpret a bare delta as prose: both thinking and tool
            // argument events also contain a `delta` field.
            Some(_) => Ok(false),
            None => Err(INVALID_FRAME),
        }
    }

    fn ingest_inner(&mut self, line: &str, output: &mut String) -> Result<bool, &'static str> {
        if line.len() > MAX_FRAME_BYTES {
            return Err(FRAME_LIMIT);
        }
        let event: Value = serde_json::from_str(line).map_err(|_| INVALID_FRAME)?;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or(INVALID_FRAME)?;
        match kind {
            "agent_start" => {
                self.completed = false;
                output.clear();
                Ok(true)
            }
            "message_start" => {
                self.completed = false;
                if event.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                    output.clear();
                    return Ok(true);
                }
                Ok(false)
            }
            "message_update" => {
                self.completed = false;
                Self::ingest_message_update(&event, output)
            }
            "message_end" => {
                self.completed = false;
                let message = event.get("message").ok_or(INVALID_MESSAGE)?;
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    replace_answer(message, output)?;
                    return Ok(true);
                }
                Ok(false)
            }
            "agent_end" => {
                if event.get("error").is_some_and(|error| !error.is_null()) {
                    return Err("PI_SUBAGENT_FAILED: child agent reported an unsuccessful run");
                }
                let last = event
                    .get("messages")
                    .and_then(Value::as_array)
                    .and_then(|messages| messages.last())
                    .ok_or(INVALID_MESSAGE)?;
                // Searching backwards could accept an old answer while the
                // actual run ended on an unresolved tool result or new input.
                if last.get("role").and_then(Value::as_str) != Some("assistant") {
                    return Err(
                        "PI_SUBAGENT_INCOMPLETE: child ended without a final assistant answer",
                    );
                }
                replace_answer(last, output)?;
                match last.get("stopReason").and_then(Value::as_str) {
                    Some("stop") => {}
                    Some("length") => {
                        return Err("PI_SUBAGENT_TRUNCATED: child answer hit its output limit");
                    }
                    Some("toolUse" | "pauseTurn") => {
                        return Err(
                            "PI_SUBAGENT_INCOMPLETE: child requires another tool or continuation turn",
                        );
                    }
                    Some("refusal") => return Err("PI_SUBAGENT_REFUSAL: child declined the task"),
                    Some("error" | "aborted") => {
                        return Err("PI_SUBAGENT_FAILED: child generation failed or was aborted");
                    }
                    _ => return Err(INVALID_MESSAGE),
                }
                if last
                    .get("errorMessage")
                    .is_some_and(|error| !error.is_null())
                {
                    return Err("PI_SUBAGENT_FAILED: child final message contains an error");
                }
                if last
                    .get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|blocks| {
                        blocks.iter().any(|block| {
                            block.get("type").and_then(Value::as_str) == Some("toolCall")
                        })
                    })
                {
                    return Err(
                        "PI_SUBAGENT_INCOMPLETE: child final answer contains unresolved tool calls",
                    );
                }
                if output.trim().is_empty() {
                    return Err("PI_SUBAGENT_EMPTY_RESULT: child completed without an answer");
                }
                self.completed = true;
                Ok(true)
            }
            "error" => Err("PI_SUBAGENT_FAILED: child emitted an error event"),
            "turn_start" | "tool_execution_start" => {
                self.completed = false;
                Ok(false)
            }
            _ => Ok(false), // Session headers, usage and future nonterminal events.
        }
    }

    pub(super) const fn finish(&self) -> Result<(), &'static str> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.completed {
            return Err("PI_SUBAGENT_INCOMPLETE: child exited before a successful agent_end");
        }
        Ok(())
    }
}

fn append_answer(output: &mut String, text: &str) -> Result<(), &'static str> {
    if text.len() > MAX_ANSWER_BYTES.saturating_sub(output.len()) {
        return Err(ANSWER_LIMIT);
    }
    output.push_str(text);
    Ok(())
}

fn replace_answer(message: &Value, output: &mut String) -> Result<(), &'static str> {
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .ok_or(INVALID_MESSAGE)?;
    if blocks.len() > MAX_CONTENT_BLOCKS {
        return Err(INVALID_MESSAGE);
    }
    let mut answer = String::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .ok_or(INVALID_MESSAGE)?;
            append_answer(&mut answer, text)?;
        }
    }
    *output = answer;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufReader, Cursor};

    fn message(text: &str, reason: &str) -> Value {
        json!({"role":"assistant","stopReason":reason,"content":[{"type":"text","text":text}]})
    }

    fn end(text: &str, reason: &str) -> Value {
        json!({"type":"agent_end","messages":[message(text, reason)]})
    }

    fn feed(
        state: &mut ChildProtocol,
        output: &mut String,
        event: &Value,
    ) -> Result<bool, &'static str> {
        state.ingest(&event.to_string(), output)
    }

    #[test]
    fn only_text_deltas_become_answer_previews() {
        let mut state = ChildProtocol::default();
        let mut output = String::new();
        for kind in ["thinking_delta", "toolcall_delta", "future_delta"] {
            assert!(!feed(&mut state, &mut output, &json!({"type":"message_update","assistantMessageEvent":{"type":kind,"delta":"not answer text"}})).unwrap());
        }
        feed(&mut state, &mut output, &json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"preview"}})).unwrap();
        assert_eq!(output, "preview");
        assert!(state.finish().is_err());
    }

    #[test]
    fn authoritative_final_snapshot_replaces_previews_and_joins_all_text_blocks() {
        let mut state = ChildProtocol::default();
        let mut output = "old tool-plan prose".to_string();
        let final_message = json!({"role":"assistant","stopReason":"stop","content":[
            {"type":"text","text":"first "}, {"type":"thinking","thinking":"private"},
            {"type":"text","text":"日本語"}, {"type":"redacted_thinking","data":"opaque"}
        ]});
        feed(
            &mut state,
            &mut output,
            &json!({"type":"message_end","message":final_message}),
        )
        .unwrap(); // ubs:ignore test assertion
        assert_eq!(output, "first 日本語");
        assert!(
            state.finish().is_err(),
            "message_end alone is not agent completion"
        );
        feed(
            &mut state,
            &mut output,
            &json!({"type":"agent_end","messages":[final_message]}),
        )
        .unwrap();
        assert_eq!(output, "first 日本語");
        state.finish().unwrap();
    }

    #[test]
    fn separate_assistant_turns_do_not_concatenate_planning_and_final_answers() {
        let mut state = ChildProtocol::default();
        let mut output = String::new();
        feed(
            &mut state,
            &mut output,
            &json!({"type":"message_end","message":message("planning", "toolUse")}),
        )
        .unwrap();
        feed(
            &mut state,
            &mut output,
            &json!({"type":"message_start","message":{"role":"assistant"}}),
        )
        .unwrap();
        assert!(output.is_empty());
        feed(&mut state, &mut output, &end("final answer", "stop")).unwrap();
        assert_eq!(output, "final answer");
        state.finish().unwrap();
    }

    #[test]
    fn an_old_completion_does_not_authorize_a_new_incomplete_run() {
        let mut state = ChildProtocol::default();
        let mut output = String::new();
        feed(&mut state, &mut output, &end("first answer", "stop")).unwrap();
        state.finish().unwrap();
        feed(&mut state, &mut output, &json!({"type":"agent_start"})).unwrap();
        assert!(output.is_empty());
        assert!(state.finish().is_err());
    }

    #[test]
    fn unsuccessful_terminal_reasons_are_never_completed_delegations() {
        for reason in [
            "length",
            "toolUse",
            "pauseTurn",
            "refusal",
            "error",
            "aborted",
            "unknown",
        ] {
            let mut state = ChildProtocol::default();
            let mut output = String::new();
            assert!(
                feed(&mut state, &mut output, &end("partial answer", reason)).is_err(),
                "{reason}"
            );
            assert!(state.finish().is_err(), "{reason}");
        }
    }

    #[test]
    fn run_errors_are_rejected_without_echoing_child_diagnostics() {
        for error in [
            json!("api key secret-value"),
            json!({"details":"secret-value"}),
        ] {
            let mut state = ChildProtocol::default();
            let mut output = String::new();
            let mut event = end("looks successful", "stop");
            event["error"] = error;
            let error = feed(&mut state, &mut output, &event).unwrap_err();
            assert!(!error.contains("secret-value"));
            assert!(state.finish().is_err());
        }
    }

    #[test]
    fn missing_stop_reason_empty_answer_and_tool_only_end_are_rejected() {
        for last in [
            json!({"role":"assistant","content":[{"type":"text","text":"no terminal reason"}]}),
            message("   ", "stop"),
            json!({"role":"toolResult","content":[{"type":"text","text":"tool output"}]}),
            json!({"role":"assistant","stopReason":"stop","content":[{"type":"toolCall","id":"call","name":"write","arguments":{}},{"type":"text","text":"plan"}]}),
        ] {
            let mut state = ChildProtocol::default();
            assert!(
                feed(
                    &mut state,
                    &mut String::new(),
                    &json!({"type":"agent_end","messages":[message("earlier", "stop"), last]})
                )
                .is_err()
            );
        }
    }

    #[test]
    fn an_error_message_cannot_hide_behind_a_stop_reason() {
        let mut final_message = message("answer", "stop");
        final_message["errorMessage"] = json!("secret diagnostic");
        let mut state = ChildProtocol::default();
        let error = feed(
            &mut state,
            &mut String::new(),
            &json!({"type":"agent_end","messages":[final_message]}),
        )
        .unwrap_err();
        assert!(!error.contains("secret diagnostic"));
    }

    #[test]
    fn protocol_failure_is_sticky_even_if_a_success_frame_follows() {
        let mut state = ChildProtocol::default();
        let mut output = String::new();
        let original = state.ingest("not JSON", &mut output).unwrap_err();
        assert_eq!(
            feed(&mut state, &mut output, &end("cannot rescue", "stop")).unwrap_err(),
            original
        );
        assert_eq!(state.finish().unwrap_err(), original);
        assert!(output.is_empty());
    }

    #[test]
    fn answer_limit_never_turns_a_truncated_prefix_into_valid_output() {
        let mut state = ChildProtocol::default();
        let mut output = String::new();
        let event = end(&"x".repeat(MAX_ANSWER_BYTES + 1), "stop");
        assert_eq!(
            feed(&mut state, &mut output, &event).unwrap_err(),
            ANSWER_LIMIT
        );
        assert!(output.len() <= MAX_ANSWER_BYTES);
        assert!(state.finish().is_err());
    }

    #[test]
    fn unknown_events_do_not_break_a_valid_completion() {
        let mut state = ChildProtocol::default();
        let mut output = String::new();
        feed(
            &mut state,
            &mut output,
            &json!({"type":"future_telemetry","data":42}),
        )
        .unwrap();
        feed(&mut state, &mut output, &end("done", "stop")).unwrap();
        feed(&mut state, &mut output, &json!({"type":"usage","tokens":5})).unwrap();
        state.finish().unwrap();
    }

    #[test]
    fn bare_or_malformed_delta_frames_are_not_treated_as_text() {
        for update in [
            json!({"delta":"private"}),
            json!({"type":"text_delta","delta":null}),
        ] {
            let mut state = ChildProtocol::default();
            let mut output = String::new();
            assert!(
                feed(
                    &mut state,
                    &mut output,
                    &json!({"type":"message_update","assistantMessageEvent":update})
                )
                .is_err()
            );
            assert!(output.is_empty());
        }
    }

    #[test]
    fn bounded_reader_handles_fragmentation_crlf_and_unterminated_final_line() {
        let mut reader = BufReader::with_capacity(2, Cursor::new(b"one\r\ntwo\nlast"));
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"one");
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"two");
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"last");
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn bounded_reader_rejects_oversized_unterminated_output_before_eof() {
        let mut input = vec![b'x'; MAX_FRAME_BYTES + 1];
        input.extend_from_slice(b"\nnot consumed");
        let mut reader = BufReader::with_capacity(4096, Cursor::new(input));
        assert_eq!(read_frame(&mut reader).unwrap_err(), FRAME_LIMIT);
    }

    #[test]
    fn bounded_reader_accepts_the_exact_frame_limit() {
        let mut input = vec![b'x'; MAX_FRAME_BYTES];
        input.push(b'\n');
        let mut reader = BufReader::new(Cursor::new(input));
        assert_eq!(
            read_frame(&mut reader).unwrap().unwrap().len(),
            MAX_FRAME_BYTES
        );
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn done_snapshot_is_only_a_preview_until_agent_end() {
        let mut state = ChildProtocol::default();
        let mut output = String::new();
        feed(&mut state, &mut output, &json!({"type":"message_update","assistantMessageEvent":{"type":"done","message":message("revised answer", "stop")}})).unwrap();
        assert_eq!(output, "revised answer");
        assert!(state.finish().is_err());
        feed(&mut state, &mut output, &end("revised answer", "stop")).unwrap();
        state.finish().unwrap();
    }
}
