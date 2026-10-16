//! Explicit, bounded native result projection over the ordinary eval bridge.
//!
//! `__pi_tool_result` is a protocol operation, never a mounted tool name. Its
//! inner name still passes the same host whitelist and execution policy.

use std::io::{self, Write};

use serde::Serialize;
use serde_json::Value;

use crate::model::ContentBlock;
use crate::tools::ToolOutput;

const STRUCTURED_LIMIT: usize = 4 * 1024 * 1024;
const TEXT_LIMIT: usize = 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 131_072;

pub(super) struct Request {
    pub name: String,
    pub args: Value,
    pub structured: bool,
}

impl Request {
    pub(super) fn parse(name: &str, input: Value) -> Result<Self, String> {
        if name != "__pi_tool_result" {
            return Ok(Self { name: name.to_owned(), args: input, structured: false });
        }
        let Value::Object(mut fields) = input else {
            return Err("EVAL_BRIDGE_INPUT: structured request requires name and args".into());
        };
        let name = match fields.remove("name") {
            Some(Value::String(name)) if !name.is_empty() && name.len() <= 128 => name,
            _ => return Err("EVAL_BRIDGE_INPUT: expected a nonempty tool name of at most 128 bytes".into()),
        };
        let args = fields.remove("args")
            .filter(Value::is_object)
            .ok_or("EVAL_BRIDGE_INPUT: structured args must be an object")?;
        if !fields.is_empty() {
            return Err("EVAL_BRIDGE_INPUT: unknown structured request fields".into());
        }
        // No recursive unwrapping. Even a deliberately nested operation must
        // be denied by the same concrete tool-name whitelist in the caller.
        Ok(Self { name, args, structured: true })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Envelope<'a> {
    schema: &'static str,
    content: &'a [ContentBlock],
    details: &'a Option<Value>,
    is_error: bool,
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("eval bridge result byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

fn check_json(value: &Value, depth: usize, remaining: &mut usize) -> Result<(), String> {
    if depth > MAX_DEPTH || *remaining == 0 {
        return Err("EVAL_BRIDGE_LIMIT: structured details exceed depth/node limits".into());
    }
    *remaining -= 1;
    match value {
        Value::Array(values) => {
            for value in values { check_json(value, depth + 1, remaining)?; }
        }
        Value::Object(values) => {
            for value in values.values() { check_json(value, depth + 1, remaining)?; }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn structured(output: &ToolOutput) -> Result<String, String> {
    let mut remaining = MAX_NODES;
    if let Some(details) = &output.details {
        check_json(details, 0, &mut remaining)?;
    }
    let envelope = Envelope {
        schema: "pi.eval.tool_result.v1",
        content: &output.content,
        details: &output.details,
        is_error: output.is_error,
    };
    let mut writer = BoundedWriter { bytes: Vec::new(), limit: STRUCTURED_LIMIT };
    serde_json::to_writer(&mut writer, &envelope).map_err(|_| {
        "EVAL_BRIDGE_LIMIT: structured result exceeds 4 MiB or cannot be encoded".to_string()
    })?;
    String::from_utf8(writer.bytes)
        .map_err(|_| "EVAL_BRIDGE_PROTOCOL: result was not UTF-8".to_string())
}

pub(super) fn text(output: &ToolOutput) -> Result<String, String> {
    let mut text = String::new();
    for block in &output.content {
        if let ContentBlock::Text(block) = block {
            if block.text.len() > TEXT_LIMIT.saturating_sub(text.len()) {
                return Err("EVAL_BRIDGE_LIMIT: text result exceeds 1 MiB".into());
            }
            text.push_str(&block.text);
        }
    }
    if output.is_error { Err(text) } else { Ok(text) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ImageContent, TextContent};
    use crate::tools::Tool;
    use serde_json::json;

    fn output(details: Option<Value>) -> ToolOutput {
        ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new("payload"))],
            details,
            is_error: false,
        }
    }

    #[test]
    fn native_details_and_error_are_preserved() {
        for details in [None, Some(Value::Null), Some(json!([1, "x"])), Some(json!(42))] {
            let mut output = output(details.clone());
            output.is_error = true;
            let value: Value = serde_json::from_str(&structured(&output).unwrap()).unwrap();
            assert_eq!(value["details"], details.unwrap_or(Value::Null));
            assert_eq!(value["isError"], true);
            assert_eq!(value["content"][0]["text"], "payload");
            assert!(text(&output).is_err());
        }
    }

    #[test]
    fn image_data_is_not_flattened_or_automatically_displayed() {
        let mut output = output(None);
        output.content.push(ContentBlock::Image(ImageContent {
            data: "aW1hZ2U=".into(), mime_type: "image/png".into(),
        }));
        let value: Value = serde_json::from_str(&structured(&output).unwrap()).unwrap();
        assert_eq!(value["content"][1]["data"], "aW1hZ2U=");
        assert_eq!(value["content"][1]["mimeType"], "image/png");
        assert_eq!(text(&output).unwrap(), "payload");
    }

    #[test]
    fn encoded_bytes_depth_and_node_limits_fail_explicitly() {
        let escaped = output(Some(json!("\u{0}".repeat(STRUCTURED_LIMIT / 6 + 1))));
        assert!(structured(&escaped).unwrap_err().contains("EVAL_BRIDGE_LIMIT"));
        let mut nested = Value::Null;
        for _ in 0..=MAX_DEPTH { nested = json!([nested]); }
        assert!(structured(&output(Some(nested))).unwrap_err().contains("depth/node"));
        let wide = Value::Array(vec![Value::Null; MAX_NODES]);
        assert!(structured(&output(Some(wide))).unwrap_err().contains("depth/node"));
    }

    #[test]
    fn malformed_operations_are_rejected_before_dispatch() {
        for input in [Value::Null, json!({}), json!({"name":"read","args":[]}),
            json!({"name":"read","args":{},"extra":true}),
            json!({"name":"","args":{}})] {
            assert!(Request::parse("__pi_tool_result", input).is_err());
        }
        let plain = Request::parse("read", json!({"path":"x"})).unwrap();
        assert!(!plain.structured);
        assert_eq!(plain.args["path"], "x");
    }

    #[test]
    fn structured_reads_equal_the_direct_native_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), "structured-read-proof\n").unwrap();
        let tool = super::super::EvalTool::new(dir.path());
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        runtime.block_on(async {
            let args = json!({"path":"data.txt"});
            let direct = crate::tools::ReadTool::new(dir.path())
                .execute("eval-bridge", args.clone(), None).await.unwrap();
            let reply = tool.run_bridge_call_parts("__pi_tool_result",
                json!({"name":"read","args":args})).await.unwrap();
            let expected = structured(&direct).unwrap();
            assert_eq!(reply, expected);
            for name in ["bash", "eval", "__pi_tool_result"] {
                let error = tool.run_bridge_call_parts("__pi_tool_result",
                    json!({"name":name,"args":{}})).await.unwrap_err();
                assert!(error.contains("EVAL_BRIDGE_DENIED"));
            }
        });
    }

