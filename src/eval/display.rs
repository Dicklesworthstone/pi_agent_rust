//! Model-visible rich output and structured SDK results from eval cells.
//!
//! Explicit MIME frames are data, never host instructions or paths. Admit
//! the entire bounded collection before publishing native content. Image checks
//! inspect only envelopes/dimensions; no raster decode or validity claim.

use base64::Engine as _;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::model::{ContentBlock, ImageContent, TextContent};
use crate::tools::ToolOutput;

mod images;

pub(super) const MAX_DISPLAYS: usize = 32;
pub(super) const MAX_ITEM_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_NODES: usize = 65_536;
const MAX_DEPTH: usize = 32;

fn invalid(message: &str) -> Error {
    Error::tool("eval", format!("EVAL_DISPLAY_INVALID: {message}"))
}

fn limit() -> Error {
    Error::tool(
        "eval",
        "EVAL_DISPLAY_LIMIT: rich output exceeds its byte, count, or structure budget",
    )
}

struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|size| *size <= MAX_ITEM_BYTES)
            .ok_or_else(|| std::io::Error::other("display size exceeded"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_shape(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes = nodes.checked_add(1).ok_or_else(limit)?;
    if depth > MAX_DEPTH || *nodes > MAX_NODES {
        return Err(limit());
    }
    match value {
        Value::Array(values) => {
            for value in values {
                json_shape(value, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                json_shape(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn png_dimensions(encoded: &str) -> Result<(u32, u32)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| invalid("PNG must use canonical standard base64"))?;
    if bytes.len() < 45
        || !bytes.starts_with(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR")
        || !bytes.ends_with(b"\x00\x00\x00\x00IEND\xaeB`\x82")
    {
        return Err(invalid("PNG header or end marker is missing"));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().map_err(|_| invalid("PNG width"))?);
    let height = u32::from_be_bytes(
        bytes[20..24]
            .try_into()
            .map_err(|_| invalid("PNG height"))?,
    );
    if width == 0
        || height == 0
        || width > 16_384
        || height > 16_384
        || u64::from(width) * u64::from(height) > 16_777_216
    {
        return Err(limit());
    }
    Ok((width, height))
}

/// Inspect an already byte-bounded image envelope without raster decoding.
/// Callers own the encoded-byte allowance; these shared checks authenticate
/// the declared format and bound its canvas, not complete raster validity.
pub fn image_dimensions(encoded: &str, mime: &str) -> Result<(u32, u32)> {
    if mime == "image/png" {
        png_dimensions(encoded)
    } else {
        images::dimensions(encoded, mime)
    }
}

#[derive(Default)]
struct Displays {
    content: Vec<ContentBlock>,
    json_outputs: Vec<Value>,
    descriptors: Vec<Value>,
}

fn collect(response: &Value) -> Result<Displays> {
    let Some(frames) = response.get("displays") else {
        return Ok(Displays::default());
    };
    let frames = frames
        .as_array()
        .ok_or_else(|| invalid("displays must be an array"))?;
    if frames.len() > MAX_DISPLAYS {
        return Err(limit());
    }
    let mut total = 0usize;
    let mut displays = Displays::default();
    for frame in frames {
        let object = frame
            .as_object()
            .ok_or_else(|| invalid("display must be an object"))?;
        if object.len() != 2 || !object.contains_key("data") {
            return Err(invalid("display fields must be exactly mimeType and data"));
        }
        let mime = frame
            .get("mimeType")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("display MIME type must be a string"))?;
        let data = &frame["data"];
        let mut nodes = 0;
        json_shape(data, 0, &mut nodes)?;
        let mut count = ByteCounter(0);
        serde_json::to_writer(&mut count, frame).map_err(|_| limit())?;
        total = total
            .checked_add(count.0)
            .filter(|size| *size <= MAX_TOTAL_BYTES)
            .ok_or_else(limit)?;
        let mut descriptor = json!({"mimeType":mime, "bytes":count.0});
        let block = match mime {
            "application/json" => {
                displays.json_outputs.push(data.clone());
                ContentBlock::Text(TextContent::new(
                    serde_json::to_string(data)
                        .map_err(|_| invalid("JSON display could not be encoded"))?,
                ))
            }
            "text/plain" | "text/markdown" => {
                let text = data
                    .as_str()
                    .ok_or_else(|| invalid("text display data must be a string"))?;
                ContentBlock::Text(TextContent::new(text))
            }
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" => {
                let encoded = data
                    .as_str()
                    .ok_or_else(|| invalid("image display data must be base64 text"))?;
                let (width, height) = image_dimensions(encoded, mime)?;
                descriptor["width"] = json!(width);
                descriptor["height"] = json!(height);
                ContentBlock::Image(ImageContent {
                    data: encoded.to_string(),
                    mime_type: mime.to_string(),
                })
            }
            _ => return Err(invalid("unsupported display MIME type")),
        };
        displays.descriptors.push(descriptor);
        displays.content.push(block);
    }
    Ok(displays)
}

fn append_line(text: &mut String, value: &str) {
    if !value.is_empty() {
        text.push_str(value);
        if !text.ends_with('\n') {
            text.push('\n');
        }
    }
}

pub(super) fn python_response(line: &str, restarted: bool, cells_run: u64) -> Result<ToolOutput> {
    let response: Value = serde_json::from_str(line)
        .map_err(|error| Error::tool("eval", format!("EVAL_PROTOCOL: {error}")))?;
    response_output(&response, "python", restarted, cells_run)
}

pub(super) fn javascript_response(
    response: &super::js_kernel::JsCellResponse,
    restarted: bool,
    cells_run: u64,
) -> Result<ToolOutput> {
    let response = json!({
        "ok": response.ok,
        "stdout": response.console,
        "result": response.result,
        "error": response.error,
        "displays": response.displays,
    });
    response_output(&response, "js", restarted, cells_run)
}

fn response_output(
    response: &Value,
    kernel: &str,
    restarted: bool,
    cells_run: u64,
) -> Result<ToolOutput> {
    let displays = collect(response)?;
    let ok = response.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let mut text = String::new();
    if restarted && cells_run == 1 {
        text.push_str(if kernel == "js" {
            "(js kernel started)\n"
        } else {
            "(kernel started)\n"
        });
    }
    append_line(
        &mut text,
        response.get("stdout").and_then(Value::as_str).unwrap_or(""),
    );
    append_line(
        &mut text,
        response.get("stderr").and_then(Value::as_str).unwrap_or(""),
    );
    let mut content = Vec::new();
    if !displays.content.is_empty() {
        if !text.is_empty() {
            content.push(ContentBlock::Text(TextContent::new(std::mem::take(
                &mut text,
            ))));
        }
        content.extend(displays.content);
    }
    if ok {
        if let Some(result) = response.get("result").and_then(Value::as_str) {
            text.push_str(result);
            text.push('\n');
        }
    } else {
        text.push_str(response.get("error").and_then(Value::as_str).unwrap_or("?"));
    }
    if content.is_empty() && text.is_empty() {
        text.push_str("(no output)\n");
    }
    if !text.is_empty() {
        content.push(ContentBlock::Text(TextContent::new(text)));
    }
    let mut details = json!({"kernel":kernel, "cell":cells_run, "restarted":restarted});
    if !displays.descriptors.is_empty() {
        details["displays"] = Value::Array(displays.descriptors);
        details["jsonOutputs"] = Value::Array(displays.json_outputs);
    }
    if !ok {
        details["errorKind"] = json!("exception");
    }
    Ok(ToolOutput {
        content,
        details: Some(details),
        is_error: !ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::needless_pass_by_value)] // test helper; callers build owned json! values inline
    fn output(displays: Value, ok: bool) -> Result<ToolOutput> {
        python_response(
            &json!({"ok":ok, "stdout":"stdout", "stderr":"stderr", "displays":displays,
            "result":"42", "error":"ValueError: later failure"})
            .to_string(),
            false,
            3,
        )
    }

    fn png(width: u32, height: u32) -> String {
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[0; 9]);
        bytes.extend_from_slice(b"\x00\x00\x00\x00IEND\xaeB`\x82");
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn mixed_displays_preserve_order_structure_and_native_images() {
        let out = output(
            json!([
                {"mimeType":"application/json", "data":{"answer":[42,true,null]}},
                {"mimeType":"text/markdown", "data":"## Answer"},
                {"mimeType":"image/png", "data":png(1,2)},
            ]),
            true,
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(
            matches!(&out.content[0], ContentBlock::Text(text) if text.text == "stdout\nstderr\n")
        );
        assert!(
            matches!(&out.content[1], ContentBlock::Text(text) if text.text.contains("answer"))
        );
        assert!(matches!(&out.content[2], ContentBlock::Text(text) if text.text == "## Answer"));
        assert!(
            matches!(&out.content[3], ContentBlock::Image(image) if image.mime_type == "image/png")
        );
        assert!(matches!(&out.content[4], ContentBlock::Text(text) if text.text == "42\n"));
        let details = out.details.unwrap();
        assert_eq!(details["jsonOutputs"], json!([{"answer":[42,true,null]}]));
        assert_eq!(details["displays"][2]["width"], 1);
        assert!(
            !details.to_string().contains(&png(1, 2)),
            "do not duplicate PNGs in details"
        );
    }

    #[test]
    fn display_before_a_cell_exception_is_retained_without_becoming_success() {
        let out = output(json!([{"mimeType":"application/json","data":null}]), false).unwrap();
        assert!(out.is_error);
        assert_eq!(out.details.unwrap()["jsonOutputs"], json!([null]));
        assert!(
            matches!(out.content.last(), Some(ContentBlock::Text(text)) if text.text.contains("later failure"))
        );
    }

    #[test]
    fn one_bad_frame_refuses_the_entire_rich_result() {
        for bad in [
            json!({"mimeType":"text/html","data":"<script>no</script>"}),
            json!({"mimeType":"text/plain","data":42}),
            json!({"mimeType":"image/png","data":"https://example.invalid/image.png"}),
            json!({"mimeType":"text/plain"}),
            json!({"mimeType":"text/plain","data":"ok","instruction":"ignored?"}),
            Value::Null,
        ] {
            assert!(
                output(
                    json!([{"mimeType":"text/plain","data":"accepted prefix"},bad]),
                    true
                )
                .is_err()
            );
        }
        assert!(output(Value::Null, true).is_err());
    }

    #[test]
    fn collection_count_and_aggregate_bytes_have_independent_limits() {
        let small = json!({"mimeType":"text/plain","data":""});
        assert!(output(Value::Array(vec![small.clone(); MAX_DISPLAYS]), true).is_ok());
        assert!(output(Value::Array(vec![small; MAX_DISPLAYS + 1]), true).is_err());
        let large = json!({"mimeType":"text/plain","data":"x".repeat(1_500_000)});
        assert!(output(json!([large.clone(), large]), true).is_ok());
        assert!(output(json!([large.clone(), large.clone(), large]), true).is_err());
    }

    #[test]
    fn item_limit_counts_serialized_utf8_and_escaping() {
        let overhead = json!({"mimeType":"text/plain","data":""}).to_string().len();
        let exact = json!({"mimeType":"text/plain","data":"x".repeat(MAX_ITEM_BYTES - overhead)});
        assert!(output(json!([exact]), true).is_ok());
        assert!(
            output(
                json!([{"mimeType":"text/plain","data":"x".repeat(MAX_ITEM_BYTES - overhead + 1)}]),
                true
            )
            .is_err()
        );
        assert!(
            output(
                json!([{"mimeType":"text/plain","data":"\0".repeat(MAX_ITEM_BYTES / 2)}]),
                true
            )
            .is_err()
        );
        assert!(
            output(
                json!([{"mimeType":"text/plain","data":"界".repeat(MAX_ITEM_BYTES / 2)}]),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn json_depth_and_node_amplification_are_bounded() {
        let mut data = Value::Null;
        for _ in 0..MAX_DEPTH {
            data = json!([data]);
        }
        assert!(output(json!([{"mimeType":"application/json","data":data}]), true).is_ok());
        assert!(output(json!([{"mimeType":"application/json","data":[data]}]), true).is_err());
        assert!(
            output(
                json!([{"mimeType":"application/json","data":vec![Value::Null; MAX_NODES]}]),
                true
            )
            .is_err()
        );
    }

    #[test]
    fn png_dimension_budgets_and_envelope_are_checked_without_raster_decoding() {
        for (w, h) in [(0, 1), (1, 0), (16385, 1), (4096, 4097)] {
            assert!(output(json!([{"mimeType":"image/png","data":png(w,h)}]), true).is_err());
        }
        assert!(
            output(
                json!([{"mimeType":"image/png","data":png(4096,4096)}]),
                true
            )
            .is_ok()
        );
        assert!(output(json!([{"mimeType":"image/png","data":"aGVsbG8="}]), true).is_err());
    }

    #[test]
    fn legacy_plain_outputs_are_unchanged_and_empty_output_remains_explicit() {
        let out = python_response(
            r#"{"ok":true,"stdout":"a","stderr":"b","result":"42"}"#,
            false,
            2,
        )
        .unwrap();
        assert!(matches!(&out.content[0], ContentBlock::Text(text) if text.text == "a\nb\n42\n"));
        assert_eq!(out.content.len(), 1);
        assert!(out.details.unwrap().get("displays").is_none());
        let out = python_response(r#"{"ok":true,"result":null,"displays":[]}"#, false, 2).unwrap();
        assert!(
            matches!(&out.content[0], ContentBlock::Text(text) if text.text == "(no output)\n")
        );
    }
}

#[cfg(test)]
mod javascript_tests {
    use super::super::js_kernel::JsCellResponse;
    use super::*;

    fn response(displays: Vec<Value>) -> JsCellResponse {
        JsCellResponse {
            ok: true,
            console: "console".into(),
            result: Some("17".into()),
            error: None,
            displays,
        }
    }

    #[test]
    fn javascript_uses_the_same_host_admission_as_python() {
        let frames = vec![json!({"mimeType":"application/json","data":{"answer":42}})];
        let js = javascript_response(&response(frames.clone()), false, 2).unwrap();
        let py = python_response(
            &json!({"ok":true,"stdout":"console","result":"17","displays":frames}).to_string(),
            false,
            2,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&js.content).unwrap(),
            serde_json::to_value(&py.content).unwrap()
        );
        assert_eq!(
            js.details.as_ref().unwrap()["jsonOutputs"],
            py.details.as_ref().unwrap()["jsonOutputs"]
        );
        assert_eq!(js.details.unwrap()["kernel"], "js");
        assert_eq!(py.details.unwrap()["kernel"], "python");
    }

    #[test]
    fn malformed_native_javascript_frame_never_publishes_its_valid_prefix() {
        for bad in [
            json!({"mimeType":"text/html","data":"bad"}),
            Value::Null,
            json!({"mimeType":"image/png","data":"bad"}),
        ] {
            let frames = vec![json!({"mimeType":"text/plain","data":"prefix"}), bad];
            assert!(javascript_response(&response(frames), false, 2).is_err());
        }
        let over = vec![json!({"mimeType":"application/json","data":0}); MAX_DISPLAYS + 1];
        assert!(javascript_response(&response(over), false, 2).is_err());
    }

    #[test]
    fn plain_javascript_output_and_start_notice_remain_unchanged() {
        for (restarted, expected) in [
            (false, "console\n17\n"),
            (true, "(js kernel started)\nconsole\n17\n"),
        ] {
            let output = javascript_response(&response(Vec::new()), restarted, 1).unwrap();
            assert_eq!(output.content.len(), 1);
            assert!(
                matches!(&output.content[0], ContentBlock::Text(text) if text.text == expected)
            );
            assert!(output.details.unwrap().get("displays").is_none());
        }
    }
}
