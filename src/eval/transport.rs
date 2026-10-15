//! Bounded Python protocol transport. The child is not a trusted parser peer:
//! malformed, unterminated, oversized, and misaddressed frames retire its kernel.
//!
//! Backpressure comes from the caller's one-slot synchronous channel. Never
//! drain an over-limit frame: the host will close/kill the checked-out kernel.

use crate::error::{Error, Result};
use serde_json::Value;
use std::io::{self, BufRead};

/// Includes JSON escaping and rich displays, not an allowance for stdout RAM.
/// At most one complete frame is queued in addition to the reader's frame.
pub(super) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

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
                Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EVAL_PROTOCOL: incomplete frame"))
            };
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let payload = end.unwrap_or(available.len());
        // Check before extending; even a newline-free writer cannot make this
        // reader allocate proportionally to the child process's output.
        if payload > limit.saturating_sub(bytes.len()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "EVAL_PROTOCOL_LIMIT: frame exceeds 16 MiB"));
        }
        bytes.extend_from_slice(&available[..payload]);
        reader.consume(payload + usize::from(end.is_some()));
        if end.is_some() {
            return String::from_utf8(bytes).map(Some).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "EVAL_PROTOCOL: frame is not UTF-8")
            });
        }
    }
}

fn invalid(message: &str) -> Error {
    Error::tool("eval", format!("EVAL_PROTOCOL: {message}; EVAL_STATE_RESET: Python kernel discarded"))
}

pub(super) fn validate_bridge(frame: &Value) -> Result<()> {
    // A peer may not make a successful final result double as a tool request.
    if frame.get("ok").is_some() || !frame.is_object() {
        return Err(invalid("ambiguous bridge response"));
    }
    let bridge = &frame["bridge"];
    if !bridge.get("call").and_then(Value::as_u64).is_some_and(|id| id > 0)
        || !bridge.get("tool").and_then(Value::as_str).is_some_and(|name| !name.is_empty())
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
        || frame["bridge"].as_object().is_none_or(|object| object.len() != 3)
    {
        return Err(invalid("ambiguous bridge envelope"));
    }
    let bridge = &frame["bridge"];
    if bridge["tool"].as_str().is_none_or(|name| name.chars().count() > 128) {
        return Err(invalid("EVAL_BRIDGE_LIMIT: tool name exceeds 128 characters"));
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
        return Err(invalid("EVAL_KERNEL_INCOMPLETE: cell did not settle safely"));
    }
    let ok = frame.get("ok").and_then(Value::as_bool)
        .ok_or_else(|| invalid("response status must be boolean"))?;
    if let Some(value) = frame.get("softTimeout") {
        let expired = value.as_bool().ok_or_else(|| invalid("soft timeout status must be boolean"))?;
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
        if !frame.get("result").is_some_and(|value| value.is_null() || value.is_string()) {
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
            assert_eq!(read_frame_with_limit(&mut input, 7).unwrap(), Some("1234567".into()));
            assert_eq!(read_frame_with_limit(&mut input, 7).unwrap(), Some("next".into()));
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
        assert!(read_frame_with_limit(&mut input, 16).unwrap_err().to_string().contains("EVAL_PROTOCOL_LIMIT"));
    }

    #[test]
    fn split_unicode_and_crlf_decode_without_losing_bytes() {
        let mut input = BufReader::with_capacity(1, Cursor::new("界😀\r\n".as_bytes()));
        assert_eq!(read_frame_with_limit(&mut input, 8).unwrap(), Some("界😀\r".into()));
    }

    #[test]
    fn truncated_or_invalid_utf8_frames_are_not_valid_eof() {
        assert_eq!(read_frame_with_limit(&mut Cursor::new(b"unterminated"), 64).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read_frame_with_limit(&mut Cursor::new(b"\xff\n"), 64).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(read_frame_with_limit(&mut Cursor::new(b"\n"), 64).unwrap(), Some(String::new()));
    }

    #[test]
    fn response_identity_and_status_fail_closed() {
        let valid = json!({"id":7,"ok":true,"stdout":"","stderr":"","result":null});
        assert!(validate_response(&valid, 7).is_ok());
        assert!(validate_response(&valid, 8).is_err());
        for (key, value) in [("id", json!("7")), ("ok", json!("true")), ("stdout", Value::Null), ("result", json!(42))] {
            let mut bad = valid.clone();
            bad[key] = value;
            assert!(validate_response(&bad, 7).is_err());
        }
        assert!(validate_response(&json!({"id":7,"ok":false,"stdout":"","stderr":"","error":"failure"}), 7).is_ok());
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
        for (id, size) in [(8,90), (7,1024*1024)] {
            assert!(validate_bridge_owner(&frame, id, size).is_err());
        }
        for (key, value) in [("id", json!(true)), ("stream", json!({})), ("softTimeout", json!(true))] {
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
            assert!(validate_response(&bad, 1).unwrap_err().to_string().contains("EVAL_KERNEL_INCOMPLETE"));
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
        if !std::process::Command::new("python3").arg("--version")
            .stdout(std::process::Stdio::null()).status().is_ok_and(|status| status.success())
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
        if !std::process::Command::new("python3").arg("--version")
            .stdout(std::process::Stdio::null()).status().is_ok_and(|status| status.success())
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
        let error = runtime.block_on(tool.execute("oversize", json!({"code":code}), None)).unwrap_err();
        assert!(error.to_string().contains("EVAL_PROTOCOL_LIMIT"), "{error}");
        assert!(!error.to_string().contains("EVAL_TIMEOUT"), "{error}");
        let next = runtime.block_on(tool.execute("next", json!({"code":"42"}), None)).unwrap();
        assert!(!next.is_error);
        assert_eq!(next.details.as_ref().unwrap()["cell"], 1);
    }
}