    #[test]
    fn quickjs_helper_reaches_the_native_reader() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), "structured-realm-proof\n").unwrap();
        let tool = super::super::EvalTool::new(dir.path());
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let result = runtime.block_on(tool.execute("structured-js", json!({
            "kernel":"js",
            "code":"const r = await tool.result('read', {path:'data.txt'}); JSON.stringify([r.schema, r.isError, r.content[0].text.includes('structured-realm-proof')])"
        }), None)).unwrap();
        assert!(!result.is_error, "{result:?}");
        let text = text(&result).unwrap();
        assert!(text.contains("pi.eval.tool_result.v1"));
        assert!(text.contains("false,true"));
    }

    #[test]
    fn python_helper_reaches_native_reader_in_retained_and_private_cells() {
        if !std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("Skipping: python3 not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), "structured-python-proof\n").unwrap();
        let tool = super::super::EvalTool::new(dir.path());
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let result = runtime.block_on(tool.execute("structured-python", json!({
            "kernel":"python",
            "code":"saved = await tool.result('read', {'path':'data.txt'})\n[saved['schema'], saved['isError'], 'structured-python-proof' in saved['content'][0]['text']]"
        }), None)).unwrap();
        assert!(!result.is_error, "{result:?}");
        let result_text = text(&result).unwrap();
        assert!(result_text.contains("pi.eval.tool_result.v1"));
        assert!(result_text.contains("False, True"));

        let private = runtime.block_on(tool.execute("private-python", json!({
            "kernel":"python", "kernel_mode":"per_call",
            "code":"('saved' in globals(), (await tool.result('read', {'path':'data.txt'}))['isError'])"
        }), None)).unwrap();
        assert!(!private.is_error, "{private:?}");
        assert!(text(&private).unwrap().contains("(False, False)"));

        let retained = runtime.block_on(tool.execute("retained-python", json!({
            "kernel":"python", "code":"saved['schema']"
        }), None)).unwrap();
        assert!(!retained.is_error, "{retained:?}");
        assert!(text(&retained).unwrap().contains("pi.eval.tool_result.v1"));
    }
}
